//! GPS (manual chapter 28, roadmap 5a): the ATGM336H on USART3 (PB11 in, 9600 baud, as stock) and
//! its power on PA8 (low = on), which this task owns (decision 76).
//!
//! - **Power (GPS-1, GPS-4):** on or off with 0 6 1 (Auto: on while something needs a position, decided in the UI
//!   task by `ferrite_app::gpsdata::auto_step`, GPS-1.5), and off around signals as 0 6 8 asks (the UI sends
//!   [`GPS_CFG`]; the Radio task's [`RF_STATE`] says what it is doing). After a signal the module comes back
//!   2 s later, so a burst of short signals doesn't cycle it. GPS off forgets the fix and publishes "off";
//!   powered off with GPS on, the last fix stays with its age (GPS-25).
//! - **PB10 while keyed (GPS-5, GPS-22):** USART3's TX pin is driven low while any chip transmits and handed
//!   back to the UART after, as stock does (0x080252D8 / 0x0802530C).
//! - **The power manager (GPS-26 to GPS-33, [`ferrite_core::gpspower`]):** with GPS on, the module sleeps between
//!   fixes: `$PCAS12,N` on USART3 TX (CASIC standby, it wakes itself N s later), a wake byte when a GPS screen
//!   or a beacon needs it sooner, a PA8 power cycle when that brings no NMEA (to confirm on the radio). The UI
//!   sends who needs a position ([`GPS_DEMAND`]); nothing goes to the module while keyed.
//! - **The NMEA set (GPS-34, [`ferrite_core::gpspower::NmeaSet`]):** once the module is heard after each power-up
//!   and wake, `$PCAS03` cuts its output to GGA and RMC every fix and GSV and GSA every 5th (every fix while the
//!   position or satellites page is open); a module still sending GLL, VTG or ZDA 2 s later is sent it again.
//! - **The terminal's bench test (TERM-36, `ferrite_core::gpspower::GpsBench`; a bench build's, TERM-38):** `gps nmea off|on`, `gps standby
//!   N [quiet]`, `gps query N`; from NMEA off or a standby until NMEA is on and the standby over, the manager is held
//!   (nothing of its sent, no power cycle) and nothing else goes to the module during a standby.
//! - **Parsing:** [`ferrite_core::gps::Nmea`] (about 260 B, here only). The fix goes to [`POSITION`] for the UI
//!   and the APRS task at most twice a second and whenever it changes state; the satellites go to [`SKY`] once
//!   a second for the sky plot.
//! - **The clock and the screens** are the UI task's: it reads POSITION and SKY into the store's GPS data
//!   (`ferrite_app::gpsdata`), and sets the clock from GPS time (GPS-7).
//! - **Slow flash** (memory.x, since Oct 13, 2026): the task and the parser run from the slow part. It is thread mode,
//!   the UART interrupt (fast) fills the ring, and NMEA at 9600 bd is under 1 KB/s.

use crate::board::GPS_PWR_N;
use crate::chan::{GPS_CFG, GPS_DEMAND, GPS_PHASE, POSITION, RF_BUSY_KEYED, RF_BUSY_RX, RF_STATE, SKY};
use crate::hal::gpio::{pin, Mode, GPIOB};
use crate::hal::uart;
use crate::report::UART_GPS;
use core::sync::atomic::Ordering::Relaxed;
use embassy_time::{Instant, Timer};
use ferrite_app::gpsdata::GpsCfg;
use ferrite_core::gps::Nmea;
#[cfg(feature = "bench")]
use ferrite_core::gpspower::GpsBench;
use ferrite_core::gpspower::{casic, set_off, shown, GpsDemand, GpsManager, GpsOut, GpsPhase, NmeaSet, Stray, CASIC_MAX, CAS_NMEA, CAS_STANDBY, POKE};
#[cfg(feature = "bench")]
use ferrite_core::msg::{DebugAns, GGA_MAX, GPS_REPLY_MS};
use ferrite_core::msg::{GpsFix, GpsSky};
use ferrite_radio::gps::GpsPower;

/// The ATGM336H's default rate.
const BAUD: u32 = 9600;
/// USART3 TX to the module.
const GPS_TX: crate::hal::gpio::Pin = pin(GPIOB, 10);
/// How often the task looks: 960 bytes/s at most, and the UART's 1 KB ring holds a second.
const POLL_MS: u64 = 50;
/// PWR-23: the look's period while GPS is off.
const OFF_POLL_MS: u64 = 500;
/// The fix is published at most this often (the module sends once a second, in several sentences).
const PUBLISH_MS: u32 = 500;

