//! Interactive TUI for operating a HackRF One directly as a live RAD950 node.
//!
//! Uses `usol950` for exact LDPC/sync/frame encoding and decoding, and streams
//! `2.4 MS/s` signed 8-bit I/Q directly to/from `hackrf_transfer` (`-r -` / `-t -`).
//!
//! Quick start:
//!   cargo run --release -p usol950 --example rad950_hackrf -- --call NQ6N --grid FN31 --band 10m --dry-tx
//!   cargo run --release -p usol950 --example rad950_hackrf -- --loopback

use std::collections::{HashSet, VecDeque};
use std::env;
use std::f64::consts::TAU;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Operate,
    Settings,
    Log,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotPhase {
    Waiting,
    Rx,
    Tx,
    DryTx,
}

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

/// Exact same 16-level RX and TX RGB565 palettes as the RT-950 Pro `draw_panafall`.
const WF_PALETTE: [u16; 16] = [
    0x0000, 0x0821, 0x0864, 0x08C8, 0x114C, 0x11D1, 0x1A75, 0x1B39,
    0x2439, 0x2554, 0x366D, 0x5728, 0x9FA4, 0xDF02, 0xFD60, 0xFFE6,
];
const TX_PALETTE: [u16; 16] = [
    0x1000, 0x2000, 0x3000, 0x4000, 0x5800, 0x7000, 0x8800, 0xA000,
    0xB800, 0xD000, 0xE820, 0xF880, 0xFA04, 0xFBA8, 0xFD8E, 0xFEF6,
];

const WF_HISTORY_MAX: usize = 160;

#[derive(Clone, Copy, Default)]
struct WfCol {
    bins: [u8; 24],
    is_tx: bool,
    slot_mark: bool,
}

fn rgb565_to_rgb888(c: u16) -> (u8, u8, u8) {
    let r = (((c >> 11) & 0x1F) as u32 * 255 / 31) as u8;
    let g = (((c >> 5) & 0x3F) as u32 * 255 / 63) as u8;
    let b = ((c & 0x1F) as u32 * 255 / 31) as u8;
    (r, g, b)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkedBadge {
    None,
    Worked,
    WorkedNewBand,
    WorkedNewGrid,
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
    worked: WorkedBadge,
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
    worked_calls: HashSet<String>,
    worked_grids_all: HashSet<String>,
    worked_grids_band: HashSet<(String, String)>, // (band_name, grid)
    events: VecDeque<String>,
    phase: SlotPhase,
    phase_note: String,
    peak_pct: f32,
    wf_hist: VecDeque<WfCol>,
    hackrf_banner: String,
    slots_rx: u64,
    slots_tx: u64,
    frames_rx: u64,
}

impl SharedState {
    fn push_event(&mut self, msg: String) {
        self.events.push_back(msg);
        while self.events.len() > 200 {
            self.events.pop_front();
        }
    }

    fn push_wf(&mut self, col: WfCol) {
        self.wf_hist.push_back(col);
        while self.wf_hist.len() > WF_HISTORY_MAX {
            self.wf_hist.pop_front();
        }
    }

    fn record_qso(&mut self, qso: QsoLogEntry) -> WorkedBadge {
        let band = band_name(qso.dial_hz).to_string();
        let valid_grid = qso.grid.len() == 4 && qso.grid != "----";
        let badge = if valid_grid && !self.worked_grids_all.contains(&qso.grid) {
            WorkedBadge::WorkedNewGrid
        } else if valid_grid && !self.worked_grids_band.contains(&(band.clone(), qso.grid.clone())) {
            WorkedBadge::WorkedNewBand
        } else {
            WorkedBadge::Worked
        };
        self.worked_calls.insert(qso.peer.clone());
        if valid_grid {
            self.worked_grids_all.insert(qso.grid.clone());
            self.worked_grids_band.insert((band, qso.grid.clone()));
        }
        append_qso_log(&self.cfg.log_path, &qso);
        self.qso_log.push(qso);
        badge
    }
}

fn load_existing_log(
    path: &str,
) -> (HashSet<String>, HashSet<String>, HashSet<(String, String)>) {
    let mut calls = HashSet::new();
    let mut grids_all = HashSet::new();
    let mut grids_band = HashSet::new();
    if let Ok(content) = fs::read_to_string(path) {
        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            // Format: HH:MM:SSZ  28.078 MHz  CALL  grid=FN31  rx=-10dB  tx=-12dB
            if parts.len() >= 5 {
                let mhz: f64 = parts[1].parse().unwrap_or(28.078);
                let band = band_name((mhz * 1_000_000.0) as u64).to_string();
                let peer = parts[3].to_string();
                calls.insert(peer);
                if let Some(g) = parts[4].strip_prefix("grid=") {
                    if g.len() == 4 && g != "----" {
                        grids_all.insert(g.to_string());
                        grids_band.insert((band, g.to_string()));
                    }
                }
            }
        }
    }
    (calls, grids_all, grids_band)
}

