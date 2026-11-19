//! Interactive CLI for operating a HackRF One directly as a live RAD950 node.
//!
//! Uses `usol950` for exact LDPC/sync/frame encoding and decoding, and streams
//! `2.4 MS/s` signed 8-bit I/Q directly to/from `hackrf_transfer` (`-r -` / `-t -`).
//!
//! Quick start:
//!   cargo run -p usol950 --example rad950_hackrf -- --call NQ6N --grid FN31 --band 10m
//!   cargo run -p usol950 --example rad950_hackrf -- --loopback

use std::collections::VecDeque;
use std::env;
use std::f64::consts::TAU;
use std::fs::OpenOptions;
use std::io::{self, BufRead, Read, Write};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use usol950::frame::{decode_grid4, encode_grid4};
use usol950::{
    encode_tones, Callsign, Decoded, Frame, Receiver, DEFAULT_SPACING_HZ, GRID_UNKNOWN,
    MAX_DECODED, N_SYMBOLS, SUBCHANNELS_HZ,
};

/// HackRF sample rate: 2.4 MS/s = exact 100:1 integer multiple of 24 kHz.
const HACKRF_FS: u32 = 2_400_000;
/// IF offset below USB dial center so USB `[0..3 kHz]` sits at `[+198.5..+201.5 kHz]`,
/// well clear of the HackRF DC spike, and `-200 kHz` NCO is an exact 12-sample table.
const RF_OFFSET_HZ: i64 = 198_500;
/// Exact `-200 kHz` NCO frequency at 2.4 MS/s (`200_000 / 2_400_000 = 1 / 12`).
const NCO1_PERIOD: usize = 12;
/// Exact `+1,500 Hz` NCO frequency at 24 kHz (`1_500 / 24_000 = 1 / 16`).
const NCO2_PERIOD: usize = 16;

/// RX capture duration inside each 10.0 s UTC slot (leaves 550 ms guard for HackRF turnaround).
const RX_SLOT_MS: u64 = 9_450;
/// TX lead-in silence before symbol 0 (RAD950 nominal burst start = +400 ms).
const TX_LEAD_MS: usize = 400;
/// TX trailing silence after last symbol.
const TX_TAIL_MS: usize = 150;
/// Symbol duration (80 ms).
const SYM_MS: usize = 80;
/// Raised-cosine burst edge ramp (10 ms).
const RAMP_MS: usize = 10;

const BANDS: &[(&str, u64)] = &[
    ("10m", 28_078_000),
    ("12m", 24_919_000),
    ("15m", 21_078_000),
    ("17m", 18_104_000),
];

#[derive(Clone)]
struct NodeConfig {
    call: Callsign,
    call_str: String,
    grid4: u16,
    grid_str: String,
    dial_hz: u64,
    subch: usize,
    lna_db: u8,
    vga_db: u8,
    tx_vga_db: u8,
    amp: bool,
    dry_tx: bool,
    serial: Option<String>,
    log_path: String,
}

#[derive(Clone)]
struct HeardEntry {
    call: Callsign,
    call_str: String,
    grid_str: String,
    snr_db: i8,
    tone_hz: u16,
    last_slot: u64,
    kind: &'static str,
}

#[derive(Clone)]
struct QsoLogEntry {
    utc_sec: u64,
    dial_hz: u64,
    peer: String,
    grid: String,
    rx_snr: i8,
    tx_snr: i8,
}

#[derive(Clone)]
struct TxJob {
    frame: Frame,
    subch: usize,
    summary: String,
}

struct SharedState {
    cfg: NodeConfig,
    running: bool,
    auto_cq: bool,
    auto_qso: bool,
    cq_parity: u64,
    tx_queue: VecDeque<TxJob>,
    heard: Vec<HeardEntry>,
    qso_log: Vec<QsoLogEntry>,
    active_peer: Option<(Callsign, String, String, i8)>, // (call, str, grid, their_snr_at_us)
    slots_rx: u64,
    slots_tx: u64,
    frames_rx: u64,
}

fn usage() {
    eprintln!("RAD950 HackRF Direct Node CLI");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  rad950_hackrf [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --call <CALL>        Station callsign (default: NQ6N)");
    eprintln!("  --grid <GRID4|NONE>  4-char Maidenhead grid (default: FN31)");
    eprintln!("  --band <10m|12m|15m|17m>  RAD950 calling band (default: 10m = 28.078 MHz)");
    eprintln!("  --freq <HZ>          Custom USB dial frequency in Hz (e.g. 28185000)");
    eprintln!("  --subch <0..7>       TX audio subchannel index (default: 0 = 600 Hz)");
    eprintln!("  --lna <0..40>        HackRF RX LNA gain in dB, 8 dB steps (default: 16)");
    eprintln!("  --vga <0..62>        HackRF RX VGA gain in dB, 2 dB steps (default: 20)");
    eprintln!("  --tx-gain <0..47>    HackRF TX VGA gain in dB (default: 16)");
    eprintln!("  --amp                Enable HackRF RF amplifier (default: off)");
    eprintln!("  --serial <HEX>       HackRF serial number suffix (if multiple connected)");
    eprintln!("  --dry-tx             Dry TX safety mode (synthesize without keying HackRF)");
    eprintln!("  --log <PATH>         QSO log file path (default: rad950_hackrf.log)");
    eprintln!("  --loopback           Run offline TX->2.4MS/s cs8->RX DDC->Decoder self-test");
    eprintln!("  -h, --help           Show this help");
}

fn band_freq(name: &str) -> Option<u64> {
    BANDS
        .iter()
        .find(|(b, _)| b.eq_ignore_ascii_case(name))
        .map(|(_, f)| *f)
}

fn band_name(dial_hz: u64) -> &'static str {
    for &(name, f) in BANDS {
        if dial_hz.abs_diff(f) < 500_000 {
            return name;
        }
    }
    "HF"
}

