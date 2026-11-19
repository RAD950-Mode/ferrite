//! SOL950 streaming receiver fitting strictly inside `ferrite_core::decoders::BUDGET` (`13,312` bytes),
//! including internal LDPC belief-propagation scratch (`work` shared with the early-acquisition ring buffer).
//!
//! Pipeline:
//! 1. `24,000 Hz -> 6,000 Hz` 4:1 low-pass FIR decimator (`udsp::fir::Decimator<31>`).
//! 2. Sliding 480-sample (`80 ms`) Hann-windowed 512-point real FFT (`udsp::fft`) evaluated every `240` samples
//!    (`40 ms` half-symbol phase `0` and `1`), with block-exponent (`1 << shift`) log-power normalization.
//! 3. `32-symbol x 2-phase x 150-bin` packed ring buffer (`9,600 B` in `work`) holding `586..2,344 Hz` across all
//!    arrival skews `start_sym in 1..=9` (`[-320 ms, +360 ms]` relative to nominal `+400 ms` start).
//! 4. Clamped contrast `S0 + S1` 2D acquisition at slot symbol `32` (`t = 2.64 s`) admitting `K = 6` tracks with
//!    frequency non-maximum suppression and separated-peak fairness reservation, backfilling `data[0..12]` and
//!    streaming `data[12..92]` across 3 linear drift branches (`-11.7 Hz, 0 Hz, +11.7 Hz` over the burst) selected
//!    at end-of-burst by `S2` (`104..110`) tail-sync scores.
//! 5. Serial normalized min-sum `(184, 78)` LDPC + CRC-14 decoding at end of slot reusing `work` for BP scratch.

use crate::frame::Frame;
use crate::ldpc::{self, N_BITS, N_EDGES};
use crate::phy::{interleave_idx, DEFAULT_SPACING_HZ, N_DATA_SYMS, N_SYMBOLS, S0, S1, S2};
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

/// First FFT bin stored (`50 * 6000 / 512 = 585.9 Hz`) and number of bins (`50..200`, up to `2,332.0 Hz`).
pub const FIRST_BIN: usize = 50;
pub const N_BINS: usize = 150;

/// Ring depth in whole symbols (`1..=32` covers `start_sym in 1..=9` through `S1` end `9 + 23 = 32`).
pub const RING_SYMS: usize = 32;
pub const MIN_START_SYM: usize = 1; // +80 ms (-320 ms vs +400 ms nominal)
pub const NOMINAL_START_SYM: usize = 5; // +400 ms nominal
pub const MAX_START_SYM: usize = 9; // +720 ms (+320 ms vs +400 ms nominal; +360 ms on phase 1)
pub const ACQUIRE_AT_SYM: usize = MAX_START_SYM + 23; // 32

pub const MAX_TRACKS: usize = 8;
pub const MAX_DECODED: usize = 8;
pub const LDPC_MAX_ITER: usize = 35;

/// Data-symbol index (`36..92`, i.e. `rel_sym = 48..104`) from which negative/positive linear drift branches diverge:
/// - `rel_sym in 48..83` (`d in 36..71`, 35 symbols): `+/- 1` half-bin (`+/- 5.86 Hz`) intermediate correction
/// - `rel_sym in 83..104` (`d in 71..92`, 21 symbols): `+/- 2` half-bins (`+/- 11.72 Hz`) end-of-burst correction
pub const DRIFT_START_DATA: usize = 36;
const TAIL_DATA_SYMS: usize = N_DATA_SYMS - DRIFT_START_DATA; // 56 dibits (packed 1 byte per dibit = 56 bytes)

/// Half-bin drift magnitude (`0`, `1`, or `2`) applied along the negative/positive linear drift trajectories at
/// burst-relative symbol `rel_sym in 0..110` (nearest-integer rounding of a `0 -> 2` half-bin ramp from `S0/S1`
/// center `rel_sym = 12` to `S2` center `rel_sym = 107`).
#[inline]
pub const fn drift_step_half_bins(rel_sym: usize) -> u16 {
    if rel_sym < 48 {
        0
    } else if rel_sym < 83 {
        1
    } else {
        2
    }
}

