//! Core, AHB and APB clocks: stock's tree (V0.27 clock setup 0x08022694, analysis/peripherals_irq.md). The 8 MHz
//! crystal (HEXT) x 15 through the PLL = 120 MHz SYSCLK; AHB /1 = 120 MHz, APB1 /2 = 60 MHz, APB2 /2 = 60 MHz;
//! timers on either APB run at twice it, 120 MHz.
//!
//! PWR-24.9 (decision 250): the speed is a runtime state with two values, [`NORMAL`] (the build's) and [`LOW`]
//! (24 MHz, Low Power Mode). The numbers come from [`ferrite_core::clocktree`] (host-tested at both speeds); every
//! driver reads the current tree when it programs a divider ([`tree`]), and [`set_low`] switches speed and
//! re-times whatever is running.
//!
//! For the power bench (Oct 21, 2026) an image can be built at another fixed speed with a `clock-*` feature
//! (`clock-24`, `clock-48`, `clock-96`, `clock-144`, `clock-192`, `clock-240`): it sets [`NORMAL`]. Above 120 MHz
//! the APBs divide by 4, so at 240 MHz every peripheral and timer runs exactly as at 120 and only the core and AHB
//! are faster. Low Power Mode still drops to 24 MHz (at `clock-24` there is nothing to switch).
use super::regs::*;
use super::{modify, rd, wr};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use ferrite_core::clocktree::{switch_steps, Elapsed, Step, Tree};

#[cfg(not(any(feature = "clock-24", feature = "clock-48", feature = "clock-96", feature = "clock-144", feature = "clock-192", feature = "clock-240")))]
const MULT: u32 = 15;
#[cfg(feature = "clock-24")]
const MULT: u32 = 3;
#[cfg(feature = "clock-48")]
const MULT: u32 = 6;
#[cfg(feature = "clock-96")]
const MULT: u32 = 12;
#[cfg(feature = "clock-144")]
const MULT: u32 = 18;
#[cfg(feature = "clock-192")]
const MULT: u32 = 24;
#[cfg(feature = "clock-240")]
const MULT: u32 = 30;

/// The build's speed: 120 MHz, or a `clock-*` bench speed.
pub const NORMAL: Tree = Tree::new(MULT);
/// Low Power Mode's speed, 24 MHz (never above the build's).
pub const LOW: Tree = if ferrite_core::clocktree::LOW.mult < MULT { ferrite_core::clocktree::LOW } else { NORMAL };
/// PWR-26.8, SAT-6: the top clock for a burst of work (satellite passes predicted): 240 MHz, the APBs at /4 so every
/// peripheral and timer runs as at 120 MHz. A `clock-*` bench build stays at its own speed.
pub const TOP: Tree = if MULT == 15 { Tree::new(30) } else { NORMAL };
const _: () = assert!(NORMAL.valid() && LOW.valid() && TOP.valid());
const _: () = assert!(MULT != 15 || NORMAL.pll_cfg() == 0x8035_0000); // stock's value at 120 MHz

/// The current tree's multiplier (a named static: interrupt context reads it, and it stays in fast flash).
static MULT_NOW: AtomicU32 = AtomicU32::new(MULT);
/// The time driver's part tick carried from the last switch (picoseconds, [`Elapsed`]).
static CARRY_PS: AtomicU32 = AtomicU32::new(0);
/// The last switch's time with interrupts held off, microseconds, and how many switches so far (the cable log).
pub static LAST_SWITCH_US: AtomicU32 = AtomicU32::new(0);
pub static SWITCHES: AtomicU32 = AtomicU32::new(0);
/// PWR-26.6: the longest a switch has held interrupts off since power on, microseconds.
pub static MAX_SWITCH_US: AtomicU32 = AtomicU32::new(0);
/// PWR-26.4: tasks waiting for the build's speed before they start audio ([`for_audio`]); the UI task's governor
/// counts it as a reason.
pub static AUDIO_WAITING: AtomicU32 = AtomicU32::new(0);
/// PWR-26.4: the governor would raise the speed for audio now (Dynamic, out of Low Power Mode); set by the UI task.
pub static AUDIO_RAISES: AtomicBool = AtomicBool::new(false);

