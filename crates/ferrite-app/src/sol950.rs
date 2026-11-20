//! SOL950 10 m weak-signal runtime host attached to [`App`] (analogous to [`crate::sat`]).
//!
//! Keeps [`App`]'s RAM footprint small (`8` bytes for `Option<&'static mut Sol950Host>`) while owning
//! the [`ferrite_core::sol950::Engine`], station profile, GPS UTC discipline state, and TX staging.

use crate::{App, Screen};
use ferrite_core::freq::Freq10;
use ferrite_core::msg::RadioCmd;
use ferrite_core::sol950::usol950::frame::encode_grid4;
use ferrite_core::sol950::usol950::{Callsign, Decoded, Frame, SoftCandidate, GRID_UNKNOWN};
use ferrite_core::sol950::{Config, CoopRxReport, Engine, OpMode, QsoRecord, SlotPlan};

/// Nominal HF USB dial windows for SOL950 (10 m, 12 m, 15 m, and 17 m; FT8-adjacent +4 kHz windows).
/// Index `0` stays `28.078 MHz` so existing `SOL950_CFG` flash records remain backward-compatible.
pub const SOL950_DIALS: [Freq10; 10] = [
    2_807_800, // 10m: 28.078 MHz (+4 kHz above FT8 28.074)
    2_808_200, // 10m: 28.082 MHz
    2_812_000, // 10m: 28.120 MHz
    2_818_000, // 10m: 28.180 MHz
    2_491_900, // 12m: 24.919 MHz (+4 kHz above FT8 24.915)
    2_492_300, // 12m: 24.923 MHz
    2_107_800, // 15m: 21.078 MHz (+4 kHz above FT8 21.074)
    2_108_200, // 15m: 21.082 MHz
    1_810_400, // 17m: 18.104 MHz (+4 kHz above FT8 18.100)
    1_810_800, // 17m: 18.108 MHz
];

/// Short amateur band label for a SOL950 HF dial frequency (`17m`, `15m`, `12m`, or `10m`).
pub fn band_label(dial: Freq10) -> &'static str {
    if dial < 2_000_000 {
        "17m"
    } else if dial < 2_300_000 {
        "15m"
    } else if dial < 2_650_000 {
        "12m"
    } else {
        "10m"
    }
}

/// Maximum GPS UTC uncertainty (ms) permitted to transmit on a 10 s slot edge (`docs/sol950-firmware-feature.md` §3).
pub const MAX_TX_UNCERT_MS: u16 = 150;

/// Scrolling horizontal panafall dimensions (`112` time slices x `24` frequency bins = `224 x 48 px` at 2x2 px/cell).
/// At 5 slices/s (`200 ms`), the strip holds `22.4 s` (~2.2 SOL950 slots) scrolling right-to-left.
pub const WF_COLS: usize = 112;
pub const WF_ROWS: usize = 24;
pub const WF_COL_BYTES: usize = WF_ROWS / 2; // 12 bytes (two 4-bit bins per byte)

/// Compact right-to-left scrolling waterfall ring (`1,378` bytes, zero heap allocation).
pub struct Sol950Waterfall {
    cols: [[u8; WF_COL_BYTES]; WF_COLS],
    slot_marks: [u8; WF_COLS / 8],
    tx_marks: [u8; WF_COLS / 8],
    head: u8,
    pub seq: u32,
}

impl Default for Sol950Waterfall {
    fn default() -> Self {
        Self::new()
    }
}

impl Sol950Waterfall {
    pub const fn new() -> Self {
        Self {
            cols: [[0; WF_COL_BYTES]; WF_COLS],
            slot_marks: [0; WF_COLS / 8],
            tx_marks: [0; WF_COLS / 8],
            head: 0,
            seq: 0,
        }
    }

    pub fn clear(&mut self) {
        self.cols = [[0; WF_COL_BYTES]; WF_COLS];
        self.slot_marks = [0; WF_COLS / 8];
        self.tx_marks = [0; WF_COLS / 8];
        self.head = 0;
        self.seq = self.seq.wrapping_add(1);
    }

    /// Append the newest 24-bin spectrum slice at the right edge (scrolling older columns left).
    pub fn push(&mut self, col: [u8; WF_COL_BYTES], slot_edge: bool, is_tx: bool) {
        let idx = self.head as usize;
        self.cols[idx] = col;
        let byte = idx >> 3;
        let bit = 1u8 << (idx & 7);
        if slot_edge {
            self.slot_marks[byte] |= bit;
        } else {
            self.slot_marks[byte] &= !bit;
        }
        if is_tx {
            self.tx_marks[byte] |= bit;
        } else {
            self.tx_marks[byte] &= !bit;
        }
        self.head = if idx + 1 == WF_COLS { 0 } else { (idx + 1) as u8 };
        self.seq = self.seq.wrapping_add(1);
    }

    /// 4-bit intensity (`0..=15`) at screen column `x_col` (`0` = oldest/left, `WF_COLS - 1` = newest/right)
    /// and frequency bin `row` (`0` = lowest ~586 Hz, `WF_ROWS - 1` = highest ~2,344 Hz).
    #[inline]
    pub fn level(&self, x_col: usize, row: usize) -> u8 {
        let idx = (self.head as usize + x_col) % WF_COLS;
        let b = self.cols[idx][row >> 1];
        if (row & 1) == 0 { b & 0x0F } else { b >> 4 }
    }

    /// True if column `x_col` (`0` = left, `WF_COLS - 1` = right) marks a 10 s UTC slot boundary.
    #[inline]
    pub fn is_slot_mark(&self, x_col: usize) -> bool {
        let idx = (self.head as usize + x_col) % WF_COLS;
        (self.slot_marks[idx >> 3] & (1u8 << (idx & 7))) != 0
    }

    /// True if column `x_col` was recorded while transmitting (rendered in warm red TX palette).
    #[inline]
    pub fn is_tx_col(&self, x_col: usize) -> bool {
        let idx = (self.head as usize + x_col) % WF_COLS;
        (self.tx_marks[idx >> 3] & (1u8 << (idx & 7))) != 0
    }
}

/// Build a 24-bin packed 4-bit waterfall column for our own transmitted 4-FSK tone at `subchannel_hz + tone * 50`.
pub fn tx_wf_col(subchannel_hz: u16, tone: u8) -> [u8; WF_COL_BYTES] {
    let mut col = [0u8; WF_COL_BYTES];
    let f_hz = (subchannel_hz as i32) + (tone as i32 & 3) * 50;
    // Map `586..2344 Hz` (1,758 Hz span) across `0..24` rows.
    let pos_x16 = ((f_hz - 586).clamp(0, 1757) * (WF_ROWS as i32 * 16)) / 1758;
    let center = (pos_x16 >> 4).clamp(0, WF_ROWS as i32 - 1) as usize;
    let frac = (pos_x16 & 15) as usize;
    let mut set = |r: usize, v: u8| {
        if r < WF_ROWS {
            let cur = if (r & 1) == 0 { col[r >> 1] & 0x0F } else { col[r >> 1] >> 4 };
            let nv = cur.max(v & 0x0F);
            if (r & 1) == 0 {
                col[r >> 1] = (col[r >> 1] & 0xF0) | nv;
            } else {
                col[r >> 1] = (col[r >> 1] & 0x0F) | (nv << 4);
            }
        }
    };
    set(center, 15);
    if frac >= 8 && center + 1 < WF_ROWS {
        set(center + 1, 9);
    } else if frac < 8 && center > 0 {
        set(center - 1, 9);
    }
    col
}

/// Cooperative RX mode under RAD950 Setup (`docs/sol950-cooperative-reception.md`).
/// Pilot exposes `Off` and `Capture`; future live cooperation will add `Lead` and `Help`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CoopRxMode {
    Off = 0,
    Capture = 1,
}

impl CoopRxMode {
    pub const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Capture,
            _ => Self::Off,
        }
    }
}

/// Transport link for Cooperative RX team data sharing (independent of radio role).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CoopLink {
    Usb = 0,
    Bluetooth = 1,
}

