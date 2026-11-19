//! AT32F403A peripherals (carried over from the Rust prototype, `fw-rs/src/hal`) (STM32F1-compatible register layout). The only module that touches registers,
//! apart from the start-up code. Raw volatile access like fw/hal; a real port would generate a PAC from
//! Artery's SVD with svd2rust. No Artery support exists in Embassy, so the time driver is ours too.
use core::ptr::{read_volatile, write_volatile};

pub mod adc;
pub mod audio;
pub mod backlight;
pub mod clock;
#[cfg(feature = "tx")]
pub mod edge;
pub mod gpio;
pub mod i2c;
pub mod irq;
#[cfg(feature = "tx")]
pub mod pa;
mod regs;
pub mod spi;
pub mod sys;
pub mod time;
pub mod time_driver;
pub mod tone;
pub mod uart;
pub mod wdt;

// SAFETY (all four): only called with the address of a memory-mapped peripheral or core register (the
// constants in regs.rs); a volatile access there cannot alias any Rust object.
fn rd(a: u32) -> u32 {
    unsafe { read_volatile(a as *const u32) }
}
fn wr(a: u32, v: u32) {
    unsafe { write_volatile(a as *mut u32, v) }
}
fn rd16(a: u32) -> u16 {
    unsafe { read_volatile(a as *const u16) }
}
fn wr8(a: u32, v: u8) {
    unsafe { write_volatile(a as *mut u8, v) }
}

/// Read-modify-write: clear then set bits.
fn modify(a: u32, clear: u32, set: u32) {
    wr(a, rd(a) & !clear | set);
}
