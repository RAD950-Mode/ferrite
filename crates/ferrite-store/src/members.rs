//! Zones without a zone map (decision 333, CHAN-35, CHAN-36): the store keeps no zone per channel in RAM. It keeps
//! the member list of each slot's zone ([`Members`]: up to [`ZONE_CAP`] channel numbers in number order, one list
//! shared by slots on the same zone), built by one pass over the channel records when a slot takes a zone no list
//! has, and kept right by every channel write after that. Everything else pages through flash:
//!
//! - A zone's channel count is in its zone record (bytes 24-25). A write that moves a channel in or out of a zone
//!   changes the count held in RAM for a few zones ([`ZCACHE`]) and marks the zone; the marked zones' records are
//!   rewritten with their counts once the writes have stopped for [`crate::store::SAVE_DELAY_MS`] (and at power
//!   off), so a link writing 4,000 channels rewrites each zone's sector a few times, not once a channel.
//! - The mount counts every zone in one pass (a table on the stack, gone after) and marks those whose stored count
//!   is wrong, so a count a power cut left behind is put right.
//! - The i-th channel of a zone with no list, of no zone, or of all channels ([`FlashStore::nth`]) is found by
//!   walking the records from a cursor kept per kind (the last one found), so a list drawn row by row, or stepped
//!   one at a time, reads only the records between. Each record is first looked at by its zone's low byte and its
//!   squelch byte (two bytes: erased reads 0xFF), and read whole only when it may be one.

use crate::records::*;
use crate::store::FlashStore;
use crate::{Nor, Result};
use ferrite_app::chan::{ZoneId, MAX_CHANNELS, MAX_ZONES, NO_ZONE, ZONE_CAP};
use ferrite_app::data::Data;

/// A key for "every channel" (the All view): not a zone, not "no zone", not Local.
pub const ALL: ZoneId = 0xFFFD;
/// A list no zone has, and a record with no channel.
pub const FREE: ZoneId = 0xFFFC;
/// Zone counts held in RAM while their records wait to be rewritten.
pub const ZCACHE: usize = 8;
/// Words of the marked-zones bitmap.
pub const DIRTY_WORDS: usize = (MAX_ZONES as usize).div_ceil(32);

/// One slot zone's channel numbers, in number order. `over`: the zone had more than [`ZONE_CAP`] (only "no zone"
/// can), so the list isn't used and that zone pages through flash like any other.
#[derive(Clone, Copy)]
pub struct Members {
    pub zone: ZoneId,
    pub n: u16,
    pub over: bool,
    pub nums: [u16; ZONE_CAP as usize],
}

impl Members {
    pub const EMPTY: Members = Members { zone: FREE, n: 0, over: false, nums: [0; ZONE_CAP as usize] };

    /// An empty list for `zone`, built as all zeros and then given its zone: copied from [`Members::EMPTY`] it was a
    /// 518-byte constant in slow flash (flash audit, Nov 1, 2026).
    pub const fn empty(zone: ZoneId) -> Members {
        let mut m = Members { zone: 0, n: 0, over: false, nums: [0; ZONE_CAP as usize] };
        m.zone = zone;
        m
    }

    fn find(&self, num: u16) -> core::result::Result<usize, usize> {
        self.nums[..self.n as usize].binary_search(&num)
    }

    /// Whether it answers for `zone` now.
    pub fn holds(&self, zone: ZoneId) -> bool {
        self.zone == zone && !self.over
    }

    fn insert(&mut self, num: u16) {
        if let Err(i) = self.find(num) {
            if self.n == ZONE_CAP {
                self.over = true;
                return;
            }
            self.nums.copy_within(i..self.n as usize, i + 1);
            self.nums[i] = num;
            self.n += 1;
        }
    }

    fn remove(&mut self, num: u16) {
        if let Ok(i) = self.find(num) {
            self.nums.copy_within(i + 1..self.n as usize, i);
            self.n -= 1;
        }
    }

    /// Where `num` is in the list (its position in the zone, CHAN-10).
    pub fn pos(&self, num: u16) -> Option<u16> {
        self.find(num).ok().map(|i| i as u16)
    }

    pub fn get(&self, i: u16) -> Option<u16> {
        (i < self.n).then(|| self.nums[i as usize])
    }
}

/// A cursor: under `key`, member number `i` is channel `num`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct At {
    pub key: ZoneId,
    pub i: u16,
    pub num: u16,
}

impl At {
    pub const NONE: At = At { key: FREE, i: 0, num: 0 };
}

