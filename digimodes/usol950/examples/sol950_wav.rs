//! Host CLI for SOL950 bench testing, carrier/BFO measurement, and asynchronous WAV decoding:
//!
//!   cargo run -p usol950 --example sol950_wav -- cq NQ6N FN31 1050 sol950-cq.wav
//!   cargo run -p usol950 --example sol950_wav -- rx K1ABC NQ6N -14 0 1275 sol950-rx-frame.wav
//!   cargo run -p usol950 --example sol950_wav -- decode sol950-rx.wav
//!   cargo run -p usol950 --example sol950_wav -- tone sol950-rx.wav

use std::env;
use std::fs;
use std::process::ExitCode;
use usol950::frame::{crc14_u64, decode_grid4, encode_grid4};
use usol950::{encode_tones, Callsign, Decoded, Frame, Modulator, Receiver, DEFAULT_SPACING_HZ, GRID_UNKNOWN, MAX_DECODED};

const RATE_24K: u32 = 24_000;

fn usage() {
    eprintln!("Usage:");
    eprintln!("  sol950_wav cq <CALL> <GRID4|NONE> <AUDIO_HZ> <OUT.wav>");
    eprintln!("  sol950_wav rx <SENDER> <RECIPIENT> <SNR_DB> <ACK:0|1> <AUDIO_HZ> <OUT.wav>");
    eprintln!("  sol950_wav decode <IN.wav>        (scans both slot-aligned and unaligned bench captures)");
    eprintln!("  sol950_wav tone <IN.wav>          (measures peak carrier Hz and SNR_2500 for BFO/path check)");
    eprintln!("  sol950_wav sweep <WEAK.wav|synth> <cq|rx> [ADJ.wav|synth:+225 ADJ_DB [TRIALS]] [--oracle]");
    eprintln!("  sol950_wav coop <WEAK.wav|synth> <cq|rx> [TRIALS] (bit-confidence & 2-radio soft-combining eval)");
    eprintln!("  sol950_wav budget <WEAK.wav|synth> <cq|rx> [TRIALS] (priority-ordered blind refinement work budget)");
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        usage();
        return ExitCode::from(2);
    }

    match args[1].as_str() {
        "cq" if args.len() == 6 => {
            let Some(call) = parse_call(&args[2]) else { return ExitCode::from(1) };
            let grid4 = if args[3].eq_ignore_ascii_case("NONE") {
                GRID_UNKNOWN
            } else if args[3].len() == 4 {
                let mut g = [0u8; 4];
                g.copy_from_slice(args[3].as_bytes());
                let Some(v) = encode_grid4(&g) else {
                    eprintln!("invalid 4-char Maidenhead grid: {}", args[3]);
                    return ExitCode::from(1);
                };
                v
            } else {
                eprintln!("grid must be 4 chars (e.g. FN31) or NONE");
                return ExitCode::from(1);
            };
            let audio_hz: u32 = args[4].parse().unwrap_or(1050);
            let frame = Frame::Cq { sender: call, grid4, tx_pwr: 10, reserved: 0 };
            emit_wav(frame, audio_hz, &args[5])
        }
        "rx" if args.len() == 8 => {
            let Some(sender) = parse_call(&args[2]) else { return ExitCode::from(1) };
            let Some(rcpt) = parse_call(&args[3]) else { return ExitCode::from(1) };
            let snr_db: i8 = args[4].parse().unwrap_or(-15);
            let ack = args[5] == "1" || args[5].eq_ignore_ascii_case("true");
            let audio_hz: u32 = args[6].parse().unwrap_or(1050);
            let frame = Frame::Rx { sender, recipient: rcpt, snr_db, ack, reserved: false };
            emit_wav(frame, audio_hz, &args[7])
        }
        "decode" if args.len() == 3 => decode_wav(&args[2]),
        "tone" if args.len() == 3 => measure_tone(&args[2]),
        "sweep" if matches!(args.len(), 4..=8) => run_sweep(&args[2..]),
        "coop" if matches!(args.len(), 4 | 5) => run_coop(&args[2..]),
        "budget" if matches!(args.len(), 4 | 5) => run_budget(&args[2..]),
        _ => {
            usage();
            ExitCode::from(2)
        }
    }
}

fn parse_call(s: &str) -> Option<Callsign> {
    match Callsign::encode(s.as_bytes()) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("unsupported callsign '{s}': {e:?}");
            None
        }
    }
}

fn emit_wav(frame: Frame, audio_hz: u32, path: &str) -> ExitCode {
    let w64 = frame.pack_u64();
    let crc = crc14_u64(w64);
    let tones = encode_tones(frame);
    println!("payload_u64 = 0x{w64:016X}");
    println!("crc14       = 0x{crc:04X}");
    print!("tones[110]  = ");
    for &t in &tones {
        print!("{t}");
    }
    println!();

    let mut pcm = vec![0i16; (RATE_24K * 10) as usize];
    let start = ((400 * RATE_24K) / 1000) as usize;
    let mut m = Modulator::new(tones, RATE_24K, audio_hz, DEFAULT_SPACING_HZ, 12_000);
    let mut i = start;
    while let Some(s) = m.next_sample() {
        if i < pcm.len() {
            pcm[i] = s;
        }
        i += 1;
    }

    if let Err(e) = write_wav_mono16(path, RATE_24K, &pcm) {
        eprintln!("failed to write {path}: {e}");
        return ExitCode::from(1);
    }
    println!("wrote 10.0 s @ 24 kHz WAV to {path} (audio_hz={audio_hz} Hz)");
    ExitCode::SUCCESS
}

/// Decode a 24 kHz PCM stream without assuming the recording began on a 10-second UTC slot boundary:
/// steps a 10.0 s window in `560 ms` (`7`-symbol) strides while at least `8.88 s` (`80 ms` earliest sync offset +
/// `8.80 s` 110-symbol burst) remains in the stream—continuing even after `win_start + 10.0 s` reaches EOF so a
/// complete burst near the end of a capture (e.g. `6.10 s .. 14.90 s` in a `15.00 s` WAV) is never missed.
fn decode_pcm_24k(pcm_24k: &[i16]) -> Vec<(i64, Decoded)> {
    let mut rx = Receiver::new();
    let dummy = Frame::Cq {
        sender: Callsign::from_raw(0).unwrap(),
        grid4: GRID_UNKNOWN,
        tx_pwr: 0,
        reserved: 0,
    };
    let slot_samples = (RATE_24K * 10) as usize;
    let min_burst_samples = (RATE_24K as usize * 888) / 100; // 8.88 s = 0.08 s min start + 8.80 s burst
    let stride_samples = (RATE_24K as usize * 560) / 1000; // 560 ms stride (< 680 ms receiver window)

    let mut found: Vec<(i64, Decoded)> = Vec::new();
    let mut win_start = 0usize;
    while win_start < pcm_24k.len() {
        let rem_total = pcm_24k.len() - win_start;
        if win_start > 0 && rem_total < min_burst_samples {
            break;
        }
        let end = (win_start + slot_samples).min(pcm_24k.len());
        let slice = &pcm_24k[win_start..end];
        rx.reset();
        for blk in slice.chunks(192) {
            rx.push_samples(blk);
        }
        if slice.len() < slot_samples {
            let zeros = [0i16; 192];
            let mut rem = slot_samples - slice.len();
            while rem > 0 {
                let n = rem.min(zeros.len());
                rx.push_samples(&zeros[..n]);
                rem -= n;
            }
        }
        let mut out = [Decoded { frame: dummy, audio_hz: 0, time_offset_ms: 0, snr_db: 0 }; MAX_DECODED];
        let n = rx.finish_slot(&mut out);
        let win_ms = (win_start as i64 * 1000) / RATE_24K as i64;
        for d in &out[..n] {
            let burst_start_ms = win_ms + 400 + d.time_offset_ms as i64;
            if !found.iter().any(|(t0, prev)| prev.frame == d.frame && (*t0 - burst_start_ms).abs() <= 1_500) {
                found.push((burst_start_ms, *d));
            }
        }
        if rem_total < min_burst_samples {
            break;
        }
        win_start += stride_samples;
    }
    found
}

