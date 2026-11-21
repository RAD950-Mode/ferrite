//! Durable RAD950 SPI flash storage:
//! - [`RAD950_LOG`](ferrite_core::layout::RAD950_LOG) (`0x0C_0000..0x0D_0000`, 64 KiB): append-only,
//!   non-overwriting journal of `1,008` versioned 64-byte CRC-32 contact records (`63` per 4 KiB sector).
//!   Returns [`AppendOutcome::LogFull`] when full; discards torn/corrupt records on mount; enforces
//!   write-once idempotency per `(peer_id, slot / 30)` cooldown window.
//! - [`RAD950_CFG`](ferrite_core::layout::RAD950_CFG) (`0x00_C000..0x00_E000`, two 4 KiB A/B sectors):
//!   power-cut-safe station identity and radio configuration.

use crate::{Crc32, Nor, NorError, Result, SECTOR};
use ferrite_core::layout::{RAD950_CFG, RAD950_LOG};

pub const REC_BYTES: usize = 64;
pub const PER_SECTOR: usize = 63; // 63 * 64 = 4,032 B; last 64 B of each 4 KiB sector free
pub const SECTORS: usize = ((RAD950_LOG.end - RAD950_LOG.start) / SECTOR) as usize; // 16
pub const CAPACITY: usize = SECTORS * PER_SECTOR; // 1,008 durable contacts

const LOG_MAGIC: [u8; 2] = *b"SL";
const LOG_VERSION: u8 = 1;
const CFG_MAGIC: [u8; 2] = *b"SC";
const CFG_VERSION_V1: u8 = 1;
const CFG_VERSION: u8 = 2;

/// Durable 64-byte RAD950 contact record stored in `RAD950_LOG`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rad950Contact {
    pub utc_sec: u32,
    pub dial_hz10: u32,
    pub peer_id: [u8; 16],
    pub my_id: [u8; 16],
    pub peer_grid: [u8; 4],
    pub my_grid: [u8; 4],
    pub rx_snr_db: i8,
    pub tx_snr_db: i8,
    /// `0` = Unset, `1` = Gps, `2` = Manual.
    pub my_grid_src: u8,
    /// Bit 0: `peer_decorated`, bit 1: `my_decorated`.
    pub flags: u8,
    pub slot: u32,
    pub seq: u32,
}

impl Rad950Contact {
    pub const EMPTY: Self = Self {
        utc_sec: 0,
        dial_hz10: 0,
        peer_id: [0; 16],
        my_id: [0; 16],
        peer_grid: *b"----",
        my_grid: *b"----",
        rx_snr_db: 0,
        tx_snr_db: 0,
        my_grid_src: 0,
        flags: 0,
        slot: 0,
        seq: 0,
    };

    pub fn encode(&self) -> [u8; REC_BYTES] {
        let mut b = [0u8; REC_BYTES];
        b[0..2].copy_from_slice(&LOG_MAGIC);
        b[2] = LOG_VERSION;
        b[3] = self.flags;
        b[4..8].copy_from_slice(&self.utc_sec.to_le_bytes());
        b[8..12].copy_from_slice(&self.dial_hz10.to_le_bytes());
        b[12..28].copy_from_slice(&self.peer_id);
        b[28..44].copy_from_slice(&self.my_id);
        b[44..48].copy_from_slice(&self.peer_grid);
        b[48..52].copy_from_slice(&self.my_grid);
        b[52] = self.rx_snr_db as u8;
        b[53] = self.tx_snr_db as u8;
        b[54] = self.my_grid_src;
        b[55] = 0; // reserved
        b[56..60].copy_from_slice(&self.slot.to_le_bytes());
        // Note: `seq` is implied by physical slot order (1-based) to keep 64 B exact with 4 B CRC-32.
        let mut c = Crc32::new();
        c.update(&b[..60]);
        b[60..64].copy_from_slice(&c.get().to_le_bytes());
        b
    }

