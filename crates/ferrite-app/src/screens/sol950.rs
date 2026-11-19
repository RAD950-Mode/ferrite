//! SOL950 10 m weak-signal mode UI (`MENU 0 3 7`):
//! - **Operate** (`page = 0`): station header, UTC/GPS readiness, heard-stations activity list, Answer / Call CQ / Stop.
//! - **Active** (`page = 1`): in-progress CQ or QSO with plain-language next action and `Saved` status.
//! - **Setup** (`page = 2`): callsign, optional prefix/suffix (`1..=15`), GPS/manual grid, USB dial, signed RX offset, TX power, live preview.

use super::shared::*;
use super::{Cx, Hints, View};
use crate::chan::ChannelStore;
use crate::keys::{KeyEvent, Press};
use crate::sol950::{field_letters, GridSource, SavedContact, Sol950Store, DIALS_10M};
use crate::{App, Screen};
use ferrite_core::freq::Freq10;
use ferrite_core::keys::Key;
use ferrite_core::msg::{LiveAll, ModeLive, RadioCmd};
use ferrite_core::sol950::usol950::frame::decode_grid4;
use ferrite_core::sol950::usol950::Callsign;
use ferrite_core::sol950::{ActivityRow, OpMode, MAX_PEERS};
use ferrite_ui::assets::*;
use ferrite_ui::gfx::*;
use ferrite_ui::panel::{Panel, W};
use ferrite_ui::text::Buf;
use ferrite_ui::widgets as w;
use ufmt::uwrite;

pub const PAGE_OPERATE: u8 = 0;
pub const PAGE_ACTIVE: u8 = 1;
pub const PAGE_SETUP: u8 = 2;
pub const PAGE_LOG: u8 = 3;
pub const PAGE_LOG_DETAIL: u8 = 4;
pub const PAGE_STATS: u8 = 5;
pub const PAGE_GRIDS_OVERVIEW: u8 = 6;
pub const PAGE_GRIDS_FIELD: u8 = 7;

const OPERATE_ROWS: usize = 5;
const LOG_ROWS: usize = 6;
const SETUP_ROWS: usize = 8;
const TOP_Y: i32 = w::SBAR_H;

/// SOL950 screen state (`8` bytes, fits inside `Screen` without growing it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Sol950Scr {
    pub page: u8,
    pub sel: u8,
    pub top: u8,
    pub field: u16,
    pub sub: u8,
}

impl Sol950Scr {
    pub const OPERATE: Self = Self { page: PAGE_OPERATE, sel: 0, top: 0, field: 0, sub: 0 };
    pub const ACTIVE: Self = Self { page: PAGE_ACTIVE, sel: 0, top: 0, field: 0, sub: 0 };
    pub const SETUP: Self = Self { page: PAGE_SETUP, sel: 0, top: 0, field: 0, sub: 0 };
    pub const LOG: Self = Self { page: PAGE_LOG, sel: 0, top: 0, field: 0, sub: 0 };
    pub const STATS: Self = Self { page: PAGE_STATS, sel: 0, top: 0, field: 0, sub: 0 };
    /// Default cursor on `FN` (`lon_f = 5, lat_f = 13 -> 5 * 18 + 13 = 103`).
    pub const GRIDS: Self = Self { page: PAGE_GRIDS_OVERVIEW, sel: 0, top: 0, field: 103, sub: 31 };
}

/// `MENU 0 3 7`: open SOL950; refused while transmitting or repeating.
pub fn open(app: &mut App) -> Screen {
    if app.radio.keyed.is_some() || app.radio.mode == super::activity::Mode::Xb {
        app.show_toast(
            b"SOL950",
            if app.radio.keyed.is_some() { b"not keyed" } else { b"stop repeater first" },
            false,
        );
        return app.screen;
    }
    Screen::Sol950(Sol950Scr::OPERATE)
}

pub fn page_id(s: &Sol950Scr) -> &'static str {
    match s.page {
        PAGE_OPERATE => "sol950.operate",
        PAGE_ACTIVE => "sol950.active",
        PAGE_SETUP => "sol950.setup",
        PAGE_LOG => "sol950.log",
        PAGE_LOG_DETAIL => "sol950.log_detail",
        PAGE_STATS => "sol950.stats",
        PAGE_GRIDS_OVERVIEW => "sol950.grids",
        PAGE_GRIDS_FIELD => "sol950.grids_field",
        _ => "sol950.operate",
    }
}

fn push_khz3<const N: usize>(b: &mut Buf<N>, dial: Freq10) {
    let mhz_i = dial / 100_000;
    let khz_i = (dial % 100_000) / 100;
    let _ = uwrite!(b, "{}.", mhz_i);
    if khz_i < 100 {
        b.push(b"0");
    }
    if khz_i < 10 {
        b.push(b"0");
    }
    let _ = uwrite!(b, "{}", khz_i);
}

fn empty_row() -> ActivityRow {
    ActivityRow {
        peer: Callsign::from_raw(0).unwrap(),
        grid4: ferrite_core::sol950::usol950::GRID_UNKNOWN,
        snr_db: 0,
        last_seen_slot: 0,
        window: 0,
        owed_reply: None,
        logged: false,
    }
}

fn plain_qso_action(mode: OpMode, owed: Option<bool>, logged: bool, tx_active: bool, saved_ack: bool) -> &'static [u8] {
    if tx_active {
        return match owed {
            Some(false) => b"Sending report",
            Some(true) => b"Sending R+RX",
            None => b"Transmitting CQ",
        };
    }
    if logged {
        return if saved_ack { b"Saved to flash log" } else { b"Complete - saving" };
    }
    match (mode, owed) {
        (OpMode::Monitor, _) => b"Listening (RX only)",
        (_, Some(false)) => b"Sending report next slot",
        (_, Some(true)) => b"Sending R+RX next slot",
        (OpMode::AutoCq { .. }, None) => b"Calling CQ next slot",
        (OpMode::Pounce, None) => b"Waiting for reply",
    }
}

