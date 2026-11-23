//! The decoder slot. The radio runs one decoder at a time, so the active one lives in a single static of this
//! enum and every decoder shares its memory: the slot is as large as the largest. FT8/FT4 (uft48) and WSPR
//! (uwspr), about 12.75 KB each, join when their audio path is in (milestone 6); the budget below already holds
//! them.

use crate::msg::DecoderKind;
use core::mem::ManuallyDrop;
use udsc::Receiver as Dsc;
use uafsk::Demodulator as Afsk;
use upocsag::Receiver as Pocsag;
use urad950::Receiver as Rad950;
use urad950::CompactReceiver60 as Rad950_60;
use crate::wwv::{WwvReceiver, WwvStatus};
use urtcw::{CwDecoder, CwMeter};

/// Audio rate the DSP task feeds the decoders.
pub const RATE_HZ: u32 = 24_000;

/// RAM the slot may take: holds RAD950-60 dual-phase Layout B (`21,864 B`) alongside FT8/WSPR/APRS/POCSAG.
pub const BUDGET: usize = 22 * 1024;

/// Exact Q14 `round(2 * cos(2*pi*((k+0.5)*125)/6000) * 16384)` for `k = 0..24` (`0..3,000 Hz` @ `6,000 Hz`, 125 Hz/bin).
const CW_SPEC_COEFF_Q14: [i32; 24] = [
    32698, 32138, 31029, 29389, 27246, 24636, 21605, 18205,
    14493, 10533,  6393,  2143, -2143, -6393,-10533,-14493,
   -18205,-21605,-24636,-27246,-29389,-31029,-32138,-32698,
];

/// Lightweight 24-bin CW audio spectrum (`100..2,400 Hz`, 100 Hz/bin, 10 Hz waterfall cadence).
struct CwSpectrum {
    dec_acc: i32,
    dec_cnt: u8,
    samp_n: u8,
    blocks: u8,
    s1: [i32; 24],
    s2: [i32; 24],
    peak: [u32; 24],
    floor_ema: u32,
    pending: Option<[u8; 12]>,
}

impl CwSpectrum {
    const fn new() -> Self {
        Self {
            dec_acc: 0,
            dec_cnt: 0,
            samp_n: 0,
            blocks: 0,
            s1: [0; 24],
            s2: [0; 24],
            peak: [0; 24],
            floor_ema: 32,
            pending: None,
        }
    }

    fn push_24k(&mut self, samples: &[i16]) {
        for &s in samples {
            self.dec_acc += s as i32;
            self.dec_cnt += 1;
            if self.dec_cnt == 4 {
                let x = self.dec_acc >> 6; // average of 4 samples, scaled >> 4 (-2048..2047)
                self.dec_acc = 0;
                self.dec_cnt = 0;
                for i in 0..24 {
                    let s1 = self.s1[i];
                    let s0 = x + (((CW_SPEC_COEFF_Q14[i] as i64 * s1 as i64) >> 14) as i32) - self.s2[i];
                    self.s2[i] = s1;
                    self.s1[i] = s0;
                }
                self.samp_n += 1;
                if self.samp_n == 48 {
                    self.samp_n = 0;
                    self.finish_8ms();
                }
            }
        }
    }

    fn finish_8ms(&mut self) {
        for i in 0..24 {
            let a = self.s1[i] as i64;
            let b = self.s2[i] as i64;
            let p = a * a + b * b - (((CW_SPEC_COEFF_Q14[i] as i64 * a) >> 14) * b);
            self.s1[i] = 0;
            self.s2[i] = 0;
            let mag = if p <= 0 { 0 } else { isqrt64(p as u64) as u32 };
            if mag > self.peak[i] {
                self.peak[i] = mag;
            }
        }
        self.blocks += 1;
        if self.blocks >= 12 {
            self.blocks = 0;
            // Estimate noise floor from 25th-percentile bin (6th smallest of 24).
            let mut sorted = self.peak;
            for i in 0..6 {
                let mut min_j = i;
                for j in (i + 1)..24 {
                    if sorted[j] < sorted[min_j] {
                        min_j = j;
                    }
                }
                sorted.swap(i, min_j);
            }
            let obs_floor = sorted[5].max(16);
            self.floor_ema = ((self.floor_ema as u64 * 7 + obs_floor as u64) >> 3).max(16) as u32;

            let mut packed = [0u8; 12];
            for i in 0..24 {
                // Map peak/floor ratio to 4-bit level (0..=15, ~2 dB per step above noise floor).
                let r = (self.peak[i].saturating_mul(8) / self.floor_ema.max(1)).max(1);
                let msb = 31 - r.leading_zeros();
                let frac = if msb >= 2 { (r >> (msb - 2)) & 3 } else { 0 };
                let q = (msb * 4 + frac).saturating_sub(11); // r=8 -> ~1, r=16 -> ~5, r=64 -> ~13
                let lvl = q.min(15) as u8;
                if i & 1 == 0 {
                    packed[i >> 1] = lvl << 4;
                } else {
                    packed[i >> 1] |= lvl;
                }
                self.peak[i] = 0;
            }
            self.pending = Some(packed);
        }
    }

