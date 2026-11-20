//! SOL950 10m weak-signal session, 3-slot QSO state machine, `SAFE-22` relay-guard TX limiter, and 2-slot
//! rendezvous scanner with Post-Completion Linger (`docs/sol950-design-v2.md` §4).

use crate::decoders::BUDGET;
use crate::freq::Freq10;
pub use usol950;
use usol950::{Callsign, Decoded, Frame, Receiver, SoftCandidate, MAX_DECODED, GRID_UNKNOWN, SUBCHANNELS_HZ};

// Compile-time check that the SOL950 streaming receiver fits inside Ferrite's shared 13 KB `Decoder` slot.
const _: () = assert!(core::mem::size_of::<Receiver>() + 16 <= BUDGET);

pub const SLOT_S: u32 = 10;
pub const EXPIRY_SLOTS: u32 = 30; // 300 s (5 minutes)
pub const COOLDOWN_SLOTS: u32 = 30; // 300 s (5 minutes)
pub const SCAN_DWELL_SLOTS: u8 = 2; // 20 s minimum dwell: sees both even and odd TX parities
pub const LINGER_SLOTS: u32 = 2; // 20 s post-completion linger after sending ACK=1
pub const MAX_TRIES: u8 = 4;
pub const MAX_COOL_RETRIES: u8 = 2;
/// SAFE-22 (`path::GUARD_SWITCHES = 10` per 60 s): each 10m TX from `Rx::Si` costs 2 relay moves, so at most 3 TX
/// slots (`<= 6` relay moves) are permitted in any rolling 6-slot (`60 s`) window.
pub const MAX_TX_PER_MIN: usize = 3;
pub const ROLLING_MIN_SLOTS: u32 = 6;

pub const MAX_WINDOWS: usize = 8;
pub const MAX_PEERS: usize = 16;
pub const MAX_LOG_RECENT: usize = 16;

/// One completed receive slot retained for an external Cooperative RX host. Every member identifies the original
/// UTC slot and dial so a host can safely associate reports from separate radios before combining their LLRs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoopRxReport {
    pub utc_slot: u32,
    pub dial_hz: u32,
    pub decoded: [Decoded; MAX_DECODED],
    pub decoded_len: u8,
    pub soft: Option<SoftCandidate>,
}

impl CoopRxReport {
    pub const EMPTY: Self = Self {
        utc_slot: 0,
        dial_hz: 0,
        decoded: [Decoded::EMPTY; MAX_DECODED],
        decoded_len: 0,
        soft: None,
    };
}

/// Operating mode selected by the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpMode {
    /// RX-only 2-slot dwell scanning across configured windows.
    Monitor,
    /// Transmit `CQ` on slots matching `parity & 1` (`0` = even, `1` = odd) on the active window (or scan list)
    /// and automatically reply to callers.
    AutoCq { parity: u8 },
    /// Silent 2-slot dwell scanning; transmit directed `RX` only when a `CQ` (or addressed `RX`) is decoded.
    Pounce,
}

/// Station configuration for SOL950.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub my_call: Callsign,
    /// Optional 4-bit numeric suffix (`0` = none, `1..=15` = `-1`..`-15`, carried in `Frame::Cq.reserved & 0x0F`).
    pub my_suffix: u8,
    pub my_grid4: u16,
    pub tx_pwr: u8,
    pub mode: OpMode,
    pub windows: [Freq10; MAX_WINDOWS],
    pub n_windows: u8,
}

/// Completed two-way QSO record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QsoRecord {
    pub peer: Callsign,
    pub peer_grid4: u16,
    pub window: Freq10,
    pub sent_snr: i8,
    pub rcvd_snr: i8,
    pub completed_slot: u32,
}

/// Summary row for the Operate screen's activity list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActivityRow {
    pub peer: Callsign,
    pub suffix: u8,
    pub grid4: u16,
    pub snr_db: i8,
    pub last_seen_slot: u32,
    pub window: Freq10,
    pub owed_reply: Option<bool>,
    pub logged: bool,
}

/// Action planned for an upcoming 10-second UTC slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotPlan {
    /// Listen on `window` for the entire slot.
    Rx { window: Freq10 },
    /// Transmit `frame` on `window` at audio subchannel offset `subchannel_hz` above USB dial.
    Tx {
        window: Freq10,
        subchannel_hz: u16,
        frame: Frame,
    },
}

#[derive(Clone, Copy, Debug)]
struct PeerSession {
    active: bool,
    peer: Callsign,
    suffix: u8,
    window: Freq10,
    grid4: u16,
    last_heard_snr: i8,
    rcvd_snr: Option<i8>,
    last_seen_slot: u32,
    /// Timestamp (slot) when a valid `RX(..., ack = false)` addressed to `my_call` was decoded from `peer`.
    rx_r0_at: Option<u32>,
    /// Timestamp (slot) when a valid `R+RX(..., ack = true)` addressed to `my_call` was decoded from `peer`
    /// *after* our own `tx_r0_done_at`.
    rx_r1_at: Option<u32>,
    /// Timestamp (slot) when our initial `RX(..., ack = false)` addressed to `peer` finished TX and unkeyed.
    tx_r0_done_at: Option<u32>,
    /// Timestamp (slot) when our confirmed `R+RX(..., ack = true)` addressed to `peer` finished TX and unkeyed.
    tx_r1_done_at: Option<u32>,
    /// Slot of our most recent completed TX to `peer` (for retry pacing).
    last_tx_slot: Option<u32>,
    /// Slot when this QSO was logged (write-once per cooldown window).
    logged_at: Option<u32>,
    tries: u8,
    cool_retries: u8,
    /// Immediate reply owed: `Some(false)` -> send `RX(ACK=0)`, `Some(true)` -> send `R+RX(ACK=1)`.
    owed_reply: Option<bool>,
}

