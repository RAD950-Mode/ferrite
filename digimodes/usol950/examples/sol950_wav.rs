//! Host CLI for SOL950 bench testing and reference vector inspection:
//!
//!   cargo run -p usol950 --example sol950_wav -- cq W1AW FN31 1050 sol950-cq.wav
//!   cargo run -p usol950 --example sol950_wav -- rx K1ABC W1AW -14 0 1275 sol950-rx-frame.wav
//!   cargo run -p usol950 --example sol950_wav -- decode sol950-rx.wav

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
    eprintln!("  sol950_wav decode <IN.wav>");
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

    // Full 10.0 s slot at 24 kHz, burst starting at nominal +0.40 s.
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
    let slot_len = (RATE_24K * 10) as usize;
    let mut total = 0usize;
    for (slot_i, slot_pcm) in pcm_24k.chunks(slot_len).enumerate() {
        rx.reset();
        for blk in slot_pcm.chunks(192) {
            rx.push_samples(blk);
        }
        let mut out = [Decoded { frame: dummy, audio_hz: 0, time_offset_ms: 0, snr_db: 0 }; MAX_DECODED];
        let n = rx.finish_slot(&mut out);
        for d in &out[..n] {
            total += 1;
            print_decoded(slot_i, d);
        }
    }
    println!("decoded {total} frame(s) from {path} (input {rate} Hz, {} samples)", samples.len());
    ExitCode::SUCCESS
}

fn print_decoded(slot_i: usize, d: &Decoded) {
    let mut s_buf = [0u8; 6];
    match d.frame {
        Frame::Cq { sender, grid4, tx_pwr, .. } => {
            let sl = sender.decode(&mut s_buf);
            let s_str = core::str::from_utf8(&s_buf[..sl]).unwrap_or("?");
            let g_str = decode_grid4(grid4)
                .map(|g| String::from_utf8_lossy(&g).into_owned())
                .unwrap_or_else(|| "----".to_string());
            println!(
                "slot {slot_i:02} | {:+4} ms | {:4} Hz | {:+3} dB | CQ {s_str} {g_str} (pwr_code={tx_pwr})",
                d.time_offset_ms, d.audio_hz, d.snr_db
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
                "slot {slot_i:02} | {:+4} ms | {:4} Hz | {:+3} dB | {s_str} -> {r_str} {tag} {:+3} dB",
                d.time_offset_ms, d.audio_hz, d.snr_db, snr_db
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
    // Linear interpolation for 16 kHz (`ferrite_link.py --raw 16000`) or other rates -> 24 kHz.
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
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // mono
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