/// The tree the core runs on now.
#[inline]
pub fn tree() -> Tree {
    Tree::new(MULT_NOW.load(Relaxed))
}
#[inline]
pub fn sysclk_hz() -> u32 {
    tree().sysclk()
}
#[inline]
pub fn mhz() -> u32 {
    tree().mhz()
}
/// Either APB (they always divide alike).
#[inline]
pub fn apb_hz() -> u32 {
    tree().apb_hz()
}
#[inline]
pub fn tmr_hz() -> u32 {
    tree().tmr_hz()
}

/// True while in the Low Power Mode speed (false at a `clock-24` build, where the two are the same).
pub fn is_low() -> bool {
    tree().mult < NORMAL.mult
}

/// CRM_MISC2 bits 4-5: AUTO_STEP_EN, set by stock around the switch to the PLL so the core steps up gradually.
const CRM_MISC2: u32 = CRM + 0x54;
const AUTO_STEP: u32 = 3 << 4;
/// CRM_CFG: PLLRC (16), PLLHEXTDIV (17), PLLMULT (18-21 and 29-30), PLLRANGE (31): stock clears these with
/// 0x1FC0FFFF and sets 0x80350000 (HEXT undivided, multiplier code 13 = x15, range above 72 MHz).
const PLL_CLEAR: u32 = 0x1FC0_FFFF;
const APB_MASK: u32 = 7 << 11 | 7 << 8;
/// The PLL lock wait's bound in polls (only reached if the PLL never locks: then the fault path).
const PLL_POLLS: u32 = 0x10_0000;

/// One step of a switch to `to` ([`switch_steps`] gives the order; the core tests it).
fn step(s: Step, to: Tree) {
    match s {
        Step::SysclkHext => {
            modify(CRM_CFG, CRM_CFG_SCLKSEL_MASK, 1);
            while rd(CRM_CFG) & CRM_CFG_SCLKSTS_MASK != 1 << 2 {}
        }
        Step::ApbUndivided => modify(CRM_CFG, APB_MASK, 0),
        Step::PllOff => modify(CRM_CTRL, CRM_CTRL_PLLEN, 0),
        Step::ApbDividers => modify(CRM_CFG, APB_MASK, to.apb_cfg()),
        Step::PllConfig => wr(CRM_CFG, rd(CRM_CFG) & PLL_CLEAR | to.pll_cfg()),
        Step::PllOnWait => {
            modify(CRM_CTRL, 0, CRM_CTRL_PLLEN);
            let mut n = 0u32;
            while rd(CRM_CTRL) & CRM_CTRL_PLLSTBL == 0 {
                // Decision 362: the lock takes most of a switch's 660 us, interrupts off: the ports' bytes taken
                // as they come (a UART holds one).
                super::uart::poll_rx();
                n += 1;
                if n == PLL_POLLS {
                    panic!("PLL did not lock");
                }
            }
        }
        Step::AutoStepOn => modify(CRM_MISC2, 0, AUTO_STEP),
        Step::SysclkPll => {
            modify(CRM_CFG, CRM_CFG_SCLKSEL_MASK, 2);
            while rd(CRM_CFG) & CRM_CFG_SCLKSTS_MASK != 2 << 2 {}
        }
        Step::AutoStepOff => modify(CRM_MISC2, AUTO_STEP, 0),
    }
}

