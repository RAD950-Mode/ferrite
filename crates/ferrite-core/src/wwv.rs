//! Lightweight WWV / WWVH / CHU shortwave time-signal synchronizer.
//!
//! Designed to run inside the shared `Decoder` union at `24,000 Hz` with zero
//! heap allocation (~500 bytes total RAM) using the SI4732 USB receiver tuned
//! `1,000 Hz` below carrier (e.g. `9,999.0 kHz` USB for `10.000 MHz` WWV):
//! - AM carrier lands at `f_c ≈ 1,000 Hz` (tolerant of `±600 Hz` SI4732 crystal
//!   or `rx_corr_hz` error — anywhere in `400..1,700 Hz` stays inside the 3.0 kHz
//!   USB filter along with its `+1,000 Hz` upper sideband at `1,400..2,700 Hz`).
//! - Square-law AM envelope demodulation `y[n] = (x[n]^2) >> 14` beats the
//!   carrier (`f_c`) against the `1,000 Hz` tick sideband (`f_c + 1,000 Hz`)
//!   to produce an **exact `1,000.000 Hz`** tone independent of crystal offset!
//! - A triangle-windowed 10 ms DFT (`240` samples) compares `1,000 Hz` against
//!   a `600 Hz` noise reference (`24,000 / 40 = 600 Hz`), keeping the `2 * f_c`
//!   carrier 2nd harmonic (`1,400..3,400 Hz`) well clear of both bins.
//!
//! A `100`-bin (`10 ms/bin`) 1 Hz comb filter locks the exact second edge within
//! ~6–10 seconds (`±10 ms`), and then detects the `800 ms` (WWV) or `500 ms` (CHU)
//! `:00` minute marker to lock exact 10 s / 60 s RAD950 slot boundaries.

/// Preset WWV / CHU frequencies (USB dial in 10 Hz units = carrier - 1,000 Hz, label).
pub const WWV_PRESETS: [(u32, &[u8]); 5] = [
    (999_900, b"WWV 10.000 MHz"),
    (499_900, b"WWV  5.000 MHz"),
    (1_499_900, b"WWV 15.000 MHz"),
    (784_900, b"CHU  7.850 MHz"),
    (332_900, b"CHU  3.330 MHz"),
];

/// Status snapshot emitted once per second (and immediately upon `:00` minute lock).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WwvStatus {
    /// Seconds elapsed since WWV listen started.
    pub secs: u16,
    /// True once the 1 Hz second-tick comb has a clear peak (`>= 4x` average).
    pub tick_locked: bool,
    /// Peak-to-average comb ratio in tenths (e.g. `65` = `6.5x`).
    pub ratio_x10: u16,
    /// True when the `:00` minute marker (`>= 250 ms` 1,000 Hz envelope tone) was just confirmed.
    pub minute_locked: bool,
    /// How many milliseconds ago the `:00` minute boundary occurred when `minute_locked` is true.
    pub minute_age_ms: u16,
}

/// 24-sample Q10 (`1024 = 1.0`) cos/sin table for exact `1,000 Hz` at `24,000 Hz`.
const COS_1000: [i16; 24] = [
    1024, 989, 887, 724, 512, 265, 0, -265, -512, -724, -887, -989,
    -1024, -989, -887, -724, -512, -265, 0, 265, 512, 724, 887, 989,
];
const SIN_1000: [i16; 24] = [
    0, 265, 512, 724, 887, 989, 1024, 989, 887, 724, 512, 265,
    0, -265, -512, -724, -887, -989, -1024, -989, -887, -724, -512, -265,
];

/// Reference adjacent noise frequency: `600 Hz` (`24,000 / 40` = exact 40 samples/cycle,
/// 6 exact cycles per 240-sample 10 ms bin, orthogonal to `1,000 Hz` and clear of `2 * f_c`).
const COS_600: [i16; 40] = [
    1024, 1011, 974, 912, 828, 724, 602, 465, 316, 160,
    0, -160, -316, -465, -602, -724, -828, -912, -974, -1011,
    -1024, -1011, -974, -912, -828, -724, -602, -465, -316, -160,
    0, 160, 316, 465, 602, 724, 828, 912, 974, 1011,
];
const SIN_600: [i16; 40] = [
    0, 160, 316, 465, 602, 724, 828, 912, 974, 1011,
    1024, 1011, 974, 912, 828, 724, 602, 465, 316, 160,
    0, -160, -316, -465, -602, -724, -828, -912, -974, -1011,
    -1024, -1011, -974, -912, -828, -724, -602, -465, -316, -160,
];

const SAMPLES_PER_BIN: u16 = 240; // 10 ms at 24,000 Hz
const BINS_PER_SEC: usize = 100; // 1.000 s

pub struct WwvReceiver {
    comb: [u32; BINS_PER_SEC],
    bin: u8,
    samp_in_bin: u16,
    acc_i_1000: i64,
    acc_q_1000: i64,
    acc_i_600: i64,
    acc_q_600: i64,
    secs: u16,
    tick_locked: bool,
    peak_bin: u8,
    /// Running count of high-1000 Hz bins in the current 500 ms window after `peak_bin`.
    min_window_pos: u8,
    min_high_bins: u8,
    in_min_window: bool,
    noise_ema: u32,
}