impl View for Sol950Scr {
    fn name(self) -> &'static str {
        match self.page {
            PAGE_ACTIVE => "SOL950 QSO",
            PAGE_SETUP => "SOL950 Setup",
            PAGE_LOG | PAGE_LOG_DETAIL => "SOL950 Log",
            PAGE_STATS => "SOL950 Stats",
            PAGE_GRIDS_OVERVIEW | PAGE_GRIDS_FIELD => "SOL950 Grids",
            _ => "SOL950",
        }
    }

    fn title(self, _app: &App, b: &mut Buf<10>) {
        let t: &[u8] = match self.page {
            PAGE_ACTIVE => b"SOL QSO",
            PAGE_SETUP => b"SOL SET",
            PAGE_LOG | PAGE_LOG_DETAIL => b"SOL LOG",
            PAGE_STATS => b"SOL STAT",
            PAGE_GRIDS_OVERVIEW | PAGE_GRIDS_FIELD => b"SOL MAP",
            _ => b"SOL950",
        };
        b.set(t);
    }

    fn own_arrows(self) -> bool {
        true
    }

    fn key(self, app: &mut App, e: KeyEvent, store: &impl ChannelStore) {
        if !matches!(e.press, Press::Short | Press::Repeat) {
            return;
        }
        match self.page {
            PAGE_OPERATE => self.key_operate(app, e.key),
            PAGE_ACTIVE => self.key_active(app, e.key),
            PAGE_SETUP => self.key_setup(app, e.key, store),
            PAGE_LOG => self.key_log(app, e.key),
            PAGE_LOG_DETAIL => self.key_log_detail(app, e.key),
            PAGE_STATS => self.key_stats(app, e.key),
            PAGE_GRIDS_OVERVIEW => self.key_grids_overview(app, e.key),
            PAGE_GRIDS_FIELD => self.key_grids_field(app, e.key),
            _ => app.screen = Screen::Sol950(Sol950Scr::OPERATE),
        }
    }

    fn knob(self, app: &mut App, delta: i32, store: &impl ChannelStore) {
        if delta == 0 {
            return;
        }
        let k = if delta > 0 { Key::Down } else { Key::Up };
        for _ in 0..delta.unsigned_abs().min(8) {
            self.key(app, KeyEvent { key: k, press: Press::Short }, store);
        }
    }

    fn render(self, app: &mut App, lcd: &mut impl Panel, cx: &Cx<impl ChannelStore>) {
        app.new_page(lcd, self.page as u32);
        match self.page {
            PAGE_OPERATE => self.draw_operate(app, lcd, cx.live),
            PAGE_ACTIVE => self.draw_active(app, lcd, cx.live),
            PAGE_SETUP => self.draw_setup(app, lcd),
            PAGE_LOG => self.draw_log(app, lcd),
            PAGE_LOG_DETAIL => self.draw_log_detail(app, lcd),
            PAGE_STATS => self.draw_stats(app, lcd),
            PAGE_GRIDS_OVERVIEW => self.draw_grids_overview(app, lcd),
            PAGE_GRIDS_FIELD => self.draw_grids_field(app, lcd),
            _ => self.draw_operate(app, lcd, cx.live),
        }
    }

    fn hints(self, _app: &App) -> Hints {
        match self.page {
            PAGE_OPERATE => Hints::new(&[("OK", "Answer"), ("1", "CQ"), ("2-4", "Log/Map")]),
            PAGE_ACTIVE => Hints::new(&[("0", "Stop"), ("*", "Setup"), ("BACK", "List")]),
            PAGE_SETUP => Hints::new(&[(ARROWS_LR, "Change"), ("BACK", "Done")]),
            PAGE_LOG => Hints::new(&[("OK", "Detail"), ("BACK", "Back")]),
            PAGE_LOG_DETAIL => Hints::new(&[("BACK", "Log")]),
            PAGE_STATS => Hints::new(&[(ARROWS_LR, "Scope"), ("BACK", "Back")]),
            PAGE_GRIDS_OVERVIEW => Hints::new(&[(ARROWS_4, "Field"), ("OK", "Zoom"), ("BACK", "Back")]),
            PAGE_GRIDS_FIELD => Hints::new(&[(ARROWS_4, "Grid"), ("OK", "Log"), ("BACK", "Map")]),
            _ => Hints::new(&[("BACK", "Back")]),
        }
    }
}

