//! `RAD950-60` 1-minute weak-signal streaming receiver (`5 × 10 s` v1 bursts in `:00..:49`,
//! `:50..:59` PA cooldown + decode turnaround).
//!
//! Fits strictly inside `ferrite_core::decoders::BUDGET` (`13,312` bytes, zero heap, `no_std`)
//! by folding all five 10-second sub-bursts into a single 5-bit-per-cell `116 × 150` spectrogram
//! (`10,875` bytes) and a 1,236-byte nominal-channel correction table, using 70 ms steady-hold rectangular FFTs (`udsp::fft`) and integer `log2_q8`
//! passband-normalized excess power.

use crate::frame::Frame;
use crate::ldpc::{self, BpScratch, N_BITS};
use crate::phy::{data_sym_pos, interleave_idx, N_DATA_SYMS, S0, S1, S2, SUBCHANNELS_HZ};
use crate::rx::{Decoded, FFT_M, FFT_N, FIRST_BIN, MAX_DECODED, N_BINS, SYM_SAMPLES};
use udsp::fft::{cfft, real_power};
use udsp::fir::Decimator;
use udsp::log::log2_q8;
use udsp::mix::lowpass;

/// Number of 80 ms symbols folded per 10 s sub-burst (`start_sym 1..=6` + `110` burst symbols = `116`).
/// Covers `[-320 ms, +80 ms]` around nominal `+400 ms` start (well within the `±150 ms` GPS TX gate).
pub const FOLD_SYMS: usize = 116;
const FOLD_CELLS: usize = FOLD_SYMS * N_BINS; // 17,400
const FOLD_BYTES: usize = (FOLD_CELLS * 5).div_ceil(8); // 10,875 bytes
const TONE_SPACING_Q4: usize = 68;
const RAMP_SAMPLES_6K: usize = 60; // 10 ms @ 6,000 Hz (skip ramp, integrate 70 ms steady hold)
const SECONDARY_CHANNELS: usize = SUBCHANNELS_HZ.len();
// Rounded half-bin positions relative to FIRST_BIN at the 6 kHz analysis rate.
static NOMINAL_HALF_BINS: [usize; SECONDARY_CHANNELS] = {
    let mut bins = [0; SECONDARY_CHANNELS];
    let mut i = 0;
    while i < bins.len() {
        bins[i] = (SUBCHANNELS_HZ[i] as usize * (FFT_N * 2) + 3_000) / 6_000 - FIRST_BIN * 2;
        assert!(i == 0 || bins[i - 1] < bins[i]);
        assert!((bins[i] * 8 + 3 * TONE_SPACING_Q4) / 16 + 1 < N_BINS);
        i += 1;
    }
    bins
};
const SECONDARY_FIRST_SYM: usize = 1 + data_sym_pos(0);
const SECONDARY_LAST_SYM: usize = FOLD_SYMS - crate::phy::N_SYMBOLS + data_sym_pos(N_DATA_SYMS - 1);
const SECONDARY_SYMS: usize = SECONDARY_LAST_SYM - SECONDARY_FIRST_SYM + 1;
/// Four 3-bit positive residuals per possible data-bearing folded symbol.
/// This is 1.5 bytes per tone quartet, 1,236 bytes in total.
const SECONDARY_BYTES: usize = (SECONDARY_CHANNELS * SECONDARY_SYMS * 4 * 3).div_ceil(8);

/// Decimation low-pass filter (`24 kHz -> 6 kHz`, identical to `urad950::rx`).
static LP_24K_TO_6K: [i16; 31] = lowpass::<31>(2_550.0 / 24_000.0);

/// Streaming `RAD950-60` receiver (`<= 13 KB`, zero heap).
pub struct Receiver60 {
    decim: Decimator<31>,
    pcm_ring: [i16; SYM_SAMPLES],
    pcm_pos: u16,
    decim_count: u32,
    subslot: u8,
    wf_col: [u8; 12],
    wf_ready: bool,
    /// Packed 5-bit unsigned folded spectrogram (`FOLD_SYMS * N_BINS` cells, `0..=31`).
    fold: [u8; FOLD_BYTES],
    /// Compact percentile-span tone residuals for the eight published channels.
    /// It adds 1,236 bytes while retaining the 13 KB decoder-slot limit.
    secondary: [u8; SECONDARY_BYTES],
}

