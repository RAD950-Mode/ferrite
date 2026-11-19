//! TERM-33 (decision 132, a bench test): the transmit baseband test signals. `txtest MODE [FILTERS] [LEVEL]` arms one for the
//! next PTT key-up of a `tx` build; the DSP task plays it out of DAC1 into the BK4829's mic input (PC12, as an APRS
//! burst) from the key-up to the release, looped, and `tools/sdrtx.py` measures it off the air (`--eye`, `--sweep`,
//! `--afsk`). Everything is made sample by sample at [`RATE_HZ`]: no buffer, only the named tables below in flash.
//!
//! - **4FSK** ([`Fsk4`]): M17's symbols, 4800 a second, levels +-3 and +-1 ([`level`]: M17's dibit map), root-
//!   raised-cosine shaped as M17 (alpha 0.5, [`RRC`]); first [`PREAMBLE_SYMBOLS`] of +3/-3 (40 ms, M17's
//!   preamble: a 2400 Hz square-ish tone the analysis finds), then PRBS9 ([`Prbs9`]) dibits for as long as PTT is
//!   held. A +3 symbol is [`FSK_AMP`], set on the bench for M17's 2400 Hz. The DAC-to-air chain inverts (measured
//!   Oct 13, 2026: a positive DAC swing goes out as negative deviation), so the generator sends each symbol negated
//!   and +3 goes out as +2400 Hz.
//! - **Sweep** ([`Sweep`]): a 1 kHz marker ([`MARKER_MS`]), then each third-octave tone of [`SWEEP_HZ`] (50 Hz to
//!   8 kHz) for [`TONE_MS`], [`GAP_MS`] of silence after each, all at [`SWEEP_AMP`]; then again.
//! - **AFSK** ([`Afsk`]): Bell 202 HDLC flags (0x7E) and nothing else: no callsign, no frame. NRZI makes each flag
//!   seven bits of one tone and one of the other.
//! - **AFSK from the BK4829** (`afsk-bk`, [`BkFlags`]): the same flags, but from the keyed BK4829's own tone
//!   generator instead of DAC1: REG_30 in stock's tone-transmit mode (0xC3FA, the mic off), tone 1 at
//!   [`BK_GAIN`] switched between 1200 and 2200 Hz (REG_71) at each NRZI change. The Radio task writes the tone;
//!   a hardware timer times the changes ([`BitClock`]: exact 1200 Bd, accumulated, never drifting). Not a DAC
//!   signal: the DSP task never plays it ([`Signal::dac`]). The method is BD1EEA's APRS for the UV-K5/K6
//!   (LerwinB/uv-k6v3-aprs, Apache-2.0: App/app/aprs.c `APRS_SendNrziBit`, App/driver/bk4829.c
//!   `BK4819_AFSK_BeginTx`/`SetTone`), adapted: the registers are checked against our driver and stock's tone path.
//!
//! - **2-level FSK** (`fsk BAUD [rc]`, [`Nrz`], TERM-33.17): PRBS9 bits as NRZ at any BAUD from [`BAUD_MIN`] to
//!   [`BAUD_MAX`], +-3 kHz ([`FSK2_DEV_HZ`]); hard edges, or with `rc` each edge a half cosine one bit long (a
//!   raised-cosine transition centred on the bit boundary: the bit centres still reach the full level).
//! - **GMSK** (`gmsk BAUD`, TERM-33.17): the same bits through a Gaussian of BT 0.5 ([`GAUSS`]), at h 0.5 (deviation
//!   BAUD / 4: 1.2 kHz at 4800).
//! - **G3RUH** (`g3ruh`, TERM-33.17): 9600 Bd packet's line signal: the PRBS9 bits through G3RUH's scrambler
//!   ([`Scrambler`], x^17 + x^12 + 1), raised-cosine edges as `fsk rc`, +-3 kHz.
//!
//! The three are negated as 4FSK is (the DAC-to-air chain inverts), so a 1 goes out as positive deviation. Their
//! deviations are nominal: [`dev_amp`] takes the sweep's 1 kHz tone (SWEEP_AMP: about 3.0 kHz in `data`, bench
//! Oct 27, 2026) as the scale, which `tools/bench-blade/txdata.py` checks on the air and the level trims.
//!
//! Each is at its nominal level times the arming's [`Arm::level`] percent ([`LEVEL_MIN`] to [`LEVEL_MAX`]). With
//! `eq` or `eq8` (TERM-33.16) the DSP task runs the samples through [`crate::txeq`]; with `bypass` (TERM-33.15) the
//! keyed chip's AF TX filters are bypassed (REG_47<0>) and its soft limiter off (REG_2F<7:5> = 000).

use crate::audio::{AFSK_AMP, DAC_HZ};
pub use crate::txeq::Eq;
/// The TX audio set-ups, shared with the APRS burst (every build): [`crate::txfilters::Filters`].
pub use crate::txfilters::Filters;
use udsp::trig::{ccos, cos_sin_phase, csin, phase_step};

/// The DAC's rate the signals are made at (48 kHz).
pub const RATE_HZ: u32 = DAC_HZ;
/// 4FSK: symbols a second (M17's) and samples per symbol.
pub const SYMBOL_HZ: u32 = 4800;
pub const SPS: usize = (RATE_HZ / SYMBOL_HZ) as usize;
/// The root-raised-cosine's roll-off (M17: 0.5) and its span in symbols (M17's 8: 81 taps at 10 samples a symbol).
pub const ALPHA: f64 = 0.5;
pub const SPAN: usize = 8;
pub const TAPS: usize = SPAN * SPS + 1;
/// The +3/-3 preamble at the start: 40 ms (M17's).
pub const PREAMBLE_SYMBOLS: u32 = SYMBOL_HZ / 25;
/// A +3 symbol's level, full scale 32767: M17's 2400 Hz in the `data` filters (bench, black radio, chip 0,
/// Oct 13, 2026: 6400 made about 3100 Hz, so 6400 x 0.77). A multiple of 3 (the unit is a third of it).
pub const FSK_AMP: i32 = 4920;

/// Sweep: the marker, each tone and the silence after each, ms; the tones' peak (the first build's half of the old
/// AFSK level, kept so sweeps compare across builds; low enough that the pre-emphasis of the `voice` filters doesn't
/// drive the limiter at the top tones).
pub const MARKER_MS: u32 = 500;
pub const MARKER_HZ: u16 = 1000;
pub const TONE_MS: u32 = 300;
pub const GAP_MS: u32 = 50;
pub const SWEEP_AMP: i32 = 4000;
/// The third-octave centre frequencies (IEC 61260's nominal ones) from 50 Hz to 8 kHz: 23 tones. A `static`: read
/// by the DSP task, so it stays in the fast flash (firmware/memory.x).
pub static SWEEP_HZ: [u16; 23] = [
    50, 63, 80, 100, 125, 160, 200, 250, 315, 400, 500, 630, 800, 1000, 1250, 1600, 2000, 2500, 3150, 4000, 5000, 6300, 8000,
];

/// What `txtest` sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Fsk4,
    Sweep,
    Afsk,
    /// AFSK flags from the BK4829's tone generator, not DAC1.
    AfskBk,
    /// TERM-34: every test above and more in one key-up, in a fixed order ([`crate::txsuite`]); the Radio task
    /// runs it, asking DAC1 for its DAC parts.
    Suite,
    /// TERM-33.17: 2-level NRZ PRBS9 at the arming's baud.
    Fsk,
    /// TERM-33.17: GMSK, BT 0.5, h 0.5, at the arming's baud.
    Gmsk,
    /// TERM-33.17: 9600 Bd G3RUH (scrambled PRBS9).
    G3ruh,
    /// SOL950 110-symbol `CQ NQ6N FN31` reference burst via BK4829 PLL stepping (`REG_38/39` + VCO toggle, TMR5).
    Sol950,
    /// SOL950 BK4829 PLL calibration staircase (`+500 Hz`/step every 500 ms, TMR5).
    Sol950Cal,
}

/// Default `REG_40` 12-bit deviation scale for `txtest sol950` / `sol950-cal`.
pub const SOL950_DEV_DEFAULT: u16 = 180;

