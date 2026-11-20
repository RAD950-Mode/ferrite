//! Each BK4829 set-up the Radio task writes, as one function over the driver: the register recipes are the
//! driver's (stock's values, uradio's `bk4829`), the order and the choices are the Radio task's. The firmware calls
//! these on its bit-banged bus; the tests on a recording bus.

use bk4829::{AfOut, Bandwidth, Bk4829, Bus, Demod, Fsk};
use ferrite_core::freq::{mhz, Freq10};
use ferrite_core::same_rx;
use ferrite_core::settle::Settle;

/// What a chip needs to receive a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxSetup {
    /// Where the chip tunes (the clarifier and the factory crystal trim already applied, CAL-12), 10 Hz units.
    pub freq: Freq10,
    pub narrow: bool,
    /// AM (receive only, TX-20).
    pub am: bool,
    /// The receive tone word (CHAN-3): CTCSS in 0.1 Hz, bit 15 DCS, bit 14 inverted; 0 none.
    pub rx_tone: u16,
    /// Tail elimination on receive (TONE-6): with no receive tone, the 55 Hz tail mutes.
    pub rx_tail: bool,
    /// APRS-55 (decision 309): the APRS slot's receive data set-up: an FM tune writes [`DATA_RX_AF`] into REG_2B.
    /// REG_28's noise gate and REG_73's AFC are [`rx_data`]'s.
    pub data: bool,
}

/// Tune a chip to receive with no settle wait (stock's order): [`wake`], then [`tune_rest`]. The Radio task does
/// the two apart, with PWR-21's wait between them when the chip was asleep.
pub fn tune<B: Bus>(bk: &mut Bk4829<B>, s: &RxSetup) {
    bk.wake();
    tune_rest(bk, s);
}

/// The receive set-up after the wake (`Radio::tune_chip`): audio muted first (the audio decision unmutes the one
/// heard; a receiving chip being retuned is silent through it), the frequency, the bandwidth, FM or AM (stock's
/// modes 7 and 6; VFO-20: AM keeps the wide filter), the sub-tone detector, the receive level, then receive on.
pub fn tune_rest<B: Bus>(bk: &mut Bk4829<B>, s: &RxSetup) {
    bk.af_out(AfOut::Mute);
    bk.set_freq(s.freq * 10);
    bk.bandwidth(if s.narrow { Bandwidth::Narrow } else { Bandwidth::Wide });
    bk.demod(if s.am { Demod::Am } else { Demod::Fm });
    if s.data && !s.am {
        // APRS-55: over demod's FM REG_2B (0x0000), so no read.
        bk.write(bk4829::reg::AF_FILTERS, DATA_RX_AF);
    }
    subtone(bk, s.rx_tone, s.rx_tail);
    bk.af_gain_rx(); // REG_48 0xB2BF, stock's receive level (RX-* / SND-2: the knob is the volume)
    bk.rx_on();
}

/// APRS-55 (decision 309): REG_2B for the APRS slot's FM receive (1 = off): bit 10 the 300 Hz high-pass and bit 8
/// de-emphasis. The 3 kHz low-pass (bit 9) stays on: the chip's fixed receive wall near 3.1 kHz is there either way,
/// and the recorded-audio study (`ferrite-app/tests/rxaf_study.rs`) decoded the same with or without it.
pub const DATA_RX_AF: u16 = 1 << 10 | 1 << 8;
/// REG_28<8>: the receive AF noise gate, on from the start-up's 0x0B40; stock's flat mode 8 clears it.
pub const NOISE_GATE: u16 = 1 << 8;
/// REG_73<4>: AFC, 1 = off. Never written otherwise (reset 0x4682: on, at the widest range); stock's flat mode 8
/// sets it.
pub const AFC_OFF: u16 = 1 << 4;

/// APRS-55 (decision 309): REG_28's noise gate off and REG_73's AFC off for the APRS slot's receive (`on`), both back
/// on for any other; each register's other bits kept, and written only when the bit changes. The Radio task calls
/// it after [`tune_rest`] when the chip's set-up changes kind (and again after the chip has slept).
pub fn rx_data<B: Bus>(bk: &mut Bk4829<B>, on: bool) {
    for (r, bit, set) in [(0x28, NOISE_GATE, !on), (0x73, AFC_OFF, on)] {
        let v = bk.read(r);
        let want = if set { v | bit } else { v & !bit };
        if want != v {
            bk.write(r, want);
        }
    }
}

/// A wait that lets the rest of the firmware run (the Radio task's `Timer::after_millis`; PWR-21's waits are a few
/// ms on the highest-priority executor, too long to spin).
#[allow(async_fn_in_trait)]
pub trait Delay {
    async fn ms(&mut self, ms: u32);
}

/// PWR-21 (decision 119): tune a chip with the wake settle. `woke` is when its REG_37 supplies came on (the Radio
/// task's record), None if it is asleep: then they come on here, at `now`. Whatever is left of `settle.wake_ms`
/// since then is waited before [`tune_rest`] (nothing for a chip woken a tick early by the hop, or long awake).
/// Returns the wake time to keep.
pub async fn tune_settled<B: Bus, D: Delay>(bk: &mut Bk4829<B>, d: &mut D, s: &RxSetup, settle: Settle, woke: Option<u32>, now: u32) -> u32 {
    let (at, left) = wake_start(bk, settle, woke, now);
    if left > 0 {
        d.ms(left).await;
    }
    tune_rest(bk, s);
    at
}

/// [`tune_settled`]'s first half (the Radio task runs the halves apart, around its own timer): REG_37's supplies
/// on if `woke` is None, and returns (the wake time, the ms of settle left at `now`).
pub fn wake_start<B: Bus>(bk: &mut Bk4829<B>, settle: Settle, woke: Option<u32>, now: u32) -> (u32, u32) {
    let at = woke.unwrap_or_else(|| {
        bk.wake();
        now
    });
    (at, settle.wake_left(at, now))
}

