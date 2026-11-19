//! Ferrite's SPI-flash store (roadmap 1d; `docs/firmware-design.md`, "SPI flash layout" and "Records"): a plain
//! fixed memory map, no file system.
//!
//! - The layout header: magic and layout version (a blank or foreign chip is formatted: first boot).
//! - [`settings`]: the settings and slots, two fixed copies (A/B); a save overwrites the older one.
//! - [`records`]: channel n, zone z and range preset p each at a fixed address, erased = empty.
//! - [`scratch`]: changing a record that is already written rewrites its 4 KB sector through one scratch
//!   sector, so a power cut leaves the old sector or the new one.
//! - [`store`]: [`FlashStore`], the UI's [`ferrite_app::chan::ChannelStore`] and [`ferrite_app::data::Data`]
//!   on top, the 2-second save timer, and on the radio the queue the Storage task works through.
//!
//! Everything goes through the [`Nor`] trait: the radio's W25Q and the simulator's [`sim::SimFlash`] (which can
//! cut the power at any byte). Programs and erases only start an operation; the store waits for it with
//! [`Nor::pause`], so on the radio the UI keeps running while a sector erases. Nothing here writes outside
//! [`ferrite_core::layout::WRITABLE`]: the radio's driver refuses it, and the simulated flash panics on it.
//!
//! RAM: no copy of the channels; one zone byte per channel number (1,000 B) for the lists, built at mount.
#![cfg_attr(not(feature = "std"), no_std)]

pub mod io;
pub mod logs;
pub mod members;
pub mod records;
pub mod restore;
pub mod sats;
pub mod scratch;
pub mod settings;
pub mod sol950;
pub mod ssb;
pub mod store;

#[cfg(feature = "std")]
pub mod sim;

use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

pub use store::FlashStore;

/// The erase unit.
pub const SECTOR: u32 = 4096;
/// The program unit (a program never crosses one).
pub const PAGE: u32 = 256;

/// The flash failed (or, in the simulator, the power went).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NorError;

pub type Result<T = ()> = core::result::Result<T, NorError>;

/// NOR flash: erased bytes read 0xFF, a program only turns bits from 1 to 0, an erase sets a whole 4 KB sector
/// back to 0xFF. A program or an erase is started and runs on in the chip; [`Nor::busy`] says when it is done.
#[allow(async_fn_in_trait)] // used inside one executor; no Send bound wanted
pub trait Nor {
    /// Reads at `addr`. On the radio a read during a program or erase suspends it, reads, and resumes it.
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result;
    /// Starts programming `data` (within one [`PAGE`]) at `addr`; the bytes are erased.
    fn start_program(&mut self, addr: u32, data: &[u8]) -> Result;
    /// Starts erasing the sector at `addr` (a multiple of [`SECTOR`]).
    fn start_erase(&mut self, addr: u32) -> Result;
    /// A program or erase is still running.
    fn busy(&mut self) -> Result<bool>;
    /// Waits a little while the chip is busy: a timer on the radio, so the other tasks run.
    async fn pause();
}

/// Runs a store future to its end without an executor: the simulator and the tests, where the simulated flash
/// is never busy, so nothing waits. Not for the radio, whose flash futures wait on timers.
pub fn block<F: Future>(f: F) -> F::Output {
    fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(core::ptr::null(), &VTABLE)
    }
    fn noop(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    // SAFETY: the vtable's functions do nothing and never touch the (null) data pointer.
    let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    let mut f = pin!(f);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

/// CRC-32 (IEEE 802.3, reflected), fed in pieces.
#[derive(Clone, Copy)]
pub struct Crc32(u32);

/// One step of the reflected CRC-32 per bit of a byte, eight at a time: entry `b` is `b` shifted through all eight.
const fn crc32_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

/// PWR-26.14: a byte a lookup, not eight steps (the settings' change check runs it over the whole state every UI
/// pass); a named `static`, so it stays in the fast flash.
static CRC32_TABLE: [u32; 256] = crc32_table();

impl Crc32 {
    pub fn new() -> Self {
        Crc32(0xFFFF_FFFF)
    }

    pub fn update(&mut self, data: &[u8]) {
        let mut c = self.0;
        for &b in data {
            c = CRC32_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }

    pub fn get(self) -> u32 {
        !self.0
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

impl ferrite_app::persist::Sink for Crc32 {
    fn put(&mut self, b: &[u8]) {
        self.update(b)
    }
}

/// CRC-16/CCITT-FALSE, for the small records.
pub fn crc16(data: &[u8]) -> u16 {
    let mut c: u16 = 0xFFFF;
    for &b in data {
        c ^= (b as u16) << 8;
        for _ in 0..8 {
            c = if c & 0x8000 != 0 { (c << 1) ^ 0x1021 } else { c << 1 };
        }
    }
    c
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

#[cfg(test)]
mod tests {
    #[test]
    fn crcs_match_their_check_values() {
        let mut c = super::Crc32::new();
        c.update(b"1234");
        c.update(b"56789");
        assert_eq!(c.get(), 0xCBF4_3926);
        assert_eq!(super::crc16(b"123456789"), 0x29B1);
        assert_eq!(super::Crc32::default().get(), super::Crc32::new().get());
    }

    /// PWR-26.14: the table gives what the eight steps a byte gave, for any bytes fed in any pieces (so every record
    /// written before still checks).
    #[test]
    fn crc32_table_matches_the_bitwise_form() {
        fn bitwise(data: &[u8]) -> u32 {
            let mut c = 0xFFFF_FFFFu32;
            for &b in data {
                c ^= b as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
                }
            }
            !c
        }
        let mut x = 0x1234_5678u32;
        let data: Vec<u8> = (0..1000).map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        }).collect();
        for n in [0, 1, 2, 7, 64, 255, 256, 1000] {
            let mut c = super::Crc32::new();
            let (a, b) = data[..n].split_at(n / 3);
            c.update(a);
            c.update(b);
            assert_eq!(c.get(), bitwise(&data[..n]), "{n} bytes");
        }
    }

    /// `block` polls a future that isn't ready yet again until it is (one that wakes itself, as the radio's
    /// timers would), without an executor.
    #[test]
    fn block_polls_until_ready() {
        let mut n = 0;
        let v = super::block(core::future::poll_fn(|cx| {
            n += 1;
            let w = cx.waker().clone(); // a timer keeps its own copy
            w.wake_by_ref();
            if n < 3 { core::task::Poll::Pending } else { core::task::Poll::Ready(n * 10) }
        }));
        assert_eq!(v, 30);
    }
}