/// Locked 110-symbol `CQ NQ6N FN31` (`tx_pwr = 10`) tone sequence (`docs/sol950-design-v2.md` §3).
/// Kept as a named `static` so the Radio task reads it from zero-wait fast flash (`firmware/memory.x`).
pub static SOL950_REF_TONES: [u8; 110] = [
    0, 3, 0, 1, 3, 2, 0, 1, 0, 3, 3, 0, 0, 0, 3, 3, 2, 1, 3, 0, 3, 2, 0, 1, 1, 0, 0, 0, 1, 2, 0, 1, 3, 0, 0, 3, 0, 1,
    0, 0, 3, 1, 3, 2, 1, 3, 0, 3, 3, 3, 2, 0, 2, 3, 0, 1, 0, 3, 2, 2, 1, 3, 2, 2, 2, 0, 3, 1, 2, 1, 0, 1, 0, 0, 3, 0,
    1, 2, 1, 2, 2, 2, 1, 0, 3, 3, 1, 0, 0, 1, 3, 0, 0, 3, 2, 2, 0, 3, 1, 0, 1, 1, 1, 0, 3, 1, 0, 2, 3, 0,
];

/// SOL950 `REG_70` tone-1 gains (`0..=127`) for tones `0..=3`, scaled by `level` (`100 %` -> `[16, 48, 80, 112]`).
pub fn sol950_gains(level: u8) -> [u8; 4] {
    [0u32, 1, 2, 3].map(|t| (((16 + 32 * t) * level as u32 + 50) / 100).min(127) as u8)
}

/// Non-blocking `1.25 ms` (`800 Hz`) substep schedule for `txtest sol950` and `txtest sol950-cal`.
#[derive(Clone, Copy, Debug)]
pub struct Sol950Bk {
    cal: bool,
    gains: [u8; 4],
}

impl Sol950Bk {
    /// Substep period in Hz (`64` substeps per `80 ms` symbol = `800 Hz`, i.e., `1,500` `TMR5` ticks at `1.2 MHz`).
    pub const STEP_HZ: u32 = 800;
    /// `0.20 s` (`160` substeps) lead-in on tone 0 after key-up before symbol 0 begins.
    pub const LEAD_STEPS: u32 = 160;
    /// `500 ms` (`400` substeps) per level in `sol950-cal`.
    pub const CAL_HOLD_STEPS: u32 = 400;

    pub fn new(cal: bool, level: u8) -> Self {
        Self { cal, gains: sol950_gains(level) }
    }

    /// Desired `REG_70` gain (`0..=127`) at `1.25 ms` substep `step` from start.
    pub fn gain_at(&self, step: u32) -> u8 {
        if self.cal {
            static CAL: [u8; 9] = [0, 16, 32, 48, 64, 80, 96, 112, 127];
            let i = ((step / Self::CAL_HOLD_STEPS) as usize).min(CAL.len() - 1);
            return CAL[i];
        }
        if step < Self::LEAD_STEPS {
            return self.gains[SOL950_REF_TONES[0] as usize];
        }
        let body = step - Self::LEAD_STEPS;
        let sym = (body / 64) as usize;
        if sym < SOL950_REF_TONES.len() {
            usol950::phy::reg70_gain_at(&SOL950_REF_TONES, sym, (body % 64) as usize, self.gains)
        } else {
            self.gains[SOL950_REF_TONES[SOL950_REF_TONES.len() - 1] as usize]
        }
    }
}

impl Signal {
    pub const ALL: [Signal; 10] = [
        Signal::Fsk4,
        Signal::Sweep,
        Signal::Afsk,
        Signal::AfskBk,
        Signal::Suite,
        Signal::Fsk,
        Signal::Gmsk,
        Signal::G3ruh,
        Signal::Sol950,
        Signal::Sol950Cal,
    ];

    /// The terminal's word for it.
    pub fn name(self) -> &'static str {
        match self {
            Signal::Fsk4 => "4fsk",
            Signal::Sweep => "sweep",
            Signal::Afsk => "afsk",
            Signal::AfskBk => "afsk-bk",
            Signal::Suite => "suite",
            Signal::Fsk => "fsk",
            Signal::Gmsk => "gmsk",
            Signal::G3ruh => "g3ruh",
            Signal::Sol950 => "sol950",
            Signal::Sol950Cal => "sol950-cal",
        }
    }

    /// Takes a BAUD (`fsk`, `gmsk`: [`BAUD_MIN`] to [`BAUD_MAX`]).
    pub fn takes_baud(self) -> bool {
        matches!(self, Signal::Fsk | Signal::Gmsk)
    }

    /// Whether this is one of the BK4829 frozen-tone SOL950 signals (`Arm.baud` carries `REG_40` `dev`).
    pub fn is_sol950(self) -> bool {
        matches!(self, Signal::Sol950 | Signal::Sol950Cal)
    }

    /// The baud (or `REG_40` `dev` for `sol950*`) an arming without one gets.
    pub fn default_baud(self) -> u16 {
        match self {
            Signal::Fsk => 1200,
            Signal::Gmsk => 4800,
            Signal::G3ruh => G3RUH_BAUD,
            Signal::Sol950 | Signal::Sol950Cal => SOL950_DEV_DEFAULT,
            _ => 0,
        }
    }

    /// Played from DAC1 by the DSP task as one signal from the key-up to the release (all but `afsk-bk` / `sol950*`,
    /// which use the BK4829's tone generator on the Radio task, and `suite`).
    pub fn dac(self) -> bool {
        !matches!(self, Signal::AfskBk | Signal::Suite | Signal::Sol950 | Signal::Sol950Cal)
    }

    /// Takes a FILTERS word and a LEVEL (the DAC signals and `afsk-bk` / `sol950`'s LEVEL; `suite` and `sol950-cal` do not).
    pub fn takes_level(self) -> bool {
        !matches!(self, Signal::Suite | Signal::Sol950Cal)
    }

    /// A word as typed, any case.
    pub fn parse(w: &[u8]) -> Option<Signal> {
        Signal::ALL.into_iter().find(|s| s.name().as_bytes().eq_ignore_ascii_case(w))
    }

    /// The filters when none are named: `data` for the DAC signals; `afsk-bk` and `sol950*` leave the key-up's
    /// voice settings (`sol950*` saves and restores its own BK4829 TX registers).
    pub fn default_filters(self) -> Filters {
        if matches!(self, Signal::AfskBk | Signal::Sol950 | Signal::Sol950Cal) {
            Filters::Voice
        } else {
            Filters::Data
        }
    }
}

/// The TX audio set-up a key-up changes (and the unkey puts back), or None for the chip's voice settings: an APRS
/// burst (a data key-up, `aprs`) always `data` (APRS-14, decision 135: the level then follows the DAC linearly,
/// where the voice set-up's MIC AGC holds it near 4 kHz whatever the DAC); a `txtest` key-up its arming's filters
/// (`voice` none; `afsk-bk` none, not a DAC signal; `suite` none: it sets its own per step, TERM-34); a voice key-up
/// none.
pub fn key_up_filters(aprs: bool, test: Option<Arm>) -> Option<Filters> {
    let f = if aprs { Some(Filters::Data) } else { test.filter(|t| t.signal.dac()).map(|t| t.filters) };
    f.filter(|&f| f != Filters::Voice)
}

/// The level, percent of the signal's nominal: the range and the default.
pub const LEVEL_MIN: u8 = 10;
pub const LEVEL_MAX: u8 = 150;
pub const LEVEL_DEFAULT: u8 = 100;

/// A test armed for the next PTT key-up: the signal, the chip's TX audio set-up for it, and its level in percent
/// of the nominal ([`LEVEL_MIN`] to [`LEVEL_MAX`]); for `fsk` and `gmsk` the baud (and `fsk`'s raised-cosine
/// edges); the equaliser the samples go through (TERM-33.16) and the AF TX filter bypass (TERM-33.15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arm {
    pub signal: Signal,
    pub filters: Filters,
    pub level: u8,
    pub baud: u16,
    pub shaped: bool,
    pub eq: Eq,
    pub bypass: bool,
}

impl Arm {
    /// `signal` with its defaults: its filters, level 100, its baud, hard edges (`g3ruh`: raised-cosine), no
    /// equaliser, no bypass.
    pub fn new(signal: Signal) -> Arm {
        Arm { signal, filters: signal.default_filters(), level: LEVEL_DEFAULT, baud: signal.default_baud(), shaped: signal == Signal::G3ruh, eq: Eq::Off, bypass: false }
    }
}

/// `amp` at `level` percent.
const fn scaled(amp: i32, level: u8) -> i32 {
    amp * level as i32 / 100
}

/// The terminal's request: show what is armed, or arm (`Some`) or disarm (`None`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask {
    Show,
    Set(Option<Arm>),
}

