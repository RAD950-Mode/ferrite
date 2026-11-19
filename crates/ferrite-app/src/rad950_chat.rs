//! RAD950 Conversation Extension runtime state (`docs/sol950-conversation-extension.md`).
//!
//! Bounded, zero-heap session state for the first handheld trial:
//! invite -> ask -> send station card -> exchange quick phrase -> short typed message -> end chat.
//! Chat traffic never adds QSO or worked-grid credit.

use ferrite_core::sol950::usol950::chat::{
    ant_name, decode_chat_frame, encode_chat_frame, encode_literal_frags, estimate_airtime_s, literal_bursts,
    move_band_ref_hz10, normalize_literal, rig_name, session_ref_for, wind_dir_name, wx_cond_name, AboutCard, ChatBody,
    ChatCtrl, ChatPacket, LiteralReassembler, RendezvousAction, RendezvousCard, StationCard, WeatherCard,
    CHAT_LITERAL_MAX, CHAT_MAX_FRAGS, PHRASES, QUESTIONS,
};
use ferrite_core::sol950::usol950::distress::{
    decode_distress_frame, distress_assoc_cksum, encode_distress_frame, grid4_matches_coords, DistressAlert,
    DistressCategory, DistressFrame, DistressLocSource, DistressPosition, GRID4_UNKNOWN,
};
use ferrite_core::sol950::usol950::{Callsign, Frame};

/// RX status of a received standalone distress incident.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DistressRxStatus {
    AlertReceived,
    PositionPending,
    PositionLinked,
    PositionConflict,
}

/// Received standalone distress incident (visible in Operate and Chat; never adds QSO/grid credit).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxDistress {
    pub alert: DistressAlert,
    pub alert_slot: u32,
    pub dial_hz10: u32,
    pub subch: u8,
    pub lat_e4: Option<i32>,
    pub lon_e4: Option<i32>,
    pub status: DistressRxStatus,
}

/// Outgoing + incoming standalone distress state.
#[derive(Clone, Copy, Debug)]
pub struct DistressState {
    pub category: DistressCategory,
    pub loc_src: DistressLocSource,
    pub lat_e4: i32,
    pub lon_e4: i32,
    pub tx_seq: u8,
    pub armed_alert: Option<DistressAlert>,
    pub armed_pos: Option<(u32, DistressPosition)>,
    pub armed_dial_hz10: u32,
    pub rx: Option<RxDistress>,
    /// Start timestamp (ms) of the 3rd consecutive PTT hold on the Distress confirmation screen.
    pub ptt_hold_start_ms: Option<u32>,
}

impl Default for DistressState {
    fn default() -> Self {
        Self::new()
    }
}

impl DistressState {
    pub const fn new() -> Self {
        Self {
            category: DistressCategory::Unspecified,
            loc_src: DistressLocSource::FreshGps,
            lat_e4: 377_749,    // 37.7749 N (CM87 default)
            lon_e4: -1_224_194, // -122.4194 W
            tx_seq: 0,
            armed_alert: None,
            armed_pos: None,
            armed_dial_hz10: crate::sol950::SOL950_DIALS[0],
            rx: None,
            ptt_hold_start_ms: None,
        }
    }

    pub fn has_tx(&self) -> bool {
        self.armed_alert.is_some() || self.armed_pos.is_some()
    }

    pub fn arm_tx(&mut self, sender: Callsign, grid4: Option<u16>, dial_hz10: u32) {
        self.tx_seq = self.tx_seq.wrapping_add(1) & 0x1F;
        self.armed_dial_hz10 = dial_hz10;
        self.armed_alert = Some(DistressAlert {
            sender,
            grid4: grid4.unwrap_or(GRID4_UNKNOWN),
            category: self.category,
            loc_src: self.loc_src,
            seq: self.tx_seq,
        });
        self.armed_pos = None;
    }

    pub fn on_slot_start(&mut self, slot: u32, utc_ok: bool) -> Option<Frame> {
        if !utc_ok {
            return None;
        }
        if let Some(alert) = self.armed_alert.take() {
            if alert.loc_src != DistressLocSource::Unavailable {
                let ck = distress_assoc_cksum(&alert, slot, self.armed_dial_hz10, 0, self.lat_e4, self.lon_e4);
                if let Some(pos) = DistressPosition::new(self.lat_e4, self.lon_e4, ck) {
                    self.armed_pos = Some((slot.saturating_add(2), pos));
                }
            }
            return Some(encode_distress_frame(DistressFrame::Alert(alert)));
        }
        if let Some((pos_slot, pos)) = self.armed_pos {
            if slot >= pos_slot {
                self.armed_pos = None;
                return Some(encode_distress_frame(DistressFrame::Position(pos)));
            }
        }
        None
    }