    pub fn decode(b: &[u8; REC_BYTES], seq: u32) -> Option<Self> {
        if b[0..2] != LOG_MAGIC || b[2] != LOG_VERSION {
            return None;
        }
        let mut c = Crc32::new();
        c.update(&b[..60]);
        let want = u32::from_le_bytes([b[60], b[61], b[62], b[63]]);
        if c.get() != want {
            return None;
        }
        let mut peer_id = [0u8; 16];
        let mut my_id = [0u8; 16];
        let mut peer_grid = [0u8; 4];
        let mut my_grid = [0u8; 4];
        peer_id.copy_from_slice(&b[12..28]);
        my_id.copy_from_slice(&b[28..44]);
        peer_grid.copy_from_slice(&b[44..48]);
        my_grid.copy_from_slice(&b[48..52]);
        Some(Self {
            utc_sec: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            dial_hz10: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            peer_id,
            my_id,
            peer_grid,
            my_grid,
            rx_snr_db: b[52] as i8,
            tx_snr_db: b[53] as i8,
            my_grid_src: b[54],
            flags: b[3],
            slot: u32::from_le_bytes([b[56], b[57], b[58], b[59]]),
            seq,
        })
    }

    /// Contact idempotency key: `(peer_id, slot / 30)` (write-once within a 5-minute cooldown window).
    pub fn dedup_key(&self) -> ([u8; 16], u32) {
        (self.peer_id, self.slot / 30)
    }

    pub fn peer_str_bytes(&self) -> &[u8] {
        let end = self.peer_id.iter().position(|&x| x == 0).unwrap_or(self.peer_id.len());
        &self.peer_id[..end]
    }
}

/// Outcome of attempting to append a contact to `RAD950_LOG`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppendOutcome {
    /// Durably written and verified; returns the 1-based record sequence number.
    Saved(u32),
    /// Contact already persisted in the same cooldown window (idempotent no-op).
    Duplicate,
    /// Journal reached `1,008` slots; older contacts are preserved and never overwritten.
    LogFull,
}

/// Aggregated statistics for `All saved` or `Today (UTC)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rad950Stats {
    pub contacts: u16,
    pub unique_stations: u16,
    pub unique_grids: u16,
    pub best_rx_snr: i8,
    pub best_tx_snr: i8,
    pub has_snr: bool,
}

/// Parse a 4-char Maidenhead grid into `(field_idx 0..324, square_idx 0..100)`.
/// `field_idx = lon_field * 18 + lat_field`, `square_idx = lon_sq * 10 + lat_sq`.
pub fn grid4_to_field_square(g: &[u8; 4]) -> Option<(u16, u8)> {
    let lf = g[0].to_ascii_uppercase();
    let bf = g[1].to_ascii_uppercase();
    if !(b'A'..=b'R').contains(&lf) || !(b'A'..=b'R').contains(&bf) {
        return None;
    }
    if !g[2].is_ascii_digit() || !g[3].is_ascii_digit() {
        return None;
    }
    let lon_f = (lf - b'A') as u16;
    let lat_f = (bf - b'A') as u16;
    let lon_s = g[2] - b'0';
    let lat_s = g[3] - b'0';
    Some((lon_f * 18 + lat_f, lon_s * 10 + lat_s))
}

/// Format a Maidenhead field index (`0..324`) as `[lon_letter, lat_letter]`.
pub fn field_letters(field_idx: u16) -> [u8; 2] {
    let lon_f = (field_idx / 18).min(17) as u8;
    let lat_f = (field_idx % 18) as u8;
    [b'A' + lon_f, b'A' + lat_f]
}

fn addr_of(slot_idx: usize) -> u32 {
    let sec = slot_idx / PER_SECTOR;
    let off = slot_idx % PER_SECTOR;
    RAD950_LOG.start + (sec as u32) * SECTOR + (off * REC_BYTES) as u32
}

fn is_erased(b: &[u8]) -> bool {
    b.iter().all(|&x| x == 0xFF)
}

async fn wait_nor<N: Nor>(flash: &mut N) -> Result {
    for _ in 0..3000 {
        if !flash.busy()? {
            return Ok(());
        }
        N::pause().await;
    }
    Err(NorError)
}

/// Short-lived `Nor` adapter over `&crate::io::Flash<N>` that drops its `RefMut` before every `N::pause().await`.
pub struct FlashNor<'a, N: Nor>(pub &'a crate::io::Flash<N>);