/// The root-raised-cosine at `t` symbols from its centre (unit symbol time; the continuous pulse's area is 1).
const fn rrc(k: i32) -> f64 {
    let pi = core::f64::consts::PI;
    let t = k as f64 / SPS as f64;
    if k == 0 {
        return 1.0 - ALPHA + 4.0 * ALPHA / pi;
    }
    // |t| = 1 / (4 alpha): the formula's 0/0, at k = +-SPS / (4 alpha) = +-5.
    if (4.0 * ALPHA * t) * (4.0 * ALPHA * t) == 1.0 {
        let a = pi / (4.0 * ALPHA);
        return ALPHA / core::f64::consts::SQRT_2 * ((1.0 + 2.0 / pi) * csin(a) + (1.0 - 2.0 / pi) * ccos(a));
    }
    let num = csin(pi * t * (1.0 - ALPHA)) + 4.0 * ALPHA * t * ccos(pi * t * (1.0 + ALPHA));
    let den = pi * t * (1.0 - (4.0 * ALPHA * t) * (4.0 * ALPHA * t));
    num / den
}

const fn rrc_taps() -> [i16; TAPS] {
    // Scaled so the taps sum to SPS: a run of one symbol then comes out at that symbol's level, and with a matched
    // RRC of unit sum at the receiver every symbol centre is the level exactly (a raised cosine is Nyquist).
    let mut sum = 0.0;
    let mut i = 0;
    while i < TAPS {
        sum += rrc(i as i32 - (TAPS / 2) as i32);
        i += 1;
    }
    let mut t = [0i16; TAPS];
    let mut i = 0;
    while i < TAPS {
        let v = rrc(i as i32 - (TAPS / 2) as i32) * SPS as f64 / sum * (1 << RRC_SHIFT) as f64;
        t[i] = (if v < 0.0 { v - 0.5 } else { v + 0.5 }) as i16;
        i += 1;
    }
    t
}

/// [`RRC`]'s scale: Q14 (the centre tap is about 1.14).
pub const RRC_SHIFT: u32 = 14;
/// The transmit filter's taps, Q14, summing to [`SPS`]. A `static`: read per sample by the DSP task (fast flash).
pub static RRC: [i16; TAPS] = rrc_taps();

/// PRBS9 (x^9 + x^5 + 1, ITU-T O.150): 511 bits, then again.
#[derive(Clone, Copy, Debug)]
pub struct Prbs9(u16);

impl Default for Prbs9 {
    fn default() -> Self {
        Prbs9(0x1FF)
    }
}

impl Prbs9 {
    pub fn bit(&mut self) -> u8 {
        let b = ((self.0 >> 8) ^ (self.0 >> 4)) & 1;
        self.0 = ((self.0 << 1) | b) & 0x1FF;
        b as u8
    }
}

/// M17's dibit to symbol map: 01 +3, 00 +1, 10 -1, 11 -3 (`hi` the first bit).
pub fn level(hi: u8, lo: u8) -> i8 {
    match (hi & 1, lo & 1) {
        (0, 1) => 3,
        (0, 0) => 1,
        (1, 0) => -1,
        _ => -3,
    }
}

/// The 4FSK test: [`PREAMBLE_SYMBOLS`] of +3/-3, then PRBS9 dibits, RRC-shaped, a sample at a time.
#[derive(Clone, Copy, Debug)]
pub struct Fsk4 {
    /// The last SPAN + 1 symbols, newest at `at`.
    syms: [i8; SPAN + 1],
    at: u8,
    /// The sample within the symbol (0: a new symbol enters).
    phase: u8,
    /// Symbols made so far (saturating: only the preamble's count matters).
    n: u32,
    prbs: Prbs9,
    /// A unit symbol's level (a third of [`FSK_AMP`], at the arming's level).
    unit: i32,
}

impl Default for Fsk4 {
    fn default() -> Self {
        Self::new(LEVEL_DEFAULT)
    }
}

impl Fsk4 {
    /// At `level` percent of [`FSK_AMP`].
    pub fn new(level: u8) -> Self {
        Fsk4 { syms: [0; SPAN + 1], at: 0, phase: 0, n: 0, prbs: Prbs9::default(), unit: scaled(FSK_AMP / 3, level) }
    }

    /// The next symbol: the preamble's, then the PRBS's.
    pub fn symbol(&mut self) -> i8 {
        let s = if self.n < PREAMBLE_SYMBOLS {
            if self.n.is_multiple_of(2) { 3 } else { -3 }
        } else {
            let hi = self.prbs.bit();
            level(hi, self.prbs.bit())
        };
        self.n = self.n.saturating_add(1);
        s
    }

    pub fn sample(&mut self) -> i16 {
        if self.phase == 0 {
            self.at = if self.at as usize == SPAN { 0 } else { self.at + 1 };
            self.syms[self.at as usize] = self.symbol();
        }
        // Polyphase: the symbol k back meets tap phase + k SPS.
        let mut acc = 0i32;
        let mut j = self.at as usize;
        let mut tap = self.phase as usize;
        while tap < TAPS {
            acc += self.syms[j] as i32 * RRC[tap] as i32;
            j = if j == 0 { SPAN } else { j - 1 };
            tap += SPS;
        }
        self.phase = if self.phase as usize + 1 == SPS { 0 } else { self.phase + 1 };
        // Negated: the DAC-to-air chain inverts, so +3 goes out as positive deviation.
        (-((acc * self.unit) >> RRC_SHIFT)).clamp(-32767, 32767) as i16
    }
}

/// Samples in `ms`.
const fn samples(ms: u32) -> u32 {
    RATE_HZ / 1000 * ms
}

/// One pass of the sweep, samples.
pub const SWEEP_SAMPLES: u32 = samples(MARKER_MS + GAP_MS) + SWEEP_HZ.len() as u32 * samples(TONE_MS + GAP_MS);

/// The sweep's tone at sample `pos` of a pass (None: a silence).
pub fn sweep_tone(pos: u32) -> Option<u16> {
    Sweep::default().tone_at(pos % SWEEP_SAMPLES)
}

/// The sweep, a sample at a time: each tone from phase 0. `txtest sweep`'s ([`Sweep::new`]: the marker, then the
/// tones, looped) or the suite's ([`Sweep::once`]: the tones alone at their own length, once, then silence).
#[derive(Clone, Copy, Debug)]
pub struct Sweep {
    pos: u32,
    phase: u32,
    amp: i32,
    /// Samples: the marker (0: none), each tone, the silence after each (and after the marker).
    marker: u32,
    tone: u32,
    gap: u32,
    /// One pass, then silence; else looped.
    once: bool,
}

impl Default for Sweep {
    fn default() -> Self {
        Self::new(LEVEL_DEFAULT)
    }
}

impl Sweep {
    /// At `level` percent of [`SWEEP_AMP`].
    pub fn new(level: u8) -> Self {
        Sweep { pos: 0, phase: 0, amp: scaled(SWEEP_AMP, level), marker: samples(MARKER_MS), tone: samples(TONE_MS), gap: samples(GAP_MS), once: false }
    }

    /// The tones of [`SWEEP_HZ`] alone, `tone_ms` each with `gap_ms` of silence after each, once, at [`SWEEP_AMP`].
    pub const fn once(tone_ms: u32, gap_ms: u32) -> Self {
        Sweep { pos: 0, phase: 0, amp: SWEEP_AMP, marker: 0, tone: samples(tone_ms), gap: samples(gap_ms), once: true }
    }

    /// One pass, samples.
    pub const fn pass(&self) -> u32 {
        (if self.marker > 0 { self.marker + self.gap } else { 0 }) + SWEEP_HZ.len() as u32 * (self.tone + self.gap)
    }

    /// The tone at sample `pos` of a pass (None: a silence; past the pass, silence).
    pub fn tone_at(&self, pos: u32) -> Option<u16> {
        let first = if self.marker > 0 { self.marker + self.gap } else { 0 };
        if pos < first {
            return (pos < self.marker).then_some(MARKER_HZ);
        }
        let r = pos - first;
        let i = (r / (self.tone + self.gap)) as usize;
        if i >= SWEEP_HZ.len() {
            return None;
        }
        (r % (self.tone + self.gap) < self.tone).then(|| SWEEP_HZ[i])
    }

