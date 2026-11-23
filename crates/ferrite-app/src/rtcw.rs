//! RTCW ("Radtel CW") runtime state inside [`App`]: wraps `urtcw`'s `CwVfo`, `IambicKeyer`,
//! `CwMeter`, 24-bin vertical panafall buffer, decoded RX text with caller auto-capture,
//! echoed TX text line, knob mode selector, and 9 canned CW macros (`1`–`9`).

use ferrite_core::freq::Freq10;
use ferrite_core::msg::RadioCmd;
use ferrite_core::urtcw::{
    CwMeter, CwVfo, IambicKeyer, KeyerConfig, KeyerMode, KeyerOutput, PaddleInput, PaddleSource,
};

use crate::App;

pub const RX_BUF_LEN: usize = 48;
pub const TX_BUF_LEN: usize = 24;
pub const RTCW_WF_BINS: usize = 24;
pub const RTCW_WF_ROWS: usize = 36;

/// SI4732 SSB hardware filter widths selectable in RTCW (`bw_idx = 0..=5`).
pub const CW_BW_HZ: [u16; 6] = [500, 1000, 1200, 2200, 3000, 4000];

/// What the top rotary encoder adjusts on the RTCW screen (cycled by Key `0`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum KnobMode {
    Freq = 0,
    Step = 1,
    Bw = 2,
    Wpm = 3,
}

impl KnobMode {
    pub const fn next(self) -> Self {
        match self {
            Self::Freq => Self::Step,
            Self::Step => Self::Bw,
            Self::Bw => Self::Wpm,
            Self::Wpm => Self::Freq,
        }
    }

    pub const fn label(self) -> &'static [u8] {
        match self {
            Self::Freq => b"FREQ",
            Self::Step => b"STEP",
            Self::Bw => b"BW",
            Self::Wpm => b"WPM",
        }
    }
}

