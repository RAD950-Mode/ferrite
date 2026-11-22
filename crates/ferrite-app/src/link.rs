//! The link protocol's radio side (manual chapter 29, LINK-1 – LINK-11, Appendix "Link protocol v1"): one
//! session per port, next to the App like the terminal ([`crate::term`]), so it reads and writes through the
//! same paths (channel writes queued for the Storage task, zones and presets through [`crate::data::Op`]).
//! The wire format is `ferrite_core::link`.
//!
//! The caller feeds the port's bytes while [`Session::ready`], and polls: a whole frame is handled when the
//! answer has room on the port and the store has taken the last write, so nothing is dropped and the PC waits
//! for each answer (every frame is acknowledged). The cable is always trusted (LINK-7); on Bluetooth, while the
//! radio's PIN lock is on, nothing but `HELLO` and `AUTH` is answered until `AUTH` gives the PIN (LINK-30).

use crate::chan::{Channel, ChannelStore, MAX_CHANNELS, NO_ZONE};
use crate::data::{Op, RangePreset, ZoneCfg};
use crate::persist::{Persist, Sink as PSink, Source};
use crate::screens::link::LinkAnswer;
use crate::screens::pin::RemotePin;
use crate::settings::Settings;
use crate::slot::{SlotCfg, STEPS};
use crate::term::Sink;
use crate::App;
use ferrite_core::freq::mhz;
use ferrite_core::link::{self as wire, msg, what, Ptt, Status, MAX_PAYLOAD, MAX_WIRE, PROTOCOL};
use crate::remote::RemoteEnd;
use crate::term::cmd::{self, Done, DumpPos, Line, List};
use ferrite_core::msg::LiveAll;
use crate::term::debug::{Job, Step};
use ferrite_core::msg::{DebugAns, DebugReq, Device};
use ferrite_core::VERSION;
use ferrite_ui::text::Buf;

/// The longest record a `WRITE` assembles: the settings alone (the store's 1 KB record adds the slots; a test checks
/// their real length against it).
pub const RECORD_MAX: usize = 512;
/// A port stays in link mode this long after its last frame (TERM-2), then the first bytes decide again.
pub const ACTIVE_MS: u32 = 10_000;
/// A write session with no frame for this long ends by itself (its progress screen closes).
pub const SESSION_MS: u32 = 5_000;

/// One port's link session.
pub struct Session {
    /// The frame being received (COBS bytes, no delimiter).
    rx: [u8; MAX_WIRE],
    rn: u16,
    /// A frame longer than any valid one: dropped at its delimiter.
    overflow: bool,
    /// A whole frame waits to be handled.
    have: bool,
    /// The cable is trusted (LINK-7); a Bluetooth session gave the radio's PIN (LINK-30).
    trusted: bool,
    /// Bluetooth at its unlocked rate (LINK-25.1): the events' rate cap is [`ferrite_core::link::event::RATE_BT_FAST`].
    pub(crate) fast: bool,
    pin_ok: bool,
    /// When the last valid frame arrived, and whether one has.
    last: u32,
    seen: bool,
    /// The record a `WRITE` is assembling: what, index, total length, bytes so far.
    asm: [u8; RECORD_MAX],
    asm_key: (u8, u16, u16),
    asm_len: u16,
    /// A write session (`BEGIN` .. `END`, LINK-9): records announced and done, cancelled by BACK.
    session: bool,
    total: u16,
    done: u16,
    cancelled: bool,
    /// The fastest rate this port's UART reaches within 2 % (LINK-11), and a rate agreed, for the caller.
    max_baud: u32,
    baud: Option<u32>,
    /// `REBOOT` was accepted: the caller restarts once the answer has gone (LINK-24).
    reboot: bool,
    /// `STOCK_RESTORE`'s `START` was accepted (BACKUP-7, decision 261): its seq, for the caller, which answers it
    /// once the radio has stopped and then serves the restore itself.
    restore: Option<u8>,
    hellos: u16,
    /// Who this session is for remote PTT and keys: [`crate::remote::BY_CABLE`] (trusted) or [`crate::remote::BY_BT`].
    by: u8,
    /// The session started over (a new connection, LINK-29), as `by` was: a remote transmission it keyed and keys it
    /// held end at the next poll (LINK-20).
    closed: Option<u8>,
    /// The other port's session is mid-write (LINK-28): this one's writes answer `busy` until it is done.
    other_writing: bool,
    /// A terminal command's output still to send (LINK-34), and the time for its debug requests.
    run: Run,
    now: u32,
    /// A `WRITE_MANY` being applied (LINK-41).
    many: Option<Many>,
    /// A `STREAM` under way (LINK-50).
    stream: Option<Stream>,
    /// LINK-51: the shared compression block is free for this session (set by the ports before each poll), and
    /// whether its last `HELLO` asked for it and got it.
    pub lz_free: bool,
    pub lz: bool,
    /// Events pushed to this session (LINK-46).
    sub: sub::Sub,
    /// LINK-13.4: the receive audio this session asked for (the cable only; never on by itself, LINK-13.5): 0 none,
    /// 1 the µ-law stream (decision 330), 2 or 3 the bench's raw stream (LINK-13.9); and the blocks' counter.
    audio: u8,
    audio_seq: u16,
}

/// A `WRITE_MANY` under way (LINK-41): its records wait in `asm` (what's after the `what` byte), applied one a
/// pass, as each may queue the one channel write the App holds; its answer goes when the last is applied or one
/// is refused. Its frame's seq, `what`, the records' length, the next one's offset, and how many are done.
/// A `STREAM` under way (LINK-50): the inner request's type and its payload for the next frame (where to go on
/// from), and the `STREAM` request's seq.
#[derive(Clone, Copy, Debug)]
struct Stream {
    ty: u8,
    /// The next frame's seq: the request's, then one more each frame, so a frame lost on the way shows as a gap.
    seq: u8,
    p: [u8; 5],
}

#[derive(Clone, Copy, Debug)]
struct Many {
    seq: u8,
    what: u8,
    len: u8,
    at: u8,
    done: u8,
}

/// What a terminal command run over the link still has to say (LINK-34): made a line at a time, as the terminal's
/// console makes them, so nothing is buffered whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Run {
    Off,
    List(List, u16),
    Dump(DumpPos),
    Debug(Job),
}

impl Session {
    pub const fn new(trusted: bool) -> Self {
        Session {
            rx: [0; MAX_WIRE],
            rn: 0,
            overflow: false,
            have: false,
            trusted,
            fast: false,
            pin_ok: false,
            last: 0,
            seen: false,
            asm: [0; RECORD_MAX],
            asm_key: (0, 0, 0),
            asm_len: 0,
            session: false,
            total: 0,
            done: 0,
            cancelled: false,
            max_baud: 115_200,
            baud: None,
            reboot: false,
            restore: None,
            hellos: 0,
            by: if trusted { crate::remote::BY_CABLE } else { crate::remote::BY_BT },
            closed: None,
            other_writing: false,
            run: Run::Off,
            now: 0,
            many: None,
            stream: None,
            lz_free: false,
            lz: false,
            sub: sub::Sub::new(),
            audio: 0,
            audio_seq: 0,
        }
    }

    /// The fastest cable rate the caller's UART can run (LINK-11): `BAUD` above it is refused (0: every rate, as on
    /// Bluetooth, LINK-15).
    pub const fn set_max_baud(&mut self, b: u32) {
        self.max_baud = b;
    }

    /// The session starts over for a new connection on `trusted`'s port (the cable, or Bluetooth: [`crate::ports`],
    /// LINK-29): a frame half received, a PIN given and a running command go; so does a remote transmission it keyed
    /// (LINK-20). `max_baud` is the port's (Bluetooth: 0, its `BAUD` takes the module's two rates: LINK-15.1).
    pub fn set_port(&mut self, trusted: bool, max_baud: u32) {
        (self.rn, self.overflow, self.have) = (0, false, false);
        (self.trusted, self.pin_ok) = (trusted, false);
        (self.seen, self.baud, self.max_baud) = (false, None, max_baud);
        self.closed = Some(self.by);
        self.by = if trusted { crate::remote::BY_CABLE } else { crate::remote::BY_BT };
        self.run = Run::Off;
        self.many = None;
        self.lz = false; // LINK-51: the shared block goes back
        self.audio = 0; // LINK-13.5: listening ends with the connection
        self.unsubscribe(); // LINK-46: events end with the connection
    }

    /// A write is under way: a write session (`BEGIN` .. `END`) or a record half assembled (LINK-28).
    pub fn writing(&self) -> bool {
        self.session || self.asm_len > 0 || self.many.is_some()
    }

    /// LINK-28: the other port's session is mid-write (set by [`crate::ports::Ports`] before each poll).
    pub fn set_other_writing(&mut self, w: bool) {
        self.other_writing = w;
    }

    /// This session may do anything a session may (LINK-7, LINK-30): the cable, Bluetooth with no PIN lock on, or
    /// Bluetooth after `AUTH` gave the PIN.
    pub fn authorised(&self, app: &App) -> bool {
        self.trusted || self.pin_ok || !app.pin_required()
    }

    /// Takes another byte now: false while a whole frame waits for its answer.
    pub fn ready(&self) -> bool {
        !self.have
    }

    /// A byte from the port. A 0x00 ends a frame (an empty one is only a sync), and claims the port for the link
    /// (TERM-2) as a whole frame does.
    pub fn feed(&mut self, b: u8, now: u32) {
        if b == 0 {
            (self.seen, self.last) = (true, now);
            if self.rn > 0 && !self.overflow {
                self.have = true;
            } else {
                self.rn = 0;
            }
            self.overflow = false;
            return;
        }
        if (self.rn as usize) < self.rx.len() {
            self.rx[self.rn as usize] = b;
            self.rn += 1;
        } else {
            (self.overflow, self.rn) = (true, 0);
        }
    }

    /// A frame or a sync arrived lately: the port belongs to the link (TERM-2).
    pub fn active(&self, now: u32) -> bool {
        self.seen && now.wrapping_sub(self.last) < ACTIVE_MS
    }

    /// LINK-13.4: this session asked for receive audio.
    pub fn listening(&self) -> bool {
        self.audio != 0
    }

    /// LINK-13.4, LINK-13.5: what this session listens to (the `AUDIO` mode, 0 none): it asked, and it is still
    /// active (a quiet session stops it).
    pub fn audio_on(&mut self, now: u32) -> u8 {
        if self.audio != 0 && !self.active(now) {
            self.audio = 0;
        }
        self.audio
    }