impl Sol950Scr {
    fn key_operate(mut self, app: &mut App, k: Key) {
        let mut rows = [empty_row(); MAX_PEERS];
        let n = app.sol950_host().map_or(0, |h| h.engine.activity_rows(&mut rows));
        match k {
            Key::Exit => {
                app.back();
                return;
            }
            Key::Up => {
                if n > 0 {
                    self.sel = if self.sel == 0 { (n - 1) as u8 } else { self.sel - 1 };
                    if (self.sel as usize) < self.top as usize {
                        self.top = self.sel;
                    } else if (self.sel as usize) >= self.top as usize + OPERATE_ROWS {
                        self.top = (self.sel as usize + 1 - OPERATE_ROWS) as u8;
                    }
                }
            }
            Key::Down => {
                if n > 0 {
                    self.sel = ((self.sel as usize + 1) % n) as u8;
                    if (self.sel as usize) < self.top as usize {
                        self.top = self.sel;
                    } else if (self.sel as usize) >= self.top as usize + OPERATE_ROWS {
                        self.top = (self.sel as usize + 1 - OPERATE_ROWS) as u8;
                    }
                }
            }
            Key::Menu => {
                if let Some(row) = rows.get(self.sel as usize).filter(|_| (self.sel as usize) < n) {
                    let peer = row.peer;
                    if let Some(h) = app.sol950_host_mut() {
                        let slot = if h.last_slot == u32::MAX { 0 } else { h.last_slot };
                        h.saved_ack = false;
                        h.target_peer = Some(peer);
                        h.engine.answer_peer(peer, slot);
                    }
                    app.screen = Screen::Sol950(Sol950Scr::ACTIVE);
                    return;
                }
            }
            Key::Digit(1) => {
                if let Some(h) = app.sol950_host_mut() {
                    let parity = if h.last_slot == u32::MAX { 0 } else { (h.last_slot & 1) as u8 };
                    h.saved_ack = false;
                    h.target_peer = None;
                    h.engine.start_cq(parity);
                }
                app.screen = Screen::Sol950(Sol950Scr::ACTIVE);
                return;
            }
            Key::Digit(0) => {
                if let Some(h) = app.sol950_host_mut() {
                    h.engine.stop_session();
                    h.pending_tx = None;
                    h.target_peer = None;
                }
                app.out.push(RadioCmd::Sol950Abort);
            }
            Key::Digit(2) => {
                if let Some(h) = app.sol950_host_mut() {
                    h.log_grid_filter = None;
                }
                app.screen = Screen::Sol950(Sol950Scr::LOG);
                return;
            }
            Key::Digit(3) => {
                app.screen = Screen::Sol950(Sol950Scr::STATS);
                return;
            }
            Key::Digit(4) => {
                app.screen = Screen::Sol950(Sol950Scr::GRIDS);
                return;
            }
            Key::Star => {
                app.screen = Screen::Sol950(Sol950Scr::SETUP);
                return;
            }
            Key::Hash => {
                app.screen = Screen::Sol950(Sol950Scr::ACTIVE);
                return;
            }
            _ => {}
        }
        app.screen = Screen::Sol950(self);
    }

    fn key_active(self, app: &mut App, k: Key) {
        match k {
            Key::Exit | Key::Hash => app.screen = Screen::Sol950(Sol950Scr::OPERATE),
            Key::Digit(0) => {
                if let Some(h) = app.sol950_host_mut() {
                    h.engine.stop_session();
                    h.pending_tx = None;
                    h.target_peer = None;
                }
                app.out.push(RadioCmd::Sol950Abort);
                app.screen = Screen::Sol950(Sol950Scr::OPERATE);
            }
            Key::Star => app.screen = Screen::Sol950(Sol950Scr::SETUP),
            _ => {}
        }
    }

    fn key_setup(mut self, app: &mut App, k: Key, store: &impl ChannelStore) {
        use super::text::{open as open_text, TextFor};
        let mut retune = false;
        match k {
            Key::Exit => {
                app.screen = Screen::Sol950(Sol950Scr::OPERATE);
                return;
            }
            Key::Up => self.sel = if self.sel == 0 { (SETUP_ROWS - 1) as u8 } else { self.sel - 1 },
            Key::Down => self.sel = (self.sel + 1) % SETUP_ROWS as u8,
            Key::Menu if matches!(self.sel, 0 | 1 | 4) => {
                let target = match self.sel {
                    0 => TextFor::Sol950Call,
                    1 => TextFor::Sol950Prefix,
                    _ => TextFor::Sol950Grid,
                };
                open_text(app, target, store);
                return;
            }
            Key::Left | Key::Right | Key::Menu => {
                let dir: i32 = if k == Key::Left { -1 } else { 1 };
                if let Some(h) = app.sol950_host_mut() {
                    match self.sel {
                        2 => {
                            h.profile.suffix = (h.profile.suffix as i32 + dir).rem_euclid(16) as u8;
                        }
                        3 => {
                            h.profile.use_gps_grid = !h.profile.use_gps_grid;
                        }
                        5 => {
                            let n = DIALS_10M.len() as i32;
                            h.profile.dial_idx = (h.profile.dial_idx as i32 + dir).rem_euclid(n) as u8;
                            retune = true;
                        }
                        6 => {
                            h.profile.rx_corr_hz = (h.profile.rx_corr_hz as i32 + dir * 25).clamp(-5000, 5000) as i16;
                            retune = true;
                        }
                        7 => {
                            h.profile.tx_power = (h.profile.tx_power as i32 + dir).rem_euclid(3) as u8;
                        }
                        _ => {}
                    }
                    h.profile_dirty = true;
                    h.sync_engine_config();
                }
                if retune {
                    app.sol950_retune();
                }
            }
            _ => {}
        }
        app.screen = Screen::Sol950(self);
    }