impl<N: Nor> Nor for FlashNor<'_, N> {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result {
        self.0.nor().read(addr, buf)
    }

    fn start_program(&mut self, addr: u32, data: &[u8]) -> Result {
        self.0.nor().start_program(addr, data)
    }

    fn start_erase(&mut self, addr: u32) -> Result {
        self.0.nor().start_erase(addr)
    }

    fn busy(&mut self) -> Result<bool> {
        self.0.nor().busy()
    }

    async fn pause() {
        N::pause().await;
    }
}

/// Compact RAM index over `RAD950_LOG` (64 B total RAM!).
#[derive(Clone, Copy, Debug)]
pub struct Rad950Journal {
    /// Total valid saved contacts.
    pub count: u16,
    /// Next erased physical slot index (`0..=CAPACITY`). `CAPACITY` means log is full.
    pub next_idx: u16,
    /// Number of torn/corrupt slots skipped on mount.
    pub torn_skipped: u16,
    /// Bitmask of worked Maidenhead fields (`18 * 18 = 324` bits in `11 * 32 = 352` bits).
    pub field_mask: [u32; 11],
}

impl Default for Rad950Journal {
    fn default() -> Self {
        Self::empty()
    }
}

impl Rad950Journal {
    pub const fn empty() -> Self {
        Self {
            count: 0,
            next_idx: 0,
            torn_skipped: 0,
            field_mask: [0; 11],
        }
    }

    pub fn is_full(&self) -> bool {
        self.next_idx as usize >= CAPACITY
    }

    pub fn field_worked(&self, field_idx: u16) -> bool {
        let i = field_idx as usize;
        if i >= 324 {
            return false;
        }
        (self.field_mask[i / 32] & (1u32 << (i % 32))) != 0
    }

    fn mark_field(&mut self, grid: &[u8; 4]) {
        if let Some((f, _)) = grid4_to_field_square(grid) {
            let i = f as usize;
            self.field_mask[i / 32] |= 1u32 << (i % 32);
        }
    }

    /// Mount and rebuild the compact RAM index from SPI flash, discarding any torn/corrupt records.
    pub fn mount(flash: &mut impl Nor) -> Result<Self> {
        let mut j = Self::empty();
        let mut buf = [0u8; REC_BYTES];
        let mut highest_non_erased: Option<usize> = None;
        for idx in 0..CAPACITY {
            flash.read(addr_of(idx), &mut buf)?;
            if is_erased(&buf) {
                continue;
            }
            highest_non_erased = Some(idx);
            if let Some(c) = Rad950Contact::decode(&buf, (idx + 1) as u32) {
                j.count = j.count.saturating_add(1);
                j.mark_field(&c.peer_grid);
            } else {
                j.torn_skipped = j.torn_skipped.saturating_add(1);
            }
        }
        j.next_idx = highest_non_erased.map_or(0, |h| (h + 1) as u16);
        Ok(j)
    }

    /// Append a completed contact durably. Enforces write-once idempotency (within 30 slots / 5 min) and non-overwriting `LogFull`.
    pub async fn append(&mut self, flash: &mut impl Nor, contact: &Rad950Contact) -> Result<AppendOutcome> {
        // Scan backwards over the most recent 32 slots for duplicate peer within the same 5-min cooldown window.
        let mut buf = [0u8; REC_BYTES];
        let end = self.next_idx as usize;
        let start = end.saturating_sub(32);
        for idx in (start..end).rev() {
            flash.read(addr_of(idx), &mut buf)?;
            if let Some(existing) = Rad950Contact::decode(&buf, (idx + 1) as u32) {
                if existing.peer_id == contact.peer_id && contact.slot.abs_diff(existing.slot) < 30 {
                    return Ok(AppendOutcome::Duplicate);
                }
            }
        }
        if self.is_full() {
            return Ok(AppendOutcome::LogFull);
        }
        let slot_idx = self.next_idx as usize;
        let raw = contact.encode();
        let addr = addr_of(slot_idx);
        flash.start_program(addr, &raw)?;
        wait_nor(flash).await?;
        // Read-back verify before advancing RAM index or reporting Saved.
        let mut verify = [0u8; REC_BYTES];
        flash.read(addr, &mut verify)?;
        if verify != raw {
            return Err(NorError);
        }
        self.next_idx = (slot_idx + 1) as u16;
        self.count = self.count.saturating_add(1);
        self.mark_field(&contact.peer_grid);
        Ok(AppendOutcome::Saved((slot_idx + 1) as u32))
    }

