//! RTCW ("Radtel CW") UI screen (`MENU 0 3 8`):
//! - Live HF CW transceiver on the RT-950 Pro's calibrated HF bands (17m / 15m / 12m / 10m).
//! - Standard vertical panafall (`100..2,400 Hz`, 24 bins) with adjustable passband "slice"
//!   centered on the `700 Hz` CW pitch (`500 Hz` / `1.0k` / `1.2k` / `2.2k` / `3.0k` / `4.0k`).
//! - Key `0` toggles top-knob target: `FREQ` -> `STEP` -> `BW` -> `WPM`.
//! - Keys `1`–`9` send CW macros (`1:CQ 2:5NN 3:TU 4:<MYCALL> 5:<CALLER> 6:AGN? 7:? 8:QTH 9:73`).
//! - Built-in iambic keyer + audible `DAC1` sidetone + `PF1`/`PF2` pocket paddles + K1 jack.

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
const SLICE_BG: u16 = 0x0967; // translucent cyan-slate passband slice tint
const SLICE_EDGE: u16 = CYAN;
const TOP_Y: i32 = w::SBAR_H;
const R_HDR: usize = 0;
const R_FREQ: usize = 1;
const R_PANA: usize = 2;
const R_RX1: usize = 3;
const R_RX2: usize = 4;
const R_TX: usize = 5;
const R_MACROS: usize = 6;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct RtcwScr;

pub fn open(_app: &mut App) -> Screen {
    Screen::Rtcw(RtcwScr)
}