/// Monotonic 2 kHz (`0.5 ms`) tick of the first byte of the most recent 1 Hz NMEA burst (`> 400 ms` UART idle),
/// latched in `USART3` RX ISR so slot timing removes `POLL_MS` (50 ms) polling jitter.
static GPS_BURST_TICK: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static GPS_LAST_RX_TICK: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Matched `(unix_sec, pps_tick)` pair latched atomically by `gps::task` only when `f.unix` advances
/// after `$GNRMC` parses — prevents pairing `fix.unix = N-1` with `GPS_BURST_TICK` from burst `N`!
/// `ATGM336H_NMEA_LAG_TICKS = 300` (`150 ms` at 2 kHz) compensates typical PPS-to-first-byte NMEA delay.
const ATGM336H_NMEA_LAG_TICKS: u32 = 300;
static GPS_SYNC_GEN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static GPS_SYNC_UNIX: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static GPS_SYNC_TICK: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

#[inline(always)] // called from `usart3` ISR (fast flash)
pub fn on_rx_isr() {
    let t = Instant::now().as_ticks() as u32;
    let prev = GPS_LAST_RX_TICK.swap(t, Relaxed);
    if t.wrapping_sub(prev) >= 800 {
        GPS_BURST_TICK.store(t, Relaxed);
    }
}

/// Latch a matched `(unix_sec, pps_tick)` pair when `$GNRMC` advances `unix`.
fn latch_sync_pair(unix: u32) {
    let burst = GPS_BURST_TICK.load(Relaxed);
    if unix == 0 || burst == 0 {
        return;
    }
    let pps_tick = burst.wrapping_sub(ATGM336H_NMEA_LAG_TICKS);
    let g = GPS_SYNC_GEN.load(Relaxed).wrapping_add(1) | 1; // odd = writing
    GPS_SYNC_GEN.store(g, core::sync::atomic::Ordering::Release);
    GPS_SYNC_UNIX.store(unix, Relaxed);
    GPS_SYNC_TICK.store(pps_tick, Relaxed);
    GPS_SYNC_GEN.store(g.wrapping_add(1), core::sync::atomic::Ordering::Release); // even = stable
}

/// Read the latest matched `(unix_sec, pps_tick)` pair (`None` until first valid `$GNRMC` fix).
pub fn latest_utc_sync() -> Option<(u32, u32)> {
    for _ in 0..3 {
        let g1 = GPS_SYNC_GEN.load(core::sync::atomic::Ordering::Acquire);
        if (g1 & 1) != 0 {
            continue;
        }
        let u = GPS_SYNC_UNIX.load(Relaxed);
        let t = GPS_SYNC_TICK.load(Relaxed);
        let g2 = GPS_SYNC_GEN.load(core::sync::atomic::Ordering::Acquire);
        if g1 == g2 && u != 0 && t != 0 {
            return Some((u, t));
        }
    }
    None
}

/// GPS-22 (TX audit F15): PB10 driven low now, by the Radio task just before a key-up, as stock's PTT press does
/// (0x0801C6D4) before its key-up; the GPS task's next look keeps it low while keyed, or hands it back.
#[cfg(feature = "tx")]
#[inline(always)] // called from the Radio task (fast flash): inlined there, never a call into this task's slow code
pub fn pb10_low_now() {
    GPS_TX.output(false, Mode::OutPp);
    crate::chan::PB10_FORCED.store(true, Relaxed);
}

fn now_ms() -> u32 {
    Instant::now().as_millis() as u32
}

/// Sends bytes to the module with their cable-log line (TERM-28, GPS-26): "gps: > " and the bytes exactly as they
/// go out, CR and LF shown as `<CR>` `<LF>`, so the bench can check every command.
#[inline(never)] // in .slow_text (memory.x)
fn send(b: &[u8]) {
    uart::write(UART_GPS, b);
    let mut t = [0u8; 56];
    let n = shown(b, &mut t);
    crate::bringup::linef(|l| {
        let _ = ufmt::uwrite!(l, "gps: > {} ({} bytes)", core::str::from_utf8(&t[..n]).unwrap_or("?"), b.len());
    });
}