    fn take(&mut self) -> Option<[u8; 12]> {
        self.pending.take()
    }
}

fn isqrt64(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) >> 1;
    while y < x {
        x = y;
        y = (x + n / x) >> 1;
    }
    x
}

struct CwSlot {
    dec: CwDecoder,
    spec: CwSpectrum,
}

union DecoderInner {
    off: (),
    afsk: ManuallyDrop<Afsk>,
    pocsag: ManuallyDrop<Pocsag>,
    dsc: ManuallyDrop<Dsc>,
    rad950: ManuallyDrop<Rad950>,
    rad950_60: ManuallyDrop<Rad950_60>,
    wwv: ManuallyDrop<WwvReceiver>,
    cw: ManuallyDrop<CwSlot>,
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
    Wwv(WwvStatus),
    Cw { ch: Option<u8>, meter: CwMeter },
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

    pub fn rad950() -> Self {
        let mut d = Self::off();
        d.switch(DecoderKind::Rad950);
        d
    }

    #[inline(never)]
    unsafe fn init_afsk(dst: *mut ManuallyDrop<Afsk>) {
        dst.write(ManuallyDrop::new(Afsk::new(uafsk::fsk::Config::new(RATE_HZ))));
    }

    #[inline(never)]
    unsafe fn init_pocsag(dst: *mut ManuallyDrop<Pocsag>) {
        dst.write(ManuallyDrop::new(Pocsag::new(RATE_HZ)));
    }

    #[inline(never)]
    unsafe fn init_dsc(dst: *mut ManuallyDrop<Dsc>) {
        dst.write(ManuallyDrop::new(Dsc::new(uafsk::fsk::Config::new(RATE_HZ))));
    }

    /// Switch the active decoder in-place without constructing a `Decoder`-sized temporary on the stack.
    #[inline(never)]
    pub fn switch(&mut self, kind: DecoderKind) {
        self.kind = DecoderKind::Off;
        match kind {
            DecoderKind::Off => {}
            DecoderKind::Afsk => {
                unsafe {
                    Self::init_afsk(core::ptr::addr_of_mut!(self.inner.afsk));
                }
                self.kind = DecoderKind::Afsk;
            }
            DecoderKind::Pocsag => {
                unsafe {
                    Self::init_pocsag(core::ptr::addr_of_mut!(self.inner.pocsag));
                }
                self.kind = DecoderKind::Pocsag;
            }
            DecoderKind::Dsc => {
                unsafe {
                    Self::init_dsc(core::ptr::addr_of_mut!(self.inner.dsc));
                }
                self.kind = DecoderKind::Dsc;
            }
            DecoderKind::Rad950 => {
                unsafe {
                    Rad950::init_in_place(core::ptr::addr_of_mut!(self.inner.rad950) as *mut Rad950);
                }
                self.kind = DecoderKind::Rad950;
            }
            DecoderKind::Rad950_60 => {
                unsafe {
                    Rad950_60::init_in_place(core::ptr::addr_of_mut!(self.inner.rad950_60) as *mut Rad950_60);
                }
                self.kind = DecoderKind::Rad950_60;
            }
            DecoderKind::Wwv => {
                unsafe {
                    core::ptr::addr_of_mut!(self.inner.wwv).write(ManuallyDrop::new(WwvReceiver::new()));
                }
                self.kind = DecoderKind::Wwv;
            }
            DecoderKind::Cw => {
                unsafe {
                    core::ptr::addr_of_mut!(self.inner.cw).write(ManuallyDrop::new(CwSlot {
                        dec: CwDecoder::new(700),
                        spec: CwSpectrum::new(),
                    }));
                }
                self.kind = DecoderKind::Cw;
            }
        }
    }