impl CoopLink {
    pub const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Bluetooth,
            _ => Self::Usb,
        }
    }
}

/// Effective grid source displayed in the SOL950 header and saved with contacts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GridSource {
    Gps,
    Manual,
    Unset,
}

/// Compact contact record shared between `Sol950Host`, UI Log/Stats/Grids screens, and flash storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SavedContact {
    pub utc_sec: u32,
    pub dial_hz10: Freq10,
    pub peer_id: [u8; 16],
    pub my_id: [u8; 16],
    pub peer_grid: [u8; 4],
    pub my_grid: [u8; 4],
    pub rx_snr_db: i8,
    pub tx_snr_db: i8,
    pub my_grid_src: GridSource,
    pub slot: u32,
    pub seq: u32,
}

impl SavedContact {
    pub const EMPTY: Self = Self {
        utc_sec: 0,
        dial_hz10: 0,
        peer_id: [0; 16],
        my_id: [0; 16],
        peer_grid: *b"----",
        my_grid: *b"----",
        rx_snr_db: 0,
        tx_snr_db: 0,
        my_grid_src: GridSource::Unset,
        slot: 0,
        seq: 0,
    };

    pub fn peer_bytes(&self) -> &[u8] {
        let n = self.peer_id.iter().position(|&b| b == 0).unwrap_or(self.peer_id.len());
        &self.peer_id[..n]
    }

    pub fn my_bytes(&self) -> &[u8] {
        let n = self.my_id.iter().position(|&b| b == 0).unwrap_or(self.my_id.len());
        &self.my_id[..n]
    }
}

/// Aggregated contact statistics (`All saved` or `Today (UTC)`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SavedStats {
    pub contacts: u16,
    pub unique_stations: u16,
    pub unique_grids: u16,
    pub best_rx_snr: i8,
    pub best_tx_snr: i8,
    pub has_snr: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved(u32),
    Duplicate,
    LogFull,
    Error,
}

/// Award classification for a Maidenhead 4-char grid relative to the saved log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GridAward {
    /// Grid has already been worked on this band (or grid is unknown).
    None,
    /// Grid was worked on another band, but is new on the current band.
    NewBandGrid,
    /// Grid has never been worked on any band.
    NewGrid,
}

/// Call-list badge status for a heard peer relative to the saved log on the active band.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerCallStatus {
    /// True if a QSO with this station is already saved on the current band (or this session).
    pub worked: bool,
    /// New-grid / new-band-grid status (for unworked stations: what working them yields;
    /// for worked stations: whether working them yielded a new grid or new band grid).
    pub award: GridAward,
}

/// Band identifier in meters (`10, 12, 15, 17`) for a RAD950 USB dial.
pub const fn dial_band_m(dial_hz10: Freq10) -> u8 {
    if dial_hz10 >= 2_800_000 {
        10
    } else if dial_hz10 >= 2_400_000 {
        12
    } else if dial_hz10 >= 2_100_000 {
        15
    } else {
        17
    }
}

/// Parse a 4-char Maidenhead grid into `(field_idx 0..324, square_idx 0..100)`.
pub fn grid4_to_field_square(g: &[u8; 4]) -> Option<(u16, u8)> {
    let lf = g[0].to_ascii_uppercase();
    let bf = g[1].to_ascii_uppercase();
    if !(b'A'..=b'R').contains(&lf) || !(b'A'..=b'R').contains(&bf) {
        return None;
    }
    if !g[2].is_ascii_digit() || !g[3].is_ascii_digit() {
        return None;
    }
    let lon_f = (lf - b'A') as u16;
    let lat_f = (bf - b'A') as u16;
    let lon_s = g[2] - b'0';
    let lat_s = g[3] - b'0';
    Some((lon_f * 18 + lat_f, lon_s * 10 + lat_s))
}

/// Format a Maidenhead field index (`0..324`) as `[lon_letter, lat_letter]`.
pub fn field_letters(field_idx: u16) -> [u8; 2] {
    let lon_f = (field_idx / 18).min(17) as u8;
    let lat_f = (field_idx % 18) as u8;
    [b'A' + lon_f, b'A' + lat_f]
}

/// Storage backend trait for SOL950 Log, Stats, and Worked Grids (implemented by flash journal or RAM store).
pub trait Sol950Store {
    fn count(&self) -> u16;
    fn is_full(&self) -> bool;
    fn field_mask(&self) -> &[u32; 11];
    fn read_newest(&self, skip: u16, grid_filter: Option<[u8; 4]>, out: &mut [Option<SavedContact>]) -> usize;
    fn stats(&self, today_utc_day: Option<u32>) -> SavedStats;
    fn field_squares(&self, field_idx: u16) -> [u16; 100];
    fn save_contact(&mut self, c: &SavedContact) -> SaveOutcome;
}

pub const MEM_LOG_CAP: usize = 32;

/// Compact in-memory store (used as default in simulator/tests and RAM cache when flash backend isn't attached).
pub struct MemSol950Store {
    rows: [SavedContact; MEM_LOG_CAP],
    len: u16,
    cap: u16,
    field_mask: [u32; 11],
}

impl Default for MemSol950Store {
    fn default() -> Self {
        Self::new()
    }
}

impl MemSol950Store {
    pub const fn new() -> Self {
        Self {
            rows: [SavedContact::EMPTY; MEM_LOG_CAP],
            len: 0,
            cap: MEM_LOG_CAP as u16,
            field_mask: [0; 11],
        }
    }

    pub fn set_capacity(&mut self, cap: u16) {
        self.cap = cap.min(MEM_LOG_CAP as u16);
    }

    /// Classify a heard peer's worked status and New Grid / New Band Grid award against the saved log.
    pub fn classify_peer(&self, call: &[u8], grid: [u8; 4], dial_hz10: Freq10, session_logged: bool) -> PeerCallStatus {
        let cur_band = dial_band_m(dial_hz10);
        let valid_grid = grid4_to_field_square(&grid).is_some();
        let mut worked = session_logged;
        let mut grid_any_count = 0u16;
        let mut grid_band_count = 0u16;
        let mut peer_in_grid_any = false;
        let mut peer_in_grid_band = false;

        for c in self.rows.iter().take(self.len as usize) {
            let same_band = dial_band_m(c.dial_hz10) == cur_band;
            let same_call = c.peer_bytes().eq_ignore_ascii_case(call);
            if same_call && same_band {
                worked = true;
            }
            if valid_grid && c.peer_grid.eq_ignore_ascii_case(&grid) {
                grid_any_count += 1;
                if same_call {
                    peer_in_grid_any = true;
                }
                if same_band {
                    grid_band_count += 1;
                    if same_call {
                        peer_in_grid_band = true;
                    }
                }
            }
        }

        let award = if !valid_grid {
            GridAward::None
        } else if !worked {
            if grid_any_count == 0 {
                GridAward::NewGrid
            } else if grid_band_count == 0 {
                GridAward::NewBandGrid
            } else {
                GridAward::None
            }
        } else if peer_in_grid_any && grid_any_count == 1 {
            // This QSO is the sole (first) contact in this grid all-time!
            GridAward::NewGrid
        } else if peer_in_grid_band && grid_band_count == 1 {
            // This QSO is the sole (first) contact in this grid on this band!
            GridAward::NewBandGrid
        } else {
            GridAward::None
        };
        PeerCallStatus { worked, award }
    }

    /// Worked Maidenhead field bitmask (`324` bits) filtered by `band_m` (`None` = all bands, `Some(10|12|15|17)`).
    pub fn field_mask_for_band(&self, band_m: Option<u8>) -> [u32; 11] {
        let Some(bm) = band_m else {
            return self.field_mask;
        };
        let mut mask = [0u32; 11];
        for c in self.rows.iter().take(self.len as usize) {
            if dial_band_m(c.dial_hz10) == bm {
                if let Some((fi, _)) = grid4_to_field_square(&c.peer_grid) {
                    let i = fi as usize;
                    if i < 324 {
                        mask[i / 32] |= 1u32 << (i % 32);
                    }
                }
            }
        }
        mask
    }

