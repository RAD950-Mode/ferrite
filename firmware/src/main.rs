//! # Ferrite
//!
//! Free firmware (source public) for the Radtel RT-950 Pro. This is the start-up and the wiring: the safe state
//! before anything else, the vector table, three executors, and the six tasks (`tasks`), each the only owner of its hardware.
//!
//! | Executor | Priority | Tasks |
//! | --- | --- | --- |
//! | interrupt, IRQ 59 | 5 (highest of the three) | input sampler |
//! | interrupt, IRQ 57 | 6 | DSP |
//! | interrupt, IRQ 56 | 7 | Radio |
//! | thread (WFE when idle) | lowest | UI, Link, Storage, GPS |
//!
//! Above them: UART receive (3) and the time driver (4). Any fault, stray interrupt or panic ends in
//! `board::fault`: interrupts off, safe state, red LED, stop.
#![no_std]
#![no_main]

mod board;
mod bringup;
mod bus;
mod chan;
mod debug;
mod display;
#[allow(dead_code)] // the prototype's HAL, carried over whole; not every part has a user yet
mod hal;
mod report;
mod restart;
#[allow(dead_code)]
mod ring;
mod sat;
mod store;
mod tasks;

use core::sync::atomic::{AtomicU32, Ordering::Relaxed};
use cortex_m_rt::{entry, exception, ExceptionFrame};
use embassy_executor::{raw, InterruptExecutor, Spawner};
use ferrite_core::chipid;
use hal::irq::Irq;
use report::{UART_BT, UART_CPS, UART_GPS};

// ---- start-up ----

// Entered by a jump from the bootloader, not a reset. cortex-m-rt has loaded our stack pointer and VTOR;
// before RAM is initialised: interrupts masked, thread mode on MSP and privileged (CONTROL cleared), then the
// safe state and the bootloader's SysTick and interrupts off. before_ram touches no statics.
core::arch::global_asm!(
    ".global __pre_init",
    ".type __pre_init,%function",
    ".thumb_func",
    "__pre_init:",
    "cpsid i",
    "movs r0, #0",
    "msr control, r0",
    "isb",
    "b before_ram",
);

#[no_mangle]
extern "C" fn before_ram() {
    board::before_ram();
}

/// Model tag at 0x080033E0: the stock updater and bootloader check it (`btf.py` refuses images without it).
#[link_section = ".model_tag"]
#[used]
#[no_mangle]
static MODEL_TAG: [u8; 16] = *b"RT-950      \0\0\0\0";

/// The same tag at 0x080037E0: the bootloader compares it at every power-on and shows "Firmware Error!" instead
/// of starting the image when it differs (bootloader 0x08000654; found on the radio, Oct 11, 2026).
#[link_section = ".model_tag_boot"]
#[used]
#[no_mangle]
static MODEL_TAG_BOOT: [u8; 16] = *b"RT-950      \0\0\0\0";

/// Spare interrupt lines for the two interrupt executors: DMA2 channels 1 and 2, never clocked.
const IRQ_RADIO: u16 = 56;
const IRQ_DSP: u16 = 57;
const IRQ_INPUT: u16 = 59; // DMA2 channels 4 and 5, never clocked
// NVIC priorities (lower is more urgent): UART receive 3 (hal::uart), time driver 4, then the executors. The
// input sampler has its own, above the Radio task: a scan keeps the Radio task busy most of the time (each
// BK4829 register write is about 250 us at stock's bus speed), and on the radio that starved the keys (Oct 12, 2026).
// The DSP task above the Radio task (APRS-56.4, decision 357): a capture half was 2 ms (8 ms since decision 358), and the Radio task's runs
// (a tick's register reads, a retune: up to 8 ms) lost audio under it, about 4 halves a second while capturing,
// enough to break one packet in six on the bench (Nov 4, 2026). The DSP's own work is under 1 ms a half; the bus
// is the MCU's clock, so a stretched edge is harmless.
const PRIO_TIME: u8 = 4;
const PRIO_INPUT: u8 = 5;
const PRIO_DSP: u8 = 6;
const PRIO_RADIO: u8 = 7;

type Vector = unsafe extern "C" fn();