    /// Diagnostic snapshot of the active RAD950 receiver: `(symbols, active_tracks, acquired)`.
    pub fn rad950_diag(&self) -> (u16, u8, bool) {
        if self.kind == DecoderKind::Rad950 {
            let d = unsafe { &*self.inner.rad950 };
            d.diag()
        } else if self.kind == DecoderKind::Rad950_60 {
            let d = unsafe { &*self.inner.rad950_60 };
            let (syms, sub) = d.diag();
            (syms, sub, false)
        } else {
            (0, 0, false)
        }
    }

    /// Take the latest 24-bin packed 4-bit panafall column (RAD950 200 ms or RTCW CW 100 ms cadence).
    pub fn rad950_take_wf(&mut self) -> Option<[u8; 12]> {
        if self.kind == DecoderKind::Rad950 {
            let d = unsafe { &mut *self.inner.rad950 };
            d.take_wf_col()
        } else if self.kind == DecoderKind::Rad950_60 {
            let d = unsafe { &mut *self.inner.rad950_60 };
            d.take_waterfall_col()
        } else if self.kind == DecoderKind::Cw {
            let d = unsafe { &mut *self.inner.cw };
            d.spec.take()
        } else {
            None
        }
    }

    /// Finish the current RAD950 slot (or 60 s minute), write decoded frames to `out`, optionally export the best
    /// failed-CRC soft candidate for Cooperative RX, and reset for the next slot.
    #[inline(never)]
    pub fn rad950_finish_with_soft(
        &mut self,
        out: &mut [urad950::Decoded; urad950::MAX_DECODED],
        soft: Option<&mut Option<urad950::SoftCandidate>>,
    ) -> usize {
        self.rad950_finish_with_stats(out, soft).0
    }

    pub fn rad950_finish_with_stats(
        &mut self,
        out: &mut [urad950::Decoded; urad950::MAX_DECODED],
        soft: Option<&mut Option<urad950::SoftCandidate>>,
    ) -> (usize, Option<urad950::FinishStats>) {
        if self.kind == DecoderKind::Rad950 {
            let d = unsafe { &mut *self.inner.rad950 };
            let n = d.finish_slot_with_soft(out, soft);
            d.reset();
            (n, None)
        } else if self.kind == DecoderKind::Rad950_60 {
            let d = unsafe { &mut *self.inner.rad950_60 };
            let stats = d.finish_minute_with_stats(out);
            d.reset();
            (stats.total_decoded as usize, Some(stats))
        } else {
            (0, None)
        }
    }

    /// Reset the RAD950 receiver for a fresh RX slot/minute without running LDPC decode (used after a TX slot).
    pub fn rad950_reset(&mut self) {
        if self.kind == DecoderKind::Rad950 {
            unsafe { (*self.inner.rad950).reset() };
        } else if self.kind == DecoderKind::Rad950_60 {
            unsafe { (*self.inner.rad950_60).reset() };
        }
    }

    /// Advance `RAD950-60` to the next 10-second sub-burst inside the active 60 s RX minute.
    pub fn rad950_start_subslot(&mut self) {
        if self.kind == DecoderKind::Rad950_60 {
            unsafe { (*self.inner.rad950_60).start_subslot() };
        }
    }

    /// Finish the current 10 s RAD950 UTC slot, write decoded frames to `out`, and reset for the next slot.
    #[inline(never)]
    pub fn rad950_finish(&mut self, out: &mut [urad950::Decoded; urad950::MAX_DECODED]) -> usize {
        self.rad950_finish_with_soft(out, None)
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
            DecoderKind::Rad950 => {
                let d = unsafe { &mut *self.inner.rad950 };
                d.push_samples(block);
            }
            DecoderKind::Rad950_60 => {
                let d = unsafe { &mut *self.inner.rad950_60 };
                d.push_samples(block);
            }
            DecoderKind::Wwv => {
                let d = unsafe { &mut *self.inner.wwv };
                if let Some(st) = d.feed(block) {
                    found(Found::Wwv(st));
                }
            }
            DecoderKind::Cw => {
                let d = unsafe { &mut *self.inner.cw };
                d.spec.push_24k(block);
                let mut chars = [0u8; 8];
                let mut n = 0usize;
                d.dec.feed(block, |ch| {
                    if n < chars.len() {
                        chars[n] = ch;
                        n += 1;
                    }
                });
                let meter = d.dec.meter;
                if n == 0 {
                    found(Found::Cw { ch: None, meter });
                } else {
                    for &ch in &chars[..n] {
                        found(Found::Cw { ch: Some(ch), meter });
                    }
                }
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