// Compile-time check that `Receiver60` fits inside Ferrite's shared 13 KB `Decoder` union slot.
const _: () = assert!(core::mem::size_of::<Receiver60>() + 16 <= 13 * 1024);

impl Default for Receiver60 {
    fn default() -> Self {
        Self::new()
    }
}

impl Receiver60 {
    /// Zero-initialize a `Receiver60` directly in-place without constructing a `~12 KB` temporary on the stack.
    ///
    /// # Safety
    /// `dst` must be valid for writes and properly aligned for `Receiver60`.
    pub unsafe fn init_in_place(dst: *mut Self) {
        core::ptr::write_bytes(dst, 0, 1);
    }

    pub fn new() -> Self {
        Self {
            decim: Decimator::new(),
            pcm_ring: [0; SYM_SAMPLES],
            pcm_pos: 0,
            decim_count: 0,
            subslot: 0,
            wf_col: [0; 12],
            wf_ready: false,
            fold: [0; FOLD_BYTES],
            secondary: [0; SECONDARY_BYTES],
        }
    }

    /// Reset for a brand-new 60-second RX minute (clears folded spectrogram and sub-slot counter).
    pub fn reset(&mut self) {
        self.decim.reset();
        self.pcm_ring.fill(0);
        self.pcm_pos = 0;
        self.decim_count = 0;
        self.subslot = 0;
        self.wf_ready = false;
        self.fold.fill(0);
        self.secondary.fill(0);
    }

    /// Begin the next 10-second sub-burst (`:00, :10, :20, :30, :40`) inside the current 60 s minute,
    /// keeping the accumulated folded spectrogram intact.
    pub fn start_subslot(&mut self) {
        self.decim.reset();
        self.pcm_ring.fill(0);
        self.pcm_pos = 0;
        self.decim_count = 0;
        self.subslot = self.subslot.saturating_add(1);
    }

    /// Take the latest 24-bin (`586..2,344 Hz`) packed 4-bit waterfall column if ready.
    pub fn take_waterfall_col(&mut self) -> Option<[u8; 12]> {
        if self.wf_ready {
            self.wf_ready = false;
            Some(self.wf_col)
        } else {
            None
        }
    }

    /// Diagnostic snapshot: `(completed_symbols_in_subslot, subslots_seen)`.
    pub fn diag(&self) -> (u16, u8) {
        let syms = (self.decim_count / SYM_SAMPLES as u32).min(u16::MAX as u32) as u16;
        (syms, self.subslot)
    }

    #[inline]
    fn get_cell(&self, sym: usize, bin: usize) -> u8 {
        let cell = sym * N_BINS + bin;
        let bit_pos = cell * 5;
        let byte_i = bit_pos >> 3;
        let shift = bit_pos & 7;
        let lo = (self.fold[byte_i] as u16) >> shift;
        let hi = if shift > 3 && byte_i + 1 < FOLD_BYTES { (self.fold[byte_i + 1] as u16) << (8 - shift) } else { 0 };
        ((lo | hi) & 0x1F) as u8
    }

    #[inline]
    fn add_cell(&mut self, sym: usize, bin: usize, delta: u8) {
        let cur = self.get_cell(sym, bin);
        let nv = cur.saturating_add(delta).min(31) as u16;
        let cell = sym * N_BINS + bin;
        let bit_pos = cell * 5;
        let byte_i = bit_pos >> 3;
        let shift = bit_pos & 7;
        let mask0 = (0x1Fu16 << shift) & 0xFF;
        self.fold[byte_i] = (self.fold[byte_i] & !(mask0 as u8)) | (((nv << shift) & 0xFF) as u8);
        if shift > 3 && byte_i + 1 < FOLD_BYTES {
            let mask1 = 0x1Fu16 >> (8 - shift);
            self.fold[byte_i + 1] = (self.fold[byte_i + 1] & !(mask1 as u8)) | ((nv >> (8 - shift)) as u8);
        }
    }

    // Two four-tone records share three bytes. Each record occupies twelve bits.
    fn residual_address(channel: usize, sym: usize) -> (usize, usize) {
        let record = channel * SECONDARY_SYMS + (sym - SECONDARY_FIRST_SYM);
        (record * 3 / 2, (record & 1) * 4)
    }