fn decode_wav(path: &str) -> ExitCode {
    let data = match fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("failed to read {path}: {e}");
            return ExitCode::from(1);
        }
    };
    let Some((rate, samples)) = read_wav_mono16(&data) else {
        eprintln!("{path}: expected 16-bit PCM mono WAV");
        return ExitCode::from(1);
    };
    let pcm_24k = resample_to_24k(rate, &samples);
    let found = decode_pcm_24k(&pcm_24k);

    for (t_ms, d) in &found {
        print_decoded(*t_ms, d);
    }
    println!("decoded {} frame(s) from {path} (input {rate} Hz, {} samples)", found.len(), samples.len());
    ExitCode::SUCCESS
}

/// Returns `(peak_hz, snr_2500_db, blocks, tone_present)`.
fn analyze_tone_samples(rate: u32, samples: &[i16]) -> Option<(f64, f64, usize, bool)> {
    let n = rate as usize;
    if samples.len() < n || rate < 6_000 {
        return None;
    }
    let lo_bin = 200usize;
    let hi_bin = 2800usize.min(n / 2 - 2);
    let mut pwr = vec![0.0f64; hi_bin + 2];
    let mut blocks = 0usize;
    for chunk in samples.chunks_exact(n) {
        blocks += 1;
        let mut w_chunk = vec![0.0f64; n];
        for (i, &s) in chunk.iter().enumerate() {
            let w = 0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos());
            w_chunk[i] = s as f64 * w;
        }
        for (k, slot) in pwr.iter_mut().enumerate().take(hi_bin + 2).skip(lo_bin - 1) {
            let omega = 2.0 * std::f64::consts::PI * k as f64 / n as f64;
            let cos_w = omega.cos();
            let coeff = 2.0 * cos_w;
            let (mut s1, mut s2) = (0.0f64, 0.0f64);
            for &x in &w_chunk {
                let s0 = x + coeff * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            let re = s1 - s2 * cos_w;
            let im = s2 * omega.sin();
            *slot += re * re + im * im;
        }
    }
    for v in &mut pwr {
        *v /= blocks.max(1) as f64;
    }

    let mut peak_k = lo_bin;
    for k in lo_bin..=hi_bin {
        if pwr[k] > pwr[peak_k] {
            peak_k = k;
        }
    }
    let a = pwr[peak_k - 1].max(1.0).ln();
    let b = pwr[peak_k].max(1.0).ln();
    let c = pwr[peak_k + 1].max(1.0).ln();
    let denom = a - 2.0 * b + c;
    let delta = if denom.abs() > 1e-12 { 0.5 * (a - c) / denom } else { 0.0 };
    let peak_hz = peak_k as f64 + delta.clamp(-0.5, 0.5);

    let mut sig_plus_n = 0.0f64;
    let mut noise_sum = 0.0f64;
    let mut noise_bins = 0usize;
    for (k, &pk) in pwr.iter().enumerate().take(hi_bin + 1).skip(lo_bin) {
        if k.abs_diff(peak_k) <= 3 {
            if k.abs_diff(peak_k) <= 2 {
                sig_plus_n += pk;
            }
        } else {
            noise_sum += pk;
            noise_bins += 1;
        }
    }
    let noise_per_bin = noise_sum / noise_bins.max(1) as f64;
    let peak_to_noise = if noise_per_bin > 0.0 { pwr[peak_k] / noise_per_bin } else { 0.0 };
    // Over 2,500 bins of pure white noise, 99.99% extreme-value threshold is ~17x for B=1 s and < 6x for B >= 3 s.
    let min_ratio = if blocks == 1 { 20.0 } else { 10.0 };
    let tone_present = peak_to_noise >= min_ratio;

    let sig_pwr = (sig_plus_n - 5.0 * noise_per_bin).max(1e-9);
    let noise_2500 = noise_per_bin * 2500.0;
    let snr_2500_db = 10.0 * (sig_pwr / noise_2500.max(1e-9)).log10();
    Some((peak_hz, snr_2500_db, blocks, tone_present))
}

/// Measure peak continuous audio tone in `200..2,800 Hz` and `SNR_2500` for Gatekeeper Experiment 1 (carrier + BFO).
fn measure_tone(path: &str) -> ExitCode {
    let data = match fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("failed to read {path}: {e}");
            return ExitCode::from(1);
        }
    };
    let Some((rate, samples)) = read_wav_mono16(&data) else {
        eprintln!("{path}: expected 16-bit PCM mono WAV");
        return ExitCode::from(1);
    };
    let Some((peak_hz, snr_2500_db, blocks, tone_present)) = analyze_tone_samples(rate, &samples) else {
        eprintln!("{path}: need at least 1.0 second of audio at >= 6 kHz");
        return ExitCode::from(1);
    };

    println!("tone_found  = {}", if tone_present { "YES" } else { "NO (noise floor only)" });
    println!("peak_hz     = {peak_hz:.2} Hz");
    println!("snr_2500_db = {snr_2500_db:+.1} dB  (averaged over {blocks} s, input {rate} Hz)");
    ExitCode::SUCCESS
}

