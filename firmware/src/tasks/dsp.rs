//! DSP: audio from ADC1 (PA2, the BK4829s' EARO) through the 4:1 step into the one decoder slot, and AFSK out
//! of DAC1 (APRS-1, APRS-14; roadmap 2f, 5b, 5c). Runs on its own interrupt executor, above the Radio task's (decision 357).
//!
//! - **Receive:** while a decoder runs, the capture runs ([`hal::audio`]); each 8 ms half goes through
//!   [`Decim`] to the slot. An AFSK frame with a good CRC goes to the UI task's APRS task through [`APRS_RX`], a
//!   page to the UI task through [`PAGES`].
//! - **One slot, two wanting it (DEC-18):** the APRS task asks for AFSK, the UI task for the pager decoder; APRS
//!   comes first (APRS mode gives it slot C), the pagers get the slot whenever APRS doesn't want it. Background
//!   jobs on watched slots (roadmap 6f) will share it more finely.
//! - **Transmit:** the UI task keys slot C, then hands a frame to [`AFSK_TX`]; it is played from TX delay's
//!   flags to the end (the flags cover the key-up), PC12 high meanwhile, and [`AFSK_DONE`] says it's out so the
//!   UI task unkeys. Receiving pauses while it plays.
//! - **Transmit test (TERM-33, TERM-34, the tx-bench build; TERM-38):** the Radio task, after a PTT key-up with `txtest` armed,
//!   signals [`TXTEST`] with the generator to play (a test signal at the arming's level, made a sample at a time:
//!   [`ferrite_core::txtest::Gen`]); a new generator while one plays takes over at the next half (the suite's
//!   steps, DAC1 running throughout); None at the release stops it. Receiving pauses while it plays, as for AFSK.

use crate::chan::DSP;
use crate::hal::{adc, audio, clock};
use core::sync::atomic::{AtomicU32, Ordering::Relaxed};
use embassy_futures::select::{select4, Either4};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex as M;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use ferrite_app::aprs_live::FRAME_MAX;
use ferrite_core::audio::{afsk_tx_at, to_dac, tx_bits, uafsk, Decim, TxBits, BLOCK_DAC, BLOCK_IN, BLOCK_OUT};
use ferrite_core::decoders::{Decoder, Found, Page};
use ferrite_core::msg::{DecoderKind, DspCmd};

/// An AX.25 frame without its FCS, as the demodulator gives it and the modulator takes it.
#[derive(Clone, Copy)]
pub struct Frame {
    pub len: u16,
    /// Transmit: the flags ahead of it, in 10 ms (KISS's TX delay; 30 = 300 ms, KISS-2's default).
    pub txdelay_10ms: u8,
    /// Transmit: FX.25 check bytes (16, 32, 64), or 0 for plain AX.25 (APRS-45).
    pub fx25: u8,
    /// Transmit: the AFSK level (APRS-49: 0 Low, 1 Mid, 2 High).
    pub level: u8,
    pub b: [u8; FRAME_MAX],
}

impl Frame {
    pub fn bytes(&self) -> &[u8] {
        &self.b[..self.len as usize]
    }
}

/// What the decoder heard, for the APRS task: a frame, or an FX.25 block for it to correct (APRS-44: 0.27 to 1.03
/// M instructions, 3 to 13 ms at 120 MHz, too long for this task's 8 ms halves).
pub enum Heard {
    Frame(Frame),
    Fx25(uafsk::Fx25Block),
    /// APRS-56: a packet heard but not decoded (bad FCS on every slicer) and the audio's peak (0-32767) then, for
    /// the decode log.
    Miss(uafsk::Miss, u16),
}

