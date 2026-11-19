//! The decoder slot. The radio runs one decoder at a time, so the active one lives in a single static of this
//! enum and every decoder shares its memory: the slot is as large as the largest. FT8/FT4 (uft48) and WSPR
//! (uwspr), about 12.75 KB each, join when their audio path is in (milestone 6); the budget below already holds
//! them.

use crate::msg::DecoderKind;
use core::mem::ManuallyDrop;
use udsc::Receiver as Dsc;
use uafsk::Demodulator as Afsk;
use upocsag::Receiver as Pocsag;
use usol950::Receiver as Sol950;

/// Audio rate the DSP task feeds the decoders.
pub const RATE_HZ: u32 = 24_000;

/// RAM the slot may take: FT8's or WSPR's footprint.
pub const BUDGET: usize = 13 * 1024;

union DecoderInner {
    off: (),
    afsk: ManuallyDrop<Afsk>,
    pocsag: ManuallyDrop<Pocsag>,
    dsc: ManuallyDrop<Dsc>,
    sol950: ManuallyDrop<Sol950>,
}

/// Shared decoder slot: holds one active decoder at a time inside a single `<= 13 KB` union,
/// initialized in-place by [`Decoder::switch`] so `dsp::task` never allocates a 13 KB temporary on the stack.
pub struct Decoder {
    kind: DecoderKind,
    inner: DecoderInner,
}