    fn draw_operate(self, app: &mut App, lcd: &mut impl Panel, live: &LiveAll) {
        let (rx_ready, tx_active, rssi, loading) = match live.mode {
            ModeLive::Sol950 { rx_ready, tx_active, rssi, loading } => (rx_ready, tx_active, rssi, loading),
            _ => (false, false, -127, 255),
        };
        let mut id_buf = [0u8; 16];
        let mut id_len = 0usize;
        let mut grid_bytes = *b"----";
        let mut grid_src = GridSource::Unset;
        let mut dial = DIALS_10M[0];
        let mut utc_ok = false;
        let mut mode_tag = 0u32;
        let mut rows = [empty_row(); MAX_PEERS];
        let mut n_rows = 0usize;
        if let Some(h) = app.sol950_host() {
            id_len = h.profile.format_identity(&mut id_buf);
            (grid_bytes, grid_src) = h.profile.effective_grid(h.gps_grid);
            dial = h.profile.dial();
            utc_ok = h.utc_ready();
            mode_tag = match h.engine.mode() {
                OpMode::Monitor => 0,
                OpMode::AutoCq { parity } => 1 + parity as u32,
                OpMode::Pounce => 3,
            };
            n_rows = h.engine.activity_rows(&mut rows);
        }

        let top_h = Mix::new()
            .b(&id_buf[..id_len])
            .b(&grid_bytes)
            .n(grid_src as u32)
            .n(dial)
            .n(utc_ok as u32)
            .n(rx_ready as u32)
            .n(tx_active as u32)
            .n(rssi as u32)
            .n(loading as u32)
            .n(mode_tag)
            .get();
        if let Some(clear) = app.dirty_shape(R_TOP, top_h, true) {
            let y0 = TOP_Y;
            redraw_box!(clear, lcd, (0, y0, W, 44, BG), {
                let mut line1 = Buf::<32>::new();
                let src_tag = match grid_src {
                    GridSource::Gps => "GPS",
                    GridSource::Manual => "MAN",
                    GridSource::Unset => "---",
                };
                let id_s = core::str::from_utf8(&id_buf[..id_len]).unwrap_or("?");
                let g_s = core::str::from_utf8(&grid_bytes).unwrap_or("----");
                let _ = uwrite!(line1, "{} {} {}", id_s, g_s, src_tag);
                lcd.text4(8, y0 + 16, line1.as_bytes(), &UI_14, TEXT, BG);

                let mut line2 = Buf::<36>::new();
                push_khz3(&mut line2, dial);
                let st = if loading < 100 {
                    " LOAD"
                } else if tx_active {
                    " TX"
                } else if rx_ready {
                    " RX"
                } else {
                    " INIT"
                };
                let utc_s = if utc_ok { " UTC OK" } else { " NO UTC" };
                let _ = uwrite!(line2, "{}{}", st, utc_s);
                let col = if tx_active { RED } else if utc_ok && rx_ready { GREEN } else { AMBER };
                lcd.text4(8, y0 + 35, line2.as_bytes(), &UI_12, col, BG);
                lcd.fill(0, y0 + 42, W, 1, RULE);
            });
        }

        let list_y = TOP_Y + 46;
        let row_h = 34;
        for i in 0..OPERATE_ROWS {
            let idx = self.top as usize + i;
            let selected = idx == self.sel as usize && idx < n_rows;
            let row = (idx < n_rows).then(|| rows[idx]);
            let mut rh = Mix::new().n(selected as u32).n(idx as u32);
            if let Some(r) = row {
                rh = rh
                    .n(r.peer.raw())
                    .n(r.grid4 as u32)
                    .n(r.snr_db as u32)
                    .n(r.logged as u32);
            }
            if let Some(clear) = app.dirty_shape(R_ROW + i, rh.get(), selected) {
                let y = list_y + i as i32 * row_h;
                let bg = if selected { BLUE } else { BG };
                let fg = if selected { BLUE_FG } else { TEXT };
                let dim = if selected { BLUE_FG } else { DIM };
                redraw_box!(clear || selected, lcd, (0, y, W, row_h, bg), {
                    if let Some(r) = row {
                        let mut cb = [0u8; 6];
                        let cn = r.peer.decode(&mut cb);
                        let cs = core::str::from_utf8(&cb[..cn]).unwrap_or("?");
                        let gb = decode_grid4(r.grid4).unwrap_or(*b"----");
                        let gs = core::str::from_utf8(&gb).unwrap_or("----");
                        let mut left = Buf::<24>::new();
                        let kind = if r.logged { "OK" } else if r.owed_reply.is_some() { "RX" } else { "CQ" };
                        let _ = uwrite!(left, "{} {} {}", kind, cs, gs);
                        lcd.text4(8, y + 22, left.as_bytes(), &UI_14, fg, bg);

                        let mut right = Buf::<12>::new();
                        let _ = uwrite!(right, "{}dB", r.snr_db);
                        lcd.text4(W - 48, y + 22, right.as_bytes(), &UI_12, dim, bg);
                    } else if i == 0 && n_rows == 0 {
                        lcd.text4(8, y + 22, b"Listening on 10m USB...", &UI_12, DIM, BG);
                    }
                });
            }
        }
    }

    fn draw_active(self, app: &mut App, lcd: &mut impl Panel, live: &LiveAll) {
        let tx_active = matches!(live.mode, ModeLive::Sol950 { tx_active: true, .. });
        let mut rows = [empty_row(); MAX_PEERS];
        let (mode, active_row, saved_ack, notice_len) = if let Some(h) = app.sol950_host() {
            let n = h.engine.activity_rows(&mut rows);
            let matched = h
                .target_peer
                .and_then(|tp| rows[..n].iter().copied().find(|r| r.peer == tp))
                .or_else(|| rows[..n].iter().copied().find(|r| r.owed_reply.is_some()));
            (h.engine.mode(), matched, h.saved_ack, h.notice_len)
        } else {
            (OpMode::Monitor, None, false, 0)
        };
        let owed = active_row.and_then(|r| r.owed_reply);
        let logged = active_row.is_some_and(|r| r.logged);
        let action = plain_qso_action(mode, owed, logged, tx_active, saved_ack);
        let h = Mix::new()
            .n(tx_active as u32)
            .n(matches!(mode, OpMode::AutoCq { .. }) as u32)
            .n(owed.map_or(0, |b| 1 + b as u32))
            .n(logged as u32)
            .n(active_row.map_or(0, |r| r.peer.raw()))
            .n(active_row.map_or(0, |r| r.grid4 as u32))
            .n(active_row.map_or(0, |r| r.snr_db as u32))
            .n(saved_ack as u32)
            .n(notice_len as u32)
            .get();
        if let Some(clear) = app.dirty_shape(R_TOP, h, true) {
            let y0 = TOP_Y;
            redraw_box!(clear, lcd, (0, y0, W, 180, BG), {
                let mut peer_line = Buf::<28>::new();
                if let Some(r) = active_row {
                    let mut cb = [0u8; 6];
                    let cn = r.peer.decode(&mut cb);
                    let cs = core::str::from_utf8(&cb[..cn]).unwrap_or("?");
                    let gb = decode_grid4(r.grid4).unwrap_or(*b"----");
                    let gs = core::str::from_utf8(&gb).unwrap_or("----");
                    let _ = uwrite!(peer_line, "Peer: {} ({})", cs, gs);
                } else {
                    let _ = uwrite!(peer_line, "Peer: none");
                }
                lcd.text4(10, y0 + 28, peer_line.as_bytes(), &UI_15B, TEXT, BG);

                let act_col = if tx_active { RED } else if saved_ack { GREEN } else { BLUE };
                lcd.text4(10, y0 + 62, action, &UI_14, act_col, BG);

                if let Some(r) = active_row {
                    let mut sline = Buf::<24>::new();
                    let _ = uwrite!(sline, "Last SNR: {} dB", r.snr_db);
                    lcd.text4(10, y0 + 90, sline.as_bytes(), &UI_12, DIM, BG);
                }
                if saved_ack {
                    lcd.text4(10, y0 + 118, b"Saved to flash log", &UI_14, GREEN, BG);
                }
                if let Some(host) = app.sol950_host() {
                    let nb = host.notice_bytes();
                    if !nb.is_empty() {
                        lcd.text4(10, y0 + 146, nb, &UI_12, AMBER, BG);
                    }
                }
            });
        }
    }

