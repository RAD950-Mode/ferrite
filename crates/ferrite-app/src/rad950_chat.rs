//! RAD950 Conversation Extension runtime state (`docs/sol950-conversation-extension.md`).
//!
//! Bounded, zero-heap session state for the first handheld trial:
//! invite -> ask -> send station card -> exchange quick phrase -> short typed message -> end chat.
//! Chat traffic never adds QSO or worked-grid credit.

use ferrite_core::sol950::usol950::chat::{
    ant_name, decode_chat_frame, encode_chat_frame, encode_literal_frags, estimate_airtime_s, literal_bursts,
    normalize_literal, rig_name, session_ref_for, ChatBody, ChatCtrl, ChatPacket, LiteralReassembler, StationCard,
    CHAT_LITERAL_MAX, CHAT_MAX_FRAGS, PHRASES, QUESTIONS,
};
use ferrite_core::sol950::usol950::{Callsign, Frame};

pub const CHAT_LOG_CAP: usize = 8;
pub const CHAT_TEXT_MAX: usize = 44;

/// Lifecycle state of a RAD950 conversation session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatPhase {
    Idle,
    Inviting,
    IncomingInvite,
    Active,
}

/// Per-message delivery visibility ("Make the wait visible").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatDelivery {
    None,
    Queued,
    Sending { sent: u8, total: u8 },
    AwaitingAck,
    Delivered,
    Cancelled,
}

/// One rendered transcript line in the Chat view.
#[derive(Clone, Copy, Debug)]
pub struct ChatEntry {
    pub from_me: bool,
    pub seq: u8,
    pub status: ChatDelivery,
    pub text: [u8; CHAT_TEXT_MAX],
    pub len: u8,
}

impl ChatEntry {
    pub const EMPTY: Self = Self {
        from_me: false,
        seq: 0,
        status: ChatDelivery::None,
        text: [0; CHAT_TEXT_MAX],
        len: 0,
    };

    pub fn bytes(&self) -> &[u8] {
        &self.text[..self.len as usize]
    }
}

/// Outgoing message staged for preview before pressing Send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatDraft {
    StationCard,
    Question(u8),
    Phrase(u8),
    Literal([u8; CHAT_LITERAL_MAX], u8),
}

/// Format a `StationCard` into a concise human-readable line (`RT-950 Pro 5W dipole 8m 18C`).
pub fn format_station_card(sc: &StationCard, out: &mut [u8]) -> usize {
    let mut n = 0usize;
    let mut push = |s: &[u8]| {
        let take = s.len().min(out.len().saturating_sub(n));
        out[n..n + take].copy_from_slice(&s[..take]);
        n += take;
    };
    push(rig_name(sc.rig).as_bytes());
    if sc.pwr_dbm != 127 {
        push(b", ");
        let w = match sc.pwr_dbm {
            0..=25 => b"0.1W".as_slice(),
            26..=32 => b"1W",
            33..=35 => b"2W",
            36..=38 => b"5W",
            39..=41 => b"10W",
            42..=45 => b"25W",
            _ => b"50W+",
        };
        push(w);
    }
    if sc.amp {
        push(b" amp");
    }
    if sc.ant != 0 {
        push(b", ");
        push(ant_name(sc.ant).as_bytes());
    }
    if sc.height_m != 127 {
        push(b" ");
        let mut num = [0u8; 4];
        let k = fmt_u8(sc.height_m, &mut num);
        push(&num[..k]);
        push(b"m");
    }
    if sc.temp_c != i8::MIN {
        push(b" ");
        if sc.temp_c < 0 {
            push(b"-");
        }
        let mut num = [0u8; 4];
        let k = fmt_u8(sc.temp_c.unsigned_abs(), &mut num);
        push(&num[..k]);
        push(b"C");
    }
    n
}

