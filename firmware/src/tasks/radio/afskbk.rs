//! TERM-33 `afsk-bk` (decision 134) and `rad950` / `rad950-cal`: BK4829 internal tone-generator transmit signals
//! timed non-blockingly by TMR5 (`crate::hal::edge`).
//!
//! Timing and the bus are split so the bus stays the Radio task's alone: TMR5's interrupt ([`on_interrupt`],
//! priority 4, above the Radio task's executor) counts the step, arms the next compare from the schedule and wakes
//! the Radio task on [`EDGE`]; the Radio task writes `REG_71` (`afsk-bk`) or `REG_70` (`rad950` when the gain step
//! changed). All modified registers are restored at unkey.
use super::*;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering::Relaxed};
use critical_section::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex as M;
use embassy_sync::signal::Signal;
#[cfg(feature = "bench")]
use ferrite_core::txtest::{bk_gain, bk_tone, Arm, BkFlags, Signal as TxSig, Rad950Bk};
use ferrite_radio::chip::Rad950Saved;

/// TMR5's priority: the time driver's (4), above every executor.
const PRIO: u8 = 4;
/// RAD950 substep rate: 64 substeps per 80 ms symbol = 800 Hz (1,500 TMR5 ticks at 1.2 MHz).
const RAD950_STEP_HZ: u32 = 800;
/// Short 5 ms lead (4 * 1.25 ms) after PA ramp before symbol 0 of a live RAD950 burst.
const RAD950_LIVE_LEAD_STEPS: u32 = 4;

enum SchedKind {
    #[cfg(feature = "bench")]
    Afsk(BkFlags),
    Rad950 { step: u32, per_step: u32 },
}

