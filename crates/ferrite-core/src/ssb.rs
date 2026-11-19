//! The SI4732's SSB patch, as the flasher bundles it and as the store keeps it (HF-18 to HF-21; firmware-design
//! "SSB patch store"). Ferrite never ships the patch: the flasher (`--ssb-from`) takes it from the owner's own stock
//! file and appends it to the image; the firmware copies it to SPI flash (layout::SSB_PATCH) at boot.

use crate::layout::SSB_PATCH;
use crate::sha256::Sha256;

/// The header in front of a patch, in the flasher's bundle and in SPI flash: 64 bytes.
pub const HEADER: usize = 64;
/// The bundle's magic (`tools/ssbpatch.py`).
pub const BUNDLE_MAGIC: [u8; 8] = *b"FERRSSB1";
/// The stored copy's magic, at SSB_PATCH.start.
pub const STORE_MAGIC: [u8; 4] = *b"SSB1";
/// Where it came from (the stored header's source field).
pub const SOURCE_FLASHER: u8 = 1;
pub const SOURCE_LINK: u8 = 2;
/// The SHA-256 of the patch in stock V0.18-V0.29 (`tools/ssbpatch.py` KNOWN_SHA256): a different one is accepted
/// but reported (HF-18).
pub const KNOWN_SHA256: [u8; 32] = [
    0x34, 0x30, 0x76, 0xb9, 0xe4, 0x72, 0x60, 0x56, 0xe8, 0xdb, 0x46, 0x98, 0x34, 0xf2, 0x61, 0x0e, 0x56, 0x83, 0x09, 0x5b,
    0xf1, 0x9b, 0x41, 0x67, 0x4e, 0xb5, 0xd5, 0xbf, 0x0f, 0xa3, 0xdc, 0x9f,
];

/// A patch's length (bytes) and hash, from either header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Patch {
    pub len: u32,
    pub sha: [u8; 32],
}

fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// The length a patch may have: whole 8-byte records, at least 500 of them (`ssbpatch.py`'s floor), fitting the
/// SPI region after its header.
fn len_ok(len: u32, records: u32) -> bool {
    len.is_multiple_of(8) && records == len / 8 && records >= 500 && (HEADER as u32 + len) <= SSB_PATCH.end - SSB_PATCH.start
}

/// The flasher's bundle header: b"FERRSSB1", u32 length, u32 records, u32 reserved, SHA-256.
pub fn bundle(h: &[u8; HEADER]) -> Option<Patch> {
    (h[..8] == BUNDLE_MAGIC && len_ok(u32le(h, 8), u32le(h, 12))).then(|| Patch { len: u32le(h, 8), sha: h[20..52].try_into().unwrap() })
}

/// The stored header: b"SSB1", u32 length, u32 records, source byte, 3 spare, SHA-256, zeros to 64.
pub fn stored(h: &[u8; HEADER]) -> Option<Patch> {
    (h[..4] == STORE_MAGIC && len_ok(u32le(h, 4), u32le(h, 8))).then(|| Patch { len: u32le(h, 4), sha: h[16..48].try_into().unwrap() })
}

/// The stored header for `p`.
pub fn store_header(p: &Patch, source: u8) -> [u8; HEADER] {
    let mut h = [0u8; HEADER];
    h[..4].copy_from_slice(&STORE_MAGIC);
    h[4..8].copy_from_slice(&p.len.to_le_bytes());
    h[8..12].copy_from_slice(&(p.len / 8).to_le_bytes());
    h[12] = source;
    h[16..48].copy_from_slice(&p.sha);
    h
}

/// The patch's records start with 0x15 (a block's first) or 0x16 (the rest), and the first is a 0x15: the chip's
/// download framing. A cheap check before trusting a hash match.
pub fn records_ok(patch: &[u8]) -> bool {
    patch.len().is_multiple_of(8) && patch.first() == Some(&0x15) && patch.chunks(8).all(|r| r[0] == 0x15 || r[0] == 0x16)
}

/// The SHA-256 of a patch fed in pieces (reads from SPI or MCU flash in chunks).
pub fn hasher() -> Sha256 {
    Sha256::new()
}

