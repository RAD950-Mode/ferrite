//! SOL950 streaming receiver fitting strictly inside `ferrite_core::decoders::BUDGET` (`13,312` bytes).
//!
//! Pipeline:
//! 1. `24,000 Hz -> 6,000 Hz` 4:1 low-pass FIR decimator (`udsp::fir::Decimator<31>`).
//! 2. Sliding 480-sample (`80 ms`) Hann-windowed 512-point real FFT (`udsp::fft`) evaluated every `240` samples
//!    (`40 ms` half-symbol phase `0` and `1`), with block-exponent (`1 << shift`) log-power normalization.
//! 3. `32-symbol x 2-phase x 156-bin` magnitude ring buffer (`9,984 B`) holding `539..2,355 Hz` across all arrival
//!    skews `start_sym in 1..=9` (`[-320 ms, +360 ms]` relative to nominal `+400 ms` start).
//! 4. Clamped contrast `S0 + S1` 2D acquisition at slot symbol `32` (`t = 2.64 s`) admitting `K = 8` tracks with
//!    frequency non-maximum suppression and separated-peak fairness reservation, backfilling `data[0..12]` (and
//!    any early `data[12..]` symbols already in the ring) and streaming the remaining symbols + `S2` drift check.
//! 5. Serial normalized min-sum `(184, 78)` LDPC + CRC-14 decoding at end of slot.

use crate::frame::Frame;
use crate::ldpc::{self, BpScratch, N_BITS};
use crate::phy::{data_sym_pos, interleave_idx, DEFAULT_SPACING_HZ, N_DATA_SYMS, N_SYMBOLS, S0, S1, S2};
use udsp::fft::{cfft, real_power};
use udsp::fir::Decimator;
use udsp::log::log2_q8;
use udsp::mix::lowpass;
use udsp::trig::ccos;

pub const INPUT_RATE_HZ: u32 = 24_000;
pub const DECIM_FACTOR: usize = 4;
pub const PROC_RATE_HZ: u32 = INPUT_RATE_HZ / DECIM_FACTOR as u32; // 6,000 Hz
pub const SYM_SAMPLES: usize = 480; // 80 ms @ 6,000 Hz
pub const HALF_SAMPLES: usize = 240; // 40 ms @ 6,000 Hz
pub const FFT_N: usize = 512;
pub const FFT_M: usize = FFT_N / 2; // 256 complex points

/// First FFT bin stored (`46 * 6000 / 512 = 539.06 Hz`) and number of bins (`46..202`, up to `2,355.5 Hz`).
pub const FIRST_BIN: usize = 46;
pub const N_BINS: usize = 156;

/// Ring depth in whole symbols (`1..=32` covers `start_sym in 1..=9` through `S1` end `9 + 23 = 32`).
pub const RING_SYMS: usize = 32;
pub const MIN_START_SYM: usize = 1; // +80 ms (-320 ms vs +400 ms nominal)
pub const NOMINAL_START_SYM: usize = 5; // +400 ms nominal
pub const MAX_START_SYM: usize = 9; // +720 ms (+320 ms vs +400 ms nominal; +360 ms on phase 1)
pub const ACQUIRE_AT_SYM: usize = MAX_START_SYM + 23; // 32

pub const MAX_TRACKS: usize = 8;
pub const MAX_DECODED: usize = 8;
pub const LDPC_MAX_ITER: usize = 35;

static DECIM_TAPS: [i16; 31] = lowpass::<31>(2500.0 / 24000.0);
static HANN_480: [i16; SYM_SAMPLES] = hann_table();

const fn hann_table() -> [i16; SYM_SAMPLES] {
    let mut w = [0i16; SYM_SAMPLES];
    let mut n = 0;
    while n < SYM_SAMPLES {
        let v = 0.5 * (1.0 - ccos(2.0 * core::f64::consts::PI * n as f64 / SYM_SAMPLES as f64));
        let q = (v * 32767.0 + 0.5) as i32;
        w[n] = if q > 32767 { 32767 } else { q as i16 };
        n += 1;
    }
    w
}

/// Decoded frame together with its measured physical-layer metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decoded {
    pub frame: Frame,
    /// Estimated lowest-tone audio frequency offset in Hz above USB dial.
    pub audio_hz: u16,
    /// Arrival offset in milliseconds relative to nominal `+400 ms` start (`[-320, +360]`).
    pub time_offset_ms: i16,
    /// Estimated SNR in dB referenced to a `2,500 Hz` noise bandwidth (`[-24, +20]`).
    pub snr_db: i8,
}