    pub fn sample(&mut self) -> i16 {
        let s = match self.tone_at(self.pos) {
            Some(hz) => {
                let (_, s) = cos_sin_phase(self.phase);
                self.phase = self.phase.wrapping_add(phase_step(hz as u64 * 1000, RATE_HZ as u64 * 1000));
                (s * self.amp) >> 15
            }
            None => {
                self.phase = 0;
                0
            }
        };
        let pass = self.pass();
        self.pos = if self.once { (self.pos + 1).min(pass) } else { (self.pos + 1) % pass };
        s as i16
    }
}

/// A steady tone (or silence at `amp` 0), phase-continuous, from phase 0: the suite's markers and silences.
#[derive(Clone, Copy, Debug)]
pub struct Tone {
    phase: u32,
    step: u32,
    amp: i32,
}

impl Tone {
    /// `hz` at a peak of `amp` (full scale 32767).
    pub fn new(hz: u16, amp: i32) -> Self {
        Tone { phase: 0, step: phase_step(hz as u64 * 1000, RATE_HZ as u64 * 1000), amp }
    }

    pub fn sample(&mut self) -> i16 {
        let (_, s) = cos_sin_phase(self.phase);
        self.phase = self.phase.wrapping_add(self.step);
        ((s * self.amp) >> 15) as i16
    }
}

/// Bell 202 flags, phase-continuous, at [`AFSK_AMP`] (the APRS beacon's level, in the `data` set-up) times the arming's level.
#[derive(Clone, Copy, Debug)]
pub struct Afsk {
    phase: u32,
    bit_phase: u32,
    /// The bit of the flag being sent (0-7, LSB first).
    bit: u8,
    mark: bool,
    amp: i32,
}

impl Default for Afsk {
    fn default() -> Self {
        Self::new(LEVEL_DEFAULT)
    }
}

const MARK_STEP: u32 = phase_step(1_200_000, RATE_HZ as u64 * 1000);
const SPACE_STEP: u32 = phase_step(2_200_000, RATE_HZ as u64 * 1000);

impl Afsk {
    /// At `level` percent of [`AFSK_AMP`].
    pub fn new(level: u8) -> Self {
        // The first bit (a 0) toggles: the pattern starts on space.
        Afsk { phase: 0, bit_phase: 0, bit: 0, mark: false, amp: scaled(AFSK_AMP as i32, level) }
    }

    /// Whether the tone now is mark (1200 Hz).
    pub fn mark(&self) -> bool {
        self.mark
    }

    pub fn sample(&mut self) -> i16 {
        let (_, s) = cos_sin_phase(self.phase);
        self.phase = self.phase.wrapping_add(if self.mark { MARK_STEP } else { SPACE_STEP });
        let before = self.bit_phase;
        self.bit_phase = self.bit_phase.wrapping_add(MARK_STEP); // 1200 baud
        if self.bit_phase < before {
            self.bit = (self.bit + 1) & 7;
            // NRZI: a 0 changes the tone.
            if (0x7E >> self.bit) & 1 == 0 {
                self.mark = !self.mark;
            }
        }
        ((s * self.amp) >> 15) as i16
    }
}

/// `afsk-bk`'s tones (Bell 202): mark and space, Hz, and the bit rate.
pub const BK_MARK_HZ: u16 = 1200;
pub const BK_SPACE_HZ: u16 = 2200;
pub const BAUD: u32 = 1200;

/// `afsk-bk`'s tone 1 gain at 100 % (REG_70[14:8], 0-127): 66, BD1EEA's `APRS_TONE_GAIN` on the BK4819/BK4829
/// family, with REG_40's deviation field (0x516 there) within 1 % of the 0x50A every Ferrite key-up writes. Meant
/// as about 3 kHz in wide FM; nothing of ours has measured the generator's deviation per gain step yet (TONE-19's
/// 0x17 is still "to confirm on the bench"), so `tools/sdrtx.py --afsk` measures it and LEVEL trims it.
pub const BK_GAIN: u8 = 66;

/// [`BK_GAIN`] at `level` percent, rounded, kept inside REG_70's field (1-127).
pub const fn bk_gain(level: u8) -> u8 {
    let g = (BK_GAIN as u32 * level as u32 + 50) / 100;
    if g < 1 {
        1
    } else if g > 127 {
        127
    } else {
        g as u8
    }
}

/// The bit (from the key-up's first, 0) at which the `k`th tone change (from 0) of an endless run of flags comes.
/// NRZI: a 0 changes the tone, a 1 keeps it; 0x7E sent LSB first is 0 1 1 1 1 1 1 0, so the changes come at each
/// flag's first and last bit, in pairs one bit apart: 0 (the start, already on space), 7, 8, 15, 16, ... The
/// key-up's start counts as the first flag's first change (the tone before it, mark, is never sent), as the DAC
/// generator [`Afsk`] starts on space.
pub const fn toggle_bit(k: u32) -> u32 {
    (k / 2) * 8 + 7 + (k & 1)
}

/// The tone after `n` changes: space at the start, then mark, space, ...
pub const fn bk_tone(n: u32) -> u16 {
    if n.is_multiple_of(2) { BK_SPACE_HZ } else { BK_MARK_HZ }
}

/// Bit boundaries in a timer's ticks, exact in the long run: each bit is `tick_hz / baud` ticks plus the remainder
/// carried (Bresenham), so after N bits the boundary is exactly floor(N x tick_hz / baud) ticks from the start, and
/// no bit is off by more than one tick. The tick count wraps at 2^32 (a 16-bit timer compares the low half).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitClock {
    q: u32,
    r: u32,
    den: u32,
    rem: u32,
    at: u32,
}

impl BitClock {
    pub const fn new(tick_hz: u32, baud: u32) -> Self {
        BitClock { q: tick_hz / baud, r: tick_hz % baud, den: baud, rem: 0, at: 0 }
    }

    /// `bits` more bits: the boundary after them, ticks from the start.
    pub fn advance(&mut self, bits: u32) -> u32 {
        for _ in 0..bits {
            self.at = self.at.wrapping_add(self.q);
            self.rem += self.r;
            if self.rem >= self.den {
                self.rem -= self.den;
                self.at = self.at.wrapping_add(1);
            }
        }
        self.at
    }
}

/// `afsk-bk`'s schedule for the timer interrupt: each tone change's tick from the start, in order.
#[derive(Clone, Copy, Debug)]
pub struct BkFlags {
    clock: BitClock,
    /// Changes scheduled so far, and the bit the last was at.
    k: u32,
    bit: u32,
}

impl BkFlags {
    /// A timer counting at `tick_hz`.
    pub const fn new(tick_hz: u32) -> Self {
        BkFlags { clock: BitClock::new(tick_hz, BAUD), k: 0, bit: 0 }
    }

    /// The next tone change's tick, from the start (wrapping).
    pub fn next_change(&mut self) -> u32 {
        let b = toggle_bit(self.k);
        self.k += 1;
        let t = self.clock.advance(b - self.bit);
        self.bit = b;
        t
    }
}

/// `fsk` and `gmsk`'s baud range (TERM-33.17) and `g3ruh`'s.
pub const BAUD_MIN: u16 = 300;
pub const BAUD_MAX: u16 = 9600;
pub const G3RUH_BAUD: u16 = 9600;
/// `fsk` and `g3ruh`'s deviation at 100 %.
pub const FSK2_DEV_HZ: i32 = 3000;

/// The DAC amplitude for `hz` of deviation: the sweep's 1 kHz tone ([`SWEEP_AMP`]) as about 3.0 kHz (`data`, bench
/// Oct 27, 2026). Nominal: the NRZ signals' low end and the other set-ups don't follow a 1 kHz tone exactly.
pub const fn dev_amp(hz: i32) -> i32 {
    hz * SWEEP_AMP / 3000
}

/// G3RUH's scrambler, x^17 + x^12 + 1, self-synchronising: each bit out is the bit in XOR the bits sent 12 and 17
/// before. The receiver's [`descramble`] undoes it from the received bits alone.
#[derive(Clone, Copy, Debug, Default)]
pub struct Scrambler(u32);

impl Scrambler {
    pub fn scramble(&mut self, b: u8) -> u8 {
        let o = (b ^ (self.0 >> 11) as u8 ^ (self.0 >> 16) as u8) & 1;
        self.0 = ((self.0 << 1) | o as u32) & 0x1_FFFF;
        o
    }
}