    /// Read newest-first valid contacts into `out`, skipping `skip` matching rows and optionally filtering by `peer_grid`.
    pub fn read_newest(
        &self,
        flash: &mut impl Nor,
        skip: u16,
        grid_filter: Option<[u8; 4]>,
        out: &mut [Option<Rad950Contact>],
    ) -> Result<usize> {
        for slot in out.iter_mut() {
            *slot = None;
        }
        let mut buf = [0u8; REC_BYTES];
        let mut skipped = 0u16;
        let mut written = 0usize;
        for idx in (0..self.next_idx as usize).rev() {
            if written >= out.len() {
                break;
            }
            flash.read(addr_of(idx), &mut buf)?;
            let Some(c) = Rad950Contact::decode(&buf, (idx + 1) as u32) else { continue };
            if let Some(gf) = grid_filter {
                if !c.peer_grid.eq_ignore_ascii_case(&gf) {
                    continue;
                }
            }
            if skipped < skip {
                skipped += 1;
                continue;
            }
            out[written] = Some(c);
            written += 1;
        }
        Ok(written)
    }

    /// Compute `All saved` (`today_utc_day = None`) or `Today (UTC)` (`Some(utc_sec / 86_400)`) statistics.
    pub fn stats(&self, flash: &mut impl Nor, today_utc_day: Option<u32>) -> Result<Rad950Stats> {
        let mut st = Rad950Stats {
            best_rx_snr: i8::MIN,
            best_tx_snr: i8::MIN,
            ..Rad950Stats::default()
        };
        let mut buf = [0u8; REC_BYTES];
        // Compact fixed-size dedup hashes (64-bit FNV) up to CAPACITY on stack? No, keep stack tiny:
        // Use a 256-entry open-addressed hash table for stations & grids (2 KiB stack, exact up to ~200 unique;
        // or scan-dedup directly against earlier flash slots for 100% exactness with O(1) RAM!).
        // Exactness with zero extra RAM: for each valid contact `idx`, check if an earlier matching slot had the same peer/grid.
        let mut prev_buf = [0u8; REC_BYTES];
        for idx in 0..self.next_idx as usize {
            flash.read(addr_of(idx), &mut buf)?;
            let Some(c) = Rad950Contact::decode(&buf, (idx + 1) as u32) else { continue };
            if let Some(day) = today_utc_day {
                if c.utc_sec / 86_400 != day {
                    continue;
                }
            }
            st.contacts = st.contacts.saturating_add(1);
            if !st.has_snr {
                st.best_rx_snr = c.rx_snr_db;
                st.best_tx_snr = c.tx_snr_db;
                st.has_snr = true;
            } else {
                st.best_rx_snr = st.best_rx_snr.max(c.rx_snr_db);
                st.best_tx_snr = st.best_tx_snr.max(c.tx_snr_db);
            }
            let mut seen_peer = false;
            let has_valid_grid = grid4_to_field_square(&c.peer_grid).is_some();
            let mut seen_grid = !has_valid_grid;
            for prev in 0..idx {
                if seen_peer && seen_grid {
                    break;
                }
                flash.read(addr_of(prev), &mut prev_buf)?;
                let Some(p) = Rad950Contact::decode(&prev_buf, (prev + 1) as u32) else { continue };
                if let Some(day) = today_utc_day {
                    if p.utc_sec / 86_400 != day {
                        continue;
                    }
                }
                if p.peer_id == c.peer_id {
                    seen_peer = true;
                }
                if has_valid_grid && p.peer_grid.eq_ignore_ascii_case(&c.peer_grid) {
                    seen_grid = true;
                }
            }
            if !seen_peer {
                st.unique_stations = st.unique_stations.saturating_add(1);
            }
            if !seen_grid {
                st.unique_grids = st.unique_grids.saturating_add(1);
            }
        }
        if !st.has_snr {
            st.best_rx_snr = 0;
            st.best_tx_snr = 0;
        }
        Ok(st)
    }