#[inline]
fn pack_dibit_i4(b0: i8, b1: i8) -> u8 {
    // Scale `[-24, +24]` by `/ 3` into signed 4-bit `[-8, +7]` nibbles.
    let q0 = ((b0 as i16 + if b0 >= 0 { 1 } else { -1 }) / 3).clamp(-8, 7) as i8;
    let q1 = ((b1 as i16 + if b1 >= 0 { 1 } else { -1 }) / 3).clamp(-8, 7) as i8;
    (((q0 as u8) & 0x0F) << 4) | ((q1 as u8) & 0x0F)
}

#[inline]
fn unpack_dibit_i4(packed: u8) -> (i8, i8) {
    // Sign-extend 4-bit nibbles and restore `* 3` LLR scale (`[-24, +21]`).
    let q0 = ((packed as i8) >> 4) * 3;
    let q1 = (((packed << 4) as i8) >> 4) * 3;
    (q0, q1)
}

/// Packed `i16` length holding `RING_SYMS * 2 * N_BINS = 9,600` bytes during capture, and reused at `finish_slot`
/// for `post: [i16; 184]` + `c2v: [i8; 525]` LDPC belief-propagation scratch (`447` `i16` words < `4,800`).
const WORK_BYTES: usize = RING_SYMS * 2 * N_BINS; // 9,600
const WORK_I16: usize = WORK_BYTES / 2; // 4,800

static DECIM_TAPS: [i16; 31] = lowpass::<31>(2500.0 / 24000.0);
static HANN_480: [i16; SYM_SAMPLES] = hann_table();
/// `2^(k/8) * 256` for `k = 0..8`, used to reconstruct unbiased linear power from `1/8`-octave log bins.
static EXP8: [u32; 8] = [256, 279, 304, 332, 362, 395, 431, 470];

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

/// Reconstruct scaled linear power (`u64`) from a quantized `1/8`-octave log-power byte `lp`.
#[inline]
fn lin_power(lp: u8) -> u64 {
    let oct = (lp >> 3).min(31) as u32;
    let frac = (lp & 7) as usize;
    (EXP8[frac] as u64) << oct
}

/// Decoded frame together with its measured physical-layer metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decoded {
    pub frame: Frame,
    /// Estimated lowest-tone audio frequency offset in Hz above USB dial (at start of burst).
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
    /// Sum of linear power on the 18 active sync tones (`S0 + S1 + S2`, evaluated along winning drift).
    on_pwr_sum: u64,
    /// Sum of linear power across the `18 * 3 = 54` inactive sync tones (`3 x` noise per bin).
    off_pwr_sum: u64,
    /// `S2` (`104..110`) contrast scores for end-of-burst drift offsets `[-2, 0, +2]` half-bins (`-11.7, 0, +11.7 Hz`).
    s2_scores: [i16; 3],
    /// Zero-drift LLRs across all 92 data symbols (`184` bits).
    llr_zero: [i8; N_BITS],
    /// Packed 4-bit dibit LLRs for `d = 36..92` along negative linear drift (`-1` then `-2` half-bins).
    llr_neg_tail: [u8; TAIL_DATA_SYMS],
    /// Packed 4-bit dibit LLRs for `d = 36..92` along positive linear drift (`+1` then `+2` half-bins).
    llr_pos_tail: [u8; TAIL_DATA_SYMS],
}

impl Track {
    const fn empty() -> Self {
        Self {
            active: false,
            start_sym: 0,
            phase: 0,
            q0: 0,
            on_pwr_sum: 0,
            off_pwr_sum: 0,
            s2_scores: [0; 3],
            llr_zero: [0; N_BITS],
            llr_neg_tail: [0; TAIL_DATA_SYMS],
            llr_pos_tail: [0; TAIL_DATA_SYMS],
        }
    }
}