/// PWR-21: a chip to sleep with the sleep settle: [`sleep_begin`] (muted, REG_30 off), `settle.sleep_ms`, then
/// REG_37's supplies off. With 0 it is stock's two writes back to back (after the mute).
pub async fn sleep_settled<B: Bus, D: Delay>(bk: &mut Bk4829<B>, d: &mut D, settle: Settle) {
    sleep_begin(bk);
    if settle.sleep_ms > 0 {
        d.ms(settle.sleep_ms as u32).await;
    }
    bk.ldo_sleep();
}

/// PWR-21, the sleep's first half: the AF route muted, then REG_30 off. The Radio task waits the sleep settle, then
/// [`Bk4829::ldo_sleep`]. Muting first means the chip never goes off, nor comes back, with its audio routed.
pub fn sleep_begin<B: Bus>(bk: &mut Bk4829<B>) {
    bk.af_out(AfOut::Mute);
    bk.off();
}

/// The slot's own sub-tone detector (CHAN-3, TONE-6): set by [`tune`], and put back after a tone hint's probe.
pub fn subtone<B: Bus>(bk: &mut Bk4829<B>, rx_tone: u16, rx_tail: bool) {
    match rx_tone {
        // TONE-6: no tone to lose, so the detector listens for the 55 Hz tail instead (stock: REG_07 0x21CD).
        0 if rx_tail => {
            bk.ctcss_rx(550);
            bk.tail_detect();
        }
        0 => bk.subtone_off(),
        t if t & 0x8000 != 0 => bk.dcs_rx(t & 0x3FFF, t & 0x4000 != 0),
        t => bk.ctcss_rx(t),
    }
}

/// The slot's receive tone is heard (CHAN-3, TONE-20), from REG_0C as stock's 0x0801EAC4 decides: CTCSS on bit 10,
/// a normal DCS code only on bit 14 (positive code), an inverted one only on bit 15 (negative code). The other
/// polarity of the same code keeps the squelch shut (decision 142: Ferrite opened on either). `rx_tone` 0 is true.
pub fn tone_heard(rx_tone: u16, st: &bk4829::Status) -> bool {
    match rx_tone {
        0 => true,
        t if t & 0x8000 == 0 => st.ctcss,
        t if t & 0x4000 != 0 => st.cdcss_neg,
        _ => st.cdcss_pos,
    }
}

/// The squelch thresholds (SQL-1): REG_78 (RSSI open/close) and REG_4F (noise), from
/// [`ferrite_core::sqcal::SqCal::regs`], written as stock's 0x0801E64C does. The glitch thresholds stay as the
/// chip's start-up left them, as stock's do.
pub fn squelch<B: Bus>(bk: &mut Bk4829<B>, regs: (u16, u16)) {
    bk.write(bk4829::reg::SQ_RSSI, regs.0);
    bk.write(bk4829::reg::SQ_NOISE, regs.1);
}

/// SQL-17 (decision 166): ordinary receive's squelch open delay, REG_4E[13:11]: stock's 7, from its start-up table's
/// 0x3815 (0x08009E94), which stock never changes.
pub const OPEN_DELAY_RX: u8 = 7;

/// SQL-17: the open delay for a chip that listens to a scan step or the strip's look (`fast`: 0, so a carrier reads
/// open within the step; the delay at 7 kept a channel scan from ever stopping on the radio, Oct 15, 2026) or for
/// ordinary receive (stock's 7, its debounce kept).
pub fn open_delay<B: Bus>(bk: &mut Bk4829<B>, fast: bool) {
    bk.open_delay(if fast { 0 } else { OPEN_DELAY_RX });
}

/// The tone hint's probe (TSRCH-12): the detectors measure whatever is on air; audio stays as it is (muted).
pub fn tone_probe<B: Bus>(bk: &mut Bk4829<B>) {
    bk.subtone_scan_on();
}

/// What the scan detectors heard as a tone word (CHAN-3's form; TSRCH-13), from [`Bk4829::subtone_scan`]: CTCSS
/// (60-260 Hz only) snapped to the nearest of `ctcss` ([`snap_ctcss`]); DCS matched against `dcs` in
/// any of its 23 rotations, normal before inverted (the complement). None if nothing usable.
#[inline(never)] // in .slow_text (memory.x): once per probe
pub fn heard_tone(ctcss_dhz: Option<u16>, dcs_word: Option<u32>, ctcss: &[u16], dcs: &[u16]) -> Option<u16> {
    if let Some(w) = dcs_word {
        for inverted in [false, true] {
            let mut x = if inverted { !w & 0x7F_FFFF } else { w & 0x7F_FFFF };
            for _ in 0..23 {
                // The word carries its code: data bits 0-8, then the fixed 100 in bits 9-11. Only a rotation
                // with that marker is checked, with one codeword: at most 46 a read, not one per code.
                let c = (x & 0x1FF) as u16;
                if x & 0xE00 == 0x800 && bk4829::dcs_codeword(c, false) == x && dcs.contains(&c) {
                    return Some(0x8000 | if inverted { 0x4000 } else { 0 } | c);
                }
                x = (x >> 1) | (x & 1) << 22;
            }
        }
    }
    let d = ctcss_dhz.filter(|d| (600..=2600).contains(d))?;
    snap_ctcss(d, ctcss)
}