    /// Count saved contacts in each of the `10 x 10` squares (`lon_sq * 10 + lat_sq`) inside `field_idx` (`0..324`).
    pub fn field_squares(&self, flash: &mut impl Nor, field_idx: u16) -> Result<[u16; 100]> {
        let mut counts = [0u16; 100];
        let mut buf = [0u8; REC_BYTES];
        for idx in 0..self.next_idx as usize {
            flash.read(addr_of(idx), &mut buf)?;
            let Some(c) = Rad950Contact::decode(&buf, (idx + 1) as u32) else { continue };
            if let Some((f, sq)) = grid4_to_field_square(&c.peer_grid) {
                if f == field_idx {
                    counts[sq as usize] = counts[sq as usize].saturating_add(1);
                }
            }
        }
        Ok(counts)
    }
}

// ---- RAD950_CFG A/B profile store ----

const CFG_BYTES: usize = 40;

/// Persisted RAD950 station profile stored in `RAD950_CFG` (A/B sectors).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rad950CfgRecord {
    pub gen: u32,
    pub base_call: [u8; 6],
    pub call_len: u8,
    pub prefix: [u8; 4],
    pub prefix_len: u8,
    pub suffix: u8,
    pub use_gps_grid: bool,
    pub manual_grid: [u8; 4],
    pub dial_idx: u8,
    pub rx_corr_hz: i16,
    pub tx_power: u8,
    /// Cooperative RX mode (`0 = Off`, `1 = Capture`; reserved `2 = Lead`, `3 = Help`).
    pub coop_rx: u8,
    /// Cooperative RX link transport (`0 = USB`, `1 = Bluetooth`).
    pub coop_link: u8,
    /// Station Card rig catalog ID (`1` = RT-950 Pro).
    pub card_rig: u16,
    /// Station Card antenna catalog ID (`1` = dipole).
    pub card_ant: u8,
    /// Station Card antenna height in whole meters (`127` = unset).
    pub card_height_m: u8,
    /// Station Card external amplifier flag.
    pub card_amp: bool,
    /// Ad-hoc Rendezvous enabled band mask (`bit 0=10m, 1=12m, 2=15m, 3=17m`).
    pub rdv_band_mask: u8,
    /// Ad-hoc Rendezvous `Follow schedule` toggle.
    pub rdv_follow: bool,
    /// Play optional alert chimes when a new station/grid is heard or worked.
    pub alert_tones: bool,
}

impl Rad950CfgRecord {
    pub fn encode(&self) -> [u8; CFG_BYTES] {
        let mut b = [0u8; CFG_BYTES];
        b[0..2].copy_from_slice(&CFG_MAGIC);
        b[2] = CFG_VERSION;
        b[3] = (self.use_gps_grid as u8)
            | ((self.tx_power.min(2)) << 1)
            | ((self.coop_rx & 3) << 3)
            | ((self.coop_link & 1) << 5);
        b[4..8].copy_from_slice(&self.gen.to_le_bytes());
        b[8..14].copy_from_slice(&self.base_call);
        b[14] = self.call_len;
        b[15] = self.prefix_len;
        b[16..20].copy_from_slice(&self.prefix);
        b[20] = self.suffix;
        b[21] = self.dial_idx;
        b[22..26].copy_from_slice(&self.manual_grid);
        b[26..28].copy_from_slice(&self.rx_corr_hz.to_le_bytes());
        b[28..30].copy_from_slice(&self.card_rig.to_le_bytes());
        b[30] = self.card_ant;
        b[31] = self.card_height_m;
        b[32] = (self.card_amp as u8)
            | ((self.rdv_follow as u8) << 1)
            | (((!self.alert_tones) as u8) << 2);
        b[33] = self.rdv_band_mask & 0x0F;
        let mut c = Crc32::new();
        c.update(&b[..36]);
        b[36..40].copy_from_slice(&c.get().to_le_bytes());
        b
    }