fn usage() {
    eprintln!("RAD950 HackRF Direct Node TUI");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  rad950_hackrf [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --call <CALL>             Station callsign (default: NQ6N)");
    eprintln!("  --grid <GRID4|NONE>       4-char Maidenhead grid (default: FN31)");
    eprintln!("  --band <10m|12m|15m|17m>  RAD950 calling band (default: 10m = 28.078 MHz)");
    eprintln!("  --freq <HZ>               Custom USB dial frequency in Hz (e.g. 28185000)");
    eprintln!("  --subch <0..7>            TX audio subchannel index (default: 0 = 600 Hz)");
    eprintln!("  --lna <0..40>             HackRF RX LNA gain in dB, 8 dB steps (default: 16)");
    eprintln!("  --vga <0..62>             HackRF RX VGA gain in dB, 2 dB steps (default: 20)");
    eprintln!("  --tx-gain <0..47>         HackRF TX VGA gain in dB (default: 16)");
    eprintln!("  --amp                     Enable HackRF RF amplifier (default: off)");
    eprintln!("  --serial <HEX>            HackRF serial number suffix (if multiple connected)");
    eprintln!("  --dry-tx                  Dry TX safety mode (synthesize without keying HackRF)");
    eprintln!("  --log <PATH>              QSO log file path (default: rad950_hackrf.log)");
    eprintln!("  --loopback                Run offline TX->2.4MS/s cs8->RX DDC->Decoder self-test");
    eprintln!("  -h, --help                Show this help");
}

fn band_freq(name: &str) -> Option<u64> {
    BANDS
        .iter()
        .find(|(b, _)| b.eq_ignore_ascii_case(name))
        .map(|(_, f)| *f)
}

fn band_index(dial_hz: u64) -> Option<usize> {
    BANDS.iter().position(|(_, f)| *f == dial_hz)
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
    let amp_peak = 104.0f64;

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
struct UsbDdc {
    nco1_re: [f32; NCO1_PERIOD],
    nco1_im: [f32; NCO1_PERIOD],
    nco1_idx: usize,

    acc_re: f32,
    acc_im: f32,
    acc_n: usize,

    fir_taps: [f32; 31],
    ring_re: [f32; 31],
    ring_im: [f32; 31],
    ring_pos: usize,
    dec2_n: usize,

    nco2_re: [f32; NCO2_PERIOD],
    nco2_im: [f32; NCO2_PERIOD],
    nco2_idx: usize,

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
            gain: 220.0,
        }
    }

    fn process_cs8(&mut self, bytes: &[u8], out: &mut Vec<i16>) {
        let pairs = bytes.len() / 2;
        for p in 0..pairs {
            let i = (bytes[p * 2] as i8) as f32;
            let q = (bytes[p * 2 + 1] as i8) as f32;

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

                self.ring_re[self.ring_pos] = s_re;
                self.ring_im[self.ring_pos] = s_im;
                self.ring_pos += 1;
                if self.ring_pos == 31 {
                    self.ring_pos = 0;
                }

                self.dec2_n += 1;
                if self.dec2_n == 10 {
                    self.dec2_n = 0;
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
    let ser = stdout
        .lines()
        .find(|l| l.contains("Serial number"))
        .map(|l| l.trim().to_string())
        .unwrap_or_else(|| "HackRF One".to_string());
    Ok(ser)
}

fn make_tx_wf_col(tones: &[u8; N_SYMBOLS], subch: usize, elapsed_ms: u64, slot_mark: bool) -> WfCol {
    let mut bins = [0u8; 24];
    let lead = TX_LEAD_MS as u64;
    let burst_end = lead + (N_SYMBOLS * SYM_MS) as u64;
    if elapsed_ms >= lead && elapsed_ms < burst_end {
        let sym_idx = ((elapsed_ms - lead) / SYM_MS as u64) as usize;
        let tone = tones[sym_idx.min(N_SYMBOLS - 1)] as f64;
        let f_hz = SUBCHANNELS_HZ[subch.min(SUBCHANNELS_HZ.len() - 1)] as f64
            + tone * DEFAULT_SPACING_HZ as f64;
        // 24 panafall bins span 600..2400 Hz (75 Hz/bin).
        let pos = ((f_hz - 600.0) / 75.0).clamp(0.0, 23.0);
        let b0 = pos.floor() as usize;
        let frac = pos - b0 as f64;
        bins[b0] = (15.0 * (1.0 - frac * 0.55)).round().clamp(1.0, 15.0) as u8;
        if b0 + 1 < 24 && frac > 0.05 {
            bins[b0 + 1] = (15.0 * (0.45 + frac * 0.55)).round().clamp(1.0, 15.0) as u8;
        }
    }
    WfCol {
        bins,
        is_tx: true,
        slot_mark,
    }
}

fn transmit_slot(cfg: &NodeConfig, job: &TxJob, state: &Arc<Mutex<SharedState>>) -> Result<(), String> {
    let tones = encode_tones(job.frame);
    let cs8 = synthesize_tx_cs8(job.frame, job.subch);
    if cfg.dry_tx {
        let t0 = std::time::Instant::now();
        let mut first = true;
        while t0.elapsed() < Duration::from_millis(9_350) {
            if !state.lock().unwrap().running {
                return Ok(());
            }
            let elapsed_ms = t0.elapsed().as_millis() as u64;
            state
                .lock()
                .unwrap()
                .push_wf(make_tx_wf_col(&tones, job.subch, elapsed_ms, first));
            first = false;
            thread::sleep(Duration::from_millis(160));
        }
        return Ok(());
    }

    // Background ticker to paint the actual live 4-FSK TX tones onto the panafall while hackrf_transfer transmits.
    let tx_running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let tx_flag = Arc::clone(&tx_running);
    let tx_state = Arc::clone(state);
    let tx_subch = job.subch;
    let wf_ticker = thread::spawn(move || {
        let t0 = std::time::Instant::now();
        let mut first = true;
        while tx_flag.load(std::sync::atomic::Ordering::Relaxed) {
            let elapsed_ms = t0.elapsed().as_millis() as u64;
            tx_state
                .lock()
                .unwrap()
                .push_wf(make_tx_wf_col(&tones, tx_subch, elapsed_ms, first));
            first = false;
            thread::sleep(Duration::from_millis(160));
        }
    });

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

    let res = (|| -> Result<(), String> {
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
    })();

    tx_running.store(false, std::sync::atomic::Ordering::Relaxed);
    let _ = wf_ticker.join();
    res
}

fn receive_slot(cfg: &NodeConfig, state: &Arc<Mutex<SharedState>>) -> Result<(Vec<Decoded>, f32), String> {
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
    let mut iq_buf = vec![0u8; 48_000];
    let mut pcm_buf = Vec::with_capacity(512);
    let mut leftover: Option<u8> = None;
    let mut peak_abs: i16 = 0;
    let mut chunk_peak: i16 = 0;
    let mut chunks = 0u32;
    let mut first_wf = true;

    loop {
        if !state.lock().unwrap().running {
            let _ = child.kill();
            break;
        }
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
            if a > chunk_peak {
                chunk_peak = a;
            }
        }
        rx.push_samples(&pcm_buf);
        chunks += 1;
        if chunks.is_multiple_of(8) {
            let wf_opt = rx.take_wf_col();
            let pct = (chunk_peak as f32 / 32767.0) * 100.0;
            chunk_peak = 0;
            let mut st = state.lock().unwrap();
            st.peak_pct = pct;
            if let Some(packed) = wf_opt {
                let mut bins = [0u8; 24];
                for b in 0..12 {
                    bins[b * 2] = packed[b] & 0x0F;
                    bins[b * 2 + 1] = (packed[b] >> 4) & 0x0F;
                }
                st.push_wf(WfCol {
                    bins,
                    is_tx: false,
                    slot_mark: first_wf,
                });
                first_wf = false;
            }
        }
    }

    let out = child
        .wait_with_output()
        .map_err(|e| format!("hackrf_transfer RX wait failed: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if !err.contains("User cancel") && !err.contains("exiting") && state.lock().unwrap().running {
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
                format!("EXT k={kind} i={idx} t=0x{tag:02X} p=0x{payload:012X}"),
            ),
        };

        st.push_event(format!(
            "[{}] RX {:+3}dB {:4}Hz  {}",
            format_utc_hms(utc_sec),
            d.snr_db,
            d.audio_hz,
            detail
        ));

        if let Some(sc) = sender_call {
            let already_worked = if st.worked_calls.contains(&sender_str) {
                WorkedBadge::Worked
            } else {
                WorkedBadge::None
            };
            if let Some(existing) = st.heard.iter_mut().find(|h| h.call == sc) {
                existing.snr_db = d.snr_db;
                existing.tone_hz = d.audio_hz;
                existing.last_slot = slot;
                existing.kind = kind;
                if existing.worked == WorkedBadge::None {
                    existing.worked = already_worked;
                }
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
                        worked: already_worked,
                    },
                );
                if st.heard.len() > 32 {
                    st.heard.pop();
                }
            }
        }

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
                    let badge = st.record_qso(qso);
                    st.push_event(format!(
                        "[{}] QSO logged {} (rx={:+}dB tx={:+}dB) -> R+RX queued",
                        format_utc_hms(utc_sec),
                        sender_str,
                        d.snr_db,
                        their_rpt_for_us
                    ));
                    if let Some(h) = st.heard.iter_mut().find(|h| h.call == sender) {
                        h.worked = badge;
                    }
                } else {
                    let qso = QsoLogEntry {
                        utc_sec,
                        dial_hz,
                        peer: sender_str.clone(),
                        grid: peer_grid,
                        rx_snr: d.snr_db,
                        tx_snr: their_rpt_for_us,
                    };
                    let badge = st.record_qso(qso);
                    st.push_event(format!(
                        "[{}] QSO complete & logged {} (rx={:+}dB tx={:+}dB)",
                        format_utc_hms(utc_sec),
                        sender_str,
                        d.snr_db,
                        their_rpt_for_us
                    ));
                    if let Some(h) = st.heard.iter_mut().find(|h| h.call == sender) {
                        h.worked = badge;
                    }
                }
            }
        }
    }
}

