//! RTCW ("Radtel CW") UI screen (`MENU 0 3 8`):
//! - Live HF CW transceiver on the RT-950 Pro's calibrated HF bands (17m / 15m / 12m / 10m).
//! - Built-in iambic keyer (Mode B / A / Ultimatic / Straight / Bug), `PF1`/`PF2` pocket paddles,
//!   passive Kenwood K1 jack paddles (`PE6` + `PC11`), and 1-byte serial paddle/keyboard support.
//! - Live `24 kHz` Goertzel CW decoder + zero-beat tuning indicator + 4 canned CW macros (`1`–`4`).

use super::shared::*;
use super::{Cx, Hints, View};
use crate::chan::ChannelStore;
use crate::keys::{KeyEvent, Press};
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
const TOP_Y: i32 = w::SBAR_H;
const R_HDR: usize = 0;
const R_FREQ: usize = 1;
const R_TUNE: usize = 2;
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
                app.rtcw.keyer.cfg.practice = !app.rtcw.keyer.cfg.practice;
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
            Key::Digit(0) => {
                app.rtcw.bw_500hz = !app.rtcw.bw_500hz;
                retune = true;
            }
            Key::Digit(5) => {
                app.rtcw.vfo.step_idx = (app.rtcw.vfo.step_idx + 1) % TUNE_STEPS_HZ.len() as u8;
            }
            Key::Digit(d @ 1..=4) => {
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
                app.rtcw.send_macro(d - 1, &call[..clen]);
            }
            _ => {}
        }
        if retune {
            app.rtcw_retune();
        }
        app.screen = Screen::Rtcw(self);
    }

    fn knob(self, app: &mut App, delta: i32, _store: &impl ChannelStore) {
        if delta != 0 {
            app.rtcw.vfo.tune(delta);
            app.rtcw_retune();
        }
        app.screen = Screen::Rtcw(self);
    }

    fn render(self, app: &mut App, lcd: &mut impl Panel, cx: &Cx<impl ChannelStore>) {
        self.draw_operate(app, lcd, cx.live);
    }

    fn hints(self, _app: &App) -> Hints {
        Hints::new(&[("Knob", "Tune"), ("^v", "WPM"), ("<>", "Band")])
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
            rit_hz,
            step_hz,
            wpm,
            mode,
            practice,
            bw_500,
            psrc,
            meter,
            tx_now,
            key_now,
            rx_buf,
            rx_len,
            tx_buf,
            tx_len,
            macro_on,
        ) = {
            let r = &app.rtcw;
            (
                r.vfo.band_idx,
                r.vfo.band(),
                r.vfo.tx_freq10,
                r.vfo.rit_hz,
                r.vfo.step_hz(),
                r.keyer.cfg.wpm,
                r.keyer.cfg.mode,
                r.keyer.cfg.practice,
                r.bw_500hz,
                r.paddle_src,
                r.meter,
                tx_active || r.tx_hold,
                r.rf_key_down || r.sidetone_on,
                r.rx_buf,
                r.rx_len as usize,
                r.tx_buf,
                r.tx_len as usize,
                r.keyer.macro_active(),
            )
        };

        // 1. Top status strip: Band, Mode, WPM, Filter, Practice/TX badge
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
            .n(wpm as u32)
            .n(mode as u32)
            .n(practice as u32)
            .n(bw_500 as u32)
            .n(psrc as u32)
            .n(tx_now as u32)
            .n(key_now as u32)
            .n(loading as u32)
            .get();
        if app.dirty(R_HDR, hdr_h) {
            redraw_box!(true, lcd, (0, TOP_Y, W, 24, CARD), {
                let mut left = Buf::<36>::new();
                let _ = uwrite!(
                    left,
                    "{} {}WPM {} {}",
                    core::str::from_utf8(band.name).unwrap_or("HF"),
                    wpm,
                    core::str::from_utf8(mode_tag).unwrap_or("CW"),
                    if bw_500 { "500Hz" } else { "1kHz" }
                );
                lcd.text4(6, TOP_Y + 17, left.as_bytes(), &UI_12, TEXT, CARD);
                let (badge, bfg, bbg): (&[u8], u16, u16) = if loading <= 100 {
                    (b"PATCH", BG, AMBER)
                } else if tx_now {
                    (if key_now { b"KEY" } else { b"TX " }, BG, RED)
                } else if practice {
                    (b"PRAC", BG, AMBER)
                } else {
                    (pad_tag, BG, GREEN)
                };
                lcd.fill(W - 56, TOP_Y + 3, 52, 18, bbg);
                lcd.text4(W - 50, TOP_Y + 16, badge, &UI_12, bfg, bbg);
            });
        }

        // 2. Big CW frequency + step + RIT readout
        let freq_h = Mix::new()
            .n(freq10)
            .n(rit_hz as u32)
            .n(step_hz as u32)
            .n(rx_ready as u32)
            .n(rssi as u32)
            .get();
        if app.dirty(R_FREQ, freq_h) {
            let y = TOP_Y + 26;
            redraw_box!(true, lcd, (0, y, W, 44, BG), {
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
                lcd.text4(8, y + 20, fb.as_bytes(), &UI_14, TEXT, BG);

                let mut sb = Buf::<36>::new();
                let _ = uwrite!(sb, "Step {}Hz  RIT ", step_hz);
                if rit_hz >= 0 {
                    let _ = uwrite!(sb, "+{}Hz", rit_hz);
                } else {
                    let _ = uwrite!(sb, "-{}Hz", rit_hz.unsigned_abs());
                }
                if rx_ready && rssi > -127 {
                    let _ = uwrite!(sb, "  {}dBm", rssi);
                }
                lcd.text4(8, y + 38, sb.as_bytes(), &UI_12, DIM, BG);
            });
        }

        // 3. Zero-beat tuning bar + RX WPM & tone LED
        let tune_h = Mix::new()
            .n(meter.tone_on as u32)
            .n(meter.tune_err as u32)
            .n(meter.wpm_est as u32)
            .n(meter.snr_est as u32)
            .get();
        if app.dirty(R_TUNE, tune_h) {
            let y = TOP_Y + 72;
            redraw_box!(true, lcd, (0, y, W, 24, CARD), {
                lcd.text4(6, y + 17, b"TUNE", &UI_12, DIM, CARD);
                let cx = 110;
                for (i, err) in [-2i8, -1, 0, 1, 2].iter().enumerate() {
                    let bx = cx + (i as i32 - 2) * 18;
                    let active = meter.snr_est >= 3 && meter.tune_err == *err;
                    let col = if active {
                        if *err == 0 { GREEN } else { AMBER }
                    } else {
                        BG
                    };
                    lcd.fill(bx, y + 5, 14, 14, col);
                }
                let led_col = if meter.tone_on { GREEN } else { BG };
                lcd.fill(172, y + 6, 10, 12, led_col);
                let mut wb = Buf::<16>::new();
                let _ = uwrite!(wb, "RX {}WPM", meter.wpm_est);
                lcd.text4(188, y + 17, wb.as_bytes(), &UI_12, TEXT, CARD);
            });
        }

        // 4. Rolling decoded RX CW text (2 lines of 24 chars)
        let rx_slice = &rx_buf[..rx_len];
        let rx1 = if rx_len > 24 { &rx_slice[..rx_len - 24] } else { rx_slice };
        let rx2 = if rx_len > 24 { &rx_slice[rx_len - 24..] } else { b"" as &[u8] };
        let rx1_h = Mix::new().b(rx1).get();
        if app.dirty(R_RX1, rx1_h) {
            let y = TOP_Y + 100;
            redraw_box!(true, lcd, (0, y, W, 22, BG), {
                lcd.text4(6, y + 16, b"RX:", &UI_12, GREEN, BG);
                if !rx1.is_empty() {
                    lcd.text4(32, y + 16, rx1, &UI_12, TEXT, BG);
                } else {
                    lcd.text4(32, y + 16, b"(listening...)", &UI_12, DIM, BG);
                }
            });
        }
        let rx2_h = Mix::new().b(rx2).get();
        if app.dirty(R_RX2, rx2_h) {
            let y = TOP_Y + 122;
            redraw_box!(true, lcd, (0, y, W, 22, BG), {
                if !rx2.is_empty() {
                    lcd.text4(32, y + 16, rx2, &UI_12, TEXT, BG);
                }
            });
        }

        // 5. Echoed TX CW line
        let tx_slice = &tx_buf[..tx_len];
        let tx_h = Mix::new().b(tx_slice).n(macro_on as u32).get();
        if app.dirty(R_TX, tx_h) {
            let y = TOP_Y + 148;
            redraw_box!(true, lcd, (0, y, W, 24, CARD), {
                lcd.text4(6, y + 17, b"TX:", &UI_12, AMBER, CARD);
                if !tx_slice.is_empty() {
                    lcd.text4(32, y + 17, tx_slice, &UI_12, TEXT, CARD);
                } else {
                    lcd.text4(32, y + 17, b"PF1/PF2 or K1 paddle", &UI_12, DIM, CARD);
                }
            });
        }

        // 6. Macro strip
        if app.dirty(R_MACROS, 1) {
            let y = TOP_Y + 176;
            redraw_box!(true, lcd, (0, y, W, 20, BG), {
                lcd.text4(6, y + 14, b"1:CQ 2:599 3:73 4:DE 0:BW *:Prac", &UI_12, DIM, BG);
            });
        }
    }
}
