//! TERM-33 `afsk-bk` (decision 134) and `sol950` / `sol950-cal`: BK4829 internal tone-generator transmit signals
//! timed non-blockingly by TMR5 (`crate::hal::edge`).
//!
//! Timing and the bus are split so the bus stays the Radio task's alone: TMR5's interrupt ([`on_interrupt`],
//! priority 4, above the Radio task's executor) counts the step, arms the next compare from the schedule and wakes
//! the Radio task on [`EDGE`]; the Radio task writes `REG_71` (`afsk-bk`) or `REG_70` (`sol950` when the gain step
//! changed). All modified registers are restored at unkey.
use super::*;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering::Relaxed};
use critical_section::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex as M;
use embassy_sync::signal::Signal;
use ferrite_core::txtest::{bk_gain, bk_tone, Arm, BkFlags, Signal as TxSig, Sol950Bk};
use ferrite_radio::chip::Sol950Saved;

/// TMR5's priority: the time driver's (4), above every executor.
const PRIO: u8 = 4;

enum SchedKind {
    Afsk(BkFlags),
    Sol950 { step: u32, per_step: u32 },
}

impl SchedKind {
    fn next_ticks(&mut self) -> u32 {
        match self {
            SchedKind::Afsk(f) => f.next_change(),
            SchedKind::Sol950 { step, per_step } => {
                *step = step.wrapping_add(1);
                step.wrapping_mul(*per_step)
            }
        }
    }
}

/// The schedule and the count at the start (TMR5's 16 bits), while playing.
static SCHED: Mutex<RefCell<Option<(SchedKind, u16)>>> = Mutex::new(RefCell::new(None));
/// Tone/gain steps made by the timer so far.
static CHANGES: AtomicU32 = AtomicU32::new(0);
/// A step was made: the Radio task's wake.
pub(super) static EDGE: Signal<M, ()> = Signal::new();

/// TMR5's interrupt: the step counted, the next one armed, the Radio task woken.
pub fn on_interrupt() {
    crate::hal::edge::ack();
    let next = critical_section::with(|cs| {
        SCHED
            .borrow_ref_mut(cs)
            .as_mut()
            .map(|(s, t0)| t0.wrapping_add(s.next_ticks() as u16))
    });
    if let Some(at) = next {
        crate::hal::edge::arm(at);
        CHANGES.fetch_add(1, Relaxed);
        EDGE.signal(());
    }
}

#[derive(Clone, Copy)]
enum RunMode {
    Afsk,
    Sol950 {
        gen: Sol950Bk,
        last_gain: u8,
        saved: Sol950Saved,
    },
}

/// `afsk-bk` or `sol950*` playing: the keyed chip, the steps processed, and steps coalesced.
#[derive(Clone, Copy)]
pub(super) struct BkRun {
    chip: u8,
    sent: u32,
    missed: u32,
    writes: u32,
    mode: RunMode,
}

impl Radio {
    /// After the key-up's plan: start `afsk-bk`, `sol950`, or `sol950-cal` on chip `n`.
    pub(super) fn bk_test_start(&mut self, n: u8, arm: &Arm) {
        if arm.signal.is_sol950() {
            let cal = arm.signal == TxSig::Sol950Cal;
            let gen = Sol950Bk::new(cal, arm.level);
            let g0 = gen.gain_at(0);
            let dev = arm.baud;
            let narrow = self.tx.narrow;
            let saved = radio_chip::sol950_bk_prime(&mut self.chip(n), dev, g0, narrow);
            crate::hal::time::delay_us(250); // quarter cycle of 1 kHz -> freeze Tone 1 near peak
            radio_chip::sol950_bk_freeze(&mut self.chip(n));
            CHANGES.store(0, Relaxed);
            EDGE.reset();
            let t0 = crate::hal::edge::start(PRIO);
            let per_step = crate::hal::edge::TICK_HZ / Sol950Bk::STEP_HZ; // exact 1,500 ticks (1.25 ms)
            let mut sk = SchedKind::Sol950 { step: 0, per_step };
            let first = t0.wrapping_add(sk.next_ticks() as u16);
            critical_section::with(|cs| *SCHED.borrow_ref_mut(cs) = Some((sk, t0)));
            crate::hal::edge::arm(first);
            self.tx.bk = Some(BkRun {
                chip: n,
                sent: 0,
                missed: 0,
                writes: 1,
                mode: RunMode::Sol950 { gen, last_gain: g0, saved },
            });
            log(b"txtest sol950 dev ", dev as u32);
        } else {
            let narrow = self.tx.narrow;
            radio_chip::afsk_bk_begin(&mut self.chip(n), bk_tone(0), bk_gain(arm.level), narrow);
            self.afsk_bk_timer(n);
            log(b"txtest afsk-bk gain ", bk_gain(arm.level) as u32);
        }
    }

