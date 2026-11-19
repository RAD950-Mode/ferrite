//! SOL950 10 m weak-signal runtime host attached to [`App`] (analogous to [`crate::sat`]).
//!
//! Keeps [`App`]'s RAM footprint small (`8` bytes for `Option<&'static mut Sol950Host>`) while owning
//! the [`ferrite_core::sol950::Engine`], station profile, GPS UTC discipline state, and TX staging.

use crate::{App, Screen};
use ferrite_core::freq::Freq10;
use ferrite_core::msg::RadioCmd;
use ferrite_core::sol950::usol950::frame::encode_grid4;
use ferrite_core::sol950::usol950::{Callsign, Decoded, Frame, GRID_UNKNOWN};
use ferrite_core::sol950::{Config, Engine, OpMode, QsoRecord, SlotPlan};

/// Nominal 10 m USB dial windows for SOL950 (`28.078`, `28.082`, `28.120`, `28.180` MHz).
pub const DIALS_10M: [Freq10; 4] = [2_807_800, 2_808_200, 2_812_000, 2_818_000];

/// Maximum GPS UTC uncertainty (ms) permitted to transmit on a 10 s slot edge (`docs/sol950-firmware-feature.md` §3).
pub const MAX_TX_UNCERT_MS: u16 = 150;