fn parse_call(s: &str) -> Result<Callsign, String> {
    Callsign::encode(s.trim().to_ascii_uppercase().as_bytes())
        .map_err(|e| format!("invalid callsign '{s}': {e:?}"))
}

fn parse_grid(s: &str) -> Result<(u16, String), String> {
    let up = s.trim().to_ascii_uppercase();
    if up == "NONE" || up == "----" || up.is_empty() {
        return Ok((GRID_UNKNOWN, "----".to_string()));
    }
    if up.len() != 4 {
        return Err(format!("grid must be 4 chars (e.g. FN31) or NONE, got '{s}'"));
    }
    let mut b = [0u8; 4];
    b.copy_from_slice(up.as_bytes());
    let g = encode_grid4(&b).ok_or_else(|| format!("invalid Maidenhead grid '{up}'"))?;
    Ok((g, up))
}

fn format_call(c: Callsign) -> String {
    let mut buf = [0u8; 6];
    let n = c.decode(&mut buf);
    String::from_utf8_lossy(&buf[..n]).to_string()
}

fn frame_sender(f: Frame) -> Option<Callsign> {
    match f {
        Frame::Cq { sender, .. } | Frame::Rx { sender, .. } => Some(sender),
        Frame::ExtFrag { .. } => None,
    }
}

fn format_grid(g: u16) -> String {
    if g == GRID_UNKNOWN {
        return "----".to_string();
    }
    if let Some(b) = decode_grid4(g) {
        String::from_utf8_lossy(&b).to_string()
    } else {
        "----".to_string()
    }
}

fn utc_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn format_utc_hms(utc_sec: u64) -> String {
    let s = utc_sec % 86_400;
    format!("{:02}:{:02}:{:02}Z", s / 3600, (s % 3600) / 60, s % 60)
}

/// Synthesize a phase-continuous complex 4-FSK RAD950 burst directly at `2.4 MS/s`
/// as interleaved signed 8-bit I/Q (`cs8`), placed at `RF_OFFSET_HZ + audio_hz + tone * 50 Hz`.
fn synthesize_tx_cs8(frame: Frame, subch: usize) -> Vec<u8> {
    let tones = encode_tones(frame);
    let audio_hz = SUBCHANNELS_HZ[subch.min(SUBCHANNELS_HZ.len() - 1)] as f64;
    let fs = HACKRF_FS as f64;

    let lead_samples = (TX_LEAD_MS * HACKRF_FS as usize) / 1000;
    let sym_samples = (SYM_MS * HACKRF_FS as usize) / 1000;
    let ramp_samples = (RAMP_MS * HACKRF_FS as usize) / 1000;
    let tail_samples = (TX_TAIL_MS * HACKRF_FS as usize) / 1000;
    let burst_samples = N_SYMBOLS * sym_samples;
    let total_samples = lead_samples + burst_samples + tail_samples;

    let mut out = vec![0u8; total_samples * 2];
    let mut phase = 0.0f64;
    let amp_peak = 104.0f64; // comfortable headroom inside [-127, 127]

    for (sym_idx, &tone_u8) in tones.iter().enumerate() {
        let tone = tone_u8 as f64;
        let f_hz = RF_OFFSET_HZ as f64 + audio_hz + tone * DEFAULT_SPACING_HZ as f64;
        let dphi = TAU * f_hz / fs;

        for k in 0..sym_samples {
            let burst_pos = sym_idx * sym_samples + k;
            let env = if burst_pos < ramp_samples {
                0.5 * (1.0 - (std::f64::consts::PI * burst_pos as f64 / ramp_samples as f64).cos())
            } else if burst_pos >= burst_samples - ramp_samples {
                let rem = burst_samples - 1 - burst_pos;
                0.5 * (1.0 - (std::f64::consts::PI * rem as f64 / ramp_samples as f64).cos())
            } else {
                1.0
            };

            let mag = amp_peak * env;
            let i_val = (mag * phase.cos()).round().clamp(-127.0, 127.0) as i8;
            let q_val = (mag * phase.sin()).round().clamp(-127.0, 127.0) as i8;
            let dst = (lead_samples + burst_pos) * 2;
            out[dst] = i_val as u8;
            out[dst + 1] = q_val as u8;

            phase += dphi;
            if phase >= TAU {
                phase -= TAU;
            }
        }
    }
    out
}

/// Streaming single-sideband USB digital downconverter:
/// `2.4 MS/s` `cs8` I/Q (centered at `dial - 198.5 kHz`) -> `24 kHz` real USB `i16` audio `[0..3 kHz]`.
///
/// Stage 1: Exact 12-sample `-200 kHz` complex NCO -> USB `[0..3 kHz]` lands at `[-1.5..+1.5 kHz]`.
/// Stage 2: 10:1 CIC/boxcar decimation (`2.4 MS/s -> 240 kHz`).
/// Stage 3: 31-tap Hann-windowed sinc LPF (`f_c = 1.65 kHz` at `240 kHz`, rejecting LSB) + 10:1 decimation (`-> 24 kHz`).
/// Stage 4: Exact 16-sample `+1.5 kHz` complex NCO at `24 kHz` + `Re(z)` -> pure USB `[0..3 kHz]`.
struct UsbDdc {
    nco1_re: [f32; NCO1_PERIOD],
    nco1_im: [f32; NCO1_PERIOD],
    nco1_idx: usize,

    // Stage 2: 10:1 accumulator
    acc_re: f32,
    acc_im: f32,
    acc_n: usize,

    // Stage 3: 31-tap FIR at 240 kHz, decimate by 10 -> 24 kHz
    fir_taps: [f32; 31],
    ring_re: [f32; 31],
    ring_im: [f32; 31],
    ring_pos: usize,
    dec2_n: usize,

    // Stage 4: +1.5 kHz NCO at 24 kHz
    nco2_re: [f32; NCO2_PERIOD],
    nco2_im: [f32; NCO2_PERIOD],
    nco2_idx: usize,