/// Frames heard, to the UI task's APRS task: two deep (a second waits while the first is read; 672 B).
pub static APRS_RX: Channel<M, Heard, 2> = Channel::new();
/// A frame to send on the transmission the UI task just keyed, and its "sent" (true) or "not sent" (false).
pub static AFSK_TX: Signal<M, Frame> = Signal::new();
pub static AFSK_DONE: Signal<M, bool> = Signal::new();
/// TERM-33, TERM-34: the transmit test's generator to play now (Some, from the key-up; another Some takes over
/// seamlessly) or to stop (None, at the release), and the equaliser its samples go through (TERM-33.16).
#[cfg(all(feature = "tx", feature = "bench"))]
pub static TXTEST: Signal<M, Option<(ferrite_core::txtest::Gen, ferrite_core::txeq::Eq)>> = Signal::new();
/// One capture half, ms (the blocks [`audio::ADC_HALF`] marks: 8 ms, decision 358).
const HALF_MS: u32 = BLOCK_IN as u32 * 1_000 / ferrite_core::audio::ADC_HZ;
/// Pages heard, to the UI task (DEC-7): two deep, 144 B.
pub static PAGES: Channel<M, Page, 2> = Channel::new();
/// LINK-13.4: blocks made; dropped (the queue below full: the cable's ring had no room for 128 ms); and the times
/// a block waited (the ring full, or a thread-mode writer mid-frame). The readback's `audio` line.
pub static AUDIO_MADE: AtomicU32 = AtomicU32::new(0);
pub static AUDIO_DROPS: AtomicU32 = AtomicU32::new(0);
pub static AUDIO_WAITS: AtomicU32 = AtomicU32::new(0);
/// TERM-32.1 (decision 294): raw PA2 counts at a rail since power-on, the loudest sample of the last second (after
/// the DC is taken off, full scale 32767), and the stream's gain (Q8, LINK-13.8). The readback's `audio` line.
pub static AUDIO_CLIPS: AtomicU32 = AtomicU32::new(0);
pub static AUDIO_PEAK: AtomicU32 = AtomicU32::new(0);
pub static AUDIO_GAIN: AtomicU32 = AtomicU32::new(ferrite_core::rxlevel::UNITY as u32);
/// PWR-26.4: how often (in capture halves: 10 s) a capture running at 24 MHz asks again for the build's speed.
const RECHECK_HALVES: u32 = 10_000 / HALF_MS;
/// The peak's window, in capture halves: one second.
const PEAK_HALVES: u32 = 1_000 / HALF_MS;
/// The miss log's level buckets (`lvl`), in capture halves: 64 ms each.
const LVL_HALVES: u32 = 64 / HALF_MS;
/// Decision 290: blocks waiting for room in `TERM_OUT`, at most this many (128 ms of 16 ms blocks, decision 330),
/// each with what it was heard from.
const AUDIO_QUEUE: usize = 8;
/// A keep-alive this many blocks apart while nothing is heard (about 1 s, LINK-13.4).
const KEEPALIVE_BLOCKS: u8 = (ferrite_core::rxaudio::RATE_HZ as usize / ferrite_core::rxaudio::BLOCK) as u8;

/// Decision 290: the DSP task's own sender of receive audio. Blocks are framed and put straight into the cable's
/// output ring (its transmit interrupt sends them), never waiting for the UI task, whose redraws took longer than
/// the old six-block hand-off held. A block waits here only while the ring is full or a thread-mode writer is
/// mid-frame ([`crate::chan::TERM_OUT_LOCK`]).
struct AudioOut {
    q: [(ferrite_core::rxaudio::Block, u16); AUDIO_QUEUE],
    head: u8,
    n: u8,
    seq: u16,
    quiet: u8,
}

impl AudioOut {
    fn new() -> Self {
        AudioOut { q: [([0; ferrite_core::rxaudio::BLOCK], 0); AUDIO_QUEUE], head: 0, n: 0, seq: 0, quiet: 0 }
    }

    /// One whole frame into `TERM_OUT`, or nothing (`false`).
    fn put(&mut self, src: u16, blk: Option<&ferrite_core::rxaudio::Block>) -> bool {
        let (slot, flags) = ferrite_core::rxaudio::unpack_source(src);
        let mut w = [0u8; ferrite_core::link::MAX_WIRE];
        let Some(k) = ferrite_core::rxaudio::frame(self.seq, slot, flags, blk, &mut w) else { return false };
        if !to_cable(&w[..k]) {
            return false;
        }
        self.seq = self.seq.wrapping_add(1);
        true
    }

    /// A block made: queued if something is heard (the oldest dropped when full), a keep-alive about once a second
    /// otherwise; then as many queued as fit go out.
    fn block(&mut self, b: &ferrite_core::rxaudio::Block) {
        let src = crate::chan::AUDIO_SRC.load(Relaxed);
        if ferrite_core::rxaudio::unpack_source(src).1 & ferrite_core::rxaudio::SQ_OPEN != 0 {
            if self.n as usize == AUDIO_QUEUE {
                AUDIO_DROPS.fetch_add(1, Relaxed); // the oldest goes: the PC hears a gap
                self.head = (self.head + 1) % AUDIO_QUEUE as u8;
                self.n -= 1;
            }
            self.q[(self.head + self.n) as usize % AUDIO_QUEUE] = (*b, src);
            self.n += 1;
            self.quiet = 0;
        } else {
            self.quiet = self.quiet.saturating_add(1);
            if self.quiet >= KEEPALIVE_BLOCKS && self.n == 0 && self.put(src, None) {
                self.quiet = 0;
            }
        }
        self.drain(true);
    }