    pub fn on_rx_frame(&mut self, frame: Frame, slot: u32, dial_hz10: u32) -> bool {
        let Some(df) = decode_distress_frame(frame) else {
            return false;
        };
        match df {
            DistressFrame::Alert(alert) => {
                let status = if alert.loc_src == DistressLocSource::Unavailable {
                    DistressRxStatus::AlertReceived
                } else {
                    DistressRxStatus::PositionPending
                };
                // Refresh existing incident if same sender+seq, preserving linked position if already linked.
                if let Some(cur) = self.rx.as_mut() {
                    if cur.alert.sender == alert.sender && cur.alert.seq == alert.seq {
                        cur.alert = alert;
                        cur.alert_slot = slot;
                        return true;
                    }
                }
                self.rx = Some(RxDistress {
                    alert,
                    alert_slot: slot,
                    dial_hz10,
                    subch: 0,
                    lat_e4: None,
                    lon_e4: None,
                    status,
                });
            }
            DistressFrame::Position(pos) => {
                if let Some(cur) = self.rx.as_mut() {
                    if slot == cur.alert_slot.saturating_add(2) {
                        let expected_ck =
                            distress_assoc_cksum(&cur.alert, cur.alert_slot, cur.dial_hz10, cur.subch, pos.lat_e4, pos.lon_e4);
                        if pos.assoc_cksum == expected_ck {
                            if grid4_matches_coords(cur.alert.grid4, pos.lat_e4, pos.lon_e4) {
                                cur.lat_e4 = Some(pos.lat_e4);
                                cur.lon_e4 = Some(pos.lon_e4);
                                cur.status = DistressRxStatus::PositionLinked;
                            } else {
                                cur.status = DistressRxStatus::PositionConflict;
                            }
                        }
                    }
                }
            }
        }
        true
    }
}

pub const CHAT_LOG_CAP: usize = 8;
pub const CHAT_TEXT_MAX: usize = 44;
pub const CHAT_MAX_RETRIES: u8 = 2;

/// Local reminder saved when a `Schedule QSO` offer is accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SavedReminder {
    pub peer: Callsign,
    pub utc_day: u32,
    pub utc_min: u16,
    pub dial_preset: u8,
    pub profile: u8,
    pub subch: u8,
}

/// Active agreed near-term frequency move (`Move to frequency`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActiveMove {
    pub peer: Callsign,
    pub dest_dial_hz10: u32,
    pub source_dial_hz10: u32,
    pub target_slot: u32,
    pub retuned: bool,
    pub peer_heard: bool,
    pub expired: bool,
}

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
    Incomplete,
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
    WeatherCard,
    AboutCard,
    Question(u8),
    Phrase(u8),
    Literal([u8; CHAT_LITERAL_MAX], u8),
    Rendezvous(RendezvousCard),
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

/// Format a `WeatherCard` (`Light rain, 12C, wind NW`).
pub fn format_weather_card(wc: &WeatherCard, out: &mut [u8]) -> usize {
    let mut n = 0usize;
    let mut push = |s: &[u8]| {
        let take = s.len().min(out.len().saturating_sub(n));
        out[n..n + take].copy_from_slice(&s[..take]);
        n += take;
    };
    push(wx_cond_name(wc.cond).as_bytes());
    if wc.temp_c != i8::MIN {
        push(b", ");
        if wc.temp_c < 0 {
            push(b"-");
        }
        let mut num = [0u8; 4];
        let k = fmt_u8(wc.temp_c.unsigned_abs(), &mut num);
        push(&num[..k]);
        push(b"C");
    }
    let dir = wind_dir_name(wc.wind_dir);
    if !dir.is_empty() || wc.wind_kph != 127 {
        push(b", wind");
        if wc.wind_kph != 127 {
            push(b" ");
            let mut num = [0u8; 4];
            let k = fmt_u8(wc.wind_kph, &mut num);
            push(&num[..k]);
            push(b"kph");
        }
        if !dir.is_empty() {
            push(b" ");
            push(dir.as_bytes());
        }
    }
    n
}

/// Format an `AboutCard` (`Age 42, licensed 1994` or `First licensed in 1994`).
pub fn format_about_card(ac: &AboutCard, out: &mut [u8]) -> usize {
    let mut n = 0usize;
    let mut push = |s: &[u8]| {
        let take = s.len().min(out.len().saturating_sub(n));
        out[n..n + take].copy_from_slice(&s[..take]);
        n += take;
    };
    if ac.age > 0 {
        push(b"Age ");
        let mut num = [0u8; 4];
        let k = fmt_u8(ac.age, &mut num);
        push(&num[..k]);
        if ac.year_licensed > 0 {
            push(b", lic ");
        }
    } else if ac.year_licensed > 0 {
        push(b"First licensed in ");
    } else {
        push(b"About me");
    }
    if ac.year_licensed > 0 {
        let mut num = [0u8; 6];
        let k = fmt_u16(ac.year_licensed, &mut num);
        push(&num[..k]);
    }
    n
}