    // Simple AGC / peak tracker for healthy 16-bit scaling
    dc_est: f32,
    gain: f32,
}

impl UsbDdc {
    fn new() -> Self {
        let mut nco1_re = [0.0f32; NCO1_PERIOD];
        let mut nco1_im = [0.0f32; NCO1_PERIOD];
        for k in 0..NCO1_PERIOD {
            let a = -TAU * (k as f64) / (NCO1_PERIOD as f64);
            nco1_re[k] = a.cos() as f32;
            nco1_im[k] = a.sin() as f32;
        }

        let mut nco2_re = [0.0f32; NCO2_PERIOD];
        let mut nco2_im = [0.0f32; NCO2_PERIOD];
        for k in 0..NCO2_PERIOD {
            let a = TAU * (k as f64) / (NCO2_PERIOD as f64);
            nco2_re[k] = a.cos() as f32;
            nco2_im[k] = a.sin() as f32;
        }

        // 31-tap Hann-windowed sinc LPF at 240 kHz, cutoff = 1.65 kHz (passes +-1.5 kHz USB cleanly).
        let mut fir_taps = [0.0f32; 31];
        let fc = 1_650.0 / 240_000.0;
        let mid = 15.0f64;
        let mut sum = 0.0f64;
        for (i, t) in fir_taps.iter_mut().enumerate() {
            let n = i as f64 - mid;
            let sinc = if n.abs() < 1e-9 {
                2.0 * fc
            } else {
                (TAU * fc * n).sin() / (std::f64::consts::PI * n)
            };
            let win = 0.5 * (1.0 - (TAU * i as f64 / 30.0).cos());
            let v = sinc * win;
            *t = v as f32;
            sum += v;
        }
        for t in &mut fir_taps {
            *t /= sum as f32;
        }

        Self {
            nco1_re,
            nco1_im,
            nco1_idx: 0,
            acc_re: 0.0,
            acc_im: 0.0,
            acc_n: 0,
            fir_taps,
            ring_re: [0.0; 31],
            ring_im: [0.0; 31],
            ring_pos: 0,
            dec2_n: 0,
            nco2_re,
            nco2_im,
            nco2_idx: 0,
            dc_est: 0.0,
            gain: 220.0, // maps full-scale cs8 (~104) to ~22,880 i16
        }
    }

    /// Process a chunk of interleaved signed 8-bit I/Q bytes and append `24 kHz` `i16` audio to `out`.
    fn process_cs8(&mut self, bytes: &[u8], out: &mut Vec<i16>) {
        let pairs = bytes.len() / 2;
        for p in 0..pairs {
            let i = (bytes[p * 2] as i8) as f32;
            let q = (bytes[p * 2 + 1] as i8) as f32;

            // Mix down by -200 kHz
            let cr = self.nco1_re[self.nco1_idx];
            let ci = self.nco1_im[self.nco1_idx];
            self.nco1_idx += 1;
            if self.nco1_idx == NCO1_PERIOD {
                self.nco1_idx = 0;
            }
            let mr = i * cr - q * ci;
            let mi = i * ci + q * cr;

            self.acc_re += mr;
            self.acc_im += mi;
            self.acc_n += 1;
            if self.acc_n == 10 {
                let s_re = self.acc_re * 0.1;
                let s_im = self.acc_im * 0.1;
                self.acc_re = 0.0;
                self.acc_im = 0.0;
                self.acc_n = 0;

                // Push into 240 kHz FIR ring
                self.ring_re[self.ring_pos] = s_re;
                self.ring_im[self.ring_pos] = s_im;
                self.ring_pos += 1;
                if self.ring_pos == 31 {
                    self.ring_pos = 0;
                }

                self.dec2_n += 1;
                if self.dec2_n == 10 {
                    self.dec2_n = 0;
                    // Compute 31-tap FIR
                    let mut fr = 0.0f32;
                    let mut fi = 0.0f32;
                    let mut idx = self.ring_pos;
                    for &tap in &self.fir_taps {
                        fr += self.ring_re[idx] * tap;
                        fi += self.ring_im[idx] * tap;
                        idx += 1;
                        if idx == 31 {
                            idx = 0;
                        }
                    }

                    // Mix up by +1,500 Hz at 24 kHz and take real part -> USB [0..3 kHz]
                    let u_re = self.nco2_re[self.nco2_idx];
                    let u_im = self.nco2_im[self.nco2_idx];
                    self.nco2_idx += 1;
                    if self.nco2_idx == NCO2_PERIOD {
                        self.nco2_idx = 0;
                    }
                    let usb = fr * u_re - fi * u_im;
                    self.dc_est += (usb - self.dc_est) * 0.002;
                    let sample = ((usb - self.dc_est) * self.gain).clamp(-32767.0, 32767.0) as i16;
                    out.push(sample);
                }
            }
        }
    }
}