    /// LINK-13.4: one block of receive audio (or a keep-alive, `None`) pushed to this session (the simulator's way;
    /// the radio's DSP task frames its own the same way, decision 290), plain: never
    /// through the compression block (LINK-51), so it goes straight to `out`. Dropped whole when there's no room
    /// (the cable's ring holds several; a lost block costs only itself).
    pub fn audio_out(&mut self, slot: u8, flags: u8, blk: Option<&ferrite_core::rxaudio::Block>, out: &mut dyn Sink) -> bool {
        let mut w = [0u8; MAX_WIRE];
        if let Some(k) = ferrite_core::rxaudio::frame(self.audio_seq, slot, flags, blk, &mut w) {
            if out.room() >= k {
                out.put(&w[..k]);
                self.audio_seq = self.audio_seq.wrapping_add(1);
                return true;
            }
        }
        false
    }

    /// The write session's progress, for the Device's `link.write_pct` (LINK-9).
    pub fn progress(&self) -> Option<u8> {
        (self.session && self.total > 0).then(|| (self.done.min(self.total) as u32 * 100 / self.total as u32) as u8)
    }

    /// A rate agreed by `BAUD`, once: switch after this answer has gone out, back to 115200 unless a `HELLO`
    /// arrives at the new rate within 1 s ([`Session::hellos`] counts them).
    pub fn take_baud(&mut self) -> Option<u32> {
        self.baud.take()
    }

    /// `REBOOT` was accepted, once: save, quiet the radio and reset after this answer has gone (LINK-24).
    pub fn take_reboot(&mut self) -> bool {
        core::mem::take(&mut self.reboot)
    }

    /// `STOCK_RESTORE`'s `START` accepted, once, with its seq: the caller stops the radio, answers it and serves the
    /// restore from then on ([`ferrite_core::restore`]); this session sent no answer.
    pub fn take_restore(&mut self) -> Option<u8> {
        self.restore.take()
    }

    /// `HELLO`s handled so far.
    pub fn hellos(&self) -> u16 {
        self.hellos
    }

    /// Handles the owner's answers, ends a quiet session, and answers a waiting frame when there is room.
    pub fn poll<S: ChannelStore>(&mut self, app: &mut App, store: &S, dev: &Device, live: &LiveAll, now: u32, out: &mut dyn Sink) {
        self.now = now;
        // BACK on the progress screen (LINK-9) is ours only while a write session runs.
        if self.session && app.take_link_answer() == Some(LinkAnswer::Cancelled) {
            self.cancelled = true;
        }
        if self.session && now.wrapping_sub(self.last) >= SESSION_MS {
            self.end(app);
        }
        // LINK-20: remote PTT ends with the session that keyed it, and every key it held (LINK-19).
        if let Some(by) = self.closed.take() {
            app.remote_session_closed(by, now, store);
        }
        // `KEY` never waits for the store, nor for room for the largest answer: its answers are a few bytes, and on
        // the cable they wait in the send ring while keyed (TX-43), so keepalives must still be handled then.
        if self.have {
            self.stream = None; // LINK-50: any new request ends a stream
        } else if self.stream.is_some() {
            // as many frames as the port has room for (the cable's ring holds four), so a pass isn't a frame
            while self.stream.is_some() && out.room() >= MAX_WIRE && !store.data().busy() {
                self.last = now;
                if self.stream.is_some_and(|s| s.ty != msg::UI) {
                    app.link_transfer(now); // DISP-30
                }
                self.stream_frame(app, store, dev, live, out);
            }
            return;
        }
        let key = self.many.is_none() && self.rn >= 2 && self.rx[0] >= 2 && self.rx[1] == msg::KEY;
        // LINK-41.1: a `WRITE_MANY` waits only for room in the App's and the store's queues, so a frame's records
        // reach the store together (and same-sector ones merge, decision 227); everything else waits for the store
        // to be idle, so what it reads includes every write before it.
        let many = self.many.is_some() || self.rn >= 2 && self.rx[0] >= 2 && self.rx[1] == msg::WRITE_MANY;
        let waits = if many { !write_room(app, store, self.many.map_or(what::CHANNEL, |m| m.what)) } else { app.write_pending() || store.data().busy() };
        if !self.have || !key && (out.room() < MAX_WIRE || waits) {
            return;
        }
        if self.many.is_some() {
            self.many_poll(app, store, now, out); // LINK-41: the frame was taken already; `have` holds the port
            return;
        }
        let mut ans = [0u8; MAX_PAYLOAD];
        let n = self.rn as usize;
        let (ty, seq, len) = match wire::parse(&mut self.rx[..n]) {
            Err(e) => {
                ans[0] = Status::BadFrame as u8;
                (e.ty, e.seq, 1)
            }
            Ok(f) => {
                let (ty, seq) = (f.ty, f.seq);
                let mut p = [0u8; MAX_PAYLOAD];
                let pl = f.payload.len();
                p[..pl].copy_from_slice(f.payload);
                (self.last, self.seen) = (now, true);
                if transfer(ty, &p[..pl]) && self.authorised(app) {
                    app.link_transfer(now); // DISP-30: the LED flashes
                }
                if ty == msg::STREAM {
                    // LINK-50: its first frame now; the rest as the port has room (the top of `poll`)
                    let inner = p[..pl].first().copied().unwrap_or(0);
                    // LINK-30: on Bluetooth with the PIN lock on, no stream before AUTH either
                    let refused = if !self.authorised(app) {
                        Some(Status::NotAuthorised)
                    } else if !matches!(inner, msg::READ_MANY | msg::HEARD | msg::MSGS | msg::LOG_READ | msg::UI) || pl > 6 {
                        Some(Status::BadArg)
                    } else {
                        None
                    };
                    if let Some(r) = refused {
                        ans[0] = r as u8;
                        self.send(ty, seq, &ans[..1], out);
                        return;
                    }
                    let mut sp = [0u8; 5];
                    sp[..pl - 1].copy_from_slice(&p[1..pl]);
                    if inner == msg::LOG_READ && pl < 5 {
                        sp[..4].copy_from_slice(&u32::MAX.to_le_bytes()); // from the newest
                    }
                    (self.have, self.rn) = (false, 0);
                    self.stream = Some(Stream { ty: inner, seq, p: sp });
                    self.stream_frame(app, store, dev, live, out);
                    return;
                }
                let len = self.handle(app, store, dev, live, now, ty, &p[..pl], &mut ans);
                if ty == msg::STOCK_RESTORE && ans[0] == Status::Ok as u8 {
                    self.restore = Some(seq); // the caller answers (BACKUP-7)
                    (self.have, self.rn) = (false, 0);
                    return;
                }
                match &mut self.many {
                    Some(m) => {
                        m.seq = seq;
                        match self.many_step(app, store, &mut ans) {
                            Some(len) => (ty, seq, len),
                            None => {
                                self.many_poll(app, store, now, out); // more records: as many as have room (LINK-41.1)
                                return;
                            }
                        }
                    }
                    None => (ty, seq, len),
                }
            }
        };
        self.send(ty, seq, &ans[..len], out);
    }

    /// The answer out, and the port takes the next frame.
    fn send(&mut self, ty: u8, seq: u8, ans: &[u8], out: &mut dyn Sink) {
        Self::frame_out(ty, seq, ans, out);
        (self.have, self.rn) = (false, 0);
    }

    /// A frame out, leaving what the port is receiving alone (a stream's frames, LINK-50).
    fn frame_out(ty: u8, seq: u8, ans: &[u8], out: &mut dyn Sink) {
        let mut w = [0u8; MAX_WIRE];
        if let Some(k) = wire::build(ty | wire::ANSWER, seq, ans, &mut w) {
            // Only a `KEY` answer can find too little room (the send ring full of them while keyed, TX-43): it is
            // dropped whole, never cut, and the request has still been acted on.
            if out.room() >= k {
                out.put(&w[..k]);
            }
        }
    }

    /// LINK-50: the stream's next answer, framed `STREAM_MORE` while more follow, `STREAM` for the last.
    fn stream_frame<S: ChannelStore>(&mut self, app: &mut App, st: &S, dev: &Device, live: &LiveAll, out: &mut dyn Sink) {
        let Some(mut s) = self.stream.take() else { return };
        let mut ans = [0u8; MAX_PAYLOAD];
        let mut o = Out { a: &mut ans, n: 1 };
        let (status, more) = match s.ty {
            msg::READ_MANY => {
                let st_ = read_many(st, &s.p, &mut o);
                let (first, end) = (le16(&s.p[1..]) as u32, le16(&s.p[1..]) as u32 + le16(&s.p[3..]) as u32);
                let n = indexes(st, s.p[0]).unwrap_or(0) as u32;
                let next = if st_ == Status::Ok { le16(&o.a[1..]) as u32 } else { first };
                s.p[1..3].copy_from_slice(&(next as u16).to_le_bytes());
                s.p[3..5].copy_from_slice(&(end.saturating_sub(next).min(u16::MAX as u32) as u16).to_le_bytes());
                (st_, st_ == Status::Ok && next > first && next < end.min(n))
            }
            msg::HEARD => {
                let (st_, next) = sub::heard_at(st, &s.p[..3], &mut o); // the index, then LINK-70's age
                if let Some(i) = next {
                    s.p[0] = i as u8;
                }
                (st_, next.is_some())
            }
            msg::MSGS => {
                let (st_, next) = sub::msgs_at(st, &s.p[..2], &mut o);
                if let Some(i) = next {
                    s.p[..2].copy_from_slice(&i.to_le_bytes());
                }
                (st_, next.is_some())
            }
            msg::LOG_READ => {
                let (st_, next) = log_read_at(st, &s.p, &mut o);
                if let Some(b) = next {
                    s.p[..4].copy_from_slice(&b.to_le_bytes());
                }
                (st_, next.is_some())
            }
            _ => {
                // UI: its first frame starts the readback, the rest go on from it (LINK-19's pages)
                if s.p[4] == 0 {
                    s.p[4] = 1;
                    self.run = Run::List([List::Ui, List::UiKeys, List::UiStatus, List::UiTexts][s.p[0].min(3) as usize], 0);
                }
                self.more(app, st, dev, live, &mut o);
                (Status::Ok, o.a[1] == 1)
            }
        };
        o.a[0] = status as u8;
        if status != Status::Ok {
            o.n = 1;
        }
        let n = o.n;
        let more = more && status == Status::Ok;
        Self::frame_out(if more { msg::STREAM_MORE } else { msg::STREAM }, s.seq, &ans[..n], out);
        s.seq = s.seq.wrapping_add(1);
        self.stream = more.then_some(s);
    }

    /// A `WRITE_MANY`'s next record (LINK-41), and its answer once the last is applied or one is refused.
    fn many_poll<S: ChannelStore>(&mut self, app: &mut App, store: &S, now: u32, out: &mut dyn Sink) {
        self.last = now;
        app.link_transfer(now); // DISP-30: a long `WRITE_MANY` keeps the LED flashing
        let seq = self.many.map_or(0, |m| m.seq);
        let mut ans = [0u8; MAX_PAYLOAD];
        while write_room(app, store, self.many.map_or(0, |m| m.what)) {
            if let Some(len) = self.many_step(app, store, &mut ans) {
                self.send(msg::WRITE_MANY, seq, &ans[..len], out);
                return;
            }
        }
    }