/// The receiver's half of [`Scrambler`] (the PC's analysis does the same; here for the tests): the bit in XOR the
/// bits received 12 and 17 before.
pub fn descramble(state: &mut u32, b: u8) -> u8 {
    let o = (b ^ (*state >> 11) as u8 ^ (*state >> 16) as u8) & 1;
    *state = ((*state << 1) | (b & 1) as u32) & 0x1_FFFF;
    o
}

/// The bits the NRZ signals carry.
#[derive(Clone, Copy, Debug)]
pub struct Bits {
    prbs: Prbs9,
    scr: Option<Scrambler>,
}

impl Bits {
    /// PRBS9, or (`g3ruh`) PRBS9 through G3RUH's scrambler.
    pub fn new(scrambled: bool) -> Self {
        Bits { prbs: Prbs9::default(), scr: scrambled.then(Scrambler::default) }
    }

    pub fn bit(&mut self) -> u8 {
        let b = self.prbs.bit();
        match &mut self.scr {
            Some(s) => s.scramble(b),
            None => b,
        }
    }
}

/// Edges of the NRZ signals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Hard,
    /// A half cosine one bit long, centred on the boundary.
    RaisedCosine,
    /// GMSK's Gaussian, BT 0.5 ([`GAUSS`]).
    Gauss,
}

/// GMSK's BT.
pub const GAUSS_BT: f64 = 0.5;
/// [`GAUSS`]'s points a bit; it spans 3 bits (the pulse of one bit, from 1.5 bits before its centre to 1.5 after).
pub const GAUSS_RES: usize = 64;
const GAUSS_LEN: usize = 3 * GAUSS_RES + 1;

/// erf, for the table (a series to |x| 4.5; beyond, 1 to within 2e-10).
const fn cerf(x: f64) -> f64 {
    if x > 4.5 {
        return 1.0;
    }
    if x < -4.5 {
        return -1.0;
    }
    // 2 / sqrt(pi) * sum (-1)^n x^(2n+1) / (n! (2n+1))
    let mut term = x; // (-1)^n x^(2n+1) / n!
    let mut sum = x;
    let mut n = 1;
    while n < 120 {
        term = -term * x * x / n as f64;
        sum += term / (2 * n + 1) as f64;
        n += 1;
    }
    sum * core::f64::consts::FRAC_2_SQRT_PI
}

const fn gauss_table() -> [i16; GAUSS_LEN] {
    // A bit's frequency pulse: a unit rectangle one bit wide through a Gaussian of BT: 0.5 (erf((u + 1/2) / s) -
    // erf((u - 1/2) / s)), s = sqrt(2) sigma = sqrt(2 ln 2) / (2 pi BT), u in bits from the bit's centre.
    let s = 1.177_410_022_515_474_7 / (2.0 * core::f64::consts::PI * GAUSS_BT);
    let mut t = [0i16; GAUSS_LEN];
    let mut j = 0;
    while j < GAUSS_LEN {
        let u = j as f64 / GAUSS_RES as f64 - 1.5;
        let p = 0.5 * (cerf((u + 0.5) / s) - cerf((u - 0.5) / s));
        t[j] = (p * 32767.0 + 0.5) as i16;
        j += 1;
    }
    t
}

/// GMSK's bit pulse, Q15 (32767 for a long run of one bit), from 1.5 bits before the bit's centre to 1.5 after,
/// [`GAUSS_RES`] points a bit. A `static`: read per sample by the DSP task (fast flash).
pub static GAUSS: [i16; GAUSS_LEN] = gauss_table();

/// `fsk`, `gmsk` and `g3ruh`: NRZ bits at any baud, a sample at a time.
#[derive(Clone, Copy, Debug)]
pub struct Nrz {
    bits: Bits,
    /// The bit before, this bit, the bit after, +-1.
    prev: i8,
    cur: i8,
    next: i8,
    /// Position in this bit (2^32 a bit) and the step a sample.
    phase: u32,
    step: u32,
    shape: Shape,
    amp: i32,
}

impl Nrz {
    /// `baud` bits a second of `bits`, edges `shape`, a 1 at +`amp` (before the inversion).
    pub fn new(bits: Bits, baud: u16, shape: Shape, amp: i32) -> Self {
        let mut n = Nrz { bits, prev: 0, cur: 0, next: 0, phase: 0, step: phase_step(baud as u64 * 1000, RATE_HZ as u64 * 1000), shape, amp };
        // The first bit before the start reads as the first bit (no edge at the start).
        n.next = n.take();
        n.cur = n.next;
        n.prev = n.cur;
        n.next = n.take();
        n
    }

    /// The bits in the order sent ([`Nrz::new`] has taken the first two).
    fn take(&mut self) -> i8 {
        if self.bits.bit() == 1 { 1 } else { -1 }
    }

    /// The level now, +-32767 for +-1.
    fn level(&self) -> i32 {
        let frac = self.phase;
        match self.shape {
            Shape::Hard => self.cur as i32 * 32767,
            Shape::RaisedCosine => {
                // sin(pi frac): the first half-bit meets the edge before, the second the edge after.
                let (_, s) = cos_sin_phase(frac >> 1);
                let o = if frac < 1 << 31 { self.prev } else { self.next } as i32;
                let c = self.cur as i32;
                ((c + o) * 32767 + (c - o) * s) / 2
            }
            Shape::Gauss => {
                // The pulses of the bit after, this bit and the bit before, each at its offset in the table.
                let i = (frac >> 26) as usize; // GAUSS_RES = 64 points a bit
                let f = ((frac >> 10) & 0xFFFF) as i32;
                let at = |k: usize| {
                    let (a, b) = (GAUSS[k] as i32, GAUSS[k + 1] as i32);
                    a + (((b - a) * f) >> 16)
                };
                self.next as i32 * at(i) + self.cur as i32 * at(i + GAUSS_RES) + self.prev as i32 * at(i + 2 * GAUSS_RES)
            }
        }
    }

    pub fn sample(&mut self) -> i16 {
        let v = self.level();
        let before = self.phase;
        self.phase = self.phase.wrapping_add(self.step);
        if self.phase < before {
            self.prev = self.cur;
            self.cur = self.next;
            self.next = self.take();
        }
        // Negated: the DAC-to-air chain inverts, so a 1 goes out as positive deviation.
        (-((v * self.amp) >> 15)).clamp(-32767, 32767) as i16
    }

    /// The bit now, as sent (+-1), for the tests.
    pub fn bit_now(&self) -> i8 {
        self.cur
    }
}

/// One of the DAC signals, for the DSP task: a `txtest` mode's, or a step of the suite's (TERM-34).
#[derive(Clone, Copy, Debug)]
pub enum Gen {
    Fsk4(Fsk4),
    Sweep(Sweep),
    Afsk(Afsk),
    Tone(Tone),
    Nrz(Nrz),
}

impl Gen {
    /// `s` at `level` percent of its nominal (its default baud); None for `afsk-bk` (not a DAC signal) and `suite`.
    pub fn new(s: Signal, level: u8) -> Option<Gen> {
        Gen::from_arm(&Arm { level, ..Arm::new(s) })
    }

    /// The arming's signal: its level, baud and edges.
    pub fn from_arm(a: &Arm) -> Option<Gen> {
        let level = a.level;
        let baud = a.baud.clamp(BAUD_MIN, BAUD_MAX);
        let rc = if a.shaped { Shape::RaisedCosine } else { Shape::Hard };
        Some(match a.signal {
            Signal::Fsk4 => Gen::Fsk4(Fsk4::new(level)),
            Signal::Sweep => Gen::Sweep(Sweep::new(level)),
            Signal::Afsk => Gen::Afsk(Afsk::new(level)),
            Signal::Fsk => Gen::Nrz(Nrz::new(Bits::new(false), baud, rc, scaled(dev_amp(FSK2_DEV_HZ), level))),
            Signal::Gmsk => Gen::Nrz(Nrz::new(Bits::new(false), baud, Shape::Gauss, scaled(dev_amp(baud as i32 / 4), level))),
            Signal::G3ruh => Gen::Nrz(Nrz::new(Bits::new(true), G3RUH_BAUD, Shape::RaisedCosine, scaled(dev_amp(FSK2_DEV_HZ), level))),
            Signal::AfskBk | Signal::Suite | Signal::Sol950 | Signal::Sol950Cal => return None,
        })
    }