/// Format a `RendezvousCard` (`Meet +1d 18:30Z 28.082` or `Move 28.082 @slot`).
pub fn format_rendezvous_card(rv: &RendezvousCard, out: &mut [u8]) -> usize {
    fn push_s(out: &mut [u8], n: &mut usize, s: &[u8]) {
        let take = s.len().min(out.len().saturating_sub(*n));
        out[*n..*n + take].copy_from_slice(&s[..take]);
        *n += take;
    }
    fn push_2d(out: &mut [u8], n: &mut usize, v: u16) {
        if *n + 2 <= out.len() {
            out[*n] = b'0' + ((v / 10) % 10) as u8;
            out[*n + 1] = b'0' + (v % 10) as u8;
            *n += 2;
        }
    }
    let mut n = 0usize;
    let prefix = |a: RendezvousAction, base: &'static [u8]| -> &'static [u8] {
        match a {
            RendezvousAction::Offer => base,
            RendezvousAction::Accept => b"Accepted ",
            RendezvousAction::Counter => b"Counter ",
            RendezvousAction::Decline => b"Declined ",
        }
    };
    match *rv {
        RendezvousCard::Schedule { action, days_ahead, utc_min, dial_preset, .. } => {
            push_s(out, &mut n, prefix(action, b"Meet "));
            if days_ahead == 0 {
                push_s(out, &mut n, b"today ");
            } else if days_ahead == 1 {
                push_s(out, &mut n, b"tmrw ");
            } else {
                push_s(out, &mut n, b"+");
                let mut num = [0u8; 4];
                let k = fmt_u8(days_ahead, &mut num);
                push_s(out, &mut n, &num[..k]);
                push_s(out, &mut n, b"d ");
            }
            push_2d(out, &mut n, utc_min / 60);
            push_s(out, &mut n, b":");
            push_2d(out, &mut n, utc_min % 60);
            push_s(out, &mut n, b"Z ");
            let dial = crate::sol950::SOL950_DIALS
                .get(dial_preset as usize)
                .copied()
                .unwrap_or(crate::sol950::SOL950_DIALS[0]);
            fmt_dial_mhz(dial, out, &mut n);
        }
        RendezvousCard::MoveFreq { action, band_id, dial_offset_10hz, .. } => {
            push_s(out, &mut n, prefix(action, b"Move "));
            if let Some(base) = move_band_ref_hz10(band_id) {
                fmt_dial_mhz(base.saturating_add(dial_offset_10hz), out, &mut n);
            } else {
                push_s(out, &mut n, b"band?");
            }
            push_s(out, &mut n, b" MHz");
        }
    }
    n
}

fn fmt_dial_mhz(dial_hz10: u32, out: &mut [u8], n: &mut usize) {
    let mhz = (dial_hz10 / 100_000) as u16;
    let khz = ((dial_hz10 % 100_000) / 100) as u16;
    let mut num = [0u8; 6];
    let k = fmt_u16(mhz, &mut num);
    let take = k.min(out.len().saturating_sub(*n));
    out[*n..*n + take].copy_from_slice(&num[..take]);
    *n += take;
    if *n + 4 <= out.len() {
        out[*n] = b'.';
        out[*n + 1] = b'0' + ((khz / 100) % 10) as u8;
        out[*n + 2] = b'0' + ((khz / 10) % 10) as u8;
        out[*n + 3] = b'0' + (khz % 10) as u8;
        *n += 4;
    }
}