fn fmt_u8(mut v: u8, out: &mut [u8; 4]) -> usize {
    if v == 0 {
        out[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 4];
    let mut k = 0usize;
    while v > 0 {
        tmp[k] = b'0' + (v % 10);
        v /= 10;
        k += 1;
    }
    for i in 0..k {
        out[i] = tmp[k - 1 - i];
    }
    k
}

/// Complete bounded conversation state attached to `Sol950Host`.
pub struct ChatState {
    pub phase: ChatPhase,
    pub peer: Callsign,
    pub session_ref: u16,
    pub my_parity: u8,
    pub expires_slot: u32,
    pub station_card: StationCard,
    pub next_seq: u8,
    /// Last topic asked by the peer (`Some(0 | 1)` highlights `My station` in Reply menu).
    pub last_peer_topic: Option<u8>,
    pub entries: [ChatEntry; CHAT_LOG_CAP],
    pub entry_count: u8,
    /// Staged draft for Preview screen.
    pub draft: Option<ChatDraft>,
    /// Outgoing burst queue (up to 8 fragments for one active message).
    tx_queue: [ChatPacket; CHAT_MAX_FRAGS],
    tx_total: u8,
    tx_sent: u8,
    tx_in_flight: bool,
    tx_entry_idx: Option<u8>,
    /// Pending 1-burst control/ACK frame (higher priority than queued chat text).
    pending_ctrl: Option<ChatCtrl>,
    /// Incoming multi-fragment literal reassembler.
    reasm: LiteralReassembler,
}

impl Default for ChatState {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatState {
    pub const fn new() -> Self {
        let dummy = ChatPacket { session_ref: 0, seq: 0, body: ChatBody::Phrase(0) };
        Self {
            phase: ChatPhase::Idle,
            peer: match Callsign::from_raw(0) {
                Some(c) => c,
                None => unreachable!(),
            },
            session_ref: 0,
            my_parity: 0,
            expires_slot: 0,
            station_card: StationCard::DEFAULT_RT950,
            next_seq: 0,
            last_peer_topic: None,
            entries: [ChatEntry::EMPTY; CHAT_LOG_CAP],
            entry_count: 0,
            draft: None,
            tx_queue: [dummy; CHAT_MAX_FRAGS],
            tx_total: 0,
            tx_sent: 0,
            tx_in_flight: false,
            tx_entry_idx: None,
            pending_ctrl: None,
            reasm: LiteralReassembler::new(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.phase != ChatPhase::Idle
    }

    pub fn has_outgoing(&self) -> bool {
        self.tx_total > 0 && self.tx_sent < self.tx_total
    }

    /// Start an outgoing invitation to `peer` after a completed QSO.
    pub fn invite(&mut self, my_call: Callsign, peer: Callsign, slot: u32) {
        self.reset_session();
        self.phase = ChatPhase::Inviting;
        self.peer = peer;
        self.session_ref = session_ref_for(my_call.raw(), peer.raw(), slot);
        self.my_parity = (slot.wrapping_add(1) & 1) as u8;
        self.expires_slot = slot.saturating_add(6);
        self.pending_ctrl = Some(ChatCtrl::Invite);
    }

    /// Accept an incoming invitation from `peer`.
    pub fn accept_invite(&mut self) {
        if self.phase == ChatPhase::IncomingInvite {
            self.phase = ChatPhase::Active;
            self.pending_ctrl = Some(ChatCtrl::Accept);
        }
    }

    /// Decline an incoming invitation and return to listening.
    pub fn decline_invite(&mut self) {
        if self.phase == ChatPhase::IncomingInvite {
            self.pending_ctrl = Some(ChatCtrl::Decline);
            self.phase = ChatPhase::Idle;
        }
    }

    /// End the active conversation cleanly.
    pub fn end_chat(&mut self) {
        if self.phase != ChatPhase::Idle {
            self.cancel_send();
            self.pending_ctrl = Some(ChatCtrl::End);
            self.phase = ChatPhase::Idle;
        }
    }

    /// Cancel any queued/in-progress outgoing message.
    pub fn cancel_send(&mut self) {
        if let Some(idx) = self.tx_entry_idx.take() {
            if (idx as usize) < self.entry_count as usize {
                self.entries[idx as usize].status = ChatDelivery::Cancelled;
            }
        }
        self.tx_total = 0;
        self.tx_sent = 0;
        self.tx_in_flight = false;
    }

    pub fn set_literal_draft(&mut self, raw: &[u8]) {
        let mut norm = [b' '; CHAT_LITERAL_MAX];
        let len = normalize_literal(raw, &mut norm);
        if len > 0 {
            self.draft = Some(ChatDraft::Literal(norm, len as u8));
        } else {
            self.draft = None;
        }
    }

    /// Render the current draft into `out`, returning `(text_len, bursts, est_airtime_s)`.
    pub fn preview_draft(&self, out: &mut [u8; 64]) -> (usize, u8, u16) {
        let Some(d) = self.draft else { return (0, 0, 0) };
        match d {
            ChatDraft::StationCard => {
                let n = format_station_card(&self.station_card, out);
                (n, 1, estimate_airtime_s(1))
            }
            ChatDraft::Question(topic) => {
                let s = QUESTIONS.get(topic as usize).copied().unwrap_or("Question?").as_bytes();
                let n = s.len().min(out.len());
                out[..n].copy_from_slice(&s[..n]);
                (n, 1, estimate_airtime_s(1))
            }
            ChatDraft::Phrase(id) => {
                let s = PHRASES.get(id as usize).copied().unwrap_or("73!").as_bytes();
                let n = s.len().min(out.len());
                out[..n].copy_from_slice(&s[..n]);
                (n, 1, estimate_airtime_s(1))
            }
            ChatDraft::Literal(buf, len) => {
                let n = (len as usize).min(out.len());
                out[..n].copy_from_slice(&buf[..n]);
                let b = literal_bursts(n);
                (n, b, estimate_airtime_s(b))
            }
        }
    }

    /// Commit the current `draft` into the outgoing TX queue and append it to the transcript as `Queued`.
    pub fn queue_draft(&mut self) -> bool {
        let Some(d) = self.draft.take() else { return false };
        if self.phase != ChatPhase::Active {
            return false;
        }
        self.cancel_send();
        let seq = self.next_seq & 7;
        self.next_seq = self.next_seq.wrapping_add(1) & 7;
        let mut preview = [0u8; 64];
        let (plen, n_frags) = match d {
            ChatDraft::StationCard => {
                self.tx_queue[0] = ChatPacket {
                    session_ref: self.session_ref,
                    seq,
                    body: ChatBody::Station(self.station_card),
                };
                (format_station_card(&self.station_card, &mut preview), 1usize)
            }
            ChatDraft::Question(topic) => {
                self.tx_queue[0] = ChatPacket {
                    session_ref: self.session_ref,
                    seq,
                    body: ChatBody::Question { topic, variant: 0 },
                };
                let s = QUESTIONS.get(topic as usize).copied().unwrap_or("Question?").as_bytes();
                let n = s.len().min(preview.len());
                preview[..n].copy_from_slice(&s[..n]);
                (n, 1usize)
            }
            ChatDraft::Phrase(id) => {
                self.tx_queue[0] = ChatPacket {
                    session_ref: self.session_ref,
                    seq,
                    body: ChatBody::Phrase(id),
                };
                let s = PHRASES.get(id as usize).copied().unwrap_or("73!").as_bytes();
                let n = s.len().min(preview.len());
                preview[..n].copy_from_slice(&s[..n]);
                (n, 1usize)
            }
            ChatDraft::Literal(buf, len) => {
                let n = len as usize;
                let nf = encode_literal_frags(self.session_ref, seq, &buf[..n], false, &mut self.tx_queue);
                preview[..n].copy_from_slice(&buf[..n]);
                (n, nf)
            }
        };
        if n_frags == 0 {
            return false;
        }
        self.tx_total = n_frags as u8;
        self.tx_sent = 0;
        self.tx_in_flight = false;
        let idx = self.push_entry(true, seq, ChatDelivery::Queued, &preview[..plen]);
        self.tx_entry_idx = Some(idx);
        true
    }

    /// Called on each 10 s UTC slot boundary; returns a `Frame` to transmit on our parity slot if queued.
    pub fn on_slot_start(&mut self, slot: u32, utc_ok: bool) -> Option<Frame> {
        if (self.phase == ChatPhase::Inviting || self.phase == ChatPhase::IncomingInvite) && slot >= self.expires_slot {
            self.phase = ChatPhase::Idle;
            self.pending_ctrl = None;
            return None;
        }
        if !utc_ok {
            return None;
        }
        if let Some(ctrl) = self.pending_ctrl.take() {
            if (slot & 1) as u8 == self.my_parity || matches!(ctrl, ChatCtrl::End | ChatCtrl::Decline | ChatCtrl::Accept) {
                let pkt = ChatPacket { session_ref: self.session_ref, seq: self.next_seq, body: ChatBody::Ctrl(ctrl) };
                return Some(encode_chat_frame(pkt));
            } else {
                self.pending_ctrl = Some(ctrl);
                return None;
            }
        }
        if self.phase == ChatPhase::Active && (slot & 1) as u8 == self.my_parity && self.tx_sent < self.tx_total {
            let pkt = self.tx_queue[self.tx_sent as usize];
            self.tx_in_flight = true;
            if let Some(idx) = self.tx_entry_idx {
                if (idx as usize) < self.entry_count as usize {
                    self.entries[idx as usize].status =
                        ChatDelivery::Sending { sent: self.tx_sent + 1, total: self.tx_total };
                }
            }
            return Some(encode_chat_frame(pkt));
        }
        None
    }

    /// Called when a chat TX burst finishes (`ok = true`) or aborts (`ok = false`).
    pub fn on_tx_done(&mut self, ok: bool) {
        if !self.tx_in_flight {
            return;
        }
        self.tx_in_flight = false;
        if ok {
            self.tx_sent = self.tx_sent.saturating_add(1);
            if let Some(idx) = self.tx_entry_idx {
                if (idx as usize) < self.entry_count as usize {
                    self.entries[idx as usize].status = if self.tx_sent >= self.tx_total {
                        ChatDelivery::AwaitingAck
                    } else {
                        ChatDelivery::Sending { sent: self.tx_sent, total: self.tx_total }
                    };
                }
            }
        }
    }

    /// Process a received `Frame::ExtFrag`; returns `true` if it was a RAD950 Chat packet.
    pub fn on_rx_frame(&mut self, frame: Frame, slot: u32) -> bool {
        let Some(pkt) = decode_chat_frame(frame) else {
            return false;
        };
        match pkt.body {
            ChatBody::Ctrl(ChatCtrl::Invite) => {
                if self.phase == ChatPhase::Idle {
                    self.reset_session();
                    self.phase = ChatPhase::IncomingInvite;
                    self.session_ref = pkt.session_ref;
                    self.my_parity = (slot.wrapping_add(1) & 1) as u8;
                    self.expires_slot = slot.saturating_add(6);
                }
            }
            ChatBody::Ctrl(ChatCtrl::Accept) if pkt.session_ref == self.session_ref => {
                if self.phase == ChatPhase::Inviting {
                    self.phase = ChatPhase::Active;
                }
            }
            ChatBody::Ctrl(ChatCtrl::Decline | ChatCtrl::End) if pkt.session_ref == self.session_ref => {
                self.cancel_send();
                self.phase = ChatPhase::Idle;
            }
            ChatBody::Ctrl(ChatCtrl::Ack { ack_seq, .. }) if pkt.session_ref == self.session_ref => {
                for i in 0..self.entry_count as usize {
                    let e = &mut self.entries[i];
                    if e.from_me && e.seq == ack_seq {
                        e.status = ChatDelivery::Delivered;
                    }
                }
            }
            ChatBody::Station(sc) if self.phase == ChatPhase::Active && pkt.session_ref == self.session_ref => {
                let mut buf = [0u8; 64];
                let n = format_station_card(&sc, &mut buf);
                self.last_peer_topic = None;
                self.push_entry(false, pkt.seq, ChatDelivery::None, &buf[..n]);
                self.pending_ctrl = Some(ChatCtrl::Ack { ack_seq: pkt.seq, frag_mask: 1 });
            }
            ChatBody::Question { topic, .. } if self.phase == ChatPhase::Active && pkt.session_ref == self.session_ref => {
                let s = QUESTIONS.get(topic as usize).copied().unwrap_or("Question?").as_bytes();
                self.last_peer_topic = Some(topic);
                self.push_entry(false, pkt.seq, ChatDelivery::None, s);
                self.pending_ctrl = Some(ChatCtrl::Ack { ack_seq: pkt.seq, frag_mask: 1 });
            }
            ChatBody::Phrase(id) if self.phase == ChatPhase::Active && pkt.session_ref == self.session_ref => {
                let s = PHRASES.get(id as usize).copied().unwrap_or("73!").as_bytes();
                self.last_peer_topic = None;
                self.push_entry(false, pkt.seq, ChatDelivery::None, s);
                self.pending_ctrl = Some(ChatCtrl::Ack { ack_seq: pkt.seq, frag_mask: 1 });
            }
            ChatBody::LiteralFrag { frag_idx, is_final, char_cnt, chars, .. }
                if self.phase == ChatPhase::Active && pkt.session_ref == self.session_ref =>
            {
                if let Some((len, mask)) = self.reasm.push(pkt.seq, frag_idx, is_final, char_cnt, chars) {
                    let mut copy = [0u8; CHAT_LITERAL_MAX];
                    let n = len.min(CHAT_LITERAL_MAX);
                    copy[..n].copy_from_slice(&self.reasm.bytes(n)[..n]);
                    self.last_peer_topic = None;
                    self.push_entry(false, pkt.seq, ChatDelivery::None, &copy[..n]);
                    self.pending_ctrl = Some(ChatCtrl::Ack { ack_seq: pkt.seq, frag_mask: mask });
                }
            }
            _ => {}
        }
        true
    }

    /// Seconds until our next transmit parity slot (`0..=19`).
    pub fn secs_until_tx(&self, utc_sec: u32) -> u8 {
        if utc_sec == 0 {
            return 0;
        }
        let slot = utc_sec / 10;
        let rem = 10 - (utc_sec % 10);
        let next_slot = slot + 1;
        if (next_slot & 1) as u8 == self.my_parity {
            rem as u8
        } else {
            (rem + 10) as u8
        }
    }

    fn reset_session(&mut self) {
        self.entries = [ChatEntry::EMPTY; CHAT_LOG_CAP];
        self.entry_count = 0;
        self.draft = None;
        self.tx_total = 0;
        self.tx_sent = 0;
        self.tx_in_flight = false;
        self.tx_entry_idx = None;
        self.pending_ctrl = None;
        self.last_peer_topic = None;
        self.next_seq = 0;
        self.reasm.reset();
    }

    fn push_entry(&mut self, from_me: bool, seq: u8, status: ChatDelivery, text: &[u8]) -> u8 {
        if self.entry_count as usize == CHAT_LOG_CAP {
            for i in 1..CHAT_LOG_CAP {
                self.entries[i - 1] = self.entries[i];
            }
            self.entry_count -= 1;
            if let Some(idx) = self.tx_entry_idx.as_mut() {
                *idx = idx.saturating_sub(1);
            }
        }
        let idx = self.entry_count as usize;
        let mut buf = [0u8; CHAT_TEXT_MAX];
        let n = text.len().min(CHAT_TEXT_MAX);
        buf[..n].copy_from_slice(&text[..n]);
        self.entries[idx] = ChatEntry { from_me, seq, status, text: buf, len: n as u8 };
        self.entry_count += 1;
        idx as u8
    }
}