fn check_hackrf_installed(serial: Option<&str>) -> Result<String, String> {
    let out = Command::new("hackrf_info")
        .output()
        .map_err(|e| format!("failed to run hackrf_info (is hackrf installed?): {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() || stdout.contains("No HackRF boards found") {
        return Err(format!("No HackRF board detected.\n{stdout}{stderr}"));
    }
    if let Some(s) = serial {
        if !stdout.contains(s) {
            return Err(format!("HackRF serial '{s}' not found in hackrf_info:\n{stdout}"));
        }
    }
    Ok(stdout)
}

fn transmit_slot(cfg: &NodeConfig, job: &TxJob) -> Result<(), String> {
    let cs8 = synthesize_tx_cs8(job.frame, job.subch);
    if cfg.dry_tx {
        println!(
            "  [DRY-TX] {} (subch {} = {} Hz, {} bytes @ 2.4 MS/s — RF suppressed)",
            job.summary,
            job.subch,
            SUBCHANNELS_HZ[job.subch],
            cs8.len()
        );
        thread::sleep(Duration::from_millis(9_200));
        return Ok(());
    }

    let center_hz = (cfg.dial_hz as i64 - RF_OFFSET_HZ).max(1_000_000) as u64;
    let mut cmd = Command::new("hackrf_transfer");
    cmd.args([
        "-t",
        "-",
        "-f",
        &center_hz.to_string(),
        "-s",
        &HACKRF_FS.to_string(),
        "-a",
        if cfg.amp { "1" } else { "0" },
        "-p",
        "0",
        "-x",
        &cfg.tx_vga_db.to_string(),
    ]);
    if let Some(ref ser) = cfg.serial {
        cmd.args(["-d", ser]);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn hackrf_transfer TX: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&cs8);
        let _ = stdin.flush();
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("hackrf_transfer TX wait failed: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("hackrf_transfer TX exited {}: {}", out.status, err.trim()));
    }
    Ok(())
}

fn receive_slot(cfg: &NodeConfig) -> Result<(Vec<Decoded>, f32), String> {
    let center_hz = (cfg.dial_hz as i64 - RF_OFFSET_HZ).max(1_000_000) as u64;
    let n_samples: u64 = (HACKRF_FS as u64 * RX_SLOT_MS) / 1000;

    let mut cmd = Command::new("hackrf_transfer");
    cmd.args([
        "-r",
        "-",
        "-f",
        &center_hz.to_string(),
        "-s",
        &HACKRF_FS.to_string(),
        "-n",
        &n_samples.to_string(),
        "-a",
        if cfg.amp { "1" } else { "0" },
        "-p",
        "0",
        "-l",
        &cfg.lna_db.to_string(),
        "-g",
        &cfg.vga_db.to_string(),
    ]);
    if let Some(ref ser) = cfg.serial {
        cmd.args(["-d", ser]);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn hackrf_transfer RX: {e}"))?;

    let mut stdout = child.stdout.take().ok_or("no stdout from hackrf_transfer")?;
    let mut ddc = UsbDdc::new();
    let mut rx = Receiver::new();
    let mut iq_buf = vec![0u8; 48_000]; // 24,000 complex samples = 10 ms at 2.4 MS/s
    let mut pcm_buf = Vec::with_capacity(512);
    let mut leftover: Option<u8> = None;
    let mut peak_abs: i16 = 0;

    loop {
        let mut start = 0usize;
        if let Some(b) = leftover.take() {
            iq_buf[0] = b;
            start = 1;
        }
        let n = stdout
            .read(&mut iq_buf[start..])
            .map_err(|e| format!("read from hackrf_transfer RX failed: {e}"))?;
        if n == 0 {
            break;
        }
        let total = start + n;
        let even = total & !1;
        if total != even {
            leftover = Some(iq_buf[even]);
        }
        pcm_buf.clear();
        ddc.process_cs8(&iq_buf[..even], &mut pcm_buf);
        for &s in &pcm_buf {
            let a = s.saturating_abs();
            if a > peak_abs {
                peak_abs = a;
            }
        }
        rx.push_samples(&pcm_buf);
    }

    let out = child
        .wait_with_output()
        .map_err(|e| format!("hackrf_transfer RX wait failed: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if !err.contains("User cancel") && !err.contains("exiting") {
            return Err(format!("hackrf_transfer RX exited {}: {}", out.status, err.trim()));
        }
    }

    let mut decoded = [Decoded::EMPTY; MAX_DECODED];
    let count = rx.finish_slot(&mut decoded);
    let peak_pct = (peak_abs as f32 / 32767.0) * 100.0;
    Ok((decoded[..count].to_vec(), peak_pct))
}

fn append_qso_log(path: &str, entry: &QsoLogEntry) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(
            f,
            "{}  {:.3} MHz  {:<10}  grid={}  rx={:+}dB  tx={:+}dB",
            format_utc_hms(entry.utc_sec),
            entry.dial_hz as f64 / 1_000_000.0,
            entry.peer,
            entry.grid,
            entry.rx_snr,
            entry.tx_snr
        );
    }
}