    fn residual_word(&self, byte: usize) -> u16 {
        u16::from_le_bytes([self.secondary[byte], self.secondary[byte + 1]])
    }

    #[inline]
    fn quantize(value: i32, floor: i32, span: i32) -> i32 {
        (((value - floor + 3).max(0) * 6) / span).clamp(0, 6)
    }

    #[inline]
    fn add_secondary_residuals(&mut self, sym: usize, bins: &[u8; N_BINS], floor: i32, primary_span: i32) {
        if sym.wrapping_sub(SECONDARY_FIRST_SYM) >= SECONDARY_SYMS {
            return;
        }
        // Ignore the top five percent of bins when choosing the alternate span.
        // A single loud neighbour then cannot coarsen every nominal channel.
        let mut histogram = [0u8; 32];
        for &value in bins {
            histogram[usize::from(value >> 3)] += 1;
        }
        let mut above = 0usize;
        let mut upper = 0i32;
        for (bucket, &count) in histogram.iter().enumerate().rev() {
            above += usize::from(count);
            if above > N_BINS / 20 {
                upper = (bucket * 8 + 7) as i32;
                break;
            }
        }
        let percentile_span = (upper - floor).clamp(18, 96);
        if percentile_span >= primary_span {
            return;
        }
        for channel in 0..SECONDARY_CHANNELS {
            let f0_half = NOMINAL_HALF_BINS[channel];
            let (byte, shift) = Self::residual_address(channel, sym);
            let word = self.residual_word(byte);
            let previous = word >> shift;
            let mut packed = 0u16;
            for tone in 0..4u8 {
                // Quantize before interpolation, matching the order used by the
                // folded primary spectrum. Retain the positive q-level difference.
                let pos_x16 = f0_half * 8 + usize::from(tone) * TONE_SPACING_Q4;
                let bin = pos_x16 >> 4;
                let frac = (pos_x16 & 15) as i32;
                let p0 = Self::quantize(i32::from(bins[bin]), floor, primary_span);
                let p1 = Self::quantize(i32::from(bins[bin + 1]), floor, primary_span);
                let a0 = Self::quantize(i32::from(bins[bin]), floor, percentile_span);
                let a1 = Self::quantize(i32::from(bins[bin + 1]), floor, percentile_span);
                let primary_q4 = p0 * (16 - frac) + p1 * frac;
                let alternate_q4 = a0 * (16 - frac) + a1 * frac;
                let delta = ((alternate_q4 - primary_q4 + 8) >> 4).max(0) as u8;
                let prior = (previous >> (3 * tone)) & 7;
                packed |= (prior + u16::from(delta)).min(7) << (3 * tone);
            }
            let updated = (word & !(0xfff << shift)) | (packed << shift);
            self.secondary[byte..byte + 2].copy_from_slice(&updated.to_le_bytes());
        }
    }

    /// Push a chunk of `24,000 Hz` signed 16-bit audio samples.
    pub fn push_samples(&mut self, samples: &[i16]) {
        for &s in samples {
            if let Some(y) = self.decim.push(s, &LP_24K_TO_6K, 4) {
                self.pcm_ring[self.pcm_pos as usize] = y;
                self.pcm_pos = if self.pcm_pos as usize + 1 == SYM_SAMPLES { 0 } else { self.pcm_pos + 1 };
                self.decim_count = self.decim_count.wrapping_add(1);
                if self.decim_count.is_multiple_of(SYM_SAMPLES as u32) {
                    let sym = (self.decim_count / SYM_SAMPLES as u32) as usize - 1;
                    if sym < FOLD_SYMS {
                        self.on_symbol(sym);
                    }
                }
            }
        }
    }

