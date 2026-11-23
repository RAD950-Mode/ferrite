//! RTCW ("Radtel CW") UI screen (`MENU 0 3 8`):
//! - Page 0 (Operate): High-contrast 26 px amber-yellow Knob Target Banner (`0` toggles FREQ/STEP/BW/WPM),
//!   full `0–3.0 kHz` panafall (24 × 125 Hz bins) with vivid cyan/gold passband bins vs muted slate stopband bins,
//!   decoded RX text with caller auto-capture, echoed TX bar, and 1–9 CW memories.
//! - Page 1 (`MENU` -> Settings): Sidetone Pitch, Sidetone Volume (0–11), CW AGC (`FAST/MED/SLOW`),
//!   Keyer Mode, Paddle Source, TX Power, and Memories `1–9`.
//! - Page 2 (Memories `1–9`): Cycle presets for each memory key `1–9`.

use super::shared::*;
use super::{Cx, Hints, View};
use crate::chan::ChannelStore;
use crate::keys::{KeyEvent, Press};
use crate::rtcw::{KnobMode, CW_BW_HZ, RTCW_WF_BINS, RTCW_WF_ROWS};
use crate::{App, Screen};
use ferrite_core::keys::Key;
use ferrite_core::msg::{LiveAll, ModeLive};
use ferrite_core::urtcw::{KeyerMode, PaddleSource, CW_BANDS, TUNE_STEPS_HZ};
use ferrite_ui::assets::*;
use ferrite_ui::gfx::*;
use ferrite_ui::panel::{Panel, W};
use ferrite_ui::text::Buf;
use ferrite_ui::widgets as w;
use ufmt::uwrite;

const CARD: u16 = 0x18E5; // dark slate card background (RGB565)
const CYAN: u16 = 0x07FF;
const BANNER_BG: u16 = 0xFE42; // vivid high-contrast amber-yellow (#FACC15)
const SLICE_BG: u16 = 0x0967; // subtle cyan-slate passband tint
const TOP_Y: i32 = w::SBAR_H;

const PAGE_OPERATE: u8 = 0;
const PAGE_SETTINGS: u8 = 1;
const PAGE_MEMORIES: u8 = 2;

const R_HDR: usize = 0;
const R_FREQ: usize = 1;
const R_KNOB: usize = 2;
const R_PANA: usize = 3;
const R_RX1: usize = 4;
const R_RX2: usize = 5;
const R_TX: usize = 6;
const R_MACROS: usize = 7;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct RtcwScr {
    page: u8,
    sel: u8,
}

pub fn open(_app: &mut App) -> Screen {
    Screen::Rtcw(RtcwScr { page: PAGE_OPERATE, sel: 0 })
}

impl View for RtcwScr {
    fn name(self) -> &'static str {
        "RTCW"
    }

    fn title(self, _app: &App, b: &mut Buf<10>) {
        match self.page {
            PAGE_SETTINGS => b.set(b"RTCW SET"),
            PAGE_MEMORIES => b.set(b"RTCW MEM"),
            _ => b.set(b"RTCW"),
        }
    }

    fn own_arrows(self) -> bool {
        true
    }

    fn key(self, app: &mut App, e: KeyEvent, _store: &impl ChannelStore) {
        if !matches!(e.press, Press::Short | Press::Repeat) {
            return;
        }
        match self.page {
            PAGE_OPERATE => self.key_operate(app, e.key),
            PAGE_SETTINGS => self.key_settings(app, e.key),
            _ => self.key_memories(app, e.key),
        }
    }

    fn knob(mut self, app: &mut App, delta: i32, _store: &impl ChannelStore) {
        if delta == 0 {
            return;
        }
        if self.page == PAGE_SETTINGS {
            self.sel = (self.sel as i32 + delta).rem_euclid(9) as u8;
            app.screen = Screen::Rtcw(self);
            return;
        }
        if self.page == PAGE_MEMORIES {
            self.sel = (self.sel as i32 + delta).rem_euclid(9) as u8;
            app.screen = Screen::Rtcw(self);
            return;
        }
        let mut retune = false;
        match app.rtcw.knob_mode {
            KnobMode::Freq => {
                app.rtcw.vfo.tune(delta);
                retune = true;
            }
            KnobMode::Step => {
                let max = TUNE_STEPS_HZ.len() as i32 - 1;
                let cur = app.rtcw.vfo.step_idx as i32;
                app.rtcw.vfo.step_idx = (cur + delta).clamp(0, max) as u8;
            }
            KnobMode::Wpm => {
                let cur = app.rtcw.keyer.cfg.wpm as i32;
                app.rtcw.keyer.cfg.wpm = (cur + delta).clamp(5, 45) as u8;
            }
        }
        if retune {
            app.rtcw_retune();
        }
        app.screen = Screen::Rtcw(self);
    }

    fn ptt(self, app: &mut App, down: bool) {
        // Never exit RTCW or key FM voice (which triggers the HF split refusal alert) when PTT is pressed.
        if down {
            app.show_toast(b"RTCW active", b"Use PF1/PF2 or K1", false);
        }
    }

    fn render(self, app: &mut App, lcd: &mut impl Panel, cx: &Cx<impl ChannelStore>) {
        match self.page {
            PAGE_OPERATE => self.draw_operate(app, lcd, cx.live),
            PAGE_SETTINGS => self.draw_settings(app, lcd),
            _ => self.draw_memories(app, lcd),
        }
    }

    fn hints(self, app: &App) -> Hints {
        match self.page {
            PAGE_SETTINGS => Hints::new(&[("^v", "Sel"), ("<>", "Chg"), ("EXIT", "Back")]),
            PAGE_MEMORIES => Hints::new(&[("^v", "Key"), ("<>", "Preset"), ("EXIT", "Back")]),
            _ => {
                if app.rtcw.keyer.macro_active() {
                    Hints::new(&[("0", "Knob"), ("MENU", "Set"), ("*", "Abort")])
                } else {
                    Hints::new(&[("0", "Knob"), ("MENU", "Set"), ("*", "Prac")])
                }
            }
        }
    }
}

