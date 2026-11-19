//! The channels between the tasks (types in ferrite_core::msg): a command channel per owner, one event bus,
//! and the snapshots the UI draws from. All static; senders wait when a channel is full, nothing is dropped.

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex as M;
use embassy_sync::channel::Channel;
use embassy_sync::pubsub::PubSubChannel;
use embassy_sync::signal::Signal;
use embassy_sync::watch::Watch;
use crate::ring::Ring;
use core::sync::atomic::{AtomicBool, AtomicI16, AtomicU16, AtomicU32, AtomicU8};
#[cfg(feature = "bench")]
use ferrite_core::msg::{DebugAns, DebugReq};
use ferrite_core::msg::{GGA_MAX, DspCmd, Event, GpsFix, GpsSky, LiveAll, RadioCmd, SameDecoded, StorageCmd};

pub static RADIO: Channel<M, RadioCmd, 8> = Channel::new();
pub static STORAGE: Channel<M, StorageCmd, 8> = Channel::new();
pub static DSP: Channel<M, DspCmd, 4> = Channel::new();
/// The terminal's BK4829 and SI4732 debug requests to the Radio task, and its answers (TERM-21): one at a time.
/// A bench build's (TERM-38): every Radio task request is a bench command.
#[cfg(feature = "bench")]
pub static DEBUG_REQ: Channel<M, DebugReq, 1> = Channel::new();
#[cfg(feature = "bench")]
pub static DEBUG_ANS: Channel<M, DebugAns, 1> = Channel::new();

/// Keys and the knob, from the input sampler to the UI.
#[derive(Clone, Copy)]
pub enum Input {
    Key(keypad::Event),
    /// Detents, + clockwise.
    Knob(i8),
}
pub static INPUT: Channel<M, Input, 16> = Channel::new();

/// Everything that happened: 16 deep, 4 subscribers (UI, Link, Storage, spare), 6 publishers (one per task).
pub static EVENTS: PubSubChannel<M, Event, 16, 4, 6> = PubSubChannel::new();

/// Alerts the decoders heard (SAME headers), to the UI task: it raises the alert screen (`App::same_alert`).
/// Two deep: a second header (SAME repeats each three times) waits while the first is shown.
pub static ALERTS: Channel<M, SameDecoded, 2> = Channel::new();

/// SOL950 decoded frames from the DSP task (`DspCmd::Sol950Finish`) to the UI task.
pub static SOL950_RX: Channel<M, ferrite_core::sol950::usol950::Decoded, 8> = Channel::new();
/// Latest 24-bin packed 4-bit SOL950 waterfall slice (`200 ms` cadence) from DSP to UI.
pub static SOL950_WF: Signal<M, [u8; 12]> = Signal::new();
/// Latest failed-CRC 92-byte soft candidate exported at end of slot for Cooperative RX capture.
pub static SOL950_SOFT: Signal<M, ferrite_core::sol950::usol950::SoftCandidate> = Signal::new();
/// SOL950 frame staged by the UI task before sending `RadioCmd::Sol950Tx` (keeps `RadioCmd <= 8 B`).
#[cfg(feature = "tx")]
pub static SOL950_TX: Signal<M, ferrite_core::sol950::usol950::Frame> = Signal::new();
/// SOL950 transmit completion from the Radio task to the UI task (`true` = all 110 symbols sent, `false` = aborted/refused).
#[cfg(feature = "tx")]
pub static SOL950_TX_DONE: Signal<M, bool> = Signal::new();

/// The Radio task's snapshot of all three slots (one copy, 308 B: the UI keeps its own).
pub static LIVE: Watch<M, LiveAll, 2> = Watch::new();