/// A CASIC sentence ([`casic`]) to the module, logged.
#[inline(never)] // in .slow_text (memory.x)
fn send_casic(id: u8, arg: u16) {
    let mut b = [0u8; CASIC_MAX];
    let n = casic(id, arg, &mut b);
    send(&b[..n]);
}

/// TERM-36: the terminal's CASIC sentence, sent now (checked again: only CAS03 NMEA off/on, CAS06 queries and CAS12
/// standby, and nothing during a bench standby, [`GpsBench::command`]), or refused; the first TXT sentence within
/// [`GPS_REPLY_MS`] is its answer (`Query` keeps the wait).
#[cfg(feature = "bench")]
#[inline(never)] // in .slow_text (memory.x)
fn term_send(bench: &mut GpsBench, set: &mut NmeaSet, (id, arg, quiet): (u8, u16, bool), powered: bool, keyed: bool, now: u32) -> Option<Query> {
    let refuse = |why| {
        crate::bringup::linef(|l| {
            let _ = ufmt::uwrite!(l, "gps: bench: refused: {}", why);
        });
        crate::chan::GPS_ANS.signal(DebugAns::Refused(why));
        crate::chan::TERM_WAKE.signal(());
        None
    };
    if !powered {
        return refuse("the GPS module is off (0 6 1)");
    }
    if keyed {
        return refuse("transmitting: PB10 is held low (GPS-22)");
    }
    match bench.command(id, arg, quiet, now) {
        Err(why) => refuse(why),
        Ok((id, arg)) => {
            if quiet {
                crate::bringup::linef(|l| {
                    let _ = ufmt::uwrite!(l, "gps: bench: quiet standby {} s (NMEA off, CAS12 in 100 ms)", arg);
                });
            }
            send_casic(id, arg);
            set.bench_sent(id, arg, now);
            Some(Query { at: now, data: [0; GGA_MAX], n: 0 })
        }
    }
}

/// A terminal query waiting for its TXT answer.
#[cfg(feature = "bench")]
struct Query {
    at: u32,
    data: [u8; GGA_MAX],
    n: u8,
}

/// The power manager's look (GPS-26 to GPS-33): what it sends the module goes out here, with a line in the cable
/// log (TERM-28); its phase goes to [`GPS_PHASE`]. The power wanted (PA8) is the caller's, through GPS-4's rule.
/// True as the module comes out of standby (its own wake or the byte): the parser has started afresh, so the fix
/// from before the sleep isn't taken for this wake's.
#[inline(never)] // in .slow_text (memory.x): once a look of the GPS task, never from an interrupt
fn manage(mgr: &mut GpsManager, nmea: &mut Nmea, phase: &mut GpsPhase, now: u32, d: &GpsDemand, keyed: bool) -> (GpsOut, bool) {
    let out = mgr.step(now, d, keyed);
    let was = core::mem::replace(phase, mgr.phase(now));
    let woke = matches!(was, GpsPhase::Standby(_)) && !matches!(*phase, GpsPhase::Standby(_));
    if woke {
        nmea.reset();
        crate::bringup::line(b"gps: awake");
    }
    GPS_PHASE.store(phase.pack(), Relaxed);
    if let Some(s) = out.standby_s {
        crate::bringup::linef(|l| {
            let _ = ufmt::uwrite!(l, "gps: standby {} s", s);
        });
        send_casic(CAS_STANDBY, s);
    }
    if out.poke {
        crate::bringup::line(b"gps: wake byte (needed sooner)");
        send(POKE);
    }
    if out.cycled {
        crate::bringup::line(b"gps: no NMEA after a wake: power cycled");
    }
    (out, woke)
}

/// TERM-30: the GGA sentence just parsed, with its `$`, into [`crate::chan::GGA`] for the terminal's `gps`.
#[inline(never)] // in .slow_text (memory.x): once a second, never from an interrupt
fn keep_gga(nmea: &Nmea, now: u32) {
    crate::chan::GGA.lock(|g| {
        let mut g = g.borrow_mut();
        let s = nmea.last();
        let n = s.len().min(g.data.len() - 1);
        g.data[0] = b'$';
        g.data[1..=n].copy_from_slice(&s[..n]);
        (g.n, g.at_ms) = (n as u8 + 1, now);
    });
}