impl RtcwScr {
    fn key_operate(mut self, app: &mut App, key: Key) {
        let mut retune = false;
        match key {
            Key::Exit => {
                app.back();
                return;
            }
            // Swallow PF1/PF2 so pocket CW paddles don't trigger global side-key shortcuts
            Key::Side(_) => {}
            Key::Menu => {
                self.page = PAGE_SETTINGS;
                self.sel = 0;
                app.redraw_all();
                app.screen = Screen::Rtcw(self);
                return;
            }
            // Up / Down: adjust passband width (500 Hz .. 4.0 kHz)
            Key::Up => {
                let max = (CW_BW_HZ.len() - 1) as u8;
                if app.rtcw.bw_idx < max {
                    app.rtcw.bw_idx += 1;
                    retune = true;
                }
            }
            Key::Down => {
                if app.rtcw.bw_idx > 0 {
                    app.rtcw.bw_idx -= 1;
                    retune = true;
                }
            }
            // Left / Right: shift passband (-600 .. +600 Hz, 50 Hz steps)
            Key::Left => {
                let next = (app.rtcw.shift_hz as i32 - 50).clamp(-600, 600) as i16;
                if next != app.rtcw.shift_hz {
                    app.rtcw.shift_hz = next;
                    retune = true;
                }
            }
            Key::Right => {
                let next = (app.rtcw.shift_hz as i32 + 50).clamp(-600, 600) as i16;
                if next != app.rtcw.shift_hz {
                    app.rtcw.shift_hz = next;
                    retune = true;
                }
            }
            Key::Star => {
                if app.rtcw.keyer.macro_active() {
                    app.rtcw.keyer.abort_macro();
                } else {
                    app.rtcw.keyer.cfg.practice = !app.rtcw.keyer.cfg.practice;
                }
            }
            Key::Hash => {
                app.rtcw.agc = app.rtcw.agc.next();
                retune = true;
            }
            // Key 0 cycles what the top knob adjusts: Freq -> Step -> BW -> WPM
            Key::Digit(0) => {
                app.rtcw.knob_mode = app.rtcw.knob_mode.next();
            }
            // Keys 1..9 trigger CW macros / memories
            Key::Digit(d @ 1..=9) => {
                self.trigger_macro(app, d);
            }
            _ => {}
        }
        if retune {
            app.rtcw_retune();
        }
        app.screen = Screen::Rtcw(self);
    }