    #[inline(never)]
    fn on_symbol(&mut self, sym: usize) {
        // Rectangular 70 ms steady-hold window (skipping the 10 ms raised-cosine ramp) zero-padded to 512 pts.
        let mut z = [0i16; FFT_N];
        let mut idx = (self.pcm_pos as usize + RAMP_SAMPLES_6K) % SYM_SAMPLES;
        let steady_n = SYM_SAMPLES - RAMP_SAMPLES_6K; // 420 samples = 70 ms
        for sample in z.iter_mut().take(steady_n) {
            *sample = self.pcm_ring[idx];
            idx = if idx + 1 == SYM_SAMPLES { 0 } else { idx + 1 };
        }
        let shift = cfft(&mut z, FFT_M);

        let mut cur_bins = [0u8; N_BINS];
        let mut out_i = 0usize;
        let mut sum = 0u32;
        real_power(&z, FFT_M, FIRST_BIN, 1, N_BINS, |_, pwr| {
            let lp = (log2_q8(pwr.max(1)) + ((2 * shift as i32 - 13) << 8)) >> 5;
            let v = lp.clamp(0, 255) as u8;
            cur_bins[out_i] = v;
            sum += v as u32;
            out_i += 1;
        });
        let floor = (sum / N_BINS as u32) as i32;
        let mut pk_all = 0u8;
        for &lp in &cur_bins {
            if lp > pk_all {
                pk_all = lp;
            }
        }
        // Adapt quantizer span to local SNR so weak signals keep ~1 dB/step resolution while
        // strong (+20 dB) signals never saturate adjacent sidelobes at `q = 6`.
        let span = (pk_all as i32 - floor).clamp(18, 96);
        for (b, &lp) in cur_bins.iter().enumerate() {
            let q = Self::quantize(i32::from(lp), floor, span) as u8;
            self.add_cell(sym, b, q);
        }
        self.add_secondary_residuals(sym, &cur_bins, floor, span);

        // Emit a 24-bin waterfall column every 3 symbols (~240 ms)
        if sym.is_multiple_of(3) {
            let mut col = [0u8; 12];
            for r in 0..24usize {
                let lo = r * N_BINS / 24;
                let hi = ((r + 1) * N_BINS / 24).max(lo + 1);
                let mut pk = 0u8;
                for &b in &cur_bins[lo..hi] {
                    if b > pk {
                        pk = b;
                    }
                }
                let lvl = (((pk as i32 - floor + 2).max(0) * 15) / 42).clamp(0, 15) as u8;
                if (r & 1) == 0 {
                    col[r >> 1] = lvl;
                } else {
                    col[r >> 1] |= lvl << 4;
                }
            }
            self.wf_col = col;
            self.wf_ready = true;
        }
    }

    #[inline]
    fn sample_tone_q4(&self, sym: usize, f0_half: usize, tone: u8) -> i32 {
        // 50 Hz tone spacing at 11.71875 Hz/bin -> 4.267 bins -> 68.27 in Q4
        let pos_x16 = (f0_half as u32) * 8 + (tone as u32) * TONE_SPACING_Q4 as u32;
        let b = (pos_x16 >> 4) as usize;
        let frac = (pos_x16 & 15) as i32;
        if b + 1 >= N_BINS || sym >= FOLD_SYMS {
            return 0;
        }
        let v0 = self.get_cell(sym, b) as i32;
        let v1 = self.get_cell(sym, b + 1) as i32;
        v0 * (16 - frac) + v1 * frac
    }

    fn decode_secondary(&self, channel: usize, start_sym: u8, scratch: &mut BpScratch) -> Option<Frame> {
        let f0_half = NOMINAL_HALF_BINS[channel];
        let mut inter_llr = [0i8; N_BITS];
        for dibit in 0..N_DATA_SYMS {
            let symbol = usize::from(start_sym) + data_sym_pos(dibit);
            let (byte, shift) = Self::residual_address(channel, symbol);
            let residuals = self.residual_word(byte) >> shift;
            let tones: [i32; 4] = core::array::from_fn(|tone| {
                let correction = ((residuals >> (3 * tone)) & 7) as i32;
                self.sample_tone_q4(symbol, f0_half, tone as u8) + correction * 16
            });
            inter_llr[2 * dibit] = (tones[0].max(tones[1]) - tones[2].max(tones[3])).clamp(-127, 127) as i8;
            inter_llr[2 * dibit + 1] = (tones[0].max(tones[3]) - tones[1].max(tones[2])).clamp(-127, 127) as i8;
        }
        let mut llr_i8 = [0i8; N_BITS];
        let mut max_abs = 1i8;
        for i in 0..N_BITS {
            let value = inter_llr[interleave_idx(i)];
            llr_i8[i] = value;
            max_abs = max_abs.max(value.abs());
        }
        for i in 0..N_BITS {
            llr_i8[i] = (i16::from(llr_i8[i]) * 60 / i16::from(max_abs)) as i8;
        }
        ldpc::decode_bp(&llr_i8, scratch, 35).and_then(|bits| Frame::from_bits78(&bits))
    }