struct SweepRng(u64);
impl SweepRng {
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

fn load_norm_burst(spec: &str) -> Option<Vec<i16>> {
    if spec == "synth" {
        return None;
    }
    let data = fs::read(spec).unwrap_or_else(|e| panic!("read {spec}: {e}"));
    let (rate, samples) = read_wav_mono16(&data).unwrap_or_else(|| panic!("{spec}: expected 24 kHz mono PCM"));
    assert_eq!(rate, RATE_24K, "{spec}: expected 24000 Hz, got {rate}");
    Some(samples)
}

/// Reproducible host AWGN & adjacent-caller replay sweep (`P_n(2500) = sigma^2 * 2500/12000`, `sigma = 1200`,
/// WAV burst inputs normalized to active signal RMS = 10,000, deterministic seed `0x2000_0000_0000_0001 + i*997`).
/// Weak and adjacent callers always use distinct payloads (`CQ NQ6N FN31` vs `RX K1ABC -> NQ6N`) and only
/// recovery of the weak frame is scored.
fn run_sweep(args: &[String]) -> ExitCode {
    let oracle = args.iter().any(|a| a == "--oracle");
    let args: Vec<String> = args.iter().filter(|a| a.as_str() != "--oracle").cloned().collect();
    if !matches!(args.len(), 2 | 4 | 5) {
        usage();
        return ExitCode::from(2);
    }
    let weak_wav = load_norm_burst(&args[0]);
    let nq6n = Callsign::encode(b"NQ6N").unwrap();
    let k1abc = Callsign::encode(b"K1ABC").unwrap();
    let fn31 = encode_grid4(b"FN31").unwrap();
    let cq_frame = Frame::Cq { sender: nq6n, grid4: fn31, tx_pwr: 10, reserved: 0 };
    let rx_frame = Frame::Rx { sender: k1abc, recipient: nq6n, snr_db: -15, ack: false, reserved: false };
    let (weak_frame, adj_frame) = if args[1].eq_ignore_ascii_case("cq") {
        (cq_frame, rx_frame)
    } else if args[1].eq_ignore_ascii_case("rx") {
        (rx_frame, cq_frame)
    } else {
        usage();
        return ExitCode::from(2);
    };

    let (adj_wav, adj_synth_hz, adj_db, trials) = if args.len() >= 4 {
        let (aw, shz) = if let Some(rest) = args[2].strip_prefix("synth:") {
            let off: i32 = rest.parse().unwrap_or(225);
            (None, Some((1037i32 + off) as u32))
        } else {
            (load_norm_burst(&args[2]), None)
        };
        let adb: f64 = args[3].parse().unwrap_or(15.0);
        let tr: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(100);
        (aw, shz, adb, tr)
    } else {
        (None, None, 0.0, 100)
    };

    let snrs = [-12.0f64, -13.0, -14.0, -15.0, -16.0, -17.0, -18.0];
    let dummy = cq_frame;
    let weak_tones = encode_tones(weak_frame);
    let adj_tones = encode_tones(adj_frame);
    const FS: usize = RATE_24K as usize;
    let sigma = 1_200.0f64;
    let p_n_2500 = sigma * sigma * (2500.0 / 12000.0);
    let seed_base = 0x2000_0000_0000_0001u64;

    println!("sol950_wav sweep: trials={trials}, norm_rms=10000, sigma_24k={sigma:.0}, seed_base=0x{seed_base:016x}, oracle={oracle}, weak={:?}", weak_frame);
    let mut rates = Vec::with_capacity(snrs.len());
    let mut orates = Vec::with_capacity(snrs.len());
    for (si, &snr_db) in snrs.iter().enumerate() {
        let p_weak = p_n_2500 * 10f64.powf(snr_db / 10.0);
        let weak_rms = p_weak.sqrt();
        let weak_amp = (2.0 * p_weak).sqrt() as i16;
        let adj_rms = weak_rms * 10f64.powf(adj_db / 20.0);
        let adj_amp = (2.0f64.sqrt() * adj_rms) as i16;
        let mut rng = SweepRng(seed_base + (si as u64) * 997);
        let mut rx = Receiver::new();
        let mut ok = 0usize;
        let mut ok_oracle = 0usize;
        let mut rec_of_missed = 0usize;

        for t in 0..trials {
            let mut slot = vec![0.0f64; FS * 10];
            for s in &mut slot {
                *s = rng.gauss(sigma);
            }
            let dt_w_ms = -160i32 + ((t as i32 * 53) % 401);
            let true_start_ms = (400 + dt_w_ms) as u32;
            if let Some(wb) = weak_wav.as_deref() {
                let start = ((200 + dt_w_ms) as usize * FS) / 1000;
                let sc = weak_rms / 10_000.0;
                for (k, &v) in wb.iter().enumerate() {
                    if start + k < slot.len() {
                        slot[start + k] += (v as f64) * sc;
                    }
                }
            } else {
                let start = (true_start_ms as usize * FS) / 1000;
                let mut m = Modulator::new(weak_tones, RATE_24K, 1037, DEFAULT_SPACING_HZ, weak_amp);
                let mut i = start;
                while let Some(v) = m.next_sample() {
                    if i < slot.len() {
                        slot[i] += v as f64;
                    }
                    i += 1;
                }
            }

            let dt_a_ms = -160i32 + (((t as i32 * 97) + 211) % 401);
            if let Some(ab) = adj_wav.as_deref() {
                let start = ((200 + dt_a_ms) as usize * FS) / 1000;
                let sc = adj_rms / 10_000.0;
                for (k, &v) in ab.iter().enumerate() {
                    if start + k < slot.len() {
                        slot[start + k] += (v as f64) * sc;
                    }
                }
            } else if let Some(ahz) = adj_synth_hz {
                let start = ((400 + dt_a_ms) as usize * FS) / 1000;
                let mut m = Modulator::new(adj_tones, RATE_24K, ahz, DEFAULT_SPACING_HZ, adj_amp);
                let mut i = start;
                while let Some(v) = m.next_sample() {
                    if i < slot.len() {
                        slot[i] += v as f64;
                    }
                    i += 1;
                }
            }

            let pcm: Vec<i16> = slot.into_iter().map(|v| v.round().clamp(-32767.0, 32767.0) as i16).collect();
            rx.reset();
            for blk in pcm.chunks(192) {
                rx.push_samples(blk);
            }
            let mut out = [Decoded { frame: dummy, audio_hz: 0, time_offset_ms: 0, snr_db: 0 }; MAX_DECODED];
            let n = rx.finish_slot(&mut out);
            let blind_hit = out[..n].iter().any(|d| d.frame == weak_frame);
            if blind_hit {
                ok += 1;
            }
            if oracle {
                rx.reset();
                rx.set_oracle_track(true_start_ms, 1037);
                for blk in pcm.chunks(192) {
                    rx.push_samples(blk);
                }
                let n2 = rx.finish_slot(&mut out);
                let oracle_hit = out[..n2].iter().any(|d| d.frame == weak_frame);
                if oracle_hit {
                    ok_oracle += 1;
                    if !blind_hit {
                        rec_of_missed += 1;
                    }
                }
            }
        }
        let frac = ok as f64 / trials as f64;
        rates.push((snr_db, frac));
        if oracle {
            let ofrac = ok_oracle as f64 / trials as f64;
            orates.push((snr_db, ofrac));
            let missed = trials - ok;
            println!("  SNR={snr_db:+5.1} dB : blind={ok:3}/{trials} ({:5.1}%) | oracle={ok_oracle:3}/{trials} ({:5.1}%) | recovered_of_missed={rec_of_missed:2}/{missed:2}", 100.0 * frac, 100.0 * ofrac);
        } else {
            println!("  SNR={snr_db:+5.1} dB : {ok:3}/{trials} ({:5.1}%)", 100.0 * frac);
        }
    }
    if let Some(w) = rates.windows(2).find(|w| w[0].1 >= 0.5 && w[1].1 <= 0.5 && (w[0].1 - w[1].1).abs() > 1e-9) {
        let snr50 = w[0].0 + (0.5 - w[0].1) / (w[1].1 - w[0].1) * (w[1].0 - w[0].0);
        println!("  interp_50pct_blind  = {snr50:+.2} dB");
    }
    if oracle {
        if let Some(w) = orates.windows(2).find(|w| w[0].1 >= 0.5 && w[1].1 <= 0.5 && (w[0].1 - w[1].1).abs() > 1e-9) {
            let snr50 = w[0].0 + (0.5 - w[0].1) / (w[1].1 - w[0].1) * (w[1].0 - w[0].0);
            println!("  interp_50pct_oracle = {snr50:+.2} dB");
        }
    }
    ExitCode::SUCCESS
}

/// Approximation of `ln(I_0(x))` for `x >= 0` (exact to < 0.01 across all `x`).
fn ln_bessel_i0(x: f64) -> f64 {
    if x < 3.75 {
        let t = (x / 3.75).powi(2);
        let i0 = 1.0 + t * (3.5156229 + t * (3.0899424 + t * (1.2067492 + t * (0.2659732 + t * (0.0360768 + t * 0.0045813)))));
        i0.ln()
    } else {
        let t = 3.75 / x;
        let poly = 0.39894228 + t * (0.01328592 + t * (0.00225319 + t * (-0.00157565 + t * (0.00916281 + t * (-0.02057706 + t * (0.02635537 + t * (-0.01647633 + t * 0.00392377)))))));
        x + (poly / x.sqrt()).ln()
    }
}

/// Exact-boundary 4-tone matched-filter reference demodulator using local sync-estimated noise `N0` and `Es/N0`.
/// Outputs `184` canonical LDPC codeword LLRs (`i8`, positive = bit 0).
fn ref_demod_llrs(pcm: &[i16], start_samp: usize, f0_hz: f64) -> [i8; usol950::ldpc::N_BITS] {
    use std::f64::consts::TAU;
    use usol950::phy::{data_sym_pos, interleave_idx, N_DATA_SYMS, S0, S1, S2};

    const SYM_N: usize = 1920; // 80 ms @ 24 kHz
    let fs = RATE_24K as f64;
    let tone_energy = |sym: usize, tone: usize| -> f64 {
        let a = start_samp + sym * SYM_N;
        let f = f0_hz + (tone as f64) * (DEFAULT_SPACING_HZ as f64);
        let w = TAU * f / fs;
        let mut re = 0.0f64;
        let mut im = 0.0f64;
        for n in 0..SYM_N {
            let x = pcm.get(a + n).copied().unwrap_or(0) as f64;
            let ph = w * (n as f64);
            re += x * ph.cos();
            im += x * ph.sin();
        }
        re * re + im * im
    };

    // Estimate local noise energy N0 and signal-to-noise ratio gamma = Es/N0 from the 18 sync symbols.
    let mut on_sum = 0.0f64;
    let mut off_sum = 0.0f64;
    let mut off_cnt = 0usize;
    let mut sync_n = 0usize;
    for &(base, pat) in &[(0usize, &S0), (18, &S1), (104, &S2)] {
        for (k, &w) in pat.iter().enumerate() {
            sync_n += 1;
            for t in 0..4 {
                let e = tone_energy(base + k, t);
                if t == w as usize {
                    on_sum += e;
                } else {
                    off_sum += e;
                    off_cnt += 1;
                }
            }
        }
    }
    let n0 = (off_sum / off_cnt.max(1) as f64).max(1.0);
    let e_on = on_sum / sync_n.max(1) as f64;
    let gamma = ((e_on / n0) - 1.0).clamp(0.3, 20.0);

    let log_sum_exp = |a: f64, b: f64| -> f64 {
        let m = a.max(b);
        m + ((a - m).exp() + (b - m).exp()).ln()
    };

    let mut raw = [0i8; usol950::ldpc::N_BITS];
    for d in 0..N_DATA_SYMS {
        let sym = data_sym_pos(d);
        let mut lp = [0.0f64; 4];
        for (t, slot) in lp.iter_mut().enumerate() {
            let e = tone_energy(sym, t);
            *slot = ln_bessel_i0(2.0 * (gamma * (e / n0)).sqrt());
        }
        let llr0 = log_sum_exp(lp[0], lp[1]) - log_sum_exp(lp[2], lp[3]);
        let llr1 = log_sum_exp(lp[0], lp[3]) - log_sum_exp(lp[1], lp[2]);
        raw[2 * d] = (llr0 * 12.0).round().clamp(-120.0, 120.0) as i8;
        raw[2 * d + 1] = (llr1 * 12.0).round().clamp(-120.0, 120.0) as i8;
    }

    let mut deinter = [0i8; usol950::ldpc::N_BITS];
    for ci in 0..usol950::ldpc::N_BITS {
        deinter[ci] = raw[interleave_idx(ci)];
    }
    deinter
}

fn decode_llrs(llrs: &[i8; usol950::ldpc::N_BITS]) -> Option<Frame> {
    let mut scratch = usol950::ldpc::BpScratch::new();
    let bits78 = usol950::ldpc::decode_bp(llrs, &mut scratch, usol950::rx::LDPC_MAX_ITER)?;
    Frame::from_bits78(&bits78)
}

/// Signed 4-bit quantizer (`[-7..=+7]`, `0` = no evidence) per `docs/sol950-cooperative-reception.md`.
fn quantize_4bit(llrs: &[i8; usol950::ldpc::N_BITS], max_abs: f64) -> [i8; usol950::ldpc::N_BITS] {
    let mut out = [0i8; usol950::ldpc::N_BITS];
    for (o, &v) in out.iter_mut().zip(llrs.iter()) {
        *o = ((v as f64 / max_abs) * 7.0).round().clamp(-7.0, 7.0) as i8;
    }
    out
}

fn combine_llrs(a: &[i8; usol950::ldpc::N_BITS], b: &[i8; usol950::ldpc::N_BITS], scale: i16) -> [i8; usol950::ldpc::N_BITS] {
    let mut out = [0i8; usol950::ldpc::N_BITS];
    for i in 0..usol950::ldpc::N_BITS {
        let sum = (a[i] as i16 + b[i] as i16) * scale;
        out[i] = sum.clamp(-120, 120) as i8;
    }
    out
}

/// Single-radio bit-confidence comparison (current oracle LLRs vs. exact-boundary `ln I_0` reference demodulator)
/// and two-radio cooperative soft-combining experiment (`full i8` and `4-bit` quantized) at `-15, -16, -17, -18 dB`.
fn run_coop(args: &[String]) -> ExitCode {
    let weak_wav = load_norm_burst(&args[0]);
    let nq6n = Callsign::encode(b"NQ6N").unwrap();
    let k1abc = Callsign::encode(b"K1ABC").unwrap();
    let fn31 = encode_grid4(b"FN31").unwrap();
    let weak_frame = if args[1].eq_ignore_ascii_case("cq") {
        Frame::Cq { sender: nq6n, grid4: fn31, tx_pwr: 10, reserved: 0 }
    } else if args[1].eq_ignore_ascii_case("rx") {
        Frame::Rx { sender: k1abc, recipient: nq6n, snr_db: -15, ack: false, reserved: false }
    } else {
        usage();
        return ExitCode::from(2);
    };
    let trials: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100);
    let weak_tones = encode_tones(weak_frame);
    const FS: usize = RATE_24K as usize;
    let sigma = 1_200.0f64;
    let p_n_2500 = sigma * sigma * (2500.0 / 12000.0);
    let seed_base = 0x3000_0000_0000_0001u64;