    fn key_settings(mut self, app: &mut App, key: Key) {
        let mut retune = false;
        match key {
            Key::Exit => {
                self.page = PAGE_OPERATE;
                app.redraw_all();
                app.screen = Screen::Rtcw(self);
                return;
            }
            Key::Side(_) => {}
            Key::Up => self.sel = (self.sel + 8) % 9,
            Key::Down => self.sel = (self.sel + 1) % 9,
            Key::Left | Key::Right | Key::Menu => {
                let dir: i32 = if key == Key::Left { -1 } else { 1 };
                match self.sel {
                    0 => {
                        let n = CW_BANDS.len() as i32;
                        let next = (app.rtcw.vfo.band_idx as i32 + dir).rem_euclid(n) as u8;
                        app.rtcw.vfo.set_band(next);
                        retune = true;
                    }
                    1 => {
                        let p = (app.rtcw.pitch_hz as i32 + dir * 50).clamp(400, 900) as u16;
                        if p != app.rtcw.pitch_hz {
                            app.rtcw.pitch_hz = p;
                            retune = true;
                        }
                    }
                    2 => {
                        app.rtcw.sidetone_vol = (app.rtcw.sidetone_vol as i32 + dir).clamp(0, 11) as u8;
                    }
                    3 => {
                        app.rtcw.agc = if dir < 0 { app.rtcw.agc.prev() } else { app.rtcw.agc.next() };
                        retune = true;
                    }
                    4 => {
                        app.rtcw.nr = if dir < 0 { app.rtcw.nr.prev() } else { app.rtcw.nr.next() };
                        retune = true;
                    }
                    5 => {
                        app.rtcw.keyer.cfg.mode = match (app.rtcw.keyer.cfg.mode, dir > 0) {
                            (KeyerMode::IambicB, true) => KeyerMode::IambicA,
                            (KeyerMode::IambicA, true) => KeyerMode::Ultimatic,
                            (KeyerMode::Ultimatic, true) => KeyerMode::Straight,
                            (KeyerMode::Straight, true) => KeyerMode::Bug,
                            (KeyerMode::Bug, true) => KeyerMode::IambicB,
                            (KeyerMode::IambicB, false) => KeyerMode::Bug,
                            (KeyerMode::IambicA, false) => KeyerMode::IambicB,
                            (KeyerMode::Ultimatic, false) => KeyerMode::IambicA,
                            (KeyerMode::Straight, false) => KeyerMode::Ultimatic,
                            (KeyerMode::Bug, false) => KeyerMode::Straight,
                        };
                    }
                    6 => {
                        app.rtcw.paddle_src = match app.rtcw.paddle_src {
                            PaddleSource::Auto => PaddleSource::SideButtons,
                            PaddleSource::SideButtons => PaddleSource::K1Jack,
                            PaddleSource::K1Jack => PaddleSource::Serial,
                            PaddleSource::Serial => PaddleSource::Auto,
                        };
                    }
                    7 => {
                        app.rtcw.tx_power = (app.rtcw.tx_power as i32 + dir).clamp(0, 2) as u8;
                    }
                    _ => {
                        if key != Key::Left {
                            self.page = PAGE_MEMORIES;
                            self.sel = 0;
                            app.redraw_all();
                        }
                    }
                }
            }
            _ => {}
        }
        if retune {
            app.rtcw_retune();
        }
        app.screen = Screen::Rtcw(self);
    }

    fn key_memories(mut self, app: &mut App, key: Key) {
        match key {
            Key::Exit => {
                self.page = PAGE_SETTINGS;
                self.sel = 8;
                app.redraw_all();
                app.screen = Screen::Rtcw(self);
                return;
            }
            Key::Side(_) => {}
            Key::Up => self.sel = (self.sel + 8) % 9,
            Key::Down => self.sel = (self.sel + 1) % 9,
            Key::Left => {
                let s = self.sel as usize;
                app.rtcw.mem_preset[s] = (app.rtcw.mem_preset[s] + 2) % 3;
            }
            Key::Right => {
                let s = self.sel as usize;
                app.rtcw.mem_preset[s] = (app.rtcw.mem_preset[s] + 1) % 3;
            }
            Key::Menu => {
                self.trigger_macro(app, self.sel + 1);
            }
            _ => {}
        }
        app.screen = Screen::Rtcw(self);
    }

    fn trigger_macro(self, app: &mut App, digit: u8) {
        let mut call = [0u8; 10];
        let mut clen = 0usize;
        for &b in app.settings().callsign.as_bytes() {
            if b == 0 || b == b' ' {
                break;
            }
            if clen < call.len() {
                call[clen] = b;
                clen += 1;
            }
        }
        let mut grid = [0u8; 4];
        let mut glen = 0usize;
        if let Some(h) = app.rad950d.as_deref() {
            let (g4, _) = h.profile.effective_grid(h.gps_grid);
            if g4 != *b"----" {
                grid = g4;
                glen = 4;
            }
        }
        app.rtcw.send_macro(digit, &call[..clen], &grid[..glen]);
    }