    /// As many queued as fit go out. `fresh`: a block was just made (a block still left then counts as waiting);
    /// else the capture half's retry (decision 331), which keeps the ring topped up between blocks.
    fn drain(&mut self, fresh: bool) {
        while self.n > 0 {
            let (b, src) = self.q[self.head as usize];
            if !self.put(src, Some(&b)) {
                if fresh {
                    AUDIO_WAITS.fetch_add(1, Relaxed);
                }
                return;
            }
            self.head = (self.head + 1) % AUDIO_QUEUE as u8;
            self.n -= 1;
        }
    }
}
/// A whole link frame into the cable's send ring (decision 290), or nothing (`false`): not while the bring-up log has
/// the cable, a thread-mode writer is mid-frame, or the ring lacks room. Decision 331: room for the frame and one
/// more of the session's ([`ferrite_core::link::MAX_WIRE`]) stays free, so a session that found room for its answer
/// before working it out still finds it after, whatever audio went in meanwhile.
fn to_cable(w: &[u8]) -> bool {
    use crate::chan::{TERM_OUT, TERM_OUT_LOCK};
    if crate::bringup::ON.load(Relaxed)
        || TERM_OUT_LOCK.load(core::sync::atomic::Ordering::SeqCst)
        || (TERM_OUT.room() as usize) < w.len() + ferrite_core::link::MAX_WIRE
    {
        return false;
    }
    for &b in w {
        TERM_OUT.put(b);
    }
    crate::hal::uart::tx_ring(crate::report::UART_CPS, &TERM_OUT);
    true
}

/// LINK-13.9 (decision 307): blocks of the bench's raw stream waiting for room in `TERM_OUT`, at most this many
/// (38 ms at 12 kHz, 28 ms at 16 kHz; the ring itself holds four more).
#[cfg(feature = "bench")]
const RAW_QUEUE: usize = 3;

/// LINK-13.9: the bench's raw receive audio, framed and sent by this task as the listening stream is: every block,
/// whatever the squelch; the oldest dropped when the queue is full (the PC sees the gap in the sample index).
#[cfg(feature = "bench")]
struct RawOut {
    pack: ferrite_core::rawaudio::Packer,
    q: [(ferrite_core::rawaudio::Block, u16); RAW_QUEUE],
    head: u8,
    n: u8,
    seq: u16,
}

#[cfg(feature = "bench")]
impl RawOut {
    fn new(mode: u8) -> Self {
        let p = ferrite_core::rawaudio::Packer::new(mode);
        let blank = ferrite_core::rawaudio::Block { first: 0, clipped: false, bytes: [0; ferrite_core::rawaudio::BLOCK_BYTES] };
        RawOut { pack: p, q: [(blank, 0); RAW_QUEUE], head: 0, n: 0, seq: 0 }
    }

    /// A chunk of decoder samples in; the blocks made queued, and as many as fit sent.
    fn chunk(&mut self, x: &[i16]) {
        let (q, head, n) = (&mut self.q, &mut self.head, &mut self.n);
        for &s in x {
            self.pack.push(s, |b| {
                AUDIO_MADE.fetch_add(1, Relaxed);
                if *n as usize == RAW_QUEUE {
                    AUDIO_DROPS.fetch_add(1, Relaxed);
                    *head = (*head + 1) % RAW_QUEUE as u8;
                    *n -= 1;
                }
                q[(*head + *n) as usize % RAW_QUEUE] = (*b, crate::chan::AUDIO_SRC.load(Relaxed));
                *n += 1;
            });
        }
        while self.n > 0 {
            let (b, src) = &self.q[self.head as usize];
            let (slot, flags) = ferrite_core::rxaudio::unpack_source(*src);
            let mut w = [0u8; ferrite_core::link::MAX_WIRE];
            let Some(k) = ferrite_core::rawaudio::frame(self.seq, slot, flags, b, &mut w) else { return };
            if !to_cable(&w[..k]) {
                AUDIO_WAITS.fetch_add(1, Relaxed);
                return;
            }
            self.seq = self.seq.wrapping_add(1);
            self.head = (self.head + 1) % RAW_QUEUE as u8;
            self.n -= 1;
        }
    }
}