/// A CTCSS reading (0.1 Hz) snapped to the nearest tone of `ctcss` (TSRCH-13, decision 179): the detector's reading
/// wanders about ±1.4 % carrier to carrier, so no fixed window fits. Refused (None) only in the middle fifth between
/// two tones: more than 2/5 of their gap from the nearer (88.5 read 87.3 or 89.6 snaps; 90.0, between 88.5 and 91.5,
/// is no tone). Past either end of the table the gap is the one to the inner neighbour.
fn snap_ctcss(d: u16, ctcss: &[u16]) -> Option<u16> {
    let near = ctcss.iter().copied().min_by_key(|&t| (t.abs_diff(d), t))?;
    // the neighbour across the reading, else (past an end) the next one inward
    let across = |up: bool| ctcss.iter().copied().filter(move |&t| if up { t > near } else { t < near });
    let other = if d >= near { across(true).min().or_else(|| across(false).max()) } else { across(false).max().or_else(|| across(true).min()) };
    let gap = other.map_or(u16::MAX, |o| o.abs_diff(near)) as u32;
    (near.abs_diff(d) as u32 * 5 <= gap * 2).then_some(near)
}

/// A local beep's start on a chip (SND-11, stock 0x0801EB8C): woken first if it was asleep, tone 1 at `tone_gain`
/// (the beep volume, SND-3: `tonegen::beep_gain`; stock's is 0x17). Returns REG_48 for [`beep_end`].
pub fn beep_start<B: Bus>(bk: &mut Bk4829<B>, hz: u32, tone_gain: u8, asleep: bool) -> u16 {
    if asleep {
        bk.wake();
    }
    bk.beep_on_gain(hz, tone_gain)
}

/// The beep's end: tone off, REG_48 back, then the chip asleep again or back to receive.
pub fn beep_end<B: Bus>(bk: &mut Bk4829<B>, gain: u16, asleep: bool) {
    bk.beep_off(gain);
    if asleep {
        bk.sleep();
    } else {
        bk.rx_on();
    }
}

/// The weather channels, 162.400-162.550 MHz (WX-1). A `static`: the Radio task checks it every tick, and only named
/// statics stay in the firmware's fast flash (unnamed constants go to the slow part: firmware/memory.x).
pub static WX: core::ops::RangeInclusive<Freq10> = mhz(162) + 40_000..=mhz(162) + 55_000;

/// The modem in its NOAA SAME receive mode (DEC-19; BK4819 application note, nobody's firmware uses it yet, so
/// every value is bench-confirm, roadmap 6b): REG_58 receive mode 000 with bandwidth 010 and receive gain 3 (bits
/// 7:6 set as in both of stock's modem modes), the rate in Tone 2's word (520.83 baud x 10.32444), the four sync
/// bytes `ZCZC`, no CRC (stock's REG_5C with bit 6 clear), and the longest header's length.
pub const SAME_FSK: Fsk = Fsk {
    mode: 0x03C4,
    tone_word: ((same_rx::BAUD_MILLI as u64 * 1_032_444 + 50_000_000) / 100_000_000) as u16,
    tone_gain: 0,
    preamble: 16,
    sync: same_rx::SYNC,
    sync4: true,
    crc: 0xA730 & !(1 << 6),
    scramble: false,
    len: same_rx::MAX_LEN as u16,
};

/// The SAME modem listening for a burst's sync word.
pub fn same_arm<B: Bus>(bk: &mut Bk4829<B>) {
    bk.fsk_setup(&SAME_FSK);
    bk.fsk_rx(&SAME_FSK);
}

/// Where each chip sits while counting (TSRCH-10): its band's middle, for the front end the path selects.
pub const COUNTER_TUNE: [Freq10; 2] = [mhz(146), mhz(446)];

/// The SI4732's input path for its mode, as stock (BCR-20, `analysis/audio_path.md` §2): FM broadcast PA7 = PC5 = 0,
/// AM (and SW, SSB, CW) PA7 = PC5 = 1, the LNA path. The pins' levels, PA7 first.
pub fn si_input(am: bool) -> [bool; 2] {
    [am, am]
}

/// What the transmitting chip is set to before the path plan keys it (TX-23).
#[cfg(feature = "tx")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxSetup {
    /// The TX frequency's REG_38/39 word (the factory crystal trim applied, CAL-12), 10 Hz units.
    pub freq: Freq10,
    pub narrow: bool,
    /// The sub-tone sent: the tone word (CTCSS in 0.1 Hz, bit 15 DCS, bit 14 inverted; 0 none).
    pub tone: u16,
    /// Its deviation, REG_51[6:0] (TONE-1, TONE-16: `TxCal::tone_dev`).
    pub tone_dev: u8,
    /// REG_7D[5:0], the mic sensitivity (VOX-15: `vox::mic_reg`; a data key-up `vox::MIC_DATA`, VOX-6).
    pub mic: u8,
}

/// The transmitting chip's set-up before the path plan keys it (TX-23), in stock's order (TX start 0x0801EAF8,
/// TX audit F1, F2, F7, F14): out of receive (REG_30 0, stock's 50 us), REG_47 0x6142 (stock's AF route while
/// keyed), the TX frequency, REG_43 with it (0x3028 wide, 0x4008 narrow; 0x0801DE38-0x0801DE52:
/// with one chip awake at a time, decision 111, a cross-band split's TX chip was last set for another slot, M7),
/// the sub-tone with the calibration's deviation, the mic gain (REG_7D), then stock's REG_40 deviation
/// (`(R & 0xF000) | 0x50A`). The chip keeps its last receive mode bits (REG_31 scrambler off).
#[cfg(feature = "tx")]
pub fn tx_setup<B: Bus>(bk: &mut Bk4829<B>, s: &TxSetup) {
    bk.tx_start();
    bk.af_out(AfOut::Normal);
    bk.set_freq(s.freq * 10);
    bk.bandwidth(if s.narrow { Bandwidth::Narrow } else { Bandwidth::Wide });
    match s.tone {
        0 => bk.subtone_off(),
        t if t & 0x8000 != 0 => bk.dcs_tx(t & 0x3FFF, t & 0x4000 != 0),
        t => bk.ctcss_tx(t),
    }
    if s.tone != 0 {
        bk.subtone_gain(s.tone_dev);
    }
    bk.mic_gain(s.mic);
    bk.tx_deviation();
}