    fn draw_operate(self, app: &mut App, lcd: &mut impl Panel, live: &LiveAll) {
        let (rx_ready, tx_active, rssi, loading) = match live.mode {
            ModeLive::Rtcw { rx_ready, tx_active, rssi, loading } => (rx_ready, tx_active, rssi, loading),
            _ => (false, false, -127, 255),
        };
        let (
            band_idx,
            band,
            freq10,
            step_hz,
            wpm,
            pitch_hz,
            mode,
            practice,
            bw_idx,
            bw_lbl,
            shift_hz,
            agc,
            nr,
            knob_mode,
            meter,
            key_now,
            rx_buf,
            rx_len,
            caller_buf,
            caller_len,
            tx_buf,
            tx_len,
            macro_on,
            wf_seq,
        ) = {
            let r = &app.rtcw;
            (
                r.vfo.band_idx,
                r.vfo.band(),
                r.vfo.tx_freq10,
                r.vfo.step_hz(),
                r.keyer.cfg.wpm,
                r.pitch_hz,
                r.keyer.cfg.mode,
                r.keyer.cfg.practice,
                r.bw_idx,
                r.bw_label(),
                r.shift_hz,
                r.agc,
                r.nr,
                r.knob_mode,
                r.meter,
                r.rf_key_down || r.sidetone_on,
                r.rx_buf,
                r.rx_len as usize,
                r.caller,
                r.caller_len as usize,
                r.tx_buf,
                r.tx_len as usize,
                r.keyer.macro_active(),
                r.wf_seq,
            )
        };

        // 1. Top status strip (`y = 24..44`): Band, AGC (FAST/MED/SLOW), NR, and true Radio TX/PRAC/RX badge
        let hdr_h = Mix::new()
            .n(band_idx as u32)
            .n(agc as u32)
            .n(nr as u32)
            .n(practice as u32)
            .n(tx_active as u32)
            .n(key_now as u32)
            .n(loading as u32)
            .get();
        if app.dirty(R_HDR, hdr_h) {
            redraw_box!(true, lcd, (0, TOP_Y, W, 20, CARD), {
                let mut left = Buf::<32>::new();
                let _ = uwrite!(
                    left,
                    "{} AGC:{}",
                    core::str::from_utf8(band.name).unwrap_or("HF"),
                    core::str::from_utf8(agc.label()).unwrap_or("FAST")
                );
                if nr != crate::rtcw::CwNr::Off {
                    let _ = uwrite!(left, " NR:{}", core::str::from_utf8(nr.label()).unwrap_or(""));
                }
                lcd.text4(6, TOP_Y + 14, left.as_bytes(), &UI_12, TEXT, CARD);
                let (badge, bfg, bbg): (&[u8], u16, u16) = if loading <= 100 {
                    (b"PATCH", BG, AMBER)
                } else if tx_active {
                    (if key_now { b"KEY" } else { b"TX " }, BG, RED)
                } else if practice {
                    (b"PRAC", BG, AMBER)
                } else {
                    (b"RX ", BG, GREEN)
                };
                lcd.fill(W - 44, TOP_Y + 2, 40, 16, bbg);
                lcd.text4(W - 38, TOP_Y + 14, badge, &UI_12, bfg, bbg);
            });
        }

        // 2. Frequency + RSSI (`y = 44..66`, 22 px)
        let freq_h = Mix::new().n(freq10).n(rx_ready as u32).n(rssi as u32).get();
        if app.dirty(R_FREQ, freq_h) {
            let y = TOP_Y + 20;
            redraw_box!(true, lcd, (0, y, W, 22, BG), {
                let mhz = freq10 / 100_000;
                let khz = (freq10 % 100_000) / 100;
                let hz10 = freq10 % 100;
                let mut fb = Buf::<24>::new();
                let _ = uwrite!(fb, "{}.", mhz);
                if khz < 100 {
                    let _ = uwrite!(fb, "0");
                }
                if khz < 10 {
                    let _ = uwrite!(fb, "0");
                }
                let _ = uwrite!(fb, "{}.", khz);
                if hz10 < 10 {
                    let _ = uwrite!(fb, "0");
                }
                let _ = uwrite!(fb, "{} MHz", hz10);
                lcd.text4(6, y + 17, fb.as_bytes(), &UI_14, TEXT, BG);

                if rx_ready && rssi > -127 {
                    let mut rb = Buf::<12>::new();
                    let _ = uwrite!(rb, "{}dBm", rssi);
                    lcd.text4(W - 58, y + 16, rb.as_bytes(), &UI_12, DIM, BG);
                }
            });
        }

        // 3. TALL HIGH-CONTRAST KNOB TARGET BANNER (`y = 66..106`: 26 px bright amber bar + 14 px sub-strip)
        let mode_tag: &[u8] = match mode {
            KeyerMode::IambicB => b"IambB",
            KeyerMode::IambicA => b"IambA",
            KeyerMode::Ultimatic => b"Ultim",
            KeyerMode::Straight => b"Strgt",
            KeyerMode::Bug => b"Bug",
        };
        let knob_h = Mix::new()
            .n(knob_mode as u32)
            .n(step_hz as u32)
            .n(bw_idx as u32)
            .n(shift_hz as u32)
            .n(wpm as u32)
            .n(pitch_hz as u32)
            .n(mode as u32)
            .get();
        if app.dirty(R_KNOB, knob_h) {
            let y = TOP_Y + 42;
            redraw_box!(true, lcd, (0, y, W, 40, BG), {
                // Full-width bright high-contrast amber-yellow banner (26 px tall)
                lcd.fill(4, y + 1, W - 8, 25, BANNER_BG);
                lcd.text4(10, y + 18, b"KNOB", &UI_12, BG, BANNER_BG);
                lcd.fill(46, y + 5, 2, 17, BG);

                let mut vb = Buf::<24>::new();
                match knob_mode {
                    KnobMode::Freq => {
                        let _ = uwrite!(vb, "TUNE ({}Hz)", step_hz);
                    }
                    KnobMode::Step => {
                        let _ = uwrite!(vb, "STEP {} Hz", step_hz);
                    }
                    KnobMode::Wpm => {
                        let _ = uwrite!(vb, "SPEED {} WPM", wpm);
                    }
                }
                lcd.text4(54, y + 19, vb.as_bytes(), &UI_14, BG, BANNER_BG);
                lcd.text4(W - 26, y + 17, b"[0]", &UI_12, BG, BANNER_BG);

                // Compact secondary summary line right under banner (shows arrow-key BW & Shift!)
                let mut sub = Buf::<40>::new();
                let _ = uwrite!(
                    sub,
                    "BW:{} SH:",
                    core::str::from_utf8(bw_lbl).unwrap_or("500")
                );
                if shift_hz >= 0 {
                    let _ = uwrite!(sub, "+{}", shift_hz);
                } else {
                    let _ = uwrite!(sub, "-{}", shift_hz.unsigned_abs());
                }
                let _ = uwrite!(
                    sub,
                    " {}WPM {}",
                    wpm,
                    core::str::from_utf8(mode_tag).unwrap_or("CW")
                );
                lcd.text4(6, y + 38, sub.as_bytes(), &UI_12, DIM, BG);
            });
        }

        // 4. FULL 0–3.0 kHz PANAFALL with PASSBAND vs STOPBAND COLORING (`y = 106..180`, 74 px)
        self.draw_panafall(app, lcd, wf_seq, bw_idx, shift_hz, pitch_hz, tx_active, meter.tone_on);

        // 5. Rolling decoded RX CW text (2 lines of 24 chars, `y = 182..220`)
        let rx_slice = &rx_buf[..rx_len];
        let rx1 = if rx_len > 24 { &rx_slice[..rx_len - 24] } else { rx_slice };
        let rx2 = if rx_len > 24 { &rx_slice[rx_len - 24..] } else { b"" as &[u8] };
        let rx1_h = Mix::new().b(rx1).get();
        if app.dirty(R_RX1, rx1_h) {
            let y = TOP_Y + 158;
            redraw_box!(true, lcd, (0, y, W, 19, BG), {
                lcd.text4(6, y + 14, b"RX:", &UI_12, GREEN, BG);
                if !rx1.is_empty() {
                    lcd.text4(30, y + 14, rx1, &UI_12, TEXT, BG);
                } else {
                    lcd.text4(30, y + 14, b"(listening...)", &UI_12, DIM, BG);
                }
            });
        }
        let rx2_h = Mix::new().b(rx2).get();
        if app.dirty(R_RX2, rx2_h) {
            let y = TOP_Y + 177;
            redraw_box!(true, lcd, (0, y, W, 19, BG), {
                if !rx2.is_empty() {
                    lcd.text4(30, y + 14, rx2, &UI_12, TEXT, BG);
                }
            });
        }

        // 6. Echoed TX CW bar (`y = 220..242`)
        let tx_slice = &tx_buf[..tx_len];
        let tx_h = Mix::new().b(tx_slice).n(macro_on as u32).get();
        if app.dirty(R_TX, tx_h) {
            let y = TOP_Y + 196;
            redraw_box!(true, lcd, (0, y, W, 22, CARD), {
                lcd.text4(6, y + 15, b"TX:", &UI_12, AMBER, CARD);
                if !tx_slice.is_empty() {
                    lcd.text4(30, y + 15, tx_slice, &UI_12, TEXT, CARD);
                } else {
                    lcd.text4(30, y + 15, b"PF1/PF2 or K1 paddle", &UI_12, DIM, CARD);
                }
            });
        }

        // 7. Compact 1-9 Macro Legend (`y = 244..274`)
        let caller_slice = &caller_buf[..caller_len];
        let mac_h = Mix::new().b(caller_slice).get();
        if app.dirty(R_MACROS, mac_h) {
            let y = TOP_Y + 220;
            redraw_box!(true, lcd, (0, y, W, 32, BG), {
                let mut l1 = Buf::<40>::new();
                let _ = uwrite!(
                    l1,
                    "1:CQ 2:5NN 3:TU 4:MY 5:{}",
                    if caller_slice.is_empty() {
                        "CALL"
                    } else {
                        core::str::from_utf8(caller_slice).unwrap_or("CALL")
                    }
                );
                lcd.text4(6, y + 12, l1.as_bytes(), &UI_12, DIM, BG);
                lcd.text4(6, y + 26, b"6:AGN 7:? 8:QTH 9:73 #:AGC", &UI_12, DIM, BG);
            });
        }
    }

