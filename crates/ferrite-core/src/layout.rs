//! The SPI flash map, layout version 3 (decision 333; `docs/firmware-design.md`, "SPI flash layout"; the
//! developer page `docs/dev/spi-flash.html`). The addresses are constants: no region table at runtime. A layout
//! or record-format change is migrated by Ferrite Console during an update (LINK-59 – LINK-62): it reads the old
//! layout's data over the link, keeps it on the PC, translates it, writes the new layout and stamps its version
//! in the layout header last. A firmware that finds another layout's header leaves the data alone (UPD-16).
//!
//! Ferrite writes only inside [`WRITABLE`]: the data range less factory calibration, the bootloader's fonts and its
//! update area, so none of them can be written by any bug above the flash driver (uradio `w25q`), which refuses everything
//! else. The first 64 KB block holds calibration, so it is only ever erased a 4 KB sector at a time (INST-7: the
//! driver has no block erase at all).

use core::ops::Range;

/// The layout this firmware stores (decision 333). 2 was the fixed map of Oct 4, 2026 (its header at
/// [`V2_HEADER`]); 1 was never released.
pub const LAYOUT_VERSION: u16 = 3;

/// The layout header: magic, layout version, the installer's bytes, CRC (ferrite-store `store`).
pub const LAYOUT_HEADER: Range<u32> = 0x00_0000..0x00_1000;
/// Settings and slots, copies A and B.
pub const SETTINGS: Range<u32> = 0x00_1000..0x00_3000;
/// User trims (CAL-17): two alternating copies.
pub const TRIMS: Range<u32> = 0x00_3000..0x00_4000;
/// 16 range presets × 224 B, then the DTMF memories and their names, in one sector.
pub const PRESETS: Range<u32> = 0x00_4000..0x00_5000;
/// HF mode's state (planned).
pub const HF_STATE: Range<u32> = 0x00_5000..0x00_6000;
/// Satellites (SAT-2, chapter 28a; ferrite-store `sats`): 32 records of 192 B, the info and the per-satellite
/// choices in logs after them, 16 records a sector.
pub const SATELLITES: Range<u32> = 0x00_6000..0x00_8000;
/// The SI4732 SSB patch (15,832 B with its 64-byte header): imported from the owner's stock firmware, never shipped.
pub const SSB_PATCH: Range<u32> = 0x00_8000..0x00_C000;
/// SOL950 station profile & settings (copies A and B, 4 KiB each).
pub const SOL950_CFG: Range<u32> = 0x00_C000..0x00_E000;
/// The migration mark (decision 339, LINK-62): while Console moves the chip's data raw, a mark here says the move
/// isn't finished, so a restart at any point waits for Console (UPD-16) and never formats; the stamp clears it.
/// Records of 16 bytes (ferrite-store `store`), a new one in the next erased place each move.
pub const MIGRATE_MARK: Range<u32> = 0x00_E000..0x00_F000;
/// Factory calibration: read only, never erased or written; it stays at stock's address (going back to stock needs
/// it there).
pub const CALIBRATION: Range<u32> = 0x00_F000..0x01_0000;
/// 64 sectors of 63 channels × 64 B (4,032 slots, 4,000 used); each sector's last 64 bytes stay free.
pub const CHANNELS: Range<u32> = 0x01_0000..0x05_0000;
/// 5 sectors of 127 zones × 32 B (635 slots, 512 used); each sector's last 32 bytes stay free.
pub const ZONES: Range<u32> = 0x05_0000..0x05_5000;
/// The APRS settings and quick messages, copies A and B, and a sector to grow into.
pub const APRS: Range<u32> = 0x05_5000..0x05_8000;
/// APRS contacts (APRS-37): 1,016 in callsign order, 127 a sector.
pub const CONTACTS: Range<u32> = 0x05_8000..0x06_0000;
/// The APRS heard list kept across restarts (APRS-4, decision 282): a ring of 496 station rows of 128 B, 31 a
/// sector.
pub const HEARD: Range<u32> = 0x06_0000..0x07_0000;
/// The APRS message ring: 496 messages of 128 B, 31 a sector.
pub const MESSAGES: Range<u32> = 0x07_0000..0x08_0000;
/// GPS waypoints (GPS-10): 2,032, 127 a sector.
pub const WAYPOINTS: Range<u32> = 0x08_0000..0x09_0000;
/// The text pack (English): long text kept off the MCU's flash (planned).
pub const TEXT_PACK: Range<u32> = 0x09_0000..0x0A_0000;
/// Held for the packet mailbox (chapter 38).
pub const MAILBOX: Range<u32> = 0x0A_0000..0x0B_0000;
/// Predictive text's words (decision 345): an SPI part, with room for a bigger dictionary.
pub const DICTIONARY: Range<u32> = 0x0B_0000..0x0C_0000;
/// SOL950 durable append-only contact journal: 16 sectors (64 KiB) of 64 B records (1,008 contacts, non-overwriting).
pub const SOL950_LOG: Range<u32> = 0x0C_0000..0x0D_0000;
/// Free (what goes here is for a later decision).
pub const FREE: Range<u32> = 0x0D_0000..0x0E_0000;
/// The stock bootloader's fonts (decision 404): its update screen draws "UPDATE" and its messages with the GB2312
/// glyphs from 0x0E0000 (75 bytes each, to 0x15BED0) and the ASCII ones at 0x15C000 (39 bytes each), read from here
/// at every update (rt950pro_bootloader.bin 0x08002212, 0x08002232). Never written: not in [`WRITABLE`]. An SSTV
/// test build put pictures here and black's update screen lost its text (Nov 16, 2026).
pub const BOOT_FONTS: Range<u32> = 0x0E_0000..0x15_D000;
/// Free, past the bootloader's fonts.
pub const FREE_HIGH: Range<u32> = 0x15_D000..0x1C_0000;
/// Language packs: English plus one other at a time (planned).
pub const LANGUAGES: Range<u32> = 0x1C_0000..0x24_0000;
/// The boot animation and custom screens (planned).
pub const SCREENS: Range<u32> = 0x24_0000..0x28_0000;
/// Fonts at the bigger sizes (planned).
pub const FONTS: Range<u32> = 0x28_0000..0x2C_0000;
/// The event log (INBOX-6): a ring of 992 records of 128 B, 31 a sector.
pub const EVENT_LOG: Range<u32> = 0x2C_0000..0x2E_0000;
/// The scratch ring (decision 227): 32 sectors taking turns at holding a data sector's new content while it is
/// rewritten, with a footer saying where it goes (ferrite-store `scratch`), so no one sector takes every rewrite's
/// erase. The free last bytes of every data sector make room for that footer.
pub const SCRATCH_RING: Range<u32> = 0x2E_0000..0x30_0000;
/// The bootloader reflashes the MCU from here if it finds its header: never written.
pub const UPDATE_AREA: Range<u32> = 0x30_0000..0x40_0000;