/// What a zone key means for a channel record's zone: a zone past the last there can be counts as none.
pub fn zkey(z: ZoneId) -> ZoneId {
    if z < MAX_ZONES { z } else { NO_ZONE }
}

impl<N: Nor, D: Data> FlashStore<N, D> {
    /// Bytes 18 and 19 of channel `num`'s record: its zone's low byte and its squelch (both 0xFF when erased; a
    /// channel's squelch is 0-10).
    fn peek(&self, num: u16) -> [u8; 2] {
        let mut b = [0xFFu8; 2];
        if self.ok() {
            let _ = self.f.read(ch_addr(num) + CH_ZONE_LO as u32, &mut b);
        }
        b
    }

    /// Nothing is stored at `num` (its squelch byte erased): no need to read the whole record.
    pub(crate) fn peek_empty(&self, num: u16) -> bool {
        self.peek(num)[1] == 0xFF
    }

    /// Channel `num`'s record, if it holds a channel under `key` (a zone, no zone, or [`ALL`]).
    pub(crate) fn member(&self, key: ZoneId, num: u16) -> Option<[u8; CH_LEN]> {
        let [lo, sq] = self.peek(num);
        if sq == 0xFF || key != ALL && lo != key as u8 {
            return None;
        }
        let b = self.read_ch(num);
        (ch_valid(&b) && (key == ALL || zkey(ch_zone(&b)) == key)).then_some(b)
    }

    /// The zone key of the channel at `num` ([`FREE`]: none there).
    pub(crate) fn key_of(&self, num: u16) -> ZoneId {
        if self.peek(num)[1] == 0xFF {
            return FREE;
        }
        let b = self.read_ch(num);
        if ch_valid(&b) { zkey(ch_zone(&b)) } else { FREE }
    }

    /// Channels under `key` by one pass over the records (a zone's when its count isn't known).
    pub(crate) fn tally(&self, key: ZoneId) -> u16 {
        (0..MAX_CHANNELS).filter(|&n| self.member(key, n).is_some()).count() as u16
    }

    /// The list holding `zone`, if one does.
    fn list_of(&self, zone: ZoneId) -> Option<usize> {
        self.lists.borrow().iter().position(|l| l.holds(zone))
    }

    /// The i-th channel under `key` (number order): from the slot list holding it, or found in flash from the
    /// nearest of its cursor, the first number and the last.
    pub(crate) fn nth(&self, key: ZoneId, i: u16) -> Option<(u16, [u8; CH_LEN])> {
        if let Some(k) = self.list_of(key) {
            let num = self.lists.borrow()[k].get(i)?;
            let b = self.read_ch(num);
            return ch_valid(&b).then_some((num, b));
        }
        let len = self.zone_len_of(key);
        if i >= len {
            return None;
        }
        let cur = if key == ALL { &self.at_all } else { &self.at_zone };
        let c = cur.get();
        // (index, number) to walk from, the nearest to `i`: index -1 before number 0, `len` after the last number,
        // or the cursor
        let dist = |p: (i32, i32)| (p.0 - i as i32).unsigned_abs();
        let mut from = (-1i32, -1i32);
        if dist((len as i32, 0)) < dist(from) {
            from = (len as i32, MAX_CHANNELS as i32);
        }
        if c.key == key && c.num < MAX_CHANNELS && c.i < len && dist((c.i as i32, 0)) < dist(from) {
            from = (c.i as i32, c.num as i32);
        }
        if from.0 == i as i32 {
            if let Some(b) = self.member(key, from.1 as u16) {
                return Some((from.1 as u16, b));
            }
            from = (-1, -1); // the cursor is stale: from the start
        }
        let (mut j, mut n) = from;
        let up = j < i as i32;
        loop {
            n += if up { 1 } else { -1 };
            if !(0..MAX_CHANNELS as i32).contains(&n) {
                cur.set(At::NONE);
                return None;
            }
            if let Some(b) = self.member(key, n as u16) {
                j += if up { 1 } else { -1 };
                if j == i as i32 {
                    cur.set(At { key, i, num: n as u16 });
                    return Some((n as u16, b));
                }
            }
        }
    }

    /// Where channel `num` (under `key`) is among `key`'s channels: from a list, else by counting those below it.
    pub(crate) fn rank(&self, key: ZoneId, num: u16) -> u16 {
        if let Some(k) = self.list_of(key) {
            if let Some(p) = self.lists.borrow()[k].pos(num) {
                return p;
            }
        }
        (0..num).filter(|&n| self.member(key, n).is_some()).count() as u16
    }