impl SchedKind {
    fn next_ticks(&mut self) -> u32 {
        match self {
            #[cfg(feature = "bench")]
            SchedKind::Afsk(f) => f.next_change(),
            SchedKind::Rad950 { step, per_step } => {
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
    #[cfg(feature = "bench")]
    Afsk,
    #[cfg(feature = "bench")]
    Rad950 {
        cal: bool,
        last_step: u32,
        saved: Rad950Saved,
    },
    Rad950Live {
        tones: [u8; ferrite_core::rad950::urad950::N_SYMBOLS],
        last_step: u32,
        saved: Rad950Saved,
    },
}

/// `afsk-bk` or `rad950*` playing: the keyed chip, the steps processed, and steps coalesced.
#[derive(Clone, Copy)]
pub(super) struct BkRun {
    chip: u8,
    sent: u32,
    missed: u32,
    writes: u32,
    mode: RunMode,
}

impl Radio {
    /// Start a live 110-symbol RAD950 burst on chip `n` via direct BK4829 PLL stepping (`REG_38/39` + `REG_30`
    /// VCO toggle), timed by TMR5 at 80 ms/symbol (`64 * 1.25 ms`).
    pub(super) fn rad950_live_start(&mut self, n: u8, tones: [u8; ferrite_core::rad950::urad950::N_SYMBOLS]) {
        let narrow = self.tx.narrow;
        let saved = radio_chip::rad950_bk_prime(&mut self.chip(n), 0, 0, narrow);
        CHANGES.store(0, Relaxed);
        EDGE.reset();
        let t0 = crate::hal::edge::start(PRIO);
        let per_step = crate::hal::edge::TICK_HZ / RAD950_STEP_HZ; // exact 1,500 ticks (1.25 ms)
        let mut sk = SchedKind::Rad950 { step: 0, per_step };
        let first = t0.wrapping_add(sk.next_ticks() as u16);
        critical_section::with(|cs| *SCHED.borrow_ref_mut(cs) = Some((sk, t0)));
        crate::hal::edge::arm(first);
        self.tx.bk = Some(BkRun {
            chip: n,
            sent: 0,
            missed: 0,
            writes: 1,
            mode: RunMode::Rad950Live { tones, last_step: 0, saved },
        });
        log(b"rad950 tx start", 0);
    }

    /// After the key-up's plan: start `afsk-bk`, `rad950`, or `rad950-cal` on chip `n`.
    #[cfg(feature = "bench")]
    pub(super) fn bk_test_start(&mut self, n: u8, arm: &Arm) {
        if arm.signal.is_rad950() {
            let cal = arm.signal == TxSig::Rad950Cal;
            let narrow = self.tx.narrow;
            let saved = radio_chip::rad950_bk_prime(&mut self.chip(n), arm.baud, 0, narrow);
            CHANGES.store(0, Relaxed);
            EDGE.reset();
            let t0 = crate::hal::edge::start(PRIO);
            let per_step = crate::hal::edge::TICK_HZ / Rad950Bk::STEP_HZ; // exact 1,500 ticks (1.25 ms)
            let mut sk = SchedKind::Rad950 { step: 0, per_step };
            let first = t0.wrapping_add(sk.next_ticks() as u16);
            critical_section::with(|cs| *SCHED.borrow_ref_mut(cs) = Some((sk, t0)));
            crate::hal::edge::arm(first);
            self.tx.bk = Some(BkRun {
                chip: n,
                sent: 0,
                missed: 0,
                writes: 1,
                mode: RunMode::Rad950 { cal, last_step: 0, saved },
            });
            log(b"txtest rad950 dev ", arm.baud as u32);
        } else {
            let narrow = self.tx.narrow;
            radio_chip::afsk_bk_begin(&mut self.chip(n), bk_tone(0), bk_gain(arm.level), narrow);
            self.afsk_bk_timer(n);
            log(b"txtest afsk-bk gain ", bk_gain(arm.level) as u32);
        }
    }

    /// The timer from now, the chip already sending the space tone (TERM-34's flag steps set it with the rest of
    /// the step): the first change 7 bits on.
    #[cfg(feature = "bench")]
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

    /// The Radio task woken by [`EDGE`]: step `REG_71` (`afsk-bk`) or `REG_38/39` (`rad950*`).
    /// Returns `true` when a live RAD950 burst has completed all 110 symbols and is ready to unkey.
    pub(super) fn afsk_bk_edge(&mut self) -> bool {
        let Some(mut run) = self.tx.bk else { return false };
        let n = CHANGES.load(Relaxed);
        if n == run.sent {
            return false;
        }
        run.missed += n - run.sent - 1;
        run.sent = n;
        let chip = run.chip;
        let mut finished = false;
        if self.has(chip) {
            match &mut run.mode {
                #[cfg(feature = "bench")]
                RunMode::Afsk => {
                    radio_chip::afsk_bk_tone(&mut self.chip(chip), bk_tone(n));
                    run.writes += 1;
                }
                #[cfg(feature = "bench")]
                RunMode::Rad950 { cal, last_step, saved } => {
                    if *cal {
                        // 500 ms per step: steps 0..4 test bit-15-only VCO toggle (0x43FA->0xC3FA), 4..9 full tx_on
                        let idx = (n / Rad950Bk::CAL_HOLD_STEPS).min(8);
                        if idx != *last_step {
                            *last_step = idx;
                            let (off_hz, bit15_only) = if idx < 4 { (idx * 500, true) } else { ((idx - 4) * 500, false) };
                            let hz = saved.base_hz.saturating_add(off_hz);
                            let narrow = saved.narrow;
                            radio_chip::rad950_bk_freq(&mut self.chip(chip), hz, bit15_only, narrow);
                            run.writes += 1;
                        }
                    } else if n >= Rad950Bk::LEAD_STEPS {
                        // 80 ms per symbol (64 * 1.25 ms), 10 Hz resolution (50 Hz/tone -> 0, 50, 100, 150 Hz)
                        let sym = ((n - Rad950Bk::LEAD_STEPS) / 64) as usize;
                        if sym < ferrite_core::txtest::RAD950_REF_TONES.len() {
                            let tone = ferrite_core::txtest::RAD950_REF_TONES[sym] as u32;
                            let idx = tone + 1;
                            if idx != *last_step {
                                *last_step = idx;
                                let hz = saved.base_hz.saturating_add(tone * 50);
                                let narrow = saved.narrow;
                                radio_chip::rad950_bk_freq(&mut self.chip(chip), hz, false, narrow);
                                run.writes += 1;
                            }
                        }
                    }
                }
                RunMode::Rad950Live { tones, last_step, saved } => {
                    if n >= RAD950_LIVE_LEAD_STEPS {
                        let sym = ((n - RAD950_LIVE_LEAD_STEPS) / 64) as usize;
                        if sym < tones.len() {
                            let tone = tones[sym] as u32;
                            let idx = tone + 1;
                            if idx != *last_step {
                                *last_step = idx;
                                let hz = saved.base_hz.saturating_add(tone * 50);
                                let narrow = saved.narrow;
                                radio_chip::rad950_bk_freq(&mut self.chip(chip), hz, false, narrow);
                                run.writes += 1;
                            }
                        } else {
                            finished = true;
                        }
                    }
                }
            }
        }
        self.tx.bk = Some(run);
        finished
    }

    /// At the release (or any unkey): the timer stopped, then the tones/registers restored on the chip.
    pub(super) fn afsk_bk_stop(&mut self) {
        let Some((chip, mode)) = self.afsk_bk_halt_mode() else { return };
        if self.has(chip) {
            match mode {
                #[cfg(feature = "bench")]
                RunMode::Afsk => radio_chip::afsk_bk_end(&mut self.chip(chip)),
                #[cfg(feature = "bench")]
                RunMode::Rad950 { saved, .. } => radio_chip::rad950_bk_end(&mut self.chip(chip), &saved),
                RunMode::Rad950Live { saved, .. } => radio_chip::rad950_bk_end(&mut self.chip(chip), &saved),
            }
        }
    }

    /// The timer stopped and the run's counts logged; the chip untouched (TERM-34's next step turns the tones off).
    #[cfg(feature = "bench")]
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

    /// Whether `afsk-bk` or `rad950*` plays (the main wait listens to [`EDGE`] only then).
    pub(super) fn afsk_bk_on(&self) -> bool {
        self.tx.bk.is_some()
    }
}