    /// One request: its answer's payload into `a` (status first); the length.
    #[allow(clippy::too_many_arguments)]
    fn handle<S: ChannelStore>(&mut self, app: &mut App, st: &S, dev: &Device, live: &LiveAll, now: u32, ty: u8, p: &[u8], a: &mut [u8; MAX_PAYLOAD]) -> usize {
        let mut o = Out { a, n: 1 };
        let s = match ty {
            msg::HELLO => {
                // LINK-20.5: a HELLO is the session starting over: a remote transmission it keyed ends now, as a
                // cut (no keepalive wait, no 400 ms hold), and the keys it holds are let go.
                app.remote_session_closed(self.by, now, st);
                self.unsubscribe(); // LINK-46
                self.hello(app, st, dev, p, &mut o)
            }
            msg::AUTH => self.auth(app, p),
            // LINK-30: on Bluetooth with the PIN lock on, nothing else until AUTH.
            _ if !self.authorised(app) => Status::NotAuthorised,
            msg::INFO => info(app, st, dev, &mut o),
            msg::READ => read(app, st.data().fix(), p, &mut o),
            msg::USED => used(st, p, &mut o),
            msg::READ_MANY => read_many(st, p, &mut o),
            msg::LOG_READ => log_read(st, p, &mut o),
            msg::WRITE | msg::DELETE | msg::BEGIN | msg::WRITE_MANY => match self.may_write(app) {
                Status::Ok => match ty {
                    msg::WRITE => self.write(app, p, &mut o),
                    msg::WRITE_MANY => self.write_many(p),
                    msg::DELETE => self.delete(app, st, p),
                    _ => self.begin(app, p),
                },
                s => s,
            },
            msg::END => {
                self.end(app);
                Status::Ok
            }
            msg::BAUD => match p.get(..4) {
                Some(b) => {
                    let r = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                    // LINK-15.1: on Bluetooth, the module's two rates (the host unlocks the module after this answer)
                    let ok = if self.trusted { wire::RATES.contains(&r) && r <= self.max_baud } else { wire::BT_RATES.contains(&r) };
                    if ok {
                        self.baud = Some(r);
                        Status::Ok
                    } else {
                        Status::Refused
                    }
                }
                None => Status::BadArg,
            },
            msg::DATA_READ => {
                app.update_checking = p.first() == Some(&ferrite_core::link::data::CRC); // UPD-16.2
                data_read(st, p, &mut o)
            }
            msg::CLOCK => clock(app, p, &mut o), // LINK-65
            msg::COOP_RX => coop_rx(app, p, &mut o),
            msg::RAD950_TX if self.by != crate::remote::BY_CABLE => Status::Refused,
            msg::RAD950_TX => rad950_tx_link(app, p),
            // LINK-60: raw writes from the cable only, not while the other port writes
            msg::DATA_WRITE if self.by != crate::remote::BY_CABLE => Status::Refused,
            msg::DATA_WRITE if self.other_writing => Status::Busy,
            msg::DATA_WRITE if p.first() == Some(&ferrite_core::link::data::PART) => match p {
                // UPD-16.2: which part the installer writes now, for the Updating page
                [_, i, n] if (1..=*n).contains(i) => {
                    app.update_part = (*i, *n);
                    Status::Ok
                }
                _ => Status::BadArg,
            },
            msg::DATA_WRITE => {
                app.update_checking = false; // UPD-16.2
                let (s, stamp) = data_write(st, p, &mut o);
                self.reboot |= stamp; // LINK-62: the radio restarts on the stamped layout
                s
            }
            msg::REBOOT if app.radio.keyed.is_some() || live.tx.keyed => Status::Busy, // LINK-24.3: a data key-up too
            msg::REBOOT => {
                self.reboot = true;
                Status::Ok
            }
            // BACKUP-7, LINK-8: the cable only, never while transmitting or while the other port writes; only `START`
            // here (the rest go to the restore the caller runs once this is accepted).
            msg::STOCK_RESTORE if self.by != crate::remote::BY_CABLE => Status::Refused,
            msg::STOCK_RESTORE if app.radio.keyed.is_some() || live.tx.keyed || self.other_writing => Status::Busy,
            msg::STOCK_RESTORE if p != [ferrite_core::restore::START] => Status::BadArg,
            msg::STOCK_RESTORE => Status::Ok,
            msg::KEY => key(app, st, self.by, now, p, &mut o),
            msg::SLOT => slot(app, st, p, &mut o),
            // LINK-67: the radio state record; LINK-68: a slot tuned by command
            msg::STATE if !p.is_empty() => Status::BadArg,
            msg::STATE => {
                o.put(&state::record(app, st, dev, live));
                Status::Ok
            }
            msg::TUNE => tune(app, st, p, &mut o),
            // LINK-69: a beacon now, as the radio's own; transmits, so a PIN lock's Bluetooth needs AUTH first
            // (LINK-30, as every request)
            msg::PHONE_POS => phone_pos(st, p, &mut o), // LINK-72
            msg::BEACON if !p.is_empty() => Status::BadArg,
            msg::BEACON => match crate::screens::opts::send_beacon(app, st) {
                Ok(()) => Status::Ok,
                Err((_, why)) => {
                    o.put(why.as_bytes());
                    Status::Invalid
                }
            },
            msg::CMD => self.command(app, st, dev, live, p, &mut o),
            msg::MORE => {
                self.more(app, st, dev, live, &mut o);
                Status::Ok
            }
            // LINK-19: what the screen shows, as the terminal's `ui` (or `ui keys`) prints it.
            // what=2: the compact one-line status (`ui status`), for a slow link.
            msg::UI => match p.first().copied().unwrap_or(0) {
                w @ (0..=3) => {
                    self.run = Run::List([List::Ui, List::UiKeys, List::UiStatus, List::UiTexts][w as usize], 0);
                    self.more(app, st, dev, live, &mut o);
                    Status::Ok
                }
                _ => Status::BadArg,
            },
            msg::SUBSCRIBE => self.subscribe(st, now, p, &mut o),
            msg::HEARD => sub::heard(st, p, &mut o),
            msg::MSGS => sub::msgs(st, p, &mut o),
            msg::MSG_SEND => sub::msg_send(app, st, dev, p, &mut o),
            msg::MSG_STOP => sub::msg_stop(st, p, &mut o),
            msg::MSG_DELETE => sub::msg_delete(st, p, &mut o),
            // LINK-13.4: receive audio, the cable only (Bluetooth can't carry 4-12 KB/s, research/ble-speed.md)
            msg::AUDIO if self.by != crate::remote::BY_CABLE => Status::Refused,
            msg::AUDIO => match p.first() {
                // decision 330: 1 the 12 kHz µ-law stream; 0 stops
                Some(&on @ (0 | 1)) => {
                    self.audio = on;
                    o.put(&(ferrite_core::rxaudio::RATE_HZ as u16).to_le_bytes());
                    o.put(&(ferrite_core::rxaudio::BLOCK as u16).to_le_bytes());
                    Status::Ok
                }
                // LINK-13.9: the raw stream, a bench build's only (decision 151)
                #[cfg(feature = "bench")]
                Some(&m @ (ferrite_core::rawaudio::MODE_12K | ferrite_core::rawaudio::MODE_16K)) => {
                    self.audio = m;
                    o.put(&(ferrite_core::rawaudio::rate(m).unwrap_or(0) as u16).to_le_bytes());
                    o.put(&(ferrite_core::rawaudio::BLOCK as u16).to_le_bytes());
                    Status::Ok
                }
                #[cfg(not(feature = "bench"))]
                Some(&(ferrite_core::rawaudio::MODE_12K | ferrite_core::rawaudio::MODE_16K)) => Status::Refused,
                _ => Status::BadArg,
            },
            _ => Status::UnknownType,
        };
        o.a[0] = s as u8;
        if s != Status::Ok && s != Status::Invalid {
            o.n = 1; // a refusal carries no partial answer
        }
        o.n
    }

    /// A debug command's next request for the firmware (TERM-21, run over the link: LINK-34), once.
    pub fn debug_req(&mut self) -> Option<DebugReq> {
        match &mut self.run {
            Run::Debug(j) => j.request(self.now),
            _ => None,
        }
    }

    /// The firmware's answer to the last [`Session::debug_req`]; ignored once the command has ended.
    pub fn debug_answer(&mut self, a: DebugAns) {
        if let Run::Debug(j) = &mut self.run {
            j.answer(a);
        }
    }

    /// `CMD` (LINK-34): the line as the terminal's command line runs it (`cmd::run`: the same commands, checks and
    /// refusals), then its first output.
    fn command<S: ChannelStore>(&mut self, app: &mut App, st: &S, dev: &Device, live: &LiveAll, p: &[u8], o: &mut Out) -> Status {
        self.run = Run::Off;
        app.cmd_by = self.by; // XB-20.6: `xb start` over the link logs which port
        let ran = cmd::run(app, st, dev, p, false);
        app.cmd_by = crate::remote::BY_TERM;
        let r = match ran {
            Ok(Done::Nothing) => Run::Off,
            Ok(Done::Said(m)) => {
                o.put(&[0]);
                o.put(m.as_bytes());
                o.put(b"\n");
                return Status::Ok;
            }
            Ok(Done::List(l)) => Run::List(l, 0),
            Ok(Done::Dump) => Run::Dump(DumpPos::default()),
            Ok(Done::Debug(j)) => Run::Debug(j),
            Ok(Done::Load | Done::SatPaste) => {
                o.put(b"load: not over the link; write the records, or load in the terminal");
                return Status::Invalid;
            }
            Err(m) => {
                o.put(m.as_bytes());
                return Status::Invalid;
            }
        };
        self.run = r;
        self.more(app, st, dev, live, o);
        Status::Ok
    }

    /// The running command's next whole lines, as many as fit: the "more" byte, then the text.
    fn more<S: ChannelStore>(&mut self, app: &mut App, st: &S, dev: &Device, live: &LiveAll, o: &mut Out) {
        let flag = o.n;
        o.put(&[0]);
        let mut l = Line::new();
        loop {
            let was = self.run;
            let got = match &mut self.run {
                Run::Off => false,
                Run::List(List::UiTexts, i) => {
                    let g = crate::uistate::line_all(app, st, dev, live, true, *i, &mut l); // LINK-19.5
                    *i += 1;
                    g
                }
                Run::List(list, i) => {
                    let g = cmd::list_line(app, st, dev, live, *list, *i, &mut l);
                    *i += 1;
                    g
                }
                Run::Dump(pos) => cmd::dump_line(app, st, dev, pos, &mut l),
                Run::Debug(j) => match j.line(self.now, &mut l) {
                    Step::Line => true,
                    Step::Wait => {
                        o.a[flag] = 1; // the firmware hasn't answered yet: MORE again
                        return;
                    }
                    Step::Over => false,
                },
            };
            if !got {
                self.run = Run::Off;
                return;
            }
            let room = MAX_PAYLOAD - o.n;
            if l.len() + 1 > room && o.n > flag + 1 {
                self.run = was; // this line goes in the next answer
                o.a[flag] = 1;
                return;
            }
            o.put(&l.as_bytes()[..l.len().min(room - 1)]); // a line longer than any answer is cut
            o.put(b"\n");
        }
    }