    /// Per-square worked counts (`0..100`) inside `field_idx`, filtered by `band_m` (`None` = all bands).
    pub fn field_squares_for_band(&self, field_idx: u16, band_m: Option<u8>) -> [u16; 100] {
        let mut out = [0u16; 100];
        for c in self.rows.iter().take(self.len as usize) {
            if let Some(bm) = band_m {
                if dial_band_m(c.dial_hz10) != bm {
                    continue;
                }
            }
            if let Some((fi, si)) = grid4_to_field_square(&c.peer_grid) {
                if fi == field_idx && (si as usize) < 100 {
                    out[si as usize] = out[si as usize].saturating_add(1);
                }
            }
        }
        out
    }
}

impl Sol950Store for MemSol950Store {
    fn count(&self) -> u16 {
        self.len
    }

    fn is_full(&self) -> bool {
        self.len >= self.cap
    }

    fn field_mask(&self) -> &[u32; 11] {
        &self.field_mask
    }

    fn read_newest(&self, skip: u16, grid_filter: Option<[u8; 4]>, out: &mut [Option<SavedContact>]) -> usize {
        for slot in out.iter_mut() {
            *slot = None;
        }
        let mut skipped = 0u16;
        let mut written = 0usize;
        for idx in (0..self.len as usize).rev() {
            if written >= out.len() {
                break;
            }
            let c = self.rows[idx];
            if let Some(gf) = grid_filter {
                if !c.peer_grid.eq_ignore_ascii_case(&gf) {
                    continue;
                }
            }
            if skipped < skip {
                skipped += 1;
                continue;
            }
            out[written] = Some(c);
            written += 1;
        }
        written
    }

    fn stats(&self, today_utc_day: Option<u32>) -> SavedStats {
        let mut st = SavedStats { best_rx_snr: i8::MIN, best_tx_snr: i8::MIN, ..SavedStats::default() };
        for i in 0..self.len as usize {
            let c = self.rows[i];
            if let Some(day) = today_utc_day {
                if c.utc_sec / 86_400 != day {
                    continue;
                }
            }
            st.contacts = st.contacts.saturating_add(1);
            if !st.has_snr {
                st.best_rx_snr = c.rx_snr_db;
                st.best_tx_snr = c.tx_snr_db;
                st.has_snr = true;
            } else {
                st.best_rx_snr = st.best_rx_snr.max(c.rx_snr_db);
                st.best_tx_snr = st.best_tx_snr.max(c.tx_snr_db);
            }
            let mut seen_peer = false;
            let has_grid = grid4_to_field_square(&c.peer_grid).is_some();
            let mut seen_grid = !has_grid;
            for &p in self.rows.iter().take(i) {
                if let Some(day) = today_utc_day {
                    if p.utc_sec / 86_400 != day {
                        continue;
                    }
                }
                if p.peer_id == c.peer_id {
                    seen_peer = true;
                }
                if has_grid && p.peer_grid.eq_ignore_ascii_case(&c.peer_grid) {
                    seen_grid = true;
                }
            }
            if !seen_peer {
                st.unique_stations = st.unique_stations.saturating_add(1);
            }
            if !seen_grid {
                st.unique_grids = st.unique_grids.saturating_add(1);
            }
        }
        if !st.has_snr {
            st.best_rx_snr = 0;
            st.best_tx_snr = 0;
        }
        st
    }

    fn field_squares(&self, field_idx: u16) -> [u16; 100] {
        let mut sqs = [0u16; 100];
        for row in self.rows.iter().take(self.len as usize) {
            if let Some((f, sq)) = grid4_to_field_square(&row.peer_grid) {
                if f == field_idx {
                    sqs[sq as usize] = sqs[sq as usize].saturating_add(1);
                }
            }
        }
        sqs
    }

    fn save_contact(&mut self, c: &SavedContact) -> SaveOutcome {
        for i in (0..self.len as usize).rev() {
            let e = self.rows[i];
            if e.peer_id == c.peer_id && c.slot.abs_diff(e.slot) < 30 {
                return SaveOutcome::Duplicate;
            }
        }
        if self.is_full() {
            return SaveOutcome::LogFull;
        }
        let idx = self.len as usize;
        let mut rec = *c;
        rec.seq = (idx + 1) as u32;
        self.rows[idx] = rec;
        self.len += 1;
        if let Some((f, _)) = grid4_to_field_square(&rec.peer_grid) {
            let fi = f as usize;
            self.field_mask[fi / 32] |= 1u32 << (fi % 32);
        }
        SaveOutcome::Saved(rec.seq)
    }
}

/// Station identity and radio configuration for SOL950.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sol950Profile {
    pub base_call: [u8; 6],
    pub call_len: u8,
    pub prefix: [u8; 4],
    pub prefix_len: u8,
    /// Optional numeric suffix (`0` = none, `1..=15` = `-1`..`-15`).
    pub suffix: u8,
    pub use_gps_grid: bool,
    pub manual_grid: [u8; 4],
    pub dial_idx: u8,
    /// Signed SI4732 RX BFO correction in Hz (default `0`).
    pub rx_corr_hz: i16,
    /// TX power level (`0` Low, `1` Mid, `2` High).
    pub tx_power: u8,
    /// Cooperative RX mode (`Off` default; `Capture` exports RX + 92-byte soft candidates to host).
    pub coop_rx: CoopRxMode,
    /// Cooperative RX link transport (`Usb` or `Bluetooth`).
    pub coop_link: CoopLink,
    /// `RAD950-60` 1-minute weak-signal cycle (`5 × 10 s` v1 bursts in `:00..:49`, `:50..:59` cooldown/decode).
    pub mode60: bool,
}

impl Default for Sol950Profile {
    fn default() -> Self {
        Self::new()
    }
}

impl Sol950Profile {
    pub const fn new() -> Self {
        Self {
            base_call: [0; 6],
            call_len: 0,
            prefix: [0; 4],
            prefix_len: 0,
            suffix: 0,
            use_gps_grid: true,
            manual_grid: *b"FN31",
            dial_idx: 0,
            rx_corr_hz: 0,
            tx_power: 0,
            coop_rx: CoopRxMode::Off,
            coop_link: CoopLink::Usb,
            mode60: false,
        }
    }

    pub fn dial(&self) -> Freq10 {
        SOL950_DIALS[self.dial_idx as usize % SOL950_DIALS.len()]
    }

    pub fn set_base_call(&mut self, s: &[u8]) {
        self.base_call = [0; 6];
        let mut n = 0usize;
        for &b in s {
            if n >= 6 {
                break;
            }
            let c = b.to_ascii_uppercase();
            if c.is_ascii_alphanumeric() {
                self.base_call[n] = c;
                n += 1;
            }
        }
        self.call_len = n as u8;
    }

    pub fn set_prefix(&mut self, s: &[u8]) {
        self.prefix = [0; 4];
        let mut n = 0usize;
        for &b in s {
            if n >= 4 {
                break;
            }
            let c = b.to_ascii_uppercase();
            if c.is_ascii_alphanumeric() {
                self.prefix[n] = c;
                n += 1;
            }
        }
        self.prefix_len = n as u8;
    }

    pub fn base_call_bytes(&self) -> &[u8] {
        &self.base_call[..self.call_len as usize]
    }

    pub fn callsign(&self) -> Option<Callsign> {
        if self.call_len == 0 {
            return None;
        }
        Callsign::encode(self.base_call_bytes()).ok()
    }

    /// Format `[PREFIX/]BASE[-SUFFIX]` into `out`, returning the byte length.
    pub fn format_identity(&self, out: &mut [u8; 16]) -> usize {
        let mut n = 0usize;
        if self.prefix_len > 0 {
            let pl = (self.prefix_len as usize).min(4);
            out[n..n + pl].copy_from_slice(&self.prefix[..pl]);
            n += pl;
            out[n] = b'/';
            n += 1;
        }
        if self.call_len > 0 {
            let cl = (self.call_len as usize).min(6);
            out[n..n + cl].copy_from_slice(&self.base_call[..cl]);
            n += cl;
        } else {
            let unk = b"NO CALL";
            out[n..n + unk.len()].copy_from_slice(unk);
            n += unk.len();
        }
        if (1..=15).contains(&self.suffix) && n + 3 <= out.len() {
            out[n] = b'-';
            n += 1;
            if self.suffix >= 10 {
                out[n] = b'1';
                out[n + 1] = b'0' + (self.suffix - 10);
                n += 2;
            } else {
                out[n] = b'0' + self.suffix;
                n += 1;
            }
        }
        n
    }