fn handle_decoded(state: &Arc<Mutex<SharedState>>, slot: u64, frames: &[Decoded]) {
    let mut st = state.lock().unwrap();
    let my_call = st.cfg.call;
    let my_subch = st.cfg.subch;
    let dial_hz = st.cfg.dial_hz;
    let log_path = st.cfg.log_path.clone();
    let utc_sec = slot * 10;

    for d in frames {
        st.frames_rx += 1;
        let sender_call = frame_sender(d.frame);
        let sender_str = sender_call.map(format_call).unwrap_or_else(|| "EXT".to_string());
        let (kind, grid_str, detail) = match d.frame {
            Frame::Cq { grid4, tx_pwr, .. } => {
                let g = format_grid(grid4);
                ("CQ", g.clone(), format!("CQ {sender_str} {g} (pwr={tx_pwr})"))
            }
            Frame::Rx {
                recipient,
                snr_db,
                ack,
                ..
            } => {
                let rcpt_str = format_call(recipient);
                let tag = if ack { "R+RX" } else { "RX" };
                (
                    tag,
                    "----".to_string(),
                    format!("{rcpt_str} de {sender_str} {tag} {:+}dB", snr_db),
                )
            }
            Frame::ExtFrag {
                kind,
                idx,
                tag,
                payload,
            } => (
                "EXT",
                "----".to_string(),
                format!("EXT kind={kind} idx={idx} tag=0x{tag:02X} payload=0x{payload:012X}"),
            ),
        };

        println!(
            "  [RX {}] {:+3} dB  {:4} Hz  {}",
            format_utc_hms(utc_sec),
            d.snr_db,
            d.audio_hz,
            detail
        );

        // Update heard table
        if let Some(sc) = sender_call {
            if let Some(existing) = st.heard.iter_mut().find(|h| h.call == sc) {
                existing.snr_db = d.snr_db;
                existing.tone_hz = d.audio_hz;
                existing.last_slot = slot;
                existing.kind = kind;
                if grid_str != "----" {
                    existing.grid_str = grid_str.clone();
                }
            } else {
                st.heard.insert(
                    0,
                    HeardEntry {
                        call: sc,
                        call_str: sender_str.clone(),
                        grid_str: grid_str.clone(),
                        snr_db: d.snr_db,
                        tone_hz: d.audio_hz,
                        last_slot: slot,
                        kind,
                    },
                );
                if st.heard.len() > 32 {
                    st.heard.pop();
                }
            }
        }

        // Auto-QSO state machine for frames addressed to us
        if let Frame::Rx {
            sender,
            recipient,
            snr_db: their_rpt_for_us,
            ack,
            ..
        } = d.frame
        {
            if recipient == my_call && st.auto_qso {
                let peer_grid = st
                    .heard
                    .iter()
                    .find(|h| h.call == sender)
                    .map(|h| h.grid_str.clone())
                    .unwrap_or_else(|| "----".to_string());

                if !ack {
                    // Station answered our CQ -> send R+RX (ack=true) and log QSO!
                    let reply = Frame::Rx {
                        sender: my_call,
                        recipient: sender,
                        snr_db: d.snr_db,
                        ack: true,
                        reserved: false,
                    };
                    let summary = format!("{sender_str} de {} R+RX {:+}dB", st.cfg.call_str, d.snr_db);
                    st.tx_queue.push_front(TxJob {
                        frame: reply,
                        subch: my_subch,
                        summary,
                    });
                    let qso = QsoLogEntry {
                        utc_sec,
                        dial_hz,
                        peer: sender_str.clone(),
                        grid: peer_grid,
                        rx_snr: d.snr_db,
                        tx_snr: their_rpt_for_us,
                    };
                    println!(
                        "  [QSO] Logged {} (rx={:+}dB, tx={:+}dB) — queued R+RX ACK",
                        qso.peer, qso.rx_snr, qso.tx_snr
                    );
                    append_qso_log(&log_path, &qso);
                    st.qso_log.push(qso);
                    st.active_peer = None;
                } else {
                    // Received R+RX ACK -> QSO complete!
                    let qso = QsoLogEntry {
                        utc_sec,
                        dial_hz,
                        peer: sender_str.clone(),
                        grid: peer_grid,
                        rx_snr: d.snr_db,
                        tx_snr: their_rpt_for_us,
                    };
                    println!(
                        "  [QSO] Complete & logged {} (rx={:+}dB, tx={:+}dB)",
                        qso.peer, qso.rx_snr, qso.tx_snr
                    );
                    append_qso_log(&log_path, &qso);
                    st.qso_log.push(qso);
                    st.active_peer = None;
                }
            }
        }
    }
}

fn slot_worker(state: Arc<Mutex<SharedState>>) {
    loop {
        if !state.lock().unwrap().running {
            break;
        }

        // Wait until the exact start of the next 10-second UTC slot
        let now_ms = utc_now_ms();
        let rem_ms = 10_000 - (now_ms % 10_000);
        // If we are within the first 250 ms of a slot, start immediately; else wait for next boundary.
        if (now_ms % 10_000) > 250 {
            let wake_step = 200u64;
            let mut left = rem_ms;
            while left > 0 {
                if !state.lock().unwrap().running {
                    return;
                }
                let d = left.min(wake_step);
                thread::sleep(Duration::from_millis(d));
                left -= d;
            }
        }

        let slot_ms = utc_now_ms();
        let slot = slot_ms / 10_000;
        let utc_sec = slot * 10;

        // Decide TX vs RX for this slot
        let (cfg, tx_job) = {
            let mut st = state.lock().unwrap();
            if !st.running {
                break;
            }
            let job = if let Some(j) = st.tx_queue.pop_front() {
                Some(j)
            } else if st.auto_cq && (slot % 2 == st.cq_parity) {
                let frame = Frame::Cq {
                    sender: st.cfg.call,
                    grid4: st.cfg.grid4,
                    tx_pwr: 10,
                    reserved: 0,
                };
                Some(TxJob {
                    frame,
                    subch: st.cfg.subch,
                    summary: format!("CQ {} {}", st.cfg.call_str, st.cfg.grid_str),
                })
            } else {
                None
            };
            (st.cfg.clone(), job)
        };

        if let Some(job) = tx_job {
            println!(
                "\n[{} slot {}] TX  {:.3} MHz  subch {} ({} Hz)  {}",
                format_utc_hms(utc_sec),
                slot % 1000,
                cfg.dial_hz as f64 / 1_000_000.0,
                job.subch,
                SUBCHANNELS_HZ[job.subch],
                job.summary
            );
            match transmit_slot(&cfg, &job) {
                Ok(()) => {
                    state.lock().unwrap().slots_tx += 1;
                }
                Err(e) => {
                    eprintln!("  [TX ERROR] {e}");
                }
            }
        } else {
            println!(
                "\n[{} slot {}] RX  {:.3} MHz ({})  listening 9.45s...",
                format_utc_hms(utc_sec),
                slot % 1000,
                cfg.dial_hz as f64 / 1_000_000.0,
                band_name(cfg.dial_hz)
            );
            match receive_slot(&cfg) {
                Ok((frames, peak_pct)) => {
                    state.lock().unwrap().slots_rx += 1;
                    if frames.is_empty() {
                        println!("  [RX] 0 decodes (audio peak {peak_pct:.1}%)");
                    } else {
                        handle_decoded(&state, slot, &frames);
                    }
                }
                Err(e) => {
                    eprintln!("  [RX ERROR] {e}");
                    thread::sleep(Duration::from_millis(1000));
                }
            }
        }

        print!("rad950> ");
        let _ = io::stdout().flush();
    }
}