#[embassy_executor::task]
pub async fn task() {
    GPS_PWR_N.write(true); // off until the first look below decides
    // PWR-23: USART3 runs only while the module is powered; off, its pins float, so PB10 idling high doesn't
    // feed the unpowered module through its RX pin.
    uart::deinit(UART_GPS);
    let mut nmea = Nmea::new();
    let (tx, sky) = (POSITION.sender(), SKY.sender());
    tx.send(GpsFix::default());
    let mut cfg = GpsCfg::DEFAULT;
    // The power and PB10 decisions (`ferrite_radio::gps`, host-tested), and when the fix and the sky were last
    // published.
    let mut power = GpsPower::default();
    let (mut mgr, mut demand) = (GpsManager::new(), GpsDemand::default());
    let mut phase = GpsPhase::Off;
    let mut set = NmeaSet::new();
    let (mut fix_at, mut sky_at) = (0u32, 0u32);
    let mut last_state = (false, 0u8);
    #[cfg(feature = "bench")]
    let mut query: Option<Query> = None;
    #[cfg(feature = "bench")]
    let mut bench = GpsBench::new();
    // The cable log (TERM-28): sentences and bytes from the module per 5 s while powered, and the fix.
    let (mut sentences, mut bytes, mut log_at) = (0u32, 0u32, 0u32);
    loop {
        if let Some(c) = GPS_CFG.try_take() {
            cfg = c;
        }
        if let Some(d) = GPS_DEMAND.try_take() {
            demand = d;
        }
        let now = now_ms();
        let rf = RF_STATE.load(Relaxed);
        let (keyed, rx) = (rf & RF_BUSY_KEYED != 0, rf & RF_BUSY_RX != 0);
        // GPS-26: the manager's look (standby, wake byte, power cycle); nothing sent while keyed (GPS-32). Held
        // during the terminal's bench test (TERM-36): the module stays powered and gets nothing of the manager's.
        #[cfg(feature = "bench")]
        let held = bench.holds();
        #[cfg(not(feature = "bench"))]
        let held = false;
        let (m, woke) = if held {
            (GpsOut { power: true, ..GpsOut::default() }, false)
        } else {
            manage(&mut mgr, &mut nmea, &mut phase, now, &GpsDemand { on: cfg.on, ..demand }, keyed)
        };
        if woke {
            last_state = (false, 0);
            (sentences, bytes, log_at) = (0, 0, now);
        }
        // GPS-22: PB10 low while keyed, back to the UART after. GPS-4: off around signals as asked, back on
        // `QUIET_MS` after the last.
        if crate::chan::PB10_FORCED.swap(false, Relaxed) {
            power.forced_low();
        }
        let out = power.step(now, cfg.on && m.power, cfg.powered(rx, keyed), keyed);
        #[cfg(feature = "bench")]
        if out.pb10_low == Some(true) && bench.in_standby() {
            // TERM-36: a transmit during a bench standby holds the module's RX line low (a break), which may wake it.
            crate::bringup::line(b"gps: bench: PB10 low (keyed) during the standby");
        }
        match out.pb10_low {
            Some(true) => GPS_TX.output(false, Mode::OutPp),
            // Back to the UART, or floating while the module is off (PWR-23).
            Some(false) if power.powered && out.power != Some(false) => GPS_TX.output(true, Mode::AfPp),
            Some(false) => GPS_TX.config(Mode::Analog),
            None => {}
        }
        let powered = power.powered;
        if let Some(want) = out.power {
            // PWR-23: the port with the module; PB10 stays low if keyed.
            if want {
                uart::init(UART_GPS, BAUD);
                if keyed {
                    GPS_TX.output(false, Mode::OutPp);
                }
            } else {
                uart::deinit(UART_GPS);
                if keyed {
                    GPS_TX.output(false, Mode::OutPp);
                }
            }
            GPS_PWR_N.write(!want);
            mgr.powered(now, want);
            crate::bringup::line(if want { b"gps: power on" } else { b"gps: power off" });
            (sentences, bytes, log_at) = (0, 0, now);
            if !want {
                #[cfg(feature = "bench")]
                {
                    if bench.holds() {
                        crate::bringup::line(b"gps: bench: test over (power off: the module's RAM settings are gone)");
                    }
                    bench.off();
                }
                nmea.reset();
                // GPS on but powered off for a while (GPS-4, the power cycle): the last fix stays, with its age.
                tx.send(GpsFix { on: cfg.on, at_ms: now, ..GpsFix::default() });
                sky.send(GpsSky::default());
                last_state = (false, 0);
            } else {
                tx.send(GpsFix { on: true, at_ms: now, ..GpsFix::default() });
            }
        }
        #[cfg(feature = "bench")]
        if let Some(c) = crate::chan::GPS_SEND.try_take() {
            query = term_send(&mut bench, &mut set, c, powered, keyed, now);
        }
        // TERM-36.4: a bench command taken just now holds the module from here on, this pass included (the NMEA set
        // below used to go right after a quiet standby's NMEA off).
        #[cfg(feature = "bench")]
        let held = held || bench.holds();
        // A quiet standby's CAS12 and its NMEA on at the latest (TERM-36); not while keyed (PB10 low).
        #[cfg(feature = "bench")]
        if powered && !keyed {
            let was = bench.holds();
            if let Some((id, arg)) = bench.step(now) {
                send_casic(id, arg);
                set.bench_sent(id, arg, now);
            }
            if was && !bench.holds() {
                crate::bringup::line(b"gps: bench: over, back to the power manager");
            }
        }
        // GPS-34: Ferrite's NMEA set once the module is heard after a power-up or wake, and the sky variant as the
        // position or satellites page opens and closes; not while keyed (GPS-32: the next look), nor while the bench
        // holds the module.
        if powered && !held {
            if let Some(sky) = set.step(now, demand.sky, mgr.awake(), keyed) {
                send_casic(CAS_NMEA, 1 + sky as u16);
            }
        }
        let mut fresh = false;
        while let Some(c) = uart::getc(UART_GPS) {
            if powered {
                bytes += 1;
                let done = nmea.feed(c);
                if done && nmea.last_is_gga() {
                    keep_gga(&nmea, now);
                }
                if done && !held && set_off(nmea.last()) {
                    match set.stray(now) {
                        Stray::Again => crate::bringup::line(b"gps: NMEA set not taken (GLL/VTG/ZDA still coming): again"),
                        Stray::GaveUp => crate::bringup::line(b"gps: NMEA set not taken 3 times: left until the next wake"),
                        Stray::None => {}
                    }
                }
                // TERM-36: the module's answer to a query is a TXT sentence; the first one is kept.
                #[cfg(feature = "bench")]
                if let Some(q) = query.as_mut().filter(|q| q.n == 0 && done) {
                    let s = nmea.last();
                    if s.len() >= 5 && s[2..5] == *b"TXT" {
                        let k = s.len().min(GGA_MAX - 1);
                        q.data[0] = b'$';
                        q.data[1..=k].copy_from_slice(&s[..k]);
                        q.n = k as u8 + 1;
                    }
                }
                sentences += done as u32;
                fresh |= done;
            }
        }
        #[cfg(feature = "bench")]
        if let Some(q) = query.take_if(|q| q.n > 0 || now.wrapping_sub(q.at) >= GPS_REPLY_MS) {
            crate::chan::GPS_ANS.signal(DebugAns::GpsSent { data: q.data, n: q.n });
            crate::chan::TERM_WAKE.signal(());
        }
        // The 5 s counts go on through a standby too (GPS-26): 0 sentences there is the module asleep.
        if powered && now.wrapping_sub(log_at) >= 5_000 {
            let f = nmea.fix(now);
            crate::bringup::linef(|b| {
                let _ = ufmt::uwrite!(b, "gps: {} sentences, {} bytes in 5 s; fix {}D, {} used, {} in view", sentences, bytes, f.fix, f.used, f.view);
            });
            (sentences, bytes, log_at) = (0, 0, now);
        }
        #[cfg(feature = "bench")]
        if powered && fresh && bench.holds() && !keyed {
            // TERM-36: the first sentence in a bench standby (it woke, or never slept), and a quiet standby's NMEA on.
            let (first, on) = bench.heard(now);
            if let Some(s) = first {
                crate::bringup::linef(|l| {
                    let _ = ufmt::uwrite!(l, "gps: bench: NMEA {} s into the standby", s);
                });
            }
            if let Some((id, arg)) = on {
                send_casic(id, arg);
                set.bench_sent(id, arg, now);
                crate::bringup::line(b"gps: bench: over, back to the power manager");
            }
        }
        if powered && fresh {
            let f = nmea.fix(now);
            if f.unix != 0 && f.unix != GPS_SYNC_UNIX.load(Relaxed) {
                latch_sync_pair(f.unix);
            }
            let ignored = if held {
                false
            } else if f.has_fix() {
                mgr.fix(now, ((f.speed_kn10 as u32 * 1852 + 5000) / 10_000) as u16)
            } else {
                mgr.sentence(now)
            };
            if ignored {
                // Decision 143: NMEA still coming 2 s into a standby; the plan holds to its end all the same.
                crate::bringup::line(b"gps: standby ignored: NMEA still coming");
            }
            let state = (f.has_fix(), f.fix);
            if state != last_state || now.wrapping_sub(fix_at) >= PUBLISH_MS {
                tx.send(f);
                (fix_at, last_state) = (now, state);
            }
            if now.wrapping_sub(sky_at) >= 1000 {
                sky.send(nmea.sky());
                sky_at = now;
            }
        }
        // PWR-23: every 50 ms while the module is on or wanted, every 500 ms while GPS is off (a key-up's PB10 goes
        // low at once through `pb10_low_now`; the release waits for the next look).
        Timer::after_millis(if power.powered || cfg.on { POLL_MS } else { OFF_POLL_MS }).await;
    }
}