/// LINK-13.4, LINK-13.9: the stream to the PC. One place in the task's future for any: the raw stream's coder and
/// queue sit where the listening stream's do. Boxing a variant isn't an option here: the room is the task's static
/// future, held whichever runs.
#[allow(clippy::large_enum_variant)]
enum Stream {
    Off,
    Listen { pack: ferrite_core::rxaudio::Packer, sender: AudioOut, agc: ferrite_core::rxlevel::Agc },
    #[cfg(feature = "bench")]
    Raw(RawOut),
}

impl Stream {
    fn new(mode: u8) -> Self {
        match mode {
            0 => Stream::Off,
            #[cfg(feature = "bench")]
            m if ferrite_core::rawaudio::rate(m).is_some() => Stream::Raw(RawOut::new(m)),
            _ => Stream::Listen { pack: ferrite_core::rxaudio::Packer::new(), sender: AudioOut::new(), agc: ferrite_core::rxlevel::Agc::new() },
        }
    }
}

/// TERM-44 (decision 315): the receive level meter's last report, for the Radio task's answer.
#[cfg(feature = "bench")]
static METER: embassy_sync::blocking_mutex::Mutex<M, core::cell::Cell<ferrite_core::rxmeter::Report>> =
    embassy_sync::blocking_mutex::Mutex::new(core::cell::Cell::new(ferrite_core::rxmeter::Report::OFF));

/// TERM-44: `meter on` zeroes the report, `meter off` keeps its bins.
#[cfg(feature = "bench")]
pub fn meter_reset(on: bool) {
    METER.lock(|m| {
        let r = m.get();
        m.set(if on { ferrite_core::rxmeter::FRESH.report } else { ferrite_core::rxmeter::Report { on: false, ..r } });
    });
}

/// TERM-44: the meter's last report.
#[cfg(feature = "bench")]
pub fn meter_report() -> ferrite_core::rxmeter::Report {
    METER.lock(|m| m.get())
}

/// Decision 316: the decimator for the listening mode: the data path (flat to 8 kHz) under the bench's raw stream
/// (LINK-13.9), the decoders' own otherwise. The decoders take the same samples.
fn decim(listen: u8) -> Decim {
    #[cfg(feature = "bench")]
    if ferrite_core::rawaudio::rate(listen).is_some() {
        return Decim::wide();
    }
    let _ = listen;
    Decim::new()
}

/// Frames decoded and dropped (the UI hadn't taken the last two). APRS-56: with the frames that failed their FCS
/// (an AX.25 address field: packets missed; none: noise between chance flags) and the FX.25 blocks the APRS task
/// couldn't correct, counted since power-on for the readback's `decode` line.
pub static FRAMES: AtomicU32 = AtomicU32::new(0);
pub static DROPPED: AtomicU32 = AtomicU32::new(0);
pub static MISSED: AtomicU32 = AtomicU32::new(0);
pub static NOISE: AtomicU32 = AtomicU32::new(0);
pub static FX25_FAILED: AtomicU32 = AtomicU32::new(0);
/// PWR-26.9.1: the last and the longest time from the APRS slot's squelch opening to the capture running, ms.
pub static CAPTURE_MS: AtomicU32 = AtomicU32::new(0);
pub static CAPTURE_MAX_MS: AtomicU32 = AtomicU32::new(0);
/// APRS-56.4: the receive path's later stages, counted since power-on: the AFSK capture starting on the APRS
/// squelch, and the demodulator hearing 1200-baud data (each start of its carrier detect). With the squelch's
/// opens (SQL-19.4), a packet lost shows where: no open, no capture, nothing heard, or heard but not framed.
pub static CAPTURES: AtomicU32 = AtomicU32::new(0);
pub static HEARD: AtomicU32 = AtomicU32::new(0);
/// APRS-56.4: the longest a capture half waited for the DSP task after its interrupt, and the longest the task
/// spent on one, us since power-on: a half is 8 ms, so their sum over that loses audio (`late`).
#[cfg(feature = "bench")]
pub static WAKE_MAX_US: AtomicU32 = AtomicU32::new(0);
/// APRS-56.4, the last capture's timeline: the APRS squelch's lag (its first open reading to the slot opening, ms:
/// the Radio task's), the Radio task's tick period then (10 or 50 ms: a reading can be that late), and the time
/// from the capture starting to the demodulator first hearing data (ms; 0 when it already heard data in the
/// first half: the capture began inside the preamble or later).
pub static SQ_LAG_MS: AtomicU32 = AtomicU32::new(0);
pub static TICK_PERIOD_MS: AtomicU32 = AtomicU32::new(0);
pub static FIRST_DATA_MS: AtomicU32 = AtomicU32::new(0);