fn slot_worker(state: Arc<Mutex<SharedState>>) {
    let mut last_tx_slot: Option<u64> = None;
    loop {
        if !state.lock().unwrap().running {
            break;
        }

        let now_ms = utc_now_ms();
        let rem_ms = 10_000 - (now_ms % 10_000);
        if (now_ms % 10_000) > 250 {
            {
                let mut st = state.lock().unwrap();
                st.phase = SlotPhase::Waiting;
                st.phase_note = "Syncing to next 10s UTC slot...".to_string();
            }
            let mut left = rem_ms;
            while left > 0 {
                if !state.lock().unwrap().running {
                    return;
                }
                let d = left.min(100);
                thread::sleep(Duration::from_millis(d));
                left -= d;
            }
        }

        let slot_ms = utc_now_ms();
        let slot = slot_ms / 10_000;
        let utc_sec = slot * 10;

        // Enforce strict alternating TX/RX turns: always listen for at least 1 RX slot after any TX slot.
        let tx_allowed = last_tx_slot.is_none_or(|prev| slot >= prev + 2);

        let (cfg, tx_job) = {
            let mut st = state.lock().unwrap();
            if !st.running {
                break;
            }
            let job = if !tx_allowed {
                None
            } else if let Some(j) = st.tx_queue.pop_front() {
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
            last_tx_slot = Some(slot);
            {
                let mut st = state.lock().unwrap();
                st.phase = if cfg.dry_tx { SlotPhase::DryTx } else { SlotPhase::Tx };
                st.phase_note = format!(
                    "{} (subch {} = {} Hz)",
                    job.summary, job.subch, SUBCHANNELS_HZ[job.subch]
                );
                st.push_event(format!(
                    "[{}] {} {:.3}MHz {}Hz  {}",
                    format_utc_hms(utc_sec),
                    if cfg.dry_tx { "DRY-TX" } else { "TX    " },
                    cfg.dial_hz as f64 / 1_000_000.0,
                    SUBCHANNELS_HZ[job.subch],
                    job.summary
                ));
            }
            match transmit_slot(&cfg, &job, &state) {
                Ok(()) => {
                    state.lock().unwrap().slots_tx += 1;
                }
                Err(e) => {
                    state
                        .lock()
                        .unwrap()
                        .push_event(format!("[{}] TX ERROR: {e}", format_utc_hms(utc_sec)));
                }
            }
        } else {
            {
                let mut st = state.lock().unwrap();
                st.phase = SlotPhase::Rx;
                st.phase_note = format!(
                    "Listening {:.3} MHz ({}) LNA={} VGA={}",
                    cfg.dial_hz as f64 / 1_000_000.0,
                    band_name(cfg.dial_hz),
                    cfg.lna_db,
                    cfg.vga_db
                );
            }
            match receive_slot(&cfg, &state) {
                Ok((frames, peak_pct)) => {
                    let mut st = state.lock().unwrap();
                    st.slots_rx += 1;
                    st.peak_pct = peak_pct;
                    if frames.is_empty() {
                        st.push_event(format!(
                            "[{}] RX {:.3}MHz  0 decodes (peak {:.0}%)",
                            format_utc_hms(utc_sec),
                            cfg.dial_hz as f64 / 1_000_000.0,
                            peak_pct
                        ));
                        drop(st);
                    } else {
                        drop(st);
                        handle_decoded(&state, slot, &frames);
                    }
                }
                Err(e) => {
                    state
                        .lock()
                        .unwrap()
                        .push_event(format!("[{}] RX ERROR: {e}", format_utc_hms(utc_sec)));
                    thread::sleep(Duration::from_millis(800));
                }
            }
        }
    }
}

/// RAII terminal raw-mode + alternate-screen guard using POSIX `stty` (works on macOS & Linux offline).
struct RawTerm {
    saved: String,
}

impl RawTerm {
    fn enter() -> io::Result<Self> {
        let saved = Command::new("stty")
            .arg("-g")
            .stdin(Stdio::inherit())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let _ = Command::new("stty")
            .args(["raw", "-echo", "min", "0", "time", "1"])
            .stdin(Stdio::inherit())
            .status();
        let mut out = io::stdout();
        let _ = write!(out, "\x1b[?1049h\x1b[?25l\x1b[2J");
        let _ = out.flush();
        Ok(Self { saved })
    }
}

impl Drop for RawTerm {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = write!(out, "\x1b[0m\x1b[?25h\x1b[?1049l");
        let _ = out.flush();
        if !self.saved.is_empty() {
            let _ = Command::new("stty")
                .arg(&self.saved)
                .stdin(Stdio::inherit())
                .status();
        } else {
            let _ = Command::new("stty")
                .arg("sane")
                .stdin(Stdio::inherit())
                .status();
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Key {
    Char(char),
    Enter,
    Esc,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    Tab,
    CtrlC,
}

fn read_key() -> Option<Key> {
    let mut b = [0u8; 1];
    let n = io::stdin().read(&mut b).ok()?;
    if n == 0 {
        return None;
    }
    match b[0] {
        3 => Some(Key::CtrlC),
        9 => Some(Key::Tab),
        10 | 13 => Some(Key::Enter),
        127 | 8 => Some(Key::Backspace),
        27 => {
            let mut seq = [0u8; 2];
            if io::stdin().read(&mut seq[0..1]).unwrap_or(0) == 1
                && seq[0] == b'['
                && io::stdin().read(&mut seq[1..2]).unwrap_or(0) == 1
            {
                return match seq[1] {
                    b'A' => Some(Key::Up),
                    b'B' => Some(Key::Down),
                    b'C' => Some(Key::Right),
                    b'D' => Some(Key::Left),
                    _ => Some(Key::Esc),
                };
            }
            Some(Key::Esc)
        }
        c if (32..=126).contains(&c) => Some(Key::Char(c as char)),
        _ => None,
    }
}

fn term_size() -> (usize, usize) {
    if let Ok(out) = Command::new("stty")
        .arg("size")
        .stdin(Stdio::inherit())
        .output()
    {
        let s = String::from_utf8_lossy(&out.stdout);
        let mut it = s.split_whitespace();
        if let (Some(r), Some(c)) = (it.next(), it.next()) {
            if let (Ok(rows), Ok(cols)) = (r.parse::<usize>(), c.parse::<usize>()) {
                return (cols.max(72), rows.max(20));
            }
        }
    }
    (96, 28)
}

fn trunc(s: &str, max_w: usize) -> String {
    if s.chars().count() <= max_w {
        s.to_string()
    } else if max_w > 1 {
        let mut out: String = s.chars().take(max_w - 1).collect();
        out.push('…');
        out
    } else {
        String::new()
    }
}

struct TuiState {
    view: View,
    heard_sel: usize,
    set_sel: usize,
    editing: Option<String>,
    toast: Option<(String, u64)>,
}

impl TuiState {
    fn set_toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), utc_now_ms() + 3_000));
    }
}