    let make_slot = |rng: &mut SweepRng, snr_db: f64, dt_ms: i32| -> (Vec<i16>, usize) {
        let p_weak = p_n_2500 * 10f64.powf(snr_db / 10.0);
        let weak_rms = p_weak.sqrt();
        let weak_amp = (2.0 * p_weak).sqrt() as i16;
        let mut slot = vec![0.0f64; FS * 10];
        for s in &mut slot {
            *s = rng.gauss(sigma);
        }
        let s0_samp = ((400 + dt_ms) as usize * FS) / 1000;
        if let Some(wb) = weak_wav.as_deref() {
            let start = ((200 + dt_ms) as usize * FS) / 1000;
            let sc = weak_rms / 10_000.0;
            for (k, &v) in wb.iter().enumerate() {
                if start + k < slot.len() {
                    slot[start + k] += (v as f64) * sc;
                }
            }
        } else {
            let mut m = Modulator::new(weak_tones, RATE_24K, 1037, DEFAULT_SPACING_HZ, weak_amp);
            let mut i = s0_samp;
            while let Some(v) = m.next_sample() {
                if i < slot.len() {
                    slot[i] += v as f64;
                }
                i += 1;
            }
        }
        (slot.into_iter().map(|v| v.round().clamp(-32767.0, 32767.0) as i16).collect(), s0_samp)
    };