impl WwvReceiver {
    pub const fn new() -> Self {
        Self {
            comb: [0; BINS_PER_SEC],
            bin: 0,
            samp_in_bin: 0,
            acc_i_1000: 0,
            acc_q_1000: 0,
            acc_i_600: 0,
            acc_q_600: 0,
            secs: 0,
            tick_locked: false,
            peak_bin: 0,
            min_window_pos: 0,
            min_high_bins: 0,
            in_min_window: false,
            noise_ema: 0,
        }
    }

    /// Feed `24,000 Hz` signed 16-bit audio samples.
    /// Returns `Some(WwvStatus)` once per second, or immediately when the `:00` minute marker is confirmed.
    pub fn feed(&mut self, samples: &[i16]) -> Option<WwvStatus> {
        let mut out = None;
        for &s in samples {
            // Square-law AM envelope demodulator: carrier (f_c) × sideband (f_c + 1000 Hz)
            // beats to exact 1,000.000 Hz regardless of SI4732 crystal / BFO offset.
            let sq = ((s as i32 * s as i32) >> 14) as i64;
            // Triangle window across the 240-sample (10 ms) bin to suppress 2*f_c sidelobes.
            let k = self.samp_in_bin as usize;
            let w = if k < 120 { (k + 1) as i64 } else { (240 - k) as i64 };
            let y = sq * w;
            let idx10 = k % 24;
            let idx06 = k % 40;
            self.acc_i_1000 += y * COS_1000[idx10] as i64;
            self.acc_q_1000 += y * SIN_1000[idx10] as i64;
            self.acc_i_600 += y * COS_600[idx06] as i64;
            self.acc_q_600 += y * SIN_600[idx06] as i64;
            self.samp_in_bin += 1;

            if self.samp_in_bin >= SAMPLES_PER_BIN {
                if let Some(st) = self.finish_bin() {
                    // Prioritize minute_locked if both fire in the same block
                    if st.minute_locked || out.is_none() {
                        out = Some(st);
                    }
                }
            }
        }
        out
    }

    fn finish_bin(&mut self) -> Option<WwvStatus> {
        // Approximate L1/L2 magnitude: max(|I|,|Q|) + 0.375 * min(|I|,|Q|)
        // Scale down by 2^17 (Q10 trig * ~60 avg window * 240 samples -> ~0..50,000 range).
        let mag = |i: i64, q: i64| -> u32 {
            let ai = (i.unsigned_abs() >> 17) as u32;
            let aq = (q.unsigned_abs() >> 17) as u32;
            let (hi, lo) = if ai >= aq { (ai, aq) } else { (aq, ai) };
            hi.saturating_add((lo >> 2).saturating_add(lo >> 3))
        };

        let m10 = mag(self.acc_i_1000, self.acc_q_1000);
        let m06 = mag(self.acc_i_600, self.acc_q_600);
        self.acc_i_1000 = 0;
        self.acc_q_1000 = 0;
        self.acc_i_600 = 0;
        self.acc_q_600 = 0;
        self.samp_in_bin = 0;

        // Excess 1,000 Hz envelope modulation energy above 600 Hz reference band
        let excess = m10.saturating_sub(m06.saturating_add(m06 >> 1));
        self.noise_ema = if self.noise_ema == 0 {
            m06.max(1)
        } else {
            ((self.noise_ema as u64 * 31 + m06 as u64) >> 5).max(1) as u32
        };

        let b = self.bin as usize;
        // Exponentially leak comb after 20 s so it adapts if user retunes frequency
        if self.secs >= 20 {
            self.comb[b] = self.comb[b].saturating_sub(self.comb[b] >> 4);
        }
        // Cap single-bin impulse so a lightning crash doesn't dominate the comb
        let cap = self.noise_ema.saturating_mul(16).max(500);
        self.comb[b] = self.comb[b].saturating_add(excess.min(cap));

        // Minute-marker detector: once tick_locked, watch 50 bins (500 ms) starting at peak_bin.
        let mut minute_hit = false;
        if self.tick_locked {
            if self.bin == self.peak_bin {
                self.in_min_window = true;
                self.min_window_pos = 0;
                self.min_high_bins = 0;
            }
            if self.in_min_window {
                let tone_thresh = self.noise_ema.saturating_mul(2).max(20);
                if excess > tone_thresh {
                    self.min_high_bins = self.min_high_bins.saturating_add(1);
                }
                self.min_window_pos += 1;
                if self.min_window_pos >= 50 {
                    self.in_min_window = false;
                    // WWV (:00) = 800 ms (>=45 bins); CHU (:00) = 500 ms (>=40 bins); normal tick = 5 ms (1-2 bins).
                    if self.min_high_bins >= 25 {
                        minute_hit = true;
                    }
                }
            }
        }

        self.bin += 1;
        let sec_wrap = self.bin as usize >= BINS_PER_SEC;
        if sec_wrap {
            self.bin = 0;
            self.secs = self.secs.saturating_add(1);
        }

        if minute_hit {
            let ratio_x10 = self.evaluate_comb();
            return Some(WwvStatus {
                secs: self.secs,
                tick_locked: true,
                ratio_x10,
                minute_locked: true,
                // Window started at `peak_bin` (`:00.000`) and ended 50 bins (`500 ms`) later.
                minute_age_ms: 500,
            });
        }

        if sec_wrap {
            let ratio_x10 = self.evaluate_comb();
            return Some(WwvStatus {
                secs: self.secs,
                tick_locked: self.tick_locked,
                ratio_x10,
                minute_locked: false,
                minute_age_ms: 0,
            });
        }

        None
    }

