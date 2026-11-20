//! RAD950 10 m weak-signal mode UI (`MENU 0 3 7`):
//! - **Operate** (`page = 0`): station header, UTC/GPS readiness, heard-stations activity list, Answer / Call CQ / Stop.
//! - **Active** (`page = 1`): in-progress CQ or QSO with plain-language next action and `Saved` status.
//! - **Setup** (`page = 2`): callsign, optional prefix/suffix (`1..=15`), GPS/manual grid, USB dial, signed RX offset, TX power, live preview.

use super::shared::*;
use super::{Cx, Hints, View};
use crate::chan::ChannelStore;
use crate::keys::{KeyEvent, Press};
use crate::rad950_chat::{ChatDelivery, ChatDraft, ChatPhase};
use crate::rad950::{
    band_label, field_letters, AutoAnswer, ClockSource, CoopLink, CoopRxMode, GridSource, SavedContact, Rad950Store,
    RAD950_DIALS, WF_COLS, WF_ROWS,
};
use crate::{App, Screen};
use ferrite_core::freq::Freq10;
use ferrite_core::keys::Key;
use ferrite_core::msg::{LiveAll, ModeLive, RadioCmd};
use ferrite_core::rad950::urad950::frame::decode_grid4;
use ferrite_core::rad950::urad950::Callsign;
use ferrite_core::rad950::{ActivityRow, OpMode, MAX_PEERS};
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
pub const PAGE_CHAT: u8 = 8;
pub const PAGE_CHAT_REPLY: u8 = 9;
pub const PAGE_CHAT_PICK: u8 = 10;
pub const PAGE_CHAT_PREVIEW: u8 = 11;
pub const PAGE_CHAT_MORE: u8 = 12;
pub const PAGE_DISTRESS: u8 = 13;
pub const PAGE_OPERATE_MENU: u8 = 14;
pub const PAGE_RENDEZVOUS: u8 = 15;

const OPERATE_ROWS: usize = 4;
const LOG_ROWS: usize = 6;
const SETUP_ROWS: usize = 15;
const SETUP_VISIBLE: usize = 8;
const TOP_Y: i32 = w::SBAR_H;

/// RAD950 screen state (`8` bytes, fits inside `Screen` without growing it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rad950Scr {
    pub page: u8,
    pub sel: u8,
    pub top: u8,
    pub field: u16,
    pub sub: u8,
}

impl Rad950Scr {
    pub const OPERATE: Self = Self { page: PAGE_OPERATE, sel: 0, top: 0, field: 0, sub: 0 };
    pub const ACTIVE: Self = Self { page: PAGE_ACTIVE, sel: 0, top: 0, field: 0, sub: 0 };
    pub const SETUP: Self = Self { page: PAGE_SETUP, sel: 0, top: 0, field: 0, sub: 0 };
    pub const LOG: Self = Self { page: PAGE_LOG, sel: 0, top: 0, field: 0, sub: 0 };
    pub const STATS: Self = Self { page: PAGE_STATS, sel: 0, top: 0, field: 0, sub: 0 };
    /// Default cursor on `FN` (`lon_f = 5, lat_f = 13 -> 5 * 18 + 13 = 103`).
    pub const GRIDS: Self = Self { page: PAGE_GRIDS_OVERVIEW, sel: 0, top: 0, field: 103, sub: 31 };
    pub const CHAT: Self = Self { page: PAGE_CHAT, sel: 0, top: 0, field: 0, sub: 0 };
    pub const CHAT_REPLY: Self = Self { page: PAGE_CHAT_REPLY, sel: 0, top: 0, field: 0, sub: 0 };
    pub const CHAT_PREVIEW: Self = Self { page: PAGE_CHAT_PREVIEW, sel: 0, top: 0, field: 0, sub: 0 };
}

/// `MENU 0 3 7`: open RAD950; refused while transmitting or repeating.
pub fn open(app: &mut App) -> Screen {
    if app.radio.keyed.is_some() || app.radio.mode == super::activity::Mode::Xb {
        app.show_toast(
            b"RAD950",
            if app.radio.keyed.is_some() { b"not keyed" } else { b"stop repeater first" },
            false,
        );
        return app.screen;
    }
    Screen::Rad950(Rad950Scr::OPERATE)
}

pub fn page_id(s: &Rad950Scr) -> &'static str {
    match s.page {
        PAGE_OPERATE => "rad950.operate",
        PAGE_ACTIVE => "rad950.active",
        PAGE_SETUP => "rad950.setup",
        PAGE_LOG => "rad950.log",
        PAGE_LOG_DETAIL => "rad950.log_detail",
        PAGE_STATS => "rad950.stats",
        PAGE_GRIDS_OVERVIEW => "rad950.grids",
        PAGE_GRIDS_FIELD => "rad950.grids_field",
        PAGE_CHAT => "rad950.chat",
        PAGE_CHAT_REPLY => "rad950.chat_reply",
        PAGE_CHAT_PICK => "rad950.chat_pick",
        PAGE_CHAT_PREVIEW => "rad950.chat_preview",
        PAGE_CHAT_MORE => "rad950.chat_more",
        PAGE_DISTRESS => "rad950.distress",
        PAGE_OPERATE_MENU => "rad950.menu",
        PAGE_RENDEZVOUS => "rad950.rendezvous",
        _ => "rad950.operate",
    }
}

const CYAN: u16 = 0x07FF;
const MAGENTA: u16 = 0xF81F;

fn grid_band_opt(sel: u8) -> Option<u8> {
    match sel % 5 {
        1 => Some(10),
        2 => Some(12),
        3 => Some(15),
        4 => Some(17),
        _ => None,
    }
}

fn grid_band_color(sel: u8) -> u16 {
    match sel % 5 {
        1 => AMBER,   // 10m
        2 => CYAN,    // 12m
        3 => GREEN,   // 15m
        4 => MAGENTA, // 17m
        _ => GREEN,   // All bands
    }
}

fn draw_band_pills(lcd: &mut impl Panel, y: i32, active_sel: u8) {
    const TAGS: [&[u8]; 5] = [b"All", b"10m", b"12m", b"15m", b"17m"];
    let cur = (active_sel % 5) as usize;
    for (i, &tag) in TAGS.iter().enumerate() {
        let bx = 8 + i as i32 * 45;
        let on = i == cur;
        let pbg = if on { grid_band_color(i as u8) } else { LINE };
        let pfg = if on { BG } else { DIM };
        lcd.fill(bx, y, 42, 17, pbg);
        lcd.text4(bx + 8, y + 13, tag, &UI_12B, pfg, pbg);
    }
}

fn draw_tx_flag(lcd: &mut impl Panel, x: i32, y: i32) {
    lcd.fill(x, y, 28, 16, RED);
    lcd.text4(x + 6, y + 12, b"TX", &UI_12B, WHITE, RED);
}