impl PeerSession {
    const fn empty() -> Self {
        Self {
            active: false,
            peer: Callsign::from_raw(0).unwrap(),
            suffix: 0,
            window: 0,
            grid4: GRID_UNKNOWN,
            last_heard_snr: 0,
            rcvd_snr: None,
            last_seen_slot: 0,
            rx_r0_at: None,
            rx_r1_at: None,
            tx_r0_done_at: None,
            tx_r1_done_at: None,
            last_tx_slot: None,
            logged_at: None,
            tries: 0,
            cool_retries: 0,
            owed_reply: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PendingTx {
    slot: u32,
    window: Freq10,
    frame: Frame,
}

/// SOL950 protocol and slot-aligned scanner engine.
pub struct Engine {
    cfg: Config,
    win_idx: u8,
    dwell_used: u8,
    /// Window lock while working an active session (`Some(until_slot)`).
    lock_until: Option<u32>,
    /// Mandatory Post-Completion Linger after sending `ACK=1` (`Some(until_slot)`).
    linger_until: Option<u32>,
    /// Recent completed TX slots (for `SAFE-22` `<= 3 TX / 60 s` check and consecutive-slot guard).
    tx_slots: [Option<u32>; MAX_TX_PER_MIN],
    pending_tx: Option<PendingTx>,
    peers: [PeerSession; MAX_PEERS],
    logs: [Option<QsoRecord>; MAX_LOG_RECENT],
    log_count: u32,
    occupied_subch_mask: u8,
}

impl Engine {
    pub const fn new(cfg: Config) -> Self {
        Self {
            cfg,
            win_idx: 0,
            dwell_used: 0,
            lock_until: None,
            linger_until: None,
            tx_slots: [None; MAX_TX_PER_MIN],
            pending_tx: None,
            peers: [PeerSession::empty(); MAX_PEERS],
            logs: [None; MAX_LOG_RECENT],
            log_count: 0,
            occupied_subch_mask: 0,
        }
    }

    pub fn current_window(&self) -> Freq10 {
        let n = (self.cfg.n_windows as usize).clamp(1, MAX_WINDOWS);
        self.cfg.windows[(self.win_idx as usize) % n]
    }

    pub fn total_logged(&self) -> u32 {
        self.log_count
    }

    pub fn recent_logs(&self) -> &[Option<QsoRecord>; MAX_LOG_RECENT] {
        &self.logs
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn mode(&self) -> OpMode {
        self.cfg.mode
    }

    pub fn set_config(&mut self, cfg: Config) {
        self.cfg = cfg;
    }

    /// Start an attended CQ session on the active window.
    pub fn start_cq(&mut self, parity: u8) {
        self.cfg.mode = OpMode::AutoCq { parity: parity & 1 };
    }

    /// Explicitly answer a heard peer from the Operate activity list.
    pub fn answer_peer(&mut self, peer: Callsign, slot: u32) -> bool {
        let win = self.current_window();
        let Some(idx) = self.find_peer(peer, win) else { return false };
        let p = &mut self.peers[idx];
        if p.logged_at.is_some() {
            return false;
        }
        self.cfg.mode = OpMode::Pounce;
        if p.owed_reply != Some(true) {
            p.owed_reply = Some(false);
        }
        self.lock_until = Some(slot + EXPIRY_SLOTS);
        true
    }

    /// Stop any attended CQ/Answer session: returns to Monitor, cancels queued replies, preserves completed logs.
    pub fn stop_session(&mut self) {
        self.cfg.mode = OpMode::Monitor;
        self.pending_tx = None;
        self.lock_until = None;
        self.linger_until = None;
        for p in &mut self.peers {
            p.owed_reply = None;
        }
    }

    /// Record that a planned TX slot missed its deadline and was skipped, leaving any pending reply observable.
    pub fn on_tx_missed_deadline(&mut self, slot: u32) {
        if self.pending_tx.is_some_and(|p| p.slot == slot) {
            self.pending_tx = None;
        }
    }

    /// Collect heard peers on any window, sorted newest-seen first, for the Operate screen.
    pub fn activity_rows(&self, out: &mut [ActivityRow; MAX_PEERS]) -> usize {
        let mut n = 0usize;
        for p in &self.peers {
            if p.active {
                out[n] = ActivityRow {
                    peer: p.peer,
                    suffix: p.suffix,
                    grid4: p.grid4,
                    snr_db: p.last_heard_snr,
                    last_seen_slot: p.last_seen_slot,
                    window: p.window,
                    owed_reply: p.owed_reply,
                    logged: p.logged_at.is_some(),
                };
                n += 1;
            }
        }
        // Insertion sort newest-first (N <= 16)
        for i in 1..n {
            let mut j = i;
            while j > 0 && out[j].last_seen_slot > out[j - 1].last_seen_slot {
                out.swap(j, j - 1);
                j -= 1;
            }
        }
        n
    }

    /// Expire stale sessions and advance the 2-slot rendezvous scanner if unlocked, then return the plan for `slot`.
    /// `utc_ok` gates scheduled TX based on GPS time uncertainty (`<= +/- 150 ms`).
    pub fn begin_slot(&mut self, slot: u32, utc_ok: bool) -> SlotPlan {
        // An unconfirmed TX from an earlier slot never counts as completed.
        self.pending_tx = None;
        self.expire(slot);

        // Advance scanner on slot boundary only when neither activity-locked nor lingering after `ACK=1`.
        let locked = self.has_active_work_on_current_window(slot)
            || self.lock_until.is_some_and(|u| slot < u)
            || self.linger_until.is_some_and(|u| slot <= u);

        let n_win = (self.cfg.n_windows as usize).clamp(1, MAX_WINDOWS);
        if !locked && n_win > 1 && self.dwell_used >= SCAN_DWELL_SLOTS {
            self.win_idx = ((self.win_idx as usize + 1) % n_win) as u8;
            self.dwell_used = 0;
            self.occupied_subch_mask = 0;
        }
        self.dwell_used = self.dwell_used.saturating_add(1);

        let window = self.current_window();
        if self.cfg.mode == OpMode::Monitor || !utc_ok || !self.can_tx_in_slot(slot) {
            self.occupied_subch_mask = 0;
            return SlotPlan::Rx { window };
        }

        // Priority 1: Owed or retryable directed `RX` / `R+RX` on the current window.
        if let Some((p_idx, ack)) = self.pick_peer_to_answer(slot, window) {
            let p = &self.peers[p_idx];
            let frame = Frame::Rx {
                sender: self.cfg.my_call,
                recipient: p.peer,
                snr_db: p.last_heard_snr,
                ack,
                reserved: false,
            };
            let subchannel_hz = self.pick_subchannel(slot);
            self.occupied_subch_mask = 0;
            self.pending_tx = Some(PendingTx { slot, window, frame });
            return SlotPlan::Tx { window, subchannel_hz, frame };
        }

        // Priority 2: Scheduled `CQ` in `AutoCq` mode when not lingering/waiting on an in-flight peer reply.
        if let OpMode::AutoCq { parity } = self.cfg.mode {
            let waiting_peer = self.has_active_work_on_current_window(slot);
            if !waiting_peer && ((slot as u8) & 1) == (parity & 1) {
                let frame = Frame::Cq {
                    sender: self.cfg.my_call,
                    grid4: self.cfg.my_grid4,
                    tx_pwr: self.cfg.tx_pwr,
                    reserved: (self.cfg.my_suffix & 0x0F) as u16,
                };
                let subchannel_hz = self.pick_subchannel(slot);
                self.occupied_subch_mask = 0;
                self.pending_tx = Some(PendingTx { slot, window, frame });
                return SlotPlan::Tx { window, subchannel_hz, frame };
            }
        }

        self.occupied_subch_mask = 0;
        SlotPlan::Rx { window }
    }

    /// Record that the planned transmission for `slot` actually completed and unkeyed cleanly.
    /// If a TX was aborted or preempted, do not call this (`tx_report_done_at` remains unset).
    pub fn confirm_tx_completed(&mut self, slot: u32) -> Option<QsoRecord> {
        let pending = self.pending_tx.take()?;
        if pending.slot != slot {
            return None;
        }
        self.record_tx_slot(slot);

        let Frame::Rx { recipient, ack, .. } = pending.frame else {
            return None;
        };
        let idx = self.find_peer(recipient, pending.window)?;
        let p = &mut self.peers[idx];
        p.last_tx_slot = Some(slot);
        p.owed_reply = None;

        if p.logged_at.is_some() {
            // Bounded cooldown repeat of `ACK=1`: never writes a duplicate log entry, resets 2-slot linger.
            p.cool_retries = p.cool_retries.saturating_add(1);
            if ack {
                self.linger_until = Some(slot + LINGER_SLOTS);
            }
            return None;
        }

        p.tries = p.tries.saturating_add(1);
        if ack {
            p.tx_r1_done_at = Some(slot);
            // Mandatory 2-slot Post-Completion Linger on this window so if Slot `slot` faded, we are still here
            // on `slot + 1` to hear the peer's retry!
            self.linger_until = Some(slot + LINGER_SLOTS);
        } else {
            p.tx_r0_done_at = Some(slot);
            self.lock_until = Some(slot + EXPIRY_SLOTS);
        }

        self.try_complete_log(idx, slot)
    }

    /// Feed all decoded frames from RX `slot` on `window`, returning the number of newly completed QSOs logged.
    pub fn on_rx_frames(&mut self, slot: u32, window: Freq10, decoded: &[Decoded], out_logs: &mut [Option<QsoRecord>]) -> usize {
        let mut n_logged = 0usize;
        for d in decoded {
            self.mark_subchannel_busy(d.audio_hz);
            match d.frame {
                Frame::Cq { sender, grid4, reserved, .. } => {
                    let suffix = (reserved & 0x0F) as u8;
                    if sender == self.cfg.my_call && suffix == self.cfg.my_suffix {
                        continue;
                    }
                    let idx = self.upsert_peer_suffix(slot, window, sender, suffix);
                    let p = &mut self.peers[idx];
                    p.grid4 = grid4;
                    p.last_heard_snr = d.snr_db;
                    p.last_seen_slot = slot;
                    if sender != self.cfg.my_call
                        && self.cfg.mode != OpMode::Monitor
                        && p.logged_at.is_none()
                        && p.tries < MAX_TRIES
                    {
                        // Queue initial `RX(ACK=0)` if we haven't already sent a report or queued `ACK=1`.
                        if p.owed_reply != Some(true) && p.tx_r0_done_at.is_none() && p.tx_r1_done_at.is_none() {
                            p.owed_reply = Some(false);
                        }
                        self.lock_until = Some(slot + 6);
                    }
                }
                Frame::Rx { sender, recipient, snr_db, ack, .. } => {
                    if sender == self.cfg.my_call {
                        continue;
                    }
                    if recipient != self.cfg.my_call {
                        let idx = self.upsert_peer(slot, window, sender);
                        self.peers[idx].last_heard_snr = d.snr_db;
                        self.peers[idx].last_seen_slot = slot;
                        continue;
                    }

                    let idx = self.upsert_peer(slot, window, sender);
                    let p = &mut self.peers[idx];
                    p.last_heard_snr = d.snr_db;
                    p.last_seen_slot = slot;

                    if p.logged_at.is_some() {
                        // Already logged within cooldown: if peer is still sending `ACK=0`, our `ACK=1` faded!
                        if !ack && p.cool_retries < MAX_COOL_RETRIES && self.cfg.mode != OpMode::Monitor {
                            p.owed_reply = Some(true);
                            self.linger_until = Some(slot + LINGER_SLOTS);
                        }
                        continue;
                    }

                    if ack {
                        // An `ACK=1` (`R+RX`) is valid ONLY if it causally follows a completed `RX(ACK=0)` we sent
                        // to `sender` within `EXPIRY_SLOTS`. A cold or stale `ACK=1` arriving without a prior
                        // `tx_r0_done_at` is ignored so it can never fabricate a log or trigger a reply loop.
                        let valid_r1 = p
                            .tx_r0_done_at
                            .is_some_and(|t0| slot > t0 && slot.saturating_sub(t0) <= EXPIRY_SLOTS);
                        if !valid_r1 {
                            continue;
                        }
                        p.rcvd_snr = Some(snr_db);
                        p.rx_r1_at = Some(slot);
                    } else {
                        p.rcvd_snr = Some(snr_db);
                        p.rx_r0_at = Some(slot);
                        if self.cfg.mode != OpMode::Monitor {
                            p.owed_reply = Some(true);
                            self.lock_until = Some(slot + 6);
                        }
                    }

                    if let Some(rec) = self.try_complete_log(idx, slot) {
                        if ack && self.peers[idx].owed_reply.is_none() {
                            self.lock_until = None;
                        }
                        if n_logged < out_logs.len() {
                            out_logs[n_logged] = Some(rec);
                            n_logged += 1;
                        }
                    }
                }
                Frame::ExtFrag { .. } => {}
            }
        }
        n_logged
    }

    fn try_complete_log(&mut self, idx: usize, slot: u32) -> Option<QsoRecord> {
        let p = &mut self.peers[idx];
        if p.logged_at.is_some() {
            return None;
        }
        // Two valid causal completion chains within `EXPIRY_SLOTS`:
        // 1. Responder chain: we completed `RX(ACK=0)` at `t0`, then decoded `R+RX(ACK=1)` at `r1 > t0`.
        let responder_ok = match (p.tx_r0_done_at, p.rx_r1_at) {
            (Some(t0), Some(r1)) => {
                r1 > t0 && slot.saturating_sub(t0) <= EXPIRY_SLOTS && slot.saturating_sub(r1) <= EXPIRY_SLOTS
            }
            _ => false,
        };
        // 2. Caller chain: we decoded `RX(ACK=0)` at `r0`, then completed `R+RX(ACK=1)` at `t1 > r0`.
        let caller_ok = match (p.rx_r0_at, p.tx_r1_done_at) {
            (Some(r0), Some(t1)) => {
                t1 > r0 && slot.saturating_sub(r0) <= EXPIRY_SLOTS && slot.saturating_sub(t1) <= EXPIRY_SLOTS
            }
            _ => false,
        };
        if !responder_ok && !caller_ok {
            return None;
        }

        p.logged_at = Some(slot);
        p.owed_reply = None;
        let rec = QsoRecord {
            peer: p.peer,
            peer_grid4: p.grid4,
            window: p.window,
            sent_snr: p.last_heard_snr,
            rcvd_snr: p.rcvd_snr.unwrap_or(0),
            completed_slot: slot,
        };
        self.logs.copy_within(0..MAX_LOG_RECENT - 1, 1);
        self.logs[0] = Some(rec);
        self.log_count += 1;
        Some(rec)
    }

    fn expire(&mut self, slot: u32) {
        if self.lock_until.is_some_and(|u| slot >= u) {
            self.lock_until = None;
        }
        if self.linger_until.is_some_and(|u| slot > u) {
            self.linger_until = None;
        }
        for p in &mut self.peers {
            if !p.active {
                continue;
            }
            if let Some(log_slot) = p.logged_at {
                if slot.saturating_sub(log_slot) >= COOLDOWN_SLOTS {
                    *p = PeerSession::empty();
                }
                continue;
            }
            if slot.saturating_sub(p.last_seen_slot) >= EXPIRY_SLOTS {
                *p = PeerSession::empty();
                continue;
            }
            // Drop unanswered session once `MAX_TRIES` is exhausted and peer's reply slot has passed.
            if p.tries >= MAX_TRIES
                && p.owed_reply.is_none()
                && p.last_tx_slot.is_some_and(|t| slot >= t + 2)
            {
                *p = PeerSession::empty();
            }
        }
    }

    fn has_active_work_on_current_window(&self, slot: u32) -> bool {
        if self.cfg.mode == OpMode::Monitor {
            return false;
        }
        let w = self.current_window();
        self.peers.iter().any(|p| {
            p.active
                && p.window == w
                && (p.owed_reply.is_some()
                    || (p.logged_at.is_none()
                        && (p.tx_r0_done_at.is_some() || p.tx_r1_done_at.is_some())
                        && p.tries < MAX_TRIES
                        && slot.saturating_sub(p.last_seen_slot) < EXPIRY_SLOTS))
        })
    }

    fn can_tx_in_slot(&self, slot: u32) -> bool {
        // Never transmit in two consecutive slots (keeps TX/RX parity alternating and structurally <= 3 TX / 60 s).
        for &prev in self.tx_slots.iter().flatten() {
            if prev + 1 == slot {
                return false;
            }
        }
        // Explicit `SAFE-22` rolling 60-second (`6` slots) check: at most `MAX_TX_PER_MIN` (3) TXs.
        let recent = self
            .tx_slots
            .iter()
            .flatten()
            .filter(|&&t| slot.saturating_sub(t) < ROLLING_MIN_SLOTS)
            .count();
        recent < MAX_TX_PER_MIN
    }

    fn record_tx_slot(&mut self, slot: u32) {
        self.tx_slots.copy_within(1.., 0);
        self.tx_slots[MAX_TX_PER_MIN - 1] = Some(slot);
    }

    fn pick_peer_to_answer(&self, slot: u32, window: Freq10) -> Option<(usize, bool)> {
        // 1. Explicit `owed_reply` (`ACK=1` prioritized ahead of `ACK=0`, highest SNR first).
        let mut best: Option<(usize, bool, u8, i8)> = None;
        for (i, p) in self.peers.iter().enumerate() {
            // Only transmit on the opposite parity from the slot we last heard `p` in,
            // preventing same-parity collisions with a peer running Auto-CQ.
            if !p.active || p.window != window || ((slot ^ p.last_seen_slot) & 1) == 0 {
                continue;
            }
            if let Some(ack) = p.owed_reply {
                if p.logged_at.is_some() && p.cool_retries >= MAX_COOL_RETRIES {
                    continue;
                }
                if p.logged_at.is_none() && p.tries >= MAX_TRIES {
                    continue;
                }
                let prio = if ack { 2 } else { 1 };
                if best.is_none_or(|(_, _, bp, bsnr)| (prio, p.last_heard_snr) > (bp, bsnr)) {
                    best = Some((i, ack, prio, p.last_heard_snr));
                }
            } else if p.logged_at.is_none()
                && p.tries < MAX_TRIES
                && p.last_tx_slot.is_some_and(|t| slot >= t + 2)
            {
                // 2. Retry after a missed peer response (`slot >= last_tx_slot + 2`).
                let ack = p.rx_r0_at.is_some();
                if best.is_none_or(|(_, _, bp, bsnr)| (0, p.last_heard_snr) > (bp, bsnr)) {
                    best = Some((i, ack, 0, p.last_heard_snr));
                }
            }
        }
        best.map(|(i, ack, _, _)| (i, ack))
    }

    fn mark_subchannel_busy(&mut self, audio_hz: u16) {
        for (i, &f) in SUBCHANNELS_HZ.iter().enumerate() {
            if audio_hz.abs_diff(f) < 120 {
                self.occupied_subch_mask |= 1 << i;
            }
        }
    }

    fn pick_subchannel(&self, slot: u32) -> u16 {
        let seed = (self.cfg.my_call.raw().wrapping_mul(2_654_435_761) ^ slot.wrapping_mul(0x9E37_79B9)) as usize;
        for step in 0..SUBCHANNELS_HZ.len() {
            let idx = (seed + step) % SUBCHANNELS_HZ.len();
            if (self.occupied_subch_mask & (1 << idx)) == 0 {
                return SUBCHANNELS_HZ[idx];
            }
        }
        SUBCHANNELS_HZ[seed % SUBCHANNELS_HZ.len()]
    }

    fn find_peer(&self, peer: Callsign, window: Freq10) -> Option<usize> {
        self.peers.iter().position(|p| p.active && p.peer == peer && p.window == window)
    }

    fn find_peer_suffix(&self, peer: Callsign, suffix: u8, window: Freq10) -> Option<usize> {
        self.peers
            .iter()
            .position(|p| p.active && p.peer == peer && p.suffix == suffix && p.window == window)
    }

    fn upsert_peer(&mut self, slot: u32, window: Freq10, peer: Callsign) -> usize {
        self.upsert_peer_suffix(slot, window, peer, 0)
    }

    fn upsert_peer_suffix(&mut self, slot: u32, window: Freq10, peer: Callsign, suffix: u8) -> usize {
        if let Some(i) = self.find_peer_suffix(peer, suffix, window) {
            return i;
        }
        if let Some(i) = self.peers.iter().position(|p| !p.active) {
            self.peers[i] = PeerSession {
                active: true,
                peer,
                suffix,
                window,
                last_seen_slot: slot,
                ..PeerSession::empty()
            };
            return i;
        }
        // Evict oldest unlogged entry (or oldest overall if full).
        let oldest = self
            .peers
            .iter()
            .enumerate()
            .min_by_key(|(_, p)| (p.logged_at.is_some(), p.last_seen_slot))
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.peers[oldest] = PeerSession {
            active: true,
            peer,
            suffix,
            window,
            last_seen_slot: slot,
            ..PeerSession::empty()
        };
        oldest
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::freq::mhz;
    use usol950::frame::encode_grid4;

    const W0: Freq10 = mhz(28) + 18_000; // 28.180 MHz
    const W1: Freq10 = mhz(28) + 18_500; // 28.185 MHz

    fn make_engine(call: &[u8], grid: &[u8; 4], mode: OpMode, windows: &[Freq10]) -> Engine {
        let mut w = [0; MAX_WINDOWS];
        for (i, &f) in windows.iter().enumerate() {
            w[i] = f;
        }
        Engine::new(Config {
            my_call: Callsign::encode(call).unwrap(),
            my_suffix: 0,
            my_grid4: encode_grid4(grid).unwrap(),
            tx_pwr: 10,
            mode,
            windows: w,
            n_windows: windows.len() as u8,
        })
    }

    /// Run a simulated multi-window air channel across `n_slots`, optionally erasing specific slots or killing the
    /// path from `cut_from_slot` onward. Asserts SAFE-22 (`<= 3 TX / 6 slots`) on every radio at every step.
    fn sim_pair(
        a: &mut Engine,
        b: &mut Engine,
        n_slots: u32,
        erase_slot: Option<u32>,
        cut_from_slot: Option<u32>,
    ) -> (u32, u32) {
        let (mut tx_a, mut tx_b) = (0u32, 0u32);
        let mut hist_a = std::vec::Vec::new();
        let mut hist_b = std::vec::Vec::new();

        for slot in 0..n_slots {
            let pa = a.begin_slot(slot, true);
            let pb = b.begin_slot(slot, true);

            let air_ok = erase_slot != Some(slot) && cut_from_slot.is_none_or(|c| slot < c);

            if let SlotPlan::Tx { window: wa, subchannel_hz: fa, frame: frm_a } = pa {
                tx_a += 1;
                hist_a.push(slot);
                assert!(hist_a.iter().filter(|&&t| slot.saturating_sub(t) < 6).count() <= MAX_TX_PER_MIN);
                a.confirm_tx_completed(slot);
                if air_ok && matches!(pb, SlotPlan::Rx { window: wb } if wb == wa) {
                    let mut logs = [None; 4];
                    b.on_rx_frames(
                        slot,
                        wa,
                        &[Decoded { frame: frm_a, audio_hz: fa, time_offset_ms: 0, snr_db: -14 }],
                        &mut logs,
                    );
                }
            }

            if let SlotPlan::Tx { window: wb, subchannel_hz: fb, frame: frm_b } = pb {
                tx_b += 1;
                hist_b.push(slot);
                assert!(hist_b.iter().filter(|&&t| slot.saturating_sub(t) < 6).count() <= MAX_TX_PER_MIN);
                b.confirm_tx_completed(slot);
                if air_ok && matches!(pa, SlotPlan::Rx { window: wa } if wa == wb) {
                    let mut logs = [None; 4];
                    a.on_rx_frames(
                        slot,
                        wb,
                        &[Decoded { frame: frm_b, audio_hz: fb, time_offset_ms: 0, snr_db: -12 }],
                        &mut logs,
                    );
                }
            }
        }
        (tx_a, tx_b)
    }

    #[test]
    fn clean_three_slot_exchange_and_aborted_tx_safety() {
        let mut a = make_engine(b"NQ6N", b"FN31", OpMode::AutoCq { parity: 0 }, &[W0]);
        let mut b = make_engine(b"K1ABC", b"FN42", OpMode::Pounce, &[W0, W1]);

        // Verify an unconfirmed/aborted TX never satisfies local completion:
        assert!(matches!(a.begin_slot(0, true), SlotPlan::Tx { .. }));
        // Do NOT call confirm_tx_completed(0) — simulate PTT/path abort!
        let mut logs = [None; 2];
        let k1abc = Callsign::encode(b"K1ABC").unwrap();
        let w1aw = Callsign::encode(b"NQ6N").unwrap();
        a.on_rx_frames(
            1,
            W0,
            &[Decoded {
                frame: Frame::Rx { sender: k1abc, recipient: w1aw, snr_db: -10, ack: true, reserved: false },
                audio_hz: 1050,
                time_offset_ms: 0,
                snr_db: -10,
            }],
            &mut logs,
        );
        assert_eq!(a.total_logged(), 0, "aborted TX must never count as tx_report_done_at");

        // Fresh clean 3-slot run: A logs at Slot 2, B logs at Slot 2, and B goes quiet right away.
        let mut a = make_engine(b"NQ6N", b"FN31", OpMode::AutoCq { parity: 0 }, &[W0]);
        let (_, tx_b) = sim_pair(&mut a, &mut b, 8, None, None);
        assert_eq!((a.total_logged(), b.total_logged()), (1, 1));
        assert_eq!(tx_b, 1, "pouncer B sent exactly 1 frame");
        assert_eq!(a.recent_logs()[0].unwrap().peer, k1abc);
        assert_eq!(b.recent_logs()[0].unwrap().peer, w1aw);
    }

    #[test]
    fn single_slot_erasure_matrix_with_multi_window_scanning() {
        // (b) Slot 0 CQ erased while B scans [W1, W0] (starting on W1, opposite initial scan phase!):
        // 2-slot dwell guarantees rendezvous and mutual completion.
        let mut a = make_engine(b"NQ6N", b"FN31", OpMode::AutoCq { parity: 0 }, &[W0]);
        let mut b = make_engine(b"K1ABC", b"FN42", OpMode::Pounce, &[W1, W0]);
        sim_pair(&mut a, &mut b, 12, Some(0), None);
        assert_eq!((a.total_logged(), b.total_logged()), (1, 1), "Slot 0 erased + opposite scan phase");

        // (c) Slot 1 (B's initial RX ACK=0) erased: B retries in Slot 3, A answers ACK=1 in Slot 4 -> both log 1.
        let mut a = make_engine(b"NQ6N", b"FN31", OpMode::AutoCq { parity: 0 }, &[W0]);
        let mut b = make_engine(b"K1ABC", b"FN42", OpMode::Pounce, &[W0, W1]);
        sim_pair(&mut a, &mut b, 10, Some(1), None);
        assert_eq!((a.total_logged(), b.total_logged()), (1, 1), "Slot 1 erased");

        // (d) Reviewer's critical case: Slot 2 (A's final ACK=1) erased while BOTH A and B have multi-window scan
        // lists! Suppose A is Pounce on [W0, W1] and B is AutoCq on [W0]:
        // Slot 0: B sends CQ on W0 -> A hears B.
        // Slot 1: A sends RX(ACK=0) on W0 -> B hears A.
        // Slot 2: B sends RX(ACK=1) on W0 (B logs!) -> ERASED!
        // Because B lingers on W0 (Post-Completion Linger), when A retries RX(ACK=0) in Slot 3, B is STILL on W0,
        // answers with cooldown ACK=1 in Slot 4 (no duplicate log at B!), and A logs in Slot 4!
        let mut a_scan = make_engine(b"K1ABC", b"FN42", OpMode::Pounce, &[W0, W1]);
        let mut b_scan = make_engine(b"NQ6N", b"FN31", OpMode::AutoCq { parity: 0 }, &[W0, W1]);
        sim_pair(&mut b_scan, &mut a_scan, 12, Some(2), None);
        assert_eq!(
            (b_scan.total_logged(), a_scan.total_logged()),
            (1, 1),
            "Slot 2 (ACK=1) erased under multi-window scan must still converge to 1 log at each end without duplicates"
        );
        // Verify B unlocked its scanner and visited W1 after the linger window passed!
        let mut saw_w1 = false;
        for slot in 12..16 {
            let (SlotPlan::Rx { window } | SlotPlan::Tx { window, .. }) = b_scan.begin_slot(slot, true);
            if window == W1 {
                saw_w1 = true;
            }
        }
        assert!(saw_w1, "scanner must unlock and visit W1 once linger finishes");
    }

    #[test]
    fn two_simultaneous_callers_and_sustained_loss_bounds() {
        // (e) Two callers (B and C) answer A's Slot 0 CQ simultaneously in Slot 1:
        // A works B in Slot 2 and C in Slot 4 while obeying <= 3 TX / 60 s, logging 2 QSOs cleanly.
        let mut a = make_engine(b"NQ6N", b"FN31", OpMode::AutoCq { parity: 0 }, &[W0]);
        let mut b = make_engine(b"K1ABC", b"FN42", OpMode::Pounce, &[W0]);
        let mut c = make_engine(b"VE3XYZ", b"EN93", OpMode::Pounce, &[W0]);

        for slot in 0..10 {
            let pa = a.begin_slot(slot, true);
            let pb = b.begin_slot(slot, true);
            let pc = c.begin_slot(slot, true);

            if let SlotPlan::Tx { window, subchannel_hz, frame } = pa {
                a.confirm_tx_completed(slot);
                let dec = [Decoded { frame, audio_hz: subchannel_hz, time_offset_ms: 0, snr_db: -10 }];
                let mut l = [None; 2];
                if matches!(pb, SlotPlan::Rx { .. }) {
                    b.on_rx_frames(slot, window, &dec, &mut l);
                }
                if matches!(pc, SlotPlan::Rx { .. }) {
                    c.on_rx_frames(slot, window, &dec, &mut l);
                }
            }
            let mut rx_at_a = std::vec::Vec::new();
            if let SlotPlan::Tx { subchannel_hz, frame, .. } = pb {
                b.confirm_tx_completed(slot);
                rx_at_a.push(Decoded { frame, audio_hz: subchannel_hz, time_offset_ms: -80, snr_db: -11 });
            }
            if let SlotPlan::Tx { subchannel_hz, frame, .. } = pc {
                c.confirm_tx_completed(slot);
                rx_at_a.push(Decoded { frame, audio_hz: subchannel_hz, time_offset_ms: 120, snr_db: -15 });
            }
            if !rx_at_a.is_empty() && matches!(pa, SlotPlan::Rx { .. }) {
                let mut l = [None; 4];
                a.on_rx_frames(slot, W0, &rx_at_a, &mut l);
            }
        }
        assert_eq!(a.total_logged(), 2);
        assert_eq!(b.total_logged(), 1);
        assert_eq!(c.total_logged(), 1);

        // (f) Sustained complete path loss after Slot 0 (`cut_from_slot = Some(1)`):
        // B hears A's Slot 0 CQ, then propagation dies completely. B must stop retrying after `MAX_TRIES = 4`
        // and neither station logs a QSO.
        let mut a2 = make_engine(b"NQ6N", b"FN31", OpMode::AutoCq { parity: 0 }, &[W0]);
        let mut b2 = make_engine(b"K1ABC", b"FN42", OpMode::Pounce, &[W0, W1]);
        let (_, tx_b2) = sim_pair(&mut a2, &mut b2, 25, None, Some(1));
        assert_eq!((a2.total_logged(), b2.total_logged()), (0, 0));
        assert_eq!(tx_b2, MAX_TRIES as u32, "B must cap unanswered retries at MAX_TRIES and go quiet");
    }

    #[test]
    fn unsolicited_or_stale_ack1_never_logs_or_replies() {
        let mut pouncer = make_engine(b"NQ6N", b"FN31", OpMode::Pounce, &[W0, W1]);
        let mut caller = make_engine(b"NQ6N", b"FN31", OpMode::AutoCq { parity: 0 }, &[W0]);
        let w1aw = Callsign::encode(b"NQ6N").unwrap();
        let k1abc = Callsign::encode(b"K1ABC").unwrap();
        let cold_ack1 = [Decoded {
            frame: Frame::Rx { sender: k1abc, recipient: w1aw, snr_db: -12, ack: true, reserved: false },
            audio_hz: 1050,
            time_offset_ms: 0,
            snr_db: -12,
        }];

        // Case 1: Fresh Pounce station receives a cold/stale `ACK=1` addressed to it -> must neither reply nor log.
        assert_eq!(pouncer.begin_slot(0, true), SlotPlan::Rx { window: W0 });
        let mut logs = [None; 2];
        assert_eq!(pouncer.on_rx_frames(0, W0, &cold_ack1, &mut logs), 0);
        assert_eq!(pouncer.begin_slot(1, true), SlotPlan::Rx { window: W0 }, "cold ACK=1 must not queue a reply");
        assert_eq!(pouncer.total_logged(), 0);

        // Case 2: AutoCq station sent `CQ` in Slot 0 (not `RX(ACK=0)`!), then receives an unsolicited `ACK=1` in Slot 1
        // -> must not queue `ACK=1` or log a QSO, because `tx_r0_done_at` was never set!
        assert!(matches!(caller.begin_slot(0, true), SlotPlan::Tx { frame: Frame::Cq { .. }, .. }));
        caller.confirm_tx_completed(0);
        assert_eq!(caller.begin_slot(1, true), SlotPlan::Rx { window: W0 });
        assert_eq!(caller.on_rx_frames(1, W0, &cold_ack1, &mut logs), 0);
        let slot2 = caller.begin_slot(2, true);
        assert!(
            matches!(slot2, SlotPlan::Tx { frame: Frame::Cq { .. }, .. }),
            "unsolicited ACK=1 after CQ must not queue an R+RX reply: got {slot2:?}"
        );
        caller.confirm_tx_completed(2);
        assert_eq!(caller.total_logged(), 0);
    }
}