/// The app is entered by a jump from the bootloader, not a reset, so the clock tree is whatever the bootloader
/// left: back to HICK first (the PLL can only be set up while not in use), then stock's sequence at [`NORMAL`].
///
/// If the crystal doesn't start (stock waits 0x3000 polls), the image can't keep its timings: it stops here
/// with a panic, which the fault path shows (red LED, `*** FAULT` on the cable).
pub fn init() {
    // Back to the reset state: SCLK from HICK, PLL off, AHB/APB undivided.
    modify(CRM_CTRL, 0, CRM_CTRL_HICKEN);
    while rd(CRM_CTRL) & CRM_CTRL_HICKSTBL == 0 {}
    modify(CRM_CFG, CRM_CFG_SCLKSEL_MASK, 0);
    while rd(CRM_CFG) & CRM_CFG_SCLKSTS_MASK != 0 {}
    modify(CRM_CFG, CRM_CFG_DIV_MASK, 0);
    modify(CRM_CTRL, CRM_CTRL_PLLEN, 0);

    // Stock: HEXT on, wait for it.
    modify(CRM_CTRL, 0, CRM_CTRL_HEXTEN);
    let mut n = 0u32;
    while rd(CRM_CTRL) & CRM_CTRL_HEXTSTBL == 0 {
        n += 1;
        if n == 0x3000 * 16 {
            panic!("8 MHz crystal did not start");
        }
    }
    // APB dividers, then the PLL from HEXT x MULT, and the switch with the automatic step, as stock does.
    for s in switch_steps() {
        if !matches!(s, Step::SysclkHext | Step::PllOff) {
            step(s, NORMAL);
        }
    }
    MULT_NOW.store(MULT, Relaxed);
    // PWR-23: HICK, the internal 48 MHz oscillator, was only the clock until the switch: off (a reset, the
    // bootloader's way in, turns it back on). A speed switch goes through HEXT, never HICK.
    modify(CRM_CTRL, CRM_CTRL_HICKEN, 0);
}

/// True once `init` has run: SYSCLK from the PLL.
pub fn on_pll() -> bool {
    rd(CRM_CFG) & CRM_CFG_SCLKSTS_MASK == 2 << 2
}

/// PWR-24.9: to Low Power Mode's speed (`low`) or back to the build's. False when already there. Call from
/// thread mode only (the UI task): then no interrupt-executor task is part way through anything (they run inside
/// their interrupts, which thread mode never interrupts), and no other thread-mode task is either (they only
/// give way at an await). So nothing is mid-transfer: the display bus, SPI2, the BK4829 and SI4732 buses are
/// all between transactions, and no busy wait has its cycle count from the old speed.
///
/// 1. The UARTs that are on finish sending (their rate changes under them).
/// 2. The time driver waits for a fresh tick (interrupts on) away from its half-period marks.
/// 3. Interrupts off: TMR3 stopped, the PLL moved (through the crystal, [`switch_steps`]), every running
///    peripheral re-timed for the new tree, TMR3 moved on by the ticks the switch took (counted in cycles at
///    each speed; the rest carried) and started again. Interrupts on.
///
/// Interrupts are held off for the PLL's lock and the register work (logged in [`LAST_SWITCH_US`]); a byte
/// arriving on the cable meanwhile can be lost (one UART byte is 87 us at 115200).
pub fn set_low(low: bool) -> bool {
    set_tree(if low { LOW } else { NORMAL })
}

/// [`set_low`]'s switch to any of the three trees ([`LOW`], [`NORMAL`], [`TOP`]); the same rules.
pub fn set_tree(to: Tree) -> bool {
    let from = tree();
    if to == from {
        return false;
    }
    super::uart::flush_all();
    loop {
        let seen = super::time_driver::wait_safe_tick(); // interrupts on
        if cortex_m::interrupt::free(|_| switch(seen, from, to)) {
            break;
        }
    }
    SWITCHES.fetch_add(1, Relaxed);
    true
}