#[derive(Clone, Copy)]
struct Track {
    active: bool,
    start_sym: u8,
    phase: u8,
    q0: u16, // half-bin index relative to FIRST_BIN
    sig_accum: i32,
    noise_accum: i32,
    s2_drift: i8, // -1, 0, +1 half-bin shift on second half if S2 prefers it
    llr: [i8; N_BITS],
}

impl Track {
    const fn empty() -> Self {
        Self {
            active: false,
            start_sym: 0,
            phase: 0,
            q0: 0,
            sig_accum: 0,
            noise_accum: 0,
            s2_drift: 0,
            llr: [0; N_BITS],
        }
    }
}

/// Streaming SOL950 slot receiver (`<= 13,312` bytes, zero heap allocation).
pub struct Receiver {
    decim: Decimator<31>,
    pcm_ring: [i16; SYM_SAMPLES],
    pcm_pos: usize,
    decim_count: u32,
    spacing_hz: u32,
    tone_q_off: [u16; 4],
    mag_ring: [[[u8; N_BINS]; 2]; RING_SYMS],
    tracks: [Track; MAX_TRACKS],
    acquired: bool,
}

impl Receiver {
    pub const fn new() -> Self {
        Self::with_spacing(DEFAULT_SPACING_HZ)
    }

    pub const fn with_spacing(spacing_hz: u32) -> Self {
        let tone_q_off = compute_tone_q_off(spacing_hz);
        Self {
            decim: Decimator::new(),
            pcm_ring: [0; SYM_SAMPLES],
            pcm_pos: 0,
            decim_count: 0,
            spacing_hz,
            tone_q_off,
            mag_ring: [[[0; N_BINS]; 2]; RING_SYMS],
            tracks: [Track::empty(); MAX_TRACKS],
            acquired: false,
        }
    }

    /// Reset state at the UTC `10.0 s` slot boundary (`t = 0.00 s`).
    pub fn reset(&mut self) {
        self.decim.reset();
        self.pcm_ring = [0; SYM_SAMPLES];
        self.pcm_pos = 0;
        self.decim_count = 0;
        self.tracks = [Track::empty(); MAX_TRACKS];
        self.acquired = false;
    }

    /// Feed signed 16-bit PCM samples at `24,000 Hz` (e.g. `192`-sample `8 ms` DSP blocks).
    pub fn push_samples(&mut self, samples: &[i16]) {
        for &x in samples {
            if let Some(y) = self.decim.push(x, &DECIM_TAPS, DECIM_FACTOR) {
                self.pcm_ring[self.pcm_pos] = y;
                self.pcm_pos = if self.pcm_pos + 1 == SYM_SAMPLES { 0 } else { self.pcm_pos + 1 };
                self.decim_count += 1;

                if self.decim_count >= SYM_SAMPLES as u32
                    && (self.decim_count - SYM_SAMPLES as u32).is_multiple_of(HALF_SAMPLES as u32)
                {
                    let h = ((self.decim_count - SYM_SAMPLES as u32) / HALF_SAMPLES as u32) as usize;
                    let sym = h / 2;
                    let phase = h % 2;
                    self.on_half_symbol(sym, phase);
                }
            }
        }
    }

    fn on_half_symbol(&mut self, sym: usize, phase: usize) {
        // Compute 480-sample Hann-windowed 512-pt real FFT on the stack (~1 KB stack).
        let mut z = [0i16; FFT_N];
        let mut idx = self.pcm_pos; // oldest sample in pcm_ring
        for (n, &w) in HANN_480.iter().enumerate() {
            let s = self.pcm_ring[idx] as i32;
            idx = if idx + 1 == SYM_SAMPLES { 0 } else { idx + 1 };
            z[n] = ((s * w as i32 + 16_384) >> 15) as i16;
        }
        let shift = cfft(&mut z, FFT_M);

        let mut cur_bins = [0u8; N_BINS];
        let mut out_i = 0usize;
        real_power(&z, FFT_M, FIRST_BIN, 1, N_BINS, |_, pwr| {
            // Exact shift-compensated log2 power in 1/8-octave (0.376 dB) steps across [2^13 .. 2^45] (~96 dB
            // up to a full-scale 32767 sine wave without u8 clipping).
            let lp = (log2_q8(pwr.max(1)) + ((2 * shift as i32 - 13) << 8)) >> 5;
            cur_bins[out_i] = lp.clamp(0, 255) as u8;
            out_i += 1;
        });

        // Store in circular buffer while in acquisition range.
        if !self.acquired {
            self.mag_ring[sym % RING_SYMS][phase] = cur_bins;
            if sym == ACQUIRE_AT_SYM && phase == 1 {
                self.acquire_candidates();
                self.acquired = true;
            }
            return;
        }

        // Stream symbols post-acquisition into active tracks matching `phase`.
        for tr_i in 0..MAX_TRACKS {
            if !self.tracks[tr_i].active || self.tracks[tr_i].phase as usize != phase {
                continue;
            }
            let start = self.tracks[tr_i].start_sym as usize;
            if sym < start {
                continue;
            }
            let rel = sym - start;
            if rel < N_SYMBOLS {
                self.feed_track_symbol(tr_i, rel, &cur_bins);
            }
        }
    }