/// Streaming SOL950 slot receiver (`<= 13,312` bytes including internal LDPC scratch, zero heap allocation).
pub struct Receiver {
    decim: Decimator<31>,
    pcm_ring: [i16; SYM_SAMPLES],
    pcm_pos: usize,
    decim_count: u32,
    tone_q_off: [u16; 4],
    work: [i16; WORK_I16],
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
            tone_q_off,
            work: [0; WORK_I16],
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
        self.work.fill(0);
        self.tracks = [Track::empty(); MAX_TRACKS];
        self.acquired = false;
    }

    /// Diagnostic hook: bypass blind `S0 + S1` acquisition and pin track 0 to the nearest `(start_sym, phase, q0)`
    /// for a known burst start time (`start_ms` from slot edge) and base tone (`audio_hz`). Call right after `reset()`.
    pub fn set_oracle_track(&mut self, start_ms: u32, audio_hz: u32) {
        let h = ((start_ms + 20) / 40) as usize;
        let start_sym = (h / 2) as u8;
        let phase = (h % 2) as u8;
        let half_bin = ((audio_hz as u64 * (FFT_N as u64 * 2) + (PROC_RATE_HZ as u64 / 2)) / PROC_RATE_HZ as u64) as usize;
        let q0 = half_bin.saturating_sub(2 * FIRST_BIN).min((N_BINS - 2) * 2) as u16;
        self.tracks[0] = Track { active: true, start_sym, phase, q0, ..Track::empty() };
        self.acquired = true;
    }

    #[inline]
    fn ring_offset(sym_mod: usize, phase: usize, bin: usize) -> usize {
        (sym_mod * 2 + phase) * N_BINS + bin
    }

    #[inline]
    fn write_ring_bins(&mut self, sym_mod: usize, phase: usize, bins: &[u8; N_BINS]) {
        let base = Self::ring_offset(sym_mod, phase, 0) >> 1;
        for i in 0..(N_BINS / 2) {
            self.work[base + i] = i16::from_le_bytes([bins[2 * i], bins[2 * i + 1]]);
        }
    }

    #[inline]
    fn read_ring_bins(&self, sym_mod: usize, phase: usize) -> [u8; N_BINS] {
        let base = Self::ring_offset(sym_mod, phase, 0) >> 1;
        let mut out = [0u8; N_BINS];
        for i in 0..(N_BINS / 2) {
            let [lo, hi] = self.work[base + i].to_le_bytes();
            out[2 * i] = lo;
            out[2 * i + 1] = hi;
        }
        out
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
        let mut z = [0i16; FFT_N];
        let mut idx = self.pcm_pos;
        for (n, &w) in HANN_480.iter().enumerate() {
            let s = self.pcm_ring[idx] as i32;
            idx = if idx + 1 == SYM_SAMPLES { 0 } else { idx + 1 };
            z[n] = ((s * w as i32 + 16_384) >> 15) as i16;
        }
        let shift = cfft(&mut z, FFT_M);

        let mut cur_bins = [0u8; N_BINS];
        let mut out_i = 0usize;
        real_power(&z, FFT_M, FIRST_BIN, 1, N_BINS, |_, pwr| {
            let lp = (log2_q8(pwr.max(1)) + ((2 * shift as i32 - 13) << 8)) >> 5;
            cur_bins[out_i] = lp.clamp(0, 255) as u8;
            out_i += 1;
        });

        if !self.acquired {
            self.write_ring_bins(sym % RING_SYMS, phase, &cur_bins);
            if sym == ACQUIRE_AT_SYM && phase == 1 {
                self.acquire_candidates();
                self.acquired = true;
            }
            return;
        }

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

    fn acquire_candidates(&mut self) {
        let max_q0 = ((N_BINS - 2) * 2).saturating_sub(self.tone_q_off[3] as usize) as u16;
        let mut best_score = [i16::MIN; 300];
        let mut best_sp = [0u8; 300];
        let n_q = (max_q0 as usize + 1).min(300);

        for s in MIN_START_SYM..=MAX_START_SYM {
            for phase in 0..2usize {
                let mut s0_bins = [[0u8; N_BINS]; 6];
                let mut s1_bins = [[0u8; N_BINS]; 6];
                for k in 0..6 {
                    s0_bins[k] = self.read_ring_bins((s + k) % RING_SYMS, phase);
                    s1_bins[k] = self.read_ring_bins((s + 18 + k) % RING_SYMS, phase);
                }
                for q0 in 0..n_q {
                    let mut contrast = 0i32;
                    let mut raw_sum = 0i32;
                    for (k, &w) in S0.iter().enumerate() {
                        let (c, rw) = Self::contrast_sym(&s0_bins[k], q0 as u16, &self.tone_q_off, w as usize);
                        contrast += c;
                        raw_sum += rw;
                    }
                    for (k, &w) in S1.iter().enumerate() {
                        let (c, rw) = Self::contrast_sym(&s1_bins[k], q0 as u16, &self.tone_q_off, w as usize);
                        contrast += c;
                        raw_sum += rw;
                    }
                    let score = contrast * 8 + (raw_sum >> 4);
                    let sc = score.clamp(i16::MIN as i32 + 1, i16::MAX as i32) as i16;
                    if sc > best_score[q0] {
                        best_score[q0] = sc;
                        best_sp[q0] = ((s as u8) << 1) | (phase as u8);
                    }
                }
            }
        }

        let mut chosen_n = 0usize;
        for slot in 0..MAX_TRACKS {
            let min_sep: usize = if slot < MAX_TRACKS - 2 { 8 } else { 32 };
            let mut pick_q = None;
            let mut pick_sc = 288i16;
            for q0 in 0..n_q {
                let sc = best_score[q0];
                if sc <= pick_sc {
                    continue;
                }
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
                ..Track::empty()
            };
            let s_usize = start_sym as usize;
            let ph_usize = phase as usize;
            for sym in s_usize..=ACQUIRE_AT_SYM {
                let rel = sym - s_usize;
                if rel < N_SYMBOLS {
                    let bins = self.read_ring_bins(sym % RING_SYMS, ph_usize);
                    self.feed_track_symbol(chosen_n, rel, &bins);
                }
            }
            chosen_n += 1;
        }
    }

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

    #[inline]
    fn extract_dibit_llr(bins: &[u8; N_BINS], q: u16, tone_q_off: &[u16; 4]) -> (i8, i8) {
        let e0 = Self::sample_tone(bins, q, tone_q_off, 0);
        let e1 = Self::sample_tone(bins, q, tone_q_off, 1);
        let e2 = Self::sample_tone(bins, q, tone_q_off, 2);
        let e3 = Self::sample_tone(bins, q, tone_q_off, 3);
        let b0 = (e0.max(e1) - e2.max(e3)).clamp(-24, 24) as i8;
        let b1 = (e0.max(e3) - e1.max(e2)).clamp(-24, 24) as i8;
        (b0, b1)
    }

    fn feed_track_symbol(&mut self, tr_i: usize, rel_sym: usize, bins: &[u8; N_BINS]) {
        let q0 = self.tracks[tr_i].q0;

        // Tail sync S2 (`104..110`): score the 3 drift endpoints `[-2, 0, +2]` half-bins (`-11.7, 0, +11.7 Hz`),
        // including the raw energy tie-breaker so high-SNR frames don't tie at the +48 contrast ceiling.
        if rel_sym >= 104 {
            let k = rel_sym - 104;
            let w = S2[k] as usize;
            let q_neg = q0.saturating_sub(2);
            let q_pos = q0.saturating_add(2);
            let (cn, wn) = Self::contrast_sym(bins, q_neg, &self.tone_q_off, w);
            let (cz, wz) = Self::contrast_sym(bins, q0, &self.tone_q_off, w);
            let (cp, wp) = Self::contrast_sym(bins, q_pos, &self.tone_q_off, w);
            self.tracks[tr_i].s2_scores[0] = self.tracks[tr_i].s2_scores[0].saturating_add((cn * 4 + (wn >> 2)) as i16);
            self.tracks[tr_i].s2_scores[1] = self.tracks[tr_i].s2_scores[1].saturating_add((cz * 4 + (wz >> 2)) as i16);
            self.tracks[tr_i].s2_scores[2] = self.tracks[tr_i].s2_scores[2].saturating_add((cp * 4 + (wp >> 2)) as i16);
            return;
        }

        // Early sync symbols `S0` (`0..6`) and `S1` (`18..24`): accumulate unbiased linear power on active vs
        // inactive sync tones for calibrated 2,500 Hz SNR estimation.
        if rel_sym < 6 || (18..24).contains(&rel_sym) {
            let w = if rel_sym < 6 { S0[rel_sym] } else { S1[rel_sym - 18] } as usize;
            for t in 0..4 {
                let lp = Self::sample_tone(bins, q0, &self.tone_q_off, t).clamp(0, 255) as u8;
                let p = lin_power(lp);
                if t == w {
                    self.tracks[tr_i].on_pwr_sum = self.tracks[tr_i].on_pwr_sum.saturating_add(p);
                } else {
                    self.tracks[tr_i].off_pwr_sum = self.tracks[tr_i].off_pwr_sum.saturating_add(p);
                }
            }
            return;
        }

        let d = if rel_sym < 18 { rel_sym - 6 } else { 12 + (rel_sym - 24) };
        if d >= N_DATA_SYMS {
            return;
        }

        let (b0, b1) = Self::extract_dibit_llr(bins, q0, &self.tone_q_off);
        self.tracks[tr_i].llr_zero[2 * d] = b0;
        self.tracks[tr_i].llr_zero[2 * d + 1] = b1;

        // Drift-divergent data symbols (`d = 36..92`, `rel_sym = 48..104`): record negative and positive linear
        // drift trajectories using `drift_step_half_bins(rel_sym)` (`1` half-bin on `48..83`, `2` on `83..104`).
        if d >= DRIFT_START_DATA {
            let step = drift_step_half_bins(rel_sym);
            let (nb0, nb1) = Self::extract_dibit_llr(bins, q0.saturating_sub(step), &self.tone_q_off);
            let (pb0, pb1) = Self::extract_dibit_llr(bins, q0.saturating_add(step), &self.tone_q_off);
            let ti = d - DRIFT_START_DATA;
            self.tracks[tr_i].llr_neg_tail[ti] = pack_dibit_i4(nb0, nb1);
            self.tracks[tr_i].llr_pos_tail[ti] = pack_dibit_i4(pb0, pb1);
        }
    }

    /// Run serial normalized min-sum LDPC + CRC-14 decoding across all admitted tracks at end of slot, reusing
    /// `self.work` (`[i16; 4800]`) for `post` (`184`) and `c2v` (`525`) scratch and ranking drift branches by `S2`.
    pub fn finish_slot(&mut self, out: &mut [Decoded; MAX_DECODED]) -> usize {
        let (post_slice, rest) = self.work.split_at_mut(N_BITS);
        let post: &mut [i16; N_BITS] = post_slice.try_into().unwrap();
        let c2v: &mut [i16; N_EDGES] = (&mut rest[..N_EDGES]).try_into().unwrap();

        let mut raw_llr = [0i8; N_BITS];
        let mut deinter = [0i8; N_BITS];
        let mut n_out = 0usize;

        for tr_i in 0..MAX_TRACKS {
            if !self.tracks[tr_i].active || n_out >= MAX_DECODED {
                continue;
            }
            // Rank the 3 drift branches `0 = neg (-2), 1 = zero (0), 2 = pos (+2)` by measured `S2` tail-sync score
            // (with small +6 tie-break bias toward zero drift when stationary).
            let sc = [
                self.tracks[tr_i].s2_scores[0],
                self.tracks[tr_i].s2_scores[1].saturating_add(16),
                self.tracks[tr_i].s2_scores[2],
            ];
            let mut order = [1usize, 0, 2];
            if sc[order[1]] > sc[order[0]] {
                order.swap(0, 1);
            }
            if sc[order[2]] > sc[order[1]] {
                order.swap(1, 2);
                if sc[order[1]] > sc[order[0]] {
                    order.swap(0, 1);
                }
            }

            let mut decoded_frame = None;
            for &branch in &order {
                raw_llr.copy_from_slice(&self.tracks[tr_i].llr_zero);
                if branch == 0 {
                    for (ti, &pk) in self.tracks[tr_i].llr_neg_tail.iter().enumerate() {
                        let (b0, b1) = unpack_dibit_i4(pk);
                        let bit_i = (DRIFT_START_DATA + ti) * 2;
                        raw_llr[bit_i] = b0;
                        raw_llr[bit_i + 1] = b1;
                    }
                } else if branch == 2 {
                    for (ti, &pk) in self.tracks[tr_i].llr_pos_tail.iter().enumerate() {
                        let (b0, b1) = unpack_dibit_i4(pk);
                        let bit_i = (DRIFT_START_DATA + ti) * 2;
                        raw_llr[bit_i] = b0;
                        raw_llr[bit_i + 1] = b1;
                    }
                }
                for code_i in 0..N_BITS {
                    deinter[code_i] = raw_llr[interleave_idx(code_i)];
                }
                if let Some(bits78) = ldpc::decode_bp_with_slices(&deinter, post, c2v, LDPC_MAX_ITER) {
                    if let Some(f) = Frame::from_bits78(&bits78) {
                        decoded_frame = Some(f);
                        break;
                    }
                }
            }

            let Some(frame) = decoded_frame else {
                continue;
            };
            if out[..n_out].iter().any(|d| d.frame == frame) {
                continue;
            }

            let tr = &self.tracks[tr_i];
            let audio_hz = (((2 * FIRST_BIN as u32 + tr.q0 as u32) * (PROC_RATE_HZ / 2) + 256) / FFT_N as u32) as u16;
            let time_offset_ms = ((tr.start_sym as i16 - NOMINAL_START_SYM as i16) * 80) + (tr.phase as i16 * 40);
            let snr_db = estimate_snr_2500_db(tr.on_pwr_sum, tr.off_pwr_sum);

            out[n_out] = Decoded { frame, audio_hz, time_offset_ms, snr_db };
            n_out += 1;
        }
        n_out
    }

    #[cfg(test)]
    pub(crate) fn top_track_drift_info(&self) -> Option<([i16; 3], usize, usize)> {
        let tr = self.tracks.iter().find(|t| t.active)?;
        let mut diff_step1 = 0usize;
        let mut diff_step2 = 0usize;
        for ti in 0..TAIL_DATA_SYMS {
            let d = DRIFT_START_DATA + ti;
            let (pb0, pb1) = unpack_dibit_i4(tr.llr_pos_tail[ti]);
            let (z0, z1) = unpack_dibit_i4(pack_dibit_i4(tr.llr_zero[2 * d], tr.llr_zero[2 * d + 1]));
            if (pb0, pb1) != (z0, z1) {
                if d < 71 {
                    diff_step1 += 1;
                } else {
                    diff_step2 += 1;
                }
            }
        }
        Some((tr.s2_scores, diff_step1, diff_step2))
    }
}