/// REG_30 read back after a key-up with the microphone (TX-7): stock's transmit value.
#[cfg(feature = "tx")]
pub const REG30_TX_MIC: u16 = 0xC1FE;

/// What the keyed chip must hold before the PA is driven (TX-7): the TX frequency word Ferrite wrote (10 Hz units,
/// REG_38/39, trimmed: CAL-13) and REG_3B (the crystal register Ferrite never writes: the value read after the chip's start-up).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxExpect {
    pub freq: Freq10,
    pub reg3b: u16,
    /// The keyed slot's bandwidth: stock keys through REG_30 0x0200 on wide, 0 on narrow (F3).
    pub narrow: bool,
}

/// The register a TX-7 read-back found wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadBack {
    /// REG_38/39: the synthesized frequency.
    Freq,
    /// REG_30: not keyed with the microphone.
    Power,
    /// REG_3B: the crystal register changed.
    Xtal,
    /// REG_36: the PA bias didn't take.
    Bias,
}

/// `Act::ChipTx` (TX-7, TX-23): the chip already holds the TX frequency and tone; key it with the microphone, then
/// read back REG_30, the frequency (REG_38/39) and REG_3B, write stock's REG_36 for the level ({0x7F, 0x57,
/// 0x57}[level], `analysis/calibration.md`) and read it back too, all before the PA is driven. Stock reads nothing
/// back; this is Ferrite's (decision 97). Err names the first register that differs: the caller goes to the safe
/// state with the PA at zero.
#[cfg(feature = "tx")]
pub fn key_mic<B: Bus>(bk: &mut Bk4829<B>, high: bool, want: &TxExpect) -> Result<(), ReadBack> {
    bk.tx_on(bk4829::tx::Source::Mic, !want.narrow);
    if bk.read(0x30) != REG30_TX_MIC {
        return Err(ReadBack::Power);
    }
    let word = bk.read(0x38) as u32 | (bk.read(0x39) as u32) << 16;
    if word != want.freq {
        return Err(ReadBack::Freq);
    }
    if bk.read(0x3B) != want.reg3b {
        return Err(ReadBack::Xtal);
    }
    let bias = if high { 0x7F } else { 0x57 };
    bk.write(0x36, bias);
    if bk.read(0x36) != bias {
        return Err(ReadBack::Bias);
    }
    Ok(())
}

/// While keyed (TONE-13, DTMF-7, DTMF-11): the tone burst or a DTMF symbol on the keyed chip, as stock sends any
/// internal tone: the generator set, REG_30 to tone transmit (the microphone off) and the sidetone as asked; both
/// None goes back to the microphone with the key-up's AF route (REG_47 0x6142, F14). `narrow`: the keyed slot's
/// bandwidth, for stock's REG_30 step (F3). Returns (a tone is sending, the microphone read-back is good).
#[cfg(feature = "tx")]
pub fn keyed_tone<B: Bus>(bk: &mut Bk4829<B>, hz: Option<u16>, dtmf: Option<char>, heard: bool, narrow: bool) -> (bool, bool) {
    use bk4829::tx::Source;
    let sending = match (hz, dtmf) {
        (Some(hz), _) => {
            bk.tone_tx(hz as u32);
            true
        }
        (None, Some(d)) => bk.dtmf_tx(d),
        (None, None) => false,
    };
    if sending {
        bk.tone_deviation(true); // TONE-19: the tones' REG_40 while they play
        bk.tx_on(Source::Tones, !narrow);
        bk.sidetone(heard);
        return (true, true);
    }
    bk.tone_tx(0);
    bk.tone_deviation(false);
    bk.af_out(AfOut::Normal);
    bk.tx_on(Source::Mic, !narrow);
    (false, bk.read(0x30) == REG30_TX_MIC)
}

/// The generator's silent lead-in before a keyed tone sequence (TONE-10, TONE-19): the tone off, the tones' REG_40,
/// then tone transmit (REG_30 through stock's step, the microphone off). The first tone after entering tone transmit
/// swells (time constant about 80 ms, 90 % at about 190 ms; measured Oct 13, 2026), so the caller waits
/// [`ferrite_core::tonegen::TONE_LEAD_MS`] before the first step (decision 141).
#[cfg(feature = "tx")]
pub fn tones_begin<B: Bus>(bk: &mut Bk4829<B>, narrow: bool) {
    bk.tone_tx(0);
    bk.tone_deviation(true);
    bk.tx_on(bk4829::tx::Source::Tones, !narrow);
}

/// After a keyed tone sequence: the tone off and REG_40 back to the key-up's (stock's 0x50A). REG_30 stays at tone
/// transmit until the unkey's REG_30 = 0.
#[cfg(feature = "tx")]
pub fn tones_end<B: Bus>(bk: &mut Bk4829<B>) {
    bk.tone_tx(0);
    bk.tone_deviation(false);
}

/// The tail sent at the unkey (TONE-5, TONE-6, TONE-9): the 55 Hz tone at the calibration's CTCSS byte (`dev`,
/// `TxCal::tail_dev`; stock 0x0801D078, TX audit F7), or REG_52's phase shift (the DCS turn-off code on DCS).
#[cfg(feature = "tx")]
pub fn tail(tx_tone: u16, phase: u8, dev: u8) -> bk4829::tx::Tail {
    use bk4829::tx::Tail;
    match ferrite_core::tonegen::tail_kind(tx_tone, phase) {
        ferrite_core::tonegen::TailKind::Tone55 => Tail::Tone55 { dev },
        ferrite_core::tonegen::TailKind::Shift(p) => Tail::Shift { phase: p },
    }
}