    #[inline]
    fn sample_tone(bins: &[u8; N_BINS], q0: u16, tone_q_off: &[u16; 4], tone: usize) -> i32 {
        let q = (q0 + tone_q_off[tone]) as usize;
        let b = q >> 1;
        if b >= N_BINS {
            return 0;
        }
        if (q & 1) == 0 || b + 1 >= N_BINS {
            bins[b] as i32
        } else {
            (bins[b] as i32 + bins[b + 1] as i32 + 1) >> 1
        }
    }

    /// Global 2D `(start_sym in 1..=9, phase in 0..1, half-bin q0)` search over `S0 + S1`.
    fn acquire_candidates(&mut self) {
        let max_q0 = ((N_BINS - 2) * 2).saturating_sub(self.tone_q_off[3] as usize) as u16;

        // First find the best `(start_sym, phase, score)` at every frequency `q0` (1D profile over frequency).
        // Stack usage: `max_q0 <= 300`, so `300 * 4 = 1,200 bytes` on stack.
        let mut best_score = [i16::MIN; 312];
        let mut best_sp = [0u8; 312]; // (start_sym << 1) | phase

        let n_q = (max_q0 as usize + 1).min(312);
        for s in MIN_START_SYM..=MAX_START_SYM {
            for phase in 0..2usize {
                for q0 in 0..n_q {
                    let mut contrast = 0i32;
                    let mut raw_sum = 0i32;
                    for (k, &w) in S0.iter().enumerate() {
                        let bins = &self.mag_ring[(s + k) % RING_SYMS][phase];
                        let (c, rw) = Self::contrast_sym(bins, q0 as u16, &self.tone_q_off, w as usize);
                        contrast += c;
                        raw_sum += rw;
                    }
                    for (k, &w) in S1.iter().enumerate() {
                        let bins = &self.mag_ring[(s + 18 + k) % RING_SYMS][phase];
                        let (c, rw) = Self::contrast_sym(bins, q0 as u16, &self.tone_q_off, w as usize);
                        contrast += c;
                        raw_sum += rw;
                    }
                    // Primary score is clamped contrast (rejects +40 dB off-tone sidelobes); minor tie-breaker
                    // (`raw_sum >> 4`) breaks any high-SNR saturation plateau cleanly at the exact tone center.
                    let score = contrast * 8 + (raw_sum >> 4);
                    let sc = score.clamp(i16::MIN as i32 + 1, i16::MAX as i32) as i16;
                    if sc > best_score[q0] {
                        best_score[q0] = sc;
                        best_sp[q0] = ((s as u8) << 1) | (phase as u8);
                    }
                }
            }
        }

        // Pick up to `MAX_TRACKS` local maxima with >= 8 half-bins (~46.9 Hz, < 1 tone spacing) NMS radius so
        // even a caller displaced by 1 tone spacing (50 Hz) can be tracked independently, while reserving the
        // last 2 tracks for peaks >= 32 half-bins (~187 Hz) away.
        let mut chosen_n = 0usize;
        for slot in 0..MAX_TRACKS {
            let min_sep: usize = if slot < MAX_TRACKS - 2 { 8 } else { 32 };
            let mut pick_q = None;
            let mut pick_sc = 288i16; // minimum positive contrast threshold above pure noise (36 * 8)
            for q0 in 0..n_q {
                let sc = best_score[q0];
                if sc <= pick_sc {
                    continue;
                }
                // Local maximum check (+/- 1 half-bin)
                if q0 > 0 && best_score[q0 - 1] > sc {
                    continue;
                }
                if q0 + 1 < n_q && best_score[q0 + 1] >= sc {
                    continue;
                }
                let mut too_close = false;
                for prev in &self.tracks[..chosen_n] {
                    if (prev.q0 as usize).abs_diff(q0) < min_sep {
                        too_close = true;
                        break;
                    }
                }
                if !too_close {
                    pick_sc = sc;
                    pick_q = Some(q0);
                }
            }
            let Some(q0) = pick_q else {
                continue;
            };
            let sp = best_sp[q0];
            let start_sym = sp >> 1;
            let phase = sp & 1;
            self.tracks[chosen_n] = Track {
                active: true,
                start_sym,
                phase,
                q0: q0 as u16,
                sig_accum: 0,
                noise_accum: 0,
                s2_drift: 0,
                llr: [0; N_BITS],
            };
            // Backfill all symbols `[start_sym ..= ACQUIRE_AT_SYM]` already sitting in `mag_ring`.
            let s_usize = start_sym as usize;
            let ph_usize = phase as usize;
            for sym in s_usize..=ACQUIRE_AT_SYM {
                let rel = sym - s_usize;
                if rel < N_SYMBOLS {
                    let bins = self.mag_ring[sym % RING_SYMS][ph_usize];
                    self.feed_track_symbol(chosen_n, rel, &bins);
                }
            }
            chosen_n += 1;
        }
    }