/// HF-19: CW is received as USB this far below the frequency shown, so a carrier on it is heard as a tone of
/// this pitch (Hz).
pub const CW_PITCH_HZ: u32 = 700;

/// HF-19: how the SI4732 tunes a frequency (10 Hz units) in SSB: the whole kHz for its tune command, the
/// sideband (true = USB; CW is USB), and the rest as the BFO offset in Hz, so a step under 1 kHz only moves the
/// BFO. CW tunes [`CW_PITCH_HZ`] below. Bench measurement on USB (`SI4732` `SSB_BFO`) showed positive BFO
/// lowers zero-beat RF (`f_tune - BFO`, raising USB audio frequency), opposite to the sign assumed here.
pub fn tune(freq: crate::freq::Freq10, mode_usb: bool, cw: bool) -> (u16, bool, i16) {
    let f = if cw { freq.saturating_sub(CW_PITCH_HZ / 10) } else { freq };
    ((f / 100) as u16, mode_usb || cw, (f % 100 * 10) as i16)
}

/// HF-19: SSB_MODE's audio bandwidth code for the HF screen's SSB filter (its order: 3, 2.2, 1.2, 0.5 kHz; the
/// chip's codes 0 = 1.2, 1 = 2.2, 2 = 3.0, 3 = 4.0, 4 = 0.5, 5 = 1.0 kHz).
pub fn audio_bw(filter: u8) -> u8 {
    [2, 1, 0, 4][filter as usize % 4]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HF-19.1: the kHz, the sideband and the BFO rest; CW 700 Hz below on USB.
    #[test]
    fn tune_splits_khz_and_bfo() {
        assert_eq!(tune(707_400, true, false), (7074, true, 0));
        assert_eq!(tune(707_453, false, false), (7074, false, 530));
        assert_eq!(tune(1_405_000, false, true), (14049, true, 300), "CW: 14.050 heard 700 Hz low, on USB");
        assert_eq!(tune(14, true, true), (0, true, 0), "never below 0");
    }

    /// HF-19.1: the screen's SSB filters to the chip's codes.
    #[test]
    fn audio_bw_codes() {
        assert_eq!([0, 1, 2, 3].map(audio_bw), [2, 1, 0, 4]);
    }

    fn fake(n: usize) -> std::vec::Vec<u8> {
        (0..n * 8).map(|i| if i % 8 == 0 { if i % 64 == 0 { 0x15 } else { 0x16 } } else { (i * 7) as u8 }).collect()
    }

    /// HF-18: the flasher's header (as tools/ssbpatch.py writes it) parses; the stored header round-trips; wrong
    /// magic, odd lengths, too few records or too long a patch are refused.
    #[test]
    fn headers() {
        let p = fake(1979);
        let sha = crate::sha256::sha256(&p);
        let mut b = [0u8; HEADER];
        b[..8].copy_from_slice(b"FERRSSB1");
        b[8..12].copy_from_slice(&(p.len() as u32).to_le_bytes());
        b[12..16].copy_from_slice(&1979u32.to_le_bytes());
        b[20..52].copy_from_slice(&sha);
        let got = bundle(&b).unwrap();
        assert_eq!(got, Patch { len: 15_832, sha });
        assert_eq!(stored(&store_header(&got, SOURCE_FLASHER)), Some(got));
        let mut bad = b;
        bad[0] = b'X';
        assert!(bundle(&bad).is_none());
        let mut odd = b;
        odd[8] = 3;
        assert!(bundle(&odd).is_none());
        let mut few = b;
        few[8..12].copy_from_slice(&800u32.to_le_bytes());
        few[12..16].copy_from_slice(&100u32.to_le_bytes());
        assert!(bundle(&few).is_none());
        let mut big = b;
        big[8..12].copy_from_slice(&(16_384u32).to_le_bytes());
        big[12..16].copy_from_slice(&2048u32.to_le_bytes());
        assert!(bundle(&big).is_none(), "the header and patch fit the 16 KB region");
        assert!(records_ok(&p));
        assert!(!records_ok(&p[8..]), "starts with a 0x15");
    }
}