/// Convert accumulated linear sync power (`on_pwr_sum` across 12 active sync bins, `off_pwr_sum` across 36 inactive
/// sync bins) into unbiased SNR in dB referenced to a `2,500 Hz` noise bandwidth.
fn estimate_snr_2500_db(on_pwr_sum: u64, off_pwr_sum: u64) -> i8 {
    // Mean noise per bin = off_pwr_sum / 36; mean (S + N) per bin = on_pwr_sum / 12 = (3 * on_pwr_sum) / 36.
    let n_36 = off_pwr_sum.max(1);
    let sn_36 = on_pwr_sum.saturating_mul(3);
    // Floor pure signal at 1/16 of per-bin noise (`-12 dB` per-bin = `-33 dB` in 2500 Hz) if noisy fluctuation dips.
    let s_36 = sn_36.saturating_sub(n_36).max((n_36 >> 4).max(1));

    // 10 * log10(s_36 / n_36) = 3.0103 * (log2(s_36) - log2(n_36)).
    // In Q8 (`1/256` log2), `d_q8 * 771 >> 16` = `d_q8 * (3.0117 / 256)` in tenths of a dB (`x10`)? Wait:
    // Let's check exact arithmetic in tenths of a dB (`0.1 dB` units)!
    // `log2_q8` is `256 * log2(x)`.
    // `100 * log10(x) = 30.103 * log2(x) = (log2_q8(x) * 3010 + 12800) / 25600` (in `0.1 dB` units!).
    let diff_q8 = log2_q8(s_36) - log2_q8(n_36);
    let bin_snr_db_x10 = (diff_q8 * 301 + 1280) / 2560;

    // 480-sample (`80 ms`) Hann window has ENBW = `1.5 * 12.5 Hz = 18.75 Hz`.
    // `10 * log10(18.75 / 2500) = -21.25 dB`, plus `+0.55 dB` average half-bin/Hann scalloping loss = `-20.7 dB` (`-207`).
    let snr_2500_x10 = bin_snr_db_x10 - 207;
    let rounded = if snr_2500_x10 >= 0 {
        (snr_2500_x10 + 5) / 10
    } else {
        (snr_2500_x10 - 5) / 10
    };
    rounded.clamp(-24, 20) as i8
}

impl Default for Receiver {
    fn default() -> Self {
        Self::new()
    }
}

const fn compute_tone_q_off(spacing_hz: u32) -> [u16; 4] {
    let mut out = [0u16; 4];
    let mut t = 0usize;
    while t < 4 {
        out[t] = ((t as u32 * spacing_hz * FFT_N as u32 + (PROC_RATE_HZ / 4)) / (PROC_RATE_HZ / 2)) as u16;
        t += 1;
    }
    out
}
