//! RAD950 Ad-Hoc Rendezvous (`docs/sol950-rendezvous-addendum.md`).
//!
//! Hourly 5-minute UTC meetup windows across 10 m (:00), 12 m (:15), 15 m (:30), and 17 m (:45),
//! with independent 50/50 call-or-listen draws on even 10-second slots (`:00`, `:20`, `:40`)
//! and clear-subchannel selection.

use crate::sol950::SOL950_DIALS;

/// One scheduled hourly rendezvous band entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RdvBand {
    pub label: &'static str,
    /// Starting UTC minute within the hour (`0, 15, 30, 45`).
    pub start_min: u8,
    /// Index into `SOL950_DIALS` for this band's shared calling dial.
    pub dial_idx: u8,
}

/// Published hourly UTC rendezvous schedule (5-minute windows = 30 turns each).
pub const RDV_BANDS: [RdvBand; 4] = [
    RdvBand { label: "10m", start_min: 0, dial_idx: 0 },  // 28.078 MHz (:00-:04)
    RdvBand { label: "12m", start_min: 15, dial_idx: 4 }, // 24.919 MHz (:15-:19)
    RdvBand { label: "15m", start_min: 30, dial_idx: 6 }, // 21.078 MHz (:30-:34)
    RdvBand { label: "17m", start_min: 45, dial_idx: 8 }, // 18.104 MHz (:45-:49)
];

/// Window duration in minutes (`5` minutes = `30` ten-second turns).
pub const RDV_WINDOW_MIN: u8 = 5;

/// Eight 225 Hz audio subchannels inside the RAD950 passband (`600..2175 Hz`, matching `usol950::SUBCHANNELS_HZ`).
pub const RDV_SUBCH_HZ: [u16; 8] = [600, 825, 1050, 1275, 1500, 1725, 1950, 2175];

/// Current or next scheduled rendezvous window relative to `utc_sec`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RdvWindow {
    /// Band index `0..4` (`10m, 12m, 15m, 17m`).
    pub band_idx: u8,
    /// `true` if `utc_sec` is currently inside this 5-minute window (`NOW`), `false` if upcoming (`NEXT`).
    pub is_now: bool,
    /// Seconds remaining in the current window (`is_now == true`) or seconds until it opens (`is_now == false`).
    pub countdown_s: u16,
    /// Shared USB calling dial (10 Hz units).
    pub dial_hz10: u32,
}

/// Pre-drawn plan for one upcoming 10-second UTC turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RdvTurnPlan {
    /// Transmit a rendezvous CQ on the chosen clear subchannel (`subch 0..=7`, `hz`).
    Cq { subch: u8, hz: u16 },
    /// Listen / monitor (or send a directed reply if one is queued).
    Listen,
}

/// Bounded zero-heap state for RAD950 Ad-Hoc Rendezvous.
#[derive(Clone, Copy, Debug)]
pub struct RdvState {
    /// Bitmask of enabled bands: bit 0 = 10m, bit 1 = 12m, bit 2 = 15m, bit 3 = 17m.
    pub band_mask: u8,
    /// Automatically tune & monitor selected bands when their window opens.
    pub follow_schedule: bool,
    /// Attended `Call & listen` arm for the active window (auto-clears when window closes).
    pub call_listen_armed: bool,
    /// Per-station PRNG seed (derived from callsign + salt so stations never collide in lockstep).
    pub seed: u32,
    /// Last active window band index (`0..4`) we were inside, to detect window boundaries.
    last_active_band: Option<u8>,
    /// Set to `true` when `follow_schedule` changes `profile.dial_idx` and needs a radio retune.
    pending_retune: bool,
}

impl Default for RdvState {
    fn default() -> Self {
        Self::new()
    }
}

impl RdvState {
    pub const fn new() -> Self {
        Self {
            band_mask: 0b1111, // All 4 bands enabled by default; operator can deselect bands without antennas.
            follow_schedule: false,
            call_listen_armed: false,
            seed: 0x5A3C_9E37,
            last_active_band: None,
            pending_retune: false,
        }
    }

    pub fn set_seed_from_call(&mut self, call_raw: u32) {
        let s = call_raw.wrapping_mul(0x9E37_79B1) ^ 0x5A3C_9E37;
        self.seed = if s == 0 { 0x1357_9BDF } else { s };
    }

    pub fn band_enabled(&self, idx: u8) -> bool {
        idx < 4 && (self.band_mask & (1u8 << idx)) != 0
    }