    fn evaluate_comb(&mut self) -> u16 {
        let mut best_idx = 0usize;
        let mut best_val = 0u32;
        let mut sum = 0u64;
        for (i, &v) in self.comb.iter().enumerate() {
            sum += v as u64;
            if v > best_val {
                best_val = v;
                best_idx = i;
            }
        }
        // Exclude the 3 bins around peak (peak ± 1) from the noise floor average
        let prev = (best_idx + BINS_PER_SEC - 1) % BINS_PER_SEC;
        let next = (best_idx + 1) % BINS_PER_SEC;
        let bg_sum = sum
            .saturating_sub(self.comb[best_idx] as u64)
            .saturating_sub(self.comb[prev] as u64)
            .saturating_sub(self.comb[next] as u64);
        let bg_avg = (bg_sum / (BINS_PER_SEC as u64 - 3)).max(1);
        let ratio_x10 = ((best_val as u64 * 10) / bg_avg).min(u16::MAX as u64) as u16;
        if self.secs >= 6 && ratio_x10 >= 40 && best_val >= 150 {
            self.tick_locked = true;
            self.peak_bin = best_idx as u8;
        }
        ratio_x10
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_am_block(
        out: &mut [i16],
        start_samp: usize,
        carrier_hz: f32,
        mod_depth: f32,
        amp: f32,
        noise: f32,
        seed: &mut u32,
    ) {
        for (i, s) in out.iter_mut().enumerate() {
            *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let n = ((*seed >> 16) as i16 as f32) * (noise / 32768.0);
            let t = (start_samp + i) as f32 / 24_000.0;
            let car = (2.0 * core::f32::consts::PI * carrier_hz * t).sin();
            let sb = (2.0 * core::f32::consts::PI * (carrier_hz + 1_000.0) * t).sin();
            let v = amp * (car + mod_depth * sb) + n;
            *s = v.clamp(-32767.0, 32767.0) as i16;
        }
    }

    #[test]
    fn wwv_locks_ticks_and_minute_marker_across_carrier_offsets() {
        // Test nominal 1,000 Hz carrier AND +565 Hz / -300 Hz crystal error offsets
        for carrier_hz in [700.0f32, 1_000.0, 1_565.0] {
            let mut rx = WwvReceiver::new();
            let mut seed = 0x1234_5678u32;
            let mut locked_minute = false;
            let mut tick_locked = false;

            // Simulate 12 seconds of AM WWV: 5 ms 1,000 Hz ticks at bin 37 each second,
            // and an 800 ms minute marker at second 10 starting at bin 37.
            let mut buf = [0i16; 240];
            let mut samp = 0usize;
            for sec in 0..12 {
                for bin in 0..100 {
                    let is_marker = sec == 10 && bin >= 37;
                    let is_marker_tail = sec == 11 && bin < 17;
                    let is_tick = sec != 9 && bin == 37; // WWV omits sec 59 tick
                    let mod_depth = if is_marker || is_marker_tail || is_tick { 0.85 } else { 0.0 };
                    synth_am_block(&mut buf, samp, carrier_hz, mod_depth, 8_000.0, 1_500.0, &mut seed);
                    samp += 240;
                    if let Some(st) = rx.feed(&buf) {
                        if st.tick_locked {
                            tick_locked = true;
                        }
                        if st.minute_locked {
                            locked_minute = true;
                            assert_eq!(st.minute_age_ms, 500);
                        }
                    }
                }
            }
            assert!(tick_locked, "WWV comb should lock ticks at carrier={carrier_hz} Hz");
            assert!(locked_minute, "WWV should lock :00 minute marker at carrier={carrier_hz} Hz");
            assert!((rx.peak_bin as i32 - 37).abs() <= 1, "peak_bin={} at carrier={carrier_hz}", rx.peak_bin);
        }
    }

    #[test]
    fn wwv_rejects_pure_noise() {
        let mut rx = WwvReceiver::new();
        let mut seed = 0xdead_beefu32;
        let mut buf = [0i16; 240];
        let mut samp = 0usize;
        for _ in 0..2000 {
            synth_am_block(&mut buf, samp, 1_000.0, 0.0, 0.0, 3_000.0, &mut seed);
            samp += 240;
            if let Some(st) = rx.feed(&buf) {
                assert!(!st.minute_locked, "noise must not trigger minute lock");
            }
        }
        assert!(!rx.tick_locked, "pure noise must not lock tick comb");
    }
}