    /// Run blind `S0 + S1 + S2` stacked-sync acquisition and `(184, 78)` LDPC + CRC-14 decoding
    /// across the folded 5-sub-burst spectrogram, returning up to `MAX_DECODED` unique frames.
    #[inline(never)]
    pub fn finish_minute(&mut self, out: &mut [Decoded; MAX_DECODED]) -> usize {
        self.finish_minute_with_counts(out).1
    }

    /// Returns (primary count, total count). Primary frames occupy the output prefix.
    pub fn finish_minute_with_counts(&mut self, out: &mut [Decoded; MAX_DECODED]) -> (usize, usize) {
        const MAX_CAND: usize = 10;
        let mut best: [(i32, u8, u16); MAX_CAND] = [(i32::MIN, 0, 0); MAX_CAND];
        let mut n_best = 0usize;
        // Keep every base-tone half-bin whose fourth tone can still interpolate from
        // two stored FFT bins.  The former `(N_BINS - 15) * 2` ceiling stopped at
        // half-bin 269, excluding the nearest 2175 Hz candidate (271) even though
        // its fourth tone remains inside the 2332 Hz stored passband.
        let max_f0_half = ((N_BINS - 1) * 16 - 1 - 3 * TONE_SPACING_Q4) / 8;

        let mut nominal_sync = [[(i32::MIN, 0u8); 2]; SECONDARY_CHANNELS];
        for start_sym in 1usize..=(FOLD_SYMS - 110) {
            let mut channel = 0;
            for f0_half in 0..=max_f0_half {
                let mut score = 0i32;
                for (k, &t) in S0.iter().enumerate() {
                    let s = start_sym + k;
                    let sig = self.sample_tone_q4(s, f0_half, t);
                    let bg = (self.sample_tone_q4(s, f0_half, 0)
                        + self.sample_tone_q4(s, f0_half, 1)
                        + self.sample_tone_q4(s, f0_half, 2)
                        + self.sample_tone_q4(s, f0_half, 3))
                        >> 2;
                    score += sig - bg;
                }
                for (k, &t) in S1.iter().enumerate() {
                    let s = start_sym + 18 + k;
                    let sig = self.sample_tone_q4(s, f0_half, t);
                    let bg = (self.sample_tone_q4(s, f0_half, 0)
                        + self.sample_tone_q4(s, f0_half, 1)
                        + self.sample_tone_q4(s, f0_half, 2)
                        + self.sample_tone_q4(s, f0_half, 3))
                        >> 2;
                    score += sig - bg;
                }
                for (k, &t) in S2.iter().enumerate() {
                    let s = start_sym + 104 + k;
                    let sig = self.sample_tone_q4(s, f0_half, t);
                    let bg = (self.sample_tone_q4(s, f0_half, 0)
                        + self.sample_tone_q4(s, f0_half, 1)
                        + self.sample_tone_q4(s, f0_half, 2)
                        + self.sample_tone_q4(s, f0_half, 3))
                        >> 2;
                    score += sig - bg;
                }

                if channel < SECONDARY_CHANNELS && f0_half == NOMINAL_HALF_BINS[channel] {
                    let ranked = &mut nominal_sync[channel];
                    if score > ranked[0].0 {
                        ranked[1] = ranked[0];
                        ranked[0] = (score, start_sym as u8);
                    } else if score > ranked[1].0 {
                        ranked[1] = (score, start_sym as u8);
                    }
                    channel += 1;
                }

                if n_best == MAX_CAND && score <= best[MAX_CAND - 1].0 {
                    continue;
                }
                // Suppress near-duplicates within +-3 half-bins
                let mut dup_idx = None;
                for (i, item) in best.iter().enumerate().take(n_best) {
                    if item.1 == start_sym as u8 && (item.2 as usize).abs_diff(f0_half) <= 3 {
                        dup_idx = Some(i);
                        break;
                    }
                }
                if let Some(di) = dup_idx {
                    if score > best[di].0 {
                        best[di] = (score, start_sym as u8, f0_half as u16);
                    }
                } else if n_best < MAX_CAND {
                    best[n_best] = (score, start_sym as u8, f0_half as u16);
                    n_best += 1;
                } else {
                    best[MAX_CAND - 1] = (score, start_sym as u8, f0_half as u16);
                }
                // Insertion sort descending by score
                for i in 1..n_best {
                    let mut j = i;
                    while j > 0 && best[j].0 > best[j - 1].0 {
                        best.swap(j, j - 1);
                        j -= 1;
                    }
                }
            }
        }

        let mut scratch = BpScratch::new();
        let mut n_out = 0usize;
        for &(score, start_sym, f0_half) in best.iter().take(n_best) {
            if n_out >= MAX_DECODED {
                break;
            }
            let mut inter_llr = [0i32; N_BITS];
            for d in 0..N_DATA_SYMS {
                let s = start_sym as usize + data_sym_pos(d);
                let m0 = self.sample_tone_q4(s, f0_half as usize, 0);
                let m1 = self.sample_tone_q4(s, f0_half as usize, 1);
                let m2 = self.sample_tone_q4(s, f0_half as usize, 2);
                let m3 = self.sample_tone_q4(s, f0_half as usize, 3);
                inter_llr[2 * d] = m0.max(m1) - m2.max(m3);
                inter_llr[2 * d + 1] = m0.max(m3) - m1.max(m2);
            }
            let mut deinter = [0i32; N_BITS];
            let mut max_abs = 1i32;
            for i in 0..N_BITS {
                let v = inter_llr[interleave_idx(i)];
                deinter[i] = v;
                let a = v.abs();
                if a > max_abs {
                    max_abs = a;
                }
            }
            let mut llr_i8 = [0i8; N_BITS];
            for i in 0..N_BITS {
                llr_i8[i] = ((deinter[i] * 60) / max_abs).clamp(-120, 120) as i8;
            }
            if let Some(bits78) = ldpc::decode_bp(&llr_i8, &mut scratch, 35) {
                if let Some(frame) = Frame::from_bits78(&bits78) {
                    let packed = frame.pack_u64();
                    if out[..n_out].iter().any(|d| d.frame.pack_u64() == packed) {
                        continue;
                    }
                    // Convert half-bin index back to Hz above USB dial: (FIRST_BIN + f0_half/2) * 6000 / 512
                    let audio_hz = (((FIRST_BIN as u32 * 2 + f0_half as u32) * 6000) / 1024) as u16;
                    let time_offset_ms = (start_sym as i16 - 5) * 80;
                    // Rough SNR estimate in dB (2500 Hz ref) from stacked sync score
                    let snr_db = (-22 + (score / 180)).clamp(-24, 10) as i8;
                    out[n_out] = Decoded { frame, audio_hz, time_offset_ms, snr_db };
                    n_out += 1;
                }
            }
        }

        let primary_count = n_out;
        // A loud adjacent caller can occupy several of the global sync candidates.
        // Try each nominal channel's two strongest timings, best first.
        for rank in 0..2 {
            for (channel, ranked) in nominal_sync.iter().enumerate() {
                let (score, start_sym) = ranked[rank];
                if n_out == MAX_DECODED {
                    break;
                }
                if out[..n_out].iter().any(|d| {
                    d.audio_hz.abs_diff(SUBCHANNELS_HZ[channel]) <= 18
                        && d.time_offset_ms == (start_sym as i16 - 5) * 80
                }) {
                    continue;
                }
                if let Some(frame) = self.decode_secondary(channel, start_sym, &mut scratch) {
                    if !out[..n_out].iter().any(|d| d.frame == frame) {
                        out[n_out] = Decoded {
                            frame,
                            audio_hz: SUBCHANNELS_HZ[channel],
                            time_offset_ms: (start_sym as i16 - 5) * 80,
                            snr_db: (-22 + score / 180).clamp(-24, 10) as i8,
                        };
                        n_out += 1;
                    }
                }
            }
        }
        (primary_count, n_out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::encode_grid4;
    use crate::phy::{encode_tones, Modulator, DEFAULT_SPACING_HZ};
    use crate::rx::INPUT_RATE_HZ;
    use crate::Callsign;

    struct Rng(u64);
    impl Rng {
        fn next_u32(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            (x >> 16) as u32
        }
        fn gauss(&mut self, sigma: f64) -> f64 {
            let mut s = 0.0;
            for _ in 0..12 {
                s += (self.next_u32() as f64) / (u32::MAX as f64 + 1.0);
            }
            (s - 6.0) * sigma
        }
    }

    #[test]
    fn receiver60_size_and_blind_weak_signal_decode() {
        assert!(core::mem::size_of::<Receiver60>() + 16 <= 13 * 1024);

        let nq6n = Callsign::encode(b"NQ6N").unwrap();
        let en61 = encode_grid4(b"EN61").unwrap();
        let frame = Frame::Cq { sender: nq6n, grid4: en61, tx_pwr: 11, reserved: 0 };
        let tones = encode_tones(frame);

        let sigma = 2000.0f64;
        let p_n_2500 = sigma * sigma * (2500.0 / (INPUT_RATE_HZ as f64 / 2.0));
        let mut rng = Rng(0x9506_0001_CAFE_BABE);
        let mut rx = Receiver60::new();

        // Verify blind decode at -19 dB (2500 Hz ref) across 5x 10s sub-bursts
        let amp = (2.0 * p_n_2500 * 10.0f64.powf(-19.0 / 10.0)).sqrt() as i16;
        let mut ok = 0usize;
        for _ in 0..6 {
            rx.reset();
            for sub in 0..5 {
                if sub > 0 {
                    rx.start_subslot();
                }
                let mut pcm = std::vec![0i16; INPUT_RATE_HZ as usize * 10];
                for s in &mut pcm {
                    *s = rng.gauss(sigma).clamp(-32_000.0, 32_000.0) as i16;
                }
                let mut m = Modulator::new(tones, INPUT_RATE_HZ, 1050, DEFAULT_SPACING_HZ, amp);
                let mut idx = (400 * INPUT_RATE_HZ as usize) / 1000;
                while let Some(v) = m.next_sample() {
                    pcm[idx] = pcm[idx].saturating_add(v);
                    idx += 1;
                }
                for chunk in pcm.chunks(192) {
                    rx.push_samples(chunk);
                }
            }
            let mut out = [Decoded::EMPTY; MAX_DECODED];
            let n = rx.finish_minute(&mut out);
            if out[..n].iter().any(|d| d.frame == frame) {
                ok += 1;
            }
        }
        assert!(ok >= 5, "Receiver60 blind decode at -19 dB too low: {ok}/6");

        // Noise-only false-accept control (10 full 50s trials)
        let mut fa = 0usize;
        for _ in 0..10 {
            rx.reset();
            for sub in 0..5 {
                if sub > 0 {
                    rx.start_subslot();
                }
                let mut pcm = std::vec![0i16; INPUT_RATE_HZ as usize * 10];
                for s in &mut pcm {
                    *s = rng.gauss(sigma).clamp(-32_000.0, 32_000.0) as i16;
                }
                for chunk in pcm.chunks(192) {
                    rx.push_samples(chunk);
                }
            }
            let mut out = [Decoded::EMPTY; MAX_DECODED];
            if rx.finish_minute(&mut out) > 0 {
                fa += 1;
            }
        }
        assert_eq!(fa, 0, "Receiver60 noise-only false accepts must be 0");
    }

    #[test]
    fn receiver60_blind_acquires_upper_subchannel() {
        let nq6n = Callsign::encode(b"NQ6N").unwrap();
        let en61 = encode_grid4(b"EN61").unwrap();
        let frame = Frame::Cq { sender: nq6n, grid4: en61, tx_pwr: 11, reserved: 0 };
        let tones = encode_tones(frame);
        let mut rx = Receiver60::new();

        for sub in 0..5 {
            if sub > 0 {
                rx.start_subslot();
            }
            let mut pcm = std::vec![0i16; INPUT_RATE_HZ as usize * 10];
            let mut m = Modulator::new(tones, INPUT_RATE_HZ, 2175, DEFAULT_SPACING_HZ, 4_000);
            let mut idx = (400 * INPUT_RATE_HZ as usize) / 1000;
            while let Some(v) = m.next_sample() {
                pcm[idx] = pcm[idx].saturating_add(v);
                idx += 1;
            }
            for chunk in pcm.chunks(192) {
                rx.push_samples(chunk);
            }
        }

        let mut out = [Decoded::EMPTY; MAX_DECODED];
        let n = rx.finish_minute(&mut out);
        let decoded = out[..n].iter().find(|d| d.frame == frame).expect("2175 Hz CQ must decode");
        assert!(decoded.audio_hz.abs_diff(2175) <= 8, "decoded at {} Hz", decoded.audio_hz);
    }
}