    /// How many channels are under `key`: [`ALL`], no zone, or a zone (its list, its count in RAM, its record, or
    /// a pass when none of those knows).
    pub(crate) fn zone_len_of(&self, key: ZoneId) -> u16 {
        match key {
            ALL => return self.count.get(),
            NO_ZONE => return self.loose.get(),
            k if k >= MAX_ZONES => return 0,
            _ => {}
        }
        if let Some(k) = self.list_of(key) {
            return self.lists.borrow()[k].n;
        }
        if let Some(&(_, n)) = self.zcache.get().iter().find(|e| e.0 == key) {
            return n;
        }
        if !self.dirty(key) {
            if let Some(n) = self.stored_count(key) {
                return n;
            }
        }
        let n = self.tally(key);
        self.cache_count(key, n);
        n
    }

    /// The count zone `z`'s record holds (None: no record, or written before counts).
    pub(crate) fn stored_count(&self, z: ZoneId) -> Option<u16> {
        if z >= self.zones.get() || !self.ok() {
            return None;
        }
        let mut b = [0xFFu8; ZONE_LEN];
        self.f.read(zone_addr(z), &mut b).ok()?;
        zone_count(&b)
    }

    pub(crate) fn cache_count(&self, z: ZoneId, n: u16) {
        let mut c = self.zcache.get();
        match c.iter_mut().find(|e| e.0 == z) {
            Some(e) => e.1 = n,
            None => {
                let k = self.zcache_next.get() as usize % ZCACHE;
                c[k] = (z, n);
                self.zcache_next.set(((k + 1) % ZCACHE) as u8);
            }
        }
        self.zcache.set(c);
    }

    pub(crate) fn dirty(&self, z: ZoneId) -> bool {
        (z as usize) < MAX_ZONES as usize && self.dirty.get()[z as usize / 32] & 1 << (z % 32) != 0
    }

    pub(crate) fn set_dirty(&self, z: ZoneId, on: bool) {
        if z >= MAX_ZONES {
            return;
        }
        let mut d = self.dirty.get();
        if on {
            d[z as usize / 32] |= 1 << (z % 32);
        } else {
            d[z as usize / 32] &= !(1 << (z % 32));
        }
        self.dirty.set(d);
    }

    /// The first zone whose record waits for its count.
    pub(crate) fn first_dirty(&self) -> Option<ZoneId> {
        let d = self.dirty.get();
        d.iter().enumerate().find(|(_, w)| **w != 0).map(|(i, w)| (i * 32) as ZoneId + w.trailing_zeros() as ZoneId)
    }

    /// One zone's count moved by `d` (a channel in or out): in RAM now, in its record later.
    fn count_moves(&self, z: ZoneId, d: i32) {
        if z >= MAX_ZONES {
            return;
        }
        let known = self.zcache.get().iter().find(|e| e.0 == z).map(|e| e.1).or_else(|| if self.dirty(z) { None } else { self.stored_count(z) });
        match known {
            Some(n) => self.cache_count(z, (n as i32 + d).clamp(0, u16::MAX as i32) as u16),
            // not known (marked, and its RAM count let go): counted again when asked
            None => {
                let mut c = self.zcache.get();
                c.iter_mut().filter(|e| e.0 == z).for_each(|e| e.0 = FREE);
                self.zcache.set(c);
            }
        }
        self.set_dirty(z, true);
        self.counts_at.set(Some(self.now_ms.get()));
    }

    /// A channel record changed from zone key `old` to `new` ([`FREE`]: no channel): the totals, the zones' counts,
    /// the slot lists and the cursors follow.
    pub(crate) fn note(&self, num: u16, old: ZoneId, new: ZoneId) {
        self.at_all.set(At::NONE);
        self.at_zone.set(At::NONE);
        self.cver.set(self.cver.get().wrapping_add(1));
        if old == new {
            return;
        }
        if old == FREE {
            self.count.set(self.count.get() + 1);
        } else if new == FREE {
            self.count.set(self.count.get().saturating_sub(1));
        }
        if old == NO_ZONE {
            self.loose.set(self.loose.get().saturating_sub(1));
        }
        if new == NO_ZONE {
            self.loose.set(self.loose.get() + 1);
        }
        let mut lists = self.lists.borrow_mut();
        for l in lists.iter_mut() {
            if l.zone == old {
                l.remove(num);
            }
            if l.zone == new {
                l.insert(num);
            }
        }
        drop(lists);
        if old < MAX_ZONES {
            self.count_moves(old, -1);
        }
        if new < MAX_ZONES {
            self.count_moves(new, 1);
        }
    }