    /// `HELLO`: the protocol, the largest payload, and whether this session may go on (0: `AUTH` first).
    fn hello<S: ChannelStore>(&mut self, app: &App, st: &S, dev: &Device, p: &[u8], o: &mut Out) -> Status {
        self.hellos = self.hellos.wrapping_add(1);
        // LINK-51: the first reserved byte's bit 0 asks for compression; granted while the shared block is free
        self.lz = p.get(1).is_some_and(|b| b & 1 != 0) && self.lz_free;
        self.audio = 0; // LINK-13.5: listening ends with the session
        o.put(&[PROTOCOL, MAX_PAYLOAD as u8, self.authorised(app) as u8]);
        o.text(VERSION.as_bytes());
        o.put(&[ferrite_core::link::build::of(dev.tx_built, app.dry_tx(), dev.bench)]); // LINK-35, TX-48
        // LINK-35.2 (decision 385): only what differs by build or session; everything else every radio has
        use ferrite_core::link::caps;
        let lz = if self.lz { caps::LZ } else { 0 };
        let audio = if self.by == crate::remote::BY_CABLE { caps::AUDIO } else { 0 };
        let cmds = if cfg!(feature = "line-cmds") { caps::LINE_CMDS } else { 0 };
        o.put(&[lz | audio | cmds]);
        layouts(st, o); // LINK-62
        o.put(&lists_gen(st).to_le_bytes()); // LINK-71
        Status::Ok
    }

    /// `AUTH`: the radio's PIN over Bluetooth (LINK-31). The cable and a session with no PIN lock need none.
    fn auth(&mut self, app: &mut App, p: &[u8]) -> Status {
        if self.authorised(app) {
            return Status::Ok;
        }
        match app.remote_pin(p) {
            RemotePin::Ok => {
                self.pin_ok = true;
                Status::Ok
            }
            RemotePin::Wrong => Status::Refused,
            RemotePin::Wait(_) => Status::Busy,
        }
    }

    /// Whether this session's writes go ahead (LINK-9: refused after BACK until `END`; LINK-28: `busy` while the
    /// other port's session is mid-write).
    fn may_write(&mut self, _app: &mut App) -> Status {
        if self.cancelled {
            return Status::Cancelled;
        }
        if self.other_writing {
            return Status::Busy;
        }
        Status::Ok
    }

    fn begin(&mut self, app: &mut App, p: &[u8]) -> Status {
        let Some(b) = p.get(..2) else { return Status::BadArg };
        (self.session, self.total, self.done) = (true, u16::from_le_bytes([b[0], b[1]]), 0);
        app.link_write_progress();
        Status::Ok
    }

    fn end(&mut self, app: &mut App) {
        if self.session {
            app.link_write_done();
        }
        (self.session, self.cancelled, self.asm_len) = (false, false, 0);
    }

    fn write(&mut self, app: &mut App, p: &[u8], o: &mut Out) -> Status {
        if p.len() < 7 {
            return Status::BadArg;
        }
        let (w, idx, off, total) = (p[0], le16(&p[1..]), le16(&p[3..]), le16(&p[5..]));
        let data = &p[7..];
        if total as usize > RECORD_MAX || off as usize + data.len() > total as usize {
            return Status::BadArg;
        }
        if off == 0 {
            (self.asm_key, self.asm_len) = ((w, idx, total), 0);
        } else if self.asm_key != (w, idx, total) || off != self.asm_len {
            return Status::BadArg; // parts in order, of one record
        }
        self.asm[off as usize..off as usize + data.len()].copy_from_slice(data);
        self.asm_len = off + data.len() as u16;
        if self.asm_len < total {
            return Status::Ok;
        }
        self.asm_len = 0;
        let rec = &self.asm[..total as usize];
        // LINK-21, LINK-3: the records bigger than a frame; a channel, zone or preset goes by `WRITE_MANY`
        let s = match w {
            what::SETTINGS if idx == 0 => apply_settings(app, rec),
            what::APRS if idx == 0 => sub::aprs_apply(app, rec, o), // LINK-43
            what::RADIO if idx == 0 => radio_apply(app, rec), // LINK-66
            _ => Status::Refused,
        };
        if s == Status::Ok && self.session {
            self.done = self.done.saturating_add(1);
        }
        s
    }

    /// `WRITE_MANY` (LINK-41): what, then records as `READ_MANY` gives them. The whole frame is checked first (a
    /// malformed one changes nothing); then the records wait in `asm` for [`Session::many_step`]. A `WRITE` half
    /// assembled is dropped.
    fn write_many(&mut self, p: &[u8]) -> Status {
        let Some((&w, recs)) = p.split_first() else { return Status::BadArg };
        if !matches!(w, what::CHANNEL | what::ZONE | what::PRESET | what::CONTACT | what::POSITION | what::QUICK | what::WAYPOINT | what::DTMF | what::PRESET_SKIPS | what::SAT | what::SAT_INFO) {
            return Status::BadArg; // the settings are one record, larger than a frame: WRITE
        }
        let mut r = recs;
        while let Some(&l) = r.get(2) {
            let Some(rest) = r.get(3 + l as usize..) else { return Status::BadArg };
            r = rest;
        }
        if !r.is_empty() || recs.is_empty() {
            return Status::BadArg;
        }
        self.asm[..recs.len()].copy_from_slice(recs);
        self.asm_len = 0;
        self.many = Some(Many { seq: 0, what: w, len: recs.len() as u8, at: 0, done: 0 });
        Status::Ok
    }

    /// The next record of a `WRITE_MANY`, applied (LINK-22's checks, LINK-9's count):
    /// None while more follow; else the answer's length: the status, how many were applied, and a refusal's
    /// reason.
    fn many_step<S: ChannelStore>(&mut self, app: &mut App, st: &S, a: &mut [u8; MAX_PAYLOAD]) -> Option<usize> {
        let mut m = self.many?;
        let mut o = Out { a, n: 2 };
        let s = if self.cancelled {
            Status::Cancelled
        } else {
            let at = m.at as usize;
            let (idx, l) = (le16(&self.asm[at..]), self.asm[at + 2] as usize);
            let s = apply(app, st, m.what, idx, &self.asm[at + 3..at + 3 + l], &mut o);
            if s == Status::Ok {
                (m.done, m.at) = (m.done + 1, (at + 3 + l) as u8);
                if self.session {
                    self.done = self.done.saturating_add(1);
                }
            }
            s
        };
        if s == Status::Ok && m.at < m.len {
            self.many = Some(m);
            return None;
        }
        self.many = None;
        (o.a[0], o.a[1]) = (s as u8, m.done);
        Some(if s == Status::Invalid { o.n } else { 2 })
    }

    fn delete<S: ChannelStore>(&mut self, app: &mut App, st: &S, p: &[u8]) -> Status {
        if p.len() < 3 {
            return Status::BadArg;
        }
        let (w, idx) = (p[0], le16(&p[1..]));
        match w {
            what::CHANNEL if idx < MAX_CHANNELS => {
                if crate::term::radio_channel(app) == Some(idx) {
                    return Status::Busy; // open in the radio's editor (TERM-17's rule here too)
                }
                if st.get(idx).is_none() {
                    return Status::NotFound;
                }
                app.delete_channel(idx, st);
                Status::Ok
            }
            what::ZONE if idx < st.zones() => {
                let s = request(st, Op::DeleteZone(idx));
                if s == Status::Ok {
                    app.zone_deleted(idx); // CHAN-12.1, as the radio's own delete
                }
                s
            }
            what::PRESET if idx < st.data().presets() as u16 => request(st, Op::DeletePreset(idx as u8)),
            what::ZONE | what::PRESET => Status::NotFound,
            _ => Status::BadArg,
        }
    }
}

/// Where a byte from the cable goes (TERM-2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Term,
    Link,
}

/// TERM-2: the cable port setting (0 9 2 3: auto, link, terminal) decides; in auto a link frame (its
/// 0x00 delimiter: the web app sends one before its first frame) takes the port, and keeps it while frames
/// keep coming ([`ACTIVE_MS`]); anything else is the terminal's, which starts on a typed Enter.
pub fn route(cable_port: u8, link_active: bool, b: u8) -> Route {
    match cable_port {
        1 => Route::Link,
        2 => Route::Term,
        _ if link_active || b == 0 => Route::Link,
        _ => Route::Term,
    }
}

fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

/// The earliest and latest clock `CLOCK` takes (2020 to 2099).
const CLOCK_RANGE: core::ops::Range<u32> = 1_577_836_800..4_102_444_800;

/// `CLOCK` (LINK-65, GPS-23, SAT-3): UTC seconds set the clock as 0 6 6 does by hand; the answer is the clock now
/// and its source.
fn clock(app: &mut App, p: &[u8], o: &mut Out) -> Status {
    let (now, src) = match p {
        [] => match app.sat_now() {
            Some(t) => (t, if app.sat.by_gps { 1 } else { 2 }),
            None => (0, 0),
        },
        [a, b, c, d] => {
            let t = u32::from_le_bytes([*a, *b, *c, *d]);
            if !CLOCK_RANGE.contains(&t) {
                return Status::BadArg;
            }
            app.set_clock_by_hand(t);
            (t, 2)
        }
        _ => return Status::BadArg,
    };
    o.put(&now.to_le_bytes());
    o.put(&[src]);
    Status::Ok
}