    pub fn decode(b: &[u8; CFG_BYTES]) -> Option<Self> {
        if b[0..2] != CFG_MAGIC {
            return None;
        }
        let ver = b[2];
        if ver == CFG_VERSION_V1 {
            let mut c = Crc32::new();
            c.update(&b[..28]);
            let want = u32::from_le_bytes([b[28], b[29], b[30], b[31]]);
            if c.get() != want {
                return None;
            }
        } else if ver == CFG_VERSION {
            let mut c = Crc32::new();
            c.update(&b[..36]);
            let want = u32::from_le_bytes([b[36], b[37], b[38], b[39]]);
            if c.get() != want {
                return None;
            }
        } else {
            return None;
        }
        let mut base_call = [0u8; 6];
        let mut prefix = [0u8; 4];
        let mut manual_grid = [0u8; 4];
        base_call.copy_from_slice(&b[8..14]);
        prefix.copy_from_slice(&b[16..20]);
        manual_grid.copy_from_slice(&b[22..26]);
        let (card_rig, card_ant, card_height_m, card_amp, rdv_band_mask, rdv_follow, alert_tones) = if ver >= CFG_VERSION {
            (
                u16::from_le_bytes([b[28], b[29]]) & 0x3FF,
                b[30] & 0x1F,
                b[31].min(127),
                (b[32] & 1) != 0,
                b[33] & 0x0F,
                (b[32] & 2) != 0,
                (b[32] & 4) == 0,
            )
        } else {
            (1, 1, 8, false, 0b1111, false, true)
        };
        Some(Self {
            gen: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            base_call,
            call_len: b[14].min(6),
            prefix,
            prefix_len: b[15].min(4),
            suffix: b[20].min(15),
            use_gps_grid: (b[3] & 1) != 0,
            manual_grid,
            dial_idx: b[21],
            rx_corr_hz: i16::from_le_bytes([b[26], b[27]]),
            tx_power: (b[3] >> 1) & 3,
            coop_rx: (b[3] >> 3) & 3,
            coop_link: (b[3] >> 5) & 1,
            card_rig,
            card_ant,
            card_height_m,
            card_amp,
            rdv_band_mask,
            rdv_follow,
            alert_tones,
        })
    }
}

/// Load the newest valid RAD950 profile from the A/B sectors of `RAD950_CFG`.
pub fn load_cfg(flash: &mut impl Nor) -> Result<Option<Rad950CfgRecord>> {
    let mut a = [0u8; CFG_BYTES];
    let mut b = [0u8; CFG_BYTES];
    flash.read(RAD950_CFG.start, &mut a)?;
    flash.read(RAD950_CFG.start + SECTOR, &mut b)?;
    let ca = Rad950CfgRecord::decode(&a);
    let cb = Rad950CfgRecord::decode(&b);
    Ok(match (ca, cb) {
        (Some(x), Some(y)) => Some(if y.gen >= x.gen { y } else { x }),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    })
}

