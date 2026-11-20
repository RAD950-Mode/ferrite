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
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Optional slot phase offset in ms (`--slot-offset-ms`), used indoors to align with a radio lacking GPS UTC.
static SLOT_OFFSET_MS: AtomicI64 = AtomicI64::new(0);

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

/// RX capture duration inside each 10.0 s UTC slot (covers +0.00s..+9.45s, capturing the full +0.40s..+9.20s burst).
const RX_SLOT_MS: u64 = 9_050;
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
enum AutoAnswer {
    Off,
    NewGrid,
    NewBandGrid,
    AnyNeeded,
}

impl AutoAnswer {
    fn next(self) -> Self {
        match self {
            Self::Off => Self::NewGrid,
            Self::NewGrid => Self::NewBandGrid,
            Self::NewBandGrid => Self::AnyNeeded,
            Self::AnyNeeded => Self::Off,
        }
    }

    fn badge(self) -> &'static str {
        match self {
            Self::Off => "",
            Self::NewGrid => " HUNT:NEW ",
            Self::NewBandGrid => " HUNT:BND ",
            Self::AnyNeeded => " HUNT:ALL ",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::NewGrid => "New Grid only",
            Self::NewBandGrid => "New Grid or Band+Grid",
            Self::AnyNeeded => "Any Needed (new call/grid)",
        }
    }

    fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "off" | "none" => Ok(Self::Off),
            "new" | "new-grid" | "grid" => Ok(Self::NewGrid),
            "band" | "new-band" | "new-band-grid" => Ok(Self::NewBandGrid),
            "any" | "all" | "needed" => Ok(Self::AnyNeeded),
            _ => Err(format!("invalid --auto-answer '{s}' (use off, new-grid, new-band, or any)")),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Operate,
    Settings,
    Log,
    Calibrate,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotPhase {
    Waiting,
    Rx,
    Tx,
    DryTx,
    Calibrating,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CalMode {
    Auto,
    WwvOnly,
    AmBcb,
    Atsc,
    Lte,
    Custom(u64),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CalKind {
    Atomic,
    LteGps,
    AtscPilot,
    Custom,
    AmBcb,
}

#[derive(Clone)]
struct CalHit {
    label: String,
    nominal_hz: u64,
    offset_hz: f64,
    ppm: f64,
    snr_db: f64,
    kind: CalKind,
    birdie: bool,
    peak_pct: f32,
}

#[derive(Clone)]
struct NodeConfig {
    call: Callsign,
    suffix: u8,
    call_str: String,
    grid4: u16,
    grid_str: String,
    dial_hz: u64,
    ppm: f64,
    subch: usize,
    lna_db: u8,
    vga_db: u8,
    tx_vga_db: u8,
    amp: bool,
    dry_tx: bool,
    serial: Option<String>,
    log_path: String,
}

fn ppm_offset_hz(dial_hz: u64, ppm: f64) -> i64 {
    (dial_hz as f64 * ppm / 1_000_000.0).round() as i64
}

fn tuned_center_hz(cfg: &NodeConfig) -> u64 {
    let cal_hz = ppm_offset_hz(cfg.dial_hz, cfg.ppm);
    (cfg.dial_hz as i64 + cal_hz - RF_OFFSET_HZ).max(1_000_000) as u64
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
    /// Optional required slot parity (`0` = even, `1` = odd) so replies always fire on the
    /// opposite parity from the slot we heard the peer transmit in (preventing Auto-CQ collisions).
    parity: Option<u64>,
}

struct SharedState {
    cfg: NodeConfig,
    running: bool,
    auto_cq: bool,
    auto_qso: bool,
    auto_answer: AutoAnswer,
    rendezvous: bool,
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
    cal_request: Option<CalMode>,
    cal_running: bool,
    cal_status: String,
    cal_hits: Vec<CalHit>,
    cal_recommended: Option<(f64, String)>,
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
    eprintln!("  --call <CALL[-1..15]>     Station callsign with optional numeric suffix (default: NQ6N)");
    eprintln!("  --grid <GRID4|NONE>       4-char Maidenhead grid (default: FN31)");
    eprintln!("  --band <10m|12m|15m|17m>  RAD950 calling band (default: 10m = 28.078 MHz)");
    eprintln!("  --freq <HZ>               Custom USB dial frequency in Hz (e.g. 28185000)");
    eprintln!("  --ppm <FLOAT>             HackRF crystal frequency calibration in ppm (default: 0.0)");
    eprintln!("  --subch <0..7>            TX audio subchannel index (default: 0 = 600 Hz)");
    eprintln!("  --lna <0..40>             HackRF RX LNA gain in dB, 8 dB steps (default: 16)");
    eprintln!("  --vga <0..62>             HackRF RX VGA gain in dB, 2 dB steps (default: 20)");
    eprintln!("  --tx-gain <0..47>         HackRF TX VGA gain in dB (default: 16)");
    eprintln!("  --amp                     Enable HackRF RF amplifier (default: off)");
    eprintln!("  --serial <HEX>            HackRF serial number suffix (if multiple connected)");
    eprintln!("  --dry-tx                  Dry TX safety mode (synthesize without keying HackRF)");
    eprintln!("  --rendezvous              Enable hourly UTC Rendezvous band-hopping & 50/50 turn cadence");
    eprintln!("  --auto-answer <MODE>      Attended Hunter auto-answer: off, new-grid, new-band, or any");
    eprintln!("  --tx-once                 Transmit a single CQ immediately and exit (for bench/OTA tests)");
    eprintln!("  --log <PATH>              QSO log file path (default: rad950_hackrf.log)");
    eprintln!("  --slot-offset-ms <MS>     Shift 10 s slot clock by signed ms (for indoor non-GPS bench sync)");
    eprintln!("  --calibrate               Auto self-calibrate ppm (LTE GPS -> ATSC TV -> WWV/CHU -> AM BCB)");
    eprintln!("  --cal-lte                 Scan US LTE FDD downlink PSS (GPS-disciplined ±0.05 ppm) and exit");
    eprintln!("  --cal-atsc                Scan UHF ATSC 1.0 TV pilot carriers (ch 14-36) and exit");
    eprintln!("  --cal-wwv                 Probe WWV/CHU atomic HF carriers only and exit");
    eprintln!("  --cal-am                  Scan AM broadcast band (540-1700 kHz, 10 kHz grid) and exit");
    eprintln!("  --cal-freq <HZ>           Probe a single known carrier frequency in Hz and exit");
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

fn parse_call_with_suffix(s: &str) -> Result<(Callsign, u8, String), String> {
    let up = s.trim().to_ascii_uppercase();
    let (base_s, suffix) = if let Some((b, suf)) = up.split_once('-') {
        let n: u8 = suf
            .parse()
            .map_err(|_| format!("invalid numeric suffix in '{s}' (use -1..-15)"))?;
        if !(1..=15).contains(&n) {
            return Err(format!("suffix must be 1..=15 in '{s}'"));
        }
        (b, n)
    } else {
        (up.as_str(), 0u8)
    };
    let call = parse_call(base_s)?;
    let disp = format_call_with_suffix(call, suffix);
    Ok((call, suffix, disp))
}

fn format_call_with_suffix(c: Callsign, suffix: u8) -> String {
    let base = format_call(c);
    if (1..=15).contains(&suffix) {
        format!("{base}-{suffix}")
    } else {
        base
    }
}

/// Published hourly UTC Rendezvous schedule matching `crates/ferrite-app/src/rad950_rdv.rs`:
/// `:00-:04` 10m (28.078), `:15-:19` 12m (24.919), `:30-:34` 15m (21.078), `:45-:49` 17m (18.104).
fn rdv_active_band(utc_sec: u64) -> Option<(&'static str, u64)> {
    const RDV: [(&str, u64, u64); 4] = [
        ("10m", 0, 28_078_000),
        ("12m", 15, 24_919_000),
        ("15m", 30, 21_078_000),
        ("17m", 45, 18_104_000),
    ];
    let sec_in_hour = utc_sec % 3600;
    for &(lbl, start_min, dial) in &RDV {
        let start = start_min * 60;
        if sec_in_hour >= start && sec_in_hour < start + 300 {
            return Some((lbl, dial));
        }
    }
    None
}

/// Fast avalanche hash matching `rad950_rdv::mix32` for independent 50/50 even-slot Rendezvous CQs.
fn rdv_plan_cq(call: Callsign, suffix: u8, slot: u64) -> Option<usize> {
    if !slot.is_multiple_of(2) {
        return None;
    }
    let raw = call.raw() ^ ((suffix as u32) << 24);
    let mut seed = raw.wrapping_mul(0x9E37_79B1) ^ 0x5A3C_9E37;
    if seed == 0 {
        seed = 0x1357_9BDF;
    }
    let mut x = seed ^ (slot as u32).wrapping_mul(0x9E37_79B9);
    x ^= x >> 16;
    x = x.wrapping_mul(0x7FEB_352D);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846C_A68B);
    x ^= x >> 16;
    if (x & 1) == 0 {
        return None;
    }
    Some(((x >> 8) as usize) % SUBCHANNELS_HZ.len())
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
    let raw = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i128;
    let off = SLOT_OFFSET_MS.load(Ordering::Relaxed) as i128;
    (raw + off).max(0) as u64
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

    let center_hz = tuned_center_hz(cfg);
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
    let center_hz = tuned_center_hz(cfg);
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
        if cfg.amp && cfg.lna_db >= 32 { "1" } else { "0" },
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
        let abort = {
            let st = state.lock().unwrap();
            !st.running || st.cal_request.is_some()
        };
        if abort {
            drop(stdout);
            let _ = child.kill();
            let _ = child.wait();
            return Ok((Vec::new(), 0.0));
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
    let my_suffix = st.cfg.suffix;
    let my_subch = st.cfg.subch;
    let dial_hz = st.cfg.dial_hz;
    let utc_sec = slot * 10;

    for d in frames {
        st.frames_rx += 1;
        let sender_call = frame_sender(d.frame);
        let cq_suffix = match d.frame {
            Frame::Cq { reserved, .. } => (reserved & 0x0F) as u8,
            _ => 0,
        };
        let sender_str = sender_call
            .map(|c| format_call_with_suffix(c, cq_suffix))
            .unwrap_or_else(|| "EXT".to_string());
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
            if sc == my_call && cq_suffix == my_suffix {
                continue;
            }
            let already_worked = if st.worked_calls.contains(&sender_str) {
                WorkedBadge::Worked
            } else {
                WorkedBadge::None
            };
            if let Some(existing) = st.heard.iter_mut().find(|h| h.call_str == sender_str) {
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
                        parity: Some((slot + 1) % 2),
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

    // Attended Auto-Answer (Hunter / Pounce): pick the highest-priority needed CQ in this slot
    // (New Grid > New Band+Grid > New Call, then strongest SNR) and queue an opposite-parity reply.
    if st.auto_answer != AutoAnswer::Off && st.tx_queue.is_empty() {
        let cur_band = band_name(dial_hz).to_string();
        let mut best: Option<(u8, i8, Callsign, String, &'static str)> = None;
        for d in frames {
            if let Frame::Cq { sender, grid4, .. } = d.frame {
                if sender == my_call {
                    continue;
                }
                let sender_str = format_call(sender);
                let g = format_grid(grid4);
                let valid_grid = g.len() == 4 && g != "----";
                let is_new_all = valid_grid && !st.worked_grids_all.contains(&g);
                let is_new_band = valid_grid && !is_new_all && !st.worked_grids_band.contains(&(cur_band.clone(), g.clone()));
                let is_new_call = !st.worked_calls.contains(&sender_str);
                let (prio, reason) = if is_new_all {
                    (3u8, "NEW GRID")
                } else if is_new_band {
                    (2u8, "NEW BAND+GRID")
                } else if is_new_call {
                    (1u8, "NEW CALL")
                } else {
                    (0u8, "")
                };
                let allowed = match st.auto_answer {
                    AutoAnswer::Off => false,
                    AutoAnswer::NewGrid => prio >= 3,
                    AutoAnswer::NewBandGrid => prio >= 2,
                    AutoAnswer::AnyNeeded => prio >= 1,
                };
                if allowed && best.as_ref().is_none_or(|b| (prio, d.snr_db) > (b.0, b.1)) {
                    best = Some((prio, d.snr_db, sender, sender_str, reason));
                }
            }
        }
        if let Some((_, snr_db, target, target_str, reason)) = best {
            let reply = Frame::Rx {
                sender: my_call,
                recipient: target,
                snr_db,
                ack: false,
                reserved: false,
            };
            let target_parity = (slot + 1) % 2;
            let summary = format!("{target_str} de {} RX {:+}dB", st.cfg.call_str, snr_db);
            st.tx_queue.push_front(TxJob {
                frame: reply,
                subch: my_subch,
                summary: summary.clone(),
                parity: Some(target_parity),
            });
            st.push_event(format!(
                "[{}] HUNT auto-answer {target_str} ({reason}, {} slot): {summary}",
                format_utc_hms(utc_sec),
                if target_parity == 0 { "even" } else { "odd" }
            ));
        }
    }
}

/// Atomic HF time-standard reference carriers (WWV/WWVH + CHU).
const WWV_REFS: &[(&str, u64)] = &[
    ("WWV 10.000 MHz", 10_000_000),
    ("WWV 15.000 MHz", 15_000_000),
    ("WWV  5.000 MHz", 5_000_000),
    ("WWV 20.000 MHz", 20_000_000),
    ("CHU  7.850 MHz", 7_850_000),
    ("CHU 14.670 MHz", 14_670_000),
];

/// In-place radix-2 Cooley-Tukey complex FFT (`re.len() == im.len()`, power of two).
fn fft_complex(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2usize;
    while len <= n {
        let ang = -2.0 * std::f64::consts::PI / (len as f64);
        let wlen_r = ang.cos();
        let wlen_i = ang.sin();
        let half = len >> 1;
        let mut i = 0usize;
        while i < n {
            let (mut wr, mut wi) = (1.0f64, 0.0f64);
            for k in 0..half {
                let u_r = re[i + k];
                let u_i = im[i + k];
                let v_r = re[i + k + half] * wr - im[i + k + half] * wi;
                let v_i = re[i + k + half] * wi + im[i + k + half] * wr;
                re[i + k] = u_r + v_r;
                im[i + k] = u_i + v_i;
                re[i + k + half] = u_r - v_r;
                im[i + k + half] = u_i - v_i;
                let nwr = wr * wlen_r - wi * wlen_i;
                wi = wr * wlen_i + wi * wlen_r;
                wr = nwr;
            }
            i += len;
        }
        len <<= 1;
    }
}

/// Capture `n_samples` raw `cs8` IQ samples from HackRF One at `center_hz` and sample rate `fs_hz`.
fn capture_hackrf_cs8_fs(
    serial: Option<&str>,
    center_hz: u64,
    fs_hz: u32,
    n_samples: u64,
    min_bytes: usize,
    lna_db: u8,
    vga_db: u8,
    amp: bool,
) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new("hackrf_transfer");
    cmd.args([
        "-r",
        "-",
        "-f",
        &center_hz.to_string(),
        "-s",
        &fs_hz.to_string(),
        "-n",
        &n_samples.to_string(),
        "-a",
        if amp { "1" } else { "0" },
        "-p",
        "0",
        "-l",
        &lna_db.to_string(),
        "-g",
        &vga_db.to_string(),
    ]);
    if let Some(ser) = serial {
        cmd.args(["-d", ser]);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn hackrf_transfer: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("no stdout from hackrf_transfer")?;

    let want_bytes = (n_samples as usize) * 2;
    let mut raw = Vec::with_capacity(want_bytes);
    let mut buf = [0u8; 65_536];
    let mut read_err: Option<String> = None;
    loop {
        match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let take = n.min(want_bytes.saturating_sub(raw.len()));
                raw.extend_from_slice(&buf[..take]);
                if raw.len() >= want_bytes {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                read_err = Some(format!("hackrf_transfer read error: {e}"));
                break;
            }
        }
    }
    // Always close stdout BEFORE waiting so hackrf_transfer can never deadlock on a full stdout pipe.
    drop(stdout);
    let deadline = std::time::Instant::now() + Duration::from_millis(800);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    if let Some(e) = read_err {
        return Err(e);
    }
    if raw.len() < min_bytes {
        let mut err_str = String::new();
        if let Some(mut se) = child.stderr.take() {
            let _ = se.read_to_string(&mut err_str);
        }
        return Err(format!("short HackRF capture ({} bytes): {}", raw.len(), err_str.trim()));
    }
    Ok(raw)
}

/// Capture `n_samples` raw `cs8` IQ samples from HackRF One at `center_hz` (`2.4 MS/s`).
fn capture_hackrf_cs8(
    serial: Option<&str>,
    center_hz: u64,
    n_samples: u64,
    lna_db: u8,
    vga_db: u8,
    amp: bool,
) -> Result<Vec<u8>, String> {
    capture_hackrf_cs8_fs(
        serial,
        center_hz,
        HACKRF_FS,
        n_samples,
        8192 * 400 * 2,
        lna_db,
        vga_db,
        amp,
    )
}

/// Demodulate a target carrier at `target_hz` from a `2.4 MS/s` `cs8` capture centered at `center_hz`,
/// returning `(offset_hz, snr_db, sideband_snr_db, peak_pct)`.
///
/// Sign convention: `offset_hz = measured_hz - target_hz` (positive when HackRF LO is low, matching `cfg.ppm`).
fn measure_carrier_iq(
    cs8: &[u8],
    center_hz: u64,
    target_hz: u64,
    max_ppm: f64,
) -> Option<(f64, f64, f64, f32)> {
    const DECIM: usize = 400; // 2.4 MS/s -> 6,000 Hz complex baseband (±3 kHz)
    const FFT_N: usize = 8192; // 6000 / 8192 = 0.7324 Hz/bin (~1.365 s)
    let need_samples = DECIM * FFT_N;
    if cs8.len() < need_samples * 2 {
        return None;
    }

    let shift_hz = target_hz as f64 - center_hz as f64;
    let dphi = -2.0 * std::f64::consts::PI * shift_hz / (HACKRF_FS as f64);
    let step_r = dphi.cos();
    let step_i = dphi.sin();

    let mut re = vec![0.0f64; FFT_N];
    let mut im = vec![0.0f64; FFT_N];
    let (mut lo_r, mut lo_i) = (1.0f64, 0.0f64);
    let mut max_abs = 0i32;

    for out_k in 0..FFT_N {
        let base = out_k * DECIM * 2;
        let (mut acc_r, mut acc_i) = (0.0f64, 0.0f64);
        for m in 0..DECIM {
            let si = cs8[base + 2 * m] as i8;
            let sq = cs8[base + 2 * m + 1] as i8;
            max_abs = max_abs.max((si as i32).abs()).max((sq as i32).abs());
            let xi = si as f64;
            let xq = sq as f64;
            acc_r += xi * lo_r - xq * lo_i;
            acc_i += xi * lo_i + xq * lo_r;
            let nr = lo_r * step_r - lo_i * step_i;
            lo_i = lo_r * step_i + lo_i * step_r;
            lo_r = nr;
        }
        // Renormalize oscillator every block to prevent floating-point drift.
        let mag = (lo_r * lo_r + lo_i * lo_i).sqrt();
        if mag > 0.0 {
            lo_r /= mag;
            lo_i /= mag;
        }
        let w = 0.5 * (1.0 - (2.0 * std::f64::consts::PI * out_k as f64 / (FFT_N as f64)).cos());
        re[out_k] = acc_r * w;
        im[out_k] = acc_i * w;
    }

    fft_complex(&mut re, &mut im);

    let fs_bb = HACKRF_FS as f64 / DECIM as f64; // 6000.0 Hz
    let bin_hz = fs_bb / FFT_N as f64;
    let search_hz = (target_hz as f64 * max_ppm / 1_000_000.0).clamp(150.0, 2600.0);
    let search_bins = (search_hz / bin_hz).ceil() as isize;

    let mut pwr = vec![0.0f64; FFT_N];
    for k in 0..FFT_N {
        pwr[k] = re[k] * re[k] + im[k] * im[k];
    }

    let mut best_signed = 0isize;
    let mut best_p = -1.0f64;
    for s in -search_bins..=search_bins {
        let idx = s.rem_euclid(FFT_N as isize) as usize;
        if pwr[idx] > best_p {
            best_p = pwr[idx];
            best_signed = s;
        }
    }

    // Parabolic interpolation in log-power around the peak bin for sub-0.05 Hz accuracy.
    let idx_m = (best_signed - 1).rem_euclid(FFT_N as isize) as usize;
    let idx_0 = best_signed.rem_euclid(FFT_N as isize) as usize;
    let idx_p = (best_signed + 1).rem_euclid(FFT_N as isize) as usize;
    let ym = pwr[idx_m].max(1e-12).ln();
    let y0 = pwr[idx_0].max(1e-12).ln();
    let yp = pwr[idx_p].max(1e-12).ln();
    let denom = ym - 2.0 * y0 + yp;
    let delta = if denom.abs() > 1e-12 {
        (0.5 * (ym - yp) / denom).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    let offset_hz = (best_signed as f64 + delta) * bin_hz;

    // Median noise floor across search window excluding ±40 Hz around peak.
    let excl_bins = (40.0 / bin_hz).ceil() as isize;
    let mut noise_bins = Vec::new();
    for s in -search_bins..=search_bins {
        if (s - best_signed).abs() > excl_bins {
            let idx = s.rem_euclid(FFT_N as isize) as usize;
            noise_bins.push(pwr[idx]);
        }
    }
    if noise_bins.is_empty() {
        return None;
    }
    noise_bins.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_noise = noise_bins[noise_bins.len() / 2].max(1e-12);
    let snr_db = 10.0 * (best_p / median_noise).log10();

    // Check AM/WWV audio sideband energy (±100 Hz BCD, ±500/600 Hz tones, ±1000 Hz ticks)
    // to distinguish real off-air WWV from unmodulated internal HackRF 5/10/20 MHz clock birdies.
    let mut sb_peak = 0.0f64;
    for &(f_lo, f_hi) in &[(90.0f64, 115.0), (485.0, 615.0), (985.0, 1015.0)] {
        let b_lo = (f_lo / bin_hz).floor() as isize;
        let b_hi = (f_hi / bin_hz).ceil() as isize;
        for sign in [-1isize, 1] {
            for db in b_lo..=b_hi {
                let idx = (best_signed + sign * db).rem_euclid(FFT_N as isize) as usize;
                sb_peak = sb_peak.max(pwr[idx]);
            }
        }
    }
    let sideband_snr_db = 10.0 * (sb_peak / median_noise).log10();
    let peak_pct = (max_abs as f32 / 127.0) * 100.0;
    Some((offset_hz, snr_db, sideband_snr_db, peak_pct))
}

fn probe_single_carrier(
    cfg: &NodeConfig,
    label: &str,
    target_hz: u64,
    kind: CalKind,
) -> Result<CalHit, String> {
    let center_hz = target_hz.saturating_sub(RF_OFFSET_HZ as u64).max(1_000_000);
    let n_samples: u64 = 400 * 8192 + 48_000; // ~1.39 s
    let cs8 = capture_hackrf_cs8(cfg.serial.as_deref(), center_hz, n_samples, 32, 32, cfg.amp)?;
    let (offset_hz, snr_db, sb_snr_db, peak_pct) =
        measure_carrier_iq(&cs8, center_hz, target_hz, 120.0).ok_or("carrier measurement failed")?;
    let ppm = (offset_hz / target_hz as f64) * 1_000_000.0;
    // HackRF internal 10/20 MHz clock harmonics land on 5 MHz multiples near ~0 ppm as pure unmodulated CW
    // (sidebands > 32 dB below carrier), whereas real off-air WWV carries 100/500/600/1000 Hz AM modulation.
    let is_5mhz_mult = target_hz.is_multiple_of(5_000_000);
    let birdie =
        kind == CalKind::Atomic && is_5mhz_mult && snr_db > 28.0 && ppm.abs() < 1.5 && (snr_db - sb_snr_db) > 32.0;
    Ok(CalHit {
        label: label.to_string(),
        nominal_hz: target_hz,
        offset_hz,
        ppm,
        snr_db,
        kind,
        birdie,
        peak_pct,
    })
}

/// Measure a UHF carrier (such as an ATSC 1.0 TV pilot) with a wide `±45 kHz` (`±75 ppm` at 600 MHz)
/// coarse search at 96 kHz baseband, followed by a fine 6 kHz FFT if `cs8` is long enough (~1.4 s).
fn measure_uhf_pilot_iq(
    cs8: &[u8],
    center_hz: u64,
    target_hz_f64: f64,
    max_ppm: f64,
) -> Option<(f64, f64, f32)> {
    const COARSE_DECIM: usize = 25; // 2.4 MS/s -> 96,000 Hz complex baseband (±48 kHz)
    const FFT_N: usize = 8192; // 96000 / 8192 = 11.71875 Hz/bin (~85.3 ms)
    let need_coarse = COARSE_DECIM * FFT_N;
    if cs8.len() < need_coarse * 2 {
        return None;
    }

    let shift_hz = target_hz_f64 - center_hz as f64;
    let dphi = -2.0 * std::f64::consts::PI * shift_hz / (HACKRF_FS as f64);
    let (step_r, step_i) = (dphi.cos(), dphi.sin());
    let mut re = vec![0.0f64; FFT_N];
    let mut im = vec![0.0f64; FFT_N];
    let (mut lo_r, mut lo_i) = (1.0f64, 0.0f64);
    let mut max_abs = 0i32;

    for out_k in 0..FFT_N {
        let base = out_k * COARSE_DECIM * 2;
        let (mut acc_r, mut acc_i) = (0.0f64, 0.0f64);
        for m in 0..COARSE_DECIM {
            let si = cs8[base + 2 * m] as i8;
            let sq = cs8[base + 2 * m + 1] as i8;
            max_abs = max_abs.max((si as i32).abs()).max((sq as i32).abs());
            let (xi, xq) = (si as f64, sq as f64);
            acc_r += xi * lo_r - xq * lo_i;
            acc_i += xi * lo_i + xq * lo_r;
            let nr = lo_r * step_r - lo_i * step_i;
            lo_i = lo_r * step_i + lo_i * step_r;
            lo_r = nr;
        }
        let mag = (lo_r * lo_r + lo_i * lo_i).sqrt();
        if mag > 0.0 {
            lo_r /= mag;
            lo_i /= mag;
        }
        let w = 0.5 * (1.0 - (2.0 * std::f64::consts::PI * out_k as f64 / FFT_N as f64).cos());
        re[out_k] = acc_r * w;
        im[out_k] = acc_i * w;
    }
    fft_complex(&mut re, &mut im);

    let fs_bb = HACKRF_FS as f64 / COARSE_DECIM as f64;
    let bin_hz = fs_bb / FFT_N as f64;
    let search_hz = (target_hz_f64 * max_ppm / 1_000_000.0).clamp(1_000.0, 44_000.0);
    let search_bins = (search_hz / bin_hz).ceil() as isize;

    let mut pwr = vec![0.0f64; FFT_N];
    for k in 0..FFT_N {
        pwr[k] = re[k] * re[k] + im[k] * im[k];
    }
    let mut best_s = 0isize;
    let mut best_p = -1.0f64;
    for s in -search_bins..=search_bins {
        let idx = s.rem_euclid(FFT_N as isize) as usize;
        if pwr[idx] > best_p {
            best_p = pwr[idx];
            best_s = s;
        }
    }
    let idx_m = (best_s - 1).rem_euclid(FFT_N as isize) as usize;
    let idx_0 = best_s.rem_euclid(FFT_N as isize) as usize;
    let idx_p = (best_s + 1).rem_euclid(FFT_N as isize) as usize;
    let (ym, y0, yp) = (
        pwr[idx_m].max(1e-12).ln(),
        pwr[idx_0].max(1e-12).ln(),
        pwr[idx_p].max(1e-12).ln(),
    );
    let denom = ym - 2.0 * y0 + yp;
    let delta = if denom.abs() > 1e-12 {
        (0.5 * (ym - yp) / denom).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    let coarse_offset_hz = (best_s as f64 + delta) * bin_hz;

    let excl_bins = (200.0 / bin_hz).ceil() as isize;
    let mut noise_bins = Vec::new();
    for s in -search_bins..=search_bins {
        if (s - best_s).abs() > excl_bins {
            noise_bins.push(pwr[s.rem_euclid(FFT_N as isize) as usize]);
        }
    }
    if noise_bins.is_empty() {
        return None;
    }
    noise_bins.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let snr_db = 10.0 * (best_p / noise_bins[noise_bins.len() / 2].max(1e-12)).log10();
    let peak_pct = (max_abs as f32 / 127.0) * 100.0;

    // If we have a full ~1.4 s capture, refine around `coarse_offset_hz` at 0.73 Hz/bin!
    if cs8.len() >= 400 * 8192 * 2 {
        let fine_target = (target_hz_f64 + coarse_offset_hz).round() as u64;
        if let Some((fine_err, fine_snr, _, fine_peak)) =
            measure_carrier_iq(cs8, center_hz, fine_target, 1.0)
        {
            let total_err = (fine_target as f64 - target_hz_f64) + fine_err;
            return Some((total_err, fine_snr, fine_peak));
        }
    }
    Some((coarse_offset_hz, snr_db, peak_pct))
}

/// Scan North American UHF ATSC 1.0 TV channels 14..=36 (470–608 MHz) for strong VSB pilot carriers
/// (`f_low + 309,440.559 Hz`), then run a 1.4 s precision FFT on the top 3 strongest stations.
fn scan_atsc_pilots(cfg: &NodeConfig, state: &Arc<Mutex<SharedState>>) -> Result<Vec<CalHit>, String> {
    let mut survey: Vec<(u32, f64, f64)> = Vec::new(); // (ch, pilot_hz, snr_db)
    let survey_samples: u64 = 25 * 8192 + 24_000; // ~95 ms
    for ch in 14u32..=36 {
        if !state.lock().unwrap().running {
            break;
        }
        let f_low = 470_000_000.0 + (ch - 14) as f64 * 6_000_000.0;
        let pilot_hz = f_low + 309_440.559;
        let center_hz = (pilot_hz - RF_OFFSET_HZ as f64).round() as u64;
        {
            let mut st = state.lock().unwrap();
            st.cal_status = format!("Scanning ATSC UHF ch {ch} ({:.3} MHz)...", pilot_hz / 1e6);
            st.phase_note = st.cal_status.clone();
        }
        if let Ok(cs8) = capture_hackrf_cs8_fs(
            cfg.serial.as_deref(),
            center_hz,
            HACKRF_FS,
            survey_samples,
            25 * 8192 * 2,
            24,
            24,
            false,
        ) {
            if let Some((_off, snr_db, _peak)) = measure_uhf_pilot_iq(&cs8, center_hz, pilot_hz, 70.0) {
                if snr_db >= 16.0 {
                    survey.push((ch, pilot_hz, snr_db));
                }
            }
        }
    }
    survey.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    let mut hits = Vec::new();
    let fine_samples: u64 = 400 * 8192 + 48_000; // ~1.39 s
    for &(ch, pilot_hz, _) in survey.iter().take(4) {
        if !state.lock().unwrap().running {
            break;
        }
        let center_hz = (pilot_hz - RF_OFFSET_HZ as f64).round() as u64;
        {
            let mut st = state.lock().unwrap();
            st.cal_status = format!("Measuring ATSC ch {ch} pilot ({:.3} MHz)...", pilot_hz / 1e6);
            st.phase_note = st.cal_status.clone();
        }
        if let Ok(cs8) = capture_hackrf_cs8(cfg.serial.as_deref(), center_hz, fine_samples, 24, 24, false) {
            if let Some((offset_hz, snr_db, peak_pct)) =
                measure_uhf_pilot_iq(&cs8, center_hz, pilot_hz, 70.0)
            {
                let ppm = (offset_hz / pilot_hz) * 1_000_000.0;
                hits.push(CalHit {
                    label: format!("ATSC ch {ch:<2}"),
                    nominal_hz: pilot_hz.round() as u64,
                    offset_hz,
                    ppm,
                    snr_db,
                    kind: CalKind::AtscPilot,
                    birdie: false,
                    peak_pct,
                });
            }
        }
    }
    hits.sort_by(|a, b| b.snr_db.partial_cmp(&a.snr_db).unwrap_or(std::cmp::Ordering::Equal));
    Ok(hits)
}

/// Prime US LTE FDD downlink center frequencies (GPS-disciplined to ±0.05 ppm).
const LTE_US_CENTERS: &[(&str, u64)] = &[
    ("LTE B13 751.0M", 751_000_000),  // Verizon 10 MHz (nationwide)
    ("LTE B12 739.0M", 739_000_000),  // AT&T 10 MHz
    ("LTE B12 734.0M", 734_000_000),  // T-Mobile 5/10 MHz
    ("LTE B12 742.5M", 742_500_000),  // AT&T/USCC
    ("LTE B12 743.5M", 743_500_000),  // AT&T
    ("LTE B5  880.0M", 880_000_000),  // AT&T/Verizon
    ("LTE B5  874.0M", 874_000_000),  // AT&T
    ("LTE B5  885.0M", 885_000_000),  // Verizon
    ("LTE B5  890.0M", 890_000_000),  // Verizon
    ("LTE B71 632.0M", 632_000_000),  // T-Mobile
    ("LTE B71 642.0M", 642_000_000),  // T-Mobile
    ("LTE B71 622.0M", 622_000_000),  // T-Mobile
];

/// Generate the 3 standard 128-sample time-domain LTE PSS templates (1.92 MS/s, Zadoff-Chu u = 25, 29, 34).
fn make_lte_pss_templates() -> [[(f64, f64); 128]; 3] {
    const ROOTS: [f64; 3] = [25.0, 29.0, 34.0];
    let mut out = [[(0.0, 0.0); 128]; 3];
    for (r_idx, &u) in ROOTS.iter().enumerate() {
        let mut re = [0.0f64; 128];
        let mut im = [0.0f64; 128];
        for n in 0..62usize {
            let angle = if n < 31 {
                -std::f64::consts::PI * u * (n * (n + 1)) as f64 / 63.0
            } else {
                -std::f64::consts::PI * u * ((n + 1) * (n + 2)) as f64 / 63.0
            };
            // Subcarrier k in [-31..-1, +1..+31] mapped to FFT bins [97..127, 1..31].
            let k = if n < 31 { 128 - 31 + n } else { 1 + (n - 31) };
            // IFFT via conjugate-FFT: store conj(X[k]) then run forward FFT and conjugate.
            re[k] = angle.cos();
            im[k] = -angle.sin();
        }
        fft_complex(&mut re, &mut im);
        let mut norm2 = 0.0f64;
        for k in 0..128 {
            im[k] = -im[k];
            norm2 += re[k] * re[k] + im[k] * im[k];
        }
        let inv = 1.0 / norm2.sqrt().max(1e-12);
        for k in 0..128 {
            out[r_idx][k] = (re[k] * inv, im[k] * inv);
        }
    }
    out
}

/// Correlate `iq` (at 1.92 MS/s) with an LTE PSS template shifted by `df_hz` at a specific sample index `pos`.
fn lte_pss_corr_at(iq: &[(f64, f64)], tmpl: &[(f64, f64); 128], pos: usize, df_hz: f64) -> (f64, f64) {
    let dphi = -2.0 * std::f64::consts::PI * df_hz / 1_920_000.0;
    let (step_r, step_i) = (dphi.cos(), dphi.sin());
    let (mut lo_r, mut lo_i) = (1.0f64, 0.0f64);
    let (mut acc_r, mut acc_i) = (0.0f64, 0.0f64);
    for m in 0..128 {
        let (xi, xq) = iq[pos + m];
        // Shift input by -df_hz, then multiply by conj(tmpl[m]).
        let sr = xi * lo_r - xq * lo_i;
        let si = xi * lo_i + xq * lo_r;
        let (tr, ti) = tmpl[m];
        acc_r += sr * tr + si * ti;
        acc_i += si * tr - sr * ti;
        let nr = lo_r * step_r - lo_i * step_i;
        lo_i = lo_r * step_i + lo_i * step_r;
        lo_r = nr;
    }
    (acc_r, acc_i)
}

/// Probe a single LTE downlink center frequency at 3.84 MS/s (2x 1.92 MS/s) using PSS correlation
/// and 5.000 ms PSS-to-PSS coherent phase advance for sub-Hz (< 0.001 ppm) frequency accuracy.
fn probe_lte_channel(
    cfg: &NodeConfig,
    label: &str,
    center_hz: u64,
    templates: &[[(f64, f64); 128]; 3],
) -> Result<Option<CalHit>, String> {
    const LTE_FS_RAW: u32 = 3_840_000;
    const HALF_FRAME: usize = 9_600; // 5.000 ms at 1.92 MS/s
    const N_SLOTS: usize = 10; // 50 ms span -> 10 PSS copies
    const N_IQ: usize = HALF_FRAME * (N_SLOTS + 1) + 256;
    let n_samples = (N_IQ * 2) as u64;

    let cs8 = capture_hackrf_cs8_fs(
        cfg.serial.as_deref(),
        center_hz,
        LTE_FS_RAW,
        n_samples,
        N_IQ * 4,
        24,
        24,
        false,
    )?;

    // Decimate 3.84 MS/s -> 1.92 MS/s (2-tap sum) and remove DC bias.
    let mut iq = Vec::with_capacity(N_IQ);
    let mut max_abs = 0i32;
    let (mut sum_i, mut sum_q) = (0.0f64, 0.0f64);
    for k in 0..N_IQ {
        let b = k * 4;
        let i0 = cs8[b] as i8 as i32;
        let q0 = cs8[b + 1] as i8 as i32;
        let i1 = cs8[b + 2] as i8 as i32;
        let q1 = cs8[b + 3] as i8 as i32;
        max_abs = max_abs.max(i0.abs()).max(q0.abs()).max(i1.abs()).max(q1.abs());
        let ri = (i0 + i1) as f64;
        let rq = (q0 + q1) as f64;
        sum_i += ri;
        sum_q += rq;
        iq.push((ri, rq));
    }
    let mean_i = sum_i / N_IQ as f64;
    let mean_q = sum_q / N_IQ as f64;
    for s in &mut iq {
        s.0 -= mean_i;
        s.1 -= mean_q;
    }

    // Coarse search: fold 2 half-frames via FFT correlation? Even faster: 16384-point FFT cross-correlation
    // across 1 half-frame (9,600 samples) for each 5 kHz frequency hypothesis in ±45 kHz!
    const C_N: usize = 16_384;
    let mut tmpl_fft = vec![(vec![0.0f64; C_N], vec![0.0f64; C_N]); 3];
    for t in 0..3 {
        for m in 0..128 {
            // Time-reversed conjugate for FFT cross-correlation: corr[n] = IFFT(FFT(x) * conj(FFT(h)))
            tmpl_fft[t].0[m] = templates[t][m].0;
            tmpl_fft[t].1[m] = templates[t][m].1;
        }
        let (ref mut tr, ref mut ti) = tmpl_fft[t];
        fft_complex(tr, ti);
    }

    let mut best_snr = 0.0f64;
    let mut best_tid = 0usize;
    let mut best_pos = 0usize;
    let mut best_df = 0.0f64;

    let mut x_re = vec![0.0f64; C_N];
    let mut x_im = vec![0.0f64; C_N];
    let mut c_re = vec![0.0f64; C_N];
    let mut c_im = vec![0.0f64; C_N];

    for step in -9i32..=9 {
        let df_hz = step as f64 * 5_000.0;
        let dphi = -2.0 * std::f64::consts::PI * df_hz / 1_920_000.0;
        let (sr, si) = (dphi.cos(), dphi.sin());
        let (mut lr, mut li) = (1.0f64, 0.0f64);
        for k in 0..C_N {
            if k < HALF_FRAME + 128 {
                let (xi, xq) = iq[k];
                x_re[k] = xi * lr - xq * li;
                x_im[k] = xi * li + xq * lr;
                let nr = lr * sr - li * si;
                li = lr * si + li * sr;
                lr = nr;
            } else {
                x_re[k] = 0.0;
                x_im[k] = 0.0;
            }
        }
        fft_complex(&mut x_re, &mut x_im);

        for (t, (tf_re, tf_im)) in tmpl_fft.iter().enumerate() {
            for k in 0..C_N {
                // Conjugate(X * conj(H)) -> forward FFT gives N * conj(corr)
                let pr = x_re[k] * tf_re[k] + x_im[k] * tf_im[k];
                let pi = x_im[k] * tf_re[k] - x_re[k] * tf_im[k];
                c_re[k] = pr;
                c_im[k] = -pi;
            }
            fft_complex(&mut c_re, &mut c_im);
            let mut peak = 0.0f64;
            let mut peak_i = 0usize;
            let mut sum_p = 0.0f64;
            for k in 0..HALF_FRAME {
                let p = c_re[k] * c_re[k] + c_im[k] * c_im[k];
                sum_p += p;
                if p > peak {
                    peak = p;
                    peak_i = k;
                }
            }
            let mean_p = (sum_p / HALF_FRAME as f64).max(1e-12);
            let snr_db = 10.0 * (peak / mean_p).log10();
            if snr_db > best_snr {
                best_snr = snr_db;
                best_tid = t;
                best_pos = peak_i;
                best_df = df_hz;
            }
        }
    }

    if best_snr < 17.5 {
        return Ok(None);
    }

    // Refine timing (±2 samples) and frequency (±4 kHz in 250 Hz steps -> ±300 Hz in 25 Hz steps)
    // coherently across all 10 consecutive 5.000 ms PSS copies!
    let tmpl = &templates[best_tid];
    let mut best_p2 = -1.0f64;
    let mut ref_pos = best_pos;
    for dp in -2isize..=2 {
        let p0 = (best_pos as isize + dp).clamp(0, (HALF_FRAME - 1) as isize) as usize;
        let (cr, ci) = lte_pss_corr_at(&iq, tmpl, p0, best_df);
        let m2 = cr * cr + ci * ci;
        if m2 > best_p2 {
            best_p2 = m2;
            ref_pos = p0;
        }
    }

    let mut cur_df = best_df;
    for &(span, step) in &[(4000.0f64, 250.0f64), (300.0, 25.0)] {
        let n_steps = (span / step) as i32;
        let mut best_coh = -1.0f64;
        let mut win_df = cur_df;
        for s in -n_steps..=n_steps {
            let df = cur_df + s as f64 * step;
            // Coherently sum all 10 PSS correlations compensated by exp(-j 2pi df * k * 0.005).
            let (mut sum_r, mut sum_i) = (0.0f64, 0.0f64);
            for k in 0..N_SLOTS {
                let pos = ref_pos + k * HALF_FRAME;
                let (cr, ci) = lte_pss_corr_at(&iq, tmpl, pos, df);
                let phi = -2.0 * std::f64::consts::PI * df * (k as f64 * 0.005);
                let (pr, pi) = (phi.cos(), phi.sin());
                sum_r += cr * pr - ci * pi;
                sum_i += cr * pi + ci * pr;
            }
            let coh = sum_r * sum_r + sum_i * sum_i;
            if coh > best_coh {
                best_coh = coh;
                win_df = df;
            }
        }
        cur_df = win_df;
    }

    // Final sub-Hz differential phase across consecutive 5.000 ms PSS slots.
    let mut prev: Option<(f64, f64)> = None;
    let (mut dsum_r, mut dsum_i) = (0.0f64, 0.0f64);
    for k in 0..N_SLOTS {
        let pos = ref_pos + k * HALF_FRAME;
        let (cr, ci) = lte_pss_corr_at(&iq, tmpl, pos, cur_df);
        let phi = -2.0 * std::f64::consts::PI * cur_df * (k as f64 * 0.005);
        let (pr, pi) = (phi.cos(), phi.sin());
        let dr = cr * pr - ci * pi;
        let di = cr * pi + ci * pr;
        if let Some((qr, qi)) = prev {
            dsum_r += dr * qr + di * qi;
            dsum_i += di * qr - dr * qi;
        }
        prev = Some((dr, di));
    }
    let resid_hz = dsum_i.atan2(dsum_r) / (2.0 * std::f64::consts::PI * 0.005);
    let total_df = cur_df + resid_hz;
    let ppm = (total_df / center_hz as f64) * 1_000_000.0;
    let peak_pct = (max_abs as f32 / 127.0) * 100.0;

    Ok(Some(CalHit {
        label: format!("{label} #{best_tid}"),
        nominal_hz: center_hz,
        offset_hz: total_df,
        ppm,
        snr_db: best_snr,
        kind: CalKind::LteGps,
        birdie: false,
        peak_pct,
    }))
}

fn scan_lte_pss(cfg: &NodeConfig, state: &Arc<Mutex<SharedState>>) -> Result<Vec<CalHit>, String> {
    let templates = make_lte_pss_templates();
    let mut hits = Vec::new();
    for &(label, freq_hz) in LTE_US_CENTERS {
        if !state.lock().unwrap().running {
            break;
        }
        {
            let mut st = state.lock().unwrap();
            st.cal_status = format!("Scanning {label} (LTE PSS)...");
            st.phase_note = st.cal_status.clone();
        }
        if let Ok(Some(hit)) = probe_lte_channel(cfg, label, freq_hz, &templates) {
            state.lock().unwrap().cal_hits.push(hit.clone());
            hits.push(hit);
        }
    }
    hits.sort_by(|a, b| b.snr_db.partial_cmp(&a.snr_db).unwrap_or(std::cmp::Ordering::Equal));
    Ok(hits)
}

/// Single 1.4 s wideband capture across the entire AM broadcast band (540–1700 kHz, 10 kHz US grid),
/// centered 5 kHz off-grid (`1,355 kHz`) so no channel lands on HackRF DC.
fn scan_am_bcb(cfg: &NodeConfig) -> Result<Vec<CalHit>, String> {
    let center_hz: u64 = 1_355_000;
    let n_samples: u64 = 400 * 8192 + 48_000; // ~1.39 s
    // Start with modest gain to avoid 8-bit ADC clipping on strong local AM stations; step down if needed.
    let mut cs8 = capture_hackrf_cs8(cfg.serial.as_deref(), center_hz, n_samples, 8, 16, false)?;
    let max_abs = cs8.iter().map(|&b| (b as i8 as i32).abs()).max().unwrap_or(0);
    if max_abs >= 115 {
        cs8 = capture_hackrf_cs8(cfg.serial.as_deref(), center_hz, n_samples, 0, 8, false)?;
    }

    // Coarse 4096-point Welch PSD across 2.4 MHz (586 Hz/bin) to rank all 10 kHz AM channels quickly.
    const W_N: usize = 4096;
    let mut psd = vec![0.0f64; W_N];
    let frames = (cs8.len() / (W_N * 2)).min(32);
    let mut re = vec![0.0f64; W_N];
    let mut im = vec![0.0f64; W_N];
    for f in 0..frames {
        let base = f * W_N * 2;
        for k in 0..W_N {
            let w = 0.5 * (1.0 - (2.0 * std::f64::consts::PI * k as f64 / W_N as f64).cos());
            re[k] = (cs8[base + 2 * k] as i8 as f64) * w;
            im[k] = (cs8[base + 2 * k + 1] as i8 as f64) * w;
        }
        fft_complex(&mut re, &mut im);
        for k in 0..W_N {
            psd[k] += re[k] * re[k] + im[k] * im[k];
        }
    }

    let bin_hz = HACKRF_FS as f64 / W_N as f64;
    let mut candidates: Vec<(u64, f64)> = Vec::new();
    let mut f_khz = 540u64;
    while f_khz <= 1700 {
        let f_hz = f_khz * 1000;
        // Skip channels near DC (1355 kHz) and exact 200 kHz clock multiples.
        if f_hz.abs_diff(center_hz) >= 15_000 && !f_khz.is_multiple_of(200) {
            let rel = f_hz as f64 - center_hz as f64;
            let b0 = (rel / bin_hz).round() as isize;
            let mut peak = 0.0f64;
            for db in -1..=1 {
                let idx = (b0 + db).rem_euclid(W_N as isize) as usize;
                peak = peak.max(psd[idx]);
            }
            let mut side = 0.0f64;
            for db in [-5isize, -4, 4, 5] {
                let idx = (b0 + db).rem_euclid(W_N as isize) as usize;
                side += psd[idx];
            }
            let ratio = peak / (side / 4.0).max(1e-12);
            candidates.push((f_hz, ratio));
        }
        f_khz += 10;
    }
    candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut hits = Vec::new();
    for &(f_hz, _) in candidates.iter().take(12) {
        if let Some((offset_hz, snr_db, _sb_snr, peak_pct)) = measure_carrier_iq(&cs8, center_hz, f_hz, 120.0) {
            let ppm = (offset_hz / f_hz as f64) * 1_000_000.0;
            hits.push(CalHit {
                label: format!("AM {:>4} kHz", f_hz / 1000),
                nominal_hz: f_hz,
                offset_hz,
                ppm,
                snr_db,
                kind: CalKind::AmBcb,
                birdie: false,
                peak_pct,
            });
        }
    }
    hits.sort_by(|a, b| b.snr_db.partial_cmp(&a.snr_db).unwrap_or(std::cmp::Ordering::Equal));
    Ok(hits)
}

fn summarize_calibration(hits: &[CalHit]) -> Option<(f64, String)> {
    // Priority 1: GPS-disciplined LTE downlink towers (±0.05 ppm spec, SNR >= 17.5 dB).
    let mut lte_good: Vec<&CalHit> = hits
        .iter()
        .filter(|h| h.kind == CalKind::LteGps && h.snr_db >= 17.5)
        .collect();
    if !lte_good.is_empty() {
        lte_good.sort_by(|a, b| a.ppm.partial_cmp(&b.ppm).unwrap_or(std::cmp::Ordering::Equal));
        let med = lte_good[lte_good.len() / 2].ppm;
        let cluster: Vec<&CalHit> = lte_good
            .iter()
            .copied()
            .filter(|h| (h.ppm - med).abs() <= 1.0)
            .collect();
        let best = cluster
            .iter()
            .max_by(|a, b| a.snr_db.partial_cmp(&b.snr_db).unwrap_or(std::cmp::Ordering::Equal))
            .copied()
            .unwrap_or(lte_good[0]);
        let wsum: f64 = cluster.iter().map(|h| h.snr_db.max(1.0)).sum();
        let avg_ppm = cluster.iter().map(|h| h.ppm * h.snr_db.max(1.0)).sum::<f64>() / wsum;
        return Some((
            avg_ppm,
            format!(
                "LTE GPS ({}/{} towers agree, best {} {:.1} dB)",
                cluster.len(),
                lte_good.len(),
                best.label,
                best.snr_db
            ),
        ));
    }

    // Priority 2: atomic HF standards (WWV/WWVH/CHU) with SNR >= 14 dB and not flagged as internal clock birdies.
    let mut atomic_good: Vec<&CalHit> = hits
        .iter()
        .filter(|h| h.kind == CalKind::Atomic && !h.birdie && h.snr_db >= 14.0)
        .collect();
    atomic_good.sort_by(|a, b| b.snr_db.partial_cmp(&a.snr_db).unwrap_or(std::cmp::Ordering::Equal));
    if let Some(best) = atomic_good.first() {
        let cluster: Vec<&CalHit> = atomic_good
            .iter()
            .copied()
            .filter(|h| (h.ppm - best.ppm).abs() <= 1.5)
            .collect();
        let wsum: f64 = cluster.iter().map(|h| h.snr_db.max(1.0)).sum();
        let avg_ppm = cluster.iter().map(|h| h.ppm * h.snr_db.max(1.0)).sum::<f64>() / wsum;
        let desc = if cluster.len() > 1 {
            format!("{} + {} atomic refs (best {:.1} dB)", best.label, cluster.len() - 1, best.snr_db)
        } else {
            format!("{} (atomic, {:.1} dB)", best.label, best.snr_db)
        };
        return Some((avg_ppm, desc));
    }

    // Priority 3: ATSC 1.0 UHF TV pilots (SNR >= 18 dB), median across up to top 4 channels.
    let mut atsc_good: Vec<&CalHit> = hits
        .iter()
        .filter(|h| h.kind == CalKind::AtscPilot && h.snr_db >= 18.0)
        .take(4)
        .collect();
    if !atsc_good.is_empty() {
        atsc_good.sort_by(|a, b| a.ppm.partial_cmp(&b.ppm).unwrap_or(std::cmp::Ordering::Equal));
        let med_ppm = atsc_good[atsc_good.len() / 2].ppm;
        let agreeing = atsc_good.iter().filter(|h| (h.ppm - med_ppm).abs() <= 2.0).count();
        let best_snr = atsc_good.iter().map(|h| h.snr_db).fold(0.0f64, f64::max);
        return Some((
            med_ppm,
            format!(
                "ATSC TV pilot median ({agreeing}/{} stations within ±2 ppm, peak {:.1} dB)",
                atsc_good.len(),
                best_snr
            ),
        ));
    }

    // Priority 4: single custom reference if present and SNR >= 14 dB.
    if let Some(custom) = hits.iter().find(|h| h.kind == CalKind::Custom && h.snr_db >= 14.0) {
        return Some((
            custom.ppm,
            format!("{} ({:.1} dB)", custom.label, custom.snr_db),
        ));
    }

    // Priority 5: median across strong AM BCB carriers (SNR >= 18 dB).
    let mut am_good: Vec<&CalHit> = hits
        .iter()
        .filter(|h| h.kind == CalKind::AmBcb && h.snr_db >= 18.0)
        .take(7)
        .collect();
    if am_good.is_empty() {
        return None;
    }
    am_good.sort_by(|a, b| a.ppm.partial_cmp(&b.ppm).unwrap_or(std::cmp::Ordering::Equal));
    let med_ppm = am_good[am_good.len() / 2].ppm;
    let agreeing = am_good.iter().filter(|h| (h.ppm - med_ppm).abs() <= 2.5).count();
    let best_snr = am_good.iter().map(|h| h.snr_db).fold(0.0f64, f64::max);
    Some((
        med_ppm,
        format!(
            "AM BCB median ({agreeing}/{} stations within ±2.5 ppm, peak {:.1} dB)",
            am_good.len(),
            best_snr
        ),
    ))
}

fn run_calibration(mode: CalMode, state: &Arc<Mutex<SharedState>>) {
    let cfg = {
        let mut st = state.lock().unwrap();
        st.cal_running = true;
        st.cal_hits.clear();
        st.cal_recommended = None;
        st.phase = SlotPhase::Calibrating;
        st.phase_note = "Frequency self-calibration...".to_string();
        st.cal_status = "Starting calibration...".to_string();
        st.cfg.clone()
    };

    let mut all_hits: Vec<CalHit> = Vec::new();

    match mode {
        CalMode::Auto => {
            // Stage 1: GPS-disciplined LTE downlink towers (±0.05 ppm, works indoors).
            if let Ok(lte_hits) = scan_lte_pss(&cfg, state) {
                all_hits.extend(lte_hits);
            }
            // If LTE didn't lock strongly, try UHF ATSC 1.0 TV pilots.
            if summarize_calibration(&all_hits).is_none() && state.lock().unwrap().running {
                if let Ok(atsc_hits) = scan_atsc_pilots(&cfg, state) {
                    all_hits.extend(atsc_hits.iter().cloned());
                    state.lock().unwrap().cal_hits.extend(atsc_hits);
                }
            }
            // If neither LTE nor ATSC locked, fall back to WWV/CHU atomic HF + AM broadcast band.
            if summarize_calibration(&all_hits).is_none() && state.lock().unwrap().running {
                for &(label, freq_hz) in WWV_REFS {
                    if !state.lock().unwrap().running {
                        break;
                    }
                    {
                        let mut st = state.lock().unwrap();
                        st.cal_status = format!("Probing {label}...");
                        st.phase_note = st.cal_status.clone();
                    }
                    if let Ok(hit) = probe_single_carrier(&cfg, label, freq_hz, CalKind::Atomic) {
                        all_hits.push(hit.clone());
                        state.lock().unwrap().cal_hits.push(hit);
                    }
                }
                if state.lock().unwrap().running {
                    {
                        let mut st = state.lock().unwrap();
                        st.cal_status = "Scanning AM broadcast band (540–1700 kHz, 10 kHz grid)...".to_string();
                        st.phase_note = st.cal_status.clone();
                    }
                    if let Ok(am_hits) = scan_am_bcb(&cfg) {
                        all_hits.extend(am_hits.iter().cloned());
                        state.lock().unwrap().cal_hits.extend(am_hits);
                    }
                }
            }
        }
        CalMode::Lte => {
            match scan_lte_pss(&cfg, state) {
                Ok(lte_hits) => all_hits.extend(lte_hits),
                Err(e) => state.lock().unwrap().cal_status = format!("LTE scan error: {e}"),
            }
        }
        CalMode::Atsc => {
            match scan_atsc_pilots(&cfg, state) {
                Ok(atsc_hits) => {
                    all_hits.extend(atsc_hits.iter().cloned());
                    state.lock().unwrap().cal_hits.extend(atsc_hits);
                }
                Err(e) => state.lock().unwrap().cal_status = format!("ATSC scan error: {e}"),
            }
        }
        CalMode::WwvOnly => {
            for &(label, freq_hz) in WWV_REFS {
                if !state.lock().unwrap().running {
                    break;
                }
                {
                    let mut st = state.lock().unwrap();
                    st.cal_status = format!("Probing {label}...");
                    st.phase_note = st.cal_status.clone();
                }
                match probe_single_carrier(&cfg, label, freq_hz, CalKind::Atomic) {
                    Ok(hit) => {
                        all_hits.push(hit.clone());
                        state.lock().unwrap().cal_hits.push(hit);
                    }
                    Err(e) => {
                        state.lock().unwrap().cal_status = format!("{label} error: {e}");
                    }
                }
            }
        }
        CalMode::AmBcb => {
            {
                let mut st = state.lock().unwrap();
                st.cal_status = "Scanning AM broadcast band (540–1700 kHz, 10 kHz grid)...".to_string();
                st.phase_note = st.cal_status.clone();
            }
            match scan_am_bcb(&cfg) {
                Ok(am_hits) => {
                    all_hits.extend(am_hits.iter().cloned());
                    state.lock().unwrap().cal_hits.extend(am_hits);
                }
                Err(e) => {
                    state.lock().unwrap().cal_status = format!("AM BCB scan error: {e}");
                }
            }
        }
        CalMode::Custom(freq_hz) => {
            let label = format!("Custom {:.6} MHz", freq_hz as f64 / 1e6);
            {
                let mut st = state.lock().unwrap();
                st.cal_status = format!("Probing {label}...");
                st.phase_note = st.cal_status.clone();
            }
            match probe_single_carrier(&cfg, &label, freq_hz, CalKind::Custom) {
                Ok(hit) => {
                    all_hits.push(hit.clone());
                    state.lock().unwrap().cal_hits.push(hit);
                }
                Err(e) => {
                    state.lock().unwrap().cal_status = format!("Custom probe error: {e}");
                }
            }
        }
    }

    // Sort results: strong hits first (atomic prioritized if >= 14 dB).
    all_hits.sort_by(|a, b| b.snr_db.partial_cmp(&a.snr_db).unwrap_or(std::cmp::Ordering::Equal));
    let rec = summarize_calibration(&all_hits);

    let mut st = state.lock().unwrap();
    st.cal_hits = all_hits;
    st.cal_recommended = rec.clone();
    st.cal_running = false;
    st.phase = SlotPhase::Waiting;
    match rec {
        Some((ppm, why)) => {
            st.cal_status = format!("Done — recommended {:+.2} ppm via {why} (press Enter to apply)", ppm);
            st.push_event(format!(
                "[{}] CAL recommended {:+.2} ppm ({why})",
                format_utc_hms(utc_now_ms() / 1000),
                ppm
            ));
        }
        None => {
            st.cal_status = "No strong reference carrier found (check antenna or try Custom freq [f])".to_string();
        }
    }
}

fn slot_worker(state: Arc<Mutex<SharedState>>) {
    let mut last_tx_slot: Option<u64> = None;
    loop {
        if !state.lock().unwrap().running {
            break;
        }
        let cal_mode = state.lock().unwrap().cal_request.take();
        if let Some(mode) = cal_mode {
            run_calibration(mode, &state);
            continue;
        }

        let now_ms = utc_now_ms();
        let in_slot = now_ms % 10_000;
        let rem_ms = 10_000 - in_slot;
        // Peek whether the upcoming slot will TX so we pre-trigger 350 ms early for TX synth,
        // while RX starts right at the 0 ms slot boundary to capture the full +0.40s..+9.20s burst.
        let next_slot = (now_ms + rem_ms + 50) / 10_000;
        let next_utc_sec = next_slot * 10;
        let will_tx = last_tx_slot.is_none_or(|prev| next_slot >= prev + 2) && {
            let st = state.lock().unwrap();
            let q_ready = st
                .tx_queue
                .front()
                .is_some_and(|j| j.parity.is_none_or(|p| next_slot % 2 == p));
            let rdv_ready = st.rendezvous
                && rdv_active_band(next_utc_sec).is_some()
                && rdv_plan_cq(st.cfg.call, st.cfg.suffix, next_slot).is_some();
            q_ready
                || (st.tx_queue.is_empty() && rdv_ready)
                || (st.tx_queue.is_empty() && st.auto_cq && (next_slot % 2 == st.cq_parity))
        };
        let lead_ms = if will_tx { 350 } else { 0 };
        if in_slot > 250 && rem_ms > lead_ms {
            {
                let mut st = state.lock().unwrap();
                st.phase = SlotPhase::Waiting;
                st.phase_note = "Syncing to next 10s UTC slot...".to_string();
            }
            let mut left = rem_ms - lead_ms;
            while left > 0 {
                let (running, has_cal) = {
                    let st = state.lock().unwrap();
                    (st.running, st.cal_request.is_some())
                };
                if !running {
                    return;
                }
                if has_cal {
                    break;
                }
                let d = left.min(100);
                thread::sleep(Duration::from_millis(d));
                left -= d;
            }
            let cal_mode = state.lock().unwrap().cal_request.take();
            if let Some(mode) = cal_mode {
                run_calibration(mode, &state);
                continue;
            }
        }

        // Round to nearest 10s slot boundary.
        let slot_ms = utc_now_ms() + 500;
        let slot = slot_ms / 10_000;
        let utc_sec = slot * 10;

        // Enforce strict alternating TX/RX turns: always listen for at least 1 RX slot after any TX slot.
        let tx_allowed = last_tx_slot.is_none_or(|prev| slot >= prev + 2);

        let (cfg, tx_job) = {
            let mut st = state.lock().unwrap();
            if !st.running {
                break;
            }
            // Rendezvous hourly UTC band-hopping (10m :00, 12m :15, 15m :30, 17m :45).
            let rdv_now = if st.rendezvous { rdv_active_band(utc_sec) } else { None };
            if let Some((lbl, rdv_dial)) = rdv_now {
                if st.cfg.dial_hz != rdv_dial {
                    st.cfg.dial_hz = rdv_dial;
                    st.push_event(format!(
                        "[{}] RDV hop -> {} ({:.3} MHz)",
                        format_utc_hms(utc_sec),
                        lbl,
                        rdv_dial as f64 / 1e6
                    ));
                }
            }
            let job = if !tx_allowed {
                None
            } else if st
                .tx_queue
                .front()
                .is_some_and(|j| j.parity.is_none_or(|p| slot % 2 == p))
            {
                st.tx_queue.pop_front()
            } else if !st.tx_queue.is_empty() {
                // Waiting for the opposite parity slot so we don't collide with the peer's TX slot.
                None
            } else if rdv_now.is_some() {
                rdv_plan_cq(st.cfg.call, st.cfg.suffix, slot).map(|subch| {
                    let frame = Frame::Cq {
                        sender: st.cfg.call,
                        grid4: st.cfg.grid4,
                        tx_pwr: 10,
                        reserved: (st.cfg.suffix & 0x0F) as u16,
                    };
                    TxJob {
                        frame,
                        subch,
                        summary: format!("RDV CQ {} {}", st.cfg.call_str, st.cfg.grid_str),
                        parity: Some(0),
                    }
                })
            } else if st.auto_cq && (slot % 2 == st.cq_parity) {
                let frame = Frame::Cq {
                    sender: st.cfg.call,
                    grid4: st.cfg.grid4,
                    tx_pwr: 10,
                    reserved: (st.cfg.suffix & 0x0F) as u16,
                };
                Some(TxJob {
                    frame,
                    subch: st.cfg.subch,
                    summary: format!("CQ {} {}", st.cfg.call_str, st.cfg.grid_str),
                    parity: Some(st.cq_parity),
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
                    if st.cal_request.is_some() || !st.running {
                        continue;
                    }
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

const SETTINGS_COUNT: usize = 13;

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
    let cq_badge = if st.rendezvous {
        "\x1b[1;30;46m RDV \x1b[0;1;37;44m"
    } else if st.auto_cq {
        "\x1b[1;30;42m AUTO-CQ \x1b[0;1;37;44m"
    } else {
        ""
    };
    let hunt_badge = if st.auto_answer != AutoAnswer::Off {
        format!("\x1b[1;30;45m{}\x1b[0;1;37;44m", st.auto_answer.badge())
    } else {
        String::new()
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
    buf.push_str(&format!("\x1b[1;37;44m{header}{tx_badge}{cq_badge}{hunt_badge}\x1b[K\x1b[0m\r\n"));

    // 2. Slot progress bar + Audio Peak
    let (phase_col, phase_lbl) = match st.phase {
        SlotPhase::Waiting => ("\x1b[33m", "WAIT  "),
        SlotPhase::Rx => ("\x1b[32m", "RX    "),
        SlotPhase::Tx => ("\x1b[1;31m", "TX    "),
        SlotPhase::DryTx => ("\x1b[1;33m", "DRY-TX"),
        SlotPhase::Calibrating => ("\x1b[1;35m", "CAL   "),
    };
    let bar_w = 26usize;
    let filled = ((slot_elapsed_ms as usize * bar_w) / 10_000).min(bar_w);
    let bar: String = "█".repeat(filled) + &"░".repeat(bar_w - filled);
    let cal_str = if st.cfg.ppm.abs() >= 0.05 {
        format!(
            "  Cal:{:+.1}ppm({:+}Hz)",
            st.cfg.ppm,
            ppm_offset_hz(st.cfg.dial_hz, st.cfg.ppm)
        )
    } else {
        String::new()
    };
    let line2 = format!(
        " {phase_col}{phase_lbl}\x1b[0m [{phase_col}{bar}\x1b[0m] {:4.1}s/10s  Peak:{:3.0}%  Grids:{}  Q:{}{cal_str}",
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
        " {} {} {} {}   \x1b[2m{} | RX:{} TX:{} Dec:{} QSO:{}\x1b[0m\x1b[K\r\n",
        tab(View::Operate, "1:Operate"),
        tab(View::Settings, "2:Settings"),
        tab(View::Log, "3:QSO Log"),
        tab(View::Calibrate, "4:Calibrate"),
        trunc(&st.hackrf_banner, cols.saturating_sub(72)),
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
                    "Freq Cal (ppm)",
                    format!(
                        "{:+.1} ppm ({:+} Hz)",
                        st.cfg.ppm,
                        ppm_offset_hz(st.cfg.dial_hz, st.cfg.ppm)
                    ),
                    "←/→ ±1 ppm, Enter to type exact ppm",
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
        View::Calibrate => {
            buf.push_str(&format!(
                "\x1b[1;36m FREQUENCY SELF-CALIBRATION — current: {:+.2} ppm ({:+} Hz on {:.3} MHz)\x1b[0m\x1b[K\r\n",
                st.cfg.ppm,
                ppm_offset_hz(st.cfg.dial_hz, st.cfg.ppm),
                st.cfg.dial_hz as f64 / 1_000_000.0
            ));
            let status_col = if st.cal_running { "\x1b[1;35m" } else { "\x1b[1;33m" };
            buf.push_str(&format!(
                " Status: {status_col}{}\x1b[0m\x1b[K\r\n",
                trunc(&st.cal_status, cols.saturating_sub(10))
            ));
            if let Some((rec_ppm, ref why)) = st.cal_recommended {
                buf.push_str(&format!(
                    " \x1b[1;32mRecommended: {:+.2} ppm\x1b[0m  ({})\x1b[K\r\n",
                    rec_ppm,
                    trunc(why, cols.saturating_sub(32))
                ));
            } else {
                buf.push_str(" \x1b[2mPress [r] Auto, [e] LTE GPS, [t] ATSC TV, [w] WWV/CHU, [m] AM BCB, or [f] Custom Hz (e.g. RT-950 FM PTT)\x1b[0m\x1b[K\r\n");
            }
            if let Some(ref ed) = tui.editing {
                buf.push_str(&format!(
                    " \x1b[1;33mCustom Reference Freq (Hz): {ed}_\x1b[0m  (Enter to probe, Esc to cancel)\x1b[K\r\n"
                ));
            } else {
                buf.push_str(" \x1b[2mSource             Nominal       Offset       SNR      Implied ppm   ADC Peak\x1b[0m\x1b[K\r\n");
            }
            let max_show = body_rows.saturating_sub(4);
            if st.cal_hits.is_empty() {
                buf.push_str("   \x1b[2m(no reference measurements yet)\x1b[0m\x1b[K\r\n");
                for _ in 1..max_show {
                    buf.push_str("\x1b[K\r\n");
                }
            } else {
                for i in 0..max_show {
                    if let Some(h) = st.cal_hits.get(i) {
                        let col = if h.birdie {
                            "\x1b[2;33m"
                        } else if matches!(h.kind, CalKind::LteGps | CalKind::Atomic) && h.snr_db >= 12.0 {
                            "\x1b[1;32m"
                        } else if matches!(h.kind, CalKind::AtscPilot | CalKind::AmBcb | CalKind::Custom) && h.snr_db >= 14.0 {
                            "\x1b[36m"
                        } else {
                            "\x1b[2m"
                        };
                        let note = if h.birdie { " [birdie?]" } else { "" };
                        let row = format!(
                            " {:<16}  {:>10.3} MHz  {:+7.2} Hz  {:5.1} dB  {:+8.2} ppm   {:4.0}%{note}",
                            h.label,
                            h.nominal_hz as f64 / 1_000_000.0,
                            h.offset_hz,
                            h.snr_db,
                            h.ppm,
                            h.peak_pct
                        );
                        buf.push_str(&format!("{col}{}\x1b[0m\x1b[K\r\n", trunc(&row, cols)));
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
        View::Operate => " [c] CQ  [a] Auto-CQ  [h] Hunt  [v] Rdv  [Enter] Answer  [b] Band  [d] Dry-TX  [s] Set  [l] Log  [k] Cal  [q] Quit",
        View::Settings => " [↑/↓] Select  [←/→] Change  [Enter] Edit/Toggle  [Esc/1] Operate  [q] Quit",
        View::Log => " [1/Esc] Operate  [2/s] Settings  [4/k] Cal  [q] Quit",
        View::Calibrate => " [r] Auto  [e] LTE GPS  [t] ATSC TV  [w] WWV/CHU  [m] AM BCB  [f] Custom Hz  [Enter] Apply  [1/Esc] Operate",
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
            st.cfg.ppm = (st.cfg.ppm + delta as f64).clamp(-200.0, 200.0);
            let hz = ppm_offset_hz(st.cfg.dial_hz, st.cfg.ppm);
            tui.set_toast(format!("Freq cal -> {:+.1} ppm ({:+} Hz)", st.cfg.ppm, hz));
        }
        5 => {
            let next = (st.cfg.subch as i32 + delta).rem_euclid(SUBCHANNELS_HZ.len() as i32) as usize;
            st.cfg.subch = next;
        }
        6 => {
            let next = (st.cfg.lna_db as i32 + delta * 8).clamp(0, 40) as u8;
            st.cfg.lna_db = (next / 8) * 8;
        }
        7 => {
            let next = (st.cfg.vga_db as i32 + delta * 2).clamp(0, 62) as u8;
            st.cfg.vga_db = (next / 2) * 2;
        }
        8 => {
            st.cfg.tx_vga_db = (st.cfg.tx_vga_db as i32 + delta).clamp(0, 47) as u8;
        }
        9 => st.cfg.amp = !st.cfg.amp,
        10 => {
            st.cfg.dry_tx = !st.cfg.dry_tx;
            tui.set_toast(if st.cfg.dry_tx {
                "Dry TX ON (RF suppressed)"
            } else {
                "Dry TX OFF — LIVE RF TRANSMIT ENABLED"
            });
        }
        11 => st.auto_qso = !st.auto_qso,
        12 => st.cq_parity = 1 - st.cq_parity,
        _ => {}
    }
}

fn commit_edit(state: &Arc<Mutex<SharedState>>, tui: &mut TuiState, text: String) {
    let mut st = state.lock().unwrap();
    match tui.set_sel {
        0 => match parse_call_with_suffix(&text) {
            Ok((c, suf, disp)) => {
                st.cfg.call = c;
                st.cfg.suffix = suf;
                st.cfg.call_str = disp;
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
        4 => match text.trim().parse::<f64>() {
            Ok(p) if (-200.0..=200.0).contains(&p) => {
                st.cfg.ppm = p;
                let hz = ppm_offset_hz(st.cfg.dial_hz, p);
                tui.set_toast(format!("Freq cal set to {:+.1} ppm ({:+} Hz)", p, hz));
            }
            _ => tui.set_toast("Invalid ppm (enter -200.0 .. +200.0)"),
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
    let mut ppm: f64 = 0.0;
    let mut subch: usize = 0;
    let mut lna_db: u8 = 16;
    let mut vga_db: u8 = 20;
    let mut tx_vga_db: u8 = 16;
    let mut amp = false;
    let mut dry_tx = false;
    let mut rendezvous = false;
    let mut auto_answer = AutoAnswer::Off;
    let mut tx_once = false;
    let mut rx_once = false;
    let mut tx_reply: Option<(String, i8, bool)> = None;
    let mut serial: Option<String> = None;
    let mut log_path = "rad950_hackrf.log".to_string();
    let mut loopback = false;
    let mut cli_cal: Option<CalMode> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                usage();
                return ExitCode::SUCCESS;
            }
            "--loopback" => loopback = true,
            "--calibrate" => cli_cal = Some(CalMode::Auto),
            "--cal-lte" => cli_cal = Some(CalMode::Lte),
            "--cal-atsc" => cli_cal = Some(CalMode::Atsc),
            "--cal-wwv" => cli_cal = Some(CalMode::WwvOnly),
            "--cal-am" => cli_cal = Some(CalMode::AmBcb),
            "--cal-freq" if i + 1 < args.len() => {
                i += 1;
                match args[i].parse::<u64>() {
                    Ok(f) if (500_000..=6_000_000_000).contains(&f) => cli_cal = Some(CalMode::Custom(f)),
                    _ => {
                        eprintln!("invalid --cal-freq (enter Hz, e.g. 10000000 or 1040000)");
                        return ExitCode::from(2);
                    }
                }
            }
            "--amp" => amp = true,
            "--dry-tx" => dry_tx = true,
            "--rendezvous" => rendezvous = true,
            "--auto-answer" if i + 1 < args.len() => {
                i += 1;
                match AutoAnswer::parse(&args[i]) {
                    Ok(m) => auto_answer = m,
                    Err(e) => {
                        eprintln!("{e}");
                        return ExitCode::from(2);
                    }
                }
            }
            "--tx-once" => tx_once = true,
            "--rx-once" => rx_once = true,
            "--tx-reply" if i + 3 < args.len() => {
                let to = args[i + 1].to_ascii_uppercase();
                let snr = args[i + 2].parse::<i8>().unwrap_or(-10);
                let ack = args[i + 3] == "1" || args[i + 3].eq_ignore_ascii_case("true");
                tx_reply = Some((to, snr, ack));
                i += 3;
            }
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
            "--ppm" if i + 1 < args.len() => {
                i += 1;
                match args[i].parse::<f64>() {
                    Ok(p) if (-200.0..=200.0).contains(&p) => ppm = p,
                    _ => {
                        eprintln!("invalid --ppm '{}' (use -200.0 .. +200.0)", args[i]);
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
            "--slot-offset-ms" if i + 1 < args.len() => {
                i += 1;
                match args[i].parse::<i64>() {
                    Ok(ms) => SLOT_OFFSET_MS.store(ms, Ordering::Relaxed),
                    Err(_) => {
                        eprintln!("invalid --slot-offset-ms '{}'", args[i]);
                        return ExitCode::from(2);
                    }
                }
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

    let (call, suffix, call_disp) = match parse_call_with_suffix(&call_str) {
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
        suffix,
        call_str: call_disp,
        grid4,
        grid_str: grid_clean,
        dial_hz,
        ppm,
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
        cfg: cfg.clone(),
        running: true,
        auto_cq: false,
        auto_qso: true,
        auto_answer,
        rendezvous,
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
        cal_request: None,
        cal_running: false,
        cal_status: "Idle — press [r] Auto, [e] LTE GPS, [t] ATSC TV, [w] WWV/CHU, [m] AM BCB, or [f] Custom Hz".to_string(),
        cal_hits: Vec::new(),
        cal_recommended: None,
    }));

    if let Some(mode) = cli_cal {
        println!("=== RAD950 HackRF Frequency Self-Calibration ===");
        run_calibration(mode, &state);
        let st = state.lock().unwrap();
        println!(
            "  {:<16}  {:>10}  {:>10}  {:>8}  {:>11}  {:>6}",
            "Source", "Nominal", "Offset", "SNR", "Implied ppm", "Peak"
        );
        for h in &st.cal_hits {
            let note = if h.birdie { " [birdie?]" } else { "" };
            println!(
                "  {:<16}  {:>7.3} MHz  {:+7.2} Hz  {:5.1} dB  {:+8.2} ppm  {:4.0}%{note}",
                h.label,
                h.nominal_hz as f64 / 1e6,
                h.offset_hz,
                h.snr_db,
                h.ppm,
                h.peak_pct
            );
        }
        match st.cal_recommended {
            Some((rec_ppm, ref why)) => {
                println!("\nRecommended: --ppm {:+.2}  ({})", rec_ppm, why);
                println!(
                    "  At {:.3} MHz this shifts HackRF LO by {:+} Hz.",
                    cfg.dial_hz as f64 / 1e6,
                    ppm_offset_hz(cfg.dial_hz, rec_ppm)
                );
            }
            None => {
                println!("\nNo strong reference carrier detected. Try --cal-freq <HZ> with a known local carrier or check antenna.");
            }
        }
        return ExitCode::SUCCESS;
    }

    if tx_once {
        let frame = Frame::Cq {
            sender: cfg.call,
            grid4: cfg.grid4,
            tx_pwr: 10,
            reserved: (cfg.suffix & 0x0F) as u16,
        };
        let job = TxJob {
            frame,
            subch: cfg.subch,
            summary: format!("CQ {} {}", cfg.call_str, cfg.grid_str),
            parity: None,
        };
        println!(
            "TX-ONCE: {} on {:.3} MHz (cal {:+.1} ppm, subch {} = {} Hz, tx_gain={} dB, dry={})",
            job.summary,
            cfg.dial_hz as f64 / 1e6,
            cfg.ppm,
            cfg.subch,
            SUBCHANNELS_HZ[cfg.subch],
            cfg.tx_vga_db,
            cfg.dry_tx
        );
        return match transmit_slot(&cfg, &job, &state) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("TX-ONCE failed: {e}");
                ExitCode::from(1)
            }
        };
    }

    if let Some((to_str, snr_db, ack)) = tx_reply {
        let rcpt = match parse_call(&to_str) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(2);
            }
        };
        let frame = Frame::Rx {
            sender: cfg.call,
            recipient: rcpt,
            snr_db,
            ack,
            reserved: false,
        };
        let tag = if ack { "R+RX" } else { "RX" };
        let job = TxJob {
            frame,
            subch: cfg.subch,
            summary: format!("{} de {} {tag} {:+}dB", to_str, cfg.call_str, snr_db),
            parity: None,
        };
        println!(
            "TX-REPLY: {} on {:.3} MHz (cal {:+.1} ppm, subch {} = {} Hz)",
            job.summary,
            cfg.dial_hz as f64 / 1e6,
            cfg.ppm,
            cfg.subch,
            SUBCHANNELS_HZ[cfg.subch]
        );
        return match transmit_slot(&cfg, &job, &state) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("TX-REPLY failed: {e}");
                ExitCode::from(1)
            }
        };
    }

    if rx_once {
        println!(
            "RX-ONCE: listening 9.45s on {:.3} MHz (cal {:+.1} ppm, LNA={} VGA={} amp={})",
            cfg.dial_hz as f64 / 1e6,
            cfg.ppm,
            cfg.lna_db,
            cfg.vga_db,
            cfg.amp
        );
        return match receive_slot(&cfg, &state) {
            Ok((frames, peak_pct)) => {
                println!("RX-ONCE done: {} decode(s), peak={:.1}%", frames.len(), peak_pct);
                for d in &frames {
                    println!("  DECODE: {:?}  hz={}  snr={:+}dB", d.frame, d.audio_hz, d.snr_db);
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("RX-ONCE failed: {e}");
                ExitCode::from(1)
            }
        };
    }

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
                    Key::Enter => {
                        if tui.view == View::Calibrate {
                            match ed.trim().parse::<u64>() {
                                Ok(f) if (500_000..=6_000_000_000).contains(&f) => {
                                    let mut st = state.lock().unwrap();
                                    st.cal_request = Some(CalMode::Custom(f));
                                    st.cal_status = format!("Queued custom probe at {:.6} MHz...", f as f64 / 1e6);
                                    tui.set_toast(format!("Probing custom carrier {:.6} MHz", f as f64 / 1e6));
                                }
                                _ => tui.set_toast("Invalid frequency (enter Hz, e.g. 10000000 or 1040000)"),
                            }
                        } else {
                            commit_edit(&state, &mut tui, ed);
                        }
                    }
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
                        View::Log => View::Calibrate,
                        View::Calibrate => View::Operate,
                    };
                }
                Key::Char('1') => tui.view = View::Operate,
                Key::Char('2') | Key::Char('s') => tui.view = View::Settings,
                Key::Char('3') | Key::Char('l') => tui.view = View::Log,
                Key::Char('4') | Key::Char('k') => tui.view = View::Calibrate,
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
                                let target_parity = (h.last_slot + 1) % 2;
                                let summary = format!(
                                    "{} de {} RX {:+}dB",
                                    h.call_str, st.cfg.call_str, h.snr_db
                                );
                                st.auto_cq = false;
                                st.tx_queue.push_front(TxJob {
                                    frame,
                                    subch,
                                    summary: summary.clone(),
                                    parity: Some(target_parity),
                                });
                                tui.set_toast(format!(
                                    "Queued answer ({} slot): {summary}",
                                    if target_parity == 0 { "even" } else { "odd" }
                                ));
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
                                    reserved: (st.cfg.suffix & 0x0F) as u16,
                                };
                                let subch = st.cfg.subch;
                                let summary = format!("CQ {} {}", st.cfg.call_str, st.cfg.grid_str);
                                st.tx_queue.push_back(TxJob {
                                    frame,
                                    subch,
                                    summary: summary.clone(),
                                    parity: None,
                                });
                                tui.set_toast(format!("Queued CQ for next TX slot: {summary}"));
                            }
                        }
                        Key::Char('v') => {
                            let mut st = state.lock().unwrap();
                            st.rendezvous = !st.rendezvous;
                            if st.rendezvous {
                                st.auto_cq = false;
                                tui.set_toast("Rendezvous ON (hourly 10m/12m/15m/17m + 50/50 turns)");
                            } else {
                                tui.set_toast("Rendezvous OFF");
                            }
                        }
                        Key::Char('h') => {
                            let mut st = state.lock().unwrap();
                            st.auto_answer = st.auto_answer.next();
                            if st.auto_answer != AutoAnswer::Off {
                                st.auto_cq = false;
                            }
                            tui.set_toast(format!("Hunter Auto-Answer: {}", st.auto_answer.label()));
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
                                4 => tui.editing = Some(format!("{:.1}", st.cfg.ppm)),
                                _ => {
                                    drop(st);
                                    adjust_setting(&state, &mut tui, 1);
                                }
                            }
                        }
                        _ => {}
                    },
                    View::Log => {}
                    View::Calibrate => match key {
                        Key::Char('r') => {
                            let mut st = state.lock().unwrap();
                            if st.cal_running {
                                tui.set_toast("Calibration already running...");
                            } else {
                                st.cal_request = Some(CalMode::Auto);
                                st.cal_status = "Queued Auto calibration (LTE GPS -> ATSC TV -> WWV/AM)...".to_string();
                                tui.set_toast("Starting Auto calibration...");
                            }
                        }
                        Key::Char('e') => {
                            let mut st = state.lock().unwrap();
                            if st.cal_running {
                                tui.set_toast("Calibration already running...");
                            } else {
                                st.cal_request = Some(CalMode::Lte);
                                st.cal_status = "Queued LTE GPS-disciplined downlink PSS scan...".to_string();
                                tui.set_toast("Starting LTE GPS calibration...");
                            }
                        }
                        Key::Char('t') => {
                            let mut st = state.lock().unwrap();
                            if st.cal_running {
                                tui.set_toast("Calibration already running...");
                            } else {
                                st.cal_request = Some(CalMode::Atsc);
                                st.cal_status = "Queued UHF ATSC 1.0 TV pilot scan (ch 14–36)...".to_string();
                                tui.set_toast("Starting ATSC TV pilot scan...");
                            }
                        }
                        Key::Char('w') => {
                            let mut st = state.lock().unwrap();
                            if st.cal_running {
                                tui.set_toast("Calibration already running...");
                            } else {
                                st.cal_request = Some(CalMode::WwvOnly);
                                st.cal_status = "Queued WWV/CHU atomic carrier probe...".to_string();
                                tui.set_toast("Starting WWV/CHU probe...");
                            }
                        }
                        Key::Char('m') => {
                            let mut st = state.lock().unwrap();
                            if st.cal_running {
                                tui.set_toast("Calibration already running...");
                            } else {
                                st.cal_request = Some(CalMode::AmBcb);
                                st.cal_status = "Queued AM broadcast band scan (540–1700 kHz)...".to_string();
                                tui.set_toast("Starting AM broadcast band scan...");
                            }
                        }
                        Key::Char('f') => {
                            if state.lock().unwrap().cal_running {
                                tui.set_toast("Wait for current calibration to finish");
                            } else {
                                tui.editing = Some("10000000".to_string());
                            }
                        }
                        Key::Enter => {
                            let mut st = state.lock().unwrap();
                            if let Some((rec_ppm, ref why)) = st.cal_recommended.clone() {
                                let applied = (rec_ppm * 100.0).round() / 100.0;
                                st.cfg.ppm = applied;
                                let hz = ppm_offset_hz(st.cfg.dial_hz, applied);
                                let dial_mhz = st.cfg.dial_hz as f64 / 1e6;
                                st.push_event(format!(
                                    "[{}] Applied CAL {:+.2} ppm ({:+} Hz) from {why}",
                                    format_utc_hms(utc_now_ms() / 1000),
                                    applied,
                                    hz
                                ));
                                tui.set_toast(format!(
                                    "Applied {:+.2} ppm ({:+} Hz on {:.3} MHz)",
                                    applied,
                                    hz,
                                    dial_mhz
                                ));
                            } else {
                                tui.set_toast("No recommended ppm yet — press [r] Auto or [m] AM BCB");
                            }
                        }
                        _ => {}
                    },
                },
            }
        }
    }

    drop(_raw);
    let _ = worker.join();
    ExitCode::SUCCESS
}