/// Effective grid source displayed in the SOL950 header and saved with contacts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GridSource {
    Gps,
    Manual,
    Unset,
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
        }
    }

    pub fn dial(&self) -> Freq10 {
        DIALS_10M[self.dial_idx as usize % DIALS_10M.len()]
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

/// Platform-owned SOL950 runtime state attached to [`App`] via [`App::sol950_attach`].
pub struct Sol950Host {
    pub engine: Engine,
    pub profile: Sol950Profile,
    /// Latest GPS UTC second and millisecond within that second.
    pub utc_sec: u32,
    pub utc_ms_in_sec: u16,
    /// Estimated UTC timing uncertainty in ms (`u16::MAX` when unsynchronized).
    pub utc_uncert_ms: u16,
    /// Latest onboard GPS 4-char grid (never from fixed position `0 6 9`).
    pub gps_grid: Option<[u8; 4]>,
    pub last_slot: u32,
    pub finish_sent_slot: u32,
    /// A live SOL950 TX burst is currently keyed on BK4829.
    pub tx_in_flight: bool,
    /// Next frame staged by `Engine::on_slot_start` for the platform to hand to `SOL950_TX` + `RadioCmd::Sol950Tx`.
    pub pending_tx: Option<(Freq10, u8, Frame)>,
    /// True when the platform should send `DspCmd::Sol950Finish` at the end of the active RX slot.
    pub finish_due: bool,
    /// Set true only after durable storage confirms a completed contact was written.
    pub saved_ack: bool,
    /// Set true if durable flash log is full (non-overwriting).
    pub log_full: bool,
    /// Most recently completed QSO record (awaiting or confirmed in durable storage).
    pub last_qso: Option<QsoRecord>,
    /// Peer currently targeted by Answer (for Active screen display).
    pub target_peer: Option<Callsign>,
    pub notice: [u8; 28],
    pub notice_len: u8,
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
            my_call: Callsign::encode(b"N0CALL").unwrap(),
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
            gps_grid: None,
            last_slot: u32::MAX,
            finish_sent_slot: u32::MAX,
            tx_in_flight: false,
            pending_tx: None,
            finish_due: false,
            saved_ack: false,
            log_full: false,
            last_qso: None,
            target_peer: None,
            notice: [0; 28],
            notice_len: 0,
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
        let my_call = self.profile.callsign().unwrap_or_else(|| Callsign::encode(b"N0CALL").unwrap());
        let (grid_bytes, src) = self.profile.effective_grid(self.gps_grid);
        let my_grid4 = if src == GridSource::Unset {
            GRID_UNKNOWN
        } else {
            encode_grid4(&grid_bytes).unwrap_or(GRID_UNKNOWN)
        };
        let mut cfg = *self.engine.config();
        cfg.my_call = my_call;
        cfg.my_grid4 = my_grid4;
        cfg.tx_pwr = self.profile.tx_power.min(2);
        cfg.windows[0] = self.profile.dial();
        cfg.n_windows = 1;
        self.engine.set_config(cfg);
    }

    /// Update GPS UTC time and onboard grid; trigger `DspCmd::Sol950Finish` at `+9.3 s` and `Engine::begin_slot`
    /// at each 10 s UTC boundary.
    pub fn update_clock(&mut self, utc_sec: u32, ms_in_sec: u16, uncert_ms: u16, gps_grid: Option<[u8; 4]>) {
        self.utc_sec = utc_sec;
        self.utc_ms_in_sec = ms_in_sec;
        self.utc_uncert_ms = uncert_ms;
        if gps_grid != self.gps_grid {
            self.gps_grid = gps_grid;
            self.sync_engine_config();
        }
        if utc_sec == 0 {
            return;
        }
        let slot = utc_sec / 10;
        let sec_in_slot = utc_sec % 10;
        // Finish the RX slot decoder inside the 1.20 s inter-slot guard (after +9.20 s).
        if sec_in_slot == 9 && ms_in_sec >= 300 && self.finish_sent_slot != slot {
            self.finish_sent_slot = slot;
            if !self.tx_in_flight {
                self.finish_due = true;
            }
        }
        // Advance the session state machine on each 10 s UTC slot boundary.
        if slot != self.last_slot {
            self.last_slot = slot;
            let utc_ok = self.utc_ready() && self.profile.callsign().is_some();
            let plan = self.engine.begin_slot(slot, utc_ok);
            if let SlotPlan::Tx { window, subchannel_hz, frame } = plan {
                if utc_ok && !self.tx_in_flight {
                    let rf_freq = window.saturating_add(subchannel_hz as u32 / 10);
                    self.pending_tx = Some((rf_freq, self.profile.tx_power, frame));
                } else {
                    self.engine.on_tx_missed_deadline(slot);
                    if !self.utc_ready() {
                        self.set_notice(b"Waiting for GPS UTC");
                    } else if self.profile.callsign().is_none() {
                        self.set_notice(b"Set callsign in Setup");
                    }
                }
            }
        }
    }

    pub fn on_rx_frame(&mut self, d: Decoded) {
        let mut logs = [None; 4];
        let w = self.profile.dial();
        let slot = if self.last_slot == u32::MAX { 0 } else { self.last_slot };
        let n = self.engine.on_rx_frames(slot, w, &[d], &mut logs);
        if n > 0 {
            if let Some(rec) = logs[0] {
                self.last_qso = Some(rec);
                self.saved_ack = false;
            }
        }
    }

    pub fn on_tx_done(&mut self, ok: bool) {
        self.tx_in_flight = false;
        let slot = if self.last_slot == u32::MAX { 0 } else { self.last_slot };
        if ok {
            if let Some(rec) = self.engine.confirm_tx_completed(slot) {
                self.last_qso = Some(rec);
                self.saved_ack = false;
            }
        } else {
            self.engine.on_tx_missed_deadline(slot);
            self.set_notice(b"TX aborted");
        }
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
            .map_or((DIALS_10M[0], 0), |h| (h.profile.dial(), h.profile.rx_corr_hz));
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

    /// Retune the SI4732 USB receiver if the user changed the dial or RX offset in Setup.
    pub(crate) fn sol950_retune(&mut self) {
        if self.radio.mode == crate::screens::activity::Mode::Sol950 {
            let (dial, rx_corr_hz) = self
                .sol950d
                .as_ref()
                .map_or((DIALS_10M[0], 0), |h| (h.profile.dial(), h.profile.rx_corr_hz));
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

    /// Take the `DspCmd::Sol950Finish` trigger at the end of a 10 s RX slot.
    pub fn sol950_take_finish(&mut self) -> bool {
        let Some(h) = self.sol950d.as_deref_mut() else { return false };
        core::mem::take(&mut h.finish_due)
    }
}