    let get_cur_llrs = |rx: &mut Receiver, pcm: &[i16], dt_ms: i32| -> [i8; usol950::ldpc::N_BITS] {
        rx.reset();
        rx.set_oracle_track((400 + dt_ms) as u32, 1037);
        for blk in pcm.chunks(192) {
            rx.push_samples(blk);
        }
        rx.track_codeword_llrs(0).unwrap()
    };

    println!("=== Part A: Single-radio known-track LLR comparison (trials={trials}, seed_base=0x{seed_base:016x}) ===");
    println!("SNR   | cur_oracle | ref_demod | rescued(ref) | lost(ref)");
    let mut rx_a = Receiver::new();
    let mut rx_b = Receiver::new();
    let mut ref_rates = Vec::new();
    for (si, &snr_db) in [-15.0f64, -16.0, -17.0, -18.0, -19.0, -20.0].iter().enumerate() {
        let mut rng = SweepRng(seed_base + (si as u64) * 997);
        let (mut cur_ok, mut ref_ok, mut rescued, mut lost) = (0, 0, 0, 0);
        for t in 0..trials {
            let dt_ms = -160i32 + ((t as i32 * 53) % 401);
            let (pcm, s0_samp) = make_slot(&mut rng, snr_db, dt_ms);
            let c_llrs = get_cur_llrs(&mut rx_a, &pcm, dt_ms);
            let r_llrs = ref_demod_llrs(&pcm, s0_samp, 1037.0);
            let c_hit = decode_llrs(&c_llrs) == Some(weak_frame);
            let r_hit = decode_llrs(&r_llrs) == Some(weak_frame);
            if c_hit { cur_ok += 1; }
            if r_hit { ref_ok += 1; }
            if r_hit && !c_hit { rescued += 1; }
            if c_hit && !r_hit { lost += 1; }
        }
        ref_rates.push((snr_db, ref_ok as f64 / trials as f64));
        println!("{snr_db:+5.1} | {:3}/{trials}    | {:3}/{trials}   | {:3}          | {:3}", cur_ok, ref_ok, rescued, lost);
    }
    if let Some(w) = ref_rates.windows(2).find(|w| w[0].1 >= 0.5 && w[1].1 <= 0.5 && (w[0].1 - w[1].1).abs() > 1e-9) {
        let snr50 = w[0].0 + (0.5 - w[0].1) / (w[1].1 - w[0].1) * (w[1].0 - w[0].0);
        println!("  interp_50pct_ref_demod = {snr50:+.2} dB");
    }

    println!("\n=== Part C: Blind acquisition -> Reference demodulation decomposition (trials={trials}) ===");
    println!("SNR   | blind_cur | in_cands | ref@blind | ref@refined | ref@oracle");
    for (si, &snr_db) in [-15.0f64, -16.0, -17.0, -18.0, -19.0].iter().enumerate() {
        let mut rng = SweepRng(seed_base + (si as u64) * 997);
        let (mut b_cur, mut in_cands, mut r_blind, mut r_ref, mut r_ora) = (0, 0, 0, 0, 0);
        let dummy = weak_frame;
        for t in 0..trials {
            let dt_ms = -160i32 + ((t as i32 * 53) % 401);
            let true_ms = 400 + dt_ms;
            let (pcm, s0_samp) = make_slot(&mut rng, snr_db, dt_ms);

            // 1. Blind acquisition via current Receiver
            rx_a.reset();
            for blk in pcm.chunks(192) {
                rx_a.push_samples(blk);
            }
            let mut matched: Option<(u32, u32)> = None;
            for ti in 0..usol950::rx::MAX_TRACKS {
                if let Some((est_ms, est_hz)) = rx_a.track_coords(ti) {
                    if (est_ms as i32 - true_ms).abs() <= 60 && (est_hz as i32 - 1037).abs() <= 12 {
                        matched = Some((est_ms, est_hz));
                        break;
                    }
                }
            }
            let mut out = [Decoded { frame: dummy, audio_hz: 0, time_offset_ms: 0, snr_db: 0 }; MAX_DECODED];
            let n = rx_a.finish_slot(&mut out);
            if out[..n].iter().any(|d| d.frame == weak_frame) {
                b_cur += 1;
            }

            if let Some((est_ms, est_hz)) = matched {
                in_cands += 1;
                let est_samp = (est_ms as usize * FS) / 1000;
                if decode_llrs(&ref_demod_llrs(&pcm, est_samp, est_hz as f64)) == Some(weak_frame) {
                    r_blind += 1;
                }
                // Local S0+S1 refinement around blind candidate (+/-20 ms in 5 ms steps, +/-4 Hz in 1 Hz steps)
                let (ref_samp, ref_hz) = refine_candidate(&pcm, est_samp, est_hz as f64);
                if decode_llrs(&ref_demod_llrs(&pcm, ref_samp, ref_hz)) == Some(weak_frame) {
                    r_ref += 1;
                }
            }

            if decode_llrs(&ref_demod_llrs(&pcm, s0_samp, 1037.0)) == Some(weak_frame) {
                r_ora += 1;
            }
        }
        println!("{snr_db:+5.1} | {:3}/{trials}   | {:3}/{trials}  | {:3}/{trials}   | {:3}/{trials}     | {:3}/{trials}", b_cur, in_cands, r_blind, r_ref, r_ora);
    }