/// TERM-33's `flat` or `data` test key-up (decision 132): the four registers as they were, for [`tx_test_restore`].
#[cfg(feature = "tx")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestRegs {
    pub r2b: u16,
    pub r19: u16,
    pub r7d: u16,
    pub r50: u16,
}

/// REG_2B[2:0]: the TX HPF300, LPF1 and pre-emphasis, each 1 = off (the register table).
#[cfg(feature = "tx")]
pub const TX_FILTERS_OFF: u16 = 0b111;
/// REG_2B[2:0] for `data`: the TX HPF300 off (bit 2), pre-emphasis and LPF on. Measured on the black radio
/// (Oct 13, 2026): the pre-emphasis cancels the DAC injection's 6 dB/octave fall, +-1 dB from 250 Hz to 2.5 kHz.
#[cfg(feature = "tx")]
pub const TX_HPF_OFF: u16 = 0b100;
/// REG_19[15]: MIC AGC, 1 = off (the register table; the BK4819 app note turns it off for DMR).
#[cfg(feature = "tx")]
pub const MIC_AGC_OFF: u16 = 1 << 15;
/// REG_7D[6]: the AF level controller (ALC), 1 = off. The BK4829's table has it here; the BK4819 app note's
/// REG_4B[5] is the older chip's place for it, and REG_4B is left alone.
#[cfg(feature = "tx")]
pub const ALC_OFF: u16 = 1 << 6;
/// REG_50[15]: AF TX mute. Kept clear: DAC1 reaches the modulator through the mic input, so this mute would silence
/// the test signal with the mic (V0.29 sets it only around its internal FSK burst).
#[cfg(feature = "tx")]
pub const TX_MUTE: u16 = 1 << 15;

/// TERM-33 `flat` or `data`, and every APRS burst's `data` (APRS-14, decision 135; never called for `voice`), after
/// [`tx_setup`] and before the key-up: REG_2B[2:0] to [`TX_FILTERS_OFF`] (`flat`) or exactly [`TX_HPF_OFF`] (`data`; the register's other bits kept), MIC AGC and the
/// ALC off and the TX mute clear, so DAC1's test signal reaches the modulator without the voice processing.
/// REG_7D[5:0] is the data key-up's [`ferrite_core::vox::MIC_DATA`] from [`tx_setup`]. Returns what was there.
#[cfg(feature = "tx")]
pub fn tx_test_filters<B: Bus>(bk: &mut Bk4829<B>, f: ferrite_core::txfilters::Filters) -> TestRegs {
    let was = TestRegs { r2b: bk.read(bk4829::reg::AF_FILTERS), r19: bk.read(0x19), r7d: bk.read(bk4829::reg::MIC), r50: bk.read(0x50) };
    let bits = if f == ferrite_core::txfilters::Filters::Flat { TX_FILTERS_OFF } else { TX_HPF_OFF };
    bk.write(bk4829::reg::AF_FILTERS, (was.r2b & !TX_FILTERS_OFF) | bits);
    bk.write(0x19, was.r19 | MIC_AGC_OFF);
    bk.write(bk4829::reg::MIC, was.r7d | ALC_OFF);
    if was.r50 & TX_MUTE != 0 {
        bk.write(0x50, was.r50 & !TX_MUTE);
    }
    was
}

/// At the unkey of a `flat` or `data` test: the four registers as [`tx_test_filters`] found them.
#[cfg(feature = "tx")]
pub fn tx_test_restore<B: Bus>(bk: &mut Bk4829<B>, r: &TestRegs) {
    bk.write(bk4829::reg::AF_FILTERS, r.r2b);
    bk.write(0x19, r.r19);
    bk.write(bk4829::reg::MIC, r.r7d);
    bk.write(0x50, r.r50);
}

/// REG_47<0>: bypass all AF TX filters (the register table; the BK4819 app note's "digital walkie-talkie" setting).
/// With it the 3.1 kHz wall of the `data` set-up is gone (bench, Oct 27, 2026; TERM-33.15).
#[cfg(all(feature = "tx", feature = "bench"))]
pub const TX_AF_BYPASS: u16 = 1;
/// REG_2F<7:5>: the TX soft limiter's factor, 000 = off (the register table; 100 at power-on, 0x9890). <13:8> is the
/// receive de-emphasis gain: kept.
#[cfg(all(feature = "tx", feature = "bench"))]
pub const TX_LIMITER: u16 = 0b111 << 5;

/// TERM-33.15, TERM-33.16: what `bypass` and `eq`/`eq8` changed on the keyed chip, for [`tx_test_extra_restore`].
#[cfg(all(feature = "tx", feature = "bench"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtraRegs {
    pub r47: u16,
    pub r2f: u16,
    pub r40: u16,
    pub r7d: u16,
}