fn fmt_u16(mut v: u16, out: &mut [u8; 6]) -> usize {
    if v == 0 {
        out[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 6];
    let mut k = 0usize;
    while v > 0 {
        tmp[k] = b'0' + (v % 10) as u8;
        v /= 10;
        k += 1;
    }
    for i in 0..k {
        out[i] = tmp[k - 1 - i];
    }
    k
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
    pub weather_card: WeatherCard,
    pub about_card: AboutCard,
    pub next_seq: u8,
    /// Last topic asked by the peer (`Some(0 | 1)` -> My station, `Some(2)` -> Weather).
    pub last_peer_topic: Option<u8>,
    pub entries: [ChatEntry; CHAT_LOG_CAP],
    pub entry_count: u8,
    /// Staged draft for Preview screen.
    pub draft: Option<ChatDraft>,
    /// Outgoing burst queue (up to 8 fragments for one active message) with selective retransmission.
    tx_queue: [ChatPacket; CHAT_MAX_FRAGS],
    tx_total: u8,
    tx_seq: u8,
    tx_need_mask: u8,
    tx_ack_mask: u8,
    tx_sent_mask: u8,
    tx_inflight_idx: Option<u8>,
    tx_retries: u8,
    tx_ack_deadline_slot: u32,
    tx_entry_idx: Option<u8>,
    /// Pending 1-burst control/ACK frame (higher priority than queued chat text).
    pending_ctrl: Option<ChatCtrl>,
    /// Bitmask of peer sequence numbers (`0..=7`) already displayed (duplicate suppression).
    rx_delivered_seq_mask: u8,
    /// Incoming multi-fragment literal reassembler.
    reasm: LiteralReassembler,
    /// Incoming Schedule/Move offer awaiting operator Accept / Counter / Decline.
    pub pending_offer: Option<RendezvousCard>,
    /// Local reminder saved when a Schedule QSO offer is accepted.
    pub saved_reminder: Option<SavedReminder>,
    /// Active near-term frequency move countdown + retune confirmation state.
    pub active_move: Option<ActiveMove>,
    /// Dial preset index staged in More menu for Schedule/Move offers (`0..SOL950_DIALS.len()`).
    pub offer_dial_idx: u8,
    /// Current session UTC day (for resolving Schedule days_ahead across midnight) and source dial.
    pub session_utc_day: u32,
    pub source_dial_hz10: u32,
    /// Standalone Distress alert & precise location state (takes TX priority over optional chat).
    pub distress: DistressState,
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
            weather_card: WeatherCard::DEFAULT,
            about_card: AboutCard::DEFAULT,
            next_seq: 0,
            last_peer_topic: None,
            entries: [ChatEntry::EMPTY; CHAT_LOG_CAP],
            entry_count: 0,
            draft: None,
            tx_queue: [dummy; CHAT_MAX_FRAGS],
            tx_total: 0,
            tx_seq: 0,
            tx_need_mask: 0,
            tx_ack_mask: 0,
            tx_sent_mask: 0,
            tx_inflight_idx: None,
            tx_retries: 0,
            tx_ack_deadline_slot: 0,
            tx_entry_idx: None,
            pending_ctrl: None,
            rx_delivered_seq_mask: 0,
            reasm: LiteralReassembler::new(),
            pending_offer: None,
            saved_reminder: None,
            active_move: None,
            offer_dial_idx: 1, // default secondary 10m preset (28.082 MHz)
            session_utc_day: 0,
            source_dial_hz10: crate::sol950::SOL950_DIALS[0],
            distress: DistressState::new(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.phase != ChatPhase::Idle || self.distress.has_tx()
    }

    pub fn has_outgoing(&self) -> bool {
        self.tx_need_mask != 0
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
            if (idx as usize) < self.entry_count as usize && self.tx_need_mask != 0 {
                self.entries[idx as usize].status = ChatDelivery::Cancelled;
            }
        }
        self.tx_total = 0;
        self.tx_need_mask = 0;
        self.tx_ack_mask = 0;
        self.tx_sent_mask = 0;
        self.tx_inflight_idx = None;
        self.tx_retries = 0;
        self.tx_ack_deadline_slot = 0;
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
            ChatDraft::WeatherCard => {
                let n = format_weather_card(&self.weather_card, out);
                (n, 1, estimate_airtime_s(1))
            }
            ChatDraft::AboutCard => {
                let n = format_about_card(&self.about_card, out);
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
            ChatDraft::Rendezvous(rv) => {
                let n = format_rendezvous_card(&rv, out);
                (n, 1, estimate_airtime_s(1))
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
            ChatDraft::WeatherCard => {
                self.tx_queue[0] = ChatPacket {
                    session_ref: self.session_ref,
                    seq,
                    body: ChatBody::Weather(self.weather_card),
                };
                (format_weather_card(&self.weather_card, &mut preview), 1usize)
            }
            ChatDraft::AboutCard => {
                self.tx_queue[0] = ChatPacket {
                    session_ref: self.session_ref,
                    seq,
                    body: ChatBody::About(self.about_card),
                };
                (format_about_card(&self.about_card, &mut preview), 1usize)
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
            ChatDraft::Rendezvous(rv) => {
                self.tx_queue[0] = ChatPacket {
                    session_ref: self.session_ref,
                    seq,
                    body: ChatBody::Rendezvous(rv),
                };
                (format_rendezvous_card(&rv, &mut preview), 1usize)
            }
        };
        if n_frags == 0 {
            return false;
        }
        self.tx_total = n_frags as u8;
        self.tx_seq = seq;
        self.tx_need_mask = if n_frags >= 8 { 0xFF } else { (1u8 << n_frags) - 1 };
        self.tx_ack_mask = 0;
        self.tx_sent_mask = 0;
        self.tx_inflight_idx = None;
        self.tx_retries = 0;
        self.tx_ack_deadline_slot = 0;
        let idx = self.push_entry(true, seq, ChatDelivery::Queued, &preview[..plen]);
        self.tx_entry_idx = Some(idx);
        true
    }

    /// Respond to an incoming `pending_offer` with `Accept`, `Counter` (+30 min / next preset), or `Decline`.
    pub fn respond_offer(&mut self, action: RendezvousAction, slot: u32) -> bool {
        let Some(offer) = self.pending_offer.take() else { return false };
        let reply = match offer {
            RendezvousCard::Schedule { days_ahead, utc_min, dial_preset, profile, subch, offer_ref, .. } => {
                let (new_days, new_min) = if action == RendezvousAction::Counter {
                    let m = utc_min + 30;
                    if m >= 1440 { (days_ahead.saturating_add(1), m - 1440) } else { (days_ahead, m) }
                } else {
                    (days_ahead, utc_min)
                };
                RendezvousCard::Schedule { action, days_ahead: new_days, utc_min: new_min, dial_preset, profile, subch, offer_ref }
            }
            RendezvousCard::MoveFreq { band_id, dial_offset_10hz, profile, turn_mod64, subch, .. } => {
                let new_turn = if action == RendezvousAction::Counter {
                    (turn_mod64 + 6) & 0x3F
                } else {
                    turn_mod64
                };
                RendezvousCard::MoveFreq { action, band_id, dial_offset_10hz, profile, turn_mod64: new_turn, subch }
            }
        };
        if action == RendezvousAction::Accept {
            self.commit_rendezvous(reply, slot);
        }
        self.draft = Some(ChatDraft::Rendezvous(reply));
        self.queue_draft()
    }

    fn commit_rendezvous(&mut self, rv: RendezvousCard, slot: u32) {
        match rv {
            RendezvousCard::Schedule { days_ahead, utc_min, dial_preset, profile, subch, .. } => {
                self.saved_reminder = Some(SavedReminder {
                    peer: self.peer,
                    utc_day: self.session_utc_day.saturating_add(days_ahead as u32),
                    utc_min,
                    dial_preset,
                    profile,
                    subch,
                });
            }
            RendezvousCard::MoveFreq { band_id, dial_offset_10hz, turn_mod64, .. } => {
                if let Some(base) = move_band_ref_hz10(band_id) {
                    let cur_mod = (slot & 0x3F) as u8;
                    let delta = turn_mod64.wrapping_sub(cur_mod) & 0x3F;
                    let target_slot = slot.saturating_add(delta.max(2) as u32);
                    self.active_move = Some(ActiveMove {
                        peer: self.peer,
                        dest_dial_hz10: base.saturating_add(dial_offset_10hz),
                        source_dial_hz10: self.source_dial_hz10,
                        target_slot,
                        retuned: false,
                        peer_heard: false,
                        expired: false,
                    });
                }
            }
        }
    }

    /// Operator confirms retune for an active Move offer; returns destination USB dial (10 Hz units).
    pub fn confirm_move_retune(&mut self) -> Option<u32> {
        let mv = self.active_move.as_mut()?;
        mv.retuned = true;
        Some(mv.dest_dial_hz10)
    }

    /// Return to source window after a failed/expired move; returns source USB dial (10 Hz units).
    pub fn return_from_move(&mut self) -> Option<u32> {
        let mv = self.active_move.take()?;
        Some(mv.source_dial_hz10)
    }

    /// Called on each 10 s UTC slot boundary; returns a `Frame` to transmit on our parity slot if queued.
    pub fn on_slot_start(&mut self, slot: u32, utc_ok: bool) -> Option<Frame> {
        // Standalone distress alert/position takes strict TX priority over optional chat traffic.
        if let Some(df) = self.distress.on_slot_start(slot, utc_ok) {
            return Some(df);
        }
        if let Some(mv) = self.active_move.as_mut() {
            if !mv.peer_heard && slot >= mv.target_slot.saturating_add(6) {
                mv.expired = true;
            }
        }
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
        if self.phase != ChatPhase::Active || self.tx_need_mask == 0 {
            return None;
        }
        // Check if all unacked fragments were sent in this round and we are waiting for ACK.
        let mut to_send = self.tx_need_mask & !self.tx_ack_mask & !self.tx_sent_mask;
        if to_send == 0 {
            if self.tx_ack_deadline_slot != 0 && slot >= self.tx_ack_deadline_slot {
                if self.tx_retries < CHAT_MAX_RETRIES {
                    self.tx_retries += 1;
                    self.tx_sent_mask = self.tx_ack_mask;
                    self.tx_ack_deadline_slot = 0;
                    to_send = self.tx_need_mask & !self.tx_ack_mask;
                } else {
                    if let Some(idx) = self.tx_entry_idx.take() {
                        if (idx as usize) < self.entry_count as usize {
                            self.entries[idx as usize].status = ChatDelivery::Incomplete;
                        }
                    }
                    self.tx_need_mask = 0;
                    self.tx_total = 0;
                    return None;
                }
            } else {
                return None;
            }
        }
        if (slot & 1) as u8 != self.my_parity {
            return None;
        }
        let frag_idx = to_send.trailing_zeros() as u8;
        if frag_idx >= self.tx_total {
            return None;
        }
        let pkt = self.tx_queue[frag_idx as usize];
        self.tx_inflight_idx = Some(frag_idx);
        self.tx_ack_deadline_slot = slot.saturating_add(4);
        let done_cnt = (self.tx_sent_mask | self.tx_ack_mask | (1u8 << frag_idx)).count_ones() as u8;
        if let Some(idx) = self.tx_entry_idx {
            if (idx as usize) < self.entry_count as usize {
                self.entries[idx as usize].status = ChatDelivery::Sending { sent: done_cnt, total: self.tx_total };
            }
        }
        Some(encode_chat_frame(pkt))
    }

    /// Called when a chat TX burst finishes (`ok = true`) or aborts (`ok = false`).
    pub fn on_tx_done(&mut self, ok: bool) {
        let Some(frag_idx) = self.tx_inflight_idx.take() else {
            return;
        };
        if ok {
            self.tx_sent_mask |= 1u8 << frag_idx;
            let remaining = self.tx_need_mask & !self.tx_ack_mask & !self.tx_sent_mask;
            let sent_cnt = (self.tx_sent_mask | self.tx_ack_mask).count_ones() as u8;
            if let Some(idx) = self.tx_entry_idx {
                if (idx as usize) < self.entry_count as usize {
                    self.entries[idx as usize].status = if remaining == 0 {
                        ChatDelivery::AwaitingAck
                    } else {
                        ChatDelivery::Sending { sent: sent_cnt, total: self.tx_total }
                    };
                }
            }
        }
    }

    /// Process a received `Frame::ExtFrag`; returns `true` if it was a RAD950 Chat or Distress packet.
    pub fn on_rx_frame(&mut self, frame: Frame, slot: u32) -> bool {
        if self.distress.on_rx_frame(frame, slot, self.source_dial_hz10) {
            return true;
        }
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
            ChatBody::Ctrl(ChatCtrl::Ack { ack_seq, frag_mask }) if pkt.session_ref == self.session_ref => {
                if self.tx_need_mask != 0 && ack_seq == self.tx_seq {
                    self.tx_ack_mask |= frag_mask;
                    if (self.tx_ack_mask & self.tx_need_mask) == self.tx_need_mask {
                        self.tx_need_mask = 0;
                        self.tx_total = 0;
                        self.tx_ack_deadline_slot = 0;
                        if let Some(idx) = self.tx_entry_idx.take() {
                            if (idx as usize) < self.entry_count as usize {
                                self.entries[idx as usize].status = ChatDelivery::Delivered;
                            }
                        }
                    } else {
                        // Selective retransmission: resend ONLY the fragments missing from `frag_mask`.
                        self.tx_sent_mask = self.tx_ack_mask;
                        self.tx_ack_deadline_slot = 0;
                    }
                } else {
                    for i in 0..self.entry_count as usize {
                        let e = &mut self.entries[i];
                        if e.from_me && e.seq == ack_seq {
                            e.status = ChatDelivery::Delivered;
                        }
                    }
                }
            }
            ChatBody::Station(sc) if self.phase == ChatPhase::Active && pkt.session_ref == self.session_ref => {
                let mut buf = [0u8; 64];
                let n = format_station_card(&sc, &mut buf);
                self.deliver_peer_once(pkt.seq, None, &buf[..n], 1);
            }
            ChatBody::Weather(wc) if self.phase == ChatPhase::Active && pkt.session_ref == self.session_ref => {
                let mut buf = [0u8; 64];
                let n = format_weather_card(&wc, &mut buf);
                self.deliver_peer_once(pkt.seq, None, &buf[..n], 1);
            }
            ChatBody::About(ac) if self.phase == ChatPhase::Active && pkt.session_ref == self.session_ref => {
                let mut buf = [0u8; 64];
                let n = format_about_card(&ac, &mut buf);
                self.deliver_peer_once(pkt.seq, None, &buf[..n], 1);
            }
            ChatBody::Rendezvous(rv) if self.phase == ChatPhase::Active && pkt.session_ref == self.session_ref => {
                let mut buf = [0u8; 64];
                let n = format_rendezvous_card(&rv, &mut buf);
                self.deliver_peer_once(pkt.seq, None, &buf[..n], 1);
                let action = match rv {
                    RendezvousCard::Schedule { action, .. } | RendezvousCard::MoveFreq { action, .. } => action,
                };
                match action {
                    RendezvousAction::Offer | RendezvousAction::Counter => {
                        self.pending_offer = Some(rv);
                    }
                    RendezvousAction::Accept => {
                        self.pending_offer = None;
                        self.commit_rendezvous(rv, slot);
                    }
                    RendezvousAction::Decline => {
                        self.pending_offer = None;
                    }
                }
            }
            ChatBody::Question { topic, .. } if self.phase == ChatPhase::Active && pkt.session_ref == self.session_ref => {
                let s = QUESTIONS.get(topic as usize).copied().unwrap_or("Question?").as_bytes();
                self.deliver_peer_once(pkt.seq, Some(topic), s, 1);
            }
            ChatBody::Phrase(id) if self.phase == ChatPhase::Active && pkt.session_ref == self.session_ref => {
                let s = PHRASES.get(id as usize).copied().unwrap_or("73!").as_bytes();
                self.deliver_peer_once(pkt.seq, None, s, 1);
            }
            ChatBody::LiteralFrag { frag_idx, is_final, char_cnt, chars, .. }
                if self.phase == ChatPhase::Active && pkt.session_ref == self.session_ref =>
            {
                if let Some((len, mask)) = self.reasm.push(pkt.seq, frag_idx, is_final, char_cnt, chars) {
                    let mut copy = [0u8; CHAT_LITERAL_MAX];
                    let n = len.min(CHAT_LITERAL_MAX);
                    copy[..n].copy_from_slice(&self.reasm.bytes(n)[..n]);
                    self.deliver_peer_once(pkt.seq, None, &copy[..n], mask);
                } else if is_final {
                    // Final fragment arrived with earlier fragments missing: return partial ACK bitmap!
                    self.pending_ctrl = Some(ChatCtrl::Ack {
                        ack_seq: pkt.seq,
                        frag_mask: self.reasm.received_mask(),
                    });
                }
            }
            _ => {}
        }
        true
    }

    fn deliver_peer_once(&mut self, seq: u8, topic: Option<u8>, text: &[u8], frag_mask: u8) {
        let bit = 1u8 << (seq & 7);
        if (self.rx_delivered_seq_mask & bit) == 0 {
            self.rx_delivered_seq_mask |= bit;
            // Clear opposite half of the 8-seq window so sequences can wrap cleanly later.
            self.rx_delivered_seq_mask &= !(1u8 << ((seq + 4) & 7));
            self.last_peer_topic = topic;
            self.push_entry(false, seq, ChatDelivery::None, text);
        }
        self.pending_ctrl = Some(ChatCtrl::Ack { ack_seq: seq, frag_mask });
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
        self.tx_seq = 0;
        self.tx_need_mask = 0;
        self.tx_ack_mask = 0;
        self.tx_sent_mask = 0;
        self.tx_inflight_idx = None;
        self.tx_retries = 0;
        self.tx_ack_deadline_slot = 0;
        self.tx_entry_idx = None;
        self.pending_ctrl = None;
        self.last_peer_topic = None;
        self.next_seq = 0;
        self.rx_delivered_seq_mask = 0;
        self.pending_offer = None;
        self.active_move = None;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selective_retransmission_and_duplicate_suppression() {
        let mut a = ChatState::new();
        let mut b = ChatState::new();
        let ca = Callsign::encode(b"NQ6N").unwrap();
        let cb = Callsign::encode(b"K1ABC").unwrap();

        // A invites B at slot 10; B receives and accepts.
        a.invite(ca, cb, 10);
        let inv = a.on_slot_start(11, true).expect("invite frame");
        assert!(b.on_rx_frame(inv, 11));
        assert_eq!(b.phase, ChatPhase::IncomingInvite);
        b.accept_invite();
        let acc = b.on_slot_start(12, true).expect("accept frame");
        assert!(a.on_rx_frame(acc, 12));
        assert_eq!(a.phase, ChatPhase::Active);
        assert_eq!(b.phase, ChatPhase::Active);

        // A queues 6-burst literal message; drop fragment 2 on the air!
        a.set_literal_draft(b"HEY MATT HOW HAVE YOU BEEN?");
        assert!(a.queue_draft());
        let mut slot = 13u32;
        for f in 0..6u8 {
            let frame = a.on_slot_start(slot, true).expect("frag tx");
            a.on_tx_done(true);
            if f != 2 {
                assert!(b.on_rx_frame(frame, slot));
            }
            slot += 2;
        }
        // B received final frag 5 with frag 2 missing -> emits partial ACK (mask = 0x3B).
        let partial_ack = b.on_slot_start(slot - 1, true).expect("partial ack");
        assert!(a.on_rx_frame(partial_ack, slot - 1));
        assert_eq!(b.entry_count, 0);

        // A selectively retransmits ONLY fragment 2!
        let resend = a.on_slot_start(slot, true).expect("selective resend");
        a.on_tx_done(true);
        let pkt = decode_chat_frame(resend).unwrap();
        assert!(matches!(pkt.body, ChatBody::LiteralFrag { frag_idx: 2, .. }));
        assert!(b.on_rx_frame(resend, slot));
        assert_eq!(b.entry_count, 1);
        assert_eq!(b.entries[0].bytes(), b"HEY MATT HOW HAVE YOU BEEN?");

        // Full ACK delivers A's entry cleanly.
        let full_ack = b.on_slot_start(slot + 1, true).expect("full ack");
        assert!(a.on_rx_frame(full_ack, slot + 1));
        assert_eq!(a.entries[0].status, ChatDelivery::Delivered);

        // Duplicate suppression: re-delivering fragment 2 must NOT add a second entry on B.
        assert!(b.on_rx_frame(resend, slot + 2));
        assert_eq!(b.entry_count, 1);
    }

    #[test]
    fn weather_and_about_formatting() {
        let mut buf = [0u8; 64];
        let wn = format_weather_card(&WeatherCard::DEFAULT, &mut buf);
        assert_eq!(&buf[..wn], b"Light rain, 12C, wind 15kph NW");
        let an = format_about_card(&AboutCard::DEFAULT, &mut buf);
        assert_eq!(&buf[..an], b"First licensed in 1994");
    }

    #[test]
    fn rendezvous_schedule_and_move_lifecycle() {
        let mut a = ChatState::new();
        let mut b = ChatState::new();
        let ca = Callsign::encode(b"NQ6N").unwrap();
        let cb = Callsign::encode(b"K1ABC").unwrap();
        a.invite(ca, cb, 10);
        b.on_rx_frame(a.on_slot_start(11, true).unwrap(), 11);
        b.accept_invite();
        a.on_rx_frame(b.on_slot_start(12, true).unwrap(), 12);

        // Schedule QSO offer -> B accepts -> both save local reminder
        a.draft = Some(ChatDraft::Rendezvous(RendezvousCard::Schedule {
            action: RendezvousAction::Offer,
            days_ahead: 1,
            utc_min: 18 * 60 + 30,
            dial_preset: 1,
            profile: 0,
            subch: 2,
            offer_ref: 0,
        }));
        assert!(a.queue_draft());
        let f_offer = a.on_slot_start(13, true).unwrap();
        a.on_tx_done(true);
        assert!(b.on_rx_frame(f_offer, 13));
        assert!(b.pending_offer.is_some());
        assert!(b.respond_offer(RendezvousAction::Accept, 14));
        assert!(b.saved_reminder.is_some());
        let _ack = b.on_slot_start(14, true).unwrap(); // ACK first
        let f_acc = b.on_slot_start(16, true).unwrap(); // Accept card
        b.on_tx_done(true);
        assert!(a.on_rx_frame(f_acc, 16));
        assert_eq!(a.saved_reminder.unwrap().utc_min, 18 * 60 + 30);
    }

    #[test]
    fn standalone_distress_alert_and_position_linking() {
        let mut tx = ChatState::new();
        let mut rx = ChatState::new();
        let sender = Callsign::encode(b"KF8QE").unwrap();
        let grid = ferrite_core::sol950::usol950::frame::encode_grid4(b"CM87").unwrap();
        tx.distress.category = DistressCategory::Medical;
        tx.distress.loc_src = DistressLocSource::FreshGps;
        tx.distress.lat_e4 = 377_749;
        tx.distress.lon_e4 = -1_224_194;
        tx.distress.arm_tx(sender, Some(grid), tx.source_dial_hz10);

        // Burst 1: Alert at slot 50
        let alert_f = tx.on_slot_start(50, true).expect("alert burst");
        assert!(rx.on_rx_frame(alert_f, 50));
        assert_eq!(rx.distress.rx.unwrap().status, DistressRxStatus::PositionPending);

        // Slot 51: position burst waits until slot 52
        assert!(tx.on_slot_start(51, true).is_none());

        // Burst 2: Position at slot 52 links cleanly!
        let pos_f = tx.on_slot_start(52, true).expect("position burst");
        assert!(rx.on_rx_frame(pos_f, 52));
        let inc = rx.distress.rx.unwrap();
        assert_eq!(inc.status, DistressRxStatus::PositionLinked);
        assert_eq!(inc.lat_e4, Some(377_749));
        assert_eq!(inc.lon_e4, Some(-1_224_194));
    }
}
