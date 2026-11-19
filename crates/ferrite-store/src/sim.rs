//! A simulated NOR flash for the simulator and the tests: erased bytes are 0xFF, a program only clears bits, an
//! erase sets a 4 KB sector. Programs and erases finish as they start, so the store never waits on it. It can cut
//! the power at any point: after a set number of operations (each programmed byte and each erase counts one) the
//! operation in progress is left half done, the worst way NOR leaves it, and every call fails until
//! [`SimFlash::power_on`]. Writing outside [`ferrite_core::layout::WRITABLE`] panics: calibration and the update
//! area must never be touched.
//!
//! It holds the parts of layout 3 Ferrite uses (decision 333): the low part, from the layout header to the end of
//! the packet mailbox, the high part, the event log and the scratch ring, and the SPI parts' slots in the boot
//! screens region (decision 335: the logo and the terminal's templates). The rest of the 4 MB reads erased and
//! refuses writes (a link's raw write there fails in the simulator).

use crate::{Nor, NorError, Result, SECTOR};
use ferrite_core::layout::{EVENT_LOG, SCRATCH_RING, SCREENS, SOL950_LOG, WRITABLE};

/// The low part: the chip from 0 to the end of the SOL950 contact log (`0x0D_0000`).
pub const LOW: usize = SOL950_LOG.end as usize;
/// The high part, after it in `mem`: the event log, then the scratch ring.
pub const HIGH: core::ops::Range<u32> = EVENT_LOG.start..SCRATCH_RING.end;
/// The SPI parts' slots in the boot screens region (`ferrite_core::part`), after the high part in `mem`.
pub const PARTS: core::ops::Range<u32> = SCREENS.start..SCREENS.start + 0x1_0000;
pub const SIZE: usize = LOW + (HIGH.end - HIGH.start) as usize + (PARTS.end - PARTS.start) as usize;
/// What the simulator keeps between runs ([`SimFlash::image`]): the low part and the event log (not the scratch
/// ring, empty whenever nothing is being written).
pub const IMAGE_LEN: usize = LOW + (EVENT_LOG.end - EVENT_LOG.start) as usize;
/// The whole chip, for the erase counts.
const CHIP: usize = 4 << 20;

#[derive(Clone)]
pub struct SimFlash {
    pub mem: Vec<u8>,
    /// Operations so far (programmed bytes and erases).
    pub ops: u64,
    /// Cut the power when `ops` reaches this.
    pub cut_at: Option<u64>,
    /// The power is off: everything fails.
    pub dead: bool,
    /// Erases done in each sector (decision 227's wear tests).
    pub erases: Vec<u32>,
    /// A change since the owner last looked (the simulator saves the image when it is set).
    pub dirty: bool,
    /// Reads so far, and the bytes they read (what a pass over the records costs on the radio's SPI bus).
    pub reads: u64,
    pub read_bytes: u64,
    rng: u64,
}

impl SimFlash {
    /// Where chip address `addr` is in `mem`, if the simulated chip holds it.
    pub fn place(addr: u32) -> Option<usize> {
        match addr {
            a if (a as usize) < LOW => Some(a as usize),
            a if HIGH.contains(&a) => Some(LOW + (a - HIGH.start) as usize),
            a if PARTS.contains(&a) => Some(LOW + (HIGH.end - HIGH.start) as usize + (a - PARTS.start) as usize),
            _ => None,
        }
    }

    /// [`SimFlash::place`] for an address the simulated chip holds (the tests).
    pub fn at(addr: u32) -> usize {
        Self::place(addr).unwrap_or_else(|| panic!("{addr:#x}: outside the simulated flash"))
    }

    /// A blank chip, as after installing (only calibration, which this never touches, would be left).
    pub fn new() -> Self {
        SimFlash { mem: vec![0xFF; SIZE], ops: 0, cut_at: None, dead: false, dirty: false, erases: vec![0; CHIP / SECTOR as usize], reads: 0, read_bytes: 0, rng: 0x9E37_79B9_7F4A_7C15 }
    }

    /// A chip holding `bytes` at chip address `at` (what the simulated chip holds of it), blank elsewhere.
    pub fn with_image(at: u32, bytes: &[u8]) -> Self {
        let mut f = Self::new();
        for (i, &b) in bytes.iter().enumerate() {
            if let Some(a) = Self::place(at + i as u32) {
                f.mem[a] = b;
            }
        }
        f
    }

    /// A chip from a kept image ([`SimFlash::image`]); one of another length (an image from another layout) gives a
    /// blank chip.
    pub fn from_image(img: &[u8]) -> Self {
        let mut f = Self::new();
        if img.len() == IMAGE_LEN {
            f.mem[..IMAGE_LEN].copy_from_slice(img);
        }
        f
    }

    /// What the simulator keeps between runs: [`IMAGE_LEN`] bytes.
    pub fn image(&self) -> Vec<u8> {
        self.mem[..IMAGE_LEN].to_vec()
    }

    /// Seeds the half-done bits of a cut (tests vary it).
    pub fn seed(&mut self, s: u64) {
        self.rng = s | 1;
    }

    /// The power back on after a cut.
    pub fn power_on(&mut self) {
        self.dead = false;
        self.cut_at = None;
    }

    fn rand(&mut self) -> u8 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng as u8
    }

    fn writable(addr: u32, len: u32) {
        assert!(
            WRITABLE.iter().any(|r| addr >= r.start && addr + len <= r.end),
            "write outside the writable ranges: {addr:#x}+{len:#x}"
        );
    }

    /// One operation; false when the power goes on it.
    fn tick(&mut self) -> bool {
        self.ops += 1;
        if self.cut_at.is_some_and(|c| self.ops >= c) {
            self.dead = true;
            return false;
        }
        true
    }
}