    pub fn toggle_band(&mut self, idx: u8) {
        if idx < 4 {
            self.band_mask ^= 1u8 << idx;
        }
    }

    pub fn take_retune(&mut self) -> bool {
        core::mem::take(&mut self.pending_retune)
    }

    /// Resolve whether a selected band window is open `NOW` or which selected band is `NEXT`.
    pub fn window_status(&self, utc_sec: u32) -> Option<RdvWindow> {
        if self.band_mask == 0 {
            return None;
        }
        let sec_in_hour = (utc_sec % 3600) as u16;
        // First check if we are currently inside any enabled 5-minute window.
        for (i, b) in RDV_BANDS.iter().enumerate() {
            if !self.band_enabled(i as u8) {
                continue;
            }
            let start = b.start_min as u16 * 60;
            let end = start + RDV_WINDOW_MIN as u16 * 60;
            if sec_in_hour >= start && sec_in_hour < end {
                return Some(RdvWindow {
                    band_idx: i as u8,
                    is_now: true,
                    countdown_s: end - sec_in_hour,
                    dial_hz10: SOL950_DIALS[b.dial_idx as usize],
                });
            }
        }
        // Otherwise find the nearest upcoming enabled window (wrapping across the hour).
        let mut best: Option<RdvWindow> = None;
        for (i, b) in RDV_BANDS.iter().enumerate() {
            if !self.band_enabled(i as u8) {
                continue;
            }
            let start = b.start_min as u16 * 60;
            let wait = if start > sec_in_hour {
                start - sec_in_hour
            } else {
                3600 - sec_in_hour + start
            };
            if best.is_none_or(|cur| wait < cur.countdown_s) {
                best = Some(RdvWindow {
                    band_idx: i as u8,
                    is_now: false,
                    countdown_s: wait,
                    dial_hz10: SOL950_DIALS[b.dial_idx as usize],
                });
            }
        }
        best
    }

    /// Independent deterministic 50/50 coin flip + clear-subchannel selection for `slot` (`utc_sec / 10`).
    ///
    /// Even slots (`:00`, `:20`, `:40`) draw `p = 0.5` when `call_listen_armed` is true and the slot falls
    /// inside an enabled 5-minute window; odd slots (`:10`, `:30`, `:50`) are always `Listen` (reserved for replies).
    pub fn plan_slot(&self, slot: u32, clear_subch_mask: u8) -> RdvTurnPlan {
        if !self.call_listen_armed || (slot & 1) != 0 {
            return RdvTurnPlan::Listen;
        }
        let slot_utc = slot.saturating_mul(10);
        let Some(win) = self.window_status(slot_utc) else {
            return RdvTurnPlan::Listen;
        };
        if !win.is_now {
            return RdvTurnPlan::Listen;
        }
        let r = mix32(self.seed ^ slot.wrapping_mul(0x9E37_79B1));
        if (r & 1) == 0 {
            return RdvTurnPlan::Listen;
        }
        // Prefer an observed clear subchannel; fall back to all 8 if all appear busy.
        let mask = if clear_subch_mask != 0 { clear_subch_mask } else { 0xFF };
        let clear_cnt = mask.count_ones();
        let pick = (mix32(r ^ 0x85EB_CA6B) % clear_cnt) as u8;
        let mut seen = 0u8;
        let mut chosen = 3u8;
        for ch in 0..8u8 {
            if (mask & (1u8 << ch)) != 0 {
                if seen == pick {
                    chosen = ch;
                    break;
                }
                seen += 1;
            }
        }
        RdvTurnPlan::Cq { subch: chosen, hz: RDV_SUBCH_HZ[chosen as usize] }
    }