/// Crisp 16x16 status icon for a call-list row:
/// - Worked station: green checkmark badge (with amber/cyan corner pip if it earned a New Grid / New Band Grid)
/// - Unworked New Grid (all-time): gold 4-point star
/// - Unworked New Band Grid: cyan plus/target icon
/// - Normal unworked: subtle hollow square
fn draw_peer_icon(
    lcd: &mut impl Panel,
    x: i32,
    y: i32,
    st: crate::rad950::PeerCallStatus,
    selected: bool,
) {
    use crate::rad950::GridAward;
    if st.worked {
        let box_col = if selected { WHITE } else { GREEN };
        let chk_col = BG;
        lcd.fill(x, y, 15, 15, box_col);
        // Checkmark stroke inside the 15x15 badge
        lcd.fill(x + 3, y + 7, 2, 2, chk_col);
        lcd.fill(x + 5, y + 9, 2, 2, chk_col);
        lcd.fill(x + 7, y + 7, 2, 2, chk_col);
        lcd.fill(x + 9, y + 5, 2, 2, chk_col);
        lcd.fill(x + 11, y + 3, 2, 2, chk_col);
        // Corner pip indicating New Grid (amber) or New Band Grid (cyan) earned by this QSO
        match st.award {
            GridAward::NewGrid => lcd.fill(x + 11, y, 4, 4, AMBER),
            GridAward::NewBandGrid => lcd.fill(x + 11, y, 4, 4, CYAN),
            GridAward::None => {}
        }
    } else {
        match st.award {
            GridAward::NewGrid => {
                // Bright gold 4-point star (needed all-time grid!)
                let col = if selected { WHITE } else { AMBER };
                lcd.fill(x + 6, y + 1, 3, 13, col);
                lcd.fill(x + 1, y + 6, 13, 3, col);
                lcd.fill(x + 4, y + 4, 7, 7, col);
            }
            GridAward::NewBandGrid => {
                // Bright cyan plus inside frame (needed on this band!)
                let col = if selected { WHITE } else { CYAN };
                lcd.fill(x + 2, y + 2, 11, 1, col);
                lcd.fill(x + 2, y + 12, 11, 1, col);
                lcd.fill(x + 2, y + 2, 1, 11, col);
                lcd.fill(x + 12, y + 2, 1, 11, col);
                lcd.fill(x + 6, y + 4, 3, 7, col);
                lcd.fill(x + 4, y + 6, 7, 3, col);
            }
            GridAward::None => {
                let col = if selected { BLUE_FG } else { DIM };
                lcd.fill(x + 4, y + 4, 7, 1, col);
                lcd.fill(x + 4, y + 10, 7, 1, col);
                lcd.fill(x + 4, y + 4, 1, 7, col);
                lcd.fill(x + 10, y + 4, 1, 7, col);
            }
        }
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
        suffix: 0,
        grid4: ferrite_core::rad950::urad950::GRID_UNKNOWN,
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

impl View for Rad950Scr {
    fn name(self) -> &'static str {
        match self.page {
            PAGE_ACTIVE => "RAD950 QSO",
            PAGE_SETUP => "RAD950 Setup",
            PAGE_LOG | PAGE_LOG_DETAIL => "RAD950 Log",
            PAGE_STATS => "RAD950 Stats",
            PAGE_GRIDS_OVERVIEW | PAGE_GRIDS_FIELD => "RAD950 Grids",
            PAGE_CHAT => "RAD950 Chat",
            PAGE_CHAT_REPLY => "RAD950 Reply",
            PAGE_CHAT_PICK => "RAD950 Pick",
            PAGE_CHAT_PREVIEW => "RAD950 Send",
            PAGE_CHAT_MORE => "RAD950 More",
            PAGE_DISTRESS => "RAD950 Distress",
            PAGE_OPERATE_MENU => "RAD950 Menu",
            PAGE_RENDEZVOUS => "RAD950 Rendezvous",
            _ => "RAD950",
        }
    }

    fn title(self, _app: &App, b: &mut Buf<10>) {
        let t: &[u8] = match self.page {
            PAGE_ACTIVE => b"RAD QSO",
            PAGE_SETUP => b"RAD SET",
            PAGE_LOG | PAGE_LOG_DETAIL => b"RAD LOG",
            PAGE_STATS => b"RAD STAT",
            PAGE_GRIDS_OVERVIEW | PAGE_GRIDS_FIELD => b"RAD MAP",
            PAGE_CHAT => b"RAD CHAT",
            PAGE_CHAT_REPLY => b"REPLY",
            PAGE_CHAT_PICK => b"PICK",
            PAGE_CHAT_PREVIEW => b"SEND",
            PAGE_CHAT_MORE => b"MORE",
            PAGE_DISTRESS => b"DISTRESS",
            PAGE_OPERATE_MENU => b"RAD MENU",
            PAGE_RENDEZVOUS => b"RENDEZV",
            _ => b"RAD950",
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
            PAGE_CHAT => self.key_chat(app, e.key),
            PAGE_CHAT_REPLY => self.key_chat_reply(app, e.key, store),
            PAGE_CHAT_PICK => self.key_chat_pick(app, e.key),
            PAGE_CHAT_PREVIEW => self.key_chat_preview(app, e.key),
            PAGE_CHAT_MORE => self.key_chat_more(app, e.key),
            PAGE_DISTRESS => self.key_distress(app, e.key),
            PAGE_OPERATE_MENU => self.key_operate_menu(app, e.key),
            PAGE_RENDEZVOUS => self.key_rendezvous(app, e.key),
            _ => app.screen = Screen::Rad950(Rad950Scr::OPERATE),
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

    fn ptt(mut self, app: &mut App, down: bool) {
        if self.page != PAGE_DISTRESS {
            // Never close RAD950 or jump to FM voice when PTT is pressed inside RAD950.
            if down {
                app.show_toast(b"RAD950 active", b"Use 1/OK to TX", false);
            }
            return;
        }
        // On the Distress screen, PTT never keys FM voice; after confirmation (`field == 1`),
        // require 3 consecutive PTT presses, holding the 3rd press for 700 ms to initiate TX.
        if self.field == 0 {
            return;
        }
        let now = app.now;
        if down {
            if self.top < 3 {
                self.top += 1;
            }
            if self.top == 3 {
                if let Some(h) = app.rad950_host_mut() {
                    h.chat.distress.ptt_hold_start_ms = Some(now);
                }
            }
        } else if self.top == 3 {
            // Released before the 700 ms hold completed: step back to 2/3 so a quick tap cannot fire.
            self.top = 2;
            if let Some(h) = app.rad950_host_mut() {
                h.chat.distress.ptt_hold_start_ms = None;
            }
        }
        app.screen = Screen::Rad950(self);
    }

    fn tick(self, app: &mut App, _live: &LiveAll) {
        use ferrite_core::rad950::urad950::frame::encode_grid4;
        // Apply pending Chat auto-QSY (on Accept), auto-return (on End), or Rendezvous band hop.
        if app.rad950_host_mut().is_some_and(|h| h.chat.take_retune() || h.rdv.take_retune()) {
            app.rad950_retune();
        }
        if self.page != PAGE_DISTRESS || self.field != 1 || self.top < 3 {
            return;
        }
        let now = app.now;
        let held_ok = app
            .rad950_host()
            .and_then(|h| h.chat.distress.ptt_hold_start_ms)
            .is_some_and(|t0| now.wrapping_sub(t0) >= 700);
        if !held_ok {
            return;
        }
        let armed = if let Some(h) = app.rad950_host_mut() {
            h.chat.distress.ptt_hold_start_ms = None;
            if let Some(me) = h.profile.callsign() {
                let (g4_bytes, _) = h.profile.effective_grid(h.gps_grid);
                let g4 = encode_grid4(&g4_bytes);
                let dial = h.profile.dial();
                h.chat.distress.arm_tx(me, g4, dial);
                true
            } else {
                false
            }
        } else {
            false
        };
        let ret_screen = if self.sub == 1 { Rad950Scr::CHAT } else { Rad950Scr::OPERATE };
        if armed {
            app.show_toast(b"DISTRESS", b"Alert queued next slot", false);
            app.screen = Screen::Rad950(ret_screen);
        } else {
            app.show_toast(b"DISTRESS", b"Set callsign first", false);
            app.screen = Screen::Rad950(Rad950Scr { field: 0, top: 0, ..self });
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
            PAGE_CHAT => self.draw_chat(app, lcd, cx.live),
            PAGE_CHAT_REPLY => self.draw_chat_reply(app, lcd),
            PAGE_CHAT_PICK => self.draw_chat_pick(app, lcd),
            PAGE_CHAT_PREVIEW => self.draw_chat_preview(app, lcd),
            PAGE_CHAT_MORE => self.draw_chat_more(app, lcd),
            PAGE_DISTRESS => self.draw_distress(app, lcd),
            PAGE_OPERATE_MENU => self.draw_operate_menu(app, lcd),
            PAGE_RENDEZVOUS => self.draw_rendezvous(app, lcd, cx.live),
            _ => self.draw_operate(app, lcd, cx.live),
        }
    }

    fn hints(self, app: &App) -> Hints {
        match self.page {
            PAGE_OPERATE => Hints::new(&[("OK", "Answer"), ("1", "CQ"), ("#", "Menu")]),
            PAGE_ACTIVE => {
                if app.rad950_host().is_some_and(|h| h.saved_ack && h.last_qso.is_some()) {
                    Hints::new(&[("2", "Chat"), ("0", "Stop"), ("BACK", "List")])
                } else {
                    Hints::new(&[("0", "Stop"), ("*", "Setup"), ("BACK", "List")])
                }
            }
            PAGE_SETUP => Hints::new(&[(ARROWS_LR, "Change"), ("BACK", "Done")]),
            PAGE_LOG => Hints::new(&[("OK", "Detail"), ("BACK", "Back")]),
            PAGE_LOG_DETAIL => Hints::new(&[("BACK", "Log")]),
            PAGE_STATS => Hints::new(&[(ARROWS_LR, "Scope"), ("BACK", "Back")]),
            PAGE_GRIDS_OVERVIEW => Hints::new(&[("OK", "Zoom+"), ("*", "Band"), ("BACK", "Out")]),
            PAGE_GRIDS_FIELD => Hints::new(&[("OK", "Log"), ("*", "Band"), ("BACK", "Zoom-")]),
            PAGE_CHAT => {
                if app.rad950_host().is_some_and(|h| h.chat.phase == ChatPhase::IncomingInvite) {
                    Hints::new(&[("OK", "Accept"), ("0", "Decline"), ("BACK", "Back")])
                } else if app.rad950_host().is_some_and(|h| h.chat.pending_offer.is_some()) {
                    Hints::new(&[("OK", "Accept"), ("1", "Counter"), ("0", "Decline")])
                } else if app.rad950_host().is_some_and(|h| h.chat.active_move.is_some_and(|m| !m.expired && !m.retuned)) {
                    Hints::new(&[("OK", "Reply"), ("3", "Retune"), ("0", "End")])
                } else {
                    Hints::new(&[("OK", "Reply"), ("0", "End"), ("BACK", "Back")])
                }
            }
            PAGE_CHAT_REPLY | PAGE_CHAT_PICK | PAGE_CHAT_MORE | PAGE_OPERATE_MENU => {
                Hints::new(&[("OK", "Select"), ("BACK", "Back")])
            }
            PAGE_CHAT_PREVIEW => Hints::new(&[("OK", "Send"), ("BACK", "Back")]),
            PAGE_DISTRESS => Hints::new(&[(ARROWS_LR, "Change"), ("OK", "Send"), ("BACK", "Cancel")]),
            PAGE_RENDEZVOUS => Hints::new(&[("OK", "Toggle"), ("1-4", "Bands"), ("BACK", "Back")]),
            _ => Hints::new(&[("BACK", "Back")]),
        }
    }
}

impl Rad950Scr {
    fn key_operate(mut self, app: &mut App, k: Key) {
        let mut rows = [empty_row(); MAX_PEERS];
        let n = app.rad950_host().map_or(0, |h| h.engine.activity_rows(&mut rows));
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
                if app.rad950_host().is_some_and(|h| h.profile.coop_rx == CoopRxMode::Capture) {
                    app.show_toast(b"Coop Capture", b"RX only mode", false);
                    return;
                }
                if let Some(row) = rows.get(self.sel as usize).filter(|_| (self.sel as usize) < n) {
                    let peer = row.peer;
                    if let Some(h) = app.rad950_host_mut() {
                        let slot = if h.last_slot == u32::MAX { 0 } else { h.last_slot };
                        h.saved_ack = false;
                        h.target_peer = Some(peer);
                        h.engine.answer_peer(peer, slot);
                    }
                    app.screen = Screen::Rad950(Rad950Scr::ACTIVE);
                    return;
                }
            }
            Key::Digit(1) => {
                if app.rad950_host().is_some_and(|h| h.profile.coop_rx == CoopRxMode::Capture) {
                    app.show_toast(b"Coop Capture", b"RX only mode", false);
                    return;
                }
                if let Some(h) = app.rad950_host_mut() {
                    // Transmit on the very next 10 s UTC slot boundary (`(last_slot + 1) & 1`).
                    let parity = if h.last_slot == u32::MAX { 0 } else { (h.last_slot.wrapping_add(1) & 1) as u8 };
                    h.saved_ack = false;
                    h.target_peer = None;
                    h.engine.start_cq(parity);
                }
                app.screen = Screen::Rad950(Rad950Scr::ACTIVE);
                return;
            }
            Key::Digit(0) => {
                if let Some(h) = app.rad950_host_mut() {
                    h.engine.stop_session();
                    h.pending_tx = None;
                    h.target_peer = None;
                    h.auto_answer = AutoAnswer::Off;
                }
                app.out.push(RadioCmd::Rad950Abort);
            }
            Key::Digit(2) => {
                if let Some(h) = app.rad950_host_mut() {
                    h.log_grid_filter = None;
                }
                app.screen = Screen::Rad950(Rad950Scr::LOG);
                return;
            }
            Key::Digit(3) => {
                app.screen = Screen::Rad950(Rad950Scr::STATS);
                return;
            }
            Key::Digit(4) => {
                app.screen = Screen::Rad950(Rad950Scr::GRIDS);
                return;
            }
            Key::Digit(5) => {
                app.screen = Screen::Rad950(Rad950Scr { page: PAGE_RENDEZVOUS, sel: 0, top: 0, field: 0, sub: 0 });
                return;
            }
            Key::Digit(6) => {
                if app.rad950_host().is_some_and(|h| h.profile.coop_rx == CoopRxMode::Capture) {
                    app.show_toast(b"Coop Capture", b"RX only mode", false);
                    return;
                }
                if let Some(h) = app.rad950_host_mut() {
                    h.auto_answer = h.auto_answer.next();
                    let lbl = h.auto_answer.label();
                    app.show_toast(b"Auto-Answer", lbl, false);
                }
            }
            Key::Digit(7) => {
                if let Some(h) = app.rad950_host_mut() {
                    h.profile.mode60 = !h.profile.mode60;
                    h.engine.stop_session();
                    h.target_peer = None;
                    let m60 = h.profile.mode60;
                    app.show_toast(b"RAD950 Cycle", if m60 { b"60s Weak-Sig" } else { b"10s Standard" }, false);
                }
            }
            Key::Star => {
                app.screen = Screen::Rad950(Rad950Scr::SETUP);
                return;
            }
            Key::Hash => {
                app.screen = Screen::Rad950(Rad950Scr { page: PAGE_OPERATE_MENU, sel: 0, top: 0, field: 0, sub: 0 });
                return;
            }
            _ => {}
        }
        app.screen = Screen::Rad950(self);
    }

    fn key_active(self, app: &mut App, k: Key) {
        match k {
            Key::Exit | Key::Hash => app.screen = Screen::Rad950(Rad950Scr::OPERATE),
            Key::Digit(2) => {
                // Invite the completed QSO peer to continue in RAD950 Chat.
                let invited = if let Some(h) = app.rad950_host_mut() {
                    if let (true, Some(rec), Some(me)) = (h.saved_ack, h.last_qso, h.profile.callsign()) {
                        let slot = if h.last_slot == u32::MAX { 0 } else { h.last_slot };
                        let calling_dial = h.profile.dial();
                        h.chat.invite(me, rec.peer, slot, calling_dial);
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if invited {
                    app.screen = Screen::Rad950(Rad950Scr::CHAT);
                }
            }
            Key::Digit(0) => {
                if let Some(h) = app.rad950_host_mut() {
                    h.engine.stop_session();
                    h.pending_tx = None;
                    h.target_peer = None;
                }
                app.out.push(RadioCmd::Rad950Abort);
                app.screen = Screen::Rad950(Rad950Scr::OPERATE);
            }
            Key::Star => app.screen = Screen::Rad950(Rad950Scr::SETUP),
            _ => {}
        }
    }

    fn key_setup(mut self, app: &mut App, k: Key, store: &impl ChannelStore) {
        use super::text::{open as open_text, TextFor};
        let mut retune = false;
        match k {
            Key::Exit => {
                app.screen = Screen::Rad950(Rad950Scr::OPERATE);
                return;
            }
            Key::Up => {
                self.sel = if self.sel == 0 { (SETUP_ROWS - 1) as u8 } else { self.sel - 1 };
                if self.sel < self.top {
                    self.top = self.sel;
                } else if self.sel as usize >= self.top as usize + SETUP_VISIBLE {
                    self.top = (self.sel as usize + 1 - SETUP_VISIBLE) as u8;
                }
            }
            Key::Down => {
                self.sel = (self.sel + 1) % SETUP_ROWS as u8;
                if self.sel < self.top {
                    self.top = self.sel;
                } else if self.sel as usize >= self.top as usize + SETUP_VISIBLE {
                    self.top = (self.sel as usize + 1 - SETUP_VISIBLE) as u8;
                }
            }
            Key::Menu if self.sel == 14 => {
                app.screen = Screen::Rad950(Rad950Scr { page: PAGE_DISTRESS, sel: 0, top: 0, field: 0, sub: 0 });
                return;
            }
            Key::Menu if matches!(self.sel, 0 | 1 | 4) => {
                let target = match self.sel {
                    0 => TextFor::Rad950Call,
                    1 => TextFor::Rad950Prefix,
                    _ => TextFor::Rad950Grid,
                };
                open_text(app, target, store);
                return;
            }
            Key::Left | Key::Right | Key::Menu => {
                let dir: i32 = if k == Key::Left { -1 } else { 1 };
                if let Some(h) = app.rad950_host_mut() {
                    match self.sel {
                        2 => {
                            h.profile.suffix = (h.profile.suffix as i32 + dir).rem_euclid(16) as u8;
                        }
                        3 => {
                            h.profile.use_gps_grid = !h.profile.use_gps_grid;
                        }
                        5 => {
                            let n = RAD950_DIALS.len() as i32;
                            h.profile.dial_idx = (h.profile.dial_idx as i32 + dir).rem_euclid(n) as u8;
                            retune = true;
                        }
                        6 => {
                            h.profile.rx_corr_hz = (h.profile.rx_corr_hz as i32 + dir * 25).clamp(-5000, 5000) as i16;
                            retune = true;
                        }
                        7 => {
                            h.profile.tx_power = (h.profile.tx_power as i32 + dir).rem_euclid(3) as u8;
                            h.chat.station_card.pwr_dbm = match h.profile.tx_power {
                                0 => 30, // 1 W
                                1 => 34, // 2.5 W
                                _ => 37, // 5 W
                            };
                        }
                        8 => {
                            h.profile.coop_rx = match h.profile.coop_rx {
                                CoopRxMode::Off => CoopRxMode::Capture,
                                CoopRxMode::Capture => CoopRxMode::Off,
                            };
                            if h.profile.coop_rx == CoopRxMode::Capture {
                                h.engine.stop_session();
                                h.pending_tx = None;
                            }
                        }
                        9 => {
                            h.profile.coop_link = match h.profile.coop_link {
                                CoopLink::Usb => CoopLink::Bluetooth,
                                CoopLink::Bluetooth => CoopLink::Usb,
                            };
                        }
                        10 => {
                            h.chat.station_card.rig = ((h.chat.station_card.rig as i32 + dir).rem_euclid(4)) as u16;
                        }
                        11 => {
                            h.chat.station_card.ant = ((h.chat.station_card.ant as i32 + dir).rem_euclid(6)) as u8;
                        }
                        12 => {
                            let cur = if h.chat.station_card.height_m == 127 { 0 } else { h.chat.station_card.height_m as i32 };
                            let next = (cur + dir * 2).clamp(0, 40);
                            h.chat.station_card.height_m = if next == 0 { 127 } else { next as u8 };
                        }
                        13 => {
                            h.chat.station_card.amp = !h.chat.station_card.amp;
                        }
                        _ => {}
                    }
                    h.profile_dirty = true;
                    h.sync_engine_config();
                }
                if retune {
                    app.rad950_retune();
                }
            }
            _ => {}
        }
        app.screen = Screen::Rad950(self);
    }

    fn draw_operate(self, app: &mut App, lcd: &mut impl Panel, live: &LiveAll) {
        let (rx_ready, tx_active, rssi, loading) = match live.mode {
            ModeLive::Rad950 { rx_ready, tx_active, rssi, loading } => (rx_ready, tx_active, rssi, loading),
            _ => (false, false, -127, 255),
        };
        let mut id_buf = [0u8; 16];
        let mut id_len = 0usize;
        let mut grid_bytes = *b"----";
        let mut grid_src = GridSource::Unset;
        let mut dial = RAD950_DIALS[0];
        let mut utc_ok = false;
        let mut clock_src = ClockSource::None;
        let mut has_pnt = false;
        let mut auto_answer = AutoAnswer::Off;
        let mut mode_tag = 0u32;
        let mut coop_rx = CoopRxMode::Off;
        let mut coop_link = CoopLink::Usb;
        let mut rows = [empty_row(); MAX_PEERS];
        let mut n_rows = 0usize;
        let mut rx_distress = None;
        let mut tx_now = tx_active;
        if let Some(h) = app.rad950_host() {
            id_len = h.profile.format_identity(&mut id_buf);
            (grid_bytes, grid_src) = h.profile.effective_grid(h.gps_grid);
            dial = h.profile.dial();
            utc_ok = h.utc_ready();
            clock_src = h.clock_src;
            has_pnt = h.has_pnt();
            auto_answer = h.auto_answer;
            coop_rx = h.profile.coop_rx;
            coop_link = h.profile.coop_link;
            tx_now |= h.tx_in_flight;
            mode_tag = match h.engine.mode() {
                OpMode::Monitor => 0,
                OpMode::AutoCq { parity } => 1 + parity as u32,
                OpMode::Pounce => 3,
            };
            n_rows = h.engine.activity_rows(&mut rows);
            rx_distress = h.chat.distress.rx;
        }

        let top_h = Mix::new()
            .b(&id_buf[..id_len])
            .b(&grid_bytes)
            .n(grid_src as u32)
            .n(dial)
            .n(utc_ok as u32)
            .n(clock_src as u32)
            .n(has_pnt as u32)
            .n(auto_answer as u32)
            .n(rx_ready as u32)
            .n(tx_now as u32)
            .n(rssi as u32)
            .n(loading as u32)
            .n(mode_tag)
            .n(coop_rx as u32)
            .n(coop_link as u32)
            .n(rx_distress.map_or(0, |d| d.alert.sender.raw() ^ ((d.status as u32) << 28)))
            .get();
        if app.dirty(R_TOP, top_h) {
            let y0 = TOP_Y;
            redraw_box!(true, lcd, (0, y0, W, 44, BG), {
                if let Some(inc) = rx_distress {
                    use crate::rad950_chat::DistressRxStatus;
                    lcd.fill(0, y0, W, 43, RED);
                    let mut cb = [0u8; 6];
                    let cn = inc.alert.sender.decode(&mut cb);
                    let cs = core::str::from_utf8(&cb[..cn]).unwrap_or("?");
                    let mut l1 = Buf::<32>::new();
                    let _ = uwrite!(l1, "DISTRESS {} [{}]", cs, inc.alert.category.label());
                    lcd.text4(8, y0 + 16, l1.as_bytes(), &UI_12B, WHITE, RED);
                    let mut l2 = Buf::<34>::new();
                    match inc.status {
                        DistressRxStatus::AlertReceived => {
                            let _ = uwrite!(l2, "Alert received");
                        }
                        DistressRxStatus::PositionPending => {
                            let _ = uwrite!(l2, "Position pending...");
                        }
                        DistressRxStatus::PositionLinked => {
                            let lat = inc.lat_e4.unwrap_or(0);
                            let lon = inc.lon_e4.unwrap_or(0);
                            let lf = (lat.abs() % 10_000) / 100;
                            let of = (lon.abs() % 10_000) / 100;
                            let _ = uwrite!(l2, "Pos {}.{}{}, {}.{}{}", lat / 10_000, lf / 10, lf % 10, lon / 10_000, of / 10, of % 10);
                        }
                        DistressRxStatus::PositionConflict => {
                            let _ = uwrite!(l2, "Alert ok (pos conflict)");
                        }
                    }
                    lcd.text4(8, y0 + 35, l2.as_bytes(), &UI_12, WHITE, RED);
                    lcd.fill(0, y0 + 42, W, 1, RULE);
                } else {
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
                    if tx_now {
                        draw_tx_flag(lcd, W - 36, y0 + 3);
                    }

                    let mut line2 = Buf::<36>::new();
                    push_khz3(&mut line2, dial);
                    let st = if loading < 100 {
                        " LOAD"
                    } else if tx_now {
                        ""
                    } else if rx_ready {
                        " RX"
                    } else {
                        " INIT"
                    };
                    let (utc_s, gps_ok) = match (utc_ok, clock_src, has_pnt) {
                        (true, ClockSource::Gps, true) => (" GPS PNT", true),
                        (true, ClockSource::Gps, false) => (" GPS CLK", true),
                        (true, ClockSource::GpsHoldover, _) => (" GPS HOLD", true),
                        (true, ClockSource::Bench, _) => (" BENCH CLK", false),
                        _ => (" NO GPS", false),
                    };
                    let _ = uwrite!(line2, "{}{}", st, utc_s);
                    let col = if tx_now { AMBER } else if gps_ok && rx_ready { GREEN } else { AMBER };
                    lcd.text4(8, y0 + 35, line2.as_bytes(), &UI_12, col, BG);
                    if coop_rx == CoopRxMode::Capture {
                        let badge: &[u8] = match coop_link {
                            CoopLink::Usb => b"COOP USB",
                            CoopLink::Bluetooth => b"COOP BT",
                        };
                        lcd.text4(W - 64, y0 + 35, badge, &UI_12, BLUE, BG);
                    } else if auto_answer != AutoAnswer::Off {
                        lcd.text4(W - 68, y0 + 35, auto_answer.badge(), &UI_12, CYAN, BG);
                    } else if app.rad950_mode60() {
                        lcd.text4(W - 56, y0 + 35, b"60s WS", &UI_12, CYAN, BG);
                    }
                    lcd.fill(0, y0 + 42, W, 1, RULE);
                }
            });
        }

        let list_y = TOP_Y + 46;
        let row_h = 34;
        for i in 0..OPERATE_ROWS {
            let idx = self.top as usize + i;
            let selected = idx == self.sel as usize && idx < n_rows;
            let row = (idx < n_rows).then(|| rows[idx]);
            let peer_status = row.map(|r| {
                let mut cb = [0u8; 6];
                let cn = r.peer.decode(&mut cb);
                let gb = decode_grid4(r.grid4).unwrap_or(*b"----");
                app.rad950_host().map_or(
                    crate::rad950::PeerCallStatus { worked: r.logged, award: crate::rad950::GridAward::None },
                    |h| h.store.classify_peer(&cb[..cn], gb, dial, r.logged),
                )
            });
            let mut rh = Mix::new().n(selected as u32).n(idx as u32).n(dial);
            if let (Some(r), Some(st)) = (row, peer_status) {
                rh = rh
                    .n(r.peer.raw())
                    .n(r.grid4 as u32)
                    .n(r.snr_db as u32)
                    .n(st.worked as u32)
                    .n(st.award as u32);
            }
            if app.dirty(R_ROW + i, rh.get()) {
                use crate::rad950::GridAward;
                let y = list_y + i as i32 * row_h;
                let bg = if selected { BLUE } else { BG };
                let fg = if selected { BLUE_FG } else { TEXT };
                let dim = if selected { BLUE_FG } else { DIM };
                redraw_box!(true, lcd, (0, y, W, row_h, bg), {
                    if let (Some(r), Some(st)) = (row, peer_status) {
                        let mut cb = [0u8; 6];
                        let cn = r.peer.decode(&mut cb);
                        let cs = core::str::from_utf8(&cb[..cn]).unwrap_or("?");
                        let mut call_disp = Buf::<12>::new();
                        if r.suffix > 0 && r.suffix <= 15 {
                            let _ = uwrite!(call_disp, "{}-{}", cs, r.suffix);
                        } else {
                            let _ = uwrite!(call_disp, "{}", cs);
                        }
                        let gb = decode_grid4(r.grid4).unwrap_or(*b"----");
                        let gs = core::str::from_utf8(&gb).unwrap_or("----");

                        // 16x16 icon: green checkmark (worked), gold star (new grid), cyan plus (new band grid)
                        draw_peer_icon(lcd, 8, y + 9, st, selected);

                        let call_col = if selected {
                            BLUE_FG
                        } else if st.worked {
                            GREEN
                        } else {
                            fg
                        };
                        let grid_col = if selected {
                            BLUE_FG
                        } else if st.worked {
                            GREEN
                        } else {
                            match st.award {
                                GridAward::NewGrid => AMBER,
                                GridAward::NewBandGrid => CYAN,
                                GridAward::None => DIM,
                            }
                        };
                        lcd.text4(30, y + 22, call_disp.as_bytes(), &UI_14, call_col, bg);
                        lcd.text4(104, y + 22, gs.as_bytes(), &UI_14, grid_col, bg);

                        // Compact badge tag (NEW / BND / WKD* / WKD+ / WKD)
                        let (tag, tcol): (&[u8], u16) = if st.worked {
                            match st.award {
                                GridAward::NewGrid => (b"WKD*", if selected { BLUE_FG } else { AMBER }),
                                GridAward::NewBandGrid => (b"WKD+", if selected { BLUE_FG } else { CYAN }),
                                GridAward::None => (b"WKD", if selected { BLUE_FG } else { GREEN }),
                            }
                        } else {
                            match st.award {
                                GridAward::NewGrid => (b"NEW", if selected { BLUE_FG } else { AMBER }),
                                GridAward::NewBandGrid => (b"BND", if selected { BLUE_FG } else { CYAN }),
                                GridAward::None => (b"", dim),
                            }
                        };
                        if !tag.is_empty() {
                            lcd.text4(150, y + 22, tag, &UI_12B, tcol, bg);
                        }

                        let mut right = Buf::<12>::new();
                        let _ = uwrite!(right, "{}dB", r.snr_db);
                        lcd.text4(W - 44, y + 22, right.as_bytes(), &UI_12, dim, bg);
                    } else if i == 0 && n_rows == 0 {
                        let mut empty = Buf::<28>::new();
                        let _ = uwrite!(empty, "Listening on {} USB...", band_label(dial));
                        lcd.text4(8, y + 22, empty.as_bytes(), &UI_12, DIM, BG);
                    }
                });
            }
        }
        self.draw_panafall(app, lcd);
    }

    /// Scrolling right-to-left horizontal panafall of the `586..2,344 Hz` RAD950 grid (`224 x 48 px`) along the bottom.
    #[inline(never)]
    fn draw_panafall(self, app: &mut App, lcd: &mut impl Panel) {
        const WF_PALETTE: [u16; 16] = [
            0x0000, 0x0821, 0x0864, 0x08C8, 0x114C, 0x11D1, 0x1A75, 0x1B39,
            0x2439, 0x2554, 0x366D, 0x5728, 0x9FA4, 0xDF02, 0xFD60, 0xFFE6,
        ];
        const TX_PALETTE: [u16; 16] = [
            0x1000, 0x2000, 0x3000, 0x4000, 0x5800, 0x7000, 0x8800, 0xA000,
            0xB800, 0xD000, 0xE820, 0xF880, 0xFA04, 0xFBA8, 0xFD8E, 0xFEF6,
        ];
        let Some(h) = app.rad950_host() else { return };
        let seq = h.wf.seq;
        let tx_now = h.tx_in_flight;
        let slot_sec = if h.utc_sec == 0 { 0 } else { (h.utc_sec % 10) as u8 };
        let wf_h = Mix::new().n(seq).n(slot_sec as u32).n(tx_now as u32).get();
        if app.dirty(R_NOTE, wf_h) {
            let y0 = 210i32;
            let wf_x = 12i32;
            let wf_y = y0 + 18; // 228..276 (48 px tall)
            let wf_w = (WF_COLS * 2) as i32; // 224 px (x = 12..236)
            let wf_ht = (WF_ROWS * 2) as i32; // 48 px
            redraw_box!(false, lcd, (0, y0, W, 70, BG), {
                lcd.fill(0, y0, W, 1, RULE);
                lcd.fill(0, y0 + 1, W, 17, BG);
                lcd.text4(6, y0 + 14, b"586-2344 Hz  225Hz", &UI_12, DIM, BG);
                if tx_now {
                    draw_tx_flag(lcd, W - 74, y0 + 1);
                }
                let mut tr = Buf::<8>::new();
                let _ = uwrite!(tr, "T+{}s", slot_sec);
                lcd.text4(W - 40, y0 + 14, tr.as_bytes(), &UI_12, if tx_now { RED } else { AMBER }, BG);

                // Subtle 225 Hz grid channel ticks along the left margin (every 3 rows ~ 220 Hz).
                for ch in 0..8i32 {
                    let ty = wf_y + ch * 6;
                    lcd.fill(4, ty, 6, 1, RULE);
                }
                lcd.fill(wf_x - 1, wf_y - 1, wf_w + 2, 1, RULE);
                lcd.fill(wf_x - 1, wf_y + wf_ht, wf_w + 2, 1, RULE);
                lcd.fill(wf_x - 1, wf_y, 1, wf_ht, RULE);
                lcd.fill(wf_x + wf_w, wf_y, 1, wf_ht, RULE);
            });

            // Stream the 224 x 48 panafall in one SPI window (inclusive x1/y1; high freq at top, newest at right).
            if let Some(host) = app.rad950_host() {
                lcd.window(wf_x, wf_y, wf_x + wf_w - 1, wf_y + wf_ht - 1);
                let mut line = [0u16; WF_COLS * 2];
                for r_top in 0..WF_ROWS {
                    let bin = WF_ROWS - 1 - r_top;
                    let is_grid_line = r_top % 3 == 0 && r_top > 0;
                    for col in 0..WF_COLS {
                        let lvl = host.wf.level(col, bin) as usize;
                        let is_tx = host.wf.is_tx_col(col);
                        let mut px = if is_tx { TX_PALETTE[lvl & 15] } else { WF_PALETTE[lvl & 15] };
                        if host.wf.is_slot_mark(col) && lvl < 4 {
                            px = if is_tx { 0x6000 } else { 0x2945 }; // vertical 10 s slot divider
                        } else if is_grid_line && lvl < 2 {
                            px = if is_tx { 0x2800 } else { 0x1082 }; // faint 225 Hz channel guide
                        }
                        line[col * 2] = px;
                        line[col * 2 + 1] = px;
                    }
                    lcd.push_buf(&line);
                    lcd.push_buf(&line);
                }
            }
        }
    }

    fn draw_active(self, app: &mut App, lcd: &mut impl Panel, live: &LiveAll) {
        let mut tx_active = matches!(live.mode, ModeLive::Rad950 { tx_active: true, .. });
        let mut rows = [empty_row(); MAX_PEERS];
        let (mode, active_row, saved_ack, notice_len) = if let Some(h) = app.rad950_host() {
            tx_active |= h.tx_in_flight;
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
        if app.dirty(R_TOP, h) {
            let y0 = TOP_Y;
            redraw_box!(true, lcd, (0, y0, W, 180, BG), {
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
                if tx_active {
                    draw_tx_flag(lcd, W - 36, y0 + 13);
                }

                let act_col = if tx_active { RED } else if saved_ack { GREEN } else { BLUE };
                lcd.text4(10, y0 + 62, action, &UI_14, act_col, BG);

                if let Some(r) = active_row {
                    let mut sline = Buf::<24>::new();
                    let _ = uwrite!(sline, "Last SNR: {} dB", r.snr_db);
                    lcd.text4(10, y0 + 90, sline.as_bytes(), &UI_12, DIM, BG);
                }
                if saved_ack {
                    use crate::rad950::GridAward;
                    let award = app.rad950_host().map_or(GridAward::None, |h| h.last_qso_award);
                    let (msg, col): (&[u8], u16) = match award {
                        GridAward::NewGrid => (b"Saved - NEW GRID!", AMBER),
                        GridAward::NewBandGrid => (b"Saved - NEW BAND GRID!", CYAN),
                        GridAward::None => (b"Saved to flash log", GREEN),
                    };
                    lcd.text4(10, y0 + 118, msg, &UI_14, col, BG);
                }
                if let Some(host) = app.rad950_host() {
                    let nb = host.notice_bytes();
                    if !nb.is_empty() {
                        lcd.text4(10, y0 + 146, nb, &UI_12, AMBER, BG);
                    }
                }
            });
        }
        self.draw_panafall(app, lcd);
    }

    fn draw_setup(self, app: &mut App, lcd: &mut impl Panel) {
        let Some(h) = app.rad950_host() else { return };
        let prof = h.profile;
        let sc = h.chat.station_card;
        let gps_grid = h.gps_grid;
        let y0 = TOP_Y + 4;
        let row_h = 24;
        for slot in 0..SETUP_VISIBLE {
            let i = self.top as usize + slot;
            if i >= SETUP_ROWS {
                break;
            }
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
                    let d = prof.dial();
                    let _ = uwrite!(val, "{} ", band_label(d));
                    push_khz3(&mut val, d);
                    b"USB dial"
                }
                6 => {
                    let _ = uwrite!(val, "{}Hz", prof.rx_corr_hz);
                    b"RX offset"
                }
                7 => {
                    let p = match prof.tx_power {
                        0 => "Low",
                        1 => "Mid",
                        _ => "High",
                    };
                    let _ = uwrite!(val, "{}", p);
                    b"TX power"
                }
                8 => {
                    let c = match prof.coop_rx {
                        CoopRxMode::Off => "Off",
                        CoopRxMode::Capture => "Capture",
                    };
                    let _ = uwrite!(val, "{}", c);
                    b"Coop RX"
                }
                9 => {
                    let l = match prof.coop_link {
                        CoopLink::Usb => "USB",
                        CoopLink::Bluetooth => "Bluetooth",
                    };
                    let _ = uwrite!(val, "{}", l);
                    b"Coop link"
                }
                10 => {
                    use ferrite_core::rad950::urad950::chat::rig_name;
                    let _ = uwrite!(val, "{}", rig_name(sc.rig));
                    b"Card rig"
                }
                11 => {
                    use ferrite_core::rad950::urad950::chat::ant_name;
                    let _ = uwrite!(val, "{}", ant_name(sc.ant));
                    b"Card ant"
                }
                12 => {
                    if sc.height_m == 127 {
                        let _ = uwrite!(val, "Unset");
                    } else {
                        let _ = uwrite!(val, "{} m", sc.height_m);
                    }
                    b"Ant height"
                }
                13 => {
                    let _ = uwrite!(val, "{}", if sc.amp { "Ext amp" } else { "Barefoot" });
                    b"Card amp"
                }
                _ => {
                    let _ = uwrite!(val, "Open");
                    b"Send distress..."
                }
            };
            let rh = Mix::new().n(i as u32).n(selected as u32).b(label).b(val.as_bytes()).get();
            if app.dirty(R_ROW + slot, rh) {
                let y = y0 + slot as i32 * row_h;
                let bg = if selected { BLUE } else { BG };
                let fg = if selected { BLUE_FG } else { TEXT };
                redraw_box!(true, lcd, (0, y, W, row_h, bg), {
                    lcd.text4(8, y + 16, label, &UI_12, fg, bg);
                    lcd.text4(W - 96, y + 16, val.as_bytes(), &UI_12, fg, bg);
                });
            }
        }
        // Live identity + effective grid preview at bottom of Setup.
        let mut id_buf = [0u8; 16];
        let id_len = prof.format_identity(&mut id_buf);
        let (eff_g, _) = prof.effective_grid(gps_grid);
        let ph = Mix::new().b(&id_buf[..id_len]).b(&eff_g).get();
        if app.dirty(R_NOTE, ph) {
            let y = y0 + SETUP_VISIBLE as i32 * row_h + 6;
            redraw_box!(true, lcd, (0, y, W, 24, BG), {
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
        let total = app.rad950_host().map_or(0, |h| {
            let mut tmp = [None; 32];
            h.store.read_newest(0, h.log_grid_filter, &mut tmp)
        });
        match k {
            Key::Exit => {
                if let Some(h) = app.rad950_host_mut() {
                    h.log_grid_filter = None;
                }
                app.screen = Screen::Rad950(Rad950Scr::OPERATE);
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
                app.screen = Screen::Rad950(Rad950Scr { page: PAGE_LOG_DETAIL, ..self });
                return;
            }
            _ => {}
        }
        app.screen = Screen::Rad950(self);
    }

    fn key_log_detail(self, app: &mut App, k: Key) {
        if k == Key::Exit {
            app.screen = Screen::Rad950(Rad950Scr { page: PAGE_LOG, ..self });
        }
    }

    fn key_stats(mut self, app: &mut App, k: Key) {
        match k {
            Key::Exit => {
                app.screen = Screen::Rad950(Rad950Scr::OPERATE);
                return;
            }
            Key::Left | Key::Right | Key::Menu => {
                self.sel ^= 1;
            }
            _ => {}
        }
        app.screen = Screen::Rad950(self);
    }

    fn key_grids_overview(mut self, app: &mut App, k: Key) {
        let mut lon_f = (self.field / 18).min(17) as i32;
        let mut lat_f = (self.field % 18) as i32;
        match k {
            Key::Exit => {
                if self.top > 0 {
                    self.top -= 1; // Zoom out: Region 3x -> World 1x
                } else {
                    app.screen = Screen::Rad950(Rad950Scr::OPERATE);
                    return;
                }
            }
            Key::Left => lon_f = (lon_f - 1).rem_euclid(18),
            Key::Right => lon_f = (lon_f + 1).rem_euclid(18),
            Key::Up => lat_f = (lat_f + 1).rem_euclid(18), // north is higher lat_f
            Key::Down => lat_f = (lat_f - 1).rem_euclid(18),
            Key::Star => self.sel = (self.sel + 1) % 5,
            Key::Digit(d @ 1..=5) => self.sel = d - 1,
            Key::Menu => {
                if self.top == 0 {
                    self.top = 1; // Zoom in: World 1x -> Region 3x
                } else {
                    app.screen = Screen::Rad950(Rad950Scr {
                        page: PAGE_GRIDS_FIELD,
                        field: self.field,
                        sub: 31,
                        sel: self.sel,
                        top: 1,
                    });
                    return;
                }
            }
            _ => {}
        }
        self.field = (lon_f * 18 + lat_f) as u16;
        app.screen = Screen::Rad950(self);
    }

    fn key_grids_field(mut self, app: &mut App, k: Key) {
        let mut lon_s = (self.sub / 10).min(9) as i32;
        let mut lat_s = (self.sub % 10) as i32;
        match k {
            Key::Exit => {
                app.screen = Screen::Rad950(Rad950Scr {
                    page: PAGE_GRIDS_OVERVIEW,
                    field: self.field,
                    sel: self.sel,
                    top: 1, // Return to 3x Region view
                    sub: self.sub,
                });
                return;
            }
            Key::Left => lon_s = (lon_s - 1).rem_euclid(10),
            Key::Right => lon_s = (lon_s + 1).rem_euclid(10),
            Key::Up => lat_s = (lat_s + 1).rem_euclid(10),
            Key::Down => lat_s = (lat_s - 1).rem_euclid(10),
            Key::Star => self.sel = (self.sel + 1) % 5,
            Key::Digit(d @ 1..=5) => self.sel = d - 1,
            Key::Menu => {
                let fl = field_letters(self.field);
                let g4 = [fl[0], fl[1], b'0' + lon_s as u8, b'0' + lat_s as u8];
                if let Some(h) = app.rad950_host_mut() {
                    h.log_grid_filter = Some(g4);
                }
                app.screen = Screen::Rad950(Rad950Scr::LOG);
                return;
            }
            _ => {}
        }
        self.sub = (lon_s * 10 + lat_s) as u8;
        app.screen = Screen::Rad950(self);
    }

    fn draw_log(self, app: &mut App, lcd: &mut impl Panel) {
        let mut page = [None; LOG_ROWS];
        let (n, count, full, filter) = if let Some(h) = app.rad950_host() {
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
        if app.dirty(R_TOP, th) {
            let y0 = TOP_Y;
            redraw_box!(true, lcd, (0, y0, W, 24, BG), {
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
            if app.dirty(R_ROW + i, rh.get()) {
                let y = list_y + i as i32 * row_h;
                let bg = if selected { BLUE } else { BG };
                let fg = if selected { BLUE_FG } else { TEXT };
                let dim = if selected { BLUE_FG } else { DIM };
                redraw_box!(true, lcd, (0, y, W, row_h, bg), {
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
        let rec: Option<SavedContact> = app.rad950_host().and_then(|h| {
            h.store.read_newest(self.sel as u16, h.log_grid_filter, &mut one);
            one[0]
        });
        let h = Mix::new().n(self.sel as u32).n(rec.map_or(0, |r| r.seq)).get();
        if app.dirty(R_TOP, h) {
            let y0 = TOP_Y + 6;
            redraw_box!(true, lcd, (0, y0, W, 200, BG), {
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
        let (st, utc_known) = if let Some(h) = app.rad950_host() {
            let day = if today_mode {
                (h.utc_sec != 0).then_some(h.utc_sec / 86_400)
            } else {
                None
            };
            let st = if today_mode && day.is_none() {
                crate::rad950::SavedStats::default()
            } else {
                h.store.stats(day)
            };
            (st, h.utc_sec != 0)
        } else {
            (crate::rad950::SavedStats::default(), false)
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
        if app.dirty(R_TOP, h) {
            let y0 = TOP_Y + 6;
            redraw_box!(true, lcd, (0, y0, W, 200, BG), {
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

    #[inline(never)]
    fn draw_grids_overview(self, app: &mut App, lcd: &mut impl Panel) {
        use crate::world_map::{is_coast, is_land, MAP_H, MAP_W};
        const OCEAN: u16 = 0x000A;
        const LAND: u16 = 0x19C4;
        const COAST: u16 = 0x3A68;
        const GRID_O: u16 = 0x0863;
        const GRID_L: u16 = 0x2A26;

        let band_opt = grid_band_opt(self.sel);
        let accent = grid_band_color(self.sel);
        let zoom_region = self.top > 0;
        let (mask, home_field) = if let Some(h) = app.rad950_host() {
            let (eff_g, _) = h.profile.effective_grid(h.gps_grid);
            let hf = crate::rad950::grid4_to_field_square(&eff_g).map(|(fi, _)| fi);
            (h.store.field_mask_for_band(band_opt), hf)
        } else {
            ([0u32; 11], None)
        };
        let worked_fields: u32 = mask.iter().map(|w| w.count_ones()).sum();
        let fl = field_letters(self.field);
        let sel_worked = {
            let i = self.field as usize;
            i < 324 && (mask[i / 32] & (1u32 << (i % 32))) != 0
        };
        let mut h = Mix::new()
            .n(self.field as u32)
            .n(self.sel as u32)
            .n(self.top as u32)
            .n(worked_fields);
        for w_word in mask {
            h = h.n(w_word);
        }
        if app.dirty(R_TOP, h.get()) {
            let y0 = TOP_Y;
            let sel_lon = (self.field / 18).min(17) as usize;
            let sel_lat = (self.field % 18).min(17) as usize;
            let start_lon = (sel_lon as i32 - 3).clamp(0, 12) as usize;
            let start_lat = (sel_lat as i32 - 2).clamp(0, 12) as usize;
            // Pre-fetch worked 10x10 square bitmasks ONCE (not per canvas band!) when in 3x Region mode.
            let mut sq_worked = [[0u32; 4]; 36];
            if zoom_region {
                if let Some(host) = app.rad950_host() {
                    for dl in 0..6usize {
                        for db in 0..6usize {
                            let fi = ((start_lon + dl) * 18 + (start_lat + db)) as u16;
                            let sqs = host.store.field_squares_for_band(fi, band_opt);
                            for (s, &cnt) in sqs.iter().enumerate() {
                                if cnt > 0 {
                                    sq_worked[dl * 6 + db][s / 32] |= 1u32 << (s % 32);
                                }
                            }
                        }
                    }
                }
            }
            redraw_box!(true, lcd, (0, y0, W, 268, BG), {
                let mut hdr = Buf::<32>::new();
                let fs = core::str::from_utf8(&fl).unwrap_or("??");
                let st = if sel_worked { "worked" } else { "empty" };
                let _ = uwrite!(hdr, "Field {} ({}) {}/324", fs, st, worked_fields);
                lcd.text4(8, y0 + 16, hdr.as_bytes(), &UI_12B, TEXT, BG);
                let ztag: &[u8] = if zoom_region { b"3x" } else { b"1x" };
                lcd.text4(W - 26, y0 + 16, ztag, &UI_12B, accent, BG);

                // Band filter pills: All / 10m / 12m / 15m / 17m
                draw_band_pills(lcd, y0 + 23, self.sel);

                let mx0 = 12i32;
                let my0 = y0 + 48;
                let mw = 216usize;
                let mh = 162usize;
                let (ry0, ry1) = lcd.rows();
                let py_start = (ry0 - my0).clamp(0, mh as i32) as usize;
                let py_end = (ry1 - my0).clamp(0, mh as i32) as usize;

                if py_start < py_end {
                    lcd.window(mx0, my0 + py_start as i32, mx0 + mw as i32 - 1, my0 + py_end as i32 - 1);
                    let mut line = [OCEAN; 216];
                    if !zoom_region {
                        // 1x World Map: 18x18 Maidenhead fields (12x9 px each) over 144x72 coastline map.
                        for py in py_start..py_end {
                            let lat_f = 17 - (py / 9).min(17);
                            let fy_in = py % 9;
                            let my = (py * MAP_H) / mh;
                            for (px, dst) in line.iter_mut().enumerate() {
                                let lon_f = (px / 12).min(17);
                                let fx_in = px % 12;
                                let mx = (px * MAP_W) / mw;
                                let land = is_land(mx, my);
                                let mut c = if land {
                                    if is_coast(mx, my) { COAST } else { LAND }
                                } else {
                                    OCEAN
                                };
                                if fx_in == 0 || fy_in == 0 {
                                    c = if land { GRID_L } else { GRID_O };
                                }
                                let idx = lon_f * 18 + lat_f;
                                let worked = (mask[idx / 32] & (1u32 << (idx % 32))) != 0;
                                if worked && (2..=9).contains(&fx_in) && (2..=6).contains(&fy_in) {
                                    c = accent;
                                }
                                if home_field == Some(idx as u16) && (5..=6).contains(&fx_in) && (4..=5).contains(&fy_in) {
                                    c = WHITE;
                                }
                                if lon_f == sel_lon && lat_f == sel_lat {
                                    if fx_in == 0 || fx_in == 11 || fy_in == 0 || fy_in == 8 {
                                        c = AMBER;
                                    } else if fx_in == 1 || fx_in == 10 || fy_in == 1 || fy_in == 7 {
                                        c = WHITE;
                                    }
                                }
                                *dst = c;
                            }
                            lcd.push_buf(&line);
                        }
                    } else {
                        // 3x Region Map: 6x6 Maidenhead fields (36x27 px each) centered on selected field,
                        // showing actual 10x10 worked 4-char grid squares directly over the continents!
                        for py in py_start..py_end {
                            let row_f = 5 - (py / 27).min(5);
                            let lat_f = start_lat + row_f;
                            let fy_in = py % 27;
                            let lat_s = 9 - ((fy_in * 10) / 27).min(9);
                            // Map pixel to 144x72 world mask (each field = 8x4 world pixels)
                            let my = (((17 - lat_f) * 4 * 27 + fy_in * 4) / 27).min(MAP_H - 1);
                            for (px, dst) in line.iter_mut().enumerate() {
                                let col_f = (px / 36).min(5);
                                let lon_f = start_lon + col_f;
                                let fx_in = px % 36;
                                let lon_s = ((fx_in * 10) / 36).min(9);
                                let mx = ((lon_f * 8 * 36 + fx_in * 8) / 36).min(MAP_W - 1);
                                let land = is_land(mx, my);
                                let mut c = if land {
                                    if is_coast(mx, my) { COAST } else { LAND }
                                } else {
                                    OCEAN
                                };
                                if fx_in == 0 || fy_in == 0 {
                                    c = if land { GRID_L } else { GRID_O };
                                }
                                let sq_i = lon_s * 10 + lat_s;
                                if (sq_worked[col_f * 6 + row_f][sq_i / 32] & (1u32 << (sq_i % 32))) != 0 {
                                    c = accent;
                                }
                                if lon_f == sel_lon && lat_f == sel_lat {
                                    if fx_in <= 1 || fx_in >= 34 || fy_in <= 1 || fy_in >= 25 {
                                        c = AMBER;
                                    } else if fx_in == 2 || fx_in == 33 || fy_in == 2 || fy_in == 24 {
                                        c = WHITE;
                                    }
                                }
                                *dst = c;
                            }
                            lcd.push_buf(&line);
                        }
                    }
                }

                // Bottom legend bar
                let ly = my0 + mh as i32 + 16;
                lcd.fill(12, ly - 10, 10, 10, accent);
                lcd.text4(26, ly, b"Worked", &UI_12, TEXT, BG);
                lcd.fill(82, ly - 10, 10, 10, LAND);
                lcd.text4(96, ly, b"Land", &UI_12, DIM, BG);
                lcd.fill(138, ly - 8, 6, 6, WHITE);
                lcd.text4(148, ly, b"Home", &UI_12, DIM, BG);
            });
        }
    }

    #[inline(never)]
    fn draw_grids_field(self, app: &mut App, lcd: &mut impl Panel) {
        use crate::world_map::{is_coast, is_land, MAP_H, MAP_W};
        const OCEAN: u16 = 0x000A;
        const LAND: u16 = 0x19C4;
        const COAST: u16 = 0x3A68;
        const GRID_O: u16 = 0x0863;
        const GRID_L: u16 = 0x2A26;

        let band_opt = grid_band_opt(self.sel);
        let accent = grid_band_color(self.sel);
        let sqs = app
            .rad950_host()
            .map_or([0u16; 100], |h| h.store.field_squares_for_band(self.field, band_opt));
        let fl = field_letters(self.field);
        let sel_sq = (self.sub as usize).min(99);
        let sel_cnt = sqs[sel_sq];
        let lon_s = sel_sq / 10;
        let lat_s = sel_sq % 10;
        let g4 = [fl[0], fl[1], b'0' + lon_s as u8, b'0' + lat_s as u8];
        let mut h = Mix::new().n(self.field as u32).n(self.sub as u32).n(self.sel as u32);
        for &c in &sqs {
            if c != 0 {
                h = h.n(c as u32);
            }
        }
        if app.dirty(R_TOP, h.get()) {
            let y0 = TOP_Y;
            redraw_box!(true, lcd, (0, y0, W, 268, BG), {
                let mut hdr = Buf::<32>::new();
                let gs = core::str::from_utf8(&g4).unwrap_or("----");
                let _ = uwrite!(hdr, "Grid {}: {} QSO", gs, sel_cnt);
                lcd.text4(8, y0 + 16, hdr.as_bytes(), &UI_12B, TEXT, BG);
                lcd.text4(W - 32, y0 + 16, b"10x", &UI_12B, accent, BG);

                // Band filter pills
                draw_band_pills(lcd, y0 + 23, self.sel);

                // 10x10 square grid (200x160 px, 20x16 per square) over zoomed coastline for this field.
                let gx = 20i32;
                let gy = y0 + 48;
                let mw = 200usize;
                let mh = 160usize;
                let lon_f = (self.field / 18).min(17) as usize;
                let lat_f = (self.field % 18).min(17) as usize;
                let (ry0, ry1) = lcd.rows();
                let py_start = (ry0 - gy).clamp(0, mh as i32) as usize;
                let py_end = (ry1 - gy).clamp(0, mh as i32) as usize;
                if py_start < py_end {
                    lcd.window(gx, gy + py_start as i32, gx + mw as i32 - 1, gy + py_end as i32 - 1);
                    let mut line = [OCEAN; 200];
                    for py in py_start..py_end {
                        let bs = 9 - (py / 16).min(9);
                        let sy_in = py % 16;
                        let my = (((17 - lat_f) * 4 * mh + py * 4) / mh).min(MAP_H - 1);
                        for (px, dst) in line.iter_mut().enumerate() {
                            let ls = (px / 20).min(9);
                            let sx_in = px % 20;
                            let mx = ((lon_f * 8 * mw + px * 8) / mw).min(MAP_W - 1);
                            let land = is_land(mx, my);
                            let mut c = if land {
                                if is_coast(mx, my) { COAST } else { LAND }
                            } else {
                                OCEAN
                            };
                            if sx_in == 0 || sy_in == 0 {
                                c = if land { GRID_L } else { GRID_O };
                            }
                            let idx = ls * 10 + bs;
                            if sqs[idx] > 0 && (3..=16).contains(&sx_in) && (3..=12).contains(&sy_in) {
                                c = accent;
                            }
                            if idx == sel_sq {
                                if sx_in == 0 || sx_in == 19 || sy_in == 0 || sy_in == 15 {
                                    c = AMBER;
                                } else if sx_in == 1 || sx_in == 18 || sy_in == 1 || sy_in == 14 {
                                    c = WHITE;
                                }
                            }
                            *dst = c;
                        }
                        lcd.push_buf(&line);
                    }
                }

                let ly = gy + mh as i32 + 16;
                lcd.text4(20, ly, b"OK Open log for grid", &UI_12, DIM, BG);
            });
        }
    }

    // ---- RAD950 Conversation Extension (Invite -> Ask -> Station Card -> Phrase -> Typed -> End) ----

    fn key_chat(mut self, app: &mut App, k: Key) {
        use ferrite_core::rad950::urad950::chat::RendezvousAction;
        let (phase, has_offer, has_move, move_expired, has_rem, count) = app.rad950_host().map_or(
            (ChatPhase::Idle, false, false, false, false, 0usize),
            |h| {
                (
                    h.chat.phase,
                    h.chat.pending_offer.is_some(),
                    h.chat.active_move.is_some(),
                    h.chat.active_move.is_some_and(|m| m.expired),
                    h.chat.saved_reminder.is_some(),
                    h.chat.entry_count as usize,
                )
            },
        );
        match k {
            Key::Exit => {
                app.screen = Screen::Rad950(Rad950Scr::OPERATE);
                return;
            }
            Key::Digit(0) => {
                if has_offer {
                    if let Some(h) = app.rad950_host_mut() {
                        let slot = if h.last_slot == u32::MAX { 0 } else { h.last_slot };
                        h.chat.respond_offer(RendezvousAction::Decline, slot);
                    }
                    return;
                }
                if move_expired {
                    if let Some(h) = app.rad950_host_mut() {
                        if let Some(src_dial) = h.chat.return_from_move() {
                            if let Some(idx) = RAD950_DIALS.iter().position(|&d| d == src_dial) {
                                h.profile.dial_idx = idx as u8;
                                h.sync_engine_config();
                            }
                        }
                    }
                    app.rad950_retune();
                    app.show_toast(b"RAD950", b"Returned to source", false);
                    return;
                }
                if let Some(h) = app.rad950_host_mut() {
                    if phase == ChatPhase::IncomingInvite {
                        h.chat.decline_invite();
                    } else {
                        h.chat.end_chat();
                    }
                }
                app.screen = Screen::Rad950(Rad950Scr::OPERATE);
                return;
            }
            Key::Digit(1) if has_offer => {
                if let Some(h) = app.rad950_host_mut() {
                    let slot = if h.last_slot == u32::MAX { 0 } else { h.last_slot };
                    h.chat.respond_offer(RendezvousAction::Counter, slot);
                }
                return;
            }
            Key::Digit(3) if has_move && !move_expired => {
                let mut retuned = false;
                if let Some(h) = app.rad950_host_mut() {
                    if let Some(dest_dial) = h.chat.confirm_move_retune() {
                        if let Some(idx) = RAD950_DIALS.iter().position(|&d| d == dest_dial) {
                            h.profile.dial_idx = idx as u8;
                            h.sync_engine_config();
                            retuned = true;
                        }
                    }
                }
                if retuned {
                    app.rad950_retune();
                    app.show_toast(b"RAD950", b"Retuned to move dial", false);
                }
                return;
            }
            Key::Digit(3) if !has_move && has_rem => {
                if let Some(h) = app.rad950_host_mut() {
                    if let Some(rem) = h.chat.saved_reminder {
                        h.profile.dial_idx = rem.dial_preset.min((RAD950_DIALS.len() - 1) as u8);
                        h.sync_engine_config();
                    }
                }
                app.rad950_retune();
                app.show_toast(b"RAD950", b"Tuned to reminder", false);
                return;
            }
            Key::Up if self.top > 0 => self.top -= 1,
            Key::Down if (self.top as usize) + 3 < count => self.top += 1,
            Key::Menu => {
                if phase == ChatPhase::IncomingInvite {
                    if let Some(h) = app.rad950_host_mut() {
                        h.chat.accept_invite();
                    }
                } else if has_offer {
                    if let Some(h) = app.rad950_host_mut() {
                        let slot = if h.last_slot == u32::MAX { 0 } else { h.last_slot };
                        h.chat.respond_offer(RendezvousAction::Accept, slot);
                    }
                } else if phase == ChatPhase::Active {
                    // Context-aware default: highlight "My station" (row 1) for antenna/rig, "More..." (row 4) for weather.
                    let default_sel = app
                        .rad950_host()
                        .and_then(|h| h.chat.last_peer_topic)
                        .map_or(0u8, |t| match t {
                            0 | 1 => 1,
                            2 => 4,
                            _ => 0,
                        });
                    app.screen = Screen::Rad950(Rad950Scr { page: PAGE_CHAT_REPLY, sel: default_sel, top: 0, field: 0, sub: 0 });
                    return;
                }
            }
            _ => {}
        }
        app.screen = Screen::Rad950(self);
    }

    fn key_chat_reply(mut self, app: &mut App, k: Key, store: &impl ChannelStore) {
        use super::text::{open as open_text, TextFor};
        let has_out = app.rad950_host().is_some_and(|h| h.chat.has_outgoing());
        let n_items: u8 = if has_out { 7 } else { 6 };
        match k {
            Key::Exit => {
                app.screen = Screen::Rad950(Rad950Scr::CHAT);
                return;
            }
            Key::Up => self.sel = if self.sel == 0 { n_items - 1 } else { self.sel - 1 },
            Key::Down => self.sel = (self.sel + 1) % n_items,
            Key::Menu => {
                match self.sel {
                    0 => {
                        app.screen = Screen::Rad950(Rad950Scr { page: PAGE_CHAT_PICK, sel: 0, top: 0, field: 0, sub: 0 });
                        return;
                    }
                    1 => {
                        if let Some(h) = app.rad950_host_mut() {
                            h.chat.draft = Some(ChatDraft::StationCard);
                        }
                        app.screen = Screen::Rad950(Rad950Scr::CHAT_PREVIEW);
                        return;
                    }
                    2 => {
                        app.screen = Screen::Rad950(Rad950Scr { page: PAGE_CHAT_PICK, sel: 0, top: 0, field: 0, sub: 1 });
                        return;
                    }
                    3 => {
                        open_text(app, TextFor::Rad950Chat, store);
                        return;
                    }
                    4 => {
                        app.screen = Screen::Rad950(Rad950Scr { page: PAGE_CHAT_MORE, sel: 0, top: 0, field: 0, sub: 0 });
                        return;
                    }
                    5 if has_out => {
                        if let Some(h) = app.rad950_host_mut() {
                            h.chat.cancel_send();
                        }
                        app.screen = Screen::Rad950(Rad950Scr::CHAT);
                        return;
                    }
                    _ => {
                        if let Some(h) = app.rad950_host_mut() {
                            h.chat.end_chat();
                        }
                        app.screen = Screen::Rad950(Rad950Scr::OPERATE);
                        return;
                    }
                }
            }
            _ => {}
        }
        app.screen = Screen::Rad950(self);
    }

    fn key_chat_more(mut self, app: &mut App, k: Key) {
        use ferrite_core::rad950::urad950::chat::{dial_to_move_band, RendezvousAction, RendezvousCard};
        const MORE_ITEMS: u8 = 5;
        match k {
            Key::Exit => {
                app.screen = Screen::Rad950(Rad950Scr::CHAT_REPLY);
                return;
            }
            Key::Up => self.sel = if self.sel == 0 { MORE_ITEMS - 1 } else { self.sel - 1 },
            Key::Down => self.sel = (self.sel + 1) % MORE_ITEMS,
            Key::Left | Key::Right => {
                let dir: i32 = if k == Key::Left { -1 } else { 1 };
                if let Some(h) = app.rad950_host_mut() {
                    match self.sel {
                        0 => {
                            h.chat.weather_card.cond = ((h.chat.weather_card.cond as i32 - 1 + dir).rem_euclid(7) + 1) as u8;
                        }
                        1 => {
                            let cur = if h.chat.about_card.year_licensed == 0 { 1994 } else { h.chat.about_card.year_licensed as i32 };
                            h.chat.about_card.year_licensed = (cur + dir).clamp(1960, 2026) as u16;
                        }
                        2 | 3 => {
                            let n = RAD950_DIALS.len() as i32;
                            h.chat.offer_dial_idx = (h.chat.offer_dial_idx as i32 + dir).rem_euclid(n) as u8;
                        }
                        _ => {}
                    }
                }
            }
            Key::Menu => {
                match self.sel {
                    0 => {
                        if let Some(h) = app.rad950_host_mut() {
                            h.chat.draft = Some(ChatDraft::WeatherCard);
                        }
                        app.screen = Screen::Rad950(Rad950Scr::CHAT_PREVIEW);
                        return;
                    }
                    1 => {
                        if let Some(h) = app.rad950_host_mut() {
                            h.chat.draft = Some(ChatDraft::AboutCard);
                        }
                        app.screen = Screen::Rad950(Rad950Scr::CHAT_PREVIEW);
                        return;
                    }
                    2 => {
                        if let Some(h) = app.rad950_host_mut() {
                            let utc_min = ((h.utc_sec / 60) % 1440) as u16;
                            h.chat.session_utc_day = h.utc_sec / 86_400;
                            h.chat.draft = Some(ChatDraft::Rendezvous(RendezvousCard::Schedule {
                                action: RendezvousAction::Offer,
                                days_ahead: 1, // Tomorrow at this time
                                utc_min,
                                dial_preset: h.chat.offer_dial_idx,
                                profile: 0,
                                subch: 2,
                                offer_ref: h.chat.next_seq & 7,
                            }));
                        }
                        app.screen = Screen::Rad950(Rad950Scr::CHAT_PREVIEW);
                        return;
                    }
                    3 => {
                        if let Some(h) = app.rad950_host_mut() {
                            let dest = RAD950_DIALS
                                .get(h.chat.offer_dial_idx as usize)
                                .copied()
                                .unwrap_or(RAD950_DIALS[1]);
                            let (band_id, off) = dial_to_move_band(dest).unwrap_or((0, 8200));
                            let slot = if h.last_slot == u32::MAX { 0 } else { h.last_slot };
                            h.chat.source_dial_hz10 = h.profile.dial();
                            h.chat.draft = Some(ChatDraft::Rendezvous(RendezvousCard::MoveFreq {
                                action: RendezvousAction::Offer,
                                band_id,
                                dial_offset_10hz: off,
                                profile: 0,
                                turn_mod64: ((slot + 4) & 0x3F) as u8,
                                subch: 2,
                            }));
                        }
                        app.screen = Screen::Rad950(Rad950Scr::CHAT_PREVIEW);
                        return;
                    }
                    _ => {
                        app.screen = Screen::Rad950(Rad950Scr { page: PAGE_DISTRESS, sel: 0, top: 0, field: 0, sub: 1 });
                        return;
                    }
                }
            }
            _ => {}
        }
        app.screen = Screen::Rad950(self);
    }

    fn key_chat_pick(mut self, app: &mut App, k: Key) {
        use ferrite_core::rad950::urad950::chat::{PHRASES, QUESTIONS};
        let list_len = if self.sub == 0 { PHRASES.len() } else { QUESTIONS.len() } as u8;
        match k {
            Key::Exit => {
                app.screen = Screen::Rad950(Rad950Scr::CHAT_REPLY);
                return;
            }
            Key::Up => self.sel = if self.sel == 0 { list_len - 1 } else { self.sel - 1 },
            Key::Down => self.sel = (self.sel + 1) % list_len,
            Key::Menu => {
                if let Some(h) = app.rad950_host_mut() {
                    h.chat.draft = Some(if self.sub == 0 {
                        ChatDraft::Phrase(self.sel)
                    } else {
                        ChatDraft::Question(self.sel)
                    });
                }
                app.screen = Screen::Rad950(Rad950Scr::CHAT_PREVIEW);
                return;
            }
            _ => {}
        }
        app.screen = Screen::Rad950(self);
    }

    fn key_chat_preview(self, app: &mut App, k: Key) {
        match k {
            Key::Exit => {
                app.screen = Screen::Rad950(Rad950Scr::CHAT_REPLY);
            }
            Key::Menu => {
                if let Some(h) = app.rad950_host_mut() {
                    h.chat.queue_draft();
                }
                app.screen = Screen::Rad950(Rad950Scr::CHAT);
            }
            _ => {}
        }
    }

    #[inline(never)]
    fn draw_chat(self, app: &mut App, lcd: &mut impl Panel, live: &LiveAll) {
        let Some(h) = app.rad950_host() else { return };
        let tx_active = matches!(live.mode, ModeLive::Rad950 { tx_active: true, .. }) || h.tx_in_flight;
        let phase = h.chat.phase;
        let peer = h.chat.peer;
        let secs = h.chat.secs_until_tx(h.utc_sec);
        let count = h.chat.entry_count as usize;
        let entries = h.chat.entries;
        let pending_offer = h.chat.pending_offer;
        let active_move = h.chat.active_move;
        let saved_rem = h.chat.saved_reminder;
        let chat_dial = h.active_dial();
        let cur_slot = if h.last_slot == u32::MAX { 0 } else { h.last_slot };

        let mut th = Mix::new()
            .n(phase as u32)
            .n(peer.raw())
            .n(secs as u32)
            .n(tx_active as u32)
            .n(count as u32)
            .n(self.top as u32)
            .n(chat_dial)
            .n(pending_offer.is_some() as u32)
            .n(active_move.map_or(0, |m| m.target_slot ^ ((m.retuned as u32) << 20) ^ ((m.expired as u32) << 21)))
            .n(saved_rem.map_or(0, |r| (r.utc_min as u32) ^ ((r.dial_preset as u32) << 12)));
        for e in &entries[..count] {
            let st_key = match e.status {
                ChatDelivery::None => 0u32,
                ChatDelivery::Queued => 1,
                ChatDelivery::Sending { sent, total } => 2 + ((sent as u32) << 8) + ((total as u32) << 16),
                ChatDelivery::AwaitingAck => 3,
                ChatDelivery::Delivered => 4,
                ChatDelivery::Incomplete => 5,
                ChatDelivery::Cancelled => 6,
            };
            th = th.n(e.from_me as u32).n(st_key).b(e.bytes());
        }
        if app.dirty(R_TOP, th.get()) {
            let y0 = TOP_Y;
            redraw_box!(true, lcd, (0, y0, W, 184, BG), {
                let mut cb = [0u8; 6];
                let cn = peer.decode(&mut cb);
                let ps = core::str::from_utf8(&cb[..cn]).unwrap_or("?");
                let mut hdr = Buf::<24>::new();
                if phase == ChatPhase::Active {
                    let _ = uwrite!(hdr, "{} ", ps);
                    push_khz3(&mut hdr, chat_dial);
                } else {
                    let _ = uwrite!(hdr, "CHAT {}", ps);
                }
                lcd.text4(8, y0 + 16, hdr.as_bytes(), &UI_14, TEXT, BG);

                // Make the wait visible on the right of the header; red TX flag when transmitting.
                if tx_active {
                    draw_tx_flag(lcd, W - 36, y0 + 3);
                } else {
                    let mut st = Buf::<16>::new();
                    let st_col = match phase {
                        ChatPhase::Inviting => {
                            let _ = uwrite!(st, "Inviting...");
                            AMBER
                        }
                        ChatPhase::IncomingInvite => {
                            let _ = uwrite!(st, "Invite!");
                            AMBER
                        }
                        ChatPhase::Active => {
                            let _ = uwrite!(st, "TX in {}s", secs);
                            GREEN
                        }
                        ChatPhase::Idle => {
                            let _ = uwrite!(st, "Ended");
                            DIM
                        }
                    };
                    lcd.text4(W - 76, y0 + 16, st.as_bytes(), &UI_12, st_col, BG);
                }
                lcd.fill(0, y0 + 22, W, 1, RULE);

                if phase == ChatPhase::IncomingInvite {
                    let mut inv = Buf::<32>::new();
                    let _ = uwrite!(inv, "{} invites you to chat", ps);
                    lcd.text4(8, y0 + 56, inv.as_bytes(), &UI_14, AMBER, BG);
                    lcd.text4(8, y0 + 82, b"OK Accept   0 Decline", &UI_12, TEXT, BG);
                } else {
                    // Optional Rendezvous banner (offer, move countdown, or saved reminder).
                    let mut list_y = y0 + 28;
                    let mut max_rows = 4usize;
                    if let Some(offer) = pending_offer {
                        let mut ob = [0u8; 64];
                        let on = crate::rad950_chat::format_rendezvous_card(&offer, &mut ob);
                        lcd.fill(0, y0 + 23, W, 34, LINE);
                        lcd.text4(8, y0 + 37, &ob[..on.min(30)], &UI_12, AMBER, LINE);
                        lcd.text4(8, y0 + 52, b"OK Accept  1 Counter  0 Decline", &UI_12, TEXT, LINE);
                        list_y = y0 + 62;
                        max_rows = 3;
                    } else if let Some(mv) = active_move {
                        lcd.fill(0, y0 + 23, W, 22, LINE);
                        let mut mb = Buf::<34>::new();
                        let mcol = if mv.expired {
                            let _ = uwrite!(mb, "Peer not heard - 0 Return");
                            AMBER
                        } else if mv.retuned {
                            let _ = uwrite!(mb, "Listening on ");
                            push_khz3(&mut mb, mv.dest_dial_hz10);
                            GREEN
                        } else {
                            let rem_s = mv.target_slot.saturating_sub(cur_slot) * 10;
                            let _ = uwrite!(mb, "Move ");
                            push_khz3(&mut mb, mv.dest_dial_hz10);
                            let _ = uwrite!(mb, " in {}s (3 Go)", rem_s);
                            AMBER
                        };
                        lcd.text4(8, y0 + 38, mb.as_bytes(), &UI_12, mcol, LINE);
                        list_y = y0 + 50;
                        max_rows = 3;
                    } else if let Some(rem) = saved_rem {
                        lcd.fill(0, y0 + 23, W, 22, LINE);
                        let mut rb = Buf::<34>::new();
                        let hh = rem.utc_min / 60;
                        let mm = rem.utc_min % 60;
                        let _ = uwrite!(rb, "Saved {}{}:{}{}Z (3 Go preset)", hh / 10, hh % 10, mm / 10, mm % 10);
                        lcd.text4(8, y0 + 38, rb.as_bytes(), &UI_12, GREEN, LINE);
                        list_y = y0 + 50;
                        max_rows = 3;
                    }
                    if count == 0 {
                        let msg: &[u8] = if phase == ChatPhase::Inviting {
                            b"Waiting for peer to accept..."
                        } else {
                            b"Connected. Press OK to reply."
                        };
                        lcd.text4(8, list_y + 26, msg, &UI_12, DIM, BG);
                    } else {
                        let start = (self.top as usize).min(count.saturating_sub(1));
                        for slot in 0..max_rows {
                            let idx = start + slot;
                            if idx >= count {
                                break;
                            }
                            let e = &entries[idx];
                            let ry = list_y + slot as i32 * 38;
                            let who: &[u8] = if e.from_me { b"You" } else { ps.as_bytes() };
                            let who_col = if e.from_me { BLUE } else { GREEN };
                            lcd.text4(8, ry + 12, who, &UI_12, who_col, BG);
                            let mut dtag = Buf::<16>::new();
                            let dcol = match e.status {
                                ChatDelivery::None => DIM,
                                ChatDelivery::Queued => {
                                    let _ = uwrite!(dtag, "Queued");
                                    AMBER
                                }
                                ChatDelivery::Sending { sent, total } => {
                                    let _ = uwrite!(dtag, "Send {}/{}", sent, total);
                                    RED
                                }
                                ChatDelivery::AwaitingAck => {
                                    let _ = uwrite!(dtag, "Wait ACK");
                                    AMBER
                                }
                                ChatDelivery::Delivered => {
                                    let _ = uwrite!(dtag, "Delivered");
                                    GREEN
                                }
                                ChatDelivery::Incomplete => {
                                    let _ = uwrite!(dtag, "Incomplete");
                                    AMBER
                                }
                                ChatDelivery::Cancelled => {
                                    let _ = uwrite!(dtag, "Cancelled");
                                    DIM
                                }
                            };
                            if !dtag.as_bytes().is_empty() {
                                lcd.text4(W - 72, ry + 12, dtag.as_bytes(), &UI_12, dcol, BG);
                            }
                            let txt = e.bytes();
                            let show = &txt[..txt.len().min(30)];
                            lcd.text4(8, ry + 28, show, &UI_12, TEXT, BG);
                        }
                    }
                }
            });
        }
        self.draw_panafall(app, lcd);
    }

    fn draw_chat_reply(self, app: &mut App, lcd: &mut impl Panel) {
        let has_out = app.rad950_host().is_some_and(|h| h.chat.has_outgoing());
        let labels: [&[u8]; 7] = if has_out {
            [b"Quick phrase", b"My station", b"Ask a question", b"Type message", b"More...", b"Cancel send", b"End chat"]
        } else {
            [b"Quick phrase", b"My station", b"Ask a question", b"Type message", b"More...", b"End chat", b""]
        };
        let n_items = if has_out { 7 } else { 6 };
        let y0 = TOP_Y + 4;
        let row_h = 28;
        for (i, &label) in labels.iter().enumerate().take(n_items) {
            let selected = i == self.sel as usize;
            let rh = Mix::new().n(selected as u32).b(label).get();
            if app.dirty(R_ROW + i, rh) {
                let y = y0 + i as i32 * row_h;
                let bg = if selected { BLUE } else { BG };
                let fg = if selected { BLUE_FG } else { TEXT };
                redraw_box!(true, lcd, (0, y, W, row_h, bg), {
                    lcd.text4(10, y + 19, label, &UI_14, fg, bg);
                });
            }
        }
    }

    fn draw_chat_more(self, app: &mut App, lcd: &mut impl Panel) {
        use ferrite_core::rad950::urad950::chat::wx_cond_name;
        let Some(h) = app.rad950_host() else { return };
        let wc = h.chat.weather_card;
        let ac = h.chat.about_card;
        let offer_dial = RAD950_DIALS
            .get(h.chat.offer_dial_idx as usize)
            .copied()
            .unwrap_or(RAD950_DIALS[1]);
        let y0 = TOP_Y + 6;
        let row_h = 34;
        for i in 0..5usize {
            let selected = i == self.sel as usize;
            let mut val = Buf::<16>::new();
            let label: &[u8] = match i {
                0 => {
                    let _ = uwrite!(val, "{} {}C", wx_cond_name(wc.cond), wc.temp_c);
                    b"Weather"
                }
                1 => {
                    if ac.year_licensed > 0 {
                        let _ = uwrite!(val, "Lic {}", ac.year_licensed);
                    } else {
                        let _ = uwrite!(val, "Set");
                    }
                    b"About me"
                }
                2 => {
                    push_khz3(&mut val, offer_dial);
                    b"Schedule QSO"
                }
                3 => {
                    push_khz3(&mut val, offer_dial);
                    b"Move freq"
                }
                _ => {
                    let _ = uwrite!(val, "3x PTT");
                    b"Send distress..."
                }
            };
            let rh = Mix::new().n(selected as u32).b(label).b(val.as_bytes()).get();
            if app.dirty(R_ROW + i, rh) {
                let y = y0 + i as i32 * row_h;
                let bg = if selected { BLUE } else { BG };
                let fg = if selected { BLUE_FG } else { TEXT };
                let dim = if selected { BLUE_FG } else { DIM };
                redraw_box!(true, lcd, (0, y, W, row_h, bg), {
                    lcd.text4(8, y + 22, label, &UI_14, fg, bg);
                    lcd.text4(W - 96, y + 22, val.as_bytes(), &UI_12, dim, bg);
                });
            }
        }
    }

    fn draw_chat_pick(self, app: &mut App, lcd: &mut impl Panel) {
        use ferrite_core::rad950::urad950::chat::{PHRASES, QUESTIONS};
        let items: &[&str] = if self.sub == 0 { PHRASES } else { QUESTIONS };
        let y0 = TOP_Y + 6;
        let row_h = 32;
        for (i, item) in items.iter().enumerate() {
            let selected = i == self.sel as usize;
            let rh = Mix::new().n(selected as u32).b(item.as_bytes()).get();
            if app.dirty(R_ROW + i, rh) {
                let y = y0 + i as i32 * row_h;
                let bg = if selected { BLUE } else { BG };
                let fg = if selected { BLUE_FG } else { TEXT };
                redraw_box!(true, lcd, (0, y, W, row_h, bg), {
                    let s = item.as_bytes();
                    lcd.text4(8, y + 20, &s[..s.len().min(29)], &UI_12, fg, bg);
                });
            }
        }
    }

    fn draw_chat_preview(self, app: &mut App, lcd: &mut impl Panel) {
        let Some(h) = app.rad950_host() else { return };
        let mut buf = [0u8; 64];
        let (len, bursts, airtime_s) = h.chat.preview_draft(&mut buf);
        let ph = Mix::new().b(&buf[..len]).n(bursts as u32).n(airtime_s as u32).get();
        if app.dirty(R_TOP, ph) {
            let y0 = TOP_Y + 8;
            redraw_box!(true, lcd, (0, y0, W, 220, BG), {
                lcd.text4(8, y0 + 16, b"Preview before sending:", &UI_12, DIM, BG);
                let txt = &buf[..len];
                // Word-wrap across 2 lines of up to 24 chars each.
                let (line1, line2) = if txt.len() <= 24 {
                    (txt, &[][..])
                } else if let Some(sp) = txt[..=24].iter().rposition(|&b| b == b' ') {
                    (&txt[..sp], &txt[sp + 1..txt.len().min(sp + 25)])
                } else {
                    (&txt[..24], &txt[24..txt.len().min(48)])
                };
                lcd.text4(8, y0 + 48, line1, &UI_14, TEXT, BG);
                if !line2.is_empty() {
                    lcd.text4(8, y0 + 72, line2, &UI_14, TEXT, BG);
                }
                let mut est = Buf::<32>::new();
                if airtime_s >= 60 {
                    let _ = uwrite!(est, "{} bursts · ~{}m {}s", bursts, airtime_s / 60, airtime_s % 60);
                } else {
                    let _ = uwrite!(est, "{} burst · ~{}s", bursts, airtime_s);
                }
                lcd.text4(8, y0 + 114, est.as_bytes(), &UI_14, AMBER, BG);
                lcd.text4(8, y0 + 144, b"Press OK to queue TX.", &UI_12, GREEN, BG);
            });
        }
    }

    fn key_distress(mut self, app: &mut App, k: Key) {
        use ferrite_core::rad950::urad950::distress::{DistressCategory, DistressLocSource};
        let ret_screen = if self.sub == 1 { Rad950Scr::CHAT } else { Rad950Scr::OPERATE };
        match k {
            Key::Exit => {
                if let Some(h) = app.rad950_host_mut() {
                    h.chat.distress.ptt_hold_start_ms = None;
                }
                app.screen = Screen::Rad950(ret_screen);
                return;
            }
            Key::Up | Key::Down => {
                self.sel ^= 1;
                self.field = 0;
                self.top = 0;
                if let Some(h) = app.rad950_host_mut() {
                    h.chat.distress.ptt_hold_start_ms = None;
                }
            }
            Key::Left | Key::Right => {
                let dir: i32 = if k == Key::Left { -1 } else { 1 };
                self.field = 0;
                self.top = 0;
                if let Some(h) = app.rad950_host_mut() {
                    h.chat.distress.ptt_hold_start_ms = None;
                    if self.sel == 0 {
                        let c = (h.chat.distress.category as i32 + dir).rem_euclid(DistressCategory::COUNT as i32) as u8;
                        h.chat.distress.category = DistressCategory::from_u8(c);
                    } else {
                        let l = (h.chat.distress.loc_src as i32 + dir).rem_euclid(4) as u8;
                        h.chat.distress.loc_src = DistressLocSource::from_u8(l);
                    }
                }
            }
            Key::Menu => {
                // Confirm settings and arm the 3x PTT + hold interlock (OK alone never transmits Distress!).
                if self.field == 0 {
                    self.field = 1;
                    self.top = 0;
                }
            }
            _ => {}
        }
        app.screen = Screen::Rad950(self);
    }

    fn draw_distress(self, app: &mut App, lcd: &mut impl Panel) {
        use ferrite_core::rad950::urad950::distress::DistressLocSource;
        let Some(h) = app.rad950_host() else { return };
        let mut id_buf = [0u8; 16];
        let id_len = h.profile.format_identity(&mut id_buf);
        let (eff_g, _) = h.profile.effective_grid(h.gps_grid);
        let d = h.chat.distress;
        let dh = Mix::new()
            .n(self.sel as u32)
            .n(self.field as u32)
            .n(self.top as u32)
            .n(d.category as u32)
            .n(d.loc_src as u32)
            .n(d.lat_e4 as u32)
            .n(d.lon_e4 as u32)
            .b(&id_buf[..id_len])
            .b(&eff_g)
            .get();
        if app.dirty(R_TOP, dh) {
            let y0 = TOP_Y + 4;
            redraw_box!(true, lcd, (0, y0, W, 232, BG), {
                lcd.fill(0, y0, W, 24, RED);
                lcd.text4(8, y0 + 17, b"SEND DISTRESS ALERT", &UI_14, WHITE, RED);

                let mut l1 = Buf::<32>::new();
                let id_s = core::str::from_utf8(&id_buf[..id_len]).unwrap_or("?");
                let g_s = core::str::from_utf8(&eff_g).unwrap_or("----");
                let _ = uwrite!(l1, "ID: {}  Grid: {}", id_s, g_s);
                lcd.text4(8, y0 + 44, l1.as_bytes(), &UI_14, TEXT, BG);

                // Row 0: Category selector
                let bg0 = if self.sel == 0 { BLUE } else { BG };
                let fg0 = if self.sel == 0 { BLUE_FG } else { TEXT };
                lcd.fill(0, y0 + 58, W, 26, bg0);
                lcd.text4(8, y0 + 76, b"Category:", &UI_12, fg0, bg0);
                lcd.text4(W - 104, y0 + 76, d.category.label().as_bytes(), &UI_12, fg0, bg0);

                // Row 1: Location source selector
                let bg1 = if self.sel == 1 { BLUE } else { BG };
                let fg1 = if self.sel == 1 { BLUE_FG } else { TEXT };
                lcd.fill(0, y0 + 86, W, 26, bg1);
                lcd.text4(8, y0 + 104, b"Location:", &UI_12, fg1, bg1);
                lcd.text4(W - 104, y0 + 104, d.loc_src.label().as_bytes(), &UI_12, fg1, bg1);

                // Coordinate preview
                let mut pos = Buf::<32>::new();
                if d.loc_src != DistressLocSource::Unavailable {
                    let lat_i = d.lat_e4 / 10_000;
                    let lat_f = (d.lat_e4.abs() % 10_000) / 100;
                    let lon_i = d.lon_e4 / 10_000;
                    let lon_f = (d.lon_e4.abs() % 10_000) / 100;
                    let _ = uwrite!(
                        pos,
                        "Pos: {}.{}{}, {}.{}{}",
                        lat_i,
                        lat_f / 10,
                        lat_f % 10,
                        lon_i,
                        lon_f / 10,
                        lon_f % 10
                    );
                } else {
                    let _ = uwrite!(pos, "Grid-only alert (1 burst)");
                }
                lcd.text4(8, y0 + 134, pos.as_bytes(), &UI_12, DIM, BG);

                if self.field == 1 {
                    lcd.fill(0, y0 + 150, W, 38, RED);
                    let mut p = Buf::<32>::new();
                    if self.top >= 3 {
                        let _ = uwrite!(p, "HOLD PTT TO SEND (3/3)...");
                    } else {
                        let _ = uwrite!(p, "Press PTT 3x, hold 3rd ({}/3)", self.top);
                    }
                    lcd.text4(8, y0 + 168, p.as_bytes(), &UI_12B, WHITE, RED);
                    lcd.text4(8, y0 + 183, b"BACK to cancel", &UI_12, WHITE, RED);
                } else {
                    lcd.text4(8, y0 + 170, "OK to confirm · BACK to cancel".as_bytes(), &UI_12, AMBER, BG);
                }
            });
        }
    }

    fn key_operate_menu(mut self, app: &mut App, k: Key) {
        const ITEMS: u8 = 6;
        match k {
            Key::Exit => {
                app.screen = Screen::Rad950(Rad950Scr::OPERATE);
                return;
            }
            Key::Up => self.sel = if self.sel == 0 { ITEMS - 1 } else { self.sel - 1 },
            Key::Down => self.sel = (self.sel + 1) % ITEMS,
            Key::Menu => {
                match self.sel {
                    0 => app.screen = Screen::Rad950(Rad950Scr { page: PAGE_RENDEZVOUS, sel: 0, top: 0, field: 0, sub: 0 }),
                    1 => app.screen = Screen::Rad950(Rad950Scr::ACTIVE),
                    2 => app.screen = Screen::Rad950(Rad950Scr::SETUP),
                    3 => {
                        if let Some(h) = app.rad950_host_mut() {
                            h.log_grid_filter = None;
                        }
                        app.screen = Screen::Rad950(Rad950Scr::LOG);
                    }
                    4 => app.screen = Screen::Rad950(Rad950Scr::GRIDS),
                    _ => {
                        app.screen = Screen::Rad950(Rad950Scr { page: PAGE_DISTRESS, sel: 0, top: 0, field: 0, sub: 0 });
                    }
                }
                return;
            }
            _ => {}
        }
        app.screen = Screen::Rad950(self);
    }

    fn draw_operate_menu(self, app: &mut App, lcd: &mut impl Panel) {
        const LABELS: [(&[u8], &[u8]); 6] = [
            (b"Rendezvous", b"5"),
            (b"Active QSO", b""),
            (b"Setup", b"*"),
            (b"Contact log", b"2"),
            (b"Worked grids", b"4"),
            (b"Send distress...", b"3x PTT"),
        ];
        let y0 = TOP_Y + 6;
        let row_h = 34;
        for (i, &(label, tag)) in LABELS.iter().enumerate() {
            let selected = i == self.sel as usize;
            let rh = Mix::new().n(selected as u32).b(label).b(tag).get();
            if app.dirty(R_ROW + i, rh) {
                let y = y0 + i as i32 * row_h;
                let bg = if selected { BLUE } else { BG };
                let fg = if selected { BLUE_FG } else if i == 5 { AMBER } else { TEXT };
                let dim = if selected { BLUE_FG } else { DIM };
                redraw_box!(true, lcd, (0, y, W, row_h, bg), {
                    lcd.text4(8, y + 22, label, &UI_14, fg, bg);
                    if !tag.is_empty() {
                        lcd.text4(W - 60, y + 22, tag, &UI_12, dim, bg);
                    }
                });
            }
        }
    }

    fn key_rendezvous(mut self, app: &mut App, k: Key) {
        match k {
            Key::Exit => {
                app.screen = Screen::Rad950(Rad950Scr::OPERATE);
                return;
            }
            Key::Up => self.sel = if self.sel == 0 { 2 } else { self.sel - 1 },
            Key::Down => self.sel = (self.sel + 1) % 3,
            Key::Digit(1) => {
                if let Some(h) = app.rad950_host_mut() {
                    h.rdv.toggle_band(0);
                    h.profile_dirty = true;
                }
            }
            Key::Digit(2) => {
                if let Some(h) = app.rad950_host_mut() {
                    h.rdv.toggle_band(1);
                    h.profile_dirty = true;
                }
            }
            Key::Digit(3) => {
                if let Some(h) = app.rad950_host_mut() {
                    h.rdv.toggle_band(2);
                    h.profile_dirty = true;
                }
            }
            Key::Digit(4) => {
                if let Some(h) = app.rad950_host_mut() {
                    h.rdv.toggle_band(3);
                    h.profile_dirty = true;
                }
            }
            Key::Left | Key::Right if self.sel == 2 => {
                let dir: i32 = if k == Key::Left { -1 } else { 1 };
                self.sub = (self.sub as i32 + dir).rem_euclid(4) as u8;
            }
            Key::Menu | Key::Left | Key::Right => {
                let mut retune = false;
                if let Some(h) = app.rad950_host_mut() {
                    match self.sel {
                        0 => {
                            h.rdv.follow_schedule = !h.rdv.follow_schedule;
                            h.profile_dirty = true;
                            // If already inside an active window, tune immediately when Follow turns ON.
                            if h.rdv.follow_schedule {
                                if let Some(win) = h.rdv.window_status(h.utc_sec) {
                                    if win.is_now {
                                        let di = crate::rad950_rdv::RDV_BANDS[win.band_idx as usize].dial_idx;
                                        if h.profile.dial_idx != di {
                                            h.profile.dial_idx = di;
                                            h.sync_engine_config();
                                            retune = true;
                                        }
                                    }
                                }
                            }
                        }
                        1 => {
                            h.rdv.call_listen_armed = !h.rdv.call_listen_armed;
                        }
                        _ => {
                            h.rdv.toggle_band(self.sub);
                            h.profile_dirty = true;
                        }
                    }
                }
                if retune {
                    app.rad950_retune();
                }
            }
            _ => {}
        }
        app.screen = Screen::Rad950(self);
    }

    #[inline(never)]
    fn draw_rendezvous(self, app: &mut App, lcd: &mut impl Panel, live: &LiveAll) {
        use crate::rad950_rdv::{RdvTurnPlan, RDV_BANDS};
        let Some(h) = app.rad950_host() else { return };
        let rdv = h.rdv;
        let utc_sec = h.utc_sec;
        let tx_now = matches!(live.mode, ModeLive::Rad950 { tx_active: true, .. }) || h.tx_in_flight;
        let win = rdv.window_status(utc_sec);
        let cur_slot = utc_sec / 10;
        let sec_in_slot = (utc_sec % 10) as u8;
        let sec_in_hour = (utc_sec % 3600) as u16;

        let rh = Mix::new()
            .n(utc_sec)
            .n(tx_now as u32)
            .n(rdv.band_mask as u32)
            .n(rdv.follow_schedule as u32)
            .n(rdv.call_listen_armed as u32)
            .n(rdv.seed)
            .n(self.sel as u32)
            .n(self.sub as u32)
            .get();
        if app.dirty(R_TOP, rh) {
            let y0 = TOP_Y;
            redraw_box!(true, lcd, (0, y0, W, 268, BG), {
                // 1. Header: NOW / NEXT band + dial + live countdown (or red TX flag)
                let mut hdr = Buf::<24>::new();
                let hdr_col = if let Some(w) = win {
                    let b = RDV_BANDS[w.band_idx as usize];
                    if w.is_now {
                        let _ = uwrite!(hdr, "NOW {} ", b.label);
                        push_khz3(&mut hdr, w.dial_hz10);
                        GREEN
                    } else {
                        let _ = uwrite!(hdr, "NEXT {} ", b.label);
                        push_khz3(&mut hdr, w.dial_hz10);
                        AMBER
                    }
                } else {
                    let _ = uwrite!(hdr, "Select a band");
                    DIM
                };
                lcd.text4(8, y0 + 16, hdr.as_bytes(), &UI_14, hdr_col, BG);
                if tx_now {
                    draw_tx_flag(lcd, W - 36, y0 + 3);
                } else if let Some(w) = win {
                    let mm = w.countdown_s / 60;
                    let ss = w.countdown_s % 60;
                    let mut cd = Buf::<16>::new();
                    if w.is_now {
                        let _ = uwrite!(cd, "{}{}:{}{} left", mm / 10, mm % 10, ss / 10, ss % 10);
                    } else {
                        let _ = uwrite!(cd, "in {}{}:{}{}", mm / 10, mm % 10, ss / 10, ss % 10);
                    }
                    lcd.text4(W - 76, y0 + 16, cd.as_bytes(), &UI_12, hdr_col, BG);
                }
                lcd.fill(0, y0 + 23, W, 1, RULE);

                // 2. Hourly 60-minute UTC band-hop timeline (y = y0 + 28 .. y0 + 82)
                let ty = y0 + 38;
                lcd.text4(8, ty, b"Hourly UTC band hops", &UI_12, DIM, BG);
                let hh = (utc_sec / 3600) % 24;
                let mm = (utc_sec / 60) % 60;
                let ss = utc_sec % 60;
                let mut clk = Buf::<12>::new();
                let _ = uwrite!(clk, "{}{}:{}{}:{}{}Z", hh / 10, hh % 10, mm / 10, mm % 10, ss / 10, ss % 10);
                lcd.text4(W - 66, ty, clk.as_bytes(), &UI_12, TEXT, BG);

                let rx = 12i32;
                let rw = 216i32; // 3.6 px per minute across 60 minutes
                let ry = ty + 8;
                let rht = 14i32;
                lcd.fill(rx, ry, rw, rht, LINE);
                for (i, b) in RDV_BANDS.iter().enumerate() {
                    let bx = rx + (b.start_min as i32 * rw) / 60;
                    let bw = (5 * rw) / 60; // 18 px
                    let enabled = rdv.band_enabled(i as u8);
                    let is_now = win.is_some_and(|w| w.is_now && w.band_idx == i as u8);
                    let is_next = win.is_some_and(|w| !w.is_now && w.band_idx == i as u8);
                    if is_now {
                        lcd.fill(bx, ry, bw, rht, GREEN);
                        lcd.fill(bx, ry, bw, 1, WHITE);
                        lcd.fill(bx, ry + rht - 1, bw, 1, WHITE);
                    } else if enabled {
                        lcd.fill(bx, ry, bw, rht, BLUE);
                        if is_next {
                            lcd.fill(bx, ry, bw, 2, AMBER);
                            lcd.fill(bx, ry + rht - 2, bw, 2, AMBER);
                        }
                    } else {
                        lcd.fill(bx, ry, bw, 1, RULE);
                        lcd.fill(bx, ry + rht - 1, bw, 1, RULE);
                        lcd.fill(bx, ry, 1, rht, RULE);
                        lcd.fill(bx + bw - 1, ry, 1, rht, RULE);
                    }
                    let lcol = if is_now { GREEN } else if is_next { AMBER } else if enabled { TEXT } else { DIM };
                    lcd.text4(bx, ry + rht + 13, b.label.as_bytes(), &UI_12, lcol, BG);
                }
                // Live UTC playhead needle across the 60-minute rail
                let px = rx + ((sec_in_hour as i32 * rw) / 3600).clamp(0, rw - 2);
                lcd.fill(px - 1, ry - 4, 4, 3, AMBER);
                lcd.fill(px, ry - 1, 2, rht + 2, WHITE);

                // 3. Upcoming 6 ten-second turns & random 50/50 CQ/RX + clear-subchannel strip
                let sy = ry + rht + 28;
                lcd.text4(8, sy, b"Next turns (50/50)", &UI_12, DIM, BG);
                let mut nt = Buf::<14>::new();
                let _ = uwrite!(nt, "Turn in {}s", 10 - sec_in_slot);
                lcd.text4(W - 76, sy, nt.as_bytes(), &UI_12, AMBER, BG);

                let cy = sy + 7;
                let cw = 34i32;
                let ch = 46i32;
                for k in 0..6usize {
                    let s = cur_slot + k as u32;
                    let cx = 12 + k as i32 * 36;
                    let plan = rdv.plan_slot(s, 0xFF);
                    let is_cq = matches!(plan, RdvTurnPlan::Cq { .. });
                    let card_bg = if k == 0 && tx_now {
                        RED
                    } else if is_cq {
                        0x4080 // dark warm amber/red tint
                    } else {
                        LINE
                    };
                    lcd.fill(cx, cy, cw, ch, card_bg);
                    if k == 0 {
                        // Bright frame on current turn
                        lcd.fill(cx, cy, cw, 1, WHITE);
                        lcd.fill(cx, cy + ch - 1, cw, 1, WHITE);
                        lcd.fill(cx, cy, 1, ch, WHITE);
                        lcd.fill(cx + cw - 1, cy, 1, ch, WHITE);
                        let prog = ((sec_in_slot as i32 + 1) * (cw - 2)) / 10;
                        lcd.fill(cx + 1, cy + ch - 3, prog, 2, if tx_now { WHITE } else { GREEN });
                    } else if is_cq {
                        lcd.fill(cx, cy, cw, 1, AMBER);
                    }
                    let sec_mark = ((s % 6) * 10) as u8;
                    let mut sl = Buf::<4>::new();
                    let _ = uwrite!(sl, ":{}{}", sec_mark / 10, sec_mark % 10);
                    lcd.text4(cx + 6, cy + 12, sl.as_bytes(), &UI_12, if k == 0 { WHITE } else { DIM }, card_bg);
                    match plan {
                        RdvTurnPlan::Cq { hz, .. } => {
                            let fg = if k == 0 && tx_now { WHITE } else { AMBER };
                            lcd.text4(cx + 8, cy + 27, b"CQ", &UI_12B, fg, card_bg);
                            let mut fb = Buf::<6>::new();
                            let _ = uwrite!(fb, "{}", hz);
                            lcd.text4(cx + 3, cy + 40, fb.as_bytes(), &UI_12, TEXT, card_bg);
                        }
                        RdvTurnPlan::Listen => {
                            let fg = if k == 0 { BLUE } else { DIM };
                            lcd.text4(cx + 8, cy + 27, b"RX", &UI_12B, fg, card_bg);
                            let tag: &[u8] = if (s & 1) == 1 { b"odd" } else { b"mon" };
                            lcd.text4(cx + 6, cy + 40, tag, &UI_12, DIM, card_bg);
                        }
                    }
                }

                // 4. Controls: Follow schedule, Call & listen, and 4 independent band checkboxes
                let ky = cy + ch + 10;
                for row in 0..3usize {
                    let y = ky + row as i32 * 24;
                    let selected = self.sel as usize == row;
                    let bg = if selected { BLUE } else { BG };
                    let fg = if selected { BLUE_FG } else { TEXT };
                    lcd.fill(0, y, W, 23, bg);
                    match row {
                        0 => {
                            lcd.text4(8, y + 16, b"Follow schedule", &UI_12, fg, bg);
                            let v = if rdv.follow_schedule { b"ON".as_slice() } else { b"OFF" };
                            let vc = if selected { BLUE_FG } else if rdv.follow_schedule { GREEN } else { DIM };
                            lcd.text4(W - 44, y + 16, v, &UI_12B, vc, bg);
                        }
                        1 => {
                            lcd.text4(8, y + 16, b"Call & listen", &UI_12, fg, bg);
                            let v = if rdv.call_listen_armed { b"ARMED".as_slice() } else { b"OFF" };
                            let vc = if selected { BLUE_FG } else if rdv.call_listen_armed { AMBER } else { DIM };
                            lcd.text4(W - 56, y + 16, v, &UI_12B, vc, bg);
                        }
                        _ => {
                            lcd.text4(8, y + 16, b"Bands:", &UI_12, fg, bg);
                            for (bi, b) in RDV_BANDS.iter().enumerate() {
                                let bx = 62 + bi as i32 * 43;
                                let on = rdv.band_enabled(bi as u8);
                                let focused = selected && self.sub as usize == bi;
                                let pbg = if focused { WHITE } else if on { GREEN } else { LINE };
                                let pfg = if focused || on { BG } else { DIM };
                                lcd.fill(bx, y + 3, 38, 17, pbg);
                                lcd.text4(bx + 7, y + 15, b.label.as_bytes(), &UI_12B, pfg, pbg);
                            }
                        }
                    }
                }
            });
        }
    }
}