/// Save a RAD950 profile to the older of the two `RAD950_CFG` A/B sectors with `gen + 1`.
pub async fn save_cfg(flash: &mut impl Nor, rec: &mut Rad950CfgRecord) -> Result {
    let mut a = [0u8; CFG_BYTES];
    let mut b = [0u8; CFG_BYTES];
    flash.read(RAD950_CFG.start, &mut a)?;
    flash.read(RAD950_CFG.start + SECTOR, &mut b)?;
    let ca = Rad950CfgRecord::decode(&a);
    let cb = Rad950CfgRecord::decode(&b);
    let (target_sector, next_gen) = match (ca, cb) {
        (Some(x), Some(y)) => {
            if y.gen >= x.gen {
                (RAD950_CFG.start, y.gen.wrapping_add(1))
            } else {
                (RAD950_CFG.start + SECTOR, x.gen.wrapping_add(1))
            }
        }
        (Some(x), None) => (RAD950_CFG.start + SECTOR, x.gen.wrapping_add(1)),
        (None, Some(y)) => (RAD950_CFG.start, y.gen.wrapping_add(1)),
        (None, None) => (RAD950_CFG.start, 1),
    };
    rec.gen = next_gen;
    let raw = rec.encode();
    flash.start_erase(target_sector)?;
    wait_nor(flash).await?;
    flash.start_program(target_sector, &raw)?;
    wait_nor(flash).await?;
    Ok(())
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use crate::block;
    use crate::sim::SimFlash;

    fn make_contact(peer: &[u8], grid: &[u8; 4], utc_sec: u32, slot: u32, rx: i8, tx: i8) -> Rad950Contact {
        let mut c = Rad950Contact::EMPTY;
        c.utc_sec = utc_sec;
        c.dial_hz10 = 2_807_800;
        let n = peer.len().min(16);
        c.peer_id[..n].copy_from_slice(&peer[..n]);
        c.my_id[..4].copy_from_slice(b"NQ6N");
        c.peer_grid = *grid;
        c.my_grid = *b"FN31";
        c.rx_snr_db = rx;
        c.tx_snr_db = tx;
        c.my_grid_src = 1;
        c.slot = slot;
        c
    }

    #[test]
    fn append_dedup_remount_and_stats_midnight() {
        let mut flash = SimFlash::new();
        let mut j = Rad950Journal::mount(&mut flash).unwrap();
        assert_eq!(j.count, 0);

        // Day 20_000 (20_000 * 86_400 = 1_728_000_000)
        let day1 = 20_000u32;
        let t1 = day1 * 86_400 + 86_390; // 23:59:50 UTC
        let t2 = (day1 + 1) * 86_400 + 600; // 00:10:00 UTC next day (> 5 min cooldown)

        let c1 = make_contact(b"K1ABC", b"FN31", t1, t1 / 10, -14, -12);
        assert_eq!(block(j.append(&mut flash, &c1)).unwrap(), AppendOutcome::Saved(1));
        // Duplicate in same 5-minute window is rejected idempotently.
        let c1_dup = make_contact(b"K1ABC", b"FN31", t1 + 20, t1 / 10 + 2, -13, -11);
        assert_eq!(block(j.append(&mut flash, &c1_dup)).unwrap(), AppendOutcome::Duplicate);
        // Same peer next UTC day (different cooldown window) is saved.
        let c2 = make_contact(b"K1ABC", b"FN31", t2, t2 / 10, -10, -8);
        assert_eq!(block(j.append(&mut flash, &c2)).unwrap(), AppendOutcome::Saved(2));
        // Second peer in different grid on day2.
        let c3 = make_contact(b"KP2/W1AW-2", b"FK77", t2 + 60, t2 / 10 + 6, -16, -15);
        assert_eq!(block(j.append(&mut flash, &c3)).unwrap(), AppendOutcome::Saved(3));

        // Remount from flash (simulated reboot).
        let j2 = Rad950Journal::mount(&mut flash).unwrap();
        assert_eq!(j2.count, 3);
        assert_eq!(j2.next_idx, 3);
        assert!(j2.field_worked(grid4_to_field_square(b"FN31").unwrap().0));
        assert!(j2.field_worked(grid4_to_field_square(b"FK77").unwrap().0));

        // All saved stats
        let all = j2.stats(&mut flash, None).unwrap();
        assert_eq!(all.contacts, 3);
        assert_eq!(all.unique_stations, 2);
        assert_eq!(all.unique_grids, 2);
        assert_eq!((all.best_rx_snr, all.best_tx_snr), (-10, -8));

        // Today (UTC) on day1 vs day1+1 (UTC midnight rollover)
        let d1 = j2.stats(&mut flash, Some(day1)).unwrap();
        assert_eq!((d1.contacts, d1.unique_stations, d1.unique_grids), (1, 1, 1));
        let d2 = j2.stats(&mut flash, Some(day1 + 1)).unwrap();
        assert_eq!((d2.contacts, d2.unique_stations, d2.unique_grids), (2, 2, 2));

        // Field square drill-down for FN
        let fn_field = grid4_to_field_square(b"FN31").unwrap().0;
        let sqs = j2.field_squares(&mut flash, fn_field).unwrap();
        assert_eq!(sqs[31], 2);
        assert_eq!(sqs[0], 0);

        // Newest-first log read + grid filter
        let mut page = [None; 4];
        let n = j2.read_newest(&mut flash, 0, None, &mut page).unwrap();
        assert_eq!(n, 3);
        assert_eq!(page[0].unwrap().peer_str_bytes(), b"KP2/W1AW-2");
        assert_eq!(page[2].unwrap().peer_str_bytes(), b"K1ABC");

        let n_fk = j2.read_newest(&mut flash, 0, Some(*b"FK77"), &mut page).unwrap();
        assert_eq!(n_fk, 1);
        assert_eq!(page[0].unwrap().peer_str_bytes(), b"KP2/W1AW-2");
    }

    #[test]
    fn torn_record_discarded_on_mount() {
        let mut flash = SimFlash::new();
        let mut j = Rad950Journal::mount(&mut flash).unwrap();
        let c1 = make_contact(b"W1AW", b"FN31", 1_700_000_000, 100, -12, -10);
        assert_eq!(block(j.append(&mut flash, &c1)).unwrap(), AppendOutcome::Saved(1));
        // Simulate a power-cut torn write at slot 1 (partial bytes, invalid CRC).
        let mut torn = [0xFFu8; REC_BYTES];
        torn[0..6].copy_from_slice(b"SL\x01\x00\x12\x34");
        flash.start_program(addr_of(1), &torn).unwrap();

        let mut j2 = Rad950Journal::mount(&mut flash).unwrap();
        assert_eq!(j2.count, 1);
        assert_eq!(j2.torn_skipped, 1);
        assert_eq!(j2.next_idx, 2, "next append skips torn slot");
        let c2 = make_contact(b"VE3XYZ", b"EN93", 1_700_001_000, 200, -15, -14);
        assert_eq!(block(j2.append(&mut flash, &c2)).unwrap(), AppendOutcome::Saved(3));
        assert_eq!(j2.count, 2);
    }

    #[test]
    fn non_overwriting_log_full() {
        let mut flash = SimFlash::new();
        let mut j = Rad950Journal::mount(&mut flash).unwrap();
        for i in 0..CAPACITY {
            let c = make_contact(b"K1ABC", b"FN31", 1_700_000_000 + i as u32 * 600, (i as u32) * 60, -12, -12);
            assert_eq!(block(j.append(&mut flash, &c)).unwrap(), AppendOutcome::Saved((i + 1) as u32));
        }
        assert!(j.is_full());
        let extra = make_contact(b"W9ZZZ", b"EN52", 1_750_000_000, 999_999, -5, -5);
        assert_eq!(block(j.append(&mut flash, &extra)).unwrap(), AppendOutcome::LogFull);
        // Verify first record is untouched.
        let mut first_raw = [0u8; REC_BYTES];
        flash.read(addr_of(0), &mut first_raw).unwrap();
        let first = Rad950Contact::decode(&first_raw, 1).unwrap();
        assert_eq!(first.peer_str_bytes(), b"K1ABC");
    }

    #[test]
    fn ab_profile_save_and_reload() {
        let mut flash = SimFlash::new();
        assert!(load_cfg(&mut flash).unwrap().is_none());
        let mut cfg = Rad950CfgRecord {
            gen: 0,
            base_call: *b"NQ6N\0\0",
            call_len: 4,
            prefix: *b"KP2\0",
            prefix_len: 3,
            suffix: 1,
            use_gps_grid: false,
            manual_grid: *b"FK77",
            dial_idx: 2,
            rx_corr_hz: -125,
            tx_power: 1,
            coop_rx: 1,
            coop_link: 1,
            card_rig: 2,
            card_ant: 3,
            card_height_m: 12,
            card_amp: true,
            rdv_band_mask: 0b1001,
            rdv_follow: true,
            alert_tones: false,
        };
        block(save_cfg(&mut flash, &mut cfg)).unwrap();
        assert_eq!(cfg.gen, 1);
        assert_eq!(load_cfg(&mut flash).unwrap(), Some(cfg));

        cfg.rx_corr_hz = 350;
        block(save_cfg(&mut flash, &mut cfg)).unwrap();
        assert_eq!(cfg.gen, 2);
        assert_eq!(load_cfg(&mut flash).unwrap(), Some(cfg));
    }
}