fn print_repl_help() {
    println!("Commands:");
    println!("  cq                       Queue one CQ in the next 10 s UTC slot");
    println!("  cq auto [even|odd]       Toggle continuous alternating-slot CQ");
    println!("  cq stop                  Stop auto-CQ and clear TX queue");
    println!("  ans <N>                  Answer heard station #N (sends RX)");
    println!("  reply <CALL> [SNR] [ack] Send RX (or R+RX with 'ack') to <CALL>");
    println!("  autoqso <on|off>         Auto-complete & log QSOs when answered (default: on)");
    println!("  band <10m|12m|15m|17m>   Switch to RAD950 calling dial");
    println!("  freq <HZ>                Set custom USB dial frequency in Hz");
    println!("  subch <0..7>             Set TX audio subchannel (600..2175 Hz)");
    println!("  gain lna <dB> | vga <dB> | tx <dB> | amp <on|off>");
    println!("  dry <on|off>             Toggle Dry TX safety mode");
    println!("  heard                    Show recently heard stations");
    println!("  log                      Show logged QSOs");
    println!("  status                   Show current node configuration & stats");
    println!("  help | quit");
}

fn run_loopback_selftest() -> ExitCode {
    println!("=== RAD950 HackRF DSP Loopback Self-Test (2.4 MS/s cs8 -> USB DDC -> usol950::Receiver) ===");
    let nq6n = Callsign::encode(b"NQ6N").unwrap();
    let k1abc = Callsign::encode(b"K1ABC").unwrap();
    let mut g = [0u8; 4];
    g.copy_from_slice(b"FN31");
    let fn31 = encode_grid4(&g).unwrap();

    let test_cases = [
        (
            "CQ NQ6N FN31 (subch 0 = 600 Hz)",
            Frame::Cq {
                sender: nq6n,
                grid4: fn31,
                tx_pwr: 10,
                reserved: 0,
            },
            0usize,
        ),
        (
            "RX K1ABC de NQ6N -12 dB ACK (subch 4 = 1500 Hz)",
            Frame::Rx {
                sender: nq6n,
                recipient: k1abc,
                snr_db: -12,
                ack: true,
                reserved: false,
            },
            4usize,
        ),
    ];

    for (label, frame, subch) in test_cases {
        let t0 = std::time::Instant::now();
        let cs8 = synthesize_tx_cs8(frame, subch);
        let synth_ms = t0.elapsed().as_millis();

        let t1 = std::time::Instant::now();
        let mut ddc = UsbDdc::new();
        let mut rx = Receiver::new();
        let mut pcm = Vec::with_capacity(512);
        for chunk in cs8.chunks(48_000) {
            pcm.clear();
            ddc.process_cs8(chunk, &mut pcm);
            rx.push_samples(&pcm);
        }
        let mut out = [Decoded::EMPTY; MAX_DECODED];
        let n = rx.finish_slot(&mut out);
        let rx_ms = t1.elapsed().as_millis();

        println!(
            "  {label}: synth={synth_ms}ms ({} MB), ddc+decode={rx_ms}ms -> {n} decode(s)",
            cs8.len() / (1024 * 1024)
        );
        if n == 0 || out[0].frame.pack_u64() != frame.pack_u64() {
            eprintln!("  FAIL: expected {:?}, got {:?}", frame, &out[..n]);
            return ExitCode::from(1);
        }
        println!(
            "    PASS: decoded {:?} at {} Hz, SNR {:+} dB",
            out[0].frame, out[0].audio_hz, out[0].snr_db
        );
    }

    println!("=== Loopback self-test PASSED (2/2) ===");
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let mut call_str = "NQ6N".to_string();
    let mut grid_str = "FN31".to_string();
    let mut dial_hz: u64 = 28_078_000;
    let mut subch: usize = 0;
    let mut lna_db: u8 = 16;
    let mut vga_db: u8 = 20;
    let mut tx_vga_db: u8 = 16;
    let mut amp = false;
    let mut dry_tx = false;
    let mut serial: Option<String> = None;
    let mut log_path = "rad950_hackrf.log".to_string();
    let mut loopback = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                usage();
                return ExitCode::SUCCESS;
            }
            "--loopback" => loopback = true,
            "--amp" => amp = true,
            "--dry-tx" => dry_tx = true,
            "--call" if i + 1 < args.len() => {
                i += 1;
                call_str = args[i].to_ascii_uppercase();
            }
            "--grid" if i + 1 < args.len() => {
                i += 1;
                grid_str = args[i].to_ascii_uppercase();
            }
            "--band" if i + 1 < args.len() => {
                i += 1;
                match band_freq(&args[i]) {
                    Some(f) => dial_hz = f,
                    None => {
                        eprintln!("unknown band '{}' (use 10m, 12m, 15m, or 17m)", args[i]);
                        return ExitCode::from(2);
                    }
                }
            }
            "--freq" if i + 1 < args.len() => {
                i += 1;
                match args[i].parse::<u64>() {
                    Ok(f) if (1_000_000..=6_000_000_000).contains(&f) => dial_hz = f,
                    _ => {
                        eprintln!("invalid --freq '{}'", args[i]);
                        return ExitCode::from(2);
                    }
                }
            }
            "--subch" if i + 1 < args.len() => {
                i += 1;
                subch = args[i].parse::<usize>().unwrap_or(0).min(SUBCHANNELS_HZ.len() - 1);
            }
            "--lna" if i + 1 < args.len() => {
                i += 1;
                lna_db = (args[i].parse::<u8>().unwrap_or(16).min(40) / 8) * 8;
            }
            "--vga" if i + 1 < args.len() => {
                i += 1;
                vga_db = (args[i].parse::<u8>().unwrap_or(20).min(62) / 2) * 2;
            }
            "--tx-gain" if i + 1 < args.len() => {
                i += 1;
                tx_vga_db = args[i].parse::<u8>().unwrap_or(16).min(47);
            }
            "--serial" if i + 1 < args.len() => {
                i += 1;
                serial = Some(args[i].clone());
            }
            "--log" if i + 1 < args.len() => {
                i += 1;
                log_path = args[i].clone();
            }
            other => {
                eprintln!("unknown argument: {other}");
                usage();
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    if loopback {
        return run_loopback_selftest();
    }

    let call = match parse_call(&call_str) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let (grid4, grid_clean) = match parse_grid(&grid_str) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    match check_hackrf_installed(serial.as_deref()) {
        Ok(info) => {
            let line = info
                .lines()
                .find(|l| l.contains("Serial number") || l.contains("Board ID"))
                .unwrap_or("HackRF ready");
            println!("Connected: {}", line.trim());
        }
        Err(e) => {
            eprintln!("{e}");
            eprintln!("Tip: run with --loopback to test the TX/RX DSP chain without hardware.");
            return ExitCode::from(1);
        }
    }

    let cfg = NodeConfig {
        call,
        call_str: format_call(call),
        grid4,
        grid_str: grid_clean,
        dial_hz,
        subch,
        lna_db,
        vga_db,
        tx_vga_db,
        amp,
        dry_tx,
        serial,
        log_path,
    };

    println!(
        "RAD950 HackRF Node: {} [{}]  dial={:.3} MHz ({})  subch={} ({} Hz)  LNA={}dB VGA={}dB TX={}dB amp={} dry={}",
        cfg.call_str,
        cfg.grid_str,
        cfg.dial_hz as f64 / 1_000_000.0,
        band_name(cfg.dial_hz),
        cfg.subch,
        SUBCHANNELS_HZ[cfg.subch],
        cfg.lna_db,
        cfg.vga_db,
        cfg.tx_vga_db,
        if cfg.amp { "on" } else { "off" },
        if cfg.dry_tx { "ON" } else { "off" }
    );
    println!("Type 'help' for commands, 'cq' to call CQ, 'quit' to exit.");

    let state = Arc::new(Mutex::new(SharedState {
        cfg,
        running: true,
        auto_cq: false,
        auto_qso: true,
        cq_parity: 0,
        tx_queue: VecDeque::new(),
        heard: Vec::new(),
        qso_log: Vec::new(),
        active_peer: None,
        slots_rx: 0,
        slots_tx: 0,
        frames_rx: 0,
    }));

    let worker_state = Arc::clone(&state);
    let worker = thread::spawn(move || slot_worker(worker_state));

    let stdin = io::stdin();
    print!("rad950> ");
    let _ = io::stdout().flush();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() {
            print!("rad950> ");
            let _ = io::stdout().flush();
            continue;
        }

        match parts[0].to_ascii_lowercase().as_str() {
            "help" | "?" => print_repl_help(),
            "quit" | "exit" | "q" => {
                state.lock().unwrap().running = false;
                println!("Stopping after current slot...");
                break;
            }
            "cq" => {
                let mut st = state.lock().unwrap();
                if parts.get(1).is_some_and(|s| s.eq_ignore_ascii_case("auto")) {
                    st.auto_cq = true;
                    if let Some(p) = parts.get(2) {
                        st.cq_parity = if p.eq_ignore_ascii_case("odd") { 1 } else { 0 };
                    } else {
                        st.cq_parity = (utc_now_ms() / 10_000 + 1) % 2;
                    }
                    println!(
                        "Auto-CQ ON (transmitting on {} UTC slots)",
                        if st.cq_parity == 0 { "even" } else { "odd" }
                    );
                } else if parts.get(1).is_some_and(|s| s.eq_ignore_ascii_case("stop")) {
                    st.auto_cq = false;
                    st.tx_queue.clear();
                    println!("Auto-CQ stopped and TX queue cleared.");
                } else {
                    let frame = Frame::Cq {
                        sender: st.cfg.call,
                        grid4: st.cfg.grid4,
                        tx_pwr: 10,
                        reserved: 0,
                    };
                    let subch = st.cfg.subch;
                    let summary = format!("CQ {} {}", st.cfg.call_str, st.cfg.grid_str);
                    st.tx_queue.push_back(TxJob {
                        frame,
                        subch,
                        summary: summary.clone(),
                    });
                    println!("Queued for next UTC slot: {summary}");
                }
            }
            "ans" | "answer" => {
                let idx = parts
                    .get(1)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(1);
                let mut st = state.lock().unwrap();
                if idx == 0 || idx > st.heard.len() {
                    println!("No heard station #{idx}. Run 'heard' first.");
                } else {
                    let h = st.heard[idx - 1].clone();
                    let frame = Frame::Rx {
                        sender: st.cfg.call,
                        recipient: h.call,
                        snr_db: h.snr_db,
                        ack: false,
                        reserved: false,
                    };
                    let subch = st.cfg.subch;
                    let summary = format!("{} de {} RX {:+}dB", h.call_str, st.cfg.call_str, h.snr_db);
                    st.tx_queue.push_front(TxJob {
                        frame,
                        subch,
                        summary: summary.clone(),
                    });
                    st.active_peer = Some((h.call, h.call_str, h.grid_str, h.snr_db));
                    println!("Queued answer: {summary}");
                }
            }
            "reply" => {
                if parts.len() < 2 {
                    println!("Usage: reply <CALL> [SNR_DB] [ack]");
                } else if let Ok(rcpt) = parse_call(parts[1]) {
                    let snr: i8 = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(-10);
                    let ack = parts.iter().any(|p| p.eq_ignore_ascii_case("ack"));
                    let mut st = state.lock().unwrap();
                    let frame = Frame::Rx {
                        sender: st.cfg.call,
                        recipient: rcpt,
                        snr_db: snr,
                        ack,
                        reserved: false,
                    };
                    let subch = st.cfg.subch;
                    let tag = if ack { "R+RX" } else { "RX" };
                    let summary = format!("{} de {} {tag} {:+}dB", format_call(rcpt), st.cfg.call_str, snr);
                    st.tx_queue.push_front(TxJob {
                        frame,
                        subch,
                        summary: summary.clone(),
                    });
                    println!("Queued: {summary}");
                } else {
                    println!("Invalid callsign '{}'", parts[1]);
                }
            }
            "autoqso" => {
                let mut st = state.lock().unwrap();
                if let Some(v) = parts.get(1) {
                    st.auto_qso = matches!(v.to_ascii_lowercase().as_str(), "on" | "1" | "true");
                }
                println!("Auto-QSO: {}", if st.auto_qso { "ON" } else { "off" });
            }
            "band" => {
                if let Some(b) = parts.get(1).and_then(|s| band_freq(s)) {
                    let mut st = state.lock().unwrap();
                    st.cfg.dial_hz = b;
                    println!("Dial set to {:.3} MHz ({})", b as f64 / 1_000_000.0, band_name(b));
                } else {
                    println!("Usage: band <10m|12m|15m|17m>");
                }
            }
            "freq" => {
                if let Some(Ok(f)) = parts.get(1).map(|s| s.parse::<u64>()) {
                    let mut st = state.lock().unwrap();
                    st.cfg.dial_hz = f;
                    println!("Dial set to {:.3} MHz", f as f64 / 1_000_000.0);
                } else {
                    println!("Usage: freq <HZ> (e.g. freq 28078000)");
                }
            }
            "subch" => {
                if let Some(Ok(sc)) = parts.get(1).map(|s| s.parse::<usize>()) {
                    if sc < SUBCHANNELS_HZ.len() {
                        let mut st = state.lock().unwrap();
                        st.cfg.subch = sc;
                        println!("TX subchannel set to {sc} ({} Hz)", SUBCHANNELS_HZ[sc]);
                    } else {
                        println!("subch must be 0..{}", SUBCHANNELS_HZ.len() - 1);
                    }
                } else {
                    println!("Usage: subch <0..7>");
                }
            }
            "gain" => {
                let mut st = state.lock().unwrap();
                match (parts.get(1).copied(), parts.get(2).copied()) {
                    (Some("lna"), Some(v)) => {
                        st.cfg.lna_db = (v.parse::<u8>().unwrap_or(st.cfg.lna_db).min(40) / 8) * 8;
                    }
                    (Some("vga"), Some(v)) => {
                        st.cfg.vga_db = (v.parse::<u8>().unwrap_or(st.cfg.vga_db).min(62) / 2) * 2;
                    }
                    (Some("tx"), Some(v)) => {
                        st.cfg.tx_vga_db = v.parse::<u8>().unwrap_or(st.cfg.tx_vga_db).min(47);
                    }
                    (Some("amp"), Some(v)) => {
                        st.cfg.amp = matches!(v.to_ascii_lowercase().as_str(), "on" | "1" | "true");
                    }
                    _ => {}
                }
                println!(
                    "Gains: LNA={}dB VGA={}dB TX={}dB amp={}",
                    st.cfg.lna_db,
                    st.cfg.vga_db,
                    st.cfg.tx_vga_db,
                    if st.cfg.amp { "on" } else { "off" }
                );
            }
            "dry" => {
                let mut st = state.lock().unwrap();
                if let Some(v) = parts.get(1) {
                    st.cfg.dry_tx = matches!(v.to_ascii_lowercase().as_str(), "on" | "1" | "true");
                }
                println!("Dry TX: {}", if st.cfg.dry_tx { "ON" } else { "off" });
            }
            "heard" => {
                let st = state.lock().unwrap();
                if st.heard.is_empty() {
                    println!("No stations heard yet.");
                } else {
                    let cur_slot = utc_now_ms() / 10_000;
                    for (i, h) in st.heard.iter().enumerate() {
                        let age_s = cur_slot.saturating_sub(h.last_slot) * 10;
                        println!(
                            "  {:2}) {:<10} {:<4}  {:+3} dB  {:4} Hz  {:<4}  ({}s ago)",
                            i + 1,
                            h.call_str,
                            h.grid_str,
                            h.snr_db,
                            h.tone_hz,
                            h.kind,
                            age_s
                        );
                    }
                }
            }
            "log" => {
                let st = state.lock().unwrap();
                if st.qso_log.is_empty() {
                    println!("No QSOs logged this session (file: {}).", st.cfg.log_path);
                } else {
                    for (i, q) in st.qso_log.iter().enumerate() {
                        println!(
                            "  {:2}) {}  {:.3} MHz  {:<10} {}  rx={:+}dB tx={:+}dB",
                            i + 1,
                            format_utc_hms(q.utc_sec),
                            q.dial_hz as f64 / 1_000_000.0,
                            q.peer,
                            q.grid,
                            q.rx_snr,
                            q.tx_snr
                        );
                    }
                }
            }
            "status" => {
                let st = state.lock().unwrap();
                let now_ms = utc_now_ms();
                println!(
                    "Node: {} [{}]  dial={:.3} MHz ({})  subch={} ({} Hz)  UTC={} (next slot in {:.1}s)",
                    st.cfg.call_str,
                    st.cfg.grid_str,
                    st.cfg.dial_hz as f64 / 1_000_000.0,
                    band_name(st.cfg.dial_hz),
                    st.cfg.subch,
                    SUBCHANNELS_HZ[st.cfg.subch],
                    format_utc_hms(now_ms / 1000),
                    (10_000 - (now_ms % 10_000)) as f64 / 1000.0
                );
                println!(
                    "Mode: auto_cq={} auto_qso={} dry_tx={}  queued_tx={}  rx_slots={} tx_slots={} decodes={} qsos={}",
                    st.auto_cq,
                    st.auto_qso,
                    st.cfg.dry_tx,
                    st.tx_queue.len(),
                    st.slots_rx,
                    st.slots_tx,
                    st.frames_rx,
                    st.qso_log.len()
                );
            }
            other => println!("Unknown command '{other}'. Type 'help'."),
        }

        print!("rad950> ");
        let _ = io::stdout().flush();
    }

    let _ = worker.join();
    ExitCode::SUCCESS
}