    /// The timer from now, the chip already sending the space tone (TERM-34's flag steps set it with the rest of
    /// the step): the first change 7 bits on.
    pub(super) fn afsk_bk_timer(&mut self, n: u8) {
        CHANGES.store(0, Relaxed);
        EDGE.reset();
        let t0 = crate::hal::edge::start(PRIO);
        let mut sk = SchedKind::Afsk(BkFlags::new(crate::hal::edge::TICK_HZ));
        let first = t0.wrapping_add(sk.next_ticks() as u16);
        critical_section::with(|cs| *SCHED.borrow_ref_mut(cs) = Some((sk, t0)));
        crate::hal::edge::arm(first);
        self.tx.bk = Some(BkRun { chip: n, sent: 0, missed: 0, writes: 0, mode: RunMode::Afsk });
    }

    /// The Radio task woken by [`EDGE`]: update `REG_71` (`afsk-bk`) or `REG_70` (`sol950*`, only when gain changed).
    pub(super) fn afsk_bk_edge(&mut self) {
        let Some(mut run) = self.tx.bk else { return };
        let n = CHANGES.load(Relaxed);
        if n == run.sent {
            return;
        }
        run.missed += n - run.sent - 1;
        run.sent = n;
        let chip = run.chip;
        if self.has(chip) {
            match &mut run.mode {
                RunMode::Afsk => {
                    radio_chip::afsk_bk_tone(&mut self.chip(chip), bk_tone(n));
                    run.writes += 1;
                }
                RunMode::Sol950 { gen, last_gain, .. } => {
                    let g = gen.gain_at(n);
                    if g != *last_gain {
                        *last_gain = g;
                        radio_chip::sol950_bk_gain(&mut self.chip(chip), g);
                        run.writes += 1;
                    }
                }
            }
        }
        self.tx.bk = Some(run);
    }

    /// At the release (or any unkey): the timer stopped, then the tones/registers restored on the chip.
    pub(super) fn afsk_bk_stop(&mut self) {
        let Some((chip, mode)) = self.afsk_bk_halt_mode() else { return };
        if self.has(chip) {
            match mode {
                RunMode::Afsk => radio_chip::afsk_bk_end(&mut self.chip(chip)),
                RunMode::Sol950 { saved, .. } => radio_chip::sol950_bk_end(&mut self.chip(chip), &saved),
            }
        }
    }

    /// The timer stopped and the run's counts logged; the chip untouched (TERM-34's next step turns the tones off).
    pub(super) fn afsk_bk_halt(&mut self) -> Option<u8> {
        self.afsk_bk_halt_mode().map(|(c, _)| c)
    }

    fn afsk_bk_halt_mode(&mut self) -> Option<(u8, RunMode)> {
        let run = self.tx.bk.take()?;
        crate::hal::edge::stop();
        critical_section::with(|cs| *SCHED.borrow_ref_mut(cs) = None);
        EDGE.reset();
        log(b"txtest bk steps ", run.sent);
        log(b"txtest bk writes ", run.writes);
        if run.missed != 0 {
            log(b"txtest bk missed ", run.missed);
        }
        Some((run.chip, run.mode))
    }

    /// Whether `afsk-bk` or `sol950*` plays (the main wait listens to [`EDGE`] only then).
    pub(super) fn afsk_bk_on(&self) -> bool {
        self.tx.bk.is_some()
    }
}
