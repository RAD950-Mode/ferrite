//! RTCW ("Radtel CW") runtime state inside [`App`]: wraps `urtcw`'s `CwVfo`, `IambicKeyer`,
//! `CwMeter`, decoded RX text line, echoed TX text line, and canned CW macros.

use ferrite_core::freq::Freq10;
use ferrite_core::msg::RadioCmd;
use ferrite_core::urtcw::{
    CwMeter, CwVfo, IambicKeyer, KeyerConfig, KeyerMode, KeyerOutput, PaddleInput, PaddleSource,
};

use crate::App;

pub const RX_BUF_LEN: usize = 48;
pub const TX_BUF_LEN: usize = 24;

pub struct RtcwHost {
    pub vfo: CwVfo,
    pub keyer: IambicKeyer,
    pub paddle_src: PaddleSource,
    /// CW RX/sidetone pitch in Hz (`700` default, matches `DecoderKind::Cw`).
    pub pitch_hz: u16,
    /// Signed BFO calibration offset in Hz (inherits RAD950 `rx_corr_hz` on entry).
    pub rx_corr_hz: i16,
    /// Narrow `500 Hz` SI4732 SSB filter (`true`) vs `1.0 kHz` (`false`).
    pub bw_500hz: bool,
    /// TX power index: `0` = Low (1 W), `1` = Mid (2.5 W), `2` = High (5 W).
    pub tx_power: u8,
    /// Live CW decoder meter & zero-beat tuning indicator.
    pub meter: CwMeter,
    /// Rolling decoded RX CW text (ASCII).
    pub rx_buf: [u8; RX_BUF_LEN],
    pub rx_len: u8,
    /// Rolling echoed TX CW text (ASCII).
    pub tx_buf: [u8; TX_BUF_LEN],
    pub tx_len: u8,
    /// Live paddle state from radio's built-in `PF1` (`Side1`) / `PF2` (`Side2`) keys.
    pub btn_dit: bool,
    pub btn_dah: bool,
    /// Live paddle state from hardware K1 jack (`PE6` / `PC11`) or serial paddle.
    pub ext_dit: bool,
    pub ext_dah: bool,
    /// Current semi-break-in TX hold state sent to the Radio task.
    pub tx_hold: bool,
    /// Current RF carrier key-down state (for UI indicator).
    pub rf_key_down: bool,
    /// Current sidetone state (for UI indicator).
    pub sidetone_on: bool,
}

impl RtcwHost {
    pub const fn new() -> Self {
        Self {
            vfo: CwVfo::new(0), // 17m (18.086 MHz QRP CW)
            keyer: IambicKeyer::new(KeyerConfig {
                mode: KeyerMode::IambicB,
                wpm: 18,
                weight_pct: 50,
                reverse_paddles: false,
                break_in_hang_ms: 400,
                practice: false,
            }),
            paddle_src: PaddleSource::Auto,
            pitch_hz: 700,
            rx_corr_hz: 0,
            bw_500hz: true,
            tx_power: 1,
            meter: CwMeter {
                tone_on: false,
                wpm_est: 18,
                snr_est: 0,
                tune_err: 0,
            },
            rx_buf: [b' '; RX_BUF_LEN],
            rx_len: 0,
            tx_buf: [b' '; TX_BUF_LEN],
            tx_len: 0,
            btn_dit: false,
            btn_dah: false,
            ext_dit: false,
            ext_dah: false,
            tx_hold: false,
            rf_key_down: false,
            sidetone_on: false,
        }
    }

    /// Push a decoded RX CW character into the rolling display buffer.
    pub fn push_rx_char(&mut self, ch: u8) {
        if ch < 0x20 || ch > 0x7E {
            return;
        }
        // Collapse repeated spaces
        if ch == b' ' && self.rx_len > 0 && self.rx_buf[(self.rx_len - 1) as usize] == b' ' {
            return;
        }
        if (self.rx_len as usize) < RX_BUF_LEN {
            self.rx_buf[self.rx_len as usize] = ch;
            self.rx_len += 1;
        } else {
            self.rx_buf.copy_within(1..RX_BUF_LEN, 0);
            self.rx_buf[RX_BUF_LEN - 1] = ch;
        }
    }

    /// Push an echoed TX CW character into the rolling TX display buffer.
    pub fn push_tx_char(&mut self, ch: u8) {
        if ch < 0x20 || ch > 0x7E {
            return;
        }
        if (self.tx_len as usize) < TX_BUF_LEN {
            self.tx_buf[self.tx_len as usize] = ch;
            self.tx_len += 1;
        } else {
            self.tx_buf.copy_within(1..TX_BUF_LEN, 0);
            self.tx_buf[TX_BUF_LEN - 1] = ch;
        }
    }