/// Where layout 2 kept its header (in stock's old codeplug space): a valid one there, with no layout 3 header,
/// means a radio updated from a layout 2 firmware, whose data waits for Console to move it (UPD-16).
pub const V2_HEADER: Range<u32> = 0x02_C000..0x02_D000;

/// The layout `HELLO` and `INFO` say the chip's data is in (LINK-62) while it is half moved: a raw write was made
/// this power-on (LINK-61), or a move was cut before its stamp, or the chip holds Ferrite's records with no header
/// (UPD-16). Only Console's copy of the old data can finish it; the chip itself is never read as a backup.
pub const LAYOUT_MOVING: u16 = 255;

/// Everything Ferrite may erase or program: the data range (the layout header to the end of the scratch ring)
/// less calibration and the bootloader's fonts. The update area is past its end. The link's raw data writes
/// (`DATA_WRITE`, LINK-60) and the way back to stock (`STOCK_RESTORE`) are held to the same ranges.
pub static WRITABLE: [Range<u32>; 3] =
    [LAYOUT_HEADER.start..CALIBRATION.start, CALIBRATION.end..BOOT_FONTS.start, BOOT_FONTS.end..UPDATE_AREA.start];

/// The data range the link's raw data reads cover (LINK-59): the whole chip below the update area, calibration
/// included (read only; a backup has it).
pub const DATA: Range<u32> = 0..UPDATE_AREA.start;

/// Every region in address order, with its name as the developer page and the build report show it.
pub const REGIONS: [(&str, Range<u32>); 30] = [
    ("Layout header", LAYOUT_HEADER),
    ("Settings and slots", SETTINGS),
    ("User trims", TRIMS),
    ("Range presets and DTMF memories", PRESETS),
    ("HF state (planned)", HF_STATE),
    ("Satellites", SATELLITES),
    ("SSB patch", SSB_PATCH),
    ("SOL950 profile", SOL950_CFG),
    ("Migration mark", MIGRATE_MARK),
    ("Factory calibration", CALIBRATION),
    ("Channels", CHANNELS),
    ("Zones", ZONES),
    ("APRS settings and quick messages", APRS),
    ("APRS contacts", CONTACTS),
    ("APRS heard list", HEARD),
    ("APRS messages", MESSAGES),
    ("GPS waypoints", WAYPOINTS),
    ("Text pack", TEXT_PACK),
    ("Packet mailbox (held)", MAILBOX),
    ("Dictionary", DICTIONARY),
    ("SOL950 contact log", SOL950_LOG),
    ("Free", FREE),
    ("Stock bootloader's fonts (kept)", BOOT_FONTS),
    ("Free", FREE_HIGH),
    ("Language packs", LANGUAGES),
    ("Boot animation and screens", SCREENS),
    ("Fonts", FONTS),
    ("Event log", EVENT_LOG),
    ("Scratch ring", SCRATCH_RING),
    ("Update area", UPDATE_AREA),
];