    fn draw_setup(self, app: &mut App, lcd: &mut impl Panel) {
        let Some(h) = app.sol950_host() else { return };
        let prof = h.profile;
        let gps_grid = h.gps_grid;
        let y0 = TOP_Y + 4;
        let row_h = 24;
        for i in 0..SETUP_ROWS {
            let selected = i == self.sel as usize;
            let mut val = Buf::<16>::new();
            let label: &[u8] = match i {
                0 => {
                    let s = core::str::from_utf8(prof.base_call_bytes()).unwrap_or("");
                    let _ = uwrite!(val, "{}", if s.is_empty() { "---" } else { s });
                    b"Base call"
                }
                1 => {
                    let s = core::str::from_utf8(&prof.prefix[..prof.prefix_len as usize]).unwrap_or("");
                    let _ = uwrite!(val, "{}", if s.is_empty() { "None" } else { s });
                    b"Prefix"
                }
                2 => {
                    if prof.suffix == 0 {
                        let _ = uwrite!(val, "Off");
                    } else {
                        let _ = uwrite!(val, "-{}", prof.suffix);
                    }
                    b"Suffix"
                }
                3 => {
                    let _ = uwrite!(val, "{}", if prof.use_gps_grid { "GPS" } else { "Manual" });
                    b"Grid src"
                }
                4 => {
                    let g = core::str::from_utf8(&prof.manual_grid).unwrap_or("----");
                    let _ = uwrite!(val, "{}", g);
                    b"Man grid"
                }
                5 => {
                    push_khz3(&mut val, prof.dial());
                    b"USB dial"
                }
                6 => {
                    let _ = uwrite!(val, "{}Hz", prof.rx_corr_hz);
                    b"RX offset"
                }
                _ => {
                    let p = match prof.tx_power {
                        0 => "Low",
                        1 => "Mid",
                        _ => "High",
                    };
                    let _ = uwrite!(val, "{}", p);
                    b"TX power"
                }
            };
            let rh = Mix::new().n(selected as u32).b(label).b(val.as_bytes()).get();
            if let Some(clear) = app.dirty_shape(R_ROW + i, rh, selected) {
                let y = y0 + i as i32 * row_h;
                let bg = if selected { BLUE } else { BG };
                let fg = if selected { BLUE_FG } else { TEXT };
                redraw_box!(clear || selected, lcd, (0, y, W, row_h, bg), {
                    lcd.text4(8, y + 16, label, &UI_12, fg, bg);
                    lcd.text4(W - 84, y + 16, val.as_bytes(), &UI_12, fg, bg);
                });
            }
        }
        // Live identity + effective grid preview at bottom of Setup.
        let mut id_buf = [0u8; 16];
        let id_len = prof.format_identity(&mut id_buf);
        let (eff_g, _) = prof.effective_grid(gps_grid);
        let ph = Mix::new().b(&id_buf[..id_len]).b(&eff_g).get();
        if let Some(clear) = app.dirty_shape(R_NOTE, ph, true) {
            let y = y0 + SETUP_ROWS as i32 * row_h + 6;
            redraw_box!(clear, lcd, (0, y, W, 24, BG), {
                let mut pv = Buf::<32>::new();
                let id_s = core::str::from_utf8(&id_buf[..id_len]).unwrap_or("?");
                let g_s = core::str::from_utf8(&eff_g).unwrap_or("----");
                let _ = uwrite!(pv, "On air: {} {}", id_s, g_s);
                lcd.text4(8, y + 16, pv.as_bytes(), &UI_12, GREEN, BG);
            });
        }
    }

    // ---- Log, Stats, and Worked Grids (Milestone 2) ----