impl View for RtcwScr {
    fn name(self) -> &'static str {
        "RTCW"
    }

    fn title(self, _app: &App, b: &mut Buf<10>) {
        b.set(b"RTCW");
    }

    fn own_arrows(self) -> bool {
        true
    }

    fn key(self, app: &mut App, e: KeyEvent, _store: &impl ChannelStore) {
        if !matches!(e.press, Press::Short | Press::Repeat) {
            return;
        }
        let mut retune = false;
        match e.key {
            Key::Exit => {
                app.back();
                return;
            }
            // Swallow PF1/PF2 so pocket CW paddles don't trigger global side-key shortcuts
            Key::Side(_) => {}
            Key::Up => {
                app.rtcw.keyer.cfg.wpm = (app.rtcw.keyer.cfg.wpm + 1).min(45);
            }
            Key::Down => {
                app.rtcw.keyer.cfg.wpm = app.rtcw.keyer.cfg.wpm.saturating_sub(1).max(5);
            }
            Key::Left => {
                let n = CW_BANDS.len() as u8;
                let next = (app.rtcw.vfo.band_idx + n - 1) % n;
                app.rtcw.vfo.set_band(next);
                retune = true;
            }
            Key::Right => {
                let n = CW_BANDS.len() as u8;
                let next = (app.rtcw.vfo.band_idx + 1) % n;
                app.rtcw.vfo.set_band(next);
                retune = true;
            }
            Key::Menu => {
                app.rtcw.paddle_src = match app.rtcw.paddle_src {
                    PaddleSource::Auto => PaddleSource::SideButtons,
                    PaddleSource::SideButtons => PaddleSource::K1Jack,
                    PaddleSource::K1Jack => PaddleSource::Serial,
                    PaddleSource::Serial => PaddleSource::Auto,
                };
            }
            Key::Star => {
                if app.rtcw.keyer.macro_active() {
                    app.rtcw.keyer.abort_macro();
                } else {
                    app.rtcw.keyer.cfg.practice = !app.rtcw.keyer.cfg.practice;
                }
            }
            Key::Hash => {
                app.rtcw.keyer.cfg.mode = match app.rtcw.keyer.cfg.mode {
                    KeyerMode::IambicB => KeyerMode::IambicA,
                    KeyerMode::IambicA => KeyerMode::Ultimatic,
                    KeyerMode::Ultimatic => KeyerMode::Straight,
                    KeyerMode::Straight => KeyerMode::Bug,
                    KeyerMode::Bug => KeyerMode::IambicB,
                };
            }
            // Key 0 cycles what the top knob adjusts: Freq -> Step -> BW -> WPM
            Key::Digit(0) => {
                app.rtcw.knob_mode = app.rtcw.knob_mode.next();
            }
            // Keys 1..9 trigger CW macros / memories
            Key::Digit(d @ 1..=9) => {
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
                app.rtcw.send_macro(d, &call[..clen], &grid[..glen]);
            }
            _ => {}
        }
        if retune {
            app.rtcw_retune();
        }
        app.screen = Screen::Rtcw(self);
    }

    fn knob(self, app: &mut App, delta: i32, _store: &impl ChannelStore) {
        if delta == 0 {
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
            KnobMode::Bw => {
                let max = CW_BW_HZ.len() as i32 - 1;
                let cur = app.rtcw.bw_idx as i32;
                let next = (cur + delta).clamp(0, max) as u8;
                if next != app.rtcw.bw_idx {
                    app.rtcw.bw_idx = next;
                    retune = true;
                }
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

    fn render(self, app: &mut App, lcd: &mut impl Panel, cx: &Cx<impl ChannelStore>) {
        self.draw_operate(app, lcd, cx.live);
    }

    fn hints(self, app: &App) -> Hints {
        if app.rtcw.keyer.macro_active() {
            Hints::new(&[("0", "Knob"), ("<>", "Band"), ("*", "Abort")])
        } else {
            Hints::new(&[("0", "Knob"), ("<>", "Band"), ("*", "Prac")])
        }
    }
}

impl RtcwScr {
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
            mode,
            practice,
            bw_idx,
            bw_hz,
            bw_lbl,
            knob_mode,
            psrc,
            meter,
            tx_now,
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
                r.keyer.cfg.mode,
                r.keyer.cfg.practice,
                r.bw_idx,
                r.bw_hz(),
                r.bw_label(),
                r.knob_mode,
                r.paddle_src,
                r.meter,
                tx_active || r.tx_hold,
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

        // 1. Top status strip (`y = 24..46`): Band, Keyer Mode, Paddle Source, Live State Badge
        let mode_tag: &[u8] = match mode {
            KeyerMode::IambicB => b"IambB",
            KeyerMode::IambicA => b"IambA",
            KeyerMode::Ultimatic => b"Ultim",
            KeyerMode::Straight => b"Strgt",
            KeyerMode::Bug => b"Bug",
        };
        let pad_tag: &[u8] = match psrc {
            PaddleSource::Auto => b"AUTO",
            PaddleSource::SideButtons => b"PF1/2",
            PaddleSource::K1Jack => b"K1",
            PaddleSource::Serial => b"USB",
        };
        let hdr_h = Mix::new()
            .n(band_idx as u32)
            .n(mode as u32)
            .n(practice as u32)
            .n(psrc as u32)
            .n(tx_now as u32)
            .n(key_now as u32)
            .n(loading as u32)
            .get();
        if app.dirty(R_HDR, hdr_h) {
            redraw_box!(true, lcd, (0, TOP_Y, W, 22, CARD), {
                let mut left = Buf::<32>::new();
                let _ = uwrite!(
                    left,
                    "{} {} {} 700Hz",
                    core::str::from_utf8(band.name).unwrap_or("HF"),
                    core::str::from_utf8(mode_tag).unwrap_or("CW"),
                    core::str::from_utf8(pad_tag).unwrap_or("AUTO")
                );
                lcd.text4(6, TOP_Y + 15, left.as_bytes(), &UI_12, TEXT, CARD);
                let (badge, bfg, bbg): (&[u8], u16, u16) = if loading <= 100 {
                    (b"PATCH", BG, AMBER)
                } else if tx_now {
                    (if key_now { b"KEY" } else { b"TX " }, BG, RED)
                } else if practice {
                    (b"PRAC", BG, AMBER)
                } else {
                    (b"RX ", BG, GREEN)
                };
                lcd.fill(W - 44, TOP_Y + 2, 40, 18, bbg);
                lcd.text4(W - 38, TOP_Y + 15, badge, &UI_12, bfg, bbg);
            });
        }

        // 2. Frequency + 4-pill Knob Mode Selector (`y = 46..86`, 40 px)
        let freq_h = Mix::new()
            .n(freq10)
            .n(step_hz as u32)
            .n(bw_idx as u32)
            .n(wpm as u32)
            .n(knob_mode as u32)
            .n(rx_ready as u32)
            .n(rssi as u32)
            .get();
        if app.dirty(R_FREQ, freq_h) {
            let y = TOP_Y + 22;
            redraw_box!(true, lcd, (0, y, W, 40, BG), {
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
                let fcol = if knob_mode == KnobMode::Freq { CYAN } else { TEXT };
                lcd.text4(6, y + 17, fb.as_bytes(), &UI_14, fcol, BG);

                if rx_ready && rssi > -127 {
                    let mut rb = Buf::<12>::new();
                    let _ = uwrite!(rb, "{}dBm", rssi);
                    lcd.text4(W - 60, y + 16, rb.as_bytes(), &UI_12, DIM, BG);
                }

                // 4 interactive knob-mode pills (`0` toggles): FREQ | STEP | BW | WPM
                let py = y + 21;
                let mut draw_pill = |x: i32, pw: i32, text: &[u8], active: bool| {
                    let (fg, bg) = if active { (BG, CYAN) } else { (DIM, CARD) };
                    lcd.fill(x, py, pw, 17, bg);
                    lcd.text4(x + 4, py + 13, text, &UI_12, fg, bg);
                };
                draw_pill(4, 42, b"FREQ", knob_mode == KnobMode::Freq);

                let mut sb = Buf::<12>::new();
                let _ = uwrite!(sb, "S:{}", step_hz);
                draw_pill(49, 54, sb.as_bytes(), knob_mode == KnobMode::Step);

                let mut bb = Buf::<12>::new();
                let _ = uwrite!(bb, "{}", core::str::from_utf8(bw_lbl).unwrap_or("500"));
                draw_pill(106, 62, bb.as_bytes(), knob_mode == KnobMode::Bw);

                let mut wb = Buf::<12>::new();
                let _ = uwrite!(wb, "{}WPM", wpm);
                draw_pill(171, 65, wb.as_bytes(), knob_mode == KnobMode::Wpm);
            });
        }

        // 3. Standard vertical panafall with adjustable passband "slice" (`y = 88..162`, 74 px)
        self.draw_panafall(app, lcd, wf_seq, bw_hz, tx_now, meter.tone_on, meter.wpm_est);

        // 4. Rolling decoded RX CW text (2 lines of 24 chars, `y = 164..206`)
        let rx_slice = &rx_buf[..rx_len];
        let rx1 = if rx_len > 24 { &rx_slice[..rx_len - 24] } else { rx_slice };
        let rx2 = if rx_len > 24 { &rx_slice[rx_len - 24..] } else { b"" as &[u8] };
        let rx1_h = Mix::new().b(rx1).get();
        if app.dirty(R_RX1, rx1_h) {
            let y = TOP_Y + 140;
            redraw_box!(true, lcd, (0, y, W, 21, BG), {
                lcd.text4(6, y + 15, b"RX:", &UI_12, GREEN, BG);
                if !rx1.is_empty() {
                    lcd.text4(32, y + 15, rx1, &UI_12, TEXT, BG);
                } else {
                    lcd.text4(32, y + 15, b"(listening...)", &UI_12, DIM, BG);
                }
            });
        }
        let rx2_h = Mix::new().b(rx2).get();
        if app.dirty(R_RX2, rx2_h) {
            let y = TOP_Y + 161;
            redraw_box!(true, lcd, (0, y, W, 21, BG), {
                if !rx2.is_empty() {
                    lcd.text4(32, y + 15, rx2, &UI_12, TEXT, BG);
                }
            });
        }

        // 5. Echoed TX CW line (`y = 208..232`)
        let tx_slice = &tx_buf[..tx_len];
        let tx_h = Mix::new().b(tx_slice).n(macro_on as u32).get();
        if app.dirty(R_TX, tx_h) {
            let y = TOP_Y + 184;
            redraw_box!(true, lcd, (0, y, W, 22, CARD), {
                lcd.text4(6, y + 15, b"TX:", &UI_12, AMBER, CARD);
                if !tx_slice.is_empty() {
                    lcd.text4(32, y + 15, tx_slice, &UI_12, TEXT, CARD);
                } else {
                    lcd.text4(32, y + 15, b"PF1/PF2 or K1 paddle", &UI_12, DIM, CARD);
                }
            });
        }

        // 6. Compact 1-9 Macro Legend (`y = 234..270`, 2 lines)
        let caller_slice = &caller_buf[..caller_len];
        let mac_h = Mix::new().b(caller_slice).get();
        if app.dirty(R_MACROS, mac_h) {
            let y = TOP_Y + 210;
            redraw_box!(true, lcd, (0, y, W, 34, BG), {
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
                lcd.text4(6, y + 13, l1.as_bytes(), &UI_12, DIM, BG);
                lcd.text4(6, y + 28, b"6:AGN 7:? 8:QTH 9:73 0:Knob", &UI_12, DIM, BG);
            });
        }
    }

    /// Standard vertical panafall (`216 x 72 px` across `100..2,400 Hz`) with a shaded passband "slice"
    /// centered on the `700 Hz` CW pitch whose width tracks `bw_hz` (`500..4,000 Hz`).
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn draw_panafall(
        self,
        app: &mut App,
        lcd: &mut impl Panel,
        wf_seq: u32,
        bw_hz: u16,
        tx_now: bool,
        tone_on: bool,
        rx_wpm: u8,
    ) {
        const WF_PALETTE: [u16; 16] = [
            0x0000, 0x0821, 0x0864, 0x08C8, 0x114C, 0x11D1, 0x1A75, 0x1B39,
            0x2439, 0x2554, 0x366D, 0x5728, 0x9FA4, 0xDF02, 0xFD60, 0xFFE6,
        ];
        const TX_PALETTE: [u16; 16] = [
            0x1000, 0x2000, 0x3000, 0x4000, 0x5800, 0x7000, 0x8800, 0xA000,
            0xB800, 0xD000, 0xE820, 0xF880, 0xFA04, 0xFBA8, 0xFD8E, 0xFEF6,
        ];

        let pana_h = Mix::new()
            .n(wf_seq)
            .n(bw_hz as u32)
            .n(tx_now as u32)
            .n(tone_on as u32)
            .n(rx_wpm as u32)
            .get();
        if !app.dirty(R_PANA, pana_h) {
            return;
        }

        let y0 = TOP_Y + 64; // y = 88..162
        const WF_X: i32 = 12;
        const BIN_W: usize = 9; // 24 bins * 9 px = 216 px (x = 12..228)
        const WF_W: usize = RTCW_WF_BINS * BIN_W; // 216 px
        const SPEC_H: i32 = 24;
        const AXIS_H: i32 = 12;
        const WATER_H: usize = RTCW_WF_ROWS; // 36 px
        let spec_y = y0 + 1;
        let axis_y = spec_y + SPEC_H;
        let water_y = axis_y + AXIS_H;

        // Passband "slice" geometry centered on `700 Hz` (bin 6 -> local pixel x = 6*9 + 4 = 58).
        let center_local: i32 = (6 * BIN_W + BIN_W / 2) as i32;
        let slice_w: i32 = ((bw_hz as i32 * BIN_W as i32) / 100).clamp(18, WF_W as i32);
        let mut sl = center_local - slice_w / 2;
        let mut sr = sl + slice_w;
        if sl < 0 {
            sl = 0;
            sr = slice_w.min(WF_W as i32);
        }
        if sr > WF_W as i32 {
            sr = WF_W as i32;
            sl = (sr - slice_w).max(0);
        }

        let mut spec_lvls = [0u8; RTCW_WF_BINS];
        let newest_tx = app.rtcw.wf_is_tx(0);
        for (b, dst) in spec_lvls.iter_mut().enumerate() {
            *dst = app.rtcw.wf_level(0, b);
        }

        // 3a. Spectrum + passband slice overlay + frequency axis (banded zero-flicker box)
        redraw_box!(false, lcd, (0, y0, W, SPEC_H + AXIS_H + 1, BG), {
            lcd.fill(0, y0, W, 1, RULE);
            lcd.fill(WF_X, spec_y, WF_W as i32, SPEC_H, 0x0002);

            // Shaded passband "slice" window + crisp cyan left/right edges
            lcd.fill(WF_X + sl, spec_y, sr - sl, SPEC_H, SLICE_BG);
            lcd.fill(WF_X + sl, spec_y, 1, SPEC_H, SLICE_EDGE);
            lcd.fill(WF_X + sr - 1, spec_y, 1, SPEC_H, SLICE_EDGE);

            // Live 24-bin spectrum bars (brighter inside the passband slice, dimmed outside)
            for (b, &lvl) in spec_lvls.iter().enumerate() {
                let bar_h = ((lvl as i32 * (SPEC_H - 2)) / 15).max(1);
                let bx = WF_X + (b * BIN_W) as i32;
                let by = spec_y + SPEC_H - bar_h;
                let bin_mid = (b * BIN_W + BIN_W / 2) as i32;
                let in_slice = bin_mid >= sl && bin_mid < sr;
                let (fill_col, cap_col) = if newest_tx {
                    (0xA000, AMBER)
                } else if in_slice {
                    (0x1358, CYAN)
                } else {
                    (0x098B, 0x2B92)
                };
                lcd.fill(bx + 1, by, (BIN_W - 2) as i32, bar_h, fill_col);
                lcd.fill(bx + 1, by, (BIN_W - 2) as i32, 2.min(bar_h), cap_col);
            }

            // Crisp 700 Hz CW pitch center cursor (turns GREEN when CW tone is locked!)
            let pitch_col = if tx_now {
                RED
            } else if tone_on {
                GREEN
            } else {
                AMBER
            };
            lcd.fill(WF_X + center_local, spec_y, 1, SPEC_H, pitch_col);

            // Frequency axis labels + RX speed estimate
            lcd.fill(0, axis_y, W, AXIS_H, BG);
            lcd.text4(WF_X + center_local - 10, axis_y + 10, b"700", &UI_12, pitch_col, BG);
            lcd.text4(WF_X + 102, axis_y + 10, b"1.2k", &UI_12, DIM, BG);
            lcd.text4(WF_X + 150, axis_y + 10, b"1.8k", &UI_12, DIM, BG);
            let mut wb = Buf::<12>::new();
            let _ = uwrite!(wb, "{}W", rx_wpm);
            lcd.text4(W - 34, axis_y + 10, wb.as_bytes(), &UI_12, if tone_on { GREEN } else { DIM }, BG);
        });

        // 3b. Downward-scrolling 216 x 36 waterfall in a single SPI window, with passband slice guides
        lcd.window(WF_X, water_y, WF_X + WF_W as i32 - 1, water_y + WATER_H as i32 - 1);
        let mut line = [0u16; WF_W];
        let sl_u = sl as usize;
        let sr_u = (sr - 1).max(0) as usize;
        let cx_u = center_local as usize;
        for age in 0..WATER_H {
            let is_tx = app.rtcw.wf_is_tx(age);
            for bin in 0..RTCW_WF_BINS {
                let lvl = app.rtcw.wf_level(age, bin) as usize;
                let px = if is_tx { TX_PALETTE[lvl & 15] } else { WF_PALETTE[lvl & 15] };
                let base = bin * BIN_W;
                for k in 0..BIN_W {
                    line[base + k] = px;
                }
            }
            // Subtle vertical passband slice edges & 700 Hz pitch guide over dark waterfall pixels
            if line[sl_u] == 0x0000 || line[sl_u] == 0x0821 {
                line[sl_u] = 0x1355;
            }
            if sr_u < WF_W && (line[sr_u] == 0x0000 || line[sr_u] == 0x0821) {
                line[sr_u] = 0x1355;
            }
            if cx_u < WF_W && (age & 1 == 0) && (line[cx_u] == 0x0000 || line[cx_u] == 0x0821) {
                line[cx_u] = 0x4200; // subtle dotted amber pitch guide
            }
            lcd.push_buf(&line);
        }
    }
}