const SETTINGS_COUNT: usize = 12;

fn render_tui(state: &Arc<Mutex<SharedState>>, tui: &TuiState) {
    let (cols, rows) = term_size();
    let st = state.lock().unwrap();
    let now_ms = utc_now_ms();
    let slot_elapsed_ms = now_ms % 10_000;
    let cur_slot = now_ms / 10_000;

    let mut buf = String::with_capacity(8192);
    buf.push_str("\x1b[H");

    // 1. Top title bar
    let tx_badge = if st.cfg.dry_tx {
        "\x1b[1;30;43m DRY-TX \x1b[0;1;37;44m"
    } else {
        "\x1b[1;37;41m LIVE-TX \x1b[0;1;37;44m"
    };
    let cq_badge = if st.auto_cq {
        "\x1b[1;30;42m AUTO-CQ \x1b[0;1;37;44m"
    } else {
        ""
    };
    let header = format!(
        " RAD950 HackRF  |  {} [{}]  |  {:.3} MHz ({})  |  Subch {} ({} Hz)  |  {} ",
        st.cfg.call_str,
        st.cfg.grid_str,
        st.cfg.dial_hz as f64 / 1_000_000.0,
        band_name(st.cfg.dial_hz),
        st.cfg.subch,
        SUBCHANNELS_HZ[st.cfg.subch],
        format_utc_hms(now_ms / 1000)
    );
    buf.push_str(&format!("\x1b[1;37;44m{header}{tx_badge}{cq_badge}\x1b[K\x1b[0m\r\n"));

    // 2. Slot progress bar + Audio Peak
    let (phase_col, phase_lbl) = match st.phase {
        SlotPhase::Waiting => ("\x1b[33m", "WAIT  "),
        SlotPhase::Rx => ("\x1b[32m", "RX    "),
        SlotPhase::Tx => ("\x1b[1;31m", "TX    "),
        SlotPhase::DryTx => ("\x1b[1;33m", "DRY-TX"),
    };
    let bar_w = 26usize;
    let filled = ((slot_elapsed_ms as usize * bar_w) / 10_000).min(bar_w);
    let bar: String = "█".repeat(filled) + &"░".repeat(bar_w - filled);
    let line2 = format!(
        " {phase_col}{phase_lbl}\x1b[0m [{phase_col}{bar}\x1b[0m] {:4.1}s/10s  Peak:{:3.0}%  Grids:{}  Q:{}",
        slot_elapsed_ms as f64 / 1000.0,
        st.peak_pct,
        st.worked_grids_all.len(),
        st.tx_queue.len()
    );
    buf.push_str(&format!("{line2}\x1b[K\r\n"));

    // 3. View tabs + HackRF info
    let tab = |v: View, name: &str| {
        if tui.view == v {
            format!("\x1b[1;30;46m {name} \x1b[0m")
        } else {
            format!("\x1b[2m {name} \x1b[0m")
        }
    };
    buf.push_str(&format!(
        " {} {} {}   \x1b[2m{} | RX:{} TX:{} Dec:{} QSO:{}\x1b[0m\x1b[K\r\n",
        tab(View::Operate, "1:Operate"),
        tab(View::Settings, "2:Settings"),
        tab(View::Log, "3:QSO Log"),
        trunc(&st.hackrf_banner, cols.saturating_sub(58)),
        st.slots_rx,
        st.slots_tx,
        st.frames_rx,
        st.qso_log.len()
    ));
    buf.push_str(&format!("\x1b[2m{}\x1b[0m\r\n", "─".repeat(cols)));

    let body_rows = rows.saturating_sub(7);

    match tui.view {
        View::Operate => {
            // Radio-style 2D True-Color Panafall (24 frequency bins rendered via Unicode half-blocks `▀`,
            // high freq at top, newest column at right, matching RT-950 Pro WF_PALETTE & TX_PALETTE).
            let wf_term_rows: usize = if body_rows >= 22 { 12 } else { 6 };
            let wf_cols = cols.saturating_sub(8).min(WF_HISTORY_MAX);
            let remaining = body_rows.saturating_sub(wf_term_rows + 1);
            let heard_rows = (remaining * 3 / 5).max(4);
            let event_rows = remaining.saturating_sub(heard_rows + 1);

            buf.push_str(&format!(
                "\x1b[1;36m PANAFALL (600–2400 Hz)\x1b[0m  \x1b[2m{}\x1b[0m\x1b[K\r\n",
                trunc(&st.phase_note, cols.saturating_sub(28))
            ));
            let empty_col = WfCol::default();
            let hist_len = st.wf_hist.len();
            let wf_pixel_color = |col: &WfCol, r_top: usize| -> (u8, u8, u8) {
                let bin = 23usize.saturating_sub(r_top);
                let lvl = col.bins[bin] as usize;
                let is_grid_line = r_top.is_multiple_of(3) && r_top > 0;
                let mut px = if col.is_tx {
                    TX_PALETTE[lvl & 15]
                } else {
                    WF_PALETTE[lvl & 15]
                };
                if col.slot_mark && lvl < 4 {
                    px = if col.is_tx { 0x6000 } else { 0x2945 };
                } else if is_grid_line && lvl < 2 {
                    px = if col.is_tx { 0x2800 } else { 0x1082 };
                }
                rgb565_to_rgb888(px)
            };

            for tr in 0..wf_term_rows {
                let r_hi = if wf_term_rows == 12 { tr * 2 } else { tr * 4 };
                let r_lo = if wf_term_rows == 12 { tr * 2 + 1 } else { tr * 4 + 2 };
                let lbl = if tr == 0 {
                    "2.4k "
                } else if tr == wf_term_rows / 2 {
                    "1.5k "
                } else if tr + 1 == wf_term_rows {
                    "0.6k "
                } else {
                    "     "
                };
                buf.push_str("\x1b[2m ");
                buf.push_str(lbl);
                buf.push_str("\x1b[0m");
                for x in 0..wf_cols {
                    let col = if x + hist_len < wf_cols {
                        &empty_col
                    } else {
                        &st.wf_hist[x + hist_len - wf_cols]
                    };
                    let (fr, fg, fb) = wf_pixel_color(col, r_hi);
                    let (br, bg, bb) = wf_pixel_color(col, r_lo);
                    buf.push_str(&format!(
                        "\x1b[38;2;{fr};{fg};{fb};48;2;{br};{bg};{bb}m▀"
                    ));
                }
                buf.push_str("\x1b[0m\x1b[K\r\n");
            }

            // Heard Stations with exact radio icons & color coding (★ NEW, + BND, ✓ WKD★/WKD+/WKD)
            let cur_band = band_name(st.cfg.dial_hz).to_string();
            buf.push_str(&format!(
                "\x1b[1;36m HEARD ({})\x1b[0m  \x1b[1;33m★NEW\x1b[0m \x1b[1;36m+BND\x1b[0m \x1b[1;32m✓WKD\x1b[0m  \x1b[2m↑/↓ select, Enter=Answer\x1b[0m\x1b[K\r\n",
                st.heard.len()
            ));
            if st.heard.is_empty() {
                buf.push_str("   \x1b[2m(no stations decoded yet — listening on 10 s UTC slots)\x1b[0m\x1b[K\r\n");
                for _ in 1..heard_rows.saturating_sub(1) {
                    buf.push_str("\x1b[K\r\n");
                }
            } else {
                let max_show = heard_rows.saturating_sub(1);
                for i in 0..max_show {
                    if let Some(h) = st.heard.get(i) {
                        let age_s = cur_slot.saturating_sub(h.last_slot) * 10;
                        let sel = i == tui.heard_sel;
                        let has_grid = h.grid_str.len() == 4 && h.grid_str != "----";
                        let is_new_all = has_grid && !st.worked_grids_all.contains(&h.grid_str);
                        let is_new_band = has_grid
                            && !is_new_all
                            && !st
                                .worked_grids_band
                                .contains(&(cur_band.clone(), h.grid_str.clone()));

                        let (icon, tag, col_ansi) = match h.worked {
                            WorkedBadge::WorkedNewGrid => ("✓", "WKD★", "\x1b[1;32m"),
                            WorkedBadge::WorkedNewBand => ("✓", "WKD+", "\x1b[1;32m"),
                            WorkedBadge::Worked => ("✓", "WKD ", "\x1b[32m"),
                            WorkedBadge::None if is_new_all => ("★", "NEW ", "\x1b[1;33m"),
                            WorkedBadge::None if is_new_band => ("+", "BND ", "\x1b[1;36m"),
                            WorkedBadge::None => ("□", "    ", "\x1b[0m"),
                        };
                        let row = format!(
                            " {:>2}. {} {:<8} {:<4}  {:+3} dB  {:4} Hz  {:<4}  {}  {:>4}s",
                            i + 1,
                            icon,
                            h.call_str,
                            h.grid_str,
                            h.snr_db,
                            h.tone_hz,
                            h.kind,
                            tag,
                            age_s
                        );
                        if sel {
                            buf.push_str(&format!("\x1b[1;30;47m{}\x1b[K\x1b[0m\r\n", trunc(&row, cols)));
                        } else {
                            buf.push_str(&format!("{col_ansi}{}\x1b[0m\x1b[K\r\n", trunc(&row, cols)));
                        }
                    } else {
                        buf.push_str("\x1b[K\r\n");
                    }
                }
            }

            if event_rows > 0 {
                buf.push_str("\x1b[1;36m ACTIVITY FEED\x1b[0m\x1b[K\r\n");
                let ev_len = st.events.len();
                let start = ev_len.saturating_sub(event_rows);
                for i in 0..event_rows {
                    if let Some(ev) = st.events.get(start + i) {
                        buf.push_str(&format!(" {}\x1b[K\r\n", trunc(ev, cols.saturating_sub(2))));
                    } else {
                        buf.push_str("\x1b[K\r\n");
                    }
                }
            }
        }
        View::Settings => {
            buf.push_str("\x1b[1;36m SETTINGS — ↑/↓ move, ←/→ adjust, Enter edit/toggle, Esc back\x1b[0m\x1b[K\r\n");
            let items: [(&str, String, &str); SETTINGS_COUNT] = [
                ("Callsign", st.cfg.call_str.clone(), "Enter to edit"),
                ("Grid Square", st.cfg.grid_str.clone(), "Enter to edit (4-char or NONE)"),
                (
                    "Band Preset",
                    match band_index(st.cfg.dial_hz) {
                        Some(bi) => format!("{} ({:.3} MHz)", BANDS[bi].0, BANDS[bi].1 as f64 / 1e6),
                        None => format!("Custom ({:.3} MHz)", st.cfg.dial_hz as f64 / 1e6),
                    },
                    "←/→ cycle 10m/12m/15m/17m",
                ),
                (
                    "Dial Freq (Hz)",
                    format!("{} ({:.3} MHz)", st.cfg.dial_hz, st.cfg.dial_hz as f64 / 1e6),
                    "Enter to type exact Hz, ←/→ ±1 kHz",
                ),
                (
                    "TX Subchannel",
                    format!("{} ({} Hz)", st.cfg.subch, SUBCHANNELS_HZ[st.cfg.subch]),
                    "←/→ cycle 0..7",
                ),
                ("RX LNA Gain", format!("{} dB", st.cfg.lna_db), "←/→ step 8 dB (0..40)"),
                ("RX VGA Gain", format!("{} dB", st.cfg.vga_db), "←/→ step 2 dB (0..62)"),
                ("TX VGA Gain", format!("{} dB", st.cfg.tx_vga_db), "←/→ step 1 dB (0..47)"),
                (
                    "RF Amplifier",
                    if st.cfg.amp { "ON (+14 dB)".into() } else { "OFF".into() },
                    "←/→ or Enter to toggle",
                ),
                (
                    "Dry TX Safety",
                    if st.cfg.dry_tx {
                        "ON (RF suppressed)".into()
                    } else {
                        "OFF (LIVE RF TX!)".into()
                    },
                    "←/→ or Enter to toggle",
                ),
                (
                    "Auto-QSO Reply",
                    if st.auto_qso { "ON".into() } else { "OFF".into() },
                    "Auto-send R+RX & log when answered",
                ),
                (
                    "Auto-CQ Parity",
                    if st.cq_parity == 0 { "Even slots".into() } else { "Odd slots".into() },
                    "←/→ toggle even/odd 10s slot",
                ),
            ];

            for i in 0..body_rows.saturating_sub(1) {
                if let Some((name, val, hint)) = items.get(i) {
                    let sel = i == tui.set_sel;
                    let shown_val = if sel {
                        if let Some(ref ed) = tui.editing {
                            format!("[{ed}_]")
                        } else {
                            val.clone()
                        }
                    } else {
                        val.clone()
                    };
                    let row = format!("  {:<16}  {:<24}  {}", name, shown_val, hint);
                    if sel {
                        buf.push_str(&format!("\x1b[1;30;47m{}\x1b[K\x1b[0m\r\n", trunc(&row, cols)));
                    } else {
                        buf.push_str(&format!("{}\x1b[K\r\n", trunc(&row, cols)));
                    }
                } else {
                    buf.push_str("\x1b[K\r\n");
                }
            }
        }
        View::Log => {
            buf.push_str(&format!(
                "\x1b[1;36m QSO LOG ({}) — file: {}\x1b[0m\x1b[K\r\n",
                st.qso_log.len(),
                st.cfg.log_path
            ));
            let max_show = body_rows.saturating_sub(1);
            if st.qso_log.is_empty() {
                buf.push_str("   \x1b[2m(no completed QSOs yet this session)\x1b[0m\x1b[K\r\n");
                for _ in 1..max_show {
                    buf.push_str("\x1b[K\r\n");
                }
            } else {
                let start = st.qso_log.len().saturating_sub(max_show);
                for i in 0..max_show {
                    if let Some(q) = st.qso_log.get(start + i) {
                        let row = format!(
                            " {:>3}. {}  {:.3} MHz  {:<8}  {:<4}  rx={:+3}dB  tx={:+3}dB",
                            start + i + 1,
                            format_utc_hms(q.utc_sec),
                            q.dial_hz as f64 / 1_000_000.0,
                            q.peer,
                            q.grid,
                            q.rx_snr,
                            q.tx_snr
                        );
                        buf.push_str(&format!("{}\x1b[K\r\n", trunc(&row, cols)));
                    } else {
                        buf.push_str("\x1b[K\r\n");
                    }
                }
            }
        }
    }

    // Footer status/toast + key hints
    buf.push_str(&format!("\x1b[2m{}\x1b[0m\r\n", "─".repeat(cols)));
    if let Some((ref msg, exp)) = tui.toast {
        if now_ms < exp {
            buf.push_str(&format!("\x1b[1;33m {msg}\x1b[0m\x1b[K\r\n"));
        } else {
            buf.push_str("\x1b[K\r\n");
        }
    } else {
        buf.push_str("\x1b[K\r\n");
    }
    let hints = match tui.view {
        View::Operate => " [c] CQ  [a] Auto-CQ  [Enter] Answer  [b] Band  [d] Dry-TX  [s] Settings  [l] Log  [q] Quit",
        View::Settings => " [↑/↓] Select  [←/→] Change  [Enter] Edit/Toggle  [Esc/1] Operate  [q] Quit",
        View::Log => " [1/Esc] Operate  [2/s] Settings  [q] Quit",
    };
    buf.push_str(&format!("\x1b[7m{}\x1b[K\x1b[0m", trunc(hints, cols)));

    let mut out = io::stdout();
    let _ = out.write_all(buf.as_bytes());
    let _ = out.flush();
}

