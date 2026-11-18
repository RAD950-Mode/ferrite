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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_burst_at_6_10s_in_15s_wav_decodes() {
        let w1aw = Callsign::encode(b"W1AW").unwrap();
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