    #[inline]
    pub fn sample(&mut self) -> i16 {
        match self {
            Gen::Fsk4(g) => g.sample(),
            Gen::Sweep(g) => g.sample(),
            Gen::Afsk(g) => g.sample(),
            Gen::Tone(g) => g.sample(),
            Gen::Nrz(g) => g.sample(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    /// TERM-33.2: PRBS9 repeats after 511 bits, not before, and is balanced (256 ones).
    #[test]
    fn prbs9_period() {
        let mut p = Prbs9::default();
        let bits: Vec<u8> = (0..1022).map(|_| p.bit()).collect();
        assert_eq!(bits[..511], bits[511..]);
        assert_eq!(bits[..511].iter().filter(|&&b| b == 1).count(), 256);
        for per in 1..511 {
            assert!(bits[..511 - per].iter().zip(&bits[per..511]).any(|(a, b)| a != b), "period {per}");
        }
    }

    /// TERM-33.2: M17's dibit map, and the four levels all used by the PRBS.
    #[test]
    fn symbol_levels() {
        assert_eq!([level(0, 1), level(0, 0), level(1, 0), level(1, 1)], [3, 1, -1, -3]);
        let mut f = Fsk4::default();
        let pre: Vec<i8> = (0..PREAMBLE_SYMBOLS).map(|_| f.symbol()).collect();
        assert_eq!(PREAMBLE_SYMBOLS, 192);
        assert!(pre.chunks(2).all(|c| c == [3, -3]));
        let mut n = [0; 4];
        for _ in 0..511 {
            n[((f.symbol() + 3) / 2) as usize] += 1;
        }
        assert!(n.iter().all(|&c| c > 100), "{n:?}");
    }

    /// TERM-33.2: the RRC taps are symmetric, sum to SPS, peak at the centre (1 - a + 4a/pi of the unit pulse), and
    /// match the continuous formula; through a matched RRC the symbol centres come out at the levels (no ISI).
    #[test]
    fn rrc_shape() {
        assert_eq!((SPS, TAPS), (10, 81));
        for i in 0..TAPS {
            assert_eq!(RRC[i], RRC[TAPS - 1 - i]);
        }
        let sum: i32 = RRC.iter().map(|&t| t as i32).sum();
        assert!((sum - (SPS as i32) * (1 << RRC_SHIFT)).abs() < 50, "{sum}");
        assert_eq!(RRC.iter().enumerate().max_by_key(|(_, t)| **t).unwrap().0, TAPS / 2);
        // Against an independent float version (std's sin/cos), and the singular points at +-5.
        let f = |t: f64| {
            let pi = core::f64::consts::PI;
            if t == 0.0 {
                return 1.0 - ALPHA + 4.0 * ALPHA / pi;
            }
            if (4.0 * ALPHA * t).abs() == 1.0 {
                return ALPHA / 2f64.sqrt() * ((1.0 + 2.0 / pi) * (pi / (4.0 * ALPHA)).sin() + (1.0 - 2.0 / pi) * (pi / (4.0 * ALPHA)).cos());
            }
            ((pi * t * (1.0 - ALPHA)).sin() + 4.0 * ALPHA * t * (pi * t * (1.0 + ALPHA)).cos()) / (pi * t * (1.0 - (4.0 * ALPHA * t).powi(2)))
        };
        let fs: Vec<f64> = (0..TAPS).map(|i| f((i as f64 - 40.0) / SPS as f64)).collect();
        let fsum: f64 = fs.iter().sum();
        for i in 0..TAPS {
            let want = fs[i] * SPS as f64 / fsum * (1 << RRC_SHIFT) as f64;
            assert!((RRC[i] as f64 - want).abs() <= 1.0, "tap {i}: {} vs {want}", RRC[i]);
        }
        // Each side's continuity at the singular taps (+-5): between its neighbours' trend.
        assert!(RRC[35] > RRC[34] && RRC[36] > RRC[35]);

        // The generator through a matched RRC (unit sum): the PRBS part's symbol centres at +-1, +-3 of FSK_AMP / 3,
        // negated (the DAC-to-air chain inverts).
        let mut g = Fsk4::default();
        let x: Vec<f64> = (0..SPS * 1400).map(|_| g.sample() as f64).collect();
        assert!(x.iter().all(|v| v.abs() < 32000.0));
        let h: Vec<f64> = fs.iter().map(|v| v / fsum).collect();
        let y: Vec<f64> = (0..x.len()).map(|n| (0..TAPS).filter(|&k| k <= n).map(|k| h[k] * x[n - k]).sum()).collect();
        // Delay: the transmit filter's 40 samples plus the receive filter's 40.
        let unit = (FSK_AMP / 3) as f64;
        let mut f2 = Fsk4::default();
        let want: Vec<i8> = (0..1400).map(|_| f2.symbol()).collect();
        for (m, &w) in want.iter().enumerate().skip(220).take(1000) {
            let got = -y[m * SPS + 80] / unit;
            assert!((got - w as f64).abs() < 0.08, "symbol {m}: {got} vs {w}");
        }
    }

    /// TERM-33.3: the sweep's 23 third-octave tones from 50 Hz to 8 kHz, each about 2^(1/3) above the last, its
    /// timeline (the 1 kHz marker, then tone, gap, tone...), and a tone's samples at the right frequency and level.
    #[test]
    fn sweep_list_and_timing() {
        assert_eq!(SWEEP_HZ.len(), 23);
        assert_eq!((SWEEP_HZ[0], SWEEP_HZ[22]), (50, 8000));
        for w in SWEEP_HZ.windows(2) {
            let r = w[1] as f64 / w[0] as f64;
            assert!((r - 2f64.powf(1.0 / 3.0)).abs() < 0.03, "{w:?}");
        }
        assert_eq!(SWEEP_SAMPLES, 48 * (550 + 23 * 350));
        assert_eq!(sweep_tone(0), Some(1000));
        assert_eq!(sweep_tone(48 * 500), None);
        assert_eq!(sweep_tone(48 * 550), Some(50));
        assert_eq!(sweep_tone(48 * 850), None);
        assert_eq!(sweep_tone(48 * 900), Some(63));
        assert_eq!(sweep_tone(SWEEP_SAMPLES - 1), None);
        assert_eq!(sweep_tone(SWEEP_SAMPLES), Some(1000)); // and again
        // The 8 kHz tone: zero crossings at 8 per ms, its samples' peak sin(60 deg) of SWEEP_AMP (six a cycle).
        let mut s = Sweep::default();
        let start = (48 * (550 + 22 * 350)) as usize;
        let x: Vec<i16> = (0..start + 48 * 300).map(|_| s.sample()).collect();
        let t = &x[start..];
        let ups = t.windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count();
        assert!((2399..=2401).contains(&ups), "{ups}");
        let peak = t.iter().map(|v| v.unsigned_abs()).max().unwrap() as i32;
        assert!((peak - SWEEP_AMP * 866 / 1000).abs() < 20, "{peak}");
    }

    /// TERM-33.4: AFSK flags: seven bits of space, one of mark, at 1200 baud (40 samples a bit), phase-continuous.
    #[test]
    fn afsk_flags() {
        let mut a = Afsk::default();
        let mut marks = Vec::new();
        let mut x = Vec::new();
        for _ in 0..40 * 64 {
            marks.push(a.mark());
            x.push(a.sample());
        }
        // Per bit (sampled mid-bit): the 8-bit pattern repeats; one mark bit in eight.
        let bits: Vec<bool> = (0..64).map(|b| marks[b * 40 + 20]).collect();
        assert_eq!(bits.iter().filter(|&&m| m).count(), 8);
        assert_eq!(bits[..8], bits[8..16]);
        for w in x.windows(2) {
            assert!((w[1] as i32 - w[0] as i32).abs() < 2500, "a jump: not phase-continuous");
        }
    }

    #[test]
    fn names() {
        for s in Signal::ALL {
            assert_eq!(Signal::parse(s.name().as_bytes()), Some(s));
        }
        assert_eq!(Signal::parse(b"SWEEP"), Some(Signal::Sweep));
        assert_eq!(Signal::parse(b"fsk"), Some(Signal::Fsk));
        assert_eq!(Signal::parse(b"2fsk"), None);
        for f in Filters::ALL {
            assert_eq!(Filters::parse(f.name().to_ascii_uppercase().as_bytes()), Some(f));
        }
        assert_eq!(Filters::parse(b"loud"), None);
        assert_eq!(Signal::ALL.map(Signal::default_filters)[..5], [Filters::Data, Filters::Data, Filters::Data, Filters::Voice, Filters::Data]);
        assert!(Signal::ALL[5..8].iter().all(|s| s.default_filters() == Filters::Data && s.dac() && s.takes_level()));
        assert_eq!(Signal::ALL.map(Signal::takes_baud)[5..8], [true, true, false]);
        assert!(Signal::ALL[8..].iter().all(|s| s.is_sol950() && !s.dac() && s.default_filters() == Filters::Voice));
        assert_eq!(Signal::parse(b"AFSK-BK"), Some(Signal::AfskBk));
        assert_eq!(Signal::parse(b"SOL950"), Some(Signal::Sol950));
        assert_eq!(Signal::parse(b"sol950-cal"), Some(Signal::Sol950Cal));
        assert_eq!(Signal::parse(b"Suite"), Some(Signal::Suite));
        assert_eq!(Signal::ALL.map(Signal::dac)[..5], [true, true, true, false, false]);
        assert_eq!(Signal::ALL.map(Signal::takes_level)[..5], [true, true, true, true, false]);
        assert!(Gen::new(Signal::AfskBk, 100).is_none(), "the DSP task never plays afsk-bk");
        assert!(Gen::new(Signal::Sol950, 100).is_none(), "the DSP task never plays sol950");
        assert!(Gen::new(Signal::Suite, 100).is_none(), "the suite's DAC parts come step by step");

        // Verify SOL950_REF_TONES matches `CQ NQ6N FN31 (tx_pwr = 10)` from `usol950::encode_tones`.
        let nq6n = usol950::Callsign::encode(b"NQ6N").unwrap();
        let fn31 = usol950::frame::encode_grid4(b"FN31").unwrap();
        let want = usol950::encode_tones(usol950::Frame::Cq { sender: nq6n, grid4: fn31, tx_pwr: 10, reserved: 0 });
        assert_eq!(SOL950_REF_TONES, want);
        let sched = Sol950Bk::new(false, 100);
        assert_eq!(sched.gain_at(0), 16);
        assert_eq!(sched.gain_at(Sol950Bk::LEAD_STEPS + 64 + 8), 112); // symbol 1 (`S0[1] = 3`), after 8-step ramp
    }

    /// TERM-33.2: 4FSK polarity. The DAC-to-air chain inverts, so the generator sends +3 as a negative swing: the
    /// preamble's first symbol (+3) comes out negative at its centre, the second (-3) positive (and `rrc_shape`'s
    /// matched-filter round trip checks every PRBS symbol negated).
    #[test]
    fn fsk4_polarity() {
        let mut f = Fsk4::default();
        // The preamble (+3, -3, ...): its first symbol's centre, 40 samples of filter delay in.
        let x: Vec<i16> = (0..SPS * 8).map(|_| f.sample()).collect();
        assert!(x[TAPS / 2] < -(FSK_AMP / 2) as i16, "{}", x[TAPS / 2]);
        assert!(x[TAPS / 2 + SPS] > (FSK_AMP / 2) as i16, "{}", x[TAPS / 2 + SPS]);
    }

    /// TERM-33.6: the level scales every signal's amplitude by its percent (10 to 150), and 100 is the nominal.
    #[test]
    fn level_scaling() {
        let peak = |s: Signal, level: u8, n: usize| {
            let mut g = Gen::new(s, level).unwrap();
            (0..n).map(|_| g.sample().unsigned_abs() as i32).max().unwrap()
        };
        for s in Signal::ALL.into_iter().filter(|s| s.dac()) {
            let n = 48 * 600;
            let full = peak(s, LEVEL_DEFAULT, n);
            for level in [LEVEL_MIN, 50, LEVEL_MAX] {
                let got = peak(s, level, n);
                let want = full * level as i32 / 100;
                assert!((got - want).abs() <= want / 50 + 2, "{s:?} at {level}: {got} vs {want}");
            }
        }
        // The nominal peaks: AFSK's sine at AFSK_AMP, the sweep's marker at SWEEP_AMP.
        assert!((peak(Signal::Afsk, 100, 4800) - AFSK_AMP as i32).abs() < 20);
        assert!((peak(Signal::Sweep, 100, 4800) - SWEEP_AMP).abs() < 20);
        // 150 % stays inside the DAC.
        assert!(peak(Signal::Fsk4, LEVEL_MAX, 48_000) < 32767);
    }

    /// The tone per bit of an endless run of flags, NRZI from scratch: 0x7E LSB first, a 0 changes the tone, the
    /// tone before the first bit mark.
    fn nrzi_flag_tones(bits: usize) -> Vec<u16> {
        let mut tone = BK_MARK_HZ;
        (0..bits)
            .map(|b| {
                if (0x7E >> (b % 8)) & 1 == 0 {
                    tone = if tone == BK_MARK_HZ { BK_SPACE_HZ } else { BK_MARK_HZ };
                }
                tone
            })
            .collect()
    }

    /// TERM-33.5 `afsk-bk`: the tone changes ([`toggle_bit`]) and the tone after each ([`bk_tone`]) are NRZI flags
    /// bit for bit (a change at each flag's first and last bit, the start on space), and the same pattern as the
    /// DAC generator's (`afsk`, sampled mid-bit), so the two modes compare on the air.
    #[test]
    fn bk_flags_nrzi() {
        let want = nrzi_flag_tones(800);
        assert_eq!(want[..9], [2200, 2200, 2200, 2200, 2200, 2200, 2200, 1200, 2200]);
        let changes: Vec<u32> = (1..800u32).filter(|&b| want[b as usize] != want[b as usize - 1]).collect();
        let sched: Vec<u32> = (0..changes.len() as u32).map(toggle_bit).collect();
        assert_eq!(changes, sched);
        assert_eq!(sched[..6], [7, 8, 15, 16, 23, 24]);
        // The tone after n changes, bit by bit.
        let mut n = 0;
        for (b, &t) in want.iter().enumerate() {
            while n < sched.len() && sched[n] as usize <= b {
                n += 1;
            }
            assert_eq!(bk_tone(n as u32), t, "bit {b}");
        }
        // The DAC generator: 40 samples a bit, its tone mid-bit.
        let mut a = Afsk::default();
        let dac: Vec<u16> = (0..800 * 40)
            .map(|_| {
                let m = a.mark();
                a.sample();
                if m { BK_MARK_HZ } else { BK_SPACE_HZ }
            })
            .collect();
        for b in 0..800 {
            assert_eq!(dac[b * 40 + 20], want[b], "bit {b}: afsk and afsk-bk differ");
        }
    }

    /// TERM-33.5 `afsk-bk`: the bit clock is exact in the long run. At 1.2 MHz (the timer's: 120 MHz / 100) a bit is
    /// exactly 1000 ticks; at 1 MHz the 833.33 us bits are 833 or 834 ticks, never off the ideal by a tick or more,
    /// and after 1200 bits exactly 1 s; after 10 million bits (2.3 hours) still exact, through the 2^32 wrap.
    #[test]
    fn bit_clock_never_drifts() {
        let mut c = BitClock::new(1_200_000, BAUD);
        assert_eq!((c.advance(1), c.advance(7), c.advance(1)), (1000, 8000, 9000));
        let mut c = BitClock::new(1_000_000, BAUD);
        let mut last = 0u32;
        for n in 1..=12_000u64 {
            let t = c.advance(1);
            let ideal = n as f64 * 1e6 / 1200.0;
            assert!((t as f64 - ideal).abs() < 1.0, "bit {n}: {t} vs {ideal}");
            assert!([833, 834].contains(&t.wrapping_sub(last)), "bit {n}");
            last = t;
            if n == 1200 {
                assert_eq!(t, 1_000_000);
            }
        }
        let mut c = BitClock::new(120_000_000, BAUD); // 100000 a bit: wraps 2^32 after 42950 bits
        let n = 10_000_000u64;
        let mut done = 0u64;
        let mut t = 0;
        while done < n {
            t = c.advance(7);
            done += 7;
        }
        assert_eq!(t, ((done * 120_000_000 / 1200) % (1 << 32)) as u32);
        let mut c = BitClock::new(1_000_000, BAUD);
        let mut t = 0;
        for _ in 0..n / 8 {
            t = c.advance(8);
        }
        assert_eq!(t as u64, (n * 1_000_000 / 1200) % (1 << 32));
    }

    /// TERM-33.5 `afsk-bk`: the schedule the timer interrupt follows is the changes' bits on the bit clock: at
    /// 1.2 MHz bits 7, 8, 15, 16 ... times 1000 ticks; the 7-bit gap and the 1-bit gap alternate. The gain: 66 at
    /// 100 %, scaled by the level and rounded, inside REG_70's 7 bits.
    #[test]
    fn bk_schedule_and_gain() {
        let mut f = BkFlags::new(1_200_000);
        let t: Vec<u32> = (0..6).map(|_| f.next_change()).collect();
        assert_eq!(t, [7000, 8000, 15000, 16000, 23000, 24000]);
        let mut f = BkFlags::new(1_000_000);
        for k in 0..10_000u32 {
            let want = (toggle_bit(k) as u64 * 1_000_000 / 1200) as u32;
            assert_eq!(f.next_change(), want, "change {k}");
        }
        assert_eq!((bk_gain(100), bk_gain(LEVEL_MIN), bk_gain(50), bk_gain(LEVEL_MAX)), (66, 7, 33, 99));
        assert_eq!((bk_gain(0), bk_gain(255)), (1, 127));
    }

    /// APRS-14.3 (decision 135), TERM-33.8: an APRS burst's key-up takes the `data` set-up whatever else is armed (a
    /// data key-up never plays a test); a test key-up its filters, none for `voice` or `afsk-bk`; a voice key-up none.
    #[test]
    fn key_up_set_up() {
        let arm = |signal, filters| Some(Arm { filters, ..Arm::new(signal) });
        assert_eq!(key_up_filters(true, None), Some(Filters::Data));
        assert_eq!(key_up_filters(true, arm(Signal::Fsk4, Filters::Flat)), Some(Filters::Data));
        assert_eq!(key_up_filters(false, None), None);
        assert_eq!(key_up_filters(false, arm(Signal::Fsk4, Filters::Flat)), Some(Filters::Flat));
        assert_eq!(key_up_filters(false, arm(Signal::Afsk, Signal::Afsk.default_filters())), Some(Filters::Data));
        assert_eq!(key_up_filters(false, arm(Signal::Afsk, Filters::Voice)), None);
        assert_eq!(key_up_filters(false, arm(Signal::AfskBk, Filters::Voice)), None);
        assert_eq!(key_up_filters(false, arm(Signal::AfskBk, Filters::Data)), None, "afsk-bk never changes the filters");
        assert_eq!(key_up_filters(false, arm(Signal::Suite, Filters::Data)), None, "the suite sets its own per step");
    }

    /// PRBS9's first `n` bits.
    fn prbs(n: usize) -> Vec<u8> {
        let mut p = Prbs9::default();
        (0..n).map(|_| p.bit()).collect()
    }

    /// TERM-33.17 `fsk BAUD [rc]`: at any baud from 300 to 9600 (whole samples a bit or not), the signal sampled
    /// mid-bit is PRBS9 bit for bit, a 1 negative at the DAC (the chain inverts: positive deviation on the air) at
    /// +-3 kHz's amplitude; `rc`'s edges are a half cosine (mid-way at the boundary, the full level mid-bit, no
    /// jump), the hard edges a step; the level scales it.
    #[test]
    fn fsk_bits() {
        let amp = dev_amp(FSK2_DEV_HZ);
        assert_eq!(amp, SWEEP_AMP);
        for baud in [300u16, 1200, 2000, 2400, 4800, 7000, 9600] {
            for shaped in [false, true] {
                let a = Arm { baud, shaped, ..Arm::new(Signal::Fsk) };
                let Some(Gen::Nrz(mut g)) = Gen::from_arm(&a) else { panic!() };
                let spb = RATE_HZ as f64 / baud as f64;
                let n = 600;
                let x: Vec<i16> = (0..(n as f64 * spb) as usize).map(|_| g.sample()).collect();
                let want = prbs(n);
                for (k, &b) in want.iter().enumerate().take(n - 2) {
                    let v = x[((k as f64 + 0.5) * spb) as usize] as i32;
                    let lim = if shaped && spb < 8.0 { amp * 80 / 100 } else { amp * 97 / 100 };
                    assert!(if b == 1 { v < -lim } else { v > lim }, "{baud} {shaped} bit {k}: {v}");
                }
                assert!(x.iter().all(|v| (v.unsigned_abs() as i32) <= amp + 1));
                if shaped {
                    let max_jump = x.windows(2).map(|w| (w[1] as i32 - w[0] as i32).abs()).max().unwrap();
                    assert!(max_jump as f64 <= 2.0 * amp as f64 * (core::f64::consts::PI / 2.0 / spb).sin() * 1.05 + 4.0, "{baud}: jump {max_jump}");
                }
            }
        }
        let Some(Gen::Nrz(mut g)) = Gen::from_arm(&Arm { level: 50, ..Arm::new(Signal::Fsk) }) else { panic!() };
        assert!((0..4800).map(|_| g.sample().unsigned_abs() as i32).max().unwrap() == SWEEP_AMP / 2);
        assert_eq!(Arm::new(Signal::Fsk).baud, 1200);
    }

    /// TERM-33.17 `gmsk BAUD`: the Gaussian bit pulse (BT 0.5) is symmetric, sums to one over its shifts (a run of
    /// one bit at the full level), and peaks under one; the signal mid-bit has PRBS9's sign, a 1 negative at the
    /// DAC; the deviation is BAUD / 4 (h 0.5) at 100 %: a 0101 run peaks lower than a long run (the Gaussian's ISI).
    #[test]
    fn gmsk_bits() {
        for j in 0..GAUSS.len() {
            assert_eq!(GAUSS[j], GAUSS[GAUSS.len() - 1 - j]);
        }
        for i in 0..GAUSS_RES {
            let sum: i32 = (0..3).map(|d| GAUSS[i + d * GAUSS_RES] as i32).sum();
            assert!((sum - 32767).abs() <= 3, "offset {i}: {sum}");
        }
        assert!(GAUSS[0] < 10 && GAUSS[GAUSS_RES * 3 / 2] < 32767 && GAUSS[GAUSS_RES * 3 / 2] > 25000);
        for baud in [1200u16, 2400, 4800, 9600] {
            let a = Arm { baud, ..Arm::new(Signal::Gmsk) };
            let Some(Gen::Nrz(mut g)) = Gen::from_arm(&a) else { panic!() };
            let amp = dev_amp(baud as i32 / 4);
            let spb = RATE_HZ as f64 / baud as f64;
            let n = 600;
            let x: Vec<i16> = (0..(n as f64 * spb) as usize).map(|_| g.sample()).collect();
            for (k, &b) in prbs(n).iter().enumerate().take(n - 2) {
                let v = x[((k as f64 + 0.5) * spb) as usize] as i32;
                assert!(if b == 1 { v < -amp / 3 } else { v > amp / 3 }, "{baud} bit {k}: {v}");
            }
            assert!(x.iter().all(|v| (v.unsigned_abs() as i32) <= amp + 1));
        }
        assert_eq!(dev_amp(4800 / 4), 1600);
    }

    /// TERM-33.17 `g3ruh`: G3RUH's scrambler is x^17 + x^12 + 1 (each bit out the bit in XOR the bits out 12 and 17
    /// before), and the receiver's descrambler gives PRBS9 back from the bits on the air alone; the generator sends
    /// the scrambled bits at 9600 Bd, raised-cosine edges, +-3 kHz.
    #[test]
    fn g3ruh_scrambler() {
        let data = prbs(3000);
        let mut s = Scrambler::default();
        let air: Vec<u8> = data.iter().map(|&b| s.scramble(b)).collect();
        for n in 0..air.len() {
            let back = |k: usize| if n >= k { air[n - k] } else { 0 };
            assert_eq!(air[n], data[n] ^ back(12) ^ back(17), "bit {n}");
        }
        assert_ne!(air, data);
        // The descrambler, started anywhere: right after 17 bits.
        let mut st = 0u32;
        let got: Vec<u8> = air[100..].iter().map(|&b| descramble(&mut st, b)).collect();
        assert_eq!(got[17..], data[117..]);
        // The generator: the scrambled bits, mid-bit (5 samples a bit), a 1 negative at the DAC.
        let a = Arm::new(Signal::G3ruh);
        assert_eq!((a.baud, a.shaped), (9600, true));
        let Some(Gen::Nrz(mut g)) = Gen::from_arm(&a) else { panic!() };
        let x: Vec<i16> = (0..5 * 600).map(|_| g.sample()).collect();
        for k in 0..598 {
            let v = x[k * 5 + 2] as i32;
            assert!(if air[k] == 1 { v < -SWEEP_AMP / 2 } else { v > SWEEP_AMP / 2 }, "bit {k}: {v}");
        }
    }
}