fn adjust_setting(state: &Arc<Mutex<SharedState>>, tui: &mut TuiState, delta: i32) {
    let mut st = state.lock().unwrap();
    match tui.set_sel {
        2 => {
            let cur = band_index(st.cfg.dial_hz).unwrap_or(0) as i32;
            let next = (cur + delta).rem_euclid(BANDS.len() as i32) as usize;
            st.cfg.dial_hz = BANDS[next].1;
            tui.set_toast(format!("Band -> {} ({:.3} MHz)", BANDS[next].0, BANDS[next].1 as f64 / 1e6));
        }
        3 => {
            let f = (st.cfg.dial_hz as i64 + delta as i64 * 1_000).clamp(1_000_000, 6_000_000_000) as u64;
            st.cfg.dial_hz = f;
        }
        4 => {
            let next = (st.cfg.subch as i32 + delta).rem_euclid(SUBCHANNELS_HZ.len() as i32) as usize;
            st.cfg.subch = next;
        }
        5 => {
            let next = (st.cfg.lna_db as i32 + delta * 8).clamp(0, 40) as u8;
            st.cfg.lna_db = (next / 8) * 8;
        }
        6 => {
            let next = (st.cfg.vga_db as i32 + delta * 2).clamp(0, 62) as u8;
            st.cfg.vga_db = (next / 2) * 2;
        }
        7 => {
            st.cfg.tx_vga_db = (st.cfg.tx_vga_db as i32 + delta).clamp(0, 47) as u8;
        }
        8 => st.cfg.amp = !st.cfg.amp,
        9 => {
            st.cfg.dry_tx = !st.cfg.dry_tx;
            tui.set_toast(if st.cfg.dry_tx {
                "Dry TX ON (RF suppressed)"
            } else {
                "Dry TX OFF — LIVE RF TRANSMIT ENABLED"
            });
        }
        10 => st.auto_qso = !st.auto_qso,
        11 => st.cq_parity = 1 - st.cq_parity,
        _ => {}
    }
}