    fn key_log(mut self, app: &mut App, k: Key) {
        let total = app.sol950_host().map_or(0, |h| {
            let mut tmp = [None; 32];
            h.store.read_newest(0, h.log_grid_filter, &mut tmp)
        });
        match k {
            Key::Exit => {
                if let Some(h) = app.sol950_host_mut() {
                    h.log_grid_filter = None;
                }
                app.screen = Screen::Sol950(Sol950Scr::OPERATE);
                return;
            }
            Key::Up if total > 0 => {
                self.sel = if self.sel == 0 { (total - 1) as u8 } else { self.sel - 1 };
                if (self.sel as usize) < self.top as usize {
                    self.top = self.sel;
                } else if (self.sel as usize) >= self.top as usize + LOG_ROWS {
                    self.top = (self.sel as usize + 1 - LOG_ROWS) as u8;
                }
            }
            Key::Down if total > 0 => {
                self.sel = ((self.sel as usize + 1) % total) as u8;
                if (self.sel as usize) < self.top as usize {
                    self.top = self.sel;
                } else if (self.sel as usize) >= self.top as usize + LOG_ROWS {
                    self.top = (self.sel as usize + 1 - LOG_ROWS) as u8;
                }
            }
            Key::Menu if (self.sel as usize) < total => {
                app.screen = Screen::Sol950(Sol950Scr { page: PAGE_LOG_DETAIL, ..self });
                return;
            }
            _ => {}
        }
        app.screen = Screen::Sol950(self);
    }

    fn key_log_detail(self, app: &mut App, k: Key) {
        if k == Key::Exit {
            app.screen = Screen::Sol950(Sol950Scr { page: PAGE_LOG, ..self });
        }
    }

    fn key_stats(mut self, app: &mut App, k: Key) {
        match k {
            Key::Exit => {
                app.screen = Screen::Sol950(Sol950Scr::OPERATE);
                return;
            }
            Key::Left | Key::Right | Key::Menu => {
                self.sel ^= 1;
            }
            _ => {}
        }
        app.screen = Screen::Sol950(self);
    }

    fn key_grids_overview(mut self, app: &mut App, k: Key) {
        let mut lon_f = (self.field / 18).min(17) as i32;
        let mut lat_f = (self.field % 18) as i32;
        match k {
            Key::Exit => {
                app.screen = Screen::Sol950(Sol950Scr::OPERATE);
                return;
            }
            Key::Left => lon_f = (lon_f - 1).rem_euclid(18),
            Key::Right => lon_f = (lon_f + 1).rem_euclid(18),
            Key::Up => lat_f = (lat_f + 1).rem_euclid(18), // north is higher lat_f
            Key::Down => lat_f = (lat_f - 1).rem_euclid(18),
            Key::Menu => {
                app.screen = Screen::Sol950(Sol950Scr { page: PAGE_GRIDS_FIELD, field: self.field, sub: 55, ..self });
                return;
            }
            _ => {}
        }
        self.field = (lon_f * 18 + lat_f) as u16;
        app.screen = Screen::Sol950(self);
    }

    fn key_grids_field(mut self, app: &mut App, k: Key) {
        let mut lon_s = (self.sub / 10).min(9) as i32;
        let mut lat_s = (self.sub % 10) as i32;
        match k {
            Key::Exit => {
                app.screen = Screen::Sol950(Sol950Scr { page: PAGE_GRIDS_OVERVIEW, field: self.field, ..self });
                return;
            }
            Key::Left => lon_s = (lon_s - 1).rem_euclid(10),
            Key::Right => lon_s = (lon_s + 1).rem_euclid(10),
            Key::Up => lat_s = (lat_s + 1).rem_euclid(10),
            Key::Down => lat_s = (lat_s - 1).rem_euclid(10),
            Key::Menu => {
                let fl = field_letters(self.field);
                let g4 = [fl[0], fl[1], b'0' + lon_s as u8, b'0' + lat_s as u8];
                if let Some(h) = app.sol950_host_mut() {
                    h.log_grid_filter = Some(g4);
                }
                app.screen = Screen::Sol950(Sol950Scr::LOG);
                return;
            }
            _ => {}
        }
        self.sub = (lon_s * 10 + lat_s) as u8;
        app.screen = Screen::Sol950(self);
    }

    fn draw_log(self, app: &mut App, lcd: &mut impl Panel) {
        let mut page = [None; LOG_ROWS];
        let (n, count, full, filter) = if let Some(h) = app.sol950_host() {
            let n = h.store.read_newest(self.top as u16, h.log_grid_filter, &mut page);
            (n, h.store.count(), h.store.is_full(), h.log_grid_filter)
        } else {
            (0, 0, false, None)
        };
        let th = Mix::new()
            .n(count as u32)
            .n(full as u32)
            .b(&filter.unwrap_or(*b"----"))
            .get();
        if let Some(clear) = app.dirty_shape(R_TOP, th, true) {
            let y0 = TOP_Y;
            redraw_box!(clear, lcd, (0, y0, W, 24, BG), {
                let mut head = Buf::<32>::new();
                if let Some(gf) = filter {
                    let gs = core::str::from_utf8(&gf).unwrap_or("----");
                    let _ = uwrite!(head, "Log grid {} ({})", gs, n);
                } else if full {
                    let _ = uwrite!(head, "Log FULL ({} saved)", count);
                } else {
                    let _ = uwrite!(head, "Saved contacts: {}", count);
                }
                let col = if full { AMBER } else { TEXT };
                lcd.text4(8, y0 + 16, head.as_bytes(), &UI_12B, col, BG);
                lcd.fill(0, y0 + 23, W, 1, RULE);
            });
        }
        let list_y = TOP_Y + 26;
        let row_h = 30;
        for (i, &row) in page.iter().enumerate() {
            let idx = self.top as usize + i;
            let selected = idx == self.sel as usize && i < n;
            let mut rh = Mix::new().n(selected as u32).n(idx as u32);
            if let Some(c) = row {
                rh = rh.n(c.seq).b(&c.peer_id).b(&c.peer_grid);
            }
            if let Some(clear) = app.dirty_shape(R_ROW + i, rh.get(), selected) {
                let y = list_y + i as i32 * row_h;
                let bg = if selected { BLUE } else { BG };
                let fg = if selected { BLUE_FG } else { TEXT };
                let dim = if selected { BLUE_FG } else { DIM };
                redraw_box!(clear || selected, lcd, (0, y, W, row_h, bg), {
                    if let Some(c) = row {
                        let ps = core::str::from_utf8(c.peer_bytes()).unwrap_or("?");
                        let gs = core::str::from_utf8(&c.peer_grid).unwrap_or("----");
                        let mut left = Buf::<24>::new();
                        let _ = uwrite!(left, "{} {}", ps, gs);
                        lcd.text4(8, y + 20, left.as_bytes(), &UI_14, fg, bg);

                        let mut right = Buf::<16>::new();
                        let _ = uwrite!(right, "{}/{}dB", c.rx_snr_db, c.tx_snr_db);
                        lcd.text4(W - 76, y + 20, right.as_bytes(), &UI_12, dim, bg);
                    } else if i == 0 && n == 0 {
                        lcd.text4(8, y + 20, b"No saved contacts", &UI_12, DIM, BG);
                    }
                });
            }
        }
    }