    /// Effective 4-char Maidenhead grid and its source.
    pub fn effective_grid(&self, gps_grid: Option<[u8; 4]>) -> ([u8; 4], GridSource) {
        if self.use_gps_grid {
            if let Some(g) = gps_grid {
                return (g, GridSource::Gps);
            }
            return (*b"----", GridSource::Unset);
        }
        if encode_grid4(&self.manual_grid).is_some() {
            (self.manual_grid, GridSource::Manual)
        } else {
            (*b"----", GridSource::Unset)
        }
    }
}

/// Session-only attended Auto-Answer ("Hunter / Pounce") filter for decoded CQs.
/// Never persisted across reboot; cleared by `0` (Stop).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AutoAnswer {
    /// Manual `OK` to answer (default).
    Off,
    /// Auto-answer only all-time unworked 4-char grids (`NEW`).
    NewGrid,
    /// Auto-answer `NEW` or `BND` (grid not yet worked on this band).
    NewBandGrid,
    /// Auto-answer any unworked callsign on this band (or `NEW` / `BND`).
    AnyNeeded,
}

impl AutoAnswer {
    pub const fn next(self) -> Self {
        match self {
            Self::Off => Self::NewGrid,
            Self::NewGrid => Self::NewBandGrid,
            Self::NewBandGrid => Self::AnyNeeded,
            Self::AnyNeeded => Self::Off,
        }
    }

    pub const fn badge(self) -> &'static [u8] {
        match self {
            Self::Off => b"",
            Self::NewGrid => b"HUNT:NEW",
            Self::NewBandGrid => b"HUNT:BND",
            Self::AnyNeeded => b"HUNT:ALL",
        }
    }

    pub const fn label(self) -> &'static [u8] {
        match self {
            Self::Off => b"Off",
            Self::NewGrid => b"New Grid",
            Self::NewBandGrid => b"New Band+Grid",
            Self::AnyNeeded => b"Any Needed",
        }
    }

    pub fn matches(self, st: PeerCallStatus) -> bool {
        match self {
            Self::Off => false,
            Self::NewGrid => st.award == GridAward::NewGrid,
            Self::NewBandGrid => matches!(st.award, GridAward::NewGrid | GridAward::NewBandGrid),
            Self::AnyNeeded => !st.worked || st.award != GridAward::None,
        }
    }
}

/// Discipline source for `Sol950Host`'s 10 s UTC slot clock.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ClockSource {
    /// No synchronized UTC time available.
    None,
    /// `bench`-feature indoor monotonic fallback (not GPS-disciplined, no PNT).
    Bench,
    /// Disciplined by a prior onboard GPS fix, currently coasting on the `HEXT` crystal within `MAX_TX_UNCERT_MS`.
    GpsHoldover,
    /// Live onboard GPS-disciplined UTC clock.
    Gps,
}

/// Platform-owned SOL950 runtime state attached to [`App`] via [`App::sol950_attach`].
pub struct Sol950Host {
    pub engine: Engine,
    pub profile: Sol950Profile,
    /// Latest GPS UTC second and millisecond within that second.
    pub utc_sec: u32,
    pub utc_ms_in_sec: u16,
    /// Estimated UTC timing uncertainty in ms (`u16::MAX` when unsynchronized).
    pub utc_uncert_ms: u16,
    /// Discipline source for the current UTC slot clock (GPS vs bench fallback vs none).
    pub clock_src: ClockSource,
    /// Latest onboard GPS 4-char grid (never from fixed position `0 6 9`).
    pub gps_grid: Option<[u8; 4]>,
    pub last_slot: u32,
    /// RF and Cooperative RX state captured at the start of the current 10 s receive slot. The dial can change in
    /// Setup while the decoder is still ingesting samples, so end-of-slot telemetry must use this snapshot.
    rx_slot_start: u32,
    rx_slot_dial_hz: u32,
    rx_slot_coop_capture: bool,
    /// Metadata frozen when a slot finish is queued. It remains valid if the UI reaches the next UTC slot before
    /// the DSP task can start the previous slot's decode.
    finish_utc_slot: u32,
    finish_dial_hz: u32,
    finish_coop_capture: bool,
    pub finish_sent_slot: u32,
    /// Last 10 s UTC slot that ran TX (so the following RX slot resets `usol950::Receiver` cleanly).
    pub tx_slot: u32,
    /// A live SOL950 TX burst is currently keyed on BK4829.
    pub tx_in_flight: bool,
    /// Next frame staged by `Engine::on_slot_start` for the platform to hand to `SOL950_TX` + `RadioCmd::Sol950Tx`.
    pub pending_tx: Option<(Freq10, u8, Frame)>,
    /// True when the platform should send `DspCmd::Sol950Finish` at the end of the active RX slot.
    pub finish_due: bool,
    /// True when the platform should send `DspCmd::Sol950Reset` to clear muted TX-slot samples before RX.
    pub reset_due: bool,
    /// True when the platform should send `DspCmd::Sol950Subslot` (`:10..:40` of a `RAD950-60` RX minute).
    pub subslot_due: bool,
    /// Active `RAD950-60` TX minute (`utc_sec / 60`) and repeated burst `(rf_freq, power, subch_hz, frame)`.
    tx60_min: u32,
    tx60_frame: Option<(Freq10, u8, u16, Frame)>,
    /// Set true only after durable storage confirms a completed contact was written.
    pub saved_ack: bool,
    /// Set true if durable flash log is full (non-overwriting).
    pub log_full: bool,
    /// Most recently completed QSO record (awaiting or confirmed in durable storage).
    pub last_qso: Option<QsoRecord>,
    /// Whether `last_qso` earned a New Grid or New Band Grid award.
    pub last_qso_award: GridAward,
    /// Peer currently targeted by Answer (for Active screen display).
    pub target_peer: Option<Callsign>,
    /// Session-only attended Auto-Answer ("Hunter") filter (never persisted across reboot).
    pub auto_answer: AutoAnswer,
    /// Optional 4-char grid filter when opening Log from Worked Grids drill-down.
    pub log_grid_filter: Option<[u8; 4]>,
    /// True when Setup changed the profile and the firmware should write `SOL950_CFG`.
    pub profile_dirty: bool,
    /// Completed contact waiting for durable SPI flash persistence (`SOL950_LOG`).
    pub pending_save: Option<SavedContact>,
    /// In-memory / mirrored durable contact store for Log, Stats, and Worked Grids.
    pub store: MemSol950Store,
    pub notice: [u8; 28],
    pub notice_len: u8,
    /// Diagnostic counters copied from the DSP task: `[finishes, last_syms, last_tracks, total_decoded]`.
    pub diag: [u32; 4],
    /// Live right-to-left scrolling passband panafall (`224 x 48 px`).
    pub wf: Sol950Waterfall,
    /// Active TX burst subchannel (Hz), encoded 110-symbol tone sequence, and last 200 ms TX waterfall tick.
    tx_subch_hz: u16,
    tx_tones: [u8; ferrite_core::sol950::usol950::phy::N_SYMBOLS],
    tx_wf_tick: u32,
    /// Most recent failed-CRC soft candidate `(slot, SoftCandidate)` for Cooperative RX host capture.
    pub last_soft: Option<(u32, SoftCandidate)>,
    /// Most recent finished receive slot for the Cooperative RX host link.
    pub last_coop: Option<CoopRxReport>,
    /// Conversation extension session state (`docs/sol950-conversation-extension.md`).
    pub chat: crate::rad950_chat::ChatState,
    /// Ad-hoc hourly UTC rendezvous state (`docs/sol950-rendezvous-addendum.md`).
    pub rdv: crate::rad950_rdv::RdvState,
}