/// `COOP_RX` (`LINK-73`): a browser's local cooperative receiver host polls the result of the last completed
/// RAD950 receive slot. The payload has no caller-controlled sizes and remains below the Link frame limit even
/// with all eight decoded frames plus the 92-byte four-bit LLR candidate.
fn coop_rx(app: &App, p: &[u8], o: &mut Out) -> Status {
    if !p.is_empty() {
        return Status::BadArg;
    }

    const VERSION: u8 = 1;
    const UTC_READY: u8 = 1 << 0;
    const ARMED: u8 = 1 << 1;
    const REPORT: u8 = 1 << 2;
    const SOFT: u8 = 1 << 3;

    let Some(h) = app.rad950_host() else {
        o.put(&[VERSION, 0]);
        o.put(&0u32.to_le_bytes());
        o.put(&0u32.to_le_bytes());
        o.put(&0u32.to_le_bytes());
        o.put(&[0]);
        return Status::Ok;
    };

    let armed = app.rad950_open() && h.coop_bluetooth_armed();
    let current_slot = if h.utc_ready() { h.utc_sec - h.utc_sec % ferrite_core::rad950::SLOT_S } else { 0 };
    // A host polls every receive guard. Do not revive an old report after Capture is toggled off and on again.
    let report = armed
        .then_some(h.last_coop)
        .flatten()
        .filter(|r| current_slot <= r.utc_slot.saturating_add(ferrite_core::rad950::SLOT_S));
    let mut flags = if h.utc_ready() { UTC_READY } else { 0 };
    if armed {
        flags |= ARMED;
    }
    if report.is_some() {
        flags |= REPORT;
    }
    if report.is_some_and(|r| r.soft.is_some()) {
        flags |= SOFT;
    }

    o.put(&[VERSION, flags]);
    o.put(&current_slot.to_le_bytes());
    o.put(&report.map_or(0, |r| r.utc_slot).to_le_bytes());
    o.put(&report.map_or(0, |r| r.dial_hz).to_le_bytes());
    let n = report.map_or(0, |r| r.decoded_len.min(ferrite_core::rad950::urad950::MAX_DECODED as u8));
    o.put(&[n]);
    let Some(report) = report else { return Status::Ok };

    for d in &report.decoded[..n as usize] {
        o.put(&d.frame.pack_u64().to_le_bytes());
        o.put(&d.audio_hz.to_le_bytes());
        o.put(&d.time_offset_ms.to_le_bytes());
        o.put(&[d.snr_db as u8]);
    }
    if let Some(soft) = report.soft {
        o.put(&soft.audio_hz.to_le_bytes());
        o.put(&soft.time_offset_ms.to_le_bytes());
        o.put(&[soft.snr_db as u8]);
        o.put(&soft.sync_score.to_le_bytes());
        o.put(&soft.llrs_4bit);
    }
    Status::Ok
}

/// `RAD950_TX` (`0x65`, cable-only): stage an immediate 110-symbol RAD950 4-FSK burst from a USB-connected
/// `rad950-node --backend rt950` host. Payload: `rf_freq_10hz: u32 LE` + `power: u8` (`0..=2`, `0xFF` = radio
/// default) + 28 bytes of packed 2-bit tones (LSB-first, 110 symbols).
fn rad950_tx_link(app: &mut App, p: &[u8]) -> Status {
    const PACKED_TONES: usize = ferrite_core::rad950::urad950::N_SYMBOLS.div_ceil(4); // 28 bytes
    if p.len() != 5 + PACKED_TONES {
        return Status::BadArg;
    }
    let Some(h) = app.rad950_host_mut() else {
        return Status::Invalid;
    };
    if h.tx_in_flight || h.pending_link_tx.is_some() {
        return Status::Busy;
    }
    let rf_freq = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
    let power = if p[4] == 0xFF { h.profile.tx_power } else { p[4].min(2) };
    let mut tones = [0u8; ferrite_core::rad950::urad950::N_SYMBOLS];
    for (i, t) in tones.iter_mut().enumerate() {
        *t = (p[5 + (i >> 2)] >> ((i & 3) * 2)) & 0x03;
    }
    h.pending_link_tx = Some((rf_freq, power, tones));
    Status::Ok
}

fn request<S: ChannelStore>(st: &S, op: Op) -> Status {
    if st.data().request(op) {
        Status::Ok
    } else {
        Status::Busy
    }
}

/// An answer's payload being written: after the status byte.
struct Out<'a> {
    a: &'a mut [u8; MAX_PAYLOAD],
    n: usize,
}

impl Out<'_> {
    #[inline(never)] // one copy, not one at each of its many callers (size review #9)
    fn put(&mut self, b: &[u8]) {
        let k = b.len().min(MAX_PAYLOAD - self.n);
        self.a[self.n..self.n + k].copy_from_slice(&b[..k]);
        self.n += k;
    }

    /// A length byte, then the text.
    fn text(&mut self, t: &[u8]) {
        self.put(&[t.len().min(255) as u8]);
        self.put(t);
    }
}

/// `KEY`: a keypad or side key, or the knob (LINK-19), or remote PTT (LINK-20).
fn key<S: ChannelStore>(app: &mut App, st: &S, by: u8, now: u32, p: &[u8], o: &mut Out) -> Status {
    use crate::remote::{KeyRefusal, Press};
    use ferrite_core::link::key as k;
    let res = match (p.first().copied(), p.get(1).copied(), p.len()) {
        (Some(c), Some(a), 2) if c <= k::LAST_CODE => match Press::of(a) {
            Some(how) => keypad(app, st, |app| app.remote_press(c, how, by, now, st)),
            None => Err(KeyRefusal::Invalid("")),
        },
        (Some(k::KNOB), Some(n), 2) => keypad(app, st, |app| app.remote_knob(n as i8, now, st)),
        (Some(k::PTT), ..) => return remote_ptt(app, st, by, now, p, o),
        _ => return Status::BadArg,
    };
    match res {
        Ok(()) => Status::Ok,
        Err(KeyRefusal::Busy) => Status::Busy,
        Err(KeyRefusal::Invalid("")) => Status::BadArg,
        Err(KeyRefusal::Invalid(why)) => {
            o.put(why.as_bytes());
            Status::Invalid
        }
    }
}

/// A remote key waits for the store as every other request does (a key may write a channel); `KEY` alone is
/// handled without waiting for it (LINK-20's keepalives), so the wait is a `busy` here.
fn keypad<S: ChannelStore>(app: &mut App, st: &S, f: impl FnOnce(&mut App) -> Result<(), crate::remote::KeyRefusal>) -> Result<(), crate::remote::KeyRefusal> {
    if app.write_pending() || st.data().busy() {
        return Err(crate::remote::KeyRefusal::Busy);
    }
    f(app)
}

/// `KEY` (LINK-20): remote PTT.
///
/// LINK-28: one keyer at a time. `on` and keepalives from the port that didn't key are refused; `off` from either
/// port unkeys (stopping a transmission is always allowed).
fn remote_ptt<S: ChannelStore>(app: &mut App, st: &S, by: u8, now: u32, p: &[u8], o: &mut Out) -> Status {
    let res = match Ptt::decode(p) {
        None => return Status::BadArg,
        Some(Ptt::Off) => {
            app.remote_unkey(RemoteEnd::Off, now, st);
            return Status::Ok;
        }
        Some(Ptt::On(slot)) => app.remote_key(slot, by, now, st).map(|s| o.put(&[s])),
        Some(Ptt::Alive) => app.remote_keepalive(by, now),
    };
    match res {
        Ok(()) => Status::Ok,
        Err(why) => {
            o.put(why.as_bytes());
            Status::Invalid
        }
    }
}

/// `SLOT` (LINK-33): the slot, then optionally a mask and its fields; the slot after, or `invalid` and the reason
/// (nothing changed).
fn slot<S: ChannelStore>(app: &mut App, st: &S, p: &[u8], o: &mut Out) -> Status {
    use ferrite_core::link::slot as m;
    let Some(&s) = p.first().filter(|&&s| (s as usize) < ferrite_core::msg::SLOTS) else { return Status::BadArg };
    let mask = p.get(1).copied().unwrap_or(0);
    let mut c = crate::slotctl::SlotChange { vfo: mask & m::VFO != 0, active: mask & m::ACTIVE != 0, ..Default::default() };
    let mut f = p.get(2..).unwrap_or(&[]);
    let mut take = |n: usize| -> Option<u32> {
        let b = f.get(..n)?;
        f = &f[n..];
        Some(b.iter().rev().fold(0u32, |v, &x| v << 8 | x as u32))
    };
    if mask & m::FREQ != 0 {
        let Some(v) = take(4) else { return Status::BadArg };
        c.freq = Some(v);
    }
    if mask & m::MODE != 0 {
        let Some(v) = take(1) else { return Status::BadArg };
        c.mode = Some(v as u8);
    }
    if mask & m::POWER != 0 {
        let Some(v) = take(1) else { return Status::BadArg };
        c.power = Some(v as u8);
    }
    if mask & m::RX_TONE != 0 {
        let Some(v) = take(2) else { return Status::BadArg };
        c.rx_tone = Some(v as u16);
    }
    if mask & m::TX_TONE != 0 {
        let Some(v) = take(2) else { return Status::BadArg };
        c.tx_tone = Some(v as u16);
    }
    if mask & m::OFFSET != 0 {
        let (Some(v), Some(split)) = (take(4), take(1)) else { return Status::BadArg };
        if split > 1 {
            return Status::BadArg;
        }
        c.offset = Some((v as i32, split == 1));
    }
    if !f.is_empty() {
        return Status::BadArg;
    }
    if !c.is_empty() {
        if let Err(why) = app.slot_apply(s, &c, st) {
            o.put(why.as_bytes());
            return Status::Invalid;
        }
    }
    let g = app.slot_state(s);
    o.put(&[s, g.active as u8]);
    o.put(&g.channel.unwrap_or(0xFFFF).to_le_bytes());
    o.put(&g.freq.to_le_bytes());
    o.put(&[g.mode, g.power]);
    o.put(&g.rx_tone.to_le_bytes());
    o.put(&g.tx_tone.to_le_bytes());
    o.put(&g.offset.to_le_bytes());
    let flags = if g.split { m::SPLIT } else { 0 } | if g.aprs { m::APRS } else { 0 } | if g.scanning { m::SCANNING } else { 0 };
    o.put(&[flags]);
    Status::Ok
}

/// `TUNE` (LINK-68): the slot, the op and its argument; the slot after as `SLOT` reads it, or `invalid` and the
/// reason (nothing changed).
fn tune<S: ChannelStore>(app: &mut App, st: &S, p: &[u8], o: &mut Out) -> Status {
    use crate::slotctl::Tune;
    use ferrite_core::link::tune as t;
    let (s, op, arg) = match p {
        [s, op, arg @ ..] if (*s as usize) < ferrite_core::msg::SLOTS => (*s, *op, arg),
        _ => return Status::BadArg,
    };
    let u16_arg = || <[u8; 2]>::try_from(arg).ok().map(u16::from_le_bytes);
    let i8_arg = |max: i8| match arg {
        &[v] if v as i8 != 0 && (v as i8).unsigned_abs() <= max as u8 => Some(v as i8),
        _ => None,
    };
    let what = match op {
        t::CHANNEL => u16_arg().map(Tune::Channel),
        t::STEP => i8_arg(100).map(Tune::Step),
        t::GROUP => i8_arg(20).map(Tune::Group),
        t::ZONE => u16_arg().map(Tune::Zone),
        t::SCAN => match arg {
            &[on @ (0 | 1)] => Some(Tune::Scan(on == 1)),
            _ => None,
        },
        t::QUIET => match arg {
            &[q @ 0..=2] => Some(Tune::Quiet(q)),
            _ => None,
        },
        _ => None,
    };
    let Some(what) = what else { return Status::BadArg };
    if let Err(why) = app.slot_tune(s, what, st) {
        o.put(why.as_bytes());
        return Status::Invalid;
    }
    slot(app, st, &[s], o)
}

