//! Physically authentic SOL950 waterfall generator for the simulator (`simcore` and `shots_sol950`):
//! synthesizes real `usol950` 4-FSK bursts + calibrated 2,500 Hz Gaussian AWGN at 24 kHz and runs them
//! through the firmware's actual `usol950::Receiver::take_wf_col()`.

use ferrite_core::sol950::usol950::frame::encode_grid4;
use ferrite_core::sol950::usol950::phy::{DEFAULT_SPACING_HZ, N_SYMBOLS};
use ferrite_core::sol950::usol950::rx::INPUT_RATE_HZ;
use ferrite_core::sol950::usol950::{encode_tones, Callsign, Frame, Receiver};
use std::f64::consts::TAU;

struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0
    }
    fn u01(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / ((1u64 << 53) as f64)
    }
    fn gauss(&mut self, sigma: f64) -> f64 {
        let u1 = self.u01().max(1e-12);
        let u2 = self.u01();
        sigma * (-2.0 * u1.ln()).sqrt() * (TAU * u2).cos()
    }
}

/// Generate one 10 s SOL950 slot (`50` columns @ 200 ms) of authentic `usol950::Receiver` waterfall columns
/// containing real 4-FSK bursts on the 225 Hz grid in calibrated 2,500 Hz AWGN.
pub fn synth_slot_wf(seed: u64, signals: &[(&[u8], [u8; 4], f64, f64)]) -> Vec<([u8; 12], bool)> {
    const FS: usize = INPUT_RATE_HZ as usize; // 24,000 Hz
    const SLOT_N: usize = FS * 10;
    const SYM_N: usize = FS * 80 / 1000; // 1,920 samples (80 ms)
    const START_N: usize = FS / 2; // +500 ms nominal burst start
    let sigma = 1_200.0f64;
    let p_n_2500 = sigma * sigma * (2_500.0 / 12_000.0);

    let mut rng = Rng(seed);
    let mut buf = vec![0.0f64; SLOT_N];
    for s in &mut buf {
        *s = rng.gauss(sigma);
    }

    for &(call, grid, audio_hz, snr_db) in signals {
        let frame = Frame::Cq {
            sender: Callsign::encode(call).unwrap(),
            grid4: encode_grid4(&grid).unwrap(),
            tx_pwr: 10,
            reserved: 0,
        };
        let tones = encode_tones(frame);
        let amp = (2.0 * p_n_2500 * 10f64.powf(snr_db / 10.0)).sqrt();
        let mut phase = rng.u01() * TAU;
        for (k, &t) in tones.iter().enumerate().take(N_SYMBOLS) {
            let f = audio_hz + (t as f64) * (DEFAULT_SPACING_HZ as f64);
            let step = TAU * f / (FS as f64);
            let base = START_N + k * SYM_N;
            for n in 0..SYM_N {
                if base + n < SLOT_N {
                    buf[base + n] += amp * phase.sin();
                }
                phase += step;
                if phase >= TAU {
                    phase -= TAU;
                }
            }
        }
    }

    let pcm: Vec<i16> = buf.into_iter().map(|v| v.round().clamp(-32767.0, 32767.0) as i16).collect();
    let mut rx = Receiver::new();
    let mut cols = Vec::with_capacity(50);
    for (ci, chunk) in pcm.chunks(192).enumerate() {
        rx.push_samples(chunk);
        if let Some(col) = rx.take_wf_col() {
            cols.push((col, ci == 0 || cols.is_empty()));
        }
    }
    cols
}

/// Generate a realistic 2-slot (`~20 s`, `~100` columns) SOL950 panafall sequence with a mix of
/// strong (`+2 dB`), medium (`-8 dB`), weak (`-14 dB`), and threshold DX (`-17 dB`) 4-FSK signals.
pub fn sample_panafall() -> Vec<([u8; 12], bool)> {
    let mut out = synth_slot_wf(
        0x5019_5001,
        &[
            (b"K1ABC", *b"FN31", 850.0, 2.0),    // strong local/Es (+2 dB)
            (b"W6XYZ", *b"CM87", 1300.0, -8.0),  // moderate (-8 dB)
            (b"G4DEF", *b"IO91", 1750.0, -14.0), // weak DX (-14 dB)
        ],
    );
    let slot2 = synth_slot_wf(
        0x5019_5002,
        &[
            (b"W1AW", *b"FN31", 1075.0, -4.0),   // solid (-4 dB)
            (b"W6XYZ", *b"CM87", 1300.0, -10.0), // moderate (-10 dB)
            (b"JA1GHI", *b"PM95", 1975.0, -17.0),// threshold DX (-17 dB)
        ],
    );
    out.extend(slot2);
    out
}