    fn draw_log_detail(self, app: &mut App, lcd: &mut impl Panel) {
        let mut one = [None; 1];
        let rec: Option<SavedContact> = app.sol950_host().and_then(|h| {
            h.store.read_newest(self.sel as u16, h.log_grid_filter, &mut one);
            one[0]
        });
        let h = Mix::new().n(self.sel as u32).n(rec.map_or(0, |r| r.seq)).get();
        if let Some(clear) = app.dirty_shape(R_TOP, h, true) {
            let y0 = TOP_Y + 6;
            redraw_box!(clear, lcd, (0, y0, W, 200, BG), {
                if let Some(c) = rec {
                    let ps = core::str::from_utf8(c.peer_bytes()).unwrap_or("?");
                    let ms = core::str::from_utf8(c.my_bytes()).unwrap_or("?");
                    let pg = core::str::from_utf8(&c.peer_grid).unwrap_or("----");
                    let mg = core::str::from_utf8(&c.my_grid).unwrap_or("----");
                    let mut l1 = Buf::<28>::new();
                    let _ = uwrite!(l1, "{} ({})", ps, pg);
                    lcd.text4(10, y0 + 18, l1.as_bytes(), &UI_15B, TEXT, BG);

                    let mut l2 = Buf::<28>::new();
                    let _ = uwrite!(l2, "My: {} ({})", ms, mg);
                    lcd.text4(10, y0 + 46, l2.as_bytes(), &UI_12, DIM, BG);

                    let mut l3 = Buf::<28>::new();
                    let _ = uwrite!(l3, "SNR RX {} / TX {} dB", c.rx_snr_db, c.tx_snr_db);
                    lcd.text4(10, y0 + 72, l3.as_bytes(), &UI_12, GREEN, BG);

                    let mut l4 = Buf::<24>::new();
                    l4.push(b"Dial: ");
                    push_khz3(&mut l4, c.dial_hz10);
                    l4.push(b" MHz");
                    lcd.text4(10, y0 + 98, l4.as_bytes(), &UI_12, TEXT, BG);

                    let mut l5 = Buf::<28>::new();
                    let hh = (c.utc_sec / 3600) % 24;
                    let mm = (c.utc_sec / 60) % 60;
                    let ss = c.utc_sec % 60;
                    let _ = uwrite!(l5, "UTC {}:{}:{} #{}", hh, mm, ss, c.seq);
                    lcd.text4(10, y0 + 124, l5.as_bytes(), &UI_12, DIM, BG);
                } else {
                    lcd.text4(10, y0 + 20, b"Record unavailable", &UI_12, DIM, BG);
                }
            });
        }
    }

    fn draw_stats(self, app: &mut App, lcd: &mut impl Panel) {
        let today_mode = (self.sel & 1) != 0;
        let (st, utc_known) = if let Some(h) = app.sol950_host() {
            let day = if today_mode {
                (h.utc_sec != 0).then_some(h.utc_sec / 86_400)
            } else {
                None
            };
            let st = if today_mode && day.is_none() {
                crate::sol950::SavedStats::default()
            } else {
                h.store.stats(day)
            };
            (st, h.utc_sec != 0)
        } else {
            (crate::sol950::SavedStats::default(), false)
        };
        let h = Mix::new()
            .n(today_mode as u32)
            .n(utc_known as u32)
            .n(st.contacts as u32)
            .n(st.unique_stations as u32)
            .n(st.unique_grids as u32)
            .n(st.best_rx_snr as u32)
            .n(st.best_tx_snr as u32)
            .get();
        if let Some(clear) = app.dirty_shape(R_TOP, h, true) {
            let y0 = TOP_Y + 6;
            redraw_box!(clear, lcd, (0, y0, W, 200, BG), {
                let title: &[u8] = if today_mode { b"Today (UTC)" } else { b"All saved" };
                lcd.text4(10, y0 + 18, title, &UI_15B, BLUE, BG);
                if today_mode && !utc_known {
                    lcd.text4(10, y0 + 50, b"Waiting for UTC date", &UI_12, AMBER, BG);
                } else {
                    let mut b1 = Buf::<28>::new();
                    let _ = uwrite!(b1, "Contacts:  {}", st.contacts);
                    lcd.text4(10, y0 + 52, b1.as_bytes(), &UI_14, TEXT, BG);

                    let mut b2 = Buf::<28>::new();
                    let _ = uwrite!(b2, "Stations:  {}", st.unique_stations);
                    lcd.text4(10, y0 + 80, b2.as_bytes(), &UI_14, TEXT, BG);

                    let mut b3 = Buf::<28>::new();
                    let _ = uwrite!(b3, "Grids:     {}", st.unique_grids);
                    lcd.text4(10, y0 + 108, b3.as_bytes(), &UI_14, GREEN, BG);

                    let mut b4 = Buf::<32>::new();
                    if st.has_snr {
                        let _ = uwrite!(b4, "Best SNR: {} / {} dB", st.best_rx_snr, st.best_tx_snr);
                    } else {
                        let _ = uwrite!(b4, "Best SNR: --");
                    }
                    lcd.text4(10, y0 + 138, b4.as_bytes(), &UI_12, DIM, BG);
                }
            });
        }
    }