/// The GPS seen from the UI task: the watches read into the store's GPS data, the status bar's GPS and clock
/// (GPS-6, GPS-7, GPS-8), the settings sent to the GPS task, and the arrival alarm (GPS-12).
pub struct GpsUi {
    pos: embassy_sync::watch::Receiver<'static, embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex, GpsFix, 3>,
    sky: embassy_sync::watch::Receiver<'static, embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex, GpsSky, 1>,
    cfg: Option<GpsCfg>,
    /// UTC seconds and the milliseconds they were true at: the clock runs on from the last GPS time even with
    /// the module off (no RTC on this board, GPS-23).
    clock: Option<(u32, u32)>,
    /// When the store's clock was last set (its log and message times), in UTC seconds.
    store_time: u32,
    arrival: ferrite_app::gpsdata::Arrival,
    /// Who needs a position, as last sent to the GPS task (GPS-26).
    demand: Option<GpsDemand>,
}

impl GpsUi {
    pub fn new() -> Self {
        GpsUi {
            pos: POSITION.receiver().unwrap(),
            sky: SKY.receiver().unwrap(),
            cfg: None,
            clock: None,
            store_time: 0,
            arrival: ferrite_app::gpsdata::Arrival::default(),
            demand: None,
        }
    }

    /// Once a turn of the UI loop.
    pub fn step(&mut self, app: &mut ferrite_app::App, store: &crate::store::Store, dev: &mut ferrite_core::msg::Device) {
        use ferrite_app::chan::ChannelStore;
        use ferrite_app::data::Data;
        let now = now_ms();
        let g = &store.extra.gps;
        if let Some(f) = self.pos.try_changed() {
            g.set_fix(f);
        }
        if let Some(s) = self.sky.try_changed() {
            g.set_sky(s);
        }
        g.tick(now);
        let set = app.settings();
        g.set_fixed(set.fixed_on.then_some(set.fixed));
        // GPS-1.5: Auto powers the module while something needs a position (and a minute after); the arrival
        // alarm armed is one of those needs, as it keeps the module running (GPS-28).
        let alarm = self.arrival.armed(ferrite_app::gpsdata::arrival_radius_m(app.settings()));
        ferrite_app::gpsdata::auto_step(app, store.data(), alarm, now);
        let cfg = app.gps_cfg(); // 0 6 1 (Off, Auto, On), off in Low Power Mode (PWR-24)
        if self.cfg != Some(cfg) {
            GPS_CFG.signal(cfg);
            self.cfg = Some(cfg);
        }
        (dev.gps, dev.sats) = g.state();
        // GPS-26 to GPS-28: who needs the module awake, sent when it changes (a beacon time that only moves with
        // the second's rounding isn't a change); and what the manager has it doing, for Status.
        let d = ferrite_app::gpsdata::demand(app, store.data(), alarm, now);
        let moved = |a: Option<u32>, b: Option<u32>| match (a, b) {
            (Some(a), Some(b)) => a.abs_diff(b) > 2000,
            (a, b) => a.is_some() != b.is_some(),
        };
        if self.demand.is_none_or(|o| GpsDemand { beacon_at: d.beacon_at, ..o } != d || moved(o.beacon_at, d.beacon_at)) {
            GPS_DEMAND.signal(d);
            self.demand = Some(d);
        }
        dev.gps_phase = GpsPhase::unpack(GPS_PHASE.load(Relaxed));
        // GPS-23: a clock set by hand runs from now; GPS time takes over when it comes, while 0 6 6 says GPS.
        if let Some(u) = app.take_clock_set() {
            self.clock = Some((u, now));
            self.store_time = 0;
        }
        let set = app.settings();
        // GPS-7: GPS time sets the clock while "from GPS" is on; the clock keeps running from it after.
        if set.clock_gps {
            if let Some(u) = g.utc_now() {
                self.clock = Some((u, now));
            }
        }
        let utc = self.clock.map(|(u, at)| u.wrapping_add(now.wrapping_sub(at) / 1000));
        // SAT-5: satellite mode's clock (GPS time, else by hand: "time ±1 min") and observer (the fix, else the
        // fixed position).
        let by_gps = set.clock_gps && g.utc_now().is_some();
        let here = g.fix().map(|f| (f.pos.lat, f.pos.lon, f.alt_m));
        app.sat_clock(utc, by_gps, here);
        let set = app.settings();
        match utc {
            Some(u) => {
                let (min, is_utc) = ferrite_app::gpsdata::local_clock(u, set.tz);
                (dev.clock_min, dev.utc) = (Some(min), is_utc);
                // The log's and messages' times (INBOX-6): set when first known and every 10 minutes after.
                if self.store_time == 0 || u.wrapping_sub(self.store_time) >= 600 {
                    store.set_time(u);
                    if self.store_time == 0 {
                        // APRS-4: the restored stations' rows stamped in UTC have real ages now (INBOX-6.1).
                        store.heard_rows(ferrite_app::aprs::HEARD_MAX as usize, &mut |s, seq| store.extra.aprs.restore(&s, seq, now / 1000));
                    }
                    self.store_time = u;
                }
            }
            None => (dev.clock_min, dev.utc) = (None, false),
        }
        // GPS-12: the waypoint being gone to is the one the Go to page shows; the alarm keeps it after.
        if let ferrite_app::Screen::Gps(s) = app.screen() {
            if s.page == ferrite_app::screens::gps::Page::Go {
                let wp = store.data().waypoint(s.cur.sel).map(|w| (s.cur.sel, w.pos));
                self.arrival.target(wp);
            }
        }
        let radius = ferrite_app::gpsdata::arrival_radius_m(set);
        // Only a fresh fix moves the alarm (GPS-2): the last one, kept while the module sleeps, already counted.
        if let Some(here) = g.fix().filter(|f| f.age_s != ferrite_app::gpsdata::FIXED_AGE && (f.age_s as u32) * 1000 < ferrite_app::gpsdata::STALE_MS).map(|f| f.pos) {
            if let Some((i, m)) = self.arrival.check(here, radius) {
                if let Some(w) = store.data().waypoint(i) {
                    arrived(app, store, &w, m);
                }
            }
        }
    }
}

/// GPS-12: a toast with the waypoint and the distance, an event in the log, and the tone when 0 7 5 4 is on.
fn arrived(app: &mut ferrite_app::App, store: &crate::store::Store, w: &ferrite_app::data::Waypoint, m: u32) {
    use ferrite_app::data::{Event, Src};
    let mut text = ferrite_ui::text::Buf::<64>::new();
    let _ = ufmt::uwrite!(text, "Arrived, {} m", m);
    app.notify(w.name.as_bytes(), text.as_bytes(), false);
    let mut name = ferrite_ui::text::Buf::<20>::new();
    name.set(w.name.as_bytes());
    let e = Event { src: Src::Gps, ago_s: 0, freq: 0, slot: 0, unread: true, id: 0, x: 0, y: 0, pos: Some(w.pos), name, text };
    store.log(&e);
    if app.settings().arrival_tone {
        let _ = crate::chan::RADIO.try_send(ferrite_core::msg::RadioCmd::Beep { hz: 880, ms: 400 });
    }
}