    /// Full `0–3.0 kHz` vertical panafall (`216 × 72 px`, 24 × `125 Hz` bins):
    /// ALL bins remain visible across the full 3 kHz span; bins inside the passband render in vivid cyan/gold,
    /// while stopband bins render in muted slate-gray.
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn draw_panafall(
        self,
        app: &mut App,
        lcd: &mut impl Panel,
        wf_seq: u32,
        bw_idx: u8,
        shift_hz: i16,
        pitch_hz: u16,
        tx_now: bool,
        tone_on: bool,
    ) {
        // Vivid cyan -> green -> gold -> white palette for PASSED bins
        const PASS_PALETTE: [u16; 16] = [
            0x0002, 0x08E6, 0x098B, 0x0A6F, 0x1355, 0x1459, 0x1D5C, 0x269E,
            0x373A, 0x5F5A, 0x8775, 0xBF6C, 0xFF08, 0xFE42, 0xFC87, 0xFFDE,
        ];
        // Muted slate-gray palette for STOPBAND bins (signals stay visible, clearly outside passband)
        const STOP_PALETTE: [u16; 16] = [
            0x0842, 0x1083, 0x10A4, 0x18E5, 0x2126, 0x2967, 0x31A8, 0x39E9,
            0x422A, 0x4A6B, 0x52AC, 0x5AED, 0x632E, 0x6B6F, 0x73B0, 0x7BF1,
        ];
        const TX_PALETTE: [u16; 16] = [
            0x1000, 0x2000, 0x3000, 0x4000, 0x5800, 0x7000, 0x8800, 0xA000,
            0xB800, 0xD000, 0xE820, 0xF880, 0xFA04, 0xFBA8, 0xFD8E, 0xFEF6,
        ];

        let pana_h = Mix::new()
            .n(wf_seq)
            .n(bw_idx as u32)
            .n(shift_hz as u32)
            .n(pitch_hz as u32)
            .n(tx_now as u32)
            .n(tone_on as u32)
            .get();
        if !app.dirty(R_PANA, pana_h) {
            return;
        }

        let y0 = TOP_Y + 82; // y = 106..180
        const WF_X: i32 = 12;
        const BIN_W: usize = 9; // 24 bins * 9 px = 216 px (0..3,000 Hz)
        const WF_W: usize = RTCW_WF_BINS * BIN_W; // 216 px
        const SPEC_H: i32 = 24;
        const AXIS_H: i32 = 11;
        const WATER_H: usize = RTCW_WF_ROWS; // 36 px
        let spec_y = y0 + 1;
        let axis_y = spec_y + SPEC_H;
        let water_y = axis_y + AXIS_H;

        let mut in_pb = [false; RTCW_WF_BINS];
        let mut first_in: i32 = -1;
        let mut last_in: i32 = -1;
        let mut spec_lvls = [0u8; RTCW_WF_BINS];
        let newest_tx = app.rtcw.wf_is_tx(0);
        for b in 0..RTCW_WF_BINS {
            in_pb[b] = app.rtcw.bin_in_passband(b);
            if in_pb[b] {
                if first_in < 0 {
                    first_in = b as i32;
                }
                last_in = b as i32;
            }
            spec_lvls[b] = app.rtcw.wf_level(0, b);
        }
        let pitch_local = ((pitch_hz as i32 * WF_W as i32) / 3000).clamp(0, WF_W as i32 - 1);

        // 4a. Spectrum + passband tint + frequency axis (0 .. 700 .. 1.5k .. 3.0k)
        redraw_box!(false, lcd, (0, y0, W, SPEC_H + AXIS_H + 1, BG), {
            lcd.fill(0, y0, W, 1, RULE);
            lcd.fill(WF_X, spec_y, WF_W as i32, SPEC_H, 0x0002);

            if first_in >= 0 {
                let sl = first_in * BIN_W as i32;
                let sw = (last_in - first_in + 1) * BIN_W as i32;
                lcd.fill(WF_X + sl, spec_y, sw, SPEC_H, SLICE_BG);
                lcd.fill(WF_X + sl, spec_y, 1, SPEC_H, CYAN);
                lcd.fill(WF_X + sl + sw - 1, spec_y, 1, SPEC_H, CYAN);
            }

            // All 24 bins drawn: vivid cyan/gold inside passband, muted slate in stopband
            for b in 0..RTCW_WF_BINS {
                let lvl = spec_lvls[b] as usize;
                let bar_h = ((lvl as i32 * (SPEC_H - 2)) / 15).max(1);
                let bx = WF_X + (b * BIN_W) as i32;
                let by = spec_y + SPEC_H - bar_h;
                let col = if newest_tx {
                    TX_PALETTE[lvl & 15]
                } else if in_pb[b] {
                    PASS_PALETTE[lvl & 15]
                } else {
                    STOP_PALETTE[lvl & 15]
                };
                lcd.fill(bx + 1, by, (BIN_W - 2) as i32, bar_h, col);
            }

            // Pitch center cursor
            let pitch_col = if tx_now { RED } else if tone_on { GREEN } else { AMBER };
            lcd.fill(WF_X + pitch_local, spec_y, 1, SPEC_H, pitch_col);

            // Full 0..3.0 kHz frequency axis
            lcd.fill(0, axis_y, W, AXIS_H, BG);
            lcd.text4(WF_X, axis_y + 9, b"0", &UI_12, DIM, BG);
            let mut pb = Buf::<6>::new();
            let _ = uwrite!(pb, "{}", pitch_hz);
            lcd.text4((WF_X + pitch_local - 10).max(WF_X + 12), axis_y + 9, pb.as_bytes(), &UI_12, pitch_col, BG);
            lcd.text4(WF_X + 98, axis_y + 9, b"1.5k", &UI_12, DIM, BG);
            lcd.text4(WF_X + 188, axis_y + 9, b"3.0k", &UI_12, DIM, BG);
        });

        // 4b. Downward-scrolling 216 × 36 waterfall: PASSED bins vivid, STOPBAND bins muted slate
        lcd.window(WF_X, water_y, WF_X + WF_W as i32 - 1, water_y + WATER_H as i32 - 1);
        let mut line = [0u16; WF_W];
        let sl_u = if first_in >= 0 { (first_in as usize) * BIN_W } else { WF_W };
        let sr_u = if last_in >= 0 { ((last_in as usize) + 1) * BIN_W - 1 } else { WF_W };
        for age in 0..WATER_H {
            let is_tx = app.rtcw.wf_is_tx(age);
            for bin in 0..RTCW_WF_BINS {
                let lvl = app.rtcw.wf_level(age, bin) as usize;
                let px = if is_tx {
                    TX_PALETTE[lvl & 15]
                } else if in_pb[bin] {
                    PASS_PALETTE[lvl & 15]
                } else {
                    STOP_PALETTE[lvl & 15]
                };
                let base = bin * BIN_W;
                for k in 0..BIN_W {
                    line[base + k] = px;
                }
            }
            if sl_u < WF_W {
                line[sl_u] = 0x1D5C;
            }
            if sr_u < WF_W {
                line[sr_u] = 0x1D5C;
            }
            lcd.push_buf(&line);
        }
    }