/// Decoder samples through the slot's decoder: what it finds to the APRS task, the pager's to the UI task, and the
/// demodulator's carrier detect counted (APRS-56.4). `level`: the miss log's peak; `since_cap`: decoder samples since
/// the capture began, until data is first heard. Out of line: the live path, the backlog and the drain share it.
#[inline(never)]
fn feed(slot: &mut Decoder, x: &[i16], level: u16, heard: &mut bool, since_cap: &mut Option<u32>) {
    slot.push(x, |f| match f {
        Found::Frame(b) => {
            FRAMES.fetch_add(1, Relaxed);
            let mut fr = Frame { len: b.len().min(FRAME_MAX) as u16, txdelay_10ms: 0, fx25: 0, level: 1, b: [0; FRAME_MAX] };
            fr.b[..fr.len as usize].copy_from_slice(&b[..fr.len as usize]);
            if APRS_RX.try_send(Heard::Frame(fr)).is_err() {
                DROPPED.fetch_add(1, Relaxed);
            }
        }
        Found::Fx25(b) => {
            if APRS_RX.try_send(Heard::Fx25(*b)).is_err() {
                DROPPED.fetch_add(1, Relaxed);
            }
        }
        Found::Miss(m) => {
            // APRS-56.1: an AX.25 address field, a packet missed; none, a packet whose start was lost (or chance
            // flags in noise): counted apart, both logged (0 5 9 8; rare, Alex, Nov 4)
            if m.addresses { &MISSED } else { &NOISE }.fetch_add(1, Relaxed);
            let _ = APRS_RX.try_send(Heard::Miss(*m, level)); // only for the log: a full queue skips it
        }
        Found::Page(m) => {
            if PAGES.try_send(Page::from(m)).is_err() {
                DROPPED.fetch_add(1, Relaxed);
            }
        }
        Found::Call(_) => {} // DSC: roadmap 6c
    });
    let at = since_cap.map(|s| s * 1_000 / ferrite_core::decoders::RATE_HZ);
    if let Some(s) = since_cap {
        *s += x.len() as u32;
    }
    let busy = slot.busy();
    if busy && !*heard {
        HEARD.fetch_add(1, Relaxed); // APRS-56.4
        if let Some(ms) = at {
            FIRST_DATA_MS.store(ms, Relaxed); // the start of the chunk it was heard in
            *since_cap = None;
        }
    }
    *heard = busy;
}
#[cfg(feature = "bench")]
pub static WORK_MAX_US: AtomicU32 = AtomicU32::new(0);