    /// Clamped per-symbol contrast score `(clamped_contrast, raw_expected_energy)`:
    /// `3 * e[expected] - sum(other 3 tones)` clamped to `[-48, +48]` rejects +40 dB off-tone sidelobes, while
    /// `want` breaks high-SNR plateaus cleanly at the exact tone center.
    #[inline]
    fn contrast_sym(bins: &[u8; N_BINS], q0: u16, tone_q_off: &[u16; 4], expected: usize) -> (i32, i32) {
        let e0 = Self::sample_tone(bins, q0, tone_q_off, 0);
        let e1 = Self::sample_tone(bins, q0, tone_q_off, 1);
        let e2 = Self::sample_tone(bins, q0, tone_q_off, 2);
        let e3 = Self::sample_tone(bins, q0, tone_q_off, 3);
        let arr = [e0, e1, e2, e3];
        let sum = e0 + e1 + e2 + e3;
        let want = arr[expected & 3];
        ((4 * want - sum).clamp(-48, 48), want)
    }

    fn feed_track_symbol(&mut self, tr_i: usize, rel_sym: usize, bins: &[u8; N_BINS]) {
        let q0 = self.tracks[tr_i].q0;
        // Tail sync S2 (`104..110`): evaluate -1, 0, +1 half-bin drift (`+/- 5.86 Hz`) and adjust late symbols if needed.
        if rel_sym >= 104 {
            let k = rel_sym - 104;
            let w = S2[k] as usize;
            let c_neg = if q0 > 0 { Self::contrast_sym(bins, q0 - 1, &self.tone_q_off, w).0 } else { -48 };
            let c_zero = Self::contrast_sym(bins, q0, &self.tone_q_off, w).0;
            let c_pos = Self::contrast_sym(bins, q0 + 1, &self.tone_q_off, w).0;
            if c_pos > c_zero + 8 && c_pos >= c_neg {
                self.tracks[tr_i].s2_drift = self.tracks[tr_i].s2_drift.saturating_add(1);
            } else if c_neg > c_zero + 8 && c_neg > c_pos {
                self.tracks[tr_i].s2_drift = self.tracks[tr_i].s2_drift.saturating_sub(1);
            }
            return;
        }
        if rel_sym < 6 || (18..24).contains(&rel_sym) {
            // Sync symbols: accumulate signal vs noise estimate for SNR reporting.
            let w = if rel_sym < 6 { S0[rel_sym] } else { S1[rel_sym - 18] } as usize;
            let mut e = [0i32; 4];
            for (t, slot) in e.iter_mut().enumerate() {
                *slot = Self::sample_tone(bins, q0, &self.tone_q_off, t);
            }
            let sig = e[w];
            let noise = (e[0] + e[1] + e[2] + e[3] - sig) / 3;
            self.tracks[tr_i].sig_accum += sig;
            self.tracks[tr_i].noise_accum += noise;
            return;
        }

        // Data symbol index `d in 0..92`
        let d = if rel_sym < 18 { rel_sym - 6 } else { 12 + (rel_sym - 24) };
        if d >= N_DATA_SYMS {
            return;
        }

        let e0 = Self::sample_tone(bins, q0, &self.tone_q_off, 0);
        let e1 = Self::sample_tone(bins, q0, &self.tone_q_off, 1);
        let e2 = Self::sample_tone(bins, q0, &self.tone_q_off, 2);
        let e3 = Self::sample_tone(bins, q0, &self.tone_q_off, 3);

        // Gray map:
        // tone 0 -> (b0=0, b1=0)
        // tone 1 -> (b0=0, b1=1)
        // tone 2 -> (b0=1, b1=1)
        // tone 3 -> (b0=1, b1=0)
        // Positive LLR favors bit 0; negative LLR favors bit 1.
        // Clamp per-symbol LLR to [-24, +24] so a loud tone collision on a single symbol acts as a soft erasure
        // rather than overwhelming the LDPC belief propagation!
        let llr_b0 = (e0.max(e1) - e2.max(e3)).clamp(-24, 24) as i8;
        let llr_b1 = (e0.max(e3) - e1.max(e2)).clamp(-24, 24) as i8;

        self.tracks[tr_i].llr[2 * d] = llr_b0;
        self.tracks[tr_i].llr[2 * d + 1] = llr_b1;
    }