    /// Lists for the zones the slots are on (see the module comment): a list no slot's zone has is let go; a zone
    /// no list holds gets a free one, built by one pass ([`FlashStore::build`]).
    pub fn set_slot_zones(&self, zones: [ZoneId; 3]) {
        if zones == self.slot_zones.get() && self.lists.borrow().iter().all(|l| l.zone == FREE || zones.contains(&l.zone)) {
            return;
        }
        self.slot_zones.set(zones);
        {
            let mut lists = self.lists.borrow_mut();
            for l in lists.iter_mut().filter(|l| !zones.contains(&l.zone)) {
                l.zone = FREE;
            }
        }
        for z in zones {
            let have = self.lists.borrow().iter().any(|l| l.zone == z);
            if !have && (z < MAX_ZONES || z == NO_ZONE) {
                let free = self.lists.borrow().iter().position(|l| l.zone == FREE);
                if let Some(k) = free {
                    self.build(k, z);
                }
            }
        }
    }

    /// List `k` built for zone `z`: one pass over the channel records.
    #[inline(never)]
    pub(crate) fn build(&self, k: usize, z: ZoneId) {
        let mut l = Members::empty(z);
        if self.ok() {
            for num in 0..MAX_CHANNELS {
                if self.member(z, num).is_some() {
                    l.insert(num);
                }
            }
        }
        self.lists.borrow_mut()[k] = l;
        if z < MAX_ZONES && !l.over {
            self.cache_count(z, l.n);
        }
    }

    /// The mount's count (see the module comment): the totals, and the zones whose stored count is wrong marked.
    /// The table of counts is on the stack only for this pass.
    #[inline(never)]
    pub(crate) fn mount_counts(&self) -> Result {
        let mut per = [0u16; MAX_ZONES as usize];
        let (mut all, mut loose) = (0u16, 0u16);
        let mut b = [0u8; CH_LEN];
        // LINK-71: each channel sector's hash from the same reads (as `rehash` makes it: `ch_held`)
        let mut h = [HASH0; CH_SECTORS];
        for num in 0..MAX_CHANNELS {
            let mut p = [0xFFu8; 2];
            self.f.raw_read(ch_addr(num) + CH_ZONE_LO as u32, &mut p)?;
            if p[1] == 0xFF {
                continue;
            }
            self.f.raw_read(ch_addr(num), &mut b)?;
            if !ch_valid(&b) {
                continue;
            }
            let k = num as usize / CH_PER_SECTOR as usize;
            h[k] = hash_mix(h[k], num, &b);
            all += 1;
            match zkey(ch_zone(&b)) {
                NO_ZONE => loose += 1,
                z => per[z as usize] += 1,
            }
        }
        self.count.set(all);
        self.loose.set(loose);
        self.hashes.borrow_mut()[..CH_SECTORS].copy_from_slice(&h);
        self.f.fresh((1u128 << CH_SECTORS) - 1);
        self.dirty.set([0; DIRTY_WORDS]);
        let mut r = [0u8; ZONE_LEN];
        for z in 0..self.zones.get() {
            self.f.raw_read(zone_addr(z), &mut r)?;
            // A deleted zone's empty record holds no count, and the rewrite skips it: marking it would rewrite
            // the sector at every mount.
            if zone_decode(&r).is_some() && zone_count(&r) != Some(per[z as usize]) {
                self.set_dirty(z, true);
                self.cache_count(z, per[z as usize]);
            }
        }
        Ok(())
    }

    /// CHAN-35: room in `zone` for channel `num` (it is there already, or the zone has fewer than 256).
    pub fn room(&self, zone: ZoneId, num: u16) -> bool {
        zone == NO_ZONE || self.key_of(num) == zone || self.zone_len_of(zone) < ZONE_CAP
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A list keeps its numbers in order, once each; a 257th makes it over (no zone only: CHAN-35).
    #[test]
    fn a_list_stays_in_order() {
        let mut l = Members { zone: 3, ..Members::EMPTY };
        for n in [9, 2, 5, 2, 7] {
            l.insert(n);
        }
        assert_eq!(&l.nums[..l.n as usize], &[2, 5, 7, 9]);
        assert_eq!((l.pos(7), l.pos(8), l.get(0), l.get(4)), (Some(2), None, Some(2), None));
        l.remove(5);
        l.remove(6);
        assert_eq!(&l.nums[..l.n as usize], &[2, 7, 9]);
        let mut full = Members { zone: NO_ZONE, ..Members::EMPTY };
        for n in 0..ZONE_CAP {
            full.insert(n * 2);
        }
        assert!(!full.over && full.holds(NO_ZONE));
        full.insert(1);
        assert!(full.over && !full.holds(NO_ZONE));
    }
}
