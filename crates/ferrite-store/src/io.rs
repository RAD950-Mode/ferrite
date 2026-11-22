//! The flash as the store uses it: reads that see a write in progress the way it will end, and programs and
//! erases that wait for the chip without holding it.
//!
//! On the radio the UI reads channel records while the Storage task writes. Two windows would show it bytes
//! that are neither old nor new, so reads look elsewhere for them:
//! - a record programmed into erased bytes: the record itself, from RAM ([`Flash::program_record`]);
//! - a sector being erased and copied back from a scratch sector: the scratch copy, or erased bytes when
//!   nothing comes back ([`Flash::redirect`]).

use crate::{Crc32, Nor, Result, PAGE, SECTOR};
use core::cell::{Cell, RefCell};
use ferrite_core::layout::SCRATCH_RING;

/// Pauses [`Flash::wait`] allows (each 1 ms on the radio).
pub const WAIT_MAX: u32 = 3000;

/// The largest record programmed in place (a channel).
const PEND_MAX: usize = 64;

pub struct Flash<N: Nor> {
    nor: RefCell<N>,
    /// This sector's reads come from that scratch sector (Some) or read erased (None).
    redirect: Cell<Option<(u32, Option<u32>)>>,
    /// The scratch ring's next sector (decision 227).
    scratch: Cell<u32>,
    /// A record being programmed in place, and its bytes.
    pending: Cell<Option<(u32, usize)>>,
    pend: RefCell<[u8; PEND_MAX]>,
    /// LINK-71: the list sectors ([`crate::records::list_sector`]) written since their hash was last made: one bit
    /// each, all at first.
    stale: Cell<u128>,
}

impl<N: Nor> Flash<N> {
    pub const fn new(nor: N) -> Self {
        Flash { nor: RefCell::new(nor), redirect: Cell::new(None), scratch: Cell::new(0), pending: Cell::new(None), pend: RefCell::new([0xFF; PEND_MAX]), stale: Cell::new(u128::MAX) }
    }

    pub fn into_inner(self) -> N {
        self.nor.into_inner()
    }