    /// Run serial normalized min-sum LDPC + CRC-14 decoding across all admitted tracks at end of slot, writing unique
    /// valid frames into `out` and returning the count decoded.
    pub fn finish_slot(&self, out: &mut [Decoded; MAX_DECODED]) -> usize {
        let mut scratch = BpScratch::new();
        let mut deinter = [0i8; N_BITS];
        let mut n_out = 0usize;

        for tr in &self.tracks {
            if !tr.active || n_out >= MAX_DECODED {
                continue;
            }
            for code_i in 0..N_BITS {
                deinter[code_i] = tr.llr[interleave_idx(code_i)];
            }
            let Some(bits78) = ldpc::decode_bp(&deinter, &mut scratch, LDPC_MAX_ITER) else {
                continue;
            };
            let Some(frame) = Frame::from_bits78(&bits78) else {
                continue;
            };

            // Deduplicate if two adjacent half-bin / half-symbol tracks converged to the same frame.
            if out[..n_out].iter().any(|d| d.frame == frame) {
                continue;
            }

            // Audio frequency in Hz: (FIRST_BIN + q0 / 2.0) * 6000 / 512 = ((2 * FIRST_BIN + q0) * 3000 + 256) / 512
            let audio_hz = (((2 * FIRST_BIN as u32 + tr.q0 as u32) * (PROC_RATE_HZ / 2) + 256) / FFT_N as u32) as u16;
            let time_offset_ms = ((tr.start_sym as i16 - NOMINAL_START_SYM as i16) * 80) + (tr.phase as i16 * 40);

            // Estimate SNR referenced to 2,500 Hz BW:
            // Per-bin noise BW (Hann window ENBW = 1.5 * 12.5 Hz = 18.75 Hz); 2500 / 18.75 = 133.3 -> -21.25 dB offset.
            // Each unit of `sig_accum - noise_accum` over 12 sync symbols is `0.376 / 12` dB of (S+N)/N.
            let diff_db_x10 = ((tr.sig_accum - tr.noise_accum).max(0) * 376) / (12 * 10);
            let snr_2500 = (diff_db_x10 - 212) / 10;
            let snr_db = snr_2500.clamp(-24, 20) as i8;

            out[n_out] = Decoded { frame, audio_hz, time_offset_ms, snr_db };
            n_out += 1;
        }
        let _ = data_sym_pos;
        let _ = self.spacing_hz;
        n_out
    }
}

impl Default for Receiver {
    fn default() -> Self {
        Self::new()
    }
}

const fn compute_tone_q_off(spacing_hz: u32) -> [u16; 4] {
    // Half-bin units: 1 bin = PROC_RATE_HZ / FFT_N = 6000 / 512 Hz, so 1 half-bin = 3000 / 512 Hz.
    // q_off(t) = round(t * spacing_hz * 512 / 3000).
    let mut out = [0u16; 4];
    let mut t = 0usize;
    while t < 4 {
        out[t] = ((t as u32 * spacing_hz * FFT_N as u32 + (PROC_RATE_HZ / 4)) / (PROC_RATE_HZ / 2)) as u16;
        t += 1;
    }
    out
}