impl Default for SimFlash {
    fn default() -> Self {
        Self::new()
    }
}

impl Nor for SimFlash {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result {
        if self.dead {
            return Err(NorError);
        }
        match Self::place(addr) {
            Some(a) => buf.copy_from_slice(&self.mem[a..a + buf.len()]),
            None => buf.fill(0xFF),
        }
        self.reads += 1;
        self.read_bytes += buf.len() as u64;
        Ok(())
    }

    fn start_program(&mut self, addr: u32, data: &[u8]) -> Result {
        if self.dead {
            return Err(NorError);
        }
        assert!(addr / crate::PAGE == (addr + data.len() as u32 - 1) / crate::PAGE, "program at {addr:#x} crosses a page");
        Self::writable(addr, data.len() as u32);
        if Self::place(addr).is_none() {
            return Err(NorError);
        }
        self.dirty = true;
        for (i, &d) in data.iter().enumerate() {
            let a = Self::at(addr) + i;
            if !self.tick() {
                // Cut: this byte and the rest of the call get some of their bits, at random.
                for (k, &d) in data.iter().enumerate().skip(i) {
                    let clear = !d & self.rand();
                    self.mem[Self::at(addr) + k] &= !clear;
                }
                return Err(NorError);
            }
            self.mem[a] &= d;
        }
        Ok(())
    }

    fn start_erase(&mut self, addr: u32) -> Result {
        if self.dead {
            return Err(NorError);
        }
        assert_eq!(addr % SECTOR, 0, "erase at {addr:#x}: not a sector");
        Self::writable(addr, SECTOR);
        if Self::place(addr).is_none() {
            return Err(NorError);
        }
        self.dirty = true;
        self.erases[(addr / SECTOR) as usize] += 1;
        let a = Self::at(addr);
        if !self.tick() {
            // Cut: some bits of every byte are set, at random.
            for i in 0..SECTOR as usize {
                let set = self.rand();
                self.mem[a + i] |= set;
            }
            return Err(NorError);
        }
        self.mem[a..a + SECTOR as usize].fill(0xFF);
        Ok(())
    }

    fn busy(&mut self) -> Result<bool> {
        if self.dead { Err(NorError) } else { Ok(false) }
    }

    async fn pause() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_core::layout::{CALIBRATION, CHANNELS, UPDATE_AREA};

    /// Bytes land where they were taken, those past the parts it holds dropped; the rest is erased. A kept image
    /// comes back whole; one of another length (another layout's) gives a blank chip; the parts not held read
    /// erased and refuse writes.
    #[test]
    fn an_image_is_placed_and_clipped() {
        let f = SimFlash::with_image(CHANNELS.start, &[0x42; 8]);
        let a = CHANNELS.start as usize;
        assert_eq!(&f.mem[a - 1..a + 9], &[0xFF, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0xFF]);
        let f = SimFlash::with_image(LOW as u32 - 4, &[0; 16]);
        assert_eq!(&f.mem[LOW - 4..LOW + 1], &[0, 0, 0, 0, 0xFF], "past the low part: dropped, the high part untouched");
        assert!(SimFlash::default().mem.iter().all(|&b| b == 0xFF));
        let mut img = vec![0xFFu8; IMAGE_LEN];
        img[IMAGE_LEN - 1] = 7;
        let mut f = SimFlash::from_image(&img);
        assert_eq!((f.image(), f.mem[SimFlash::at(EVENT_LOG.end - 1)]), (img, 7));
        assert!(SimFlash::from_image(&[0; 300]).mem.iter().all(|&b| b == 0xFF), "an image from another layout");
        let mut b = [0u8; 2];
        assert!(f.read(0x10_0000, &mut b).is_ok() && b == [0xFF; 2]);
        assert!(f.start_erase(0x0D_0000).is_err() && f.start_program(0x0D_0000, &[0]).is_err());
    }

    /// The power off: every call fails, nothing changes, until it is back on.
    #[test]
    fn a_dead_chip_does_nothing() {
        let mut f = SimFlash::new();
        f.dead = true;
        let mut b = [0u8; 4];
        assert!(f.read(CHANNELS.start, &mut b).is_err() && f.start_program(CHANNELS.start, &[0]).is_err() && f.start_erase(CHANNELS.start).is_err() && f.busy().is_err());
        assert!(f.mem.iter().all(|&b| b == 0xFF) && f.ops == 0 && !f.dirty);
        f.power_on();
        assert!(f.start_program(CHANNELS.start, &[0]).is_ok() && f.busy() == Ok(false) && f.mem[CHANNELS.start as usize] == 0);
    }

    /// SAFE-1: calibration is never written; the simulated chip panics on it, as on the update area.
    #[test]
    #[should_panic(expected = "write outside the writable ranges")]
    fn programming_calibration_panics() {
        let _ = SimFlash::new().start_program(CALIBRATION.start, &[0]);
    }

    #[test]
    #[should_panic(expected = "outside the simulated flash")]
    fn the_update_area_is_outside_the_simulated_chip() {
        SimFlash::at(UPDATE_AREA.start);
    }

    /// Decision 404: the stock bootloader's fonts (its update screen's "UPDATE") are never written: the simulated
    /// flash fails the test that tries.
    #[test]
    #[should_panic(expected = "write outside the writable ranges")]
    fn the_bootloader_fonts_are_never_written() {
        let mut f = SimFlash::new();
        let _ = f.start_erase(0x15_C000);
    }
}