/// Whether `len` bytes at `addr` lie wholly inside one of the writable ranges (the link's raw writes, LINK-60).
pub fn writable(addr: u32, len: u32) -> bool {
    let end = addr as u64 + len as u64;
    WRITABLE.iter().any(|r| addr >= r.start && end <= r.end as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlaps(a: &Range<u32>, b: &Range<u32>) -> bool {
        a.start < b.end && b.start < a.end
    }

    /// SAFE-5, UPD-7, decision 333: calibration and the update area are never writable; nothing else below the
    /// update area is out of reach.
    #[test]
    fn protected_ranges_are_not_writable() {
        for w in &WRITABLE {
            for p in [CALIBRATION, BOOT_FONTS, UPDATE_AREA] {
                assert!(!overlaps(w, &p), "{w:x?} overlaps {p:x?}");
            }
        }
        assert!(writable(0, 0x1000) && writable(0xE000, 0x1000) && writable(0x2F_F000, 0x1000));
        assert!(!writable(0xE000, 0x1001) && !writable(0xF000, 1) && !writable(0xFFFF, 2) && !writable(0x30_0000, 1));
        assert!(!writable(0x2F_FFFF, 2) && !writable(u32::MAX, 2), "no wrap");
        // decision 404: the bootloader's fonts, its ASCII glyphs ("UPDATE") at 0x15C000 above all
        assert!(!writable(0x0E_0000, 1) && !writable(0x15_C000, 0x1000) && !writable(0x0D_F000, 0x2000));
        assert!(writable(0x0D_F000, 0x1000) && writable(0x15_D000, 0x1000));
    }

    /// Decision 333's table: the regions tile the chip from 0 to 4 MB in order, each on a sector boundary; the
    /// big ones on 64 KB boundaries; the counts each holds.
    #[test]
    fn regions_tile_the_chip() {
        for p in REGIONS.windows(2) {
            assert_eq!(p[0].1.end, p[1].1.start, "{} then {}", p[0].0, p[1].0);
        }
        assert_eq!((REGIONS[0].1.start, REGIONS[REGIONS.len() - 1].1.end), (0, 4 << 20));
        for (n, r) in &REGIONS {
            assert!(r.start % 4096 == 0 && r.end % 4096 == 0, "{n}: sector aligned");
        }
        for r in [CHANNELS, HEARD, MESSAGES, WAYPOINTS, TEXT_PACK, MAILBOX, DICTIONARY, FREE, BOOT_FONTS, LANGUAGES, SCREENS, FONTS, EVENT_LOG, SCRATCH_RING, UPDATE_AREA] {
            assert_eq!(r.start % 0x1_0000, 0, "{r:x?}: on a 64 KB boundary");
        }
        assert_eq!(CALIBRATION, 0xF000..0x1_0000, "calibration stays at stock's address");
        assert_eq!(UPDATE_AREA, 0x30_0000..0x40_0000);
        let sectors = |r: &Range<u32>| r.len() as u32 / 4096;
        assert!(sectors(&CHANNELS) * 63 >= 4000, "4,000 channels at 63 a sector");
        assert!(sectors(&ZONES) * 127 >= 512, "512 zones at 127 a sector");
        assert_eq!((sectors(&CONTACTS) * 127, sectors(&WAYPOINTS) * 127), (1016, 2032));
        assert_eq!((sectors(&HEARD) * 31, sectors(&MESSAGES) * 31, sectors(&EVENT_LOG) * 31), (496, 496, 992));
        assert_eq!(sectors(&SCRATCH_RING), 32);
        assert_eq!(sectors(&SOL950_LOG) * 63, 1008);
        assert_eq!((SSB_PATCH.len(), LANGUAGES.len(), SOL950_LOG.len() + FREE.len() + FREE_HIGH.len()), (16 << 10, 512 << 10, (1024 << 10) - BOOT_FONTS.len()));
        assert!(!overlaps(&V2_HEADER, &LAYOUT_HEADER) && CHANNELS.contains(&V2_HEADER.start), "layout 2's header sits where channels go now");
        assert_eq!(DATA.end, UPDATE_AREA.start);
    }
}