    /// Advance rendezvous state at a 10-second UTC slot boundary.
    ///
    /// Returns `(retune_dial_idx, cq_subch_hz)`:
    /// - `retune_dial_idx`: `Some(dial_idx)` if `follow_schedule` should tune to a newly opened window
    ///   (deferred while `has_active_qso` is true!).
    /// - `cq_subch_hz`: `Some(hz)` if `call_listen_armed` drew a 50/50 CQ on this even slot
    ///   (never fires while `has_active_qso` is true!).
    pub fn on_slot_start(
        &mut self,
        utc_sec: u32,
        slot: u32,
        has_active_qso: bool,
        cur_dial_idx: u8,
        clear_subch_mask: u8,
    ) -> (Option<u8>, Option<u16>) {
        let win = self.window_status(utc_sec);
        let active_band = win.filter(|w| w.is_now).map(|w| w.band_idx);

        // Auto-disarm Call & listen when leaving an active rendezvous window.
        if active_band != self.last_active_band {
            if self.last_active_band.is_some() {
                self.call_listen_armed = false;
            }
            self.last_active_band = active_band;
        }

        // In-flight directed contact takes precedence over automatic retuning and fresh CQs.
        if has_active_qso {
            return (None, None);
        }

        let mut new_dial = None;
        if self.follow_schedule {
            if let Some(b_idx) = active_band {
                let target_dial_idx = RDV_BANDS[b_idx as usize].dial_idx;
                if cur_dial_idx != target_dial_idx {
                    new_dial = Some(target_dial_idx);
                    self.pending_retune = true;
                }
            }
        }

        let cq_hz = match self.plan_slot(slot, clear_subch_mask) {
            RdvTurnPlan::Cq { hz, .. } => Some(hz),
            RdvTurnPlan::Listen => None,
        };
        (new_dial, cq_hz)
    }
}

/// Fast avalanche hash for independent per-slot 50/50 draws.
const fn mix32(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7FEB_352D);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846C_A68B);
    x ^ (x >> 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hourly_windows_and_partial_band_mask() {
        let mut rdv = RdvState::new();
        // 10m + 17m operator only (bits 0 and 3)
        rdv.band_mask = 0b1001;

        // :02:00 UTC -> inside 10m window, 180 s left
        let w0 = rdv.window_status(120).unwrap();
        assert_eq!(w0.band_idx, 0);
        assert!(w0.is_now);
        assert_eq!(w0.countdown_s, 180);

        // :16:00 UTC -> 12m is unselected, so NEXT should be 17m at :45:00 (29 min = 1740 s)
        let w1 = rdv.window_status(16 * 60).unwrap();
        assert_eq!(w1.band_idx, 3);
        assert!(!w1.is_now);
        assert_eq!(w1.countdown_s, 29 * 60);

        // :47:30 UTC -> inside 17m window, 150 s left
        let w2 = rdv.window_status(47 * 60 + 30).unwrap();
        assert_eq!(w2.band_idx, 3);
        assert!(w2.is_now);
        assert_eq!(w2.countdown_s, 150);
    }

    #[test]
    fn call_listen_50_50_even_only_and_window_boundary_disarm() {
        let mut a = RdvState::new();
        let mut b = RdvState::new();
        a.set_seed_from_call(0x1111_2222);
        b.set_seed_from_call(0x3333_4444);
        a.follow_schedule = true;
        a.call_listen_armed = true;
        b.call_listen_armed = true;

        let mut a_cqs = 0u32;
        let mut diffs = 0u32;
        // 15 even slots inside :00..:04 (slots 0, 2, 4, ..., 28)
        for s in 0..30u32 {
            let pa = a.plan_slot(s, 0b1111_0000); // only upper 4 subchannels clear
            let pb = b.plan_slot(s, 0xFF);
            if s & 1 == 1 {
                assert_eq!(pa, RdvTurnPlan::Listen, "odd slots never call CQ");
            } else {
                if let RdvTurnPlan::Cq { subch, .. } = pa {
                    a_cqs += 1;
                    assert!(subch >= 4, "must pick from clear subchannels");
                }
                if matches!(pa, RdvTurnPlan::Cq { .. }) != matches!(pb, RdvTurnPlan::Cq { .. }) {
                    diffs += 1;
                }
            }
        }
        assert!(a_cqs > 0 && a_cqs < 15, "50/50 mix expected, got {a_cqs}/15");
        assert!(diffs > 0, "two radios must make independent draws");

        // Enter window at :00:00, then cross :05:00 boundary -> Call & listen auto-disarms!
        let _ = a.on_slot_start(0, 0, false, 4, 0xFF);
        assert!(a.call_listen_armed);
        let (dial, cq) = a.on_slot_start(5 * 60, 30, false, 0, 0xFF);
        assert_eq!(dial, None);
        assert_eq!(cq, None);
        assert!(!a.call_listen_armed);

        // In-flight QSO defers auto-retuning when a new window opens.
        a.follow_schedule = true;
        let (dial_busy, _) = a.on_slot_start(15 * 60, 90, true, 0, 0xFF);
        assert_eq!(dial_busy, None);
        let (dial_free, _) = a.on_slot_start(15 * 60 + 20, 92, false, 0, 0xFF);
        assert_eq!(dial_free, Some(4)); // 12m (24.919 MHz)
    }
}