#[embassy_executor::task]
pub async fn task() {
    // The slot lives in this task's future, which is a static: one decoder's memory, whichever is running.
    let mut slot = Decoder::off();
    let mut dec = Decim::new();
    let mut capturing = false;
    // What each side asks for, and what runs (DEC-18).
    let (mut aprs, mut pager, mut running) = (DecoderKind::Off, false, DecoderKind::Off);
    // LINK-13.4: receive audio for the PC, coded from the same samples the decoder gets (the `AUDIO` mode: 0 none, 1
    // µ-law, 2 or 3 the bench's raw stream, LINK-13.9). LINK-13.8: the listening stream's gain is its own, ahead of its
    // coder only (the decoders take the samples as they are).
    let mut listen = 0u8;
    let mut stream = Stream::Off;
    let mut peak = 0i32;
    // APRS-56: the audio's peak over the last 64-128 ms (two 32-half buckets), a missed packet's level
    let mut lvl = (0u16, 0u16);
    let mut halves = 0u32;
    // TERM-44 (decision 315): the bench's receive level meter, on ADC1's 48 kHz samples ahead of `dec`
    let mut meter_on = false;
    #[cfg(feature = "bench")]
    let mut meter: Option<ferrite_core::rxmeter::Meter> = None;
    // PWR-26.9: the APRS slot's squelch (the Radio task's), and the halves left of the capture's tail once it shut
    let (mut sq, mut tail) = (false, 0u32);
    let mut sq_at: Option<embassy_time::Instant> = None;
    let mut heard = false; // APRS-56.4: the demodulator's carrier detect at the last block
    let mut since_cap: Option<u32> = None; // APRS-56.4: decoder samples since the capture began, until data is heard
    loop {
        // TERM-33's signal only in a `tx` bench build: the others never wait on it, nor carry its generators.
        #[cfg(all(feature = "tx", feature = "bench"))]
        let test = TXTEST.wait();
        #[cfg(not(all(feature = "tx", feature = "bench")))]
        let test = core::future::pending::<Option<core::convert::Infallible>>();
        match select4(DSP.receive(), audio::ADC_HALF.wait(), AFSK_TX.wait(), test).await {
            Either4::First(cmd @ (DspCmd::Decoder(_) | DspCmd::Pager(_) | DspCmd::Audio(_) | DspCmd::Meter(_) | DspCmd::Squelch(_))) => {
                match cmd {
                    DspCmd::Decoder(k) => aprs = k,
                    DspCmd::Pager(on) => pager = on,
                    DspCmd::Audio(mode) => {
                        if mode != listen {
                            stream = Stream::new(mode);
                            dec = decim(mode); // the raw stream takes the data path (decision 316)
                        }
                        listen = mode;
                    }
                    DspCmd::Meter(on) => {
                        meter_on = on && cfg!(feature = "bench");
                        #[cfg(feature = "bench")]
                        {
                            meter = on.then_some(ferrite_core::rxmeter::FRESH);
                        }
                    }
                    DspCmd::Squelch(open) => {
                        if sq && !open {
                            tail = ferrite_core::msg::SQ_TAIL_MS / HALF_MS;
                        }
                        if open && !sq {
                            sq_at = Some(embassy_time::Instant::now());
                        }
                        sq = open;
                    }
                    _ => {}
                }
                let kind = match (aprs, pager) {
                    (DecoderKind::Off, true) => DecoderKind::Pocsag,
                    (k, _) => k,
                };
                if kind != running {
                    running = kind;
                    slot.switch(kind);
                }
                // PWR-26.9: AFSK only while the APRS slot's squelch is open, and its tail; the pager decoder always
                let want = kind != DecoderKind::Off && (kind != DecoderKind::Afsk || sq || tail != 0) || listen != 0 || meter_on;
                if want && !capturing {
                    clock::for_audio().await; // PWR-26.4: the speed first, never under a running capture
                    adc::init();
                    dec = decim(listen);
                    audio::capture_start();
                    if running == DecoderKind::Afsk {
                        CAPTURES.fetch_add(1, Relaxed);
                        since_cap = Some(0);
                    }
                    // PWR-26.9.1: how long after the squelch opened the capture started (the readback's `decode` line),
                    // to check it lands well inside a packet's TX delay
                    if let Some(at) = sq_at.take() {
                        let ms = at.elapsed().as_millis() as u32;
                        CAPTURE_MS.store(ms, Relaxed);
                        CAPTURE_MAX_MS.fetch_max(ms, Relaxed);
                    }
                } else if !want && capturing {
                    audio::capture_stop();
                }
                capturing = want;
            }
            Either4::First(DspCmd::Sol950Finish) => {
                if running == DecoderKind::Sol950 {
                    if capturing {
                        audio::capture_stop();
                    }
                    use ferrite_core::sol950::usol950::{Decoded, MAX_DECODED};
                    let mut out = [Decoded::EMPTY; MAX_DECODED];
                    let n = slot.sol950_finish(&mut out);
                    for d in &out[..n] {
                        if crate::chan::SOL950_RX.try_send(*d).is_err() {
                            DROPPED.fetch_add(1, Relaxed);
                        }
                    }
                    if n != 0 {
                        crate::chan::TERM_WAKE.signal(());
                    }
                    if capturing {
                        clock::for_audio().await;
                        dec = decim(listen);
                        audio::capture_start();
                    }
                }
            }
            Either4::First(DspCmd::Beep { .. }) => {} // beeps are the BK4829's (SND-11)
            Either4::Second(half) => {
                // APRS-56.4: the half's wake and work times, bench builds only (decision 363)
                #[cfg(feature = "bench")]
                let (t0, mhz) = (crate::hal::time::cycles(), crate::hal::clock::mhz().max(1));
                #[cfg(feature = "bench")]
                WAKE_MAX_US.fetch_max(t0.wrapping_sub(crate::hal::audio::ADC_AT.load(Relaxed)) / mhz, Relaxed);
                // PWR-26.4: a capture that started at 24 MHz (the governor didn't answer in time) gets the build's
                // speed: stopped, the speed asked for, started again; looked at every 10 s.
                halves = halves.wrapping_add(1);
                // PWR-26.9: the squelch shut and its tail done: AFSK's capture stops (the build's speed may go)
                if tail != 0 {
                    tail -= 1;
                    if tail == 0 && !sq && running == DecoderKind::Afsk && listen == 0 && !meter_on {
                        audio::capture_stop();
                        capturing = false;
                        continue;
                    }
                }
                if halves.is_multiple_of(RECHECK_HALVES) && clock::is_low() && clock::AUDIO_RAISES.load(Relaxed) {
                    audio::capture_stop();
                    clock::for_audio().await;
                    dec = decim(listen);
                    audio::capture_start();
                    continue;
                }
                let base = half as usize * BLOCK_IN;
                let mut out = [0i16; BLOCK_OUT];
                let mut n = 0;
                let mut clips = 0;
                for s in &audio::ADC_BUF[base..base + BLOCK_IN] {
                    let s = s.load(Relaxed);
                    clips += ferrite_core::rxlevel::at_rail(s) as u32; // TERM-32.1
                    dec.push(s, |x| {
                        if n < BLOCK_OUT {
                            out[n] = x;
                            n += 1;
                        }
                    });
                }
                #[cfg(feature = "bench")]
                if let Some(m) = &mut meter {
                    for s in &audio::ADC_BUF[base..base + BLOCK_IN] {
                        if m.push(s.load(Relaxed)) {
                            let r = m.report;
                            METER.lock(|c| c.set(r));
                        }
                    }
                }
                if clips != 0 {
                    AUDIO_CLIPS.fetch_add(clips, Relaxed);
                }
                peak = out[..n].iter().fold(peak, |p, &x| p.max((x as i32).abs()));
                lvl.0 = out[..n].iter().fold(lvl.0, |p, &x| p.max(x.unsigned_abs()));
                if halves.is_multiple_of(LVL_HALVES) {
                    lvl = (0, lvl.0);
                }
                if halves.is_multiple_of(PEAK_HALVES) {
                    AUDIO_PEAK.store(peak as u32, Relaxed);
                    peak = 0;
                }
                match &mut stream {
                    Stream::Off => {}
                    Stream::Listen { pack, sender, agc } => {
                        // LINK-13.8: scaled for the PC only, held while nothing is heard
                        let open = ferrite_core::rxaudio::unpack_source(crate::chan::AUDIO_SRC.load(Relaxed)).1 & ferrite_core::rxaudio::SQ_OPEN != 0;
                        let mut y = out;
                        // the AGC's steps are per 2 ms chunk (its release rate), whatever the half
                        for c in y[..n].chunks_mut(ferrite_core::rxlevel::CHUNK) {
                            agc.chunk(c, open);
                        }
                        AUDIO_GAIN.store(agc.gain() as u32, Relaxed);
                        let mut made = false;
                        for &x in &y[..n] {
                            pack.push(x, |b| {
                                AUDIO_MADE.fetch_add(1, Relaxed);
                                sender.block(b);
                                made = true;
                            });
                        }
                        if !made {
                            sender.drain(false); // decision 331: blocks left waiting go as soon as there's room
                        }
                    }
                    // LINK-13.9: the decoders' samples as they are (no gain)
                    #[cfg(feature = "bench")]
                    Stream::Raw(r) => r.chunk(&out[..n]),
                }
                feed(&mut slot, &out[..n], lvl.0.max(lvl.1), &mut heard, &mut since_cap);
                #[cfg(feature = "bench")]
                WORK_MAX_US.fetch_max(crate::hal::time::cycles().wrapping_sub(t0) / mhz, Relaxed);
            }
            Either4::Fourth(None) => {} // a stop with nothing playing
            #[cfg(not(all(feature = "tx", feature = "bench")))]
            Either4::Fourth(Some(never)) => match never {},
            #[cfg(all(feature = "tx", feature = "bench"))]
            Either4::Fourth(Some((g, eq))) => {
                if capturing {
                    audio::capture_stop();
                }
                play_test(g, eq).await;
                if capturing {
                    clock::for_audio().await;
                    dec = decim(listen);
                    audio::capture_start();
                }
            }
            Either4::Third(frame) => {
                if capturing {
                    audio::capture_stop();
                }
                play(&frame).await;
                // PWR-26.9.1: a digipeater's copy comes back before the squelch's open reaches here (Alex, Nov 4,
                // clear: the speaker broke squelch on the echo, nothing framed): capture now, whatever the squelch.
                if running == DecoderKind::Afsk {
                    tail = tail.max(ferrite_core::msg::AFTER_TX_MS / HALF_MS);
                }
                let want = capturing || running == DecoderKind::Afsk;
                if want {
                    clock::for_audio().await;
                    if !capturing {
                        adc::init();
                    }
                    dec = decim(listen);
                    audio::capture_start();
                }
                capturing = want;
                AFSK_DONE.signal(true);
                crate::chan::TERM_WAKE.signal(()); // the UI task's APRS unkeys now, not at its next 20 ms pass
            }
        }
    }
}