fn info<S: ChannelStore>(app: &App, st: &S, dev: &Device, o: &mut Out) -> Status {
    o.text(VERSION.as_bytes());
    o.put(&st.count().to_le_bytes());
    o.put(&st.zones().to_le_bytes());
    o.put(&[st.data().presets()]);
    o.put(&(settings_record(app, &mut Window { b: &mut [], from: 0, n: 0 }) as u16).to_le_bytes());
    o.put(&[app.set.region]);
    o.text(app.set.callsign.as_bytes());
    o.put(&[ferrite_core::link::build::of(dev.tx_built, app.dry_tx(), dev.bench)]); // LINK-35, TX-48
    o.put(&ferrite_core::uid::bytes(dev.selftest.uid)); // LINK-35.1: the unique ID, zeros until read
    layouts(st, o); // LINK-62
    Status::Ok
}

/// LINK-72 (decision 388): the phone's position, kept for GPS-3.1 (used locally while fresh, never beaconed).
fn phone_pos<S: ChannelStore>(st: &S, p: &[u8], o: &mut Out) -> Status {
    let [a, b, c, d, e, f, g, h, i, j, k, l, m, n] = *p else { return Status::BadArg };
    let (lat, lon) = (i32::from_le_bytes([a, b, c, d]), i32::from_le_bytes([e, f, g, h]));
    let alt = i16::from_le_bytes([i, j]);
    let (acc_m, age_s) = (u16::from_le_bytes([k, l]), u16::from_le_bytes([m, n]));
    if lat.unsigned_abs() > 900_000_000 || lon.unsigned_abs() > 1_800_000_000 {
        o.put(b"position out of range");
        return Status::Invalid;
    }
    let pos = crate::data::Pos { lat, lon };
    st.data().phone_position(crate::data::PhonePos { pos, alt_m: (alt != 0x7FFF).then_some(alt), acc_m, age_s });
    Status::Ok
}

/// LINK-71: the lists hash: changes when a channel, zone or range preset stored changes; the same across restarts
/// when nothing was written.
pub(crate) fn lists_gen<S: ChannelStore>(st: &S) -> u32 {
    st.lists_versions().iter().fold(0x811C_9DC5u32, |a, &v| v.to_le_bytes().iter().fold(a, |h, &b| (h ^ b as u32).wrapping_mul(0x0100_0193)))
}

/// LINK-62, UPD-16 (decision 333): the layout this firmware stores, then the one the chip's data is in (the same
/// when all is well; another: Console finishes the update), a byte each.
fn layouts<S: ChannelStore>(st: &S, o: &mut Out) {
    let own = ferrite_core::layout::LAYOUT_VERSION;
    let found = match st.data().layout_found() {
        0 => own,
        v => v,
    };
    o.put(&[own as u8, found.min(255) as u8]);
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// `DATA_READ` (LINK-59): bytes or a CRC of the data range, never calibration or the update area; `busy` while the
/// store still has writes to make (a read-back then shows them all).
fn data_read<S: ChannelStore>(st: &S, p: &[u8], o: &mut Out) -> Status {
    use ferrite_core::link::data::*;
    let d = st.data();
    match *p {
        [READ, a0, a1, a2, a3, n] => {
            let (addr, n) = (le32(&[a0, a1, a2, a3]), n as usize);
            if n == 0 || n > READ_MAX {
                return Status::BadArg;
            }
            if !allowed(addr, n as u32) {
                return Status::Refused;
            }
            if d.busy() {
                return Status::Busy;
            }
            let mut b = [0u8; READ_MAX];
            if !d.data_read(addr, &mut b[..n]) {
                return Status::NotFound;
            }
            o.put(&b[..n]);
            Status::Ok
        }
        [CRC, a0, a1, a2, a3, l0, l1, l2, l3] => {
            let (addr, len) = (le32(&[a0, a1, a2, a3]), le32(&[l0, l1, l2, l3]));
            if !allowed(addr, len) {
                return Status::Refused;
            }
            if d.busy() {
                return Status::Busy;
            }
            match d.data_crc(addr, len) {
                Some(c) => {
                    o.put(&c.to_le_bytes());
                    Status::Ok
                }
                None => Status::NotFound,
            }
        }
        _ => Status::BadArg,
    }
}

/// LINK-60 (decision 339): the layout header and the migration mark are the store's own (the begin and the stamp
/// write them), never a raw write's.
fn stores_own(addr: u32, len: u32) -> bool {
    use ferrite_core::layout::{LAYOUT_HEADER, MIGRATE_MARK};
    let end = addr as u64 + len as u64;
    [LAYOUT_HEADER, MIGRATE_MARK].iter().any(|r| (addr as u64) < r.end as u64 && end > r.start as u64)
}

/// `DATA_WRITE` (LINK-60): a sector erased, bytes programmed or the header stamped, queued for the store's owner
/// (`busy`: its queue is full, or for the stamp anything is still queued or being written: send it again). Never
/// calibration or the update area, the header or the migration mark; a program inside one page. True as the second
/// value for a stamp: the radio restarts once the store has done it (the restart waits for the store, LINK-62).
fn data_write<S: ChannelStore>(st: &S, p: &[u8], o: &mut Out) -> (Status, bool) {
    use crate::data::DataWrite;
    use ferrite_core::link::data::*;
    let w = match p {
        [ERASE, a0, a1, a2, a3] => {
            let addr = le32(&[*a0, *a1, *a2, *a3]);
            if !addr.is_multiple_of(SECTOR) {
                return (Status::BadArg, false);
            }
            if !allowed(addr, SECTOR) || !ferrite_core::layout::writable(addr, SECTOR) || stores_own(addr, SECTOR) {
                return (Status::Refused, false);
            }
            DataWrite::Erase(addr)
        }
        [PROGRAM, a0, a1, a2, a3, bytes @ ..] => {
            let addr = le32(&[*a0, *a1, *a2, *a3]);
            let n = bytes.len();
            if n == 0 || n > PROGRAM_MAX || addr / PAGE != (addr + n as u32 - 1) / PAGE {
                return (Status::BadArg, false);
            }
            if !allowed(addr, n as u32) || !ferrite_core::layout::writable(addr, n as u32) || stores_own(addr, n as u32) {
                return (Status::Refused, false);
            }
            let mut b = [0xFFu8; PROGRAM_MAX];
            b[..n].copy_from_slice(bytes);
            DataWrite::Program { addr, len: n as u8, bytes: b }
        }
        [STAMP, v0, v1] => {
            if u16::from_le_bytes([*v0, *v1]) != ferrite_core::layout::LAYOUT_VERSION {
                o.put(b"layout: this firmware stores layout 3");
                return (Status::Invalid, false);
            }
            DataWrite::Stamp
        }
        _ => return (Status::BadArg, false),
    };
    if !st.data().data_write(w) {
        return (Status::Busy, false);
    }
    (Status::Ok, w == DataWrite::Stamp)
}

// ---- Records (Appendix "Link protocol v1": the same byte layouts as the radio's own storage) ----

/// A record's bytes `from..from + b.len()` into `b` as it is written out, counting all of it: a read takes a
/// window of a record without a copy of the whole.
struct Window<'a> {
    b: &'a mut [u8],
    from: usize,
    n: usize,
}

impl PSink for Window<'_> {
    fn put(&mut self, bytes: &[u8]) {
        for (k, &c) in bytes.iter().enumerate() {
            if let Some(i) = (self.n + k).checked_sub(self.from) {
                if let Some(d) = self.b.get_mut(i) {
                    *d = c;
                }
            }
        }
        self.n += bytes.len();
    }
}

/// Bytes out of a record.
struct Rd<'a> {
    b: &'a [u8],
    n: usize,
}

impl Source for Rd<'_> {
    fn take(&mut self, out: &mut [u8]) -> bool {
        match self.b.get(self.n..self.n + out.len()) {
            Some(s) => {
                out.copy_from_slice(s);
                self.n += out.len();
                true
            }
            None => false,
        }
    }
}

/// The settings record as the link reads it, into `w`: the PIN reads as zeros (LINK-7). Its whole length.
fn settings_record(app: &App, w: &mut Window) -> usize {
    let m = Settings { pin: 0, pin_misses: 0, pin_wait: 0, ..*app.set };
    m.save(w);
    w.n
}

/// The radio record (LINK-66): Watch, Battery Save, then the position the radio would use (GPS-3) and its fix.
fn radio_record(app: &App, fix: Option<crate::data::Fix>, w: &mut Window) {
    let (kind, pos, age) = match fix {
        Some(f) if f.phone => (4, Some(f.pos), f.age_s), // GPS-3.1: the phone's (LINK-72)
        Some(f) if f.age_s != crate::gpsdata::FIXED_AGE => (if f.three_d { 3 } else { 2 }, Some(f.pos), f.age_s),
        _ => (0, (app.set.fixed_on).then_some(app.set.fixed), 0xFFFF),
    };
    let pos = pos.unwrap_or(crate::data::Pos { lat: 0, lon: 0 });
    w.put(&[app.watch, app.bsave, kind, app.aprs_cfg().sound]);
    w.put(&pos.lat.to_le_bytes());
    w.put(&pos.lon.to_le_bytes());
    w.put(&age.to_le_bytes());
}

/// A radio record written (LINK-66): Watch and Battery Save, each changed as the menu changes it, and the message
/// sound when the record reaches it.
fn radio_apply(app: &mut App, rec: &[u8]) -> Status {
    let [watch, bsave, ..] = *rec else { return Status::BadArg };
    let sound = rec.get(3).copied();
    if watch > 2 || bsave > 3 || sound.is_some_and(|s| s as usize >= ferrite_core::tonegen::CHIMES.len()) {
        return Status::Invalid;
    }
    app.change(crate::Item::Watch, watch as i32 - app.watch as i32);
    app.change(crate::Item::BatterySave, bsave as i32 - app.bsave as i32);
    if let Some(sound) = sound.filter(|&s| s != app.aprs_cfg().sound) {
        app.set_aprs(crate::aprs::AprsCfg { sound, ..app.aprs_cfg() });
    }
    Status::Ok
}

/// Settings the link never changes (LINK-5): the region, Extend TX, the PIN, setup, the cable port (the link
/// would lose its own port), the KISS port and the spare bytes. Dry TX (TX-48) isn't in the record at all. A written
/// record keeps the radio's. The callsign is written, in capitals as on the radio (LINK-26, decision 77: as nicFW's
/// RMS writes it). Low Power Mode and its Auto level are written (PWR-24.1, decision 328).
fn keep_protected(new: &mut Settings, old: &Settings) {
    let call = new.callsign;
    new.set_callsign(call.as_bytes());
    new.region = old.region;
    (new.extend_tx, new.extend_warned) = (old.extend_tx, old.extend_warned);
    (new.pin_mode, new.pin, new.pin_len, new.pin_misses, new.pin_wait) = (old.pin_mode, old.pin, old.pin_len, old.pin_misses, old.pin_wait);
    new.setup_done = old.setup_done;
    new.cable_port = old.cable_port;
    new.kiss_port = old.kiss_port; // KISS-7: turning KISS on is the owner's consent, made on the radio
    new.spare_tokens = [0; 30];
    new.spare_temp = 0;
    new.spare_lock = 0;
}