/// 96 peripheral vectors: the ones Ferrite uses, every other one the fault path.
#[link_section = ".vector_table.interrupts"]
#[no_mangle]
#[used]
static __INTERRUPTS: [Vector; 96] = {
    let mut v: [Vector; 96] = [stray; 96];
    v[hal::time_driver::IRQ as usize] = tmr3;
    #[cfg(all(feature = "tx", feature = "bench"))]
    {
        v[hal::edge::IRQ as usize] = tmr5;
    }
    v[IRQ_RADIO as usize] = radio_irq;
    v[IRQ_DSP as usize] = dsp_irq;
    v[IRQ_INPUT as usize] = input_irq;
    v[hal::irq::USART1 as usize] = usart1;
    v[hal::irq::USART3 as usize] = usart3;
    v[hal::irq::UART4 as usize] = uart4;
    v[hal::audio::IRQ_ADC_DMA as usize] = adc_dma;
    v[hal::audio::IRQ_DAC_DMA as usize] = dac_dma;
    v
};

extern "C" fn stray() {
    board::fault()
}
extern "C" fn tmr3() {
    hal::time_driver::on_interrupt()
}
/// TERM-33 `afsk-bk`'s bit timer (decision 134); a bench build's (TERM-38).
#[cfg(all(feature = "tx", feature = "bench"))]
extern "C" fn tmr5() {
    tasks::radio::afskbk::on_interrupt()
}
extern "C" fn usart1() {
    hal::uart::isr(UART_BT)
}
extern "C" fn usart3() {
    let before = hal::uart::stats(UART_GPS).rx_bytes;
    hal::uart::isr(UART_GPS);
    if hal::uart::stats(UART_GPS).rx_bytes != before {
        tasks::gps::on_rx_isr();
    }
}
extern "C" fn uart4() {
    hal::uart::isr(UART_CPS)
}
extern "C" fn adc_dma() {
    hal::audio::adc_isr()
}
extern "C" fn dac_dma() {
    hal::audio::dac_isr()
}

static RADIO_EXECUTOR: InterruptExecutor = InterruptExecutor::new();
static DSP_EXECUTOR: InterruptExecutor = InterruptExecutor::new();
static INPUT_EXECUTOR: InterruptExecutor = InterruptExecutor::new();

/// Cycles spent in the interrupt executors' tasks (Radio, DSP, input), so the cable log's draw lines can tell
/// drawing time from time the UI task was preempted (DISP-29; one executor preempting another counts twice).
pub static IRQ_CYCLES: AtomicU32 = AtomicU32::new(0);

/// SELF-9.4 (bench builds' cable log): the cycles each interrupt executor and the thread executor's polls took, so
/// the CPU figure can be split by who used it.
pub static RADIO_CYCLES: AtomicU32 = AtomicU32::new(0);
pub static DSP_CYCLES: AtomicU32 = AtomicU32::new(0);
pub static INPUT_CYCLES: AtomicU32 = AtomicU32::new(0);
pub static THREAD_CYCLES: AtomicU32 = AtomicU32::new(0);

#[inline(always)]
fn counted(f: impl FnOnce(), max_us: &AtomicU32, mine: &AtomicU32) {
    let c0 = hal::time::cycles();
    f();
    let c = hal::time::cycles().wrapping_sub(c0);
    IRQ_CYCLES.fetch_add(c, Relaxed);
    mine.fetch_add(c, Relaxed);
    // APRS-56.4 (bench builds, decision 363): only while capturing: a run then holds the DSP task off its halves
    #[cfg(not(feature = "bench"))]
    let _ = max_us;
    #[cfg(feature = "bench")]
    if hal::audio::CAPTURING.load(Relaxed) {
        let us = c / hal::clock::mhz().max(1);
        max_us.fetch_max(us, Relaxed);
        if us > 1_500 && !core::ptr::eq(max_us, &DSP_RUN_MAX_US) {
            LONG_RUNS.fetch_add(1, Relaxed);
        }
    }
}

/// APRS-56.4: the longest one run of the Radio and the input executors while the ADC captures, us since power-on,
/// and how many ran over 1.5 ms: the measure that moved the DSP task above the Radio task (decision 357); a run of
/// the input executor (above the DSP) longer than a capture half (8 ms) would still lose audio.
pub static LONG_RUNS: AtomicU32 = AtomicU32::new(0);
pub static RADIO_RUN_MAX_US: AtomicU32 = AtomicU32::new(0);
pub static INPUT_RUN_MAX_US: AtomicU32 = AtomicU32::new(0);
static DSP_RUN_MAX_US: AtomicU32 = AtomicU32::new(0);