impl Default for Sol950Host {
    fn default() -> Self {
        Self::new()
    }
}

impl Sol950Host {
    pub fn new() -> Self {
        let profile = Sol950Profile::new();
        let cfg = Config {
            my_call: Callsign::from_raw(0).unwrap(),
            my_suffix: 0,
            my_grid4: GRID_UNKNOWN,
            tx_pwr: 0,
            mode: OpMode::Monitor,
            windows: [profile.dial(); 8],
            n_windows: 1,
        };
        Self {
            engine: Engine::new(cfg),
            profile,
            utc_sec: 0,
            utc_ms_in_sec: 0,
            utc_uncert_ms: u16::MAX,
            clock_src: ClockSource::None,
            gps_grid: None,
            last_slot: u32::MAX,
            rx_slot_start: 0,
            rx_slot_dial_hz: 0,
            rx_slot_coop_capture: false,
            finish_utc_slot: 0,
            finish_dial_hz: 0,
            finish_coop_capture: false,
            finish_sent_slot: u32::MAX,
            tx_slot: u32::MAX,
            tx_in_flight: false,
            pending_tx: None,
            finish_due: false,
            reset_due: false,
            subslot_due: false,
            tx60_min: u32::MAX,
            tx60_frame: None,
            saved_ack: false,
            log_full: false,
            last_qso: None,
            last_qso_award: GridAward::None,
            target_peer: None,
            auto_answer: AutoAnswer::Off,
            log_grid_filter: None,
            profile_dirty: false,
            pending_save: None,
            store: MemSol950Store::new(),
            notice: [0; 28],
            notice_len: 0,
            diag: [0; 4],
            wf: Sol950Waterfall::new(),
            tx_subch_hz: 1100,
            tx_tones: [0; ferrite_core::sol950::usol950::phy::N_SYMBOLS],
            tx_wf_tick: u32::MAX,
            last_soft: None,
            last_coop: None,
            chat: crate::rad950_chat::ChatState::new(),
            rdv: crate::rad950_rdv::RdvState::new(),
        }
    }

    pub fn utc_ready(&self) -> bool {
        self.utc_sec != 0 && self.utc_uncert_ms <= MAX_TX_UNCERT_MS
    }

    pub fn set_notice(&mut self, s: &[u8]) {
        let n = s.len().min(self.notice.len());
        self.notice[..n].copy_from_slice(&s[..n]);
        self.notice_len = n as u8;
    }

    pub fn notice_bytes(&self) -> &[u8] {
        &self.notice[..self.notice_len as usize]
    }

    /// Push the current profile + effective grid into the underlying `Engine`.
    pub fn sync_engine_config(&mut self) {
        if let Some(c) = self.profile.callsign() {
            self.rdv.set_seed_from_call(c.raw() ^ ((self.profile.suffix as u32) << 24));
        }
        let my_call = self.profile.callsign().unwrap_or_else(|| Callsign::from_raw(0).unwrap());
        let (grid_bytes, src) = self.profile.effective_grid(self.gps_grid);
        let my_grid4 = if src == GridSource::Unset {
            GRID_UNKNOWN
        } else {
            encode_grid4(&grid_bytes).unwrap_or(GRID_UNKNOWN)
        };
        let mut cfg = *self.engine.config();
        cfg.my_call = my_call;
        cfg.my_suffix = self.profile.suffix & 0x0F;
        cfg.my_grid4 = my_grid4;
        cfg.tx_pwr = self.profile.tx_power.min(2);
        cfg.windows[0] = self.profile.dial();
        cfg.n_windows = 1;
        self.engine.set_config(cfg);
    }

    /// Active USB dial (10 Hz units): the negotiated chat QSY dial while a chat is Active, otherwise the calling dial.
    pub fn active_dial(&self) -> Freq10 {
        self.chat.active_chat_dial().unwrap_or_else(|| self.profile.dial())
    }

    /// True when the radio has full GPS PNT (live GPS-disciplined UTC clock + onboard GPS grid).
    pub fn has_pnt(&self) -> bool {
        self.clock_src == ClockSource::Gps && self.utc_ready() && self.gps_grid.is_some()
    }