    /// Combined active paddle input according to `paddle_src`.
    pub fn paddles(&self) -> PaddleInput {
        match self.paddle_src {
            PaddleSource::Auto => PaddleInput {
                dit: self.btn_dit || self.ext_dit,
                dah: self.btn_dah || self.ext_dah,
            },
            PaddleSource::SideButtons => PaddleInput {
                dit: self.btn_dit,
                dah: self.btn_dah,
            },
            PaddleSource::K1Jack | PaddleSource::Serial => PaddleInput {
                dit: self.ext_dit,
                dah: self.ext_dah,
            },
        }
    }

    /// Advance the iambic keyer by `dt_ms` (typically 1..5 ms).
    pub fn tick_keyer(&mut self, dt_ms: u16) -> KeyerOutput {
        let out = self.keyer.tick(dt_ms, self.paddles());
        if let Some(ch) = out.echoed_char {
            self.push_tx_char(ch);
        }
        self.rf_key_down = out.rf_key_down;
        self.sidetone_on = out.sidetone_on;
        out
    }

    /// SI4732 USB RX dial frequency (10 Hz units) for zero-beat reception at `pitch_hz`.
    pub fn rx_dial10(&self) -> Freq10 {
        self.vfo.rx_usb_dial10(self.pitch_hz)
    }

    /// Queue one of 4 canned CW macros (`0`=CQ, `1`=599, `2`=TU 73, `3`=Callsign).
    pub fn send_macro(&mut self, idx: u8, callsign: &[u8]) {
        let mut msg = [0u8; 48];
        let mut n = 0usize;
        let mut push = |s: &[u8]| {
            for &b in s {
                if n < msg.len() {
                    msg[n] = b;
                    n += 1;
                }
            }
        };
        let call = if callsign.is_empty() { b"DE" as &[u8] } else { callsign };
        match idx {
            0 => {
                push(b"CQ CQ DE ");
                push(call);
                push(b" ");
                push(call);
                push(b" K");
            }
            1 => {
                push(b"UR 599 599 BK");
            }
            2 => {
                push(b"TU 73 DE ");
                push(call);
                push(b" *"); // <SK>
            }
            _ => {
                push(b"DE ");
                push(call);
            }
        }
        self.keyer.send_text(&msg[..n]);
    }
}

impl App {
    /// True while the RTCW screen is open or underneath a child screen.
    pub fn rtcw_open(&self) -> bool {
        matches!(self.screen, crate::screens::Screen::Rtcw(_))
            || self.back.iter().flatten().any(|s| matches!(s, crate::screens::Screen::Rtcw(_)))
    }

    /// Called by `activity::start_cmds(Mode::Rtcw)`.
    pub(crate) fn rtcw_start(&mut self) {
        if let Some(h) = self.rad950d.as_deref() {
            // Inherit the user's calibrated SI4732 RX BFO correction from RAD950 Setup
            self.rtcw.rx_corr_hz = h.profile.rx_corr_hz;
        }
        self.rtcw.btn_dit = false;
        self.rtcw.btn_dah = false;
        self.rtcw.ext_dit = false;
        self.rtcw.ext_dah = false;
        self.rtcw.tx_hold = false;
        self.rtcw.keyer.abort_macro();
        let dial = self.rtcw.rx_dial10();
        let rx_corr_hz = self.rtcw.rx_corr_hz;
        let bw_500hz = self.rtcw.bw_500hz;
        self.out.push(RadioCmd::RtcwRx { dial, rx_corr_hz, bw_500hz, on: true });
    }

    /// Called by `activity::stop_mode(Mode::Rtcw)`.
    pub(crate) fn rtcw_stop(&mut self) {
        self.rtcw.keyer.abort_macro();
        self.rtcw.btn_dit = false;
        self.rtcw.btn_dah = false;
        self.rtcw.ext_dit = false;
        self.rtcw.ext_dah = false;
        if self.rtcw.tx_hold {
            self.rtcw.tx_hold = false;
            self.out.push(RadioCmd::RtcwTx { freq: 0, power: 0, on: false });
        }
        self.out.push(RadioCmd::RtcwRx { dial: 0, rx_corr_hz: 0, bw_500hz: true, on: false });
    }

    /// Retune the SI4732 USB receiver for RTCW (after VFO/band/RIT/filter change).
    pub(crate) fn rtcw_retune(&mut self) {
        if self.radio.mode == crate::screens::activity::Mode::Rtcw {
            let dial = self.rtcw.rx_dial10();
            let rx_corr_hz = self.rtcw.rx_corr_hz;
            let bw_500hz = self.rtcw.bw_500hz;
            self.out.push(RadioCmd::RtcwRx { dial, rx_corr_hz, bw_500hz, on: true });
        }
    }
}