/// One frame out of DAC1: both halves filled, then each half refilled as it goes out; mid-scale after the last
/// sample, and stopped once that has played too.
async fn play(f: &Frame) {
    // The TX delay's flags first: the transmitter's key-up fits in them. FX.25 (APRS-45) when the frame asks for it.
    let cfg = afsk_tx_at(f.txdelay_10ms as u16 * 10, f.level);
    let mut enc = None;
    let mut m = uafsk::Modulator::from_bits(tx_bits(f.bytes(), f.fx25, &cfg, &mut enc), cfg);
    let fill = |half: usize, m: &mut uafsk::Modulator<TxBits>| -> bool {
        let mut more = true;
        for d in &audio::DAC_BUF[half * BLOCK_DAC..(half + 1) * BLOCK_DAC] {
            let s = if more { m.next() } else { None };
            more &= s.is_some();
            d.store(to_dac(s.unwrap_or(0)), Relaxed);
        }
        more
    };
    let mut more = fill(0, &mut m) && fill(1, &mut m);
    clock::for_audio().await; // PWR-26.4
    audio::DAC_HALF.reset();
    audio::play_start();
    // After the modulator ends: one half of silence written, then wait for it to play.
    let mut tail = 2;
    while tail > 0 {
        let half = audio::DAC_HALF.wait().await as usize;
        if more {
            more = fill(half, &mut m);
        } else {
            fill(half, &mut m);
            tail -= 1;
        }
    }
    audio::play_stop();
}