    /// Update GPS UTC time and onboard grid; trigger `DspCmd::Sol950Finish` at `+9.3 s` and `Engine::begin_slot`
    /// at each 10 s UTC boundary.
    pub fn update_clock(
        &mut self,
        utc_sec: u32,
        ms_in_sec: u16,
        uncert_ms: u16,
        clock_src: ClockSource,
        gps_grid: Option<[u8; 4]>,
    ) {
        self.utc_sec = utc_sec;
        self.utc_ms_in_sec = ms_in_sec;
        self.utc_uncert_ms = uncert_ms;
        self.clock_src = clock_src;
        self.chat.calling_dial_hz10 = self.profile.dial();
        if gps_grid != self.gps_grid {
            self.gps_grid = gps_grid;
            self.sync_engine_config();
        }
        if utc_sec == 0 {
            return;
        }
        let slot = utc_sec / 10;
        let sec_in_slot = utc_sec % 10;
        // While transmitting, push our own live 4-FSK tone trace onto the panafall at 5 Hz (200 ms).
        if self.tx_in_flight {
            let slot_ms = sec_in_slot * 1000 + ms_in_sec as u32;
            let tick = slot.wrapping_mul(50).wrapping_add(slot_ms / 200);
            if tick != self.tx_wf_tick {
                self.tx_wf_tick = tick;
                let sym = (slot_ms / 80) as usize;
                let col = if sym < self.tx_tones.len() {
                    tx_wf_col(self.tx_subch_hz, self.tx_tones[sym])
                } else {
                    [0; WF_COL_BYTES]
                };
                self.wf.push(col, slot_ms < 220, true);
            }
        }
        // `RAD950-60` 1-minute weak-signal mode: 5× repeated 10 s v1 bursts (`:00..:40`) + `:50..:59` cooldown/decode.
        if self.profile.mode60 {
            let minute = slot / 6;
            let sub = slot % 6;
            if sub == 4 && sec_in_slot == 9 && ms_in_sec >= 550 && self.finish_sent_slot != slot {
                self.finish_sent_slot = slot;
                if !self.tx_in_flight && self.tx60_min != minute {
                    self.queue_finish();
                }
            }
            if slot != self.last_slot {
                self.last_slot = slot;
                self.rx_slot_start = utc_sec - sec_in_slot;
                self.rx_slot_dial_hz = self.active_dial().saturating_mul(10);
                self.rx_slot_coop_capture = false;
                let utc_ok = self.utc_ready() && self.profile.callsign().is_some();

                if sub == 5 {
                    // `:50..:59` PA cooldown + decode turnaround guard
                    if self.tx60_min == minute {
                        self.reset_due = true;
                    }
                    self.tx60_frame = None;
                    return;
                }
                if sub == 0 {
                    self.tx60_frame = None;
                    let plan = self.engine.begin_slot(minute, utc_ok);
                    if let SlotPlan::Tx { window, subchannel_hz, frame } = plan {
                        if utc_ok && !self.tx_in_flight {
                            let rf_freq = window.saturating_add(subchannel_hz as u32 / 10);
                            self.tx_subch_hz = subchannel_hz;
                            self.tx_tones = ferrite_core::sol950::usol950::encode_tones(frame);
                            self.pending_tx = Some((rf_freq, self.profile.tx_power, frame));
                            self.tx60_min = minute;
                            self.tx60_frame = Some((rf_freq, self.profile.tx_power, subchannel_hz, frame));
                            self.finish_sent_slot = slot;
                            self.tx_slot = slot;
                        } else {
                            self.engine.on_tx_missed_deadline(minute);
                        }
                    } else {
                        self.reset_due = true;
                    }
                } else if self.tx60_min == minute {
                    // Sub-slots 1..=4 of an active TX minute: repeat exact same v1 burst
                    if let Some((rf_freq, pwr, subch_hz, frame)) = self.tx60_frame {
                        if utc_ok && !self.tx_in_flight {
                            self.tx_subch_hz = subch_hz;
                            self.pending_tx = Some((rf_freq, pwr, frame));
                            self.finish_sent_slot = slot;
                            self.tx_slot = slot;
                        }
                    }
                } else {
                    // Sub-slots 1..=4 of an RX minute: advance Receiver60 sub-slot counter while keeping folded spectrogram
                    self.subslot_due = true;
                }
            }
            return;
        }
        // Finish the RX slot decoder at `+9.55 s` (after the latest valid burst at `start_sym=9` ends at `+9.52 s`).
        // On the 120 MHz M4, 8-track x 3-branch LDPC `finish_slot()` takes ~420 ms, so `Receiver::reset()` +
        // `audio::capture_start()` land right at `+9.97..10.00 s` (aligned with the `0.00 s` UTC slot edge).
        if sec_in_slot == 9 && ms_in_sec >= 550 && self.finish_sent_slot != slot {
            self.finish_sent_slot = slot;
            if !self.tx_in_flight {
                self.queue_finish();
            }
        }
        // Advance the session state machine on each 10 s UTC slot boundary.
        if slot != self.last_slot {
            // Safety net: if the previous slot rolled over before `ms_in_sec >= 940` was polled, still
            // flush the RX decoder once so no slot is ever silently dropped. If the previous slot was a
            // TX slot, reset the DSP receiver cleanly so muted TX-slot audio does not lock out RX acquisition.
            if self.last_slot != u32::MAX && self.last_slot == self.tx_slot {
                self.reset_due = true;
            } else if self.last_slot != u32::MAX && self.finish_sent_slot != self.last_slot && !self.tx_in_flight {
                self.finish_sent_slot = self.last_slot;
                self.queue_finish();
            }
            self.last_slot = slot;
            self.rx_slot_start = utc_sec - sec_in_slot;
            self.rx_slot_dial_hz = self.active_dial().saturating_mul(10);
            self.rx_slot_coop_capture = self.profile.coop_rx == CoopRxMode::Capture;
            let utc_ok = self.utc_ready() && self.profile.callsign().is_some();
            if self.chat.is_open() {
                if let Some(frame) = self.chat.on_slot_start(slot, utc_ok) {
                    if utc_ok && !self.tx_in_flight {
                        let is_distress = ferrite_core::sol950::usol950::distress::decode_distress_frame(frame).is_some();
                        let tx_dial = if is_distress { self.profile.dial() } else { self.active_dial() };
                        let subchannel_hz = if is_distress { 1300u16 } else { self.chat.subch_hz() };
                        let rf_freq = tx_dial.saturating_add(subchannel_hz as u32 / 10);
                        self.tx_subch_hz = subchannel_hz;
                        self.tx_tones = ferrite_core::sol950::usol950::encode_tones(frame);
                        self.pending_tx = Some((rf_freq, self.profile.tx_power, frame));
                        self.finish_sent_slot = slot; // TX slot: never fire spurious RX finish at +9.55 s
                        self.tx_slot = slot;
                    }
                }
                return;
            }
            let clear_mask = !self.engine.occupied_subch_mask();
            let plan = self.engine.begin_slot(slot, utc_ok);
            let has_active_qso =
                matches!(plan, SlotPlan::Tx { .. }) || !matches!(self.engine.mode(), OpMode::Monitor);
            let (new_dial, rdv_cq_hz) =
                self.rdv.on_slot_start(utc_sec, slot, has_active_qso, self.profile.dial_idx, clear_mask);
            if let Some(di) = new_dial {
                self.profile.dial_idx = di;
                self.sync_engine_config();
                self.rx_slot_dial_hz = self.active_dial().saturating_mul(10);
            }
            if let SlotPlan::Tx { window, subchannel_hz, frame } = plan {
                if utc_ok && !self.tx_in_flight {
                    let rf_freq = window.saturating_add(subchannel_hz as u32 / 10);
                    self.tx_subch_hz = subchannel_hz;
                    self.tx_tones = ferrite_core::sol950::usol950::encode_tones(frame);
                    self.pending_tx = Some((rf_freq, self.profile.tx_power, frame));
                    self.finish_sent_slot = slot; // TX slot: never fire spurious RX finish at +9.55 s
                    self.tx_slot = slot;
                } else {
                    self.engine.on_tx_missed_deadline(slot);
                    if !self.utc_ready() {
                        self.set_notice(b"Waiting for GPS UTC");
                    } else if self.profile.callsign().is_none() {
                        self.set_notice(b"Set callsign in Setup");
                    }
                }
            } else if let Some(subch_hz) = rdv_cq_hz {
                if utc_ok && !self.tx_in_flight && self.profile.coop_rx != CoopRxMode::Capture {
                    if let Some(me) = self.profile.callsign() {
                        let (g4_bytes, _) = self.profile.effective_grid(self.gps_grid);
                        let grid4 = ferrite_core::sol950::usol950::frame::encode_grid4(&g4_bytes).unwrap_or(0);
                        let frame = Frame::Cq {
                            sender: me,
                            grid4,
                            tx_pwr: self.profile.tx_power,
                            reserved: (self.profile.suffix & 0x0F) as u16,
                        };
                        let rf_freq = self.profile.dial().saturating_add(subch_hz as u32 / 10);
                        self.tx_subch_hz = subch_hz;
                        self.tx_tones = ferrite_core::sol950::usol950::encode_tones(frame);
                        self.pending_tx = Some((rf_freq, self.profile.tx_power, frame));
                        self.finish_sent_slot = slot;
                        self.tx_slot = slot;
                    }
                }
            }
        }
    }

    fn persist_qso(&mut self, rec: QsoRecord) {
        self.last_qso = Some(rec);
        let mut peer_id = [0u8; 16];
        let mut cb = [0u8; 6];
        let cn = rec.peer.decode(&mut cb);
        peer_id[..cn].copy_from_slice(&cb[..cn]);
        let mut my_id = [0u8; 16];
        let my_n = self.profile.format_identity(&mut my_id);
        let _ = my_n;
        let peer_grid = ferrite_core::sol950::usol950::frame::decode_grid4(rec.peer_grid4).unwrap_or(*b"----");
        let (my_grid, my_grid_src) = self.profile.effective_grid(self.gps_grid);
        // Classify award BEFORE saving so we know whether this QSO earned a New Grid / New Band Grid.
        let pre = self.store.classify_peer(&cb[..cn], peer_grid, rec.window, false);
        self.last_qso_award = pre.award;
        let contact = SavedContact {
            utc_sec: self.utc_sec,
            dial_hz10: rec.window,
            peer_id,
            my_id,
            peer_grid,
            my_grid,
            rx_snr_db: rec.sent_snr,
            tx_snr_db: rec.rcvd_snr,
            my_grid_src,
            slot: rec.completed_slot,
            seq: 0,
        };
        match self.store.save_contact(&contact) {
            SaveOutcome::Saved(_) => {
                self.pending_save = Some(contact);
                self.saved_ack = true;
                self.log_full = self.store.is_full();
            }
            SaveOutcome::Duplicate => {
                self.saved_ack = true;
                self.log_full = self.store.is_full();
            }
            SaveOutcome::LogFull => {
                self.saved_ack = false;
                self.log_full = true;
                self.set_notice(b"Log full - not saved");
            }
            SaveOutcome::Error => {
                self.saved_ack = false;
                self.set_notice(b"Save failed");
            }
        }
        // In attended Auto-Answer (Hunter) or Rendezvous Call & Listen mode, release the completed peer
        // so we return to Monitor and can hunt or answer the next station.
        if (self.auto_answer != AutoAnswer::Off || self.rdv.call_listen_armed) && self.target_peer == Some(rec.peer) {
            self.target_peer = None;
            self.engine.stop_session();
        }
    }

