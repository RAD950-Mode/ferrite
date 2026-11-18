//! Host CLI for SOL950 bench testing, carrier/BFO measurement, and asynchronous WAV decoding:
//!
//!   cargo run -p usol950 --example sol950_wav -- cq W1AW FN31 1050 sol950-cq.wav
//!   cargo run -p usol950 --example sol950_wav -- rx K1ABC W1AW -14 0 1275 sol950-rx-frame.wav
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

/// Decode a WAV file without assuming the recording began on a 10-second UTC slot boundary:
/// steps a 10.0 s window in `560 ms` (`7`-symbol) strides across the file (the receiver's internal `[-320, +360]` ms
/// search spans `680 ms`, so `560 ms` strides cover every possible burst start time seamlessly with 120 ms overlap)
/// and deduplicates bursts that appear in two overlapping windows.
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

    let mut rx = Receiver::new();
    let dummy = Frame::Cq {
        sender: Callsign::from_raw(0).unwrap(),
        grid4: GRID_UNKNOWN,
        tx_pwr: 0,
        reserved: 0,
    };
    let slot_samples = (RATE_24K * 10) as usize;
    let min_burst_samples = (RATE_24K as usize * 89) / 10; // 8.9 s minimum needed for a complete 8.8 s burst
    let stride_samples = (RATE_24K as usize * 560) / 1000; // 560 ms stride (< 680 ms receiver window)

    // `(burst_start_ms, Decoded)` deduplicated within +/- 1,000 ms for the same frame.
    let mut found: Vec<(i64, Decoded)> = Vec::new();
    let mut win_start = 0usize;
    loop {
        if win_start >= pcm_24k.len() {
            break;
        }
        let end = (win_start + slot_samples).min(pcm_24k.len());
        let slice = &pcm_24k[win_start..end];
        if slice.len() < min_burst_samples && win_start > 0 {
            break;
        }
        rx.reset();
        for blk in slice.chunks(192) {
            rx.push_samples(blk);
        }
        // Pad short tail up to 9.6 s so any burst starting near +0.08 s finishes all 110 symbols.
        if slice.len() < slot_samples {
            let pad_needed = slot_samples - slice.len();
            let zeros = [0i16; 192];
            let mut rem = pad_needed;
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
        if end == pcm_24k.len() {
            break;
        }
        win_start += stride_samples;
    }

    for (t_ms, d) in &found {
        print_decoded(*t_ms, d);
    }
    println!("decoded {} frame(s) from {path} (input {rate} Hz, {} samples)", found.len(), samples.len());
    ExitCode::SUCCESS
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
    // Average Welch periodograms at the native sample rate (`N = rate` -> exact 1.000 Hz bin spacing, zero resampling
    // filter droop on the noise floor).
    let n = rate as usize;
    if samples.len() < n || rate < 6_000 {
        eprintln!("{path}: need at least 1.0 second of audio at >= 6 kHz");
        return ExitCode::from(1);
    }
    let lo_bin = 200usize;
    let hi_bin = 2800usize.min(n / 2 - 2);
    let mut pwr = vec![0.0f64; hi_bin + 2];
    let mut blocks = 0usize;
    for chunk in samples.chunks_exact(n) {
        blocks += 1;
        // Direct Hann-windowed DFT over 200..=2801 Hz (fast on host for a few 1-second blocks, exact 1.000 Hz bins!).
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
    // Parabolic log-magnitude interpolation for sub-0.1 Hz peak accuracy:
    let a = pwr[peak_k - 1].max(1.0).ln();
    let b = pwr[peak_k].max(1.0).ln();
    let c = pwr[peak_k + 1].max(1.0).ln();
    let denom = a - 2.0 * b + c;
    let delta = if denom.abs() > 1e-12 { 0.5 * (a - c) / denom } else { 0.0 };
    let peak_hz = peak_k as f64 + delta.clamp(-0.5, 0.5);

    // Sum signal power over peak_k +/- 2 bins, and estimate noise spectral density from the remaining bins in 300..2800 Hz.
    let mut sig_plus_n = 0.0f64;
    let mut noise_sum = 0.0f64;
    let mut noise_bins = 0usize;
    for (k, &pk) in pwr.iter().enumerate().take(hi_bin + 1).skip(300) {
        if k.abs_diff(peak_k) <= 3 {
            if k.abs_diff(peak_k) <= 2 {
                sig_plus_n += pk;
            }
        } else {
            noise_sum += pk;
            noise_bins += 1;
        }
    }
    // Parseval identity for 1-second Hann DFT: signal sum over peak +/- 2 bins is `(3/16) N^2 P_s`, and expected
    // noise per bin is `(3/16) N^2 N_0` (one-sided PSD per Hz), so `(3/16) N^2` cancels identically!
    let noise_per_bin = noise_sum / noise_bins.max(1) as f64;
    let sig_pwr = (sig_plus_n - 5.0 * noise_per_bin).max(1e-9);
    let noise_2500 = noise_per_bin * 2500.0;
    let snr_2500_db = 10.0 * (sig_pwr / noise_2500.max(1e-9)).log10();

    println!("peak_hz     = {peak_hz:.2} Hz");
    println!("snr_2500_db = {snr_2500_db:+.1} dB  (averaged over {blocks} s, input {rate} Hz)");
    ExitCode::SUCCESS
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