/// The `tx` build: the factory calibration's transmit side, read once at boot by the UI task (which mounts the
/// SPI flash) for the Radio task: the PA curves (None when blank or implausible: transmit locked, CAL-5, CAL-6)
/// and the battery gauge (PWR-20).
#[cfg(feature = "tx")]
pub static TX_CAL: Signal<M, (Option<ferrite_core::txcal::TxCal>, ferrite_core::txcal::Gauge)> = Signal::new();
/// SQL-1: the calibration's squelch tables, read at boot in every build (the UI task), for the Radio task.
pub static SQ_CAL: Signal<M, ferrite_core::sqcal::SqCal> = Signal::new();
/// GPS (manual chapter 28): the latest fix for the UI and the APRS task (and one spare reader), and the
/// satellites in view for the UI's sky plot. The GPS task sends both.
pub static POSITION: Watch<M, GpsFix, 3> = Watch::new();
pub static SKY: Watch<M, GpsSky, 1> = Watch::new();
/// What the GPS should do (0 6 1, 0 6 8), from the UI when the settings change.
pub static GPS_CFG: Signal<M, ferrite_app::gpsdata::GpsCfg> = Signal::new();
/// Who needs a position (GPS-26 to GPS-28), from the UI when it changes, for the GPS task's power manager; and
/// what the manager has the module doing ([`ferrite_core::gpspower::GpsPhase::pack`]) for the Status page.
pub static GPS_DEMAND: Signal<M, ferrite_core::gpspower::GpsDemand> = Signal::new();
pub static GPS_PHASE: AtomicU32 = AtomicU32::new(0);
/// What the radio is doing, for the GPS task (GPS-4, GPS-22): set by the Radio task with each snapshot.
pub static RF_STATE: AtomicU8 = AtomicU8::new(0);
pub const RF_BUSY_KEYED: u8 = 1;
/// Dry TX (TX-48): the PA drive held at 0 on every key-up of a `tx` build. Written only by the UI task, from the
/// App's switch, before it hands the Radio task any command ([`crate::tasks::ui`]'s `to_radio`); read by
/// [`crate::hal::pa::set`] and the cable's keyed send rule. False at every power-on: never stored.
pub static DRY_TX: AtomicBool = AtomicBool::new(false);
/// GPS-22 (TX audit F15): the Radio task drove PB10 low itself just before a key-up; the GPS task takes the pin
/// back from there (`GpsPower::forced_low`).
pub static PB10_FORCED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// The MCU's die temperature in tenths of a degree C (chapter 8, SBAR-20), smoothed; `i16::MIN` until read. The
/// Radio task reads it with its other ADC reads (the preempted group isn't shared between tasks); the UI shows it.
pub static MCU_TEMP: AtomicI16 = AtomicI16::new(i16::MIN);
/// The last MCU temperature reading's raw ADC counts, sensor << 16 | VREFINT, for the cable log (SBAR-20).
pub static MCU_TEMP_RAW: AtomicU32 = AtomicU32::new(0);
/// SND-12: the side connector's speaker mic, debounced by the input task ([`ferrite_core::acc::AccDet::pack`]: bit 0
/// connected, bit 1 a change being debounced). The Radio task holds PB8 on while connected and keeps VOX off while
/// it changes (VOX-3); the UI shows it on the Status page.
/// PWR-26.17: the panel asleep (the screen dark), for the input task's slower turn.
pub static SCREEN_DARK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
pub static ACCESSORY: AtomicU8 = AtomicU8::new(ferrite_core::acc::CHANGING); // changing until the boot check
/// TERM-30: the GPS module's last GGA sentence as received (from `$` to the checksum) and when (ms since boot): the
/// GPS task writes it, the terminal's `gps` reads it. 88 B.
pub static GGA: embassy_sync::blocking_mutex::Mutex<M, core::cell::RefCell<GgaCopy>> =
    embassy_sync::blocking_mutex::Mutex::new(core::cell::RefCell::new(GgaCopy { data: [0; GGA_MAX], n: 0, at_ms: 0 }));

/// TERM-36: a CASIC sentence for the GPS task to send (`$PCAS<id>,<arg>` and the quiet flag, already passed by
/// `ferrite_core::gpspower::casic_refused`), from the UI task's terminal; and the GPS task's answer, which the UI task
/// hands to the terminal.
#[cfg(feature = "bench")]
pub static GPS_SEND: Signal<M, (u8, u16, bool)> = Signal::new();
#[cfg(feature = "bench")]
pub static GPS_ANS: Signal<M, ferrite_core::msg::DebugAns> = Signal::new();

/// [`GGA`]'s contents.
pub struct GgaCopy {
    pub data: [u8; GGA_MAX],
    pub n: u8,
    pub at_ms: u32,
}
/// PWR-20: the latest PA1 battery reading (12-bit, averaged), 0 until the first one; the Radio task reads it.
pub static BATTERY_RAW: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(0);
pub const RF_BUSY_RX: u8 = 2;

/// The terminal over the cable (manual chapter 37): the Link task moves the cable's bytes into `TERM_IN` and
/// signals `TERM_WAKE`; the UI task, which owns the App the terminal reads and edits, feeds them to it and puts
/// what it draws in `TERM_OUT`, which the Link task sends. 64 B in (typed keys), 1 KB out (a few rows).
static TERM_IN_BUF: [AtomicU8; 64] = [const { AtomicU8::new(0) }; 64];
static TERM_OUT_BUF: [AtomicU8; 1024] = [const { AtomicU8::new(0) }; 1024];
pub static TERM_IN: Ring<'static> = Ring::new(&TERM_IN_BUF);
pub static TERM_OUT: Ring<'static> = Ring::new(&TERM_OUT_BUF);
pub static TERM_WAKE: Signal<M, ()> = Signal::new();
/// Decision 290: a thread-mode task is writing link frames into `TERM_OUT`; the DSP task, which preempts it, puts
/// its audio frames in only while this is clear, so a frame never lands inside another.
pub static TERM_OUT_LOCK: AtomicBool = AtomicBool::new(false);
/// LINK-13.4, decision 290: what the receive audio carries now (`ferrite_core::rxaudio::source`), set by the Radio
/// task, read by the DSP task as it frames each block.
pub static AUDIO_SRC: AtomicU16 = AtomicU16::new(0x00FF);