fn channel_save(c: &Channel, s: &mut dyn PSink) {
    c.num.save(s);
    c.zone.save(s);
    c.rx.save(s);
    s.put(&c.name);
    c.cfg.save(s);
    c.flags.save(s);
    c.tones.save(s); // decision 86: end tone and tail; a writer from before it leaves it off (Global)
}

fn zone_save(name: &[u8], c: &ZoneCfg, s: &mut dyn PSink) {
    let mut n = [0u8; 16];
    n[..name.len().min(16)].copy_from_slice(&name[..name.len().min(16)]);
    s.put(&n);
    for v in [c.scan_all, c.shown, c.by_distance] {
        v.save(s);
    }
    c.resume.save(s);
    c.dwell.save(s);
    0u8.save(s); // reserved: it was smart scan (SCAN-23)
    c.pri.save(s);
}

fn preset_save(p: &RangePreset, s: &mut dyn PSink) {
    let mut n = [0u8; 12];
    let b = p.name.as_bytes();
    n[..b.len().min(12)].copy_from_slice(&b[..b.len().min(12)]);
    s.put(&n);
    p.start.save(s);
    p.end.save(s);
    p.step.save(s);
    p.mode.save(s);
    p.narrow.save(s);
    p.resume.save(s); // SCAN-24; a record without it reads as Global
}

/// A name from a fixed field: up to the first 0, printable ASCII only.
fn name_of<const N: usize>(b: &[u8]) -> Option<Buf<N>> {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len()).min(N);
    if !b[..n].iter().all(|c| (0x20..0x7F).contains(c)) {
        return None;
    }
    Some(Buf::from(&b[..n]))
}

/// DISP-30: the requests of a big transfer, records read or written (channels, zones, presets, settings, the lists,
/// APRS heard and messages, the event log, a stock restore), which flash the LED; not the screen readback, keys,
/// commands, events or audio.
fn transfer(ty: u8, p: &[u8]) -> bool {
    match ty {
        msg::READ | msg::WRITE | msg::DELETE | msg::BEGIN | msg::USED | msg::READ_MANY | msg::WRITE_MANY | msg::LOG_READ | msg::HEARD | msg::MSGS | msg::STOCK_RESTORE | msg::DATA_READ | msg::DATA_WRITE => true,
        msg::STREAM => p.first().is_some_and(|&t| t != msg::UI),
        _ => false,
    }
}

/// Channel, zone or preset `idx`, as `READ_MANY` gives it, into `win`: `not found` when there is none.
fn record<S: ChannelStore>(st: &S, w: u8, idx: u16, win: &mut Window) -> Status {
    match w {
        what::CHANNEL => match st.get(idx) {
            Some(c) => channel_save(&c, win),
            None => return Status::NotFound,
        },
        what::ZONE => match st.zone_name(idx).filter(|_| idx < st.zones()) {
            Some(n) => zone_save(n.as_bytes(), &st.data().zone_cfg(idx), win),
            None => return Status::NotFound,
        },
        what::PRESET => match st.data().preset(idx as u8).filter(|_| idx < 256) {
            Some(pr) => preset_save(&pr, win),
            None => return Status::NotFound,
        },
        // LINK-53: latitude and longitude, 1e-7 degree; a channel without one isn't listed
        what::POSITION => match st.position(idx).filter(|_| idx < MAX_CHANNELS) {
            Some(p) => {
                win.put(&p.lat.to_le_bytes());
                win.put(&p.lon.to_le_bytes());
            }
            None => return Status::NotFound,
        },
        // APRS-42: callsign (10) and name (12), 0-padded, then the flags (0)
        what::CONTACT => match st.data().contact(idx) {
            Some(c) => {
                let mut b = [0u8; 23];
                b[..c.call.len()].copy_from_slice(c.call.as_bytes());
                b[10..10 + c.name.len()].copy_from_slice(c.name.as_bytes());
                win.put(&b);
            }
            None => return Status::NotFound,
        },
        what::QUICK | what::WAYPOINT | what::DTMF | what::PRESET_SKIPS | what::SAT | what::SAT_INFO => return lists::record(st, w, idx, win), // LINK-54 – LINK-56, LINK-58, LINK-63, LINK-64
        // LINK-71: a list sector's hash
        what::LIST_HASH => match st.sector_hash(idx).filter(|_| idx < what::LIST_SECTORS) {
            Some(h) => win.put(&h.to_le_bytes()),
            None => return Status::NotFound,
        },
        _ => return Status::BadArg,
    }
    Status::Ok
}

fn read(app: &App, fix: Option<crate::data::Fix>, p: &[u8], o: &mut Out) -> Status {
    if p.len() < 6 {
        return Status::BadArg;
    }
    let (w, idx, off, len) = (p[0], le16(&p[1..]), le16(&p[3..]) as usize, (p[5] as usize).min(MAX_PAYLOAD - 3));
    let mut buf = [0u8; MAX_PAYLOAD];
    let mut win = Window { b: &mut buf[..len], from: off, n: 0 };
    match w {
        // LINK-21: the records bigger than a frame; a channel, zone or preset goes by `READ_MANY`
        what::SETTINGS if idx == 0 => _ = settings_record(app, &mut win),
        what::APRS if idx == 0 => sub::aprs_record(app, &mut win), // LINK-43
        what::RADIO if idx == 0 => radio_record(app, fix, &mut win), // LINK-66
        _ => return Status::BadArg,
    }
    let total = win.n;
    if off > total {
        return Status::BadArg;
    }
    let got = len.min(total - off);
    o.put(&(total as u16).to_le_bytes());
    o.put(&buf[..got]);
    Status::Ok
}

/// How many indexes `w` has: channels 0-999, the zones and presets there are. None for the settings.
fn indexes<S: ChannelStore>(st: &S, w: u8) -> Option<u16> {
    match w {
        what::CHANNEL | what::POSITION => Some(MAX_CHANNELS),
        what::ZONE => Some(st.zones()),
        what::PRESET => Some(st.data().presets() as u16),
        what::CONTACT => Some(st.data().contacts()),
        what::LIST_HASH => Some(what::LIST_SECTORS), // LINK-71
        _ => lists::indexes(st, w),
    }
}

/// Whether index `i` of `w` holds a record; a channel from the store's zone map (RAM on the radio), without
/// reading it.
fn held<S: ChannelStore>(st: &S, w: u8, i: u16) -> bool {
    match w {
        // LINK-53: a stored channel; whether it has a position is its record's (read by `READ_MANY` only)
        what::CHANNEL | what::POSITION => st.zone_of(i).is_some(),
        what::ZONE => st.zone_name(i).is_some(),
        what::CONTACT => i < st.data().contacts(),
        what::LIST_HASH => true,
        what::QUICK | what::WAYPOINT | what::DTMF | what::PRESET_SKIPS | what::SAT | what::SAT_INFO => lists::held(st, w, i),
        _ => st.data().preset(i as u8).is_some(),
    }
}

/// `USED` (LINK-39): the index count, the first index answered (the request's, 0 without one), then the bitmap of
/// the ones holding a record from there, as many as fit (1,880: 4,000 channels take three asks).
fn used<S: ChannelStore>(st: &S, p: &[u8], o: &mut Out) -> Status {
    let Some(w) = p.first().copied().filter(|&w| w < what::POSITION) else { return Status::BadArg }; // LINK-53 – LINK-56: READ_MANY
    let Some(n) = indexes(st, w) else { return Status::BadArg };
    let first = if p.len() >= 3 { le16(&p[1..]) } else { 0 };
    if first > n {
        return Status::BadArg;
    }
    let end = (first as u32 + USED_MAX as u32).min(n as u32) as u16;
    o.put(&n.to_le_bytes());
    o.put(&first.to_le_bytes());
    let mut byte = 0u8;
    for i in first..end {
        let k = i - first;
        if held(st, w, i) {
            byte |= 1 << (k % 8);
        }
        if k % 8 == 7 || i + 1 == end {
            o.put(&[byte]);
            byte = 0;
        }
    }
    Status::Ok
}

/// LINK-39: the most indexes one `USED` answer maps (its bytes after the status and the two counts).
pub const USED_MAX: u16 = ((MAX_PAYLOAD - 5) * 8) as u16;

/// `READ_MANY` (LINK-40): from `first`, `count` indexes on, each record held as a whole, as many as fit; the
/// index to go on from first.
fn read_many<S: ChannelStore>(st: &S, p: &[u8], o: &mut Out) -> Status {
    if p.len() < 5 {
        return Status::BadArg;
    }
    let (w, first, count) = (p[0], le16(&p[1..]), le16(&p[3..]));
    let Some(n) = indexes(st, w) else { return Status::BadArg };
    let end = (first as u32 + count as u32).min(n as u32) as u16;
    let next = o.n;
    o.put(&[0, 0]);
    let mut i = first;
    let mut buf = [0u8; MAX_PAYLOAD];
    while i < end {
        if held(st, w, i) {
            let mut win = Window { b: &mut buf, from: 0, n: 0 };
            if record(st, w, i, &mut win) == Status::Ok {
                let k = win.n;
                if o.n + 3 + k > MAX_PAYLOAD {
                    break;
                }
                o.put(&i.to_le_bytes());
                o.put(&[k as u8]);
                o.put(&buf[..k]);
            }
        }
        i += 1;
    }
    o.a[next..next + 2].copy_from_slice(&i.max(first).to_le_bytes());
    Status::Ok
}

/// `LOG_READ` (LINK-42): the event log (INBOX-6) newest first, from below the sequence number `before`.
fn log_read<S: ChannelStore>(st: &S, p: &[u8], o: &mut Out) -> Status {
    log_read_at(st, p, o).0
}

/// [`log_read`], and for a stream (LINK-50) the `before` to go on from when older records are left.
fn log_read_at<S: ChannelStore>(st: &S, p: &[u8], o: &mut Out) -> (Status, Option<u32>) {
    let Some(b) = p.get(..4) else { return (Status::BadArg, None) };
    let before = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let most = p.get(4).copied().filter(|&m| m > 0).unwrap_or(u8::MAX);
    let d = st.data();
    let kept = d.events();
    let top = d.event_seq(0).map_or(0, |s| s.wrapping_add(1));
    o.put(&top.to_le_bytes());
    o.put(&kept.to_le_bytes());
    let count = o.n;
    o.put(&[0]);
    let (mut i, mut n) = (top.saturating_sub(before).min(kept as u32) as u16, 0u8);
    let mut last = None;
    while i < kept && n < most {
        if let (Some(e), Some(seq)) = (d.event(i), d.event_seq(i)) {
            if !log_record(o, seq, &e) {
                break;
            }
            n += 1;
            last = Some(seq);
        }
        i += 1;
    }
    o.a[count] = n;
    (Status::Ok, last.filter(|_| i < kept))
}