/// TERM-33.15 and TERM-33.16, after [`tx_test_filters`] (the key-up's REG_47 0x6142 from [`tx_setup`], its REG_40
/// and REG_7D 0x34 in place) and before the key-up: `bypass` sets REG_47<0> and turns the soft limiter off
/// (REG_2F<7:5> = 000); an equaliser's level (the bypass's 6.8 dB loss, TERM-33.16) comes partly from REG_7D's
/// mic sensitivity (`mic_steps` 0.5 dB steps up, at most 0x3F) and REG_40's deviation field (`deviation`, the top
/// four bits kept). Returns what was there.
#[cfg(all(feature = "tx", feature = "bench"))]
pub fn tx_test_extra<B: Bus>(bk: &mut Bk4829<B>, bypass: bool, mic_steps: u8, deviation: Option<u16>) -> ExtraRegs {
    let was = ExtraRegs { r47: bk.read(bk4829::reg::AF_OUT), r2f: bk.read(0x2F), r40: bk.read(bk4829::reg::DEVIATION), r7d: bk.read(bk4829::reg::MIC) };
    if bypass {
        bk.write(bk4829::reg::AF_OUT, was.r47 | TX_AF_BYPASS);
        bk.write(0x2F, was.r2f & !TX_LIMITER);
    }
    if mic_steps > 0 {
        let g = ((was.r7d & 0x3F) + mic_steps as u16).min(0x3F);
        bk.write(bk4829::reg::MIC, (was.r7d & !0x3F) | g);
    }
    if let Some(d) = deviation.filter(|&d| d != was.r40 & 0x0FFF) {
        bk.write(bk4829::reg::DEVIATION, (was.r40 & 0xF000) | (d & 0x0FFF));
    }
    was
}

/// At the unkey, before [`tx_test_restore`]: the registers [`tx_test_extra`] found.
#[cfg(all(feature = "tx", feature = "bench"))]
pub fn tx_test_extra_restore<B: Bus>(bk: &mut Bk4829<B>, r: &ExtraRegs) {
    bk.write(bk4829::reg::AF_OUT, r.r47);
    bk.write(0x2F, r.r2f);
    bk.write(bk4829::reg::DEVIATION, r.r40);
    bk.write(bk4829::reg::MIC, r.r7d);
}

/// TERM-33 `afsk-bk`, on the keyed chip after the key-up's plan: AFSK from the tone generator, as stock sends any
/// internal tone while keyed (the end tone's path, [`bk4829::Bk4829::tone_tx`] after `tx_on(Tones)`): REG_30 to
/// tone transmit through stock's step (0x0200 wide or 0 narrow, about 50 us, then 0xC3FA: the mic ADC off, so the
/// mic adds nothing), then tone 1 at `hz` (REG_71), tone 2 off (REG_72 0) and REG_70 tone 1 on at `gain` (bits
/// 14:8; tone 2 off). BD1EEA's `BK4819_AFSK_BeginTx` writes the same REG_70/71 and the same REG_30 value
/// (`BK4819_EnableTXLink`: VCO cal, AF DAC, disc mode, PLL, PA gain, TX DSP = 0xC3FA); its REG_50 TX mute around
/// the switch (0xBB18/0x3B18) is left out, as stock's tone path leaves it. REG_47 is left at the key-up's 0x6142
/// (no sidetone: the speaker is off for a data key-up).
#[cfg(all(feature = "tx", feature = "bench"))]
pub fn afsk_bk_begin<B: Bus>(bk: &mut Bk4829<B>, hz: u16, gain: u8, narrow: bool) {
    bk.tx_on(bk4829::tx::Source::Tones, !narrow);
    bk.tones(hz as u32, gain, 0, 0);
}

/// `afsk-bk`'s tone change: REG_71 only (BD1EEA's `BK4819_AFSK_SetTone`), one bus write.
#[cfg(all(feature = "tx", feature = "bench"))]
pub fn afsk_bk_tone<B: Bus>(bk: &mut Bk4829<B>, hz: u16) {
    bk.write(bk4829::reg::TONE1_FREQ, bk4829::tone_word(hz as u32));
}

/// `afsk-bk` stopped at the release, before the unkey's steps: tones off (REG_70 0), as the end tone leaves the
/// chip at the unkey (`play_keyed`); REG_30 stays at tone transmit (mic off) until the unkey's REG_30 = 0.
#[cfg(all(feature = "tx", feature = "bench"))]
pub fn afsk_bk_end<B: Bus>(bk: &mut Bk4829<B>) {
    bk.tone_tx(0);
}

/// Registers saved by [`rad950_bk_prime`] and restored at unkey by [`rad950_bk_end`].
#[cfg(feature = "tx")]
#[derive(Clone, Copy)]
pub struct Rad950Saved {
    pub regs: [(u8, u16); 9],
    pub base_hz: u32,
    pub narrow: bool,
}

/// Start RAD950 carrier-stepping on the keyed BK4829: saves modified registers and initial frequency
/// `base_hz`, enters `Source::Tones` (`REG_30 = 0xC3FA`, mic off, Tone 1 off).
#[cfg(feature = "tx")]
pub fn rad950_bk_prime<B: Bus>(bk: &mut Bk4829<B>, _dev: u16, _g0: u8, narrow: bool) -> Rad950Saved {
    let regs = [0x38u8, 0x39, 0x70, 0x71, 0x47, 0x7E, 0x29, 0x2B, 0x40].map(|r| (r, bk.read(r)));
    let base_hz = bk.freq();
    bk.tx_on(bk4829::tx::Source::Tones, !narrow);
    bk.write(0x70, 0);
    Rad950Saved { regs, base_hz, narrow }
}

/// Set RAD950 frequency step (`hz` absolute), latching via VCO bit-15 toggle (`bit15_only = true`:
/// `0x43FA -> 0xC3FA`) or full `tx_on` (`0x0200 -> 50 us -> 0xC3FA`).
#[cfg(feature = "tx")]
pub fn rad950_bk_freq<B: Bus>(bk: &mut Bk4829<B>, hz: u32, bit15_only: bool, narrow: bool) {
    bk.set_freq(hz);
    if bit15_only {
        bk.write(bk4829::reg::POWER, 0x43FA);
        bk.write(bk4829::reg::POWER, 0xC3FA);
    } else {
        bk.tx_on(bk4829::tx::Source::Tones, !narrow);
    }
}