    pub fn nor(&self) -> core::cell::RefMut<'_, N> {
        self.nor.borrow_mut()
    }

    /// What a reader should see at `addr` (`buf` stays inside one sector).
    pub fn read(&self, addr: u32, buf: &mut [u8]) -> Result {
        match self.redirect.get() {
            Some((s, Some(from))) if (s..s + SECTOR).contains(&addr) => self.raw_read(from + addr - s, buf)?,
            Some((s, None)) if (s..s + SECTOR).contains(&addr) => buf.fill(0xFF),
            _ => self.raw_read(addr, buf)?,
        }
        if let Some((p, n)) = self.pending.get() {
            let pend = self.pend.borrow();
            for (i, b) in buf.iter_mut().enumerate() {
                let a = addr + i as u32;
                if (p..p + n as u32).contains(&a) {
                    *b = pend[(a - p) as usize];
                }
            }
        }
        Ok(())
    }

    /// The bytes in the chip, unless the chip is in use right now (false then, or when the read fails): the SPI
    /// parts' reads (decision 335), which come from drawing code and must never panic on a borrow.
    pub fn try_read(&self, addr: u32, buf: &mut [u8]) -> bool {
        self.nor.try_borrow_mut().is_ok_and(|mut n| n.read(addr, buf).is_ok())
    }

    /// The bytes in the chip.
    pub fn raw_read(&self, addr: u32, buf: &mut [u8]) -> Result {
        self.nor.borrow_mut().read(addr, buf)
    }

    /// Whether `len` bytes at `addr` are erased.
    pub fn blank(&self, addr: u32, len: u32) -> Result<bool> {
        let mut buf = [0u8; 64];
        let mut a = addr;
        while a < addr + len {
            let n = (addr + len - a).min(64) as usize;
            self.raw_read(a, &mut buf[..n])?;
            if buf[..n].iter().any(|&b| b != 0xFF) {
                return Ok(false);
            }
            a += n as u32;
        }
        Ok(true)
    }

    /// CRC-32 of `len` bytes at `addr`.
    pub fn crc(&self, addr: u32, len: u32) -> Result<u32> {
        let mut buf = [0u8; 64];
        let mut c = Crc32::new();
        let mut a = addr;
        while a < addr + len {
            let n = (addr + len - a).min(64) as usize;
            self.raw_read(a, &mut buf[..n])?;
            c.update(&buf[..n]);
            a += n as u32;
        }
        Ok(c.get())
    }

    /// Until the chip is idle, or [`WAIT_MAX`] pauses (about 3 s on the radio, several times the slowest sector
    /// erase): a chip that never clears BUSY (a stuck part, or MISO reading 0xFF) fails the write instead of
    /// holding the Storage task, or the boot's mount, for ever.
    async fn wait(&self) -> Result {
        let mut n = 0;
        while self.nor.borrow_mut().busy()? {
            n += 1;
            if n > WAIT_MAX {
                return Err(crate::NorError);
            }
            N::pause().await;
        }
        Ok(())
    }

    /// LINK-71: a write at `addr` (`len` bytes) changes the list sectors it touches; marked before it starts and
    /// again when done, so a hash made while it ran is made again.
    fn mark(&self, addr: u32, len: usize) {
        for a in [addr, addr + len.saturating_sub(1) as u32] {
            if let Some(k) = crate::records::list_sector(a) {
                self.stale.set(self.stale.get() | 1 << k);
            }
        }
    }

    /// LINK-71: list sectors to hash again (a read that failed, or the mount's own hashes taken back off).
    pub fn mark_stale(&self, sectors: u128) {
        self.stale.set(self.stale.get() | sectors);
    }

    /// LINK-71: the mount made these sectors' hashes itself: not stale.
    pub fn fresh(&self, sectors: u128) {
        self.stale.set(self.stale.get() & !sectors);
    }

    /// LINK-71: the list sectors to hash again, now cleared.
    pub fn take_stale(&self) -> u128 {
        self.stale.replace(0)
    }

    /// Programs `data` at `addr` (erased bytes), page by page; all-erased pieces are skipped.
    pub async fn program(&self, addr: u32, data: &[u8]) -> Result {
        self.mark(addr, data.len());
        let r = self.program_pages(addr, data).await;
        self.mark(addr, data.len());
        r
    }

    async fn program_pages(&self, addr: u32, data: &[u8]) -> Result {
        let mut a = addr;
        let mut rest = data;
        while !rest.is_empty() {
            let n = ((PAGE - a % PAGE) as usize).min(rest.len());
            if rest[..n].iter().any(|&b| b != 0xFF) {
                self.nor.borrow_mut().start_program(a, &rest[..n])?;
                self.wait().await?;
            }
            a += n as u32;
            rest = &rest[n..];
        }
        Ok(())
    }

    pub async fn erase(&self, addr: u32) -> Result {
        self.mark(addr, 1);
        self.nor.borrow_mut().start_erase(addr)?;
        let r = self.wait().await;
        self.mark(addr, 1);
        r
    }

    /// Erases a sector that readers should see as erased from the start (settings reset).
    pub async fn erase_blanked(&self, addr: u32) -> Result {
        self.redirect.set(Some((addr, None)));
        let r = self.erase(addr).await;
        self.redirect.set(None);
        r
    }

    /// Programs a record (up to 64 bytes) into erased bytes; readers see it whole meanwhile.
    pub async fn program_record(&self, addr: u32, rec: &[u8]) -> Result {
        self.pend.borrow_mut()[..rec.len()].copy_from_slice(rec);
        self.pending.set(Some((addr, rec.len())));
        let r = self.program(addr, rec).await;
        self.pending.set(None);
        r
    }

    /// Reads of `sector` come from the scratch sector `from` until [`Flash::end_redirect`].
    pub fn redirect(&self, sector: u32, from: u32) {
        self.redirect.set(Some((sector, Some(from))));
    }

    /// The scratch ring's sector for the next rewrite; each rewrite takes the next one, wrapping.
    pub fn scratch_next(&self) -> u32 {
        let n = SCRATCH_RING.len() as u32 / SECTOR;
        let k = self.scratch.get() % n;
        self.scratch.set(k + 1);
        SCRATCH_RING.start + k * SECTOR
    }

    /// Where the ring starts this power-on: a number that moves on between power-ons (the settings' save count),
    /// so a power-on with few rewrites doesn't wear the first sectors.
    pub fn scratch_seed(&self, n: u32) {
        self.scratch.set(n % (SCRATCH_RING.len() as u32 / SECTOR));
    }

    pub fn end_redirect(&self) {
        self.redirect.set(None);
    }

    /// Copies `len` bytes from `from` to `to` (erased).
    pub async fn copy(&self, from: u32, to: u32, len: u32) -> Result {
        let mut buf = [0u8; PAGE as usize];
        let mut off = 0;
        while off < len {
            let n = (len - off).min(PAGE) as usize;
            self.raw_read(from + off, &mut buf[..n])?;
            self.program(to + off, &buf[..n]).await?;
            off += n as u32;
        }
        Ok(())
    }

    /// Whether `len` bytes at `a` and `b` are the same.
    pub fn same(&self, a: u32, b: u32, len: u32) -> Result<bool> {
        let (mut x, mut y) = ([0u8; 64], [0u8; 64]);
        let mut off = 0;
        while off < len {
            let n = (len - off).min(64) as usize;
            self.raw_read(a + off, &mut x[..n])?;
            self.raw_read(b + off, &mut y[..n])?;
            if x[..n] != y[..n] {
                return Ok(false);
            }
            off += n as u32;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod wait_tests {
    use super::*;
    use crate::{block, NorError};

    /// A chip that never finishes: every program or erase leaves BUSY set.
    struct Stuck {
        polls: u32,
    }
    impl Nor for Stuck {
        fn read(&mut self, _: u32, buf: &mut [u8]) -> Result {
            buf.fill(0xFF);
            Ok(())
        }
        fn start_program(&mut self, _: u32, _: &[u8]) -> Result {
            Ok(())
        }
        fn start_erase(&mut self, _: u32) -> Result {
            Ok(())
        }
        fn busy(&mut self) -> Result<bool> {
            self.polls += 1;
            Ok(true)
        }
        async fn pause() {}
    }

    /// A stuck BUSY fails the erase and the program after WAIT_MAX polls; nothing waits for ever.
    #[test]
    fn stuck_busy_fails_cleanly() {
        let f = Flash::new(Stuck { polls: 0 });
        assert_eq!(block(f.erase(SCRATCH_RING.start)), Err(NorError));
        assert!(f.nor().polls > WAIT_MAX && f.nor().polls <= WAIT_MAX + 2);
        let f = Flash::new(Stuck { polls: 0 });
        assert_eq!(block(f.program(SCRATCH_RING.start, &[0u8; 16])), Err(NorError));
    }
}

#[cfg(all(test, feature = "std"))]
mod read_tests {
    use super::*;
    use crate::sim::SimFlash;
    use core::future::Future;
    use core::task::{Context, Poll, Waker};
    use ferrite_core::layout::CHANNELS;

    /// The simulated chip with an erase that lands only after two busy polls, and a pause that yields once: an
    /// erase still running when the reader looks.
    struct Late {
        f: SimFlash,
        erase: Option<u32>,
        polls: u32,
    }

    impl Nor for Late {
        fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result {
            self.f.read(addr, buf)
        }
        fn start_program(&mut self, addr: u32, data: &[u8]) -> Result {
            self.f.start_program(addr, data)
        }
        fn start_erase(&mut self, addr: u32) -> Result {
            (self.erase, self.polls) = (Some(addr), 2);
            Ok(())
        }
        fn busy(&mut self) -> Result<bool> {
            if self.polls > 0 {
                self.polls -= 1;
                return Ok(true);
            }
            if let Some(a) = self.erase.take() {
                self.f.start_erase(a)?;
            }
            Ok(false)
        }
        async fn pause() {
            let mut once = false;
            core::future::poll_fn(|_| if core::mem::replace(&mut once, true) { Poll::Ready(()) } else { Poll::Pending }).await
        }
    }

    /// A sector being erased for the reset reads as erased from the start, while the chip still holds the old
    /// bytes; once the erase lands both agree.
    #[test]
    fn a_blanked_erase_reads_erased_while_it_runs() {
        let s = CHANNELS.start;
        let mut chip = SimFlash::new();
        chip.mem[s as usize..s as usize + 64].fill(0x12);
        let f = Flash::new(Late { f: chip, erase: None, polls: 0 });
        let mut cx = Context::from_waker(Waker::noop());
        let mut fut = core::pin::pin!(f.erase_blanked(s));
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        let (mut seen, mut raw) = ([0u8; 16], [0u8; 16]);
        f.read(s + 8, &mut seen).unwrap();
        f.raw_read(s + 8, &mut raw).unwrap();
        assert_eq!((seen, raw), ([0xFF; 16], [0x12; 16]), "the reader sees it erased, the chip not yet");
        while fut.as_mut().poll(&mut cx).is_pending() {}
        f.raw_read(s + 8, &mut raw).unwrap();
        assert_eq!(raw, [0xFF; 16]);
    }

    /// `same` compares in 64-byte pieces and stops at the first that differs.
    #[test]
    fn same_finds_a_difference_past_the_first_piece() {
        let mut chip = SimFlash::new();
        let (a, b) = (CHANNELS.start as usize, CHANNELS.start as usize + 4096);
        chip.mem[a..a + 200].fill(7);
        chip.mem[b..b + 200].fill(7);
        chip.mem[b + 150] = 8;
        let f = Flash::new(chip);
        assert_eq!(f.same(a as u32, b as u32, 150), Ok(true));
        assert_eq!(f.same(a as u32, b as u32, 200), Ok(false));
    }
}