    println!("\n=== Part B: Two-radio cooperative soft combining (independent AWGN, trials={trials}) ===");
    println!("Mode | SNR   | leader | either | joint_miss | rescued_full | rescued_4bit | total_4bit");
    for (mode_name, use_ref, qmax, qscale) in [("cur", false, 24.0, 3i16), ("ref", true, 120.0, 8i16)] {
        for (si, &snr_db) in [-15.0f64, -16.0, -17.0, -18.0, -19.0].iter().enumerate() {
            let mut rng_a = SweepRng(seed_base + 100_000 + (si as u64) * 997);
            let mut rng_b = SweepRng(seed_base + 200_000 + (si as u64) * 997);
            let (mut lead_ok, mut either_ok, mut jm, mut res_full, mut res_4b) = (0, 0, 0, 0, 0);
            for t in 0..trials {
                let dt_a = -160i32 + ((t as i32 * 53) % 401);
                let dt_b = -160i32 + (((t as i32 * 97) + 173) % 401);
                let (pcm_a, s0_a) = make_slot(&mut rng_a, snr_db, dt_a);
                let (pcm_b, s0_b) = make_slot(&mut rng_b, snr_db, dt_b);
                let llr_a = if use_ref { ref_demod_llrs(&pcm_a, s0_a, 1037.0) } else { get_cur_llrs(&mut rx_a, &pcm_a, dt_a) };
                let llr_b = if use_ref { ref_demod_llrs(&pcm_b, s0_b, 1037.0) } else { get_cur_llrs(&mut rx_b, &pcm_b, dt_b) };
                let ha = decode_llrs(&llr_a) == Some(weak_frame);
                let hb = decode_llrs(&llr_b) == Some(weak_frame);
                if ha { lead_ok += 1; }
                if ha || hb {
                    either_ok += 1;
                } else {
                    jm += 1;
                    let comb_full = combine_llrs(&llr_a, &llr_b, 1);
                    if decode_llrs(&comb_full) == Some(weak_frame) {
                        res_full += 1;
                    }
                    let qa = quantize_4bit(&llr_a, qmax);
                    let qb = quantize_4bit(&llr_b, qmax);
                    let comb_4b = combine_llrs(&qa, &qb, qscale);
                    if decode_llrs(&comb_4b) == Some(weak_frame) {
                        res_4b += 1;
                    }
                }
            }
            println!(
                "{mode_name:4} | {snr_db:+5.1} | {:3}/{trials} | {:3}/{trials} | {:3}        | {:3}/{:<3}      | {:3}/{:<3}      | {:3}/{trials}",
                lead_ok, either_ok, jm, res_full, jm, res_4b, jm, either_ok + res_4b
            );
        }
    }

    // False-accept controls: (1) pure noise full & 4-bit (cur & ref), (2) wrong-caller pairings (CQ + RX)
    let mut rng_n1 = SweepRng(seed_base + 999_001);
    let mut rng_n2 = SweepRng(seed_base + 999_002);
    let (mut fa_cur_full, mut fa_cur_4b, mut fa_ref_full, mut fa_ref_4b, mut fa_third_4b, mut fa_misattrib_4b) = (0, 0, 0, 0, 0, 0);
    let other_frame = if matches!(weak_frame, Frame::Cq { .. }) {
        Frame::Rx { sender: k1abc, recipient: nq6n, snr_db: -15, ack: false, reserved: false }
    } else {
        Frame::Cq { sender: nq6n, grid4: fn31, tx_pwr: 10, reserved: 0 }
    };
    let other_tones = encode_tones(other_frame);
    let p_m16 = p_n_2500 * 10f64.powf(-16.0 / 10.0);
    let amp_m16 = (2.0 * p_m16).sqrt() as i16;
    for _ in 0..100 {
        let mut p1 = vec![0i16; FS * 10];
        let mut p2 = vec![0i16; FS * 10];
        for (a, b) in p1.iter_mut().zip(p2.iter_mut()) {
            *a = rng_n1.gauss(sigma).clamp(-32767.0, 32767.0) as i16;
            *b = rng_n2.gauss(sigma).clamp(-32767.0, 32767.0) as i16;
        }
        let c1 = get_cur_llrs(&mut rx_a, &p1, 0);
        let c2 = get_cur_llrs(&mut rx_b, &p2, 0);
        if decode_llrs(&combine_llrs(&c1, &c2, 1)).is_some() { fa_cur_full += 1; }
        if decode_llrs(&combine_llrs(&quantize_4bit(&c1, 24.0), &quantize_4bit(&c2, 24.0), 3)).is_some() { fa_cur_4b += 1; }
        let s0_0 = (400 * FS) / 1000;
        let r1 = ref_demod_llrs(&p1, s0_0, 1037.0);
        let r2 = ref_demod_llrs(&p2, s0_0, 1037.0);
        if decode_llrs(&combine_llrs(&r1, &r2, 1)).is_some() { fa_ref_full += 1; }
        if decode_llrs(&combine_llrs(&quantize_4bit(&r1, 120.0), &quantize_4bit(&r2, 120.0), 8)).is_some() { fa_ref_4b += 1; }

        // Wrong-caller pairing: slot A carries weak_frame at -16 dB, slot B carries other_frame at -16 dB
        let (pa, sa) = make_slot(&mut rng_n1, -16.0, 0);
        let mut pb = vec![0.0f64; FS * 10];
        for s in &mut pb { *s = rng_n2.gauss(sigma); }
        let mut m = Modulator::new(other_tones, RATE_24K, 1037, DEFAULT_SPACING_HZ, amp_m16);
        let mut idx = s0_0;
        while let Some(v) = m.next_sample() {
            if idx < pb.len() { pb[idx] += v as f64; }
            idx += 1;
        }
        let pb_i16: Vec<i16> = pb.into_iter().map(|v| v.round().clamp(-32767.0, 32767.0) as i16).collect();
        let ra = ref_demod_llrs(&pa, sa, 1037.0);
        let rb = ref_demod_llrs(&pb_i16, s0_0, 1037.0);
        if let Some(f) = decode_llrs(&combine_llrs(&quantize_4bit(&ra, 120.0), &quantize_4bit(&rb, 120.0), 8)) {
            if f == other_frame {
                fa_misattrib_4b += 1;
            } else if f != weak_frame {
                fa_third_4b += 1;
            }
        }
    }
    println!("\nfalse-accept controls (100 pairs): noise cur(full/4b)={fa_cur_full}/{fa_cur_4b}, ref(full/4b)={fa_ref_full}/{fa_ref_4b}, wrong-caller 4b third={fa_third_4b} misattrib={fa_misattrib_4b}");
    ExitCode::SUCCESS
}