pub struct RtcwHost {
    pub vfo: CwVfo,
    pub keyer: IambicKeyer,
    pub paddle_src: PaddleSource,
    /// CW RX/sidetone pitch in Hz (`700` default, matches `DecoderKind::Cw`).
    pub pitch_hz: u16,
    /// Signed BFO calibration offset in Hz (inherits RAD950 `rx_corr_hz` on entry).
    pub rx_corr_hz: i16,
    /// SI4732 SSB hardware filter index (`0..=5`: 500 Hz, 1.0k, 1.2k, 2.2k, 3.0k, 4.0k).
    pub bw_idx: u8,
    /// Active top-knob function (toggled by Key `0`).
    pub knob_mode: KnobMode,
    /// TX power index: `0` = Low (1 W), `1` = Mid (2.5 W), `2` = High (5 W).
    pub tx_power: u8,
    /// Live CW decoder meter & zero-beat tuning indicator.
    pub meter: CwMeter,
    /// Rolling decoded RX CW text (ASCII).
    pub rx_buf: [u8; RX_BUF_LEN],
    pub rx_len: u8,
    /// Most recent caller callsign auto-captured from decoded RX text (for Macro `5`).
    pub caller: [u8; 10],
    pub caller_len: u8,
    /// Rolling echoed TX CW text (ASCII).
    pub tx_buf: [u8; TX_BUF_LEN],
    pub tx_len: u8,
    /// Vertical panafall ring buffer (24 bins packed 4-bit = 12 bytes per row, 100..2,400 Hz).
    wf: [[u8; 12]; RTCW_WF_ROWS],
    wf_tx: [bool; RTCW_WF_ROWS],
    wf_head: u8,
    pub wf_seq: u32,
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
            bw_idx: 0, // 500 Hz default CW passband
            knob_mode: KnobMode::Freq,
            tx_power: 1,
            meter: CwMeter {
                tone_on: false,
                wpm_est: 18,
                snr_est: 0,
                tune_err: 0,
            },
            rx_buf: [b' '; RX_BUF_LEN],
            rx_len: 0,
            caller: [0; 10],
            caller_len: 0,
            tx_buf: [b' '; TX_BUF_LEN],
            tx_len: 0,
            wf: [[0; 12]; RTCW_WF_ROWS],
            wf_tx: [false; RTCW_WF_ROWS],
            wf_head: 0,
            wf_seq: 0,
            btn_dit: false,
            btn_dah: false,
            ext_dit: false,
            ext_dah: false,
            tx_hold: false,
            rf_key_down: false,
            sidetone_on: false,
        }
    }

    pub fn bw_hz(&self) -> u16 {
        CW_BW_HZ[(self.bw_idx as usize).min(CW_BW_HZ.len() - 1)]
    }

    pub fn bw_label(&self) -> &'static [u8] {
        match self.bw_idx {
            0 => b"500Hz",
            1 => b"1.0k",
            2 => b"1.2k",
            3 => b"2.2k",
            4 => b"3.0k",
            _ => b"4.0k",
        }
    }

    pub fn caller_bytes(&self) -> &[u8] {
        &self.caller[..self.caller_len as usize]
    }

    /// Push one 24-bin packed 4-bit spectrum slice onto the top of the vertical panafall.
    pub fn push_wf(&mut self, col: [u8; 12], is_tx: bool) {
        self.wf_head = (self.wf_head + 1) % RTCW_WF_ROWS as u8;
        let idx = self.wf_head as usize;
        self.wf[idx] = col;
        self.wf_tx[idx] = is_tx;
        self.wf_seq = self.wf_seq.wrapping_add(1);
    }

    /// Read waterfall 4-bit level (`0..=15`) for row `age` (`0` = newest) and `bin` (`0..24`).
    pub fn wf_level(&self, age: usize, bin: usize) -> u8 {
        if age >= RTCW_WF_ROWS || bin >= RTCW_WF_BINS {
            return 0;
        }
        let idx = (self.wf_head as usize + RTCW_WF_ROWS - age) % RTCW_WF_ROWS;
        let b = self.wf[idx][bin >> 1];
        if bin & 1 == 0 { b >> 4 } else { b & 0x0F }
    }

    pub fn wf_is_tx(&self, age: usize) -> bool {
        if age >= RTCW_WF_ROWS {
            return false;
        }
        let idx = (self.wf_head as usize + RTCW_WF_ROWS - age) % RTCW_WF_ROWS;
        self.wf_tx[idx]
    }

    /// Push a decoded RX CW character into the rolling display buffer and auto-capture caller callsigns.
    pub fn push_rx_char(&mut self, ch: u8) {
        if ch < 0x20 || ch > 0x7E {
            return;
        }
        // Collapse repeated spaces
        if ch == b' ' && self.rx_len > 0 && self.rx_buf[(self.rx_len - 1) as usize] == b' ' {
            return;
        }
        if ch == b' ' {
            self.capture_last_token_as_caller();
        }
        if (self.rx_len as usize) < RX_BUF_LEN {
            self.rx_buf[self.rx_len as usize] = ch;
            self.rx_len += 1;
        } else {
            self.rx_buf.copy_within(1..RX_BUF_LEN, 0);
            self.rx_buf[RX_BUF_LEN - 1] = ch;
        }
    }

    fn capture_last_token_as_caller(&mut self) {
        let len = self.rx_len as usize;
        if len == 0 {
            return;
        }
        let mut start = len;
        while start > 0 && self.rx_buf[start - 1] != b' ' {
            start -= 1;
        }
        let tok = &self.rx_buf[start..len];
        if tok.len() < 3 || tok.len() > 9 {
            return;
        }
        if tok == b"5NN" || tok == b"599" || tok == b"579" || tok == b"RST" {
            return;
        }
        let mut has_alpha = false;
        let mut has_digit = false;
        for &b in tok {
            if b.is_ascii_uppercase() {
                has_alpha = true;
            } else if b.is_ascii_digit() {
                has_digit = true;
            } else if b != b'/' {
                return;
            }
        }
        if has_alpha && has_digit {
            let n = tok.len().min(self.caller.len());
            self.caller[..n].copy_from_slice(&tok[..n]);
            self.caller_len = n as u8;
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

    /// Queue one of 9 CW macros (`digit = 1..=9`):
    /// `1=CQ`, `2=5NN`, `3=TU`, `4=<mycall>`, `5=<caller>`, `6=AGN?`, `7=?`, `8=QTH <grid>`, `9=73`.
    pub fn send_macro(&mut self, digit: u8, my_call: &[u8], grid: &[u8]) {
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
        let call = if my_call.is_empty() { b"DE" as &[u8] } else { my_call };
        let caller = if self.caller_len > 0 {
            &self.caller[..self.caller_len as usize]
        } else {
            b"" as &[u8]
        };
        match digit {
            1 => {
                push(b"CQ CQ DE ");
                push(call);
                push(b" ");
                push(call);
                push(b" K");
            }
            2 => {
                push(b"UR 5NN 5NN BK");
            }
            3 => {
                push(b"TU 73 DE ");
                push(call);
                push(b" *"); // <SK>
            }
            4 => {
                push(call);
            }
            5 => {
                if !caller.is_empty() {
                    push(caller);
                } else {
                    push(b"DE ");
                    push(call);
                }
            }
            6 => {
                push(b"AGN?");
            }
            7 => {
                push(b"?");
            }
            8 => {
                push(b"QTH");
                if !grid.is_empty() {
                    push(b" ");
                    push(grid);
                }
            }
            9 => {
                if !caller.is_empty() {
                    push(caller);
                    push(b" ");
                }
                push(b"73 DE ");
                push(call);
            }
            _ => return,
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
        let bw_on = 1 + self.rtcw.bw_idx.min(5);
        self.out.push(RadioCmd::RtcwRx { dial, rx_corr_hz, bw_on });
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
        self.out.push(RadioCmd::RtcwRx { dial: 0, rx_corr_hz: 0, bw_on: 0 });
    }

    /// Retune the SI4732 USB receiver for RTCW (after VFO/band/RIT/filter change).
    pub(crate) fn rtcw_retune(&mut self) {
        if self.radio.mode == crate::screens::activity::Mode::Rtcw {
            let dial = self.rtcw.rx_dial10();
            let rx_corr_hz = self.rtcw.rx_corr_hz;
            let bw_on = 1 + self.rtcw.bw_idx.min(5);
            self.out.push(RadioCmd::RtcwRx { dial, rx_corr_hz, bw_on });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knob_mode_cycles_all_four_targets() {
        let mut m = KnobMode::Freq;
        m = m.next();
        assert_eq!(m, KnobMode::Step);
        m = m.next();
        assert_eq!(m, KnobMode::Bw);
        m = m.next();
        assert_eq!(m, KnobMode::Wpm);
        m = m.next();
        assert_eq!(m, KnobMode::Freq);
    }

    #[test]
    fn caller_auto_capture_and_macros() {
        let mut h = RtcwHost::new();
        for &b in b"CQ DE W1AW 5NN " {
            h.push_rx_char(b);
        }
        // `5NN` is ignored; `W1AW` is retained as caller.
        assert_eq!(h.caller_bytes(), b"W1AW");

        // All 9 macros queue non-empty CW text
        for d in 1..=9u8 {
            h.keyer.abort_macro();
            h.send_macro(d, b"NQ6N", b"EN61");
            assert!(h.keyer.macro_active(), "macro {d} should activate keyer");
        }
    }

    #[test]
    fn panafall_ring_tracks_newest_at_age_zero() {
        let mut h = RtcwHost::new();
        let mut col = [0u8; 12];
        col[3] = 0xF0; // bin 6 (700 Hz) = 15
        h.push_wf(col, false);
        assert_eq!(h.wf_level(0, 6), 15);
        assert_eq!(h.wf_level(0, 7), 0);
        h.push_wf([0; 12], true);
        assert_eq!(h.wf_level(0, 6), 0);
        assert!(h.wf_is_tx(0));
        assert_eq!(h.wf_level(1, 6), 15);
        assert!(!h.wf_is_tx(1));
    }
}