/// [`set_low`]'s work, interrupts off. False (nothing done) when TMR3 has already moved past `seen`, the tick
/// waited for (an interrupt came between): the caller waits for the next one.
fn switch(seen: u16, from: Tree, to: Tree) -> bool {
    let c0 = super::time::cycles();
    let Some(cnt) = super::time_driver::freeze(seen) else {
        return false;
    };
    // A task in an interrupt may have written to a port since `set_low`'s flush (the cable log): at most a byte
    // or two still going out, the bytes coming in taken meanwhile (decision 362).
    super::uart::flush_all_rx();
    // The cycle counter runs at the old speed until SYSCLK is the crystal, at 8 MHz until it is the PLL again,
    // then at the new speed: each part counted at its own rate.
    let (mut c1, mut c2) = (c0, c0);
    for s in switch_steps() {
        step(s, to);
        match s {
            Step::SysclkHext => c1 = super::time::cycles(),
            // Decision 362: the ports at their rates from the crystal until the PLL is back.
            Step::ApbUndivided => super::uart::retime(ferrite_core::clocktree::CRYSTAL),
            Step::SysclkPll => c2 = super::time::cycles(),
            _ => {}
        }
    }
    MULT_NOW.store(to.mult, Relaxed);
    retime(to);
    let c3 = super::time::cycles();
    let mut e = Elapsed { carry_ps: CARRY_PS.load(Relaxed) as u64 };
    let ticks = e.ticks(c1.wrapping_sub(c0), from.sysclk(), c2.wrapping_sub(c1), c3.wrapping_sub(c2), to.sysclk());
    CARRY_PS.store(e.carry_ps as u32, Relaxed);
    super::time_driver::thaw(cnt, ticks, to);
    let us = c1.wrapping_sub(c0) / from.mhz()
        + c2.wrapping_sub(c1) / (ferrite_core::clocktree::HEXT_HZ / 1_000_000)
        + super::time::cycles().wrapping_sub(c2) / to.mhz();
    LAST_SWITCH_US.store(us, Relaxed);
    MAX_SWITCH_US.fetch_max(us, Relaxed);
    true
}

/// PWR-26.4: before a task starts the audio capture or playback: at 24 MHz, ask the UI task's governor for the
/// build's speed and wait for it (the switch never happens while audio runs, so it must come first). Returns at
/// once at the build's speed, in Low Power Mode or with Fixed (the governor won't raise it there), and after
/// [`AUDIO_WAIT_MS`] whatever happened (audio then starts at 24 MHz; its timers follow the clock).
pub async fn for_audio() {
    if !is_low() || !AUDIO_RAISES.load(Relaxed) {
        return;
    }
    AUDIO_WAITING.fetch_add(1, Relaxed);
    crate::chan::TERM_WAKE.signal(()); // the UI task's next turn now
    let t0 = embassy_time::Instant::now();
    while is_low() && AUDIO_RAISES.load(Relaxed) && t0.elapsed() < embassy_time::Duration::from_millis(AUDIO_WAIT_MS) {
        embassy_time::Timer::after_millis(2).await;
    }
    AUDIO_WAITING.fetch_sub(1, Relaxed);
}

/// Decision 360: between a redraw's bands (`ferrite_ui::canvas`'s hook, the UI task in thread mode with nothing
/// mid-transfer, as [`set_low`] needs): an audio task waiting in [`for_audio`] gets the build's speed now, not after
/// the redraw (at 24 MHz 100 ms and more, a packet's flags gone). The governor keeps it (its audio reason).
pub fn raise_for_audio() {
    if AUDIO_WAITING.load(Relaxed) != 0 && is_low() && AUDIO_RAISES.load(Relaxed) && !super::audio::busy() {
        set_tree(NORMAL);
    }
}

/// PWR-26.4: [`for_audio`]'s longest wait: the UI task turns at least every 250 ms, a redraw at 24 MHz included.
pub const AUDIO_WAIT_MS: u64 = 500;

/// Everything clocked from the tree that is running, set again for `t` (interrupts are off). Things that are
/// off pick up the new tree when they start (each reads [`tree`]).
fn retime(t: Tree) {
    super::time::retime(t);
    super::uart::retime(t);
    super::spi::retime(t);
    super::adc::retime(t);
    super::audio::retime(t);
    super::tone::retime();
    super::backlight::retime(t);
    #[cfg(feature = "tx")]
    super::edge::retime(t);
}