    fn draw_settings(self, app: &mut App, lcd: &mut impl Panel) {
        let (band, pitch_hz, sidetone_vol, agc, nr, kmode, psrc, pwr) = {
            let r = &app.rtcw;
            (r.vfo.band(), r.pitch_hz, r.sidetone_vol, r.agc, r.nr, r.keyer.cfg.mode, r.paddle_src, r.tx_power)
        };
        let h = Mix::new()
            .n(self.sel as u32)
            .b(band.name)
            .n(pitch_hz as u32)
            .n(sidetone_vol as u32)
            .n(agc as u32)
            .n(nr as u32)
            .n(kmode as u32)
            .n(psrc as u32)
            .n(pwr as u32)
            .get();
        if !app.dirty(R_HDR, h) {
            return;
        }

        redraw_box!(true, lcd, (0, TOP_Y, W, 272, 0x0000), {
            lcd.text4(8, TOP_Y + 15, b"RTCW SETTINGS", &UI_14, CYAN, 0x0000);

            let mode_s = match kmode {
                KeyerMode::IambicB => "Iambic B",
                KeyerMode::IambicA => "Iambic A",
                KeyerMode::Ultimatic => "Ultimatic",
                KeyerMode::Straight => "Straight",
                KeyerMode::Bug => "Bug",
            };
            let pad_s = match psrc {
                PaddleSource::Auto => "AUTO",
                PaddleSource::SideButtons => "PF1/PF2",
                PaddleSource::K1Jack => "K1 Jack",
                PaddleSource::Serial => "USB",
            };
            let pwr_s = match pwr {
                0 => "LOW (1W)",
                1 => "MID (2.5W)",
                _ => "HIGH (5W)",
            };

            for i in 0..9usize {
                let y = TOP_Y + 21 + (i as i32) * 27;
                let active = i == self.sel as usize;
                let (bg, fg, vfg) = if active {
                    (BANNER_BG, 0x0000u16, 0x0000u16)
                } else {
                    (0x0000u16, 0xFFFFu16, BANNER_BG)
                };
                lcd.fill(4, y, W - 8, 25, bg);
                if !active {
                    lcd.fill(4, y + 25, W - 8, 1, 0x2945);
                }
                let mut vb = Buf::<16>::new();
                let label: &[u8] = match i {
                    0 => {
                        let _ = uwrite!(vb, "{}", core::str::from_utf8(band.name).unwrap_or("HF"));
                        b"HF Band"
                    }
                    1 => {
                        let _ = uwrite!(vb, "{} Hz", pitch_hz);
                        b"Sidetone Pitch"
                    }
                    2 => {
                        if sidetone_vol == 0 {
                            let _ = uwrite!(vb, "OFF");
                        } else {
                            let _ = uwrite!(vb, "{} / 11", sidetone_vol);
                        }
                        b"Sidetone Vol"
                    }
                    3 => {
                        let _ = uwrite!(vb, "{}", core::str::from_utf8(agc.label()).unwrap_or("FAST"));
                        b"CW AGC"
                    }
                    4 => {
                        let _ = uwrite!(vb, "{}", core::str::from_utf8(nr.label()).unwrap_or("OFF"));
                        b"Noise Reduc"
                    }
                    5 => {
                        let _ = uwrite!(vb, "{}", mode_s);
                        b"Keyer Mode"
                    }
                    6 => {
                        let _ = uwrite!(vb, "{}", pad_s);
                        b"Paddle Src"
                    }
                    7 => {
                        let _ = uwrite!(vb, "{}", pwr_s);
                        b"TX Power"
                    }
                    _ => {
                        let _ = uwrite!(vb, "1-9 >");
                        b"Memories"
                    }
                };
                lcd.text4(10, y + 17, label, &UI_12, fg, bg);
                lcd.text4(132, y + 17, vb.as_bytes(), &UI_12, vfg, bg);
            }
        });
    }