/// Priority-ordered blind refinement benchmark under fixed track work budgets `K in [1, 2, 4, 8]`,
/// refining candidates strictly in `Receiver`'s admission priority order without using true coordinates.
fn run_budget(args: &[String]) -> ExitCode {
    use std::time::Instant;
    let weak_wav = load_norm_burst(&args[0]);
    let nq6n = Callsign::encode(b"NQ6N").unwrap();
    let k1abc = Callsign::encode(b"K1ABC").unwrap();
    let fn31 = encode_grid4(b"FN31").unwrap();
    let weak_frame = if args[1].eq_ignore_ascii_case("cq") {
        Frame::Cq { sender: nq6n, grid4: fn31, tx_pwr: 10, reserved: 0 }
    } else if args[1].eq_ignore_ascii_case("rx") {
        Frame::Rx { sender: k1abc, recipient: nq6n, snr_db: -15, ack: false, reserved: false }
    } else {
        usage();
        return ExitCode::from(2);
    };
    let trials: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100);
    let weak_tones = encode_tones(weak_frame);
    const FS: usize = RATE_24K as usize;
    let sigma = 1_200.0f64;
    let p_n_2500 = sigma * sigma * (2500.0 / 12000.0);
    let seed_base = 0x3000_0000_0000_0001u64;

    println!("=== Priority-ordered blind refinement budget (trials={trials}, seed_base=0x{seed_base:016x}) ===");
    println!("SNR   | K=1 ok/fa (ms) | K=2 ok/fa (ms) | K=4 ok/fa (ms) | K=8 ok/fa (ms) | winning rank [0..7]");
    let mut rx = Receiver::new();
    for (si, &snr_db) in [-15.0f64, -16.0, -17.0, -18.0].iter().enumerate() {
        let p_weak = p_n_2500 * 10f64.powf(snr_db / 10.0);
        let weak_rms = p_weak.sqrt();
        let weak_amp = (2.0 * p_weak).sqrt() as i16;
        let mut rng = SweepRng(seed_base + (si as u64) * 997);

        let mut ok_k = [0usize; 4]; // K = 1, 2, 4, 8
        let mut fa_k = [0usize; 4];
        let mut us_k = [0u128; 4];
        let mut rank_hist = [0usize; usol950::rx::MAX_TRACKS];
        const KS: [usize; 4] = [1, 2, 4, 8];

        for t in 0..trials {
            let dt_ms = -160i32 + ((t as i32 * 53) % 401);
            let mut slot = vec![0.0f64; FS * 10];
            for s in &mut slot { *s = rng.gauss(sigma); }
            if let Some(wb) = weak_wav.as_deref() {
                let start = ((200 + dt_ms) as usize * FS) / 1000;
                let sc = weak_rms / 10_000.0;
                for (k, &v) in wb.iter().enumerate() {
                    if start + k < slot.len() { slot[start + k] += (v as f64) * sc; }
                }
            } else {
                let start = ((400 + dt_ms) as usize * FS) / 1000;
                let mut m = Modulator::new(weak_tones, RATE_24K, 1037, DEFAULT_SPACING_HZ, weak_amp);
                let mut i = start;
                while let Some(v) = m.next_sample() {
                    if i < slot.len() { slot[i] += v as f64; }
                    i += 1;
                }
            }
            let pcm: Vec<i16> = slot.into_iter().map(|v| v.round().clamp(-32767.0, 32767.0) as i16).collect();
            rx.reset();
            for blk in pcm.chunks(192) { rx.push_samples(blk); }

            let mut win_rank = None;
            let mut false_at_rank = [false; usol950::rx::MAX_TRACKS];
            let mut cum_us = 0u128;
            for ti in 0..usol950::rx::MAX_TRACKS {
                if let Some((est_ms, est_hz)) = rx.track_coords(ti) {
                    let t0 = Instant::now();
                    let est_samp = (est_ms as usize * FS) / 1000;
                    let (ref_samp, ref_hz) = refine_candidate(&pcm, est_samp, est_hz as f64);
                    let dec = decode_llrs(&ref_demod_llrs(&pcm, ref_samp, ref_hz));
                    cum_us += t0.elapsed().as_micros();
                    if let Some(f) = dec {
                        if f == weak_frame {
                            if win_rank.is_none() { win_rank = Some(ti); }
                        } else {
                            false_at_rank[ti] = true;
                        }
                    }
                }
                for (ki, &k) in KS.iter().enumerate() {
                    if ti + 1 == k {
                        us_k[ki] += cum_us;
                        if win_rank.is_some_and(|r| r < k) { ok_k[ki] += 1; }
                        if false_at_rank[..k].iter().any(|&b| b) { fa_k[ki] += 1; }
                    }
                }
            }
            if let Some(r) = win_rank { rank_hist[r] += 1; }
        }
        println!(
            "{snr_db:+5.1} | {:3}/{:<2} ({:4.1}) | {:3}/{:<2} ({:4.1}) | {:3}/{:<2} ({:4.1}) | {:3}/{:<2} ({:4.1}) | {:?}",
            ok_k[0], fa_k[0], us_k[0] as f64 / (trials as f64 * 1000.0),
            ok_k[1], fa_k[1], us_k[1] as f64 / (trials as f64 * 1000.0),
            ok_k[2], fa_k[2], us_k[2] as f64 / (trials as f64 * 1000.0),
            ok_k[3], fa_k[3], us_k[3] as f64 / (trials as f64 * 1000.0),
            rank_hist
        );
    }
    ExitCode::SUCCESS
}

/// Refine a blind `(est_samp, est_hz)` candidate using `S0 + S1` matched-filter energy over `+/-20 ms` (5 ms steps)
/// and `+/-4 Hz` (1 Hz steps).
fn refine_candidate(pcm: &[i16], est_samp: usize, est_hz: f64) -> (usize, f64) {
    use std::f64::consts::TAU;
    use usol950::phy::{S0, S1};
    const SYM_N: usize = 1920;
    let fs = RATE_24K as f64;
    let mut best_sc = -1.0f64;
    let mut best = (est_samp, est_hz);
    for dt_ms in (-20i32..=20).step_by(5) {
        let s0 = (est_samp as isize + (dt_ms as isize * RATE_24K as isize) / 1000).max(0) as usize;
        for df_i in -4i32..=4 {
            let f0 = est_hz + df_i as f64;
            let mut sc = 0.0f64;
            for &(base, pat) in &[(0usize, &S0), (18, &S1)] {
                for (k, &w) in pat.iter().enumerate() {
                    let a = s0 + (base + k) * SYM_N;
                    let w_rad = TAU * (f0 + (w as f64) * (DEFAULT_SPACING_HZ as f64)) / fs;
                    let (mut re, mut im) = (0.0f64, 0.0f64);
                    for n in (0..SYM_N).step_by(4) {
                        let x = pcm.get(a + n).copied().unwrap_or(0) as f64;
                        let ph = w_rad * (n as f64);
                        re += x * ph.cos();
                        im += x * ph.sin();
                    }
                    sc += re * re + im * im;
                }
            }
            if sc > best_sc {
                best_sc = sc;
                best = (s0, f0);
            }
        }
    }
    best
}