/// One event log record (LINK-42) as `LOG_READ` and the event log's `EVENT` (LINK-57) give it; false (nothing put)
/// when it doesn't fit.
fn log_record(o: &mut Out, seq: u32, e: &crate::data::Event) -> bool {
    use ferrite_core::link::{LOG_FIXED, LOG_POS, LOG_UNREAD};
    let (nm, tx) = (e.name.as_bytes(), e.text.as_bytes());
    let k = LOG_FIXED + if e.pos.is_some() { 8 } else { 0 } + 2 + nm.len() + tx.len();
    if o.n + k > MAX_PAYLOAD {
        return false;
    }
    o.put(&seq.to_le_bytes());
    o.put(&e.ago_s.to_le_bytes());
    let flags = if e.unread { LOG_UNREAD } else { 0 } | if e.pos.is_some() { LOG_POS } else { 0 };
    o.put(&[e.src as u8, e.slot, flags]);
    o.put(&e.freq.to_le_bytes());
    o.put(&e.id.to_le_bytes());
    o.put(&e.x.to_le_bytes());
    o.put(&e.y.to_le_bytes());
    if let Some(ps) = e.pos {
        o.put(&ps.lat.to_le_bytes());
        o.put(&ps.lon.to_le_bytes());
    }
    o.text(nm);
    o.text(tx);
    true
}

/// LINK-41.1: room for one more record of a `WRITE_MANY` of `what`: the App holds no write, and for a channel the
/// store's queue has room (behind channel writes only), for anything else the store is idle (a zone may need the
/// one before it stored).
fn write_room<S: ChannelStore>(app: &App, st: &S, what: u8) -> bool {
    !app.write_pending() && if matches!(what, what::CHANNEL | what::POSITION) { st.data().write_room() > 0 } else { !st.data().busy() }
}

/// The settings record written (`WRITE`): the protected ones keep the radio's (LINK-5).
fn apply_settings(app: &mut App, rec: &[u8]) -> Status {
    let old = *app.set;
    let mut new = old;
    new.load(&mut Rd { b: rec, n: 0 });
    new.sanitize();
    keep_protected(&mut new, &old);
    *app.set = new;
    app.send_radio_settings();
    if new.lpm != old.lpm {
        app.set_lpm_mode(new.lpm); // PWR-24.1: as the menu turns it (On goes in, Off comes out)
    }
    Status::Ok
}

/// A channel, zone or preset written (`WRITE_MANY`): checked like the radio's own editors (LINK-5), then saved the
/// same way.
#[inline(never)] // one copy, not one in `many_step` (flash)
fn apply<S: ChannelStore>(app: &mut App, st: &S, w: u8, idx: u16, rec: &[u8], o: &mut Out) -> Status {
    let invalid = |o: &mut Out, why: &[u8]| {
        o.put(why);
        Status::Invalid
    };
    match w {
        what::CHANNEL if idx < MAX_CHANNELS && rec.is_empty() => {
            // LINK-41.2: a record of length 0 deletes the channel (none there: nothing to do)
            if crate::term::radio_channel(app) == Some(idx) {
                return Status::Busy; // open in the radio's editor (TERM-17)
            }
            if st.get(idx).is_some() {
                app.delete_channel_queued(idx, st);
            }
            Status::Ok
        }
        what::CHANNEL if idx < MAX_CHANNELS => {
            let mut c = Channel { num: idx, zone: NO_ZONE, rx: 0, name: [0; 16], cfg: SlotCfg::default(), flags: 0, tones: 0 };
            let mut r = Rd { b: rec, n: 0 };
            let mut num = 0u16;
            num.load(&mut r);
            c.zone.load(&mut r);
            c.rx.load(&mut r);
            let mut name = [0u8; 16];
            if !r.take(&mut name) {
                return Status::BadArg;
            }
            c.cfg.load(&mut r);
            c.flags.load(&mut r);
            if r.n < rec.len() {
                c.tones.load(&mut r); // decision 86; absent from an older writer: Global
                c.tones = c.end_tone() | c.tail() << 3;
            }
            if r.n != rec.len() {
                return Status::BadArg;
            }
            let Some(nm) = name_of::<16>(&name) else { return invalid(o, "name: printable ASCII only".as_bytes()) };
            c.set_name(nm.as_bytes());
            if c.zone != NO_ZONE && c.zone >= st.zones() {
                return invalid(o, "zone: no such zone".as_bytes());
            }
            if !st.zone_room(c.zone, idx) {
                return invalid(o, crate::text::load(crate::term::cmd::ZONE_FULL).or(b"zone: full")); // CHAN-35
            }
            if !crate::slot::slot_mode_ok(c.cfg.mode) || c.cfg.step as usize >= STEPS.len() {
                return invalid(o, "mode or step out of range".as_bytes());
            }
            if let Err(m) = crate::term::cmd::check(&c) {
                return invalid(o, m.as_bytes());
            }
            if crate::term::radio_channel(app) == Some(idx) {
                return Status::Busy; // open in the radio's editor: the radio wins (TERM-17)
            }
            if st.get(idx) != Some(c) {
                app.put_channel_queued(c, st);
            }
            Status::Ok
        }
        // LINK-53: a position (8 bytes) or none (0), checked as the terminal's `pos=` is (CHAN-2: within 90 and 180
        // degrees); it goes to the store's queue behind the channel writes before it, so it lands on the channel as
        // they leave it and shares their sector's rewrite (decision 227). No channel at that number: nothing changes.
        what::POSITION if idx < MAX_CHANNELS => {
            let pos = match *rec {
                [] => None,
                [a, b, c, d, e, f, g, h] => {
                    let (lat, lon) = (i32::from_le_bytes([a, b, c, d]), i32::from_le_bytes([e, f, g, h]));
                    if lat.unsigned_abs() > 900_000_000 || lon.unsigned_abs() > 1_800_000_000 {
                        return invalid(o, "position: latitude -90 to 90, longitude -180 to 180".as_bytes());
                    }
                    Some(crate::data::Pos { lat, lon })
                }
                _ => return Status::BadArg,
            };
            if st.position(idx) != pos || st.data().busy() {
                app.put_pos_queued(idx, pos, st);
            }
            Status::Ok
        }
        what::ZONE if idx <= st.zones() && idx < crate::chan::MAX_ZONES => {
            let mut r = Rd { b: rec, n: 0 };
            let mut name = [0u8; 16];
            if !r.take(&mut name) {
                return Status::BadArg;
            }
            let mut c = ZoneCfg::default();
            for v in [&mut c.scan_all, &mut c.shown, &mut c.by_distance] {
                v.load(&mut r);
            }
            c.resume.load(&mut r);
            c.dwell.load(&mut r);
            0u8.load(&mut r); // reserved
            c.pri.load(&mut r);
            let Some(nm) = name_of::<16>(&name).filter(|n| !n.as_bytes().is_empty()) else { return invalid(o, "name: 1 to 16 printable characters".as_bytes()) };
            if c.resume > 12 {
                return invalid(o, "resume out of range".as_bytes());
            }
            request(st, Op::PutZone(idx, nm, c))
        }
        what::PRESET if idx <= st.data().presets() as u16 && idx < 16 => {
            let mut r = Rd { b: rec, n: 0 };
            let mut name = [0u8; 12];
            if !r.take(&mut name) {
                return Status::BadArg;
            }
            let Some(nm) = name_of::<12>(&name) else { return invalid(o, "name: printable ASCII only".as_bytes()) };
            let mut p = RangePreset { name: nm, start: 0, end: 0, step: 0, mode: 0, narrow: true, skips: 0, resume: 0 };
            p.start.load(&mut r);
            p.end.load(&mut r);
            p.step.load(&mut r);
            p.mode.load(&mut r);
            p.narrow.load(&mut r);
            p.resume.load(&mut r);
            if p.resume > 12 {
                return invalid(o, "resume out of range".as_bytes());
            }
            if !(mhz(18)..=mhz(1300)).contains(&p.start) || p.end <= p.start || p.end > mhz(1300) {
                return invalid(o, "start and end: 18 to 1300 MHz, end above start".as_bytes());
            }
            if p.step as usize >= STEPS.len() || !crate::slot::slot_mode_ok(p.mode) {
                return invalid(o, "step or mode out of range".as_bytes());
            }
            if let Some(old) = st.data().preset(idx as u8) {
                p.skips = old.skips;
                if old == p {
                    return Status::Ok;
                }
            }
            request(st, Op::PutPreset(idx as u8, p))
        }
        what::ZONE | what::PRESET => Status::NotFound,
        // APRS-42: added or renamed by callsign (the index is ignored), deleted with flag bit 0
        what::CONTACT => {
            if rec.len() != 23 {
                return Status::BadArg;
            }
            let call = &rec[..rec[..10].iter().position(|&c| c == 0).unwrap_or(10)];
            if call.len() < 3 || call.len() > 9 || !call.iter().all(|&c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'-') {
                return invalid(o, "callsign: capitals, digits and -, 3 to 9".as_bytes());
            }
            let call = Buf::<10>::from(call);
            if rec[22] & 1 != 0 {
                return if st.data().contact_of(call.as_bytes()).is_some() { request(st, Op::DeleteContact(call)) } else { Status::NotFound };
            }
            let mut name = Buf::<12>::new();
            for &c in rec[10..22].iter().take_while(|&&c| c != 0) {
                name.push(&[ferrite_core::name::map_name(c)]); // CHAN-31's set (decision 221)
            }
            let c = crate::data::Contact { call, name };
            if st.data().contact_of(call.as_bytes()) == Some(c) {
                return Status::Ok;
            }
            if st.data().contacts() >= crate::screens::contacts::CONTACT_MAX && st.data().contact_of(call.as_bytes()).is_none() {
                return invalid(o, "1016 contacts".as_bytes());
            }
            request(st, Op::SaveContact(c))
        }
        what::QUICK | what::WAYPOINT | what::DTMF | what::PRESET_SKIPS | what::SAT | what::SAT_INFO => lists::apply(st, w, idx, rec, o), // LINK-54 – LINK-56, LINK-58, LINK-63, LINK-64
        _ => Status::Refused, // nothing else is written over the link (LINK-3)
    }
}

pub mod lists;
pub mod state;
mod sub;
pub mod z;
/// LINK-43: the APRS record's length, for Ferrite Console (which reads its layout from this crate).
pub use sub::{APRS_REC, APRS_REPLY_AT, APRS_TEXT_AT};

#[cfg(test)]
pub(crate) mod tests;