extern "C" fn radio_irq() {
    // SAFETY: the executor's own interrupt handler, the only place it is polled.
    counted(|| unsafe { RADIO_EXECUTOR.on_interrupt() }, &RADIO_RUN_MAX_US, &RADIO_CYCLES)
}
extern "C" fn dsp_irq() {
    // SAFETY: as above.
    counted(|| unsafe { DSP_EXECUTOR.on_interrupt() }, &DSP_RUN_MAX_US, &DSP_CYCLES)
}
extern "C" fn input_irq() {
    // SAFETY: as above.
    counted(|| unsafe { INPUT_EXECUTOR.on_interrupt() }, &INPUT_RUN_MAX_US, &INPUT_CYCLES)
}

// With panic=immediate-abort a panic is an undefined instruction, so it arrives here as a HardFault.
#[exception]
unsafe fn HardFault(ef: &ExceptionFrame) -> ! {
    board::fault_at(Some(ef.pc()))
}

#[exception]
unsafe fn DefaultHandler(_: i16) {
    board::fault()
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    board::fault()
}

/// Microseconds the core spent asleep: the CPU-load figure.
pub static IDLE_US: AtomicU32 = AtomicU32::new(0);
/// SELF-9.3: the part of [`IDLE_US`] asleep at Low Power Mode's speed (24 MHz).
pub static IDLE_LOW_US: AtomicU32 = AtomicU32::new(0);

#[entry]
fn main() -> ! {
    board::init();
    // SAFE-8.1: a restart after a fault or a hang is noted for the UI task; one too many in a row stops here.
    if !restart::at_start(hal::sys::reset_cause()) {
        board::stopped();
    }
    hal::time::us_init();
    // Transmit (in the tx build) also needs the expected MCU; receive runs regardless.
    let _mcu_ok = hal::sys::idcode() == chipid::EXPECTED_PID;

    hal::sys::paint_stack();
    hal::sys::wake_on_pending();
    hal::time_driver::init(PRIO_TIME);
    hal::irq::set_priority(IRQ_RADIO, PRIO_RADIO);
    hal::irq::set_priority(IRQ_DSP, PRIO_DSP);
    hal::irq::set_priority(IRQ_INPUT, PRIO_INPUT);
    RADIO_EXECUTOR.start(Irq(IRQ_RADIO)).spawn(tasks::radio::task().unwrap());
    let dsp = DSP_EXECUTOR.start(Irq(IRQ_DSP));
    dsp.spawn(tasks::dsp::task().unwrap());
    INPUT_EXECUTOR.start(Irq(IRQ_INPUT)).spawn(tasks::input::task().unwrap());
    // SAFETY: start-up is done; interrupts may run from here.
    unsafe { cortex_m::interrupt::enable() };
    run_thread_executor(|s| {
        s.spawn(tasks::ui::task().unwrap());
        s.spawn(tasks::storage::task().unwrap());
        s.spawn(tasks::link::task().unwrap());
        s.spawn(tasks::gps::task().unwrap());
    })
}

/// Embassy's thread-mode executor with idle accounting: poll the ready tasks, then sleep until the next event,
/// with interrupts masked (WFE still wakes on them: SEVONPEND) so the time measured is purely asleep.
fn run_thread_executor(init: impl FnOnce(Spawner)) -> ! {
    // embassy-executor's cortex-m pender treats this context as the thread executor: it signals with SEV.
    const THREAD_CONTEXT: *mut () = usize::MAX as *mut ();
    let executor: &'static raw::Executor =
        cortex_m::singleton!(: raw::Executor = raw::Executor::new(THREAD_CONTEXT)).unwrap();
    init(executor.spawner());
    // SAFE-8 (decision 364): the watchdog from here, fed after every poll; SAFE-8.1's count cleared after a minute.
    hal::wdt::start();
    let mut settled = false;
    loop {
        let c0 = hal::time::cycles();
        // SAFETY: polled only here and never re-entered (the pender only sets the event register).
        unsafe { executor.poll() };
        THREAD_CYCLES.fetch_add(hal::time::cycles().wrapping_sub(c0), Relaxed); // SELF-9.4 (interrupts in it too)
        hal::wdt::feed();
        if !settled && embassy_time::Instant::now().as_millis() >= restart::SETTLED_MS {
            restart::settled();
            settled = true;
        }
        cortex_m::interrupt::disable();
        let t0 = hal::time::us16();
        cortex_m::asm::wfe(); // returns at once if a task was woken since the poll
        let t1 = hal::time::us16();
        // SAFETY: re-enables what the line above masked; no critical section is open here.
        unsafe { cortex_m::interrupt::enable() };
        let slept = t1.wrapping_sub(t0) as u32;
        IDLE_US.fetch_add(slept, Relaxed);
        if hal::clock::is_low() {
            IDLE_LOW_US.fetch_add(slept, Relaxed); // SELF-9.3
        }
    }
}