fn print_decoded(burst_start_ms: i64, d: &Decoded) {
    let mut s_buf = [0u8; 6];
    match d.frame {
        Frame::Cq { sender, grid4, tx_pwr, .. } => {
            let sl = sender.decode(&mut s_buf);
            let s_str = core::str::from_utf8(&s_buf[..sl]).unwrap_or("?");
            let g_str = decode_grid4(grid4)
                .map(|g| String::from_utf8_lossy(&g).into_owned())
                .unwrap_or_else(|| "----".to_string());
            println!(
                "t={burst_start_ms:6} ms | {:4} Hz | {:+3} dB | CQ {s_str} {g_str} (pwr_code={tx_pwr})",
                d.audio_hz, d.snr_db
            );
        }
        Frame::Rx { sender, recipient, snr_db, ack, .. } => {
            let sl = sender.decode(&mut s_buf);
            let s_str = core::str::from_utf8(&s_buf[..sl]).unwrap_or("?").to_string();
            let mut r_buf = [0u8; 6];
            let rl = recipient.decode(&mut r_buf);
            let r_str = core::str::from_utf8(&r_buf[..rl]).unwrap_or("?");
            let tag = if ack { "R+RX" } else { "RX" };
            println!(
                "t={burst_start_ms:6} ms | {:4} Hz | {:+3} dB | {s_str} -> {r_str} {tag} {:+3} dB",
                d.audio_hz, d.snr_db, snr_db
            );
        }
        Frame::ExtFrag { kind, idx, tag, .. } => {
            println!(
                "t={burst_start_ms:6} ms | {:4} Hz | {:+3} dB | EXT kind={kind} idx={idx} tag=0x{tag:02x}",
                d.audio_hz, d.snr_db
            );
        }
    }
}

fn resample_to_24k(in_rate: u32, samples: &[i16]) -> Vec<i16> {
    if in_rate == RATE_24K {
        return samples.to_vec();
    }
    if in_rate == 48_000 {
        return samples
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| ((c[0] as i32 + c[1] as i32) / 2) as i16)
            .collect();
    }
    let out_len = ((samples.len() as u64 * RATE_24K as u64) / in_rate as u64) as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos_q16 = ((i as u64 * in_rate as u64) << 16) / RATE_24K as u64;
        let idx = (pos_q16 >> 16) as usize;
        let frac = (pos_q16 & 0xFFFF) as i32;
        let s0 = samples[idx.min(samples.len().saturating_sub(1))] as i32;
        let s1 = samples[(idx + 1).min(samples.len().saturating_sub(1))] as i32;
        out.push((s0 + (((s1 - s0) * frac) >> 16)) as i16);
    }
    out
}

fn write_wav_mono16(path: &str, rate: u32, pcm: &[i16]) -> std::io::Result<()> {
    let data_bytes = (pcm.len() * 2) as u32;
    let mut buf = Vec::with_capacity(44 + data_bytes as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    buf.extend_from_slice(b"WAVEfmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&rate.to_le_bytes());
    buf.extend_from_slice(&(rate * 2).to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes());
    buf.extend_from_slice(&16u16.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_bytes.to_le_bytes());
    for &s in pcm {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    fs::write(path, buf)
}

fn read_wav_mono16(b: &[u8]) -> Option<(u32, Vec<i16>)> {
    if b.len() < 44 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12usize;
    let mut rate = 0u32;
    let mut channels = 0u16;
    let mut bits = 0u16;
    while pos + 8 <= b.len() {
        let tag = &b[pos..pos + 4];
        let len = u32::from_le_bytes(b[pos + 4..pos + 8].try_into().ok()?) as usize;
        pos += 8;
        if pos + len > b.len() {
            break;
        }
        if tag == b"fmt " && len >= 16 {
            let fmt = u16::from_le_bytes(b[pos..pos + 2].try_into().ok()?);
            channels = u16::from_le_bytes(b[pos + 2..pos + 4].try_into().ok()?);
            rate = u32::from_le_bytes(b[pos + 4..pos + 8].try_into().ok()?);
            bits = u16::from_le_bytes(b[pos + 14..pos + 16].try_into().ok()?);
            if fmt != 1 {
                return None;
            }
        } else if tag == b"data" && channels == 1 && bits == 16 && rate > 0 {
            let samples: Vec<i16> = b[pos..pos + len]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&[c0, c1]| i16::from_le_bytes([c0, c1]))
                .collect();
            return Some((rate, samples));
        }
        pos += len + (len & 1);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_burst_at_6_10s_in_15s_wav_decodes() {
        let w1aw = Callsign::encode(b"NQ6N").unwrap();
        let fn31 = encode_grid4(b"FN31").unwrap();
        let frame = Frame::Cq { sender: w1aw, grid4: fn31, tx_pwr: 10, reserved: 0 };
        let tones = encode_tones(frame);

        // Place an 8.80 s burst starting at 6.10 s and ending at 14.90 s inside a 15.00 s 16 kHz capture
        // (matching `ferrite_link.py PORT audio 15 ... --raw 16000`).
        let fs = 16_000u32;
        let mut pcm_16k = vec![0i16; (fs * 15) as usize];
        let start = (6_100 * fs as usize) / 1000;
        let mut m = Modulator::new(tones, fs, 1050, DEFAULT_SPACING_HZ, 10_000);
        let mut idx = start;
        while let Some(s) = m.next_sample() {
            pcm_16k[idx] = s;
            idx += 1;
        }
        assert_eq!(idx, (14_900 * fs as usize) / 1000);

        let pcm_24k = resample_to_24k(fs, &pcm_16k);
        let found = decode_pcm_24k(&pcm_24k);
        assert_eq!(found.len(), 1, "6.10s..14.90s burst in 15.00s recording must decode once");
        assert_eq!(found[0].1.frame, frame);
        assert!((found[0].0 - 6_100).abs() <= 40, "expected start ~6100 ms, got {} ms", found[0].0);
    }

    #[test]
    fn tone_analyzer_distinguishes_carrier_from_noise() {
        let fs = 16_000u32;
        let mut x = 0x1234_5678_9ABC_DEF0u64;
        let mut next_noise = || -> i16 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (((x >> 16) as i32 & 0x3FF) - 512) as i16
        };

        let noise_only: Vec<i16> = (0..(fs * 5)).map(|_| next_noise()).collect();
        let (_, _, _, present_noise) = analyze_tone_samples(fs, &noise_only).unwrap();
        assert!(!present_noise, "noise-only capture must report tone_present = false");

        let with_tone: Vec<i16> = (0..(fs * 5))
            .map(|i| {
                let c = (350.0 * (2.0 * std::f64::consts::PI * 1050.25 * i as f64 / fs as f64).cos()) as i16;
                next_noise().saturating_add(c)
            })
            .collect();
        let (hz, _, _, present_tone) = analyze_tone_samples(fs, &with_tone).unwrap();
        assert!(present_tone, "carrier capture must report tone_present = true");
        assert!((hz - 1050.25).abs() <= 0.15, "peak_hz {hz}");
    }
}