/// What a decoder found, handed to the event bus.
pub enum Found<'a> {
    Frame(&'a [u8]),
    /// An FX.25 block, collected but not corrected (APRS-44): too slow for the DSP task, so the APRS task
    /// corrects it ([`uafsk::Fx25Block::decode`]).
    Fx25(&'a uafsk::Fx25Block),
    /// A frame heard but not decoded (APRS-56): its FCS failed on every slicer.
    Miss(&'a uafsk::Miss),
    Page(&'a upocsag::Message<'a>),
    Call(&'a udsc::Call<'a>),
}

impl Decoder {
    pub const fn off() -> Self {
        Self { kind: DecoderKind::Off, inner: DecoderInner { off: () } }
    }

    pub fn afsk() -> Self {
        let mut d = Self::off();
        d.switch(DecoderKind::Afsk);
        d
    }

    pub fn pocsag() -> Self {
        let mut d = Self::off();
        d.switch(DecoderKind::Pocsag);
        d
    }

    pub fn dsc() -> Self {
        let mut d = Self::off();
        d.switch(DecoderKind::Dsc);
        d
    }

    pub fn sol950() -> Self {
        let mut d = Self::off();
        d.switch(DecoderKind::Sol950);
        d
    }

    /// Switch the active decoder in-place without constructing a `Decoder`-sized temporary on the stack.
    #[inline(never)]
    pub fn switch(&mut self, kind: DecoderKind) {
        self.kind = DecoderKind::Off;
        match kind {
            DecoderKind::Off => {}
            DecoderKind::Afsk => {
                unsafe {
                    core::ptr::addr_of_mut!(self.inner.afsk)
                        .write(ManuallyDrop::new(Afsk::new(uafsk::fsk::Config::new(RATE_HZ))));
                }
                self.kind = DecoderKind::Afsk;
            }
            DecoderKind::Pocsag => {
                unsafe {
                    core::ptr::addr_of_mut!(self.inner.pocsag)
                        .write(ManuallyDrop::new(Pocsag::new(RATE_HZ)));
                }
                self.kind = DecoderKind::Pocsag;
            }
            DecoderKind::Dsc => {
                unsafe {
                    core::ptr::addr_of_mut!(self.inner.dsc)
                        .write(ManuallyDrop::new(Dsc::new(uafsk::fsk::Config::new(RATE_HZ))));
                }
                self.kind = DecoderKind::Dsc;
            }
            DecoderKind::Sol950 => {
                unsafe {
                    Sol950::init_in_place(core::ptr::addr_of_mut!(self.inner.sol950) as *mut Sol950);
                }
                self.kind = DecoderKind::Sol950;
            }
        }
    }

    /// Diagnostic snapshot of the active SOL950 receiver: `(symbols, active_tracks, acquired)`.
    pub fn sol950_diag(&self) -> (u16, u8, bool) {
        if self.kind == DecoderKind::Sol950 {
            let d = unsafe { &*self.inner.sol950 };
            d.diag()
        } else {
            (0, 0, false)
        }
    }

    /// Take the latest 24-bin packed 4-bit SOL950 panafall column (`200 ms` cadence).
    pub fn sol950_take_wf(&mut self) -> Option<[u8; 12]> {
        if self.kind == DecoderKind::Sol950 {
            let d = unsafe { &mut *self.inner.sol950 };
            d.take_wf_col()
        } else {
            None
        }
    }

    /// Finish the current 10 s SOL950 UTC slot, write decoded frames to `out`, and reset for the next slot.
    #[inline(never)]
    pub fn sol950_finish(&mut self, out: &mut [usol950::Decoded; usol950::MAX_DECODED]) -> usize {
        if self.kind == DecoderKind::Sol950 {
            let d = unsafe { &mut *self.inner.sol950 };
            let n = d.finish_slot(out);
            d.reset();
            n
        } else {
            0
        }
    }

    /// The AFSK demodulator's carrier detect: 1200-baud data heard now (APRS-56.4); false for the others.
    pub fn busy(&self) -> bool {
        self.kind == DecoderKind::Afsk && unsafe { (*self.inner.afsk).busy() }
    }

    /// One block of audio.
    pub fn push(&mut self, block: &[i16], mut found: impl FnMut(Found)) {
        match self.kind {
            DecoderKind::Off => {}
            DecoderKind::Afsk => {
                let d = unsafe { &mut *self.inner.afsk };
                block.iter().for_each(|&x| d.push(x, |f| found(Found::Frame(f))));
                // The demodulator's one block buffer: handed over (the DSP task copies it into APRS_RX's slot,
                // which a plain frame's size already sets), then freed for the next tag.
                if let Some(b) = d.pending_fx25() {
                    found(Found::Fx25(b));
                    d.release_fx25();
                }
                if let Some(m) = d.take_miss() {
                    found(Found::Miss(&m)); // a miss's window (32 bits) is longer than a block: one at most
                }
            }
            DecoderKind::Pocsag => {
                let d = unsafe { &mut *self.inner.pocsag };
                block.iter().for_each(|&x| d.push(x, |m| found(Found::Page(m))));
            }
            DecoderKind::Dsc => {
                let d = unsafe { &mut *self.inner.dsc };
                block.iter().for_each(|&x| d.push(x, |c| found(Found::Call(c))));
            }
            DecoderKind::Sol950 => {
                let d = unsafe { &mut *self.inner.sol950 };
                d.push_samples(block);
            }
        }
    }
}

const _: () = assert!(core::mem::size_of::<Decoder>() <= BUDGET);

/// A pager message's characters kept: the event log's text field holds 64.
pub const PAGE_TEXT: usize = 64;
/// The largest capcode: POCSAG addresses are 21 bits.
pub const CAPCODE_MAX: u32 = (1 << 21) - 1;

/// A decoded page, small enough to cross from the DSP task to the UI task (DEC-4, DEC-7): 72 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page {
    /// The 21-bit address (capcode).
    pub capcode: u32,
    pub baud: u16,
    /// Function bits, 0-3.
    pub function: u8,
    /// Read as numeric characters (else 7-bit text).
    pub numeric: bool,
    pub len: u8,
    pub text: [u8; PAGE_TEXT],
}

impl Page {
    /// A message's text, read the way it most likely is: function 0 as numeric digits (the usual convention),
    /// others as 7-bit characters when nine in ten of them are printable (control characters and the padding
    /// senders end with dropped), else as digits after all (systems differ: upocsag's `Message`). A tone-only
    /// page has no text.
    pub fn from(m: &upocsag::Message) -> Page {
        let mut p = Page { capcode: m.address, baud: m.baud, function: m.function, numeric: false, len: 0, text: [0; PAGE_TEXT] };
        let (mut n, mut ok) = (0usize, 0usize);
        for c in m.alpha() {
            n += 1;
            ok += (0x20..0x7F).contains(&c) as usize;
        }
        let mut put = |c: u8| {
            if (p.len as usize) < PAGE_TEXT {
                p.text[p.len as usize] = c;
                p.len += 1;
            }
        };
        if m.function != 0 && n > 0 && ok * 10 >= n * 9 {
            m.alpha().filter(|c| (0x20..0x7F).contains(c)).for_each(&mut put);
        } else {
            p.numeric = true;
            m.numeric().for_each(&mut put);
        }
        while p.len > 0 && p.text[p.len as usize - 1] == b' ' {
            p.len -= 1; // numeric pages end padded with spaces
        }
        p
    }

    pub fn text(&self) -> &[u8] {
        &self.text[..self.len as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page through the codec and back, as both kinds.
    #[test]
    fn page_text() {
        extern crate std;
        use std::vec::Vec;
        for (function, content, want, numeric) in [
            (3, upocsag::encode::Content::Alpha(b"STN 4 RESPOND HWY 417"), &b"STN 4 RESPOND HWY 417"[..], false),
            (0, upocsag::encode::Content::Numeric(b"5551234"), &b"5551234"[..], true),
        ] {
            let mut words = Vec::new();
            upocsag::encode::encode(1_234_567, function, content, |w| words.push(w));
            let mut d: upocsag::decode::Decoder = upocsag::decode::Decoder::new();
            d.baud = 1200;
            let mut got = None;
            for w in words {
                if w == upocsag::SYNC {
                    d.sync(); // a batch starts
                } else {
                    d.word(w, |m| got = Some(Page::from(m)));
                }
            }
            d.end(|m| got = got.or(Some(Page::from(m))));
            let p = got.expect("decoded");
            assert_eq!((p.capcode, p.function, p.baud, p.numeric), (1_234_567, function, 1200, numeric));
            assert_eq!(p.text(), want);
        }
    }

    #[test]
    fn slot_size() {
        let n = core::mem::size_of::<Decoder>();
        std::println!("decoder slot: {n} B");
        assert!(n <= BUDGET);
        let mut d = Decoder::pocsag();
        let mut hits = 0;
        d.push(&[0; 1200], |_| hits += 1);
        assert_eq!(hits, 0, "silence decodes nothing");
    }

    /// The other slots: Off takes audio and does nothing; DSC and AFSK decode nothing from silence.
    #[test]
    fn other_slots_on_silence() {
        let mut hits = 0;
        for mut d in [Decoder::off(), Decoder::dsc(), Decoder::afsk()] {
            d.push(&[0; 2400], |_| hits += 1);
        }
        assert_eq!(hits, 0);
    }

    /// DEC-4: a text page longer than the event log's field keeps its first [`PAGE_TEXT`] characters.
    #[test]
    fn long_page_cut_to_the_field() {
        extern crate std;
        use std::vec::Vec;
        let text: Vec<u8> = (0..90u8).map(|i| b'A' + i % 26).collect();
        let mut words = Vec::new();
        upocsag::encode::encode(42, 3, upocsag::encode::Content::Alpha(&text), |w| words.push(w));
        let mut d: upocsag::decode::Decoder = upocsag::decode::Decoder::new();
        d.baud = 1200;
        let mut got = None;
        for w in words {
            if w == upocsag::SYNC {
                d.sync();
            } else {
                d.word(w, |m| got = Some(Page::from(m)));
            }
        }
        d.end(|m| got = got.or(Some(Page::from(m))));
        let p = got.expect("decoded");
        assert!(!p.numeric);
        assert_eq!(p.text(), &text[..PAGE_TEXT]);
    }
}