/// Restore all registers saved by [`rad950_bk_prime`] before unkey completes.
#[cfg(feature = "tx")]
pub fn rad950_bk_end<B: Bus>(bk: &mut Bk4829<B>, saved: &Rad950Saved) {
    for &(r, v) in &saved.regs {
        bk.write(r, v);
    }
}

/// TERM-34 `txtest suite` (decision 139): what the suite changes on the keyed chip, as the key-up left it: the four
/// registers of the `flat`/`data` set-up, REG_40 (the deviation field), REG_07 and REG_52 (the sub-tone word and
/// the CTCSS threshold the sub-tone steps rewrite) and the key-up's sub-tone (the slot's tone word and its REG_51
/// gain, [`TxSetup`]). REG_08 (DCS's codeword, written in two halves) isn't kept: the key-up's DCS code is written
/// again, and with no DCS the next receive tune sets it.
#[cfg(all(feature = "tx", feature = "bench"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SuiteRegs {
    pub filters: TestRegs,
    pub r40: u16,
    pub r07: u16,
    pub r52: u16,
    pub tone: u16,
    pub tone_dev: u8,
}

/// The suite's start, on the keyed chip after the key-up's plan: what it will change read and kept, and the slot's
/// sub-tone off (REG_51 0), so only section 6 sends one. `tone`, `tone_dev`: the key-up's [`TxSetup`] values.
#[cfg(all(feature = "tx", feature = "bench"))]
pub fn suite_begin<B: Bus>(bk: &mut Bk4829<B>, tone: u16, tone_dev: u8) -> SuiteRegs {
    let filters = TestRegs { r2b: bk.read(bk4829::reg::AF_FILTERS), r19: bk.read(0x19), r7d: bk.read(bk4829::reg::MIC), r50: bk.read(0x50) };
    let r = SuiteRegs { filters, r40: bk.read(bk4829::reg::DEVIATION), r07: bk.read(bk4829::reg::CTC), r52: bk.read(bk4829::reg::CTCSS_THRESH), tone, tone_dev };
    bk.subtone_off();
    r
}

/// The TX audio set-up `f` from the key-up's registers: `voice` the four as they were, `flat` and `data` as
/// [`tx_test_filters`] makes them from those (the same values whatever the step before set).
#[cfg(all(feature = "tx", feature = "bench"))]
fn suite_filters<B: Bus>(bk: &mut Bk4829<B>, was: &TestRegs, f: ferrite_core::txtest::Filters) {
    use ferrite_core::txtest::Filters;
    if f == Filters::Voice {
        tx_test_restore(bk, was);
        return;
    }
    let bits = if f == Filters::Flat { TX_FILTERS_OFF } else { TX_HPF_OFF };
    bk.write(bk4829::reg::AF_FILTERS, (was.r2b & !TX_FILTERS_OFF) | bits);
    bk.write(0x19, was.r19 | MIC_AGC_OFF);
    bk.write(bk4829::reg::MIC, was.r7d | ALC_OFF);
    bk.write(0x50, was.r50 & !TX_MUTE);
}

/// One step of the suite on the chip, from the step before (None: the first, the chip as the key-up left it, on
/// the mic in the voice set-up at the key-up's REG_40): only what differs is written, in this order: the tone
/// generator off when its sound ends (REG_70 0), a sub-tone off when it ends, the TX audio set-up, REG_40's low 12
/// bits (the top four as the key-up found them), the chip between the mic and tone transmit (stock's step,
/// [`Bk4829::tx_on`]), then the new sound: a generator tone at stock's gain, the flags' first tone (space at
/// `afsk-bk`'s gain; the caller starts TMR5), or the sub-tone with `sub_dev` as its REG_51 gain (the calibration's
/// byte for its kind). DAC1 and TMR5 are the caller's. `narrow`: the keyed slot's bandwidth.
#[cfg(all(feature = "tx", feature = "bench"))]
pub fn suite_step<B: Bus>(bk: &mut Bk4829<B>, was: &SuiteRegs, prev: Option<&ferrite_core::txsuite::Step>, next: &ferrite_core::txsuite::Step, narrow: bool, sub_dev: u8) {
    use ferrite_core::txsuite::Src;
    use ferrite_core::txtest::{bk_gain, Filters, BK_SPACE_HZ, LEVEL_DEFAULT};
    let (p_src, p_filters, p_reg40) = match prev {
        Some(p) => (p.src, p.filters, p.reg40),
        None => (Src::Dac(ferrite_core::txsuite::Dac::Silence), Filters::Voice, was.r40 & 0x0FFF),
    };
    let sounding = |s: Src| matches!(s, Src::BkFlags | Src::BkTone(_));
    if sounding(p_src) && !sounding(next.src) || p_src == Src::BkFlags && next.src != Src::BkFlags {
        bk.tone_tx(0);
    }
    let sub = next.src.subtone();
    if p_src.subtone() != 0 && p_src.subtone() != sub {
        bk.subtone_off();
    }
    if p_filters != next.filters {
        suite_filters(bk, &was.filters, next.filters);
    }
    if p_reg40 != next.reg40 & 0x0FFF {
        bk.write(bk4829::reg::DEVIATION, (was.r40 & 0xF000) | (next.reg40 & 0x0FFF));
    }
    if p_src.tones() != next.src.tones() {
        bk.tx_on(if next.src.tones() { bk4829::tx::Source::Tones } else { bk4829::tx::Source::Mic }, !narrow);
    }
    match next.src {
        Src::BkTone(hz) if p_src != next.src => bk.tone_tx(hz as u32),
        Src::BkFlags => bk.tones(BK_SPACE_HZ as u32, bk_gain(LEVEL_DEFAULT), 0, 0),
        Src::Sub(t) if t != p_src.subtone() => {
            if t & 0x8000 != 0 {
                bk.dcs_tx(t & 0x3FFF, t & 0x4000 != 0);
            } else {
                bk.ctcss_tx(t);
            }
            bk.subtone_gain(sub_dev);
        }
        _ => {}
    }
}