    pub fn on_rx_frame(&mut self, d: Decoded) {
        let slot = if self.last_slot == u32::MAX {
            0
        } else if self.profile.mode60 {
            self.last_slot / 6
        } else {
            self.last_slot
        };
        if self.chat.on_rx_frame(d.frame, slot) {
            return;
        }
        let mut logs = [None; 4];
        let w = self.profile.dial();
        let n = self.engine.on_rx_frames(slot, w, &[d], &mut logs);
        if n > 0 {
            if let Some(rec) = logs[0] {
                self.persist_qso(rec);
            }
        }
        // When someone answers our Rendezvous CQ (or calls us directly while Hunter is armed) with RX(ACK=0),
        // promote out of Monitor to send R+RX (ACK=1) and complete the QSO.
        if (self.rdv.call_listen_armed || self.auto_answer != AutoAnswer::Off)
            && self.profile.coop_rx != CoopRxMode::Capture
            && matches!(self.engine.mode(), OpMode::Monitor)
            && self.target_peer.is_none()
        {
            if let Frame::Rx { sender, recipient, ack: false, .. } = d.frame {
                let my_call = self.engine.config().my_call;
                if recipient == my_call && sender != my_call && self.engine.answer_peer(sender, slot) {
                    self.saved_ack = false;
                    self.target_peer = Some(sender);
                    return;
                }
            }
        }
        // Attended Auto-Answer (Hunter / Pounce): when armed and idle, auto-answer needed CQs.
        if self.auto_answer != AutoAnswer::Off
            && self.profile.coop_rx != CoopRxMode::Capture
            && matches!(self.engine.mode(), OpMode::Monitor)
            && self.target_peer.is_none()
        {
            if let Frame::Cq { sender, grid4, .. } = d.frame {
                let my_call = self.engine.config().my_call;
                if sender != my_call {
                    let mut cb = [0u8; 6];
                    let cn = sender.decode(&mut cb);
                    let peer_grid = ferrite_core::sol950::usol950::frame::decode_grid4(grid4).unwrap_or(*b"----");
                    let st = self.store.classify_peer(&cb[..cn], peer_grid, w, false);
                    if self.auto_answer.matches(st) && self.engine.answer_peer(sender, slot) {
                        self.saved_ack = false;
                        self.target_peer = Some(sender);
                    }
                }
            }
        }
    }

    pub fn on_tx_done(&mut self, ok: bool) {
        self.tx_in_flight = false;
        if self.last_slot != u32::MAX && self.last_slot != self.tx_slot {
            self.reset_due = true;
        }
        if self.chat.is_open() {
            self.chat.on_tx_done(ok);
            return;
        }
        let slot = if self.last_slot == u32::MAX {
            0
        } else if self.profile.mode60 {
            if self.last_slot % 6 != 4 {
                // Only finalize the 60 s TX minute after the 5th repeated sub-burst (:40) completes.
                return;
            }
            self.last_slot / 6
        } else {
            self.last_slot
        };
        if ok {
            if let Some(rec) = self.engine.confirm_tx_completed(slot) {
                self.persist_qso(rec);
            }
        } else {
            self.engine.on_tx_missed_deadline(slot);
            self.set_notice(b"TX aborted");
        }
    }

    /// Whether this radio is parked in receive-only Cooperative RX capture for the browser host.
    pub fn coop_bluetooth_armed(&self) -> bool {
        self.profile.coop_rx == CoopRxMode::Capture
            && self.profile.coop_link == CoopLink::Bluetooth
            && matches!(self.engine.mode(), OpMode::Monitor)
            && !self.tx_in_flight
    }

    /// Freeze the current receive slot's association metadata when its DSP decode is scheduled.
    fn queue_finish(&mut self) {
        self.finish_utc_slot = self.rx_slot_start;
        self.finish_dial_hz = self.rx_slot_dial_hz;
        self.finish_coop_capture = self.rx_slot_coop_capture;
        self.finish_due = true;
    }
}

impl App {
    /// Attach the platform's SOL950 runtime host (firmware static or simulator heap).
    pub fn sol950_attach(&mut self, host: &'static mut Sol950Host) {
        if host.profile.call_len == 0 && self.set.callsign.0[0] != 0 {
            let end = self.set.callsign.0.iter().position(|&b| b == 0).unwrap_or(self.set.callsign.0.len());
            host.profile.set_base_call(&self.set.callsign.0[..end]);
        }
        host.sync_engine_config();
        self.sol950d = Some(host);
    }

    pub fn sol950_host(&self) -> Option<&Sol950Host> {
        self.sol950d.as_deref()
    }

    pub fn sol950_host_mut(&mut self) -> Option<&mut Sol950Host> {
        self.sol950d.as_deref_mut()
    }

    /// True while the SOL950 screen is open or underneath a child screen.
    pub fn sol950_open(&self) -> bool {
        matches!(self.screen, Screen::Sol950(_)) || self.back.iter().flatten().any(|s| matches!(s, Screen::Sol950(_)))
    }

    pub fn sol950_wants_gps_grid(&self) -> bool {
        self.sol950d.as_ref().is_none_or(|h| h.profile.use_gps_grid)
    }

    /// Called by `activity::start_cmds(Mode::Sol950)`.
    pub(crate) fn sol950_start(&mut self) {
        let (dial, rx_corr_hz) = self
            .sol950d
            .as_ref()
            .map_or((SOL950_DIALS[0], 0), |h| (h.profile.dial(), h.profile.rx_corr_hz));
        self.out.push(RadioCmd::Sol950Rx { dial, rx_corr_hz, on: true });
    }

    /// Called by `activity::stop_mode(Mode::Sol950)`.
    pub(crate) fn sol950_stop(&mut self) {
        if let Some(h) = self.sol950d.as_deref_mut() {
            h.engine.stop_session();
            h.pending_tx = None;
            h.tx_in_flight = false;
        }
        self.out.push(RadioCmd::Sol950Rx { dial: 0, rx_corr_hz: 0, on: false });
    }

    /// Retune the SI4732 USB receiver (for Setup dial change, Chat QSY on Accept, or return on End).
    pub(crate) fn sol950_retune(&mut self) {
        if self.radio.mode == crate::screens::activity::Mode::Sol950 {
            let (dial, rx_corr_hz) = self
                .sol950d
                .as_ref()
                .map_or((SOL950_DIALS[0], 0), |h| (h.active_dial(), h.profile.rx_corr_hz));
            self.out.push(RadioCmd::Sol950Rx { dial, rx_corr_hz, on: true });
        }
    }

    /// Take any pending TX burst staged for the current slot (`(rf_freq, power, frame)`).
    pub fn sol950_take_tx(&mut self) -> Option<(Freq10, u8, Frame)> {
        let h = self.sol950d.as_deref_mut()?;
        let tx = h.pending_tx.take()?;
        h.tx_in_flight = true;
        Some(tx)
    }

    /// Take the `DspCmd::Sol950Finish` trigger at the end of a 10 s RX slot, preserving the slot identity for
    /// Cooperative RX telemetry even while the decode finishes during the following guard interval.
    pub fn sol950_take_finish(&mut self) -> Option<(u32, u32, bool)> {
        let h = self.sol950d.as_deref_mut()?;
        if !core::mem::take(&mut h.finish_due) {
            return None;
        }
        let coop_capture = h.finish_coop_capture
            && h.coop_bluetooth_armed()
            && h.utc_ready()
            && h.profile.dial().saturating_mul(10) == h.finish_dial_hz;
        Some((h.finish_utc_slot, h.finish_dial_hz, coop_capture))
    }

    /// Take the `DspCmd::Sol950Reset` trigger after a TX slot so the DSP receiver restarts cleanly at 0.00 s.
    pub fn sol950_take_reset(&mut self) -> bool {
        self.sol950d.as_deref_mut().is_some_and(|h| core::mem::take(&mut h.reset_due))
    }

    /// Take the `DspCmd::Sol950Subslot` trigger for `:10..:40` sub-slots inside a `RAD950-60` RX minute.
    pub fn sol950_take_subslot(&mut self) -> bool {
        self.sol950d.as_deref_mut().is_some_and(|h| core::mem::take(&mut h.subslot_due))
    }

    /// Whether `RAD950-60` (1-minute weak-signal cycle) is currently selected.
    pub fn sol950_mode60(&self) -> bool {
        self.sol950d.as_ref().is_some_and(|h| h.profile.mode60)
    }