/// The Radio task: what is heard now, into [`AUDIO_SRC`] (`ferrite_core::rxaudio::source`).
pub fn heard_now(open: [bool; 3], bcast: Option<u8>, hf: bool) {
    AUDIO_SRC.store(ferrite_core::rxaudio::source(open, bcast, hf), core::sync::atomic::Ordering::Relaxed);
}

/// Runs `f`, which writes into `TERM_OUT`, with [`TERM_OUT_LOCK`] held (thread-mode writers only).
pub fn term_out_locked<R>(f: impl FnOnce() -> R) -> R {
    TERM_OUT_LOCK.store(true, core::sync::atomic::Ordering::SeqCst);
    let r = f();
    TERM_OUT_LOCK.store(false, core::sync::atomic::Ordering::SeqCst);
    r
}

/// The link protocol on the cable (manual chapter 29), between the UI task (which runs the session next to the
/// App, as it runs the terminal) and the Link task (which owns UART4): a rate agreed by `BAUD`, to switch to once
/// the answer has gone; the `HELLO`s seen, the proof the new rate works (LINK-11); the rate now; and whether the
/// link holds the port (no XON/XOFF in its binary frames then).
pub static LINK_BAUD: Signal<M, u32> = Signal::new();
pub static LINK_HELLOS: AtomicU16 = AtomicU16::new(0);
pub static CABLE_BAUD: AtomicU32 = AtomicU32::new(115_200);
pub static LINK_ON: AtomicBool = AtomicBool::new(false);

/// Bluetooth (manual chapter 29, roadmap 3d): the Link task moves USART1's bytes into `BT_IN` and sends `BT_OUT`,
/// as it does the cable's; the UI task runs the terminal, the link or KISS on them ([`ferrite_app::ports`]).
/// 64 B in (the UART's own 256 B ring is behind it), 512 B out (a terminal page in a few turns, a link answer,
/// a KISS frame). `BT_ON` is 0 9 2 1 (the module's power, PB9), `BT_LINK_ON` the link holding Bluetooth (no
/// XON/XOFF then).
static BT_IN_BUF: [AtomicU8; 64] = [const { AtomicU8::new(0) }; 64];
static BT_OUT_BUF: [AtomicU8; 512] = [const { AtomicU8::new(0) }; 512];
pub static BT_IN: Ring<'static> = Ring::new(&BT_IN_BUF);
pub static BT_OUT: Ring<'static> = Ring::new(&BT_OUT_BUF);
pub static BT_ON: AtomicBool = AtomicBool::new(false);
pub static BT_LINK_ON: AtomicBool = AtomicBool::new(false);
/// USART1's rate (LINK-25): the module's resting 9600 ([`ferrite_core::link::BT_BAUD`]), back to it whenever the
/// module is powered; 115200 once Bluetooth's `BAUD` has agreed it and a `HELLO` arrived at it (LINK-15.1: the host
/// unlocks the module, the MCU never writes to it); the other rate after a framing error (LINK-25.2). A bench
/// build's `bt baud N` sets it too. The Link task restarts USART1 when it differs from the running one.
pub static BT_RATE: AtomicU32 = AtomicU32::new(ferrite_core::link::BT_BAUD);
/// LINK-15.1: a rate agreed by Bluetooth's `BAUD`, to switch to once the answer has gone; Bluetooth's `HELLO`s.
pub static BT_LINK_BAUD: Signal<M, u32> = Signal::new();
pub static BT_HELLOS: AtomicU16 = AtomicU16::new(0);
/// LINK-36.1: the bursts and gaps at 115200 ([`ferrite_core::link::BtPace::FAST`] packed); `bt pace B G` sets
/// them while USART1 runs fast.
pub static BT_PACE_FAST: AtomicU32 = AtomicU32::new(ferrite_core::link::BtPace::FAST.pack());
/// LINK-36's Bluetooth pacing ([`ferrite_core::link::BtPace`] packed): bursts and gaps for the BT513. A bench
/// build's `bt pace B G` changes it in RAM; the Link task reads it every turn.
pub static BT_PACE: AtomicU32 = AtomicU32::new(ferrite_core::link::BtPace::DEFAULT.pack());