/// TERM-33, TERM-34: the test signal out of DAC1 until [`TXTEST`] says stop (None); a new generator (Some) takes
/// over from the next half filled, DAC1 still running (the suite's steps). Each generator loops or ends in silence
/// by itself. Then a half of silence, and stopped once that has played. TERM-33.16: the samples through the
/// equaliser asked for (its 380-byte delay line lives here, in the DSP task's future: bench builds only).
#[cfg(all(feature = "tx", feature = "bench"))]
async fn play_test(mut g: ferrite_core::txtest::Gen, eq: ferrite_core::txeq::Eq) {
    use ferrite_core::txeq::Fir;
    let mut fir = Fir::new(eq);
    let fill = |half: usize, g: Option<&mut ferrite_core::txtest::Gen>, fir: &mut Fir| match g {
        Some(g) => {
            for d in &audio::DAC_BUF[half * BLOCK_DAC..(half + 1) * BLOCK_DAC] {
                d.store(to_dac(fir.run(g.sample())), Relaxed);
            }
        }
        None => {
            for d in &audio::DAC_BUF[half * BLOCK_DAC..(half + 1) * BLOCK_DAC] {
                d.store(to_dac(0), Relaxed);
            }
        }
    };
    fill(0, Some(&mut g), &mut fir);
    fill(1, Some(&mut g), &mut fir);
    clock::for_audio().await; // PWR-26.4
    audio::DAC_HALF.reset();
    audio::play_start();
    loop {
        use embassy_futures::select::{select, Either};
        match select(audio::DAC_HALF.wait(), TXTEST.wait()).await {
            Either::First(half) => fill(half as usize, Some(&mut g), &mut fir),
            Either::Second(Some((next, e))) => {
                g = next;
                if e != fir.eq() {
                    fir = Fir::new(e);
                }
            }
            Either::Second(None) => break,
        }
    }
    // Both halves silent, so whichever plays next is mid-scale; then off.
    for _ in 0..2 {
        let half = audio::DAC_HALF.wait().await as usize;
        fill(half, None, &mut fir);
    }
    audio::play_stop();
}