    fn draw_grids_overview(self, app: &mut App, lcd: &mut impl Panel) {
        let mask = app.sol950_host().map_or([0u32; 11], |h| *h.store.field_mask());
        let worked_fields: u32 = mask.iter().map(|w| w.count_ones()).sum();
        let fl = field_letters(self.field);
        let sel_worked = {
            let i = self.field as usize;
            i < 324 && (mask[i / 32] & (1u32 << (i % 32))) != 0
        };
        let mut h = Mix::new().n(self.field as u32).n(worked_fields);
        for w_word in mask {
            h = h.n(w_word);
        }
        if let Some(clear) = app.dirty_shape(R_TOP, h.get(), true) {
            let y0 = TOP_Y;
            redraw_box!(clear, lcd, (0, y0, W, 240, BG), {
                let mut hdr = Buf::<32>::new();
                let fs = core::str::from_utf8(&fl).unwrap_or("??");
                let st = if sel_worked { "worked" } else { "empty" };
                let _ = uwrite!(hdr, "Field {} ({})  {}/324", fs, st, worked_fields);
                lcd.text4(8, y0 + 16, hdr.as_bytes(), &UI_12B, TEXT, BG);

                // 18x18 Maidenhead field grid: lon A..R left-to-right, lat R..A top-to-bottom (north up).
                // Non-color-dependent: hollow 1px frame = empty, filled inner block + bright border = worked,
                // double-frame = selected cursor.
                let gx = 12i32;
                let gy = y0 + 24;
                let cw = 12i32;
                let ch = 11i32;
                for lon_f in 0..18usize {
                    for lat_f in 0..18usize {
                        let idx = lon_f * 18 + lat_f;
                        let x = gx + lon_f as i32 * cw;
                        let y = gy + (17 - lat_f) as i32 * ch;
                        let worked = (mask[idx / 32] & (1u32 << (idx % 32))) != 0;
                        let sel = idx == self.field as usize;
                        // Outer 1px border
                        let border = if sel { AMBER } else if worked { TEXT } else { LINE };
                        lcd.fill(x, y, cw - 1, 1, border);
                        lcd.fill(x, y + ch - 2, cw - 1, 1, border);
                        lcd.fill(x, y, 1, ch - 1, border);
                        lcd.fill(x + cw - 2, y, 1, ch - 1, border);
                        if worked {
                            lcd.fill(x + 2, y + 2, cw - 5, ch - 5, GREEN);
                        }
                        if sel {
                            // Inner second frame for selected cell (non-color-dependent)
                            lcd.fill(x + 1, y + 1, cw - 3, 1, WHITE);
                            lcd.fill(x + 1, y + ch - 3, cw - 3, 1, WHITE);
                        }
                    }
                }
            });
        }
    }

    fn draw_grids_field(self, app: &mut App, lcd: &mut impl Panel) {
        let sqs = app.sol950_host().map_or([0u16; 100], |h| h.store.field_squares(self.field));
        let fl = field_letters(self.field);
        let sel_sq = (self.sub as usize).min(99);
        let sel_cnt = sqs[sel_sq];
        let lon_s = sel_sq / 10;
        let lat_s = sel_sq % 10;
        let g4 = [fl[0], fl[1], b'0' + lon_s as u8, b'0' + lat_s as u8];
        let mut h = Mix::new().n(self.field as u32).n(self.sub as u32);
        for &c in &sqs {
            if c != 0 {
                h = h.n(c as u32);
            }
        }
        if let Some(clear) = app.dirty_shape(R_TOP, h.get(), true) {
            let y0 = TOP_Y;
            redraw_box!(clear, lcd, (0, y0, W, 240, BG), {
                let mut hdr = Buf::<32>::new();
                let gs = core::str::from_utf8(&g4).unwrap_or("----");
                let _ = uwrite!(hdr, "Grid {}: {} QSO", gs, sel_cnt);
                lcd.text4(8, y0 + 16, hdr.as_bytes(), &UI_12B, TEXT, BG);

                // 10x10 square grid inside selected field: lon 0..9 L->R, lat 9..0 top->bottom.
                let gx = 20i32;
                let gy = y0 + 26;
                let cw = 20i32;
                let ch = 18i32;
                for ls in 0..10usize {
                    for bs in 0..10usize {
                        let idx = ls * 10 + bs;
                        let x = gx + ls as i32 * cw;
                        let y = gy + (9 - bs) as i32 * ch;
                        let worked = sqs[idx] > 0;
                        let sel = idx == sel_sq;
                        let border = if sel { AMBER } else if worked { TEXT } else { LINE };
                        lcd.fill(x, y, cw - 1, 1, border);
                        lcd.fill(x, y + ch - 2, cw - 1, 1, border);
                        lcd.fill(x, y, 1, ch - 1, border);
                        lcd.fill(x + cw - 2, y, 1, ch - 1, border);
                        if worked {
                            lcd.fill(x + 3, y + 3, cw - 7, ch - 7, GREEN);
                        }
                        if sel {
                            lcd.fill(x + 1, y + 1, cw - 3, 1, WHITE);
                            lcd.fill(x + 1, y + ch - 3, cw - 3, 1, WHITE);
                        }
                    }
                }
            });
        }
    }
}