/// The suite's end (the release, or any unkey), before the unkey's steps: the generator off (REG_70 0), the TX
/// audio set-up, REG_40, REG_07 and REG_52 as the key-up left them, and the key-up's sub-tone again (REG_51 and
/// REG_08 as [`tx_setup`] wrote them). REG_30 is left for the unkey's REG_30 = 0.
#[cfg(all(feature = "tx", feature = "bench"))]
pub fn suite_end<B: Bus>(bk: &mut Bk4829<B>, was: &SuiteRegs) {
    bk.tone_tx(0);
    tx_test_restore(bk, &was.filters);
    bk.write(bk4829::reg::DEVIATION, was.r40);
    match was.tone {
        0 => bk.subtone_off(),
        t if t & 0x8000 != 0 => bk.dcs_tx(t & 0x3FFF, t & 0x4000 != 0),
        t => bk.ctcss_tx(t),
    }
    if was.tone != 0 {
        bk.subtone_gain(was.tone_dev);
    }
    bk.write(bk4829::reg::CTC, was.r07);
    bk.write(bk4829::reg::CTCSS_THRESH, was.r52);
}

/// TERM-42 (decision 305, a bench test): `bk rxaf`'s receive AF filter bits, REG_2B 10:8 (1 = off: 10 the 300 Hz
/// high-pass, 9 the 3 kHz low-pass, 8 de-emphasis), written over [`tune_rest`]'s FM REG_2B (0x0000) with the
/// register's other bits kept. The Radio task calls it after every FM tune while the override is set.
#[cfg(feature = "bench")]
pub fn rx_af_override<B: Bus>(bk: &mut Bk4829<B>, bits: u16) {
    let r = bk.read(bk4829::reg::AF_FILTERS);
    bk.write(bk4829::reg::AF_FILTERS, (r & !0x0700) | (bits & 0x0700));
}

/// TERM-43 (decision 305, a bench test): the modem set-up for an `fsk` run (`ferrite_core::fskbench`): REG_58 and the
/// rate word, tone 2's gain, REG_5C, all from [`ferrite_core::fskbench::Speed`]; [`ferrite_core::fskbench::PREAMBLE`]
/// bytes of preamble, the 4-byte sync word, [`ferrite_core::fskbench::LEN`] data bytes, no scramble.
#[cfg(feature = "bench")]
pub fn fsk_bench(c: &ferrite_core::fskbench::Cfg) -> Fsk {
    use ferrite_core::fskbench as f;
    Fsk {
        mode: c.speed.reg58(),
        tone_word: c.speed.rate_word(),
        tone_gain: c.speed.tone_gain(),
        preamble: f::PREAMBLE,
        sync: f::SYNC,
        sync4: true,
        crc: c.speed.reg5c(c.crc),
        scramble: false,
        len: f::LEN as u16,
    }
}

/// TERM-43: the modem receiving on a tuned chip (set up, FIFO cleared, receive on, its interrupts enabled); again
/// after each frame, `fsk_bench_rearm`.
#[cfg(feature = "bench")]
pub fn fsk_bench_rx<B: Bus>(bk: &mut Bk4829<B>, c: &ferrite_core::fskbench::Cfg) {
    let f = fsk_bench(c);
    bk.fsk_setup(&f);
    bk.fsk_rx(&f);
}

/// TERM-43: listening for the next frame: FIFO cleared and receive on again (REG_59), as stock after each packet.
#[cfg(feature = "bench")]
pub fn fsk_bench_rearm<B: Bus>(bk: &mut Bk4829<B>, c: &ferrite_core::fskbench::Cfg) {
    bk.fsk_rx(&fsk_bench(c));
}

/// TERM-43 `fsk tx`, on the keyed chip after the key-up's plan: tone transmit through stock's step (REG_30 0x0200
/// wide or 0 narrow, then 0xC3FA, the mic off), the modem set up, then frame `n` loaded and sent.
#[cfg(all(feature = "tx", feature = "bench"))]
pub fn fsk_bench_tx_begin<B: Bus>(bk: &mut Bk4829<B>, c: &ferrite_core::fskbench::Cfg, narrow: bool, n: u16) {
    bk.tx_on(bk4829::tx::Source::Tones, !narrow);
    bk.fsk_setup(&fsk_bench(c));
    fsk_bench_tx_frame(bk, c, n);
}

/// TERM-43: frame `n` into the FIFO and sent (the FIFO cleared first, the done interrupt enabled).
#[cfg(all(feature = "tx", feature = "bench"))]
pub fn fsk_bench_tx_frame<B: Bus>(bk: &mut Bk4829<B>, c: &ferrite_core::fskbench::Cfg, n: u16) {
    use ferrite_core::fskbench::{payload, words};
    bk.fsk_send(&fsk_bench(c), &words(&payload(n)));
}

/// TERM-43: the frames done or the key-up ending: transmit off in the modem, its interrupts off, then the modem
/// off (REG_70 0, REG_58 0, as stock after its FSK). REG_30 stays at tone transmit for the unkey.
#[cfg(all(feature = "tx", feature = "bench"))]
pub fn fsk_bench_tx_end<B: Bus>(bk: &mut Bk4829<B>, c: &ferrite_core::fskbench::Cfg) {
    bk.fsk_tx_end(&fsk_bench(c));
    bk.fsk_off();
}