fn commit_edit(state: &Arc<Mutex<SharedState>>, tui: &mut TuiState, text: String) {
    let mut st = state.lock().unwrap();
    match tui.set_sel {
        0 => match parse_call(&text) {
            Ok(c) => {
                st.cfg.call = c;
                st.cfg.call_str = format_call(c);
                tui.set_toast(format!("Callsign set to {}", st.cfg.call_str));
            }
            Err(e) => tui.set_toast(e),
        },
        1 => match parse_grid(&text) {
            Ok((g, s)) => {
                st.cfg.grid4 = g;
                st.cfg.grid_str = s.clone();
                tui.set_toast(format!("Grid set to {s}"));
            }
            Err(e) => tui.set_toast(e),
        },
        3 => match text.trim().parse::<u64>() {
            Ok(f) if (1_000_000..=6_000_000_000).contains(&f) => {
                st.cfg.dial_hz = f;
                tui.set_toast(format!("Dial set to {:.3} MHz", f as f64 / 1e6));
            }
            _ => tui.set_toast("Invalid frequency (enter Hz, e.g. 28078000)"),
        },
        _ => {}
    }
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

    let hackrf_banner = match check_hackrf_installed(serial.as_deref()) {
        Ok(info) => info,
        Err(e) => {
            eprintln!("{e}");
            eprintln!("Tip: run with --loopback to test the TX/RX DSP chain without hardware.");
            return ExitCode::from(1);
        }
    };

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

    let (worked_calls, worked_grids_all, worked_grids_band) = load_existing_log(&cfg.log_path);

    let state = Arc::new(Mutex::new(SharedState {
        cfg,
        running: true,
        auto_cq: false,
        auto_qso: true,
        cq_parity: 0,
        tx_queue: VecDeque::new(),
        heard: Vec::new(),
        qso_log: Vec::new(),
        worked_calls,
        worked_grids_all,
        worked_grids_band,
        events: VecDeque::new(),
        phase: SlotPhase::Waiting,
        phase_note: "Starting RAD950 HackRF node...".to_string(),
        peak_pct: 0.0,
        wf_hist: VecDeque::with_capacity(WF_HISTORY_MAX),
        hackrf_banner,
        slots_rx: 0,
        slots_tx: 0,
        frames_rx: 0,
    }));

    let _raw = match RawTerm::enter() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("failed to enter raw terminal mode: {e}");
            return ExitCode::from(1);
        }
    };

    let worker_state = Arc::clone(&state);
    let worker = thread::spawn(move || slot_worker(worker_state));

    let mut tui = TuiState {
        view: View::Operate,
        heard_sel: 0,
        set_sel: 0,
        editing: None,
        toast: Some(("Connected to HackRF One — press 's' for Settings, 'c' for CQ".into(), utc_now_ms() + 4_000)),
    };

    loop {
        render_tui(&state, &tui);

        if let Some(key) = read_key() {
            // Inline text editing inside Settings
            if let Some(mut ed) = tui.editing.take() {
                match key {
                    Key::Enter => commit_edit(&state, &mut tui, ed),
                    Key::Esc => tui.set_toast("Edit cancelled"),
                    Key::Backspace => {
                        ed.pop();
                        tui.editing = Some(ed);
                    }
                    Key::Char(c) if ed.len() < 12 => {
                        ed.push(c.to_ascii_uppercase());
                        tui.editing = Some(ed);
                    }
                    Key::CtrlC => {
                        state.lock().unwrap().running = false;
                        break;
                    }
                    _ => tui.editing = Some(ed),
                }
                continue;
            }

            match key {
                Key::CtrlC | Key::Char('q') => {
                    state.lock().unwrap().running = false;
                    break;
                }
                Key::Tab => {
                    tui.view = match tui.view {
                        View::Operate => View::Settings,
                        View::Settings => View::Log,
                        View::Log => View::Operate,
                    };
                }
                Key::Char('1') => tui.view = View::Operate,
                Key::Char('2') | Key::Char('s') => tui.view = View::Settings,
                Key::Char('3') | Key::Char('l') => tui.view = View::Log,
                Key::Esc => tui.view = View::Operate,
                _ => match tui.view {
                    View::Operate => match key {
                        Key::Up => tui.heard_sel = tui.heard_sel.saturating_sub(1),
                        Key::Down => {
                            let len = state.lock().unwrap().heard.len();
                            if len > 0 && tui.heard_sel + 1 < len {
                                tui.heard_sel += 1;
                            }
                        }
                        Key::Enter => {
                            let mut st = state.lock().unwrap();
                            if let Some(h) = st.heard.get(tui.heard_sel).cloned() {
                                let frame = Frame::Rx {
                                    sender: st.cfg.call,
                                    recipient: h.call,
                                    snr_db: h.snr_db,
                                    ack: false,
                                    reserved: false,
                                };
                                let subch = st.cfg.subch;
                                let summary = format!(
                                    "{} de {} RX {:+}dB",
                                    h.call_str, st.cfg.call_str, h.snr_db
                                );
                                st.tx_queue.push_front(TxJob {
                                    frame,
                                    subch,
                                    summary: summary.clone(),
                                });
                                tui.set_toast(format!("Queued answer: {summary}"));
                            } else {
                                tui.set_toast("No heard station selected");
                            }
                        }
                        Key::Char('c') => {
                            let mut st = state.lock().unwrap();
                            if st.tx_queue.iter().any(|j| matches!(j.frame, Frame::Cq { .. })) {
                                tui.set_toast("CQ already queued for next TX slot");
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
                                tui.set_toast(format!("Queued CQ for next TX slot: {summary}"));
                            }
                        }
                        Key::Char('a') => {
                            let mut st = state.lock().unwrap();
                            st.auto_cq = !st.auto_cq;
                            if st.auto_cq {
                                st.cq_parity = (utc_now_ms() / 10_000 + 1) % 2;
                                tui.set_toast(format!(
                                    "Auto-CQ ON ({} slots)",
                                    if st.cq_parity == 0 { "even" } else { "odd" }
                                ));
                            } else {
                                st.tx_queue.clear();
                                tui.set_toast("Auto-CQ OFF");
                            }
                        }
                        Key::Char('b') => {
                            let mut st = state.lock().unwrap();
                            let next = (band_index(st.cfg.dial_hz).map_or(0, |i| i + 1)) % BANDS.len();
                            st.cfg.dial_hz = BANDS[next].1;
                            tui.set_toast(format!(
                                "Band -> {} ({:.3} MHz)",
                                BANDS[next].0,
                                BANDS[next].1 as f64 / 1e6
                            ));
                        }
                        Key::Char('d') => {
                            let mut st = state.lock().unwrap();
                            st.cfg.dry_tx = !st.cfg.dry_tx;
                            tui.set_toast(if st.cfg.dry_tx {
                                "Dry TX ON (RF suppressed)"
                            } else {
                                "Dry TX OFF — LIVE RF ENABLED"
                            });
                        }
                        _ => {}
                    },
                    View::Settings => match key {
                        Key::Up => tui.set_sel = tui.set_sel.saturating_sub(1),
                        Key::Down => {
                            if tui.set_sel + 1 < SETTINGS_COUNT {
                                tui.set_sel += 1;
                            }
                        }
                        Key::Left => adjust_setting(&state, &mut tui, -1),
                        Key::Right => adjust_setting(&state, &mut tui, 1),
                        Key::Enter => {
                            let st = state.lock().unwrap();
                            match tui.set_sel {
                                0 => tui.editing = Some(st.cfg.call_str.clone()),
                                1 => tui.editing = Some(st.cfg.grid_str.clone()),
                                3 => tui.editing = Some(st.cfg.dial_hz.to_string()),
                                _ => {
                                    drop(st);
                                    adjust_setting(&state, &mut tui, 1);
                                }
                            }
                        }
                        _ => {}
                    },
                    View::Log => {}
                },
            }
        }
    }

    drop(_raw);
    let _ = worker.join();
    ExitCode::SUCCESS
}