    /// Maximum sleep duration (ms) for the UI task while SOL950 is active, so `DspCmd::Sol950Finish`
    /// (`+9.94 s`) and `Sol950Tx` (`0.00 s`) fire within `<= 10 ms` even when the display is dark.
    pub fn sol950_next_wake_ms(&self) -> Option<u32> {
        if !self.sol950_open() {
            return None;
        }
        let h = self.sol950d.as_deref()?;
        if h.utc_sec == 0 {
            return Some(250);
        }
        let slot_ms = (h.utc_sec % 10) * 1000 + h.utc_ms_in_sec as u32;
        if !(50..9_500).contains(&slot_ms) {
            Some(10)
        } else {
            Some(9_500 - slot_ms)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_host(call: &[u8], grid: [u8; 4], dial_idx: u8) -> Sol950Host {
        let mut h = Sol950Host::new();
        h.profile.set_base_call(call);
        h.profile.manual_grid = grid;
        h.profile.use_gps_grid = false;
        h.profile.dial_idx = dial_idx;
        h.sync_engine_config();
        h
    }

    /// Run `slots` 10-second UTC slots across `hosts`, delivering every completed TX burst
    /// to all other hosts currently tuned to the same dial frequency.
    fn run_ether(hosts: &mut [Sol950Host], start_slot: u32, slots: u32, trace: bool) {
        for s in start_slot..(start_slot + slots) {
            let utc_sec = s * 10;
            // Start of 10s slot: advance clock and collect any scheduled TX bursts.
            let mut tx_bursts: [Option<(usize, Freq10, u16, Frame)>; 4] = [None; 4];
            for (idx, h) in hosts.iter_mut().enumerate() {
                h.update_clock(utc_sec, 0, 10, ClockSource::Gps, None);
                if let Some((_rf, _pwr, frame)) = h.pending_tx.take() {
                    h.tx_in_flight = true;
                    if trace {
                        std::eprintln!("  slot {s} host[{idx}] dial={} TX {:?}", h.profile.dial(), frame);
                    }
                    tx_bursts[idx] = Some((idx, h.profile.dial(), h.tx_subch_hz, frame));
                }
            }
            // End of 10s slot: TX stations finish TX, RX stations decode all bursts on their dial.
            for (rx_idx, h) in hosts.iter_mut().enumerate() {
                if h.tx_in_flight {
                    h.on_tx_done(true);
                } else {
                    let rx_dial = h.profile.dial();
                    for b in tx_bursts.iter().flatten() {
                        let (tx_idx, tx_dial, subch_hz, frame) = *b;
                        if tx_idx != rx_idx && tx_dial == rx_dial {
                            h.on_rx_frame(Decoded {
                                frame,
                                audio_hz: subch_hz,
                                snr_db: -6,
                                time_offset_ms: 0,
                            });
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn three_station_hunter_modes_and_rendezvous() {
        // Scenario 1: Same grid (EN61), 2 Auto-CQ callers (W2BBB, N3CCC) + 1 Hunter in AnyNeeded (K1AAA).
        // Expected: K1AAA logs 2 QSOs (W2BBB + N3CCC), zero dupes.
        {
            let mut hosts = [
                make_host(b"K1AAA", *b"EN61", 0),
                make_host(b"W2BBB", *b"EN61", 0),
                make_host(b"N3CCC", *b"EN61", 0),
            ];
            hosts[0].auto_answer = AutoAnswer::AnyNeeded;
            hosts[1].engine.start_cq(0);
            hosts[2].engine.start_cq(0);
            run_ether(&mut hosts, 100, 30, false);
            std::eprintln!("Scenario 1 (AnyNeeded, 10m): counts = [{}, {}, {}]", hosts[0].store.count(), hosts[1].store.count(), hosts[2].store.count());
            assert_eq!(hosts[0].store.count(), 2, "AnyNeeded should work both new calls in EN61");
            assert_eq!(hosts[1].store.count(), 1, "W2BBB should log K1AAA once");
            assert_eq!(hosts[2].store.count(), 1, "N3CCC should log K1AAA once");
        }

        // Scenario 2: Same grid (EN61), Hunter in NewBandGrid across 10m then 12m.
        // Expected: 1 QSO on 10m (EN61 now worked on 10m), then +1 QSO on 12m (EN61 new on 12m) = 2 total.
        {
            let mut hosts = [
                make_host(b"K1AAA", *b"EN61", 0),
                make_host(b"W2BBB", *b"EN61", 0),
                make_host(b"N3CCC", *b"EN61", 0),
            ];
            hosts[0].auto_answer = AutoAnswer::NewBandGrid;
            hosts[1].engine.start_cq(0);
            hosts[2].engine.start_cq(0);
            run_ether(&mut hosts, 100, 20, false);
            std::eprintln!("Scenario 2a (NewBandGrid, 10m): counts = [{}, {}, {}]", hosts[0].store.count(), hosts[1].store.count(), hosts[2].store.count());
            assert_eq!(hosts[0].store.count(), 1, "NewBandGrid should work only 1 EN61 station on 10m");
            // Hop all 3 stations to 12m (SOL950_DIALS[4] = 24.919 MHz).
            for h in &mut hosts {
                h.profile.dial_idx = 4;
                h.sync_engine_config();
            }
            run_ether(&mut hosts, 120, 20, false);
            std::eprintln!("Scenario 2b (NewBandGrid, 12m): counts = [{}, {}, {}]", hosts[0].store.count(), hosts[1].store.count(), hosts[2].store.count());
            assert_eq!(hosts[0].store.count(), 2, "NewBandGrid should work 1 EN61 station on 12m");
        }

        // Scenario 3: Same grid (EN61), Hunter in NewGrid across 10m then 12m.
        // Expected: 1 QSO on 10m, then 0 more on 12m (EN61 already worked all-time) = 1 total.
        {
            let mut hosts = [
                make_host(b"K1AAA", *b"EN61", 0),
                make_host(b"W2BBB", *b"EN61", 0),
                make_host(b"N3CCC", *b"EN61", 0),
            ];
            hosts[0].auto_answer = AutoAnswer::NewGrid;
            hosts[1].engine.start_cq(0);
            hosts[2].engine.start_cq(0);
            run_ether(&mut hosts, 100, 20, false);
            assert_eq!(hosts[0].store.count(), 1, "NewGrid should work 1 EN61 station on 10m");
            for h in &mut hosts {
                h.profile.dial_idx = 4;
                h.sync_engine_config();
            }
            run_ether(&mut hosts, 120, 20, false);
            std::eprintln!("Scenario 3 (NewGrid, 10m->12m): counts = [{}, {}, {}]", hosts[0].store.count(), hosts[1].store.count(), hosts[2].store.count());
            assert_eq!(hosts[0].store.count(), 1, "NewGrid should NOT re-work EN61 on 12m");
        }

        // Scenario 4: 3 distinct grids (EN61, FN31, EM79), all 3 in Rendezvous + Hunter AnyNeeded during 10m window (:00-:04).
        {
            let mut hosts = [
                make_host(b"K1AAA", *b"EN61", 0),
                make_host(b"W2BBB", *b"FN31", 0),
                make_host(b"N3CCC", *b"EM79", 0),
            ];
            for h in &mut hosts {
                h.auto_answer = AutoAnswer::AnyNeeded;
                h.rdv.follow_schedule = true;
                h.rdv.call_listen_armed = true;
                h.sync_engine_config();
            }
            // Start at exact top of hour (slot 360 = 3600s = 01:00:00Z, 10m Rendezvous window = 30 slots).
            run_ether(&mut hosts, 360, 30, false);
            let counts = [hosts[0].store.count(), hosts[1].store.count(), hosts[2].store.count()];
            std::eprintln!("Scenario 4 (3x Rendezvous + HUNT:ALL, 30 slots): logged counts = {:?}", counts);
            assert_eq!(counts, [2, 2, 2], "All 3 distinct stations should complete all 3 unique pairs (2 QSOs each)");
        }
    }
}