    fn draw_memories(self, app: &mut App, lcd: &mut impl Panel) {
        let mut mix = Mix::new().n(self.sel as u32);
        for &p in &app.rtcw.mem_preset {
            mix = mix.n(p as u32);
        }
        if !app.dirty(R_HDR, mix.get()) {
            return;
        }
        let presets = app.rtcw.mem_preset;
        redraw_box!(true, lcd, (0, TOP_Y, W, 272, 0x0000), {
            lcd.text4(8, TOP_Y + 15, b"MEMORIES (1-9) <>:Preset", &UI_12, CYAN, 0x0000);
            for i in 0..9usize {
                let y = TOP_Y + 22 + (i as i32) * 27;
                let active = i == self.sel as usize;
                let (bg, fg) = if active { (BANNER_BG, 0x0000u16) } else { (0x0000u16, 0xFFFFu16) };
                lcd.fill(4, y, W - 8, 25, bg);
                if !active {
                    lcd.fill(4, y + 25, W - 8, 1, 0x2945);
                }
                let mut line = Buf::<32>::new();
                let tmpl = crate::rtcw::MEM_PRESETS[i][(presets[i] as usize).min(2)];
                let _ = uwrite!(line, "{}: {}", i + 1, core::str::from_utf8(tmpl).unwrap_or(""));
                lcd.text4(8, y + 17, line.as_bytes(), &UI_12, fg, bg);
            }
        });
    }
}
