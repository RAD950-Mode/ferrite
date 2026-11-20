//! The terminal against a small VT100 model: what a terminal would show after the bytes it was sent.

use ufmt::uwrite;
use super::*;
use crate::sample::Sample;
use crate::chan::FLAG_PRIORITY;
use crate::screens::advanced::GROUPS;
use ferrite_core::msg::GpsState;
use ferrite_core::freq::mhz;
use std::string::String;
use std::vec;
use std::vec::Vec;

/// A VT100 screen: the characters, which cells are reverse video, the cursor; plus every byte, and how much
/// the sink takes at once.
struct Vt {
    cols: usize,
    rows: usize,
    cells: Vec<Vec<u8>>,
    rev: Vec<Vec<bool>>,
    cur: (usize, usize),
    saved: (usize, usize),
    attr_rev: bool,
    raw: Vec<u8>,
    room: usize,
    asked_size: bool,
}

impl Vt {
    fn new(cols: usize, rows: usize) -> Self {
        Vt {
            cols,
            rows,
            cells: vec![vec![b' '; cols]; rows],
            rev: vec![vec![false; cols]; rows],
            cur: (0, 0),
            saved: (0, 0),
            attr_rev: false,
            raw: Vec::new(),
            room: 1 << 20,
            asked_size: false,
        }
    }

    fn line(&self, r: usize) -> String {
        String::from_utf8_lossy(&self.cells[r]).trim_end().into()
    }

    fn text(&self) -> String {
        (0..self.rows).map(|r| self.line(r)).collect::<Vec<_>>().join("\n")
    }

    fn has(&self, s: &str) -> bool {
        self.text().contains(s)
    }

    /// The row holding `s`.
    fn row_of(&self, s: &str) -> Option<usize> {
        (0..self.rows).find(|&r| self.line(r).contains(s))
    }

    fn apply(&mut self, b: &[u8]) {
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if c == 0x1B {
                let rest = &b[i + 1..];
                match rest.first() {
                    Some(b'7') => {
                        self.saved = self.cur;
                        i += 2;
                        continue;
                    }
                    Some(b'8') => {
                        self.cur = self.saved;
                        i += 2;
                        continue;
                    }
                    Some(b'[') => {
                        let end = rest.iter().position(|c| c.is_ascii_alphabetic()).expect("a CSI final byte");
                        let params = core::str::from_utf8(&rest[1..end]).unwrap();
                        let n: Vec<usize> = params.split(';').map(|p| p.parse().unwrap_or(0)).collect();
                        match rest[end] {
                            b'H' => {
                                let r = n.first().copied().unwrap_or(1).max(1);
                                let c = n.get(1).copied().unwrap_or(1).max(1);
                                self.cur = ((r - 1).min(self.rows - 1), (c - 1).min(self.cols - 1));
                            }
                            b'J' => {
                                for r in 0..self.rows {
                                    self.cells[r].fill(b' ');
                                    self.rev[r].fill(false);
                                }
                            }
                            b'K' => {
                                let (r, c) = self.cur;
                                if c < self.cols {
                                    self.cells[r][c..].fill(b' ');
                                    self.rev[r][c..].fill(false);
                                }
                            }
                            b'm' => {
                                for p in &n {
                                    match p {
                                        0 => self.attr_rev = false,
                                        7 => self.attr_rev = true,
                                        _ => {}
                                    }
                                }
                            }
                            b'n' => self.asked_size = true,
                            other => panic!("unexpected CSI {}", other as char),
                        }
                        i += 2 + end;
                        continue;
                    }
                    _ => panic!("unexpected ESC sequence"),
                }
            }
            match c {
                b'\r' => self.cur.1 = 0,
                b'\n' => self.cur.0 = (self.cur.0 + 1).min(self.rows - 1),
                0x20..=0x7E => {
                    let (r, col) = self.cur;
                    if col < self.cols {
                        self.cells[r][col] = c;
                        self.rev[r][col] = self.attr_rev;
                        self.cur.1 += 1;
                    }
                }
                _ => panic!("unexpected byte {c:#x}"),
            }
            i += 1;
        }
    }
}

impl Sink for Vt {
    fn room(&self) -> usize {
        self.room
    }
    fn put(&mut self, b: &[u8]) {
        assert!(b.len() <= self.room, "the sink was overfilled");
        self.room -= b.len();
        self.raw.extend_from_slice(b);
        self.apply(b);
    }
}

/// A store the rig can hand channel writes to.
trait Apply {
    fn apply(&self, w: ChanWrite);
}

impl Apply for Sample {
    fn apply(&self, w: ChanWrite) {
        Sample::apply(self, w);
    }
}

struct Rig<S = Sample> {
    app: App,
    store: S,
    term: Term,
    vt: Vt,
    dev: Device,
    live: LiveAll,
    now: u32,
}

impl Rig<Sample> {
    fn new() -> Self {
        Rig::with(Sample::new())
    }

    /// Started and on the main menu.
    fn main() -> Self {
        let mut r = Rig::new();
        r.send(b"\r\r");
        assert_eq!(r.term.page(), Page::Main);
        r
    }
}

impl<S: ChannelStore + Apply> Rig<S> {
    fn with(store: S) -> Self {
        let mut app = App::new(crate::tests::set(), crate::tests::alert());
        while app.pop_cmd().is_some() {}
        let dev = Device { battery_mv: 7900, clock_min: Some(14 * 60 + 32), ..Default::default() };
        Rig { app, store, term: Term::new(), vt: Vt::new(80, 24), dev, live: LiveAll::default(), now: 1000 }
    }

    fn send(&mut self, b: &[u8]) {
        for &c in b {
            // A whole line waits for the next poll to run it (TERM-13): the cable holds the rest meanwhile.
            while !self.term.ready() {
                self.poll();
            }
            self.term.feed(&mut self.app, c, self.now, &self.store, &self.dev, &self.live);
            while let Some(w) = self.app.pop_write() {
                self.store.apply(w);
            }
        }
        self.poll();
    }

    fn poll(&mut self) {
        self.now += 100;
        self.term.poll(&mut self.app, &self.store, &self.dev, &self.live, self.now, &mut self.vt);
        while let Some(w) = self.app.pop_write() {
            self.store.apply(w);
        }
        while self.app.pop_cmd().is_some() {}
    }

    /// A command typed after `:`, run.
    fn cmd(&mut self, c: &str) {
        self.send(b":");
        self.send(c.as_bytes());
        self.send(b"\r");
        self.poll();
        self.poll();
    }

    /// <- -> (or Enter) on the settings item the term has selected, through the program (TERM-7).
    fn settings_change(&mut self, d: i32) {
        self.term.go(Page::Settings);
        let _ = self.term.call(tpl::W_SETTINGS_SCHANGE, d, &mut self.app, &self.store, &self.dev, &self.live);
    }

    fn bottom(&self) -> String {
        self.vt.line(self.vt.rows - 1)
    }

    /// Everything after the console started (the raw bytes as text).
    fn raw(&self) -> String {
        String::from_utf8_lossy(&self.vt.raw).into()
    }

    /// `:load`, the text line by line (CR LF), then polls until the console is done.
    fn load(&mut self, text: &str) {
        self.cmd("load");
        assert!(matches!(self.term.con, Con::Run(Body::Load { .. })), "{:?}", self.term.con);
        for l in text.lines() {
            self.send(l.as_bytes());
            self.send(b"\r\n");
        }
        for _ in 0..8 {
            self.poll();
        }
        assert!(matches!(self.term.con, Con::Done | Con::Off), "the load ended: {:?}", self.term.con);
    }
}

/// The dump as lines.
fn dump<S: ChannelStore>(app: &App, st: &S, dev: &Device) -> Vec<String> {
    let mut p = cmd::DumpPos::default();
    let mut l = cmd::Line::new();
    let mut v = Vec::new();
    while cmd::dump_line(app, st, dev, &mut p, &mut l) {
        v.push(String::from_utf8_lossy(l.as_bytes()).into());
    }
    v
}

/// TERM-2: in auto nothing answers until a typed Enter; then the banner (TERM-20) under the top line (TERM-11).
#[test]
fn auto_starts_on_enter_with_the_banner() {
    let mut r = Rig::new();
    r.send(b"hello");
    assert_eq!(r.term.page(), Page::Off);
    assert!(r.vt.raw.is_empty(), "silent until Enter");
    r.send(b"\r");
    assert_eq!(r.term.page(), Page::Splash);
    assert!(r.vt.asked_size, "asks the terminal's size (TERM-15)");
    let top = r.vt.line(0);
    assert!(top.starts_with(" Ferrite 0.9.3") && top.contains("RT-950 Pro") && top.contains("cable 115200"), "{top}");
    assert!(top.ends_with("14:32"), "the clock at the far right: {top}");
    assert!(r.vt.rev[0].iter().all(|&v| v), "the top line is reverse video, full width");
    assert!(r.vt.has("FERRITE 0.9.3 - free firmware for the RT-950 Pro"));
    assert!(r.vt.has("Press Enter for the menu, ? for help"));
    assert!(r.vt.line(23).contains("Enter Menu"), "keys on the bottom line");
    // No ASCII art: nothing but the banner's three lines in the body.
    let body: Vec<String> = (1..23).map(|i| r.vt.line(i)).filter(|l| !l.is_empty()).collect();
    assert_eq!(body.len(), 3, "{body:?}");
}

/// Link and KISS never start the terminal; Terminal starts it on any byte.
#[test]
fn cable_port_setting_decides() {
    let mut r = Rig::new();
    r.app.set.cable_port = 1;
    r.send(b"\r\r");
    assert_eq!(r.term.page(), Page::Off);
    r.app.set.cable_port = 3;
    r.send(b"\r");
    assert_eq!(r.term.page(), Page::Off);
    r.app.set.cable_port = 2;
    r.send(b"x");
    assert_eq!(r.term.page(), Page::Splash);
}

/// The main menu with the radio's state; digits jump; Esc back; Ctrl-C leaves and waits for Enter again.
#[test]
fn main_menu_and_leaving() {
    let mut r = Rig::main();
    assert!(r.vt.has("Ferrite terminal"));
    let n = r.store.count();
    assert!(r.vt.has(&std::format!("2  Channels      {n} used of 4000")), "{}", r.vt.text());
    assert!(r.vt.has("8  About"));
    assert!(r.vt.has("Radio now"));
    assert!(r.vt.has("Battery 7.9 V"));
    let sel = r.vt.row_of("1  Radio").unwrap();
    assert!(r.vt.rev[sel][2], "the selection is reverse video");
    r.send(b"\x1b[B"); // down
    let z = r.vt.row_of("2  Channels").unwrap();
    assert!(r.vt.rev[z][2] && !r.vt.rev[sel][2]);
    r.send(b"9");
    assert_eq!(r.term.page(), Page::Main, "9: no ninth entry since Remote went");
    r.send(b"8");
    assert_eq!(r.term.page(), Page::About);
    r.send(b"\x1b");
    r.now += 100; // a lone ESC is Esc after a moment
    r.poll();
    assert_eq!(r.term.page(), Page::Main);
    r.send(b"\x03");
    assert_eq!(r.term.page(), Page::Off);
    assert!(r.vt.has("Ferrite terminal closed"));
    r.send(b"\r");
    assert_eq!(r.term.page(), Page::Splash);
}

/// TERM-19: nothing is sent again while nothing changed; Ctrl-L sends it all (TERM-16).
#[test]
fn only_changed_rows_are_sent() {
    let mut r = Rig::main();
    let before = r.vt.raw.len();
    r.now += 2000; // the refresh checks every row
    r.poll();
    assert_eq!(r.vt.raw.len(), before, "unchanged rows are not resent");
    r.dev.clock_min = Some(14 * 60 + 33);
    r.now += 2000;
    r.poll();
    let sent = r.vt.raw.len() - before;
    assert!(sent > 0 && sent < 200, "only the top line: {sent} bytes");
    assert!(r.vt.line(0).ends_with("14:33"));
    r.send(b"\x0c");
    assert!(r.vt.raw.len() - before > 1000, "Ctrl-L redraws everything");
}

/// TERM-47: a run reads its code through a small cache of lines, not the whole page. Every page's run uses at
/// most all but four lines of the cache (so a redraw reads each line about once, not again and again), and a full
/// redraw (Ctrl-L) reads few lines from the SPI flash (each about 30 us at 15 MHz).
#[test]
fn code_read_through_a_cache() {
    let mut r = Rig::main();
    let fills = || tpl::FILLS.with(|f| f.get());
    for p in super::pages::PAGES {
        for help in [false, true] {
            r.term.go(p);
            if help {
                r.send(b"?");
            }
            tpl::PEAK.with(|p| p.set(0));
            let n = fills();
            r.send(b"\x0c");
            let (draw, peak) = (fills() - n, tpl::PEAK.with(|p| p.get()) as usize);
            std::println!("{p:?}{}: a full redraw read {draw} lines; a run used at most {peak}", if help { " help" } else { "" });
            assert!(peak <= tpl::LINES - 4, "{p:?}: a run uses {peak} lines of {}", tpl::LINES);
            assert!(draw <= 100, "{p:?}: {draw} lines read");
        }
    }
}

/// TERM-4.1: a full sink holds rows back; later polls finish the page.
#[test]
fn never_overfills_the_sink() {
    let mut r = Rig::new();
    r.vt.room = 300;
    r.send(b"\r");
    for _ in 0..50 {
        r.vt.room = 300;
        r.poll();
    }
    assert!(!r.term.busy());
    assert!(r.vt.has("Press Enter for the menu"));
}

/// TERM-15: a larger terminal is used whole; the reply to the size question sets it.
#[test]
fn larger_terminal_from_the_size_report() {
    let mut r = Rig::new();
    r.vt = Vt::new(120, 40);
    r.send(b"\r");
    r.send(b"\x1b[40;120R");
    assert_eq!(r.term.size(), (120, 40));
    assert!(r.vt.line(0).ends_with("14:32") && r.vt.line(0).len() > 100, "{}", r.vt.line(0));
    assert!(r.vt.line(39).contains("Enter Menu"), "the bottom line on the last row");
    // Smaller than 80 x 24 counts as 80 x 24.
    r.send(b"\x1b[10;40R");
    assert_eq!(r.term.size(), (80, 24));
}

/// Help on ? and PF1, closed by Esc.
#[test]
fn help() {
    let mut r = Rig::main();
    r.send(b"?");
    assert!(r.vt.has("Help") && r.vt.has("Ctrl-L redraws"));
    r.send(b"?");
    assert!(!r.vt.has("Ctrl-L redraws"));
    r.send(b"\x1bOP"); // PF1
    assert!(r.vt.has("Ctrl-L redraws"));
}

/// Channels: the list with paging, then the editor; changes go through the App (TERM-6, TERM-9) and the
/// radio's slot on that channel follows.
#[test]
fn channels_list_and_edit() {
    let mut r = Rig::main();
    r.send(b"2");
    assert_eq!(r.term.page(), Page::Channels);
    assert!(r.vt.has("zone All channels"));
    assert!(r.vt.has("No") && r.vt.has("RX MHz") && r.vt.has("Flags"));
    let first = r.store.by_number(0).unwrap();
    let mut name = String::from_utf8_lossy(first.name()).into_owned();
    name.truncate(16);
    assert!(r.vt.has(&name), "{}", r.vt.text());
    // PgDn moves a page; Home comes back.
    r.send(b"\x1b[6~");
    assert!(r.term.chan_sel() > 0);
    r.send(b"\x1b[H");
    assert_eq!(r.term.chan_sel(), 0);
    // Edit the first channel: power up one step.
    r.send(b"\r");
    assert_eq!(r.term.page(), Page::Edit);
    assert!(r.vt.has(&std::format!("Channels > {} ", first.num + 1)));
    for _ in 0..8 {
        r.send(b"\x1b[B"); // down to Power (past Offset and TX)
    }
    assert_eq!(FIELDS[r.term.field() as usize], F::It(Item::Power));
    r.send(b"\x1b[C");
    let after = r.store.by_num(first.num).unwrap().0;
    assert_eq!(after.cfg.power, (first.cfg.power + 1) % 3, "saved in the store");
    assert!(r.vt.has("Saved"));
}

/// A typed frequency is checked (CHAN-22, TERM-7); a typed name is saved; Esc cancels typing.
#[test]
fn typed_values() {
    let mut r = Rig::main();
    r.send(b"2\r");
    let num = r.term.edit().unwrap().num;
    // Name: field 0.
    r.send(b"\r");
    for _ in 0..20 {
        r.send(b"\x7f");
    }
    r.send(b"Test name\r");
    assert_eq!(r.store.by_num(num).unwrap().0.name(), b"Test name");
    // RX: field 2.
    r.send(b"\x1b[B\x1b[B");
    r.send(b"\r700\r");
    assert!(r.vt.has("756-1300 MHz"), "refused");
    r.send(b"\r146.52\r");
    assert_eq!(r.store.by_num(num).unwrap().0.rx, 14_652_000);
    assert!(r.vt.has("146.52000 MHz"));
    r.send(b"\r147");
    r.send(b"\x1b");
    r.now += 100;
    r.poll();
    assert_eq!(r.store.by_num(num).unwrap().0.rx, 14_652_000, "Esc cancels typing");
    assert_eq!(r.term.page(), Page::Edit);
}

/// A slot on the edited channel follows at once (TERM-9): the radio's screen shows the change.
#[test]
fn the_radio_follows_an_edit() {
    let mut r = Rig::main();
    // Put slot A on the first channel, as the radio would.
    let first = r.store.by_number(0).unwrap();
    {
        let s = &mut r.app.slots[0];
        (s.ch, s.zone, s.vfo_mode) = (Some(first), first.zone, false);
    }
    while r.app.pop_cmd().is_some() {}
    r.send(b"2\r");
    for _ in 0..9 {
        r.send(b"\x1b[B"); // Squelch
    }
    assert_eq!(FIELDS[r.term.field() as usize], F::It(Item::Squelch));
    r.send(b"\x1b[C");
    let next = if first.cfg.squelch >= 9 { crate::slot::SQ_GLOBAL } else { first.cfg.squelch + 1 }; // 9, then Global
    assert_eq!(r.app.slot(0).cfg().squelch, next, "the slot's copy follows");
}

/// Zones: every channel, each zone, no zone; Enter lists that zone's channels and Esc comes back.
#[test]
fn zones() {
    let mut r = Rig::main();
    r.send(b"3");
    assert_eq!(r.term.page(), Page::Zones);
    assert!(r.vt.has("All channels"));
    let z0 = r.store.zone_name(0).unwrap();
    assert!(r.vt.has(core::str::from_utf8(z0.as_bytes()).unwrap()));
    r.send(b"\x1b[B\r");
    assert_eq!(r.term.page(), Page::Channels);
    assert_eq!(r.term.zone(), 0);
    r.send(b"\x1b");
    r.now += 100;
    r.poll();
    assert_eq!(r.term.page(), Page::Zones);
}

/// Settings: the radio's groups and numbers; a wired setting changes the App (TERM-6); the region and PIN are
/// left to the radio (TERM-7).
#[test]
fn settings_same_numbers_and_validation() {
    let mut r = Rig::main();
    r.send(b"4");
    assert!(r.vt.has("1  Watch and scan") && r.vt.has("9  System"));
    // 0 7 1: theme.
    r.send(b"7");
    assert!(r.vt.has("Settings > 0 7 Display, sound, power"));
    assert!(r.vt.has("Path on the radio: MENU 0 7 1"));
    let theme = r.app.set.theme;
    r.send(b"\x1b[C");
    assert_eq!(r.app.set.theme, (theme + 1) % crate::settings::THEMES.len() as u8, "changed in the App");
    assert!(r.vt.has("* "), "marked as not the default");
    r.send(b"#");
    assert_eq!(r.app.set.theme, theme);
    // 0 9 4: the region stays on the radio.
    r.send(b"\x1b");
    r.now += 100;
    r.poll();
    r.send(b"9");
    let region = r.app.set.region;
    for _ in 0..3 {
        r.send(b"\x1b[B");
    }
    r.send(b"\x1b[C");
    assert_eq!(r.app.set.region, region);
    assert!(r.vt.has("Change this one on the radio"));
    // 0 9 2 3: the cable port itself.
    r.send(b"\x1b[A\x1b[A\r");
    assert!(r.vt.has("Cable port"));
    for _ in 0..2 {
        r.send(b"\x1b[B");
    }
    r.send(b"\x1b[C");
    assert_eq!(r.app.set.cable_port, 1);
    r.send(b"#");
    assert_eq!(r.app.set.cable_port, 0);
}

/// Monitor: the event log's decodes one line each, newest at the bottom, and a slot receiving now (TERM-5).
#[test]
fn monitor() {
    let mut r = Rig::main();
    r.live.slots[1].squelch_open = true;
    r.live.slots[1].freq = 44_347_500;
    r.live.slots[1].rssi = -80;
    r.send(b"7");
    assert_eq!(r.term.page(), Page::Monitor);
    assert!(r.vt.has("-- live --"));
    let newest = r.store.data().event(0).unwrap();
    let now_row = r.vt.row_of("B 443.475").expect("the slot receiving now");
    let ev_row = r.vt.row_of(core::str::from_utf8(newest.src.name().as_bytes()).unwrap()).unwrap();
    assert!(ev_row < now_row, "events above the live slots");
    assert!(r.vt.line(now_row).trim_start().starts_with("now"));
}

/// The main page's slot lines show the APRS slot as APRS on the APRS frequency, not the channel it keeps
/// underneath (daily use, Oct 29: clear in APRS mode read as on an FRS channel).
#[test]
fn radio_now_shows_the_aprs_slot() {
    let mut r = Rig::main();
    r.app.set_aprs_mode(true);
    for _ in 0..30 {
        r.poll();
    }
    let f = super::mhz5(r.app.aprs_freq());
    let f = core::str::from_utf8(f.as_bytes()).unwrap().trim();
    let line = r.vt.line(r.vt.row_of("C  APRS").expect("an APRS slot line"));
    assert!(line.trim_start().starts_with("C  APRS"), "{line}");
    assert!(line.contains(f), "{line}");
}

#[test]
fn frequencies() {
    assert_eq!(parse_mhz(b"146.52"), Some(14_652_000));
    assert_eq!(parse_mhz(b"446"), Some(44_600_000));
    assert_eq!(parse_mhz(b"146.520 00"), Some(14_652_000));
    assert_eq!(parse_mhz(b"146.5205"), Some(14_652_050));
    assert_eq!(parse_mhz(b"abc"), None);
    assert_eq!(parse_mhz(b""), None);
    assert_eq!(mhz5(14_652_000).as_bytes(), b"146.52000");
    assert_eq!(mhz5(44_347_500).as_bytes(), b"443.47500");
}

/// The state is small: no screen buffer (TERM-19); the command line's 255-byte buffer (TERM-12), a debug
/// command's one answer (64 bytes of memory at most, TERM-22, or a GGA sentence of 80, TERM-30: about 100 bytes)
/// and the channel list's tools (TERM-14: marks, a bit per channel, 125 bytes; a sorted page of up to 56 channel
/// numbers, 112 bytes; Oct 6, 2026) on top. The GGA answer added 16 bytes (Oct 13, 2026); 4,000 channels' marks
/// 375 more (decision 333); `sat add`'s paste 64 more (a name and a TLE's line 1 as read, SAT-3; decision 340); the
/// pages or the plain command line, chosen at start by whether the template part is there, 16 more (decision 335);
/// a typed value as long as an APRS message (67 characters, TERM-49) 44 more. Bench builds 32 more: their debug
/// commands' kinds (a memory read's address and length, 20 bytes) and `bk dump`'s row of eight registers (16), less
/// 4 of padding (never counted: the release CI's bench test run would have failed, found Nov 4, 2026).
#[test]
fn small() {
    let n = core::mem::size_of::<Term>();
    std::eprintln!("Term is {n} bytes");
    let bench = if cfg!(feature = "bench") { 32 } else { 0 };
    assert!(n <= 512 + 256 + 112 + 304 + 16 + 375 + 64 + 16 + 44 + bench, "Term is {n} bytes");
}

// ---- The command line and dump/load (TERM-12, TERM-13) ----

use crate::chan::{ChanWrite, Name, FLAG_APRS, FLAG_RX_ONLY, FLAG_SKIP};
use crate::data::{Data, Op, RangePreset, ZoneCfg};
use crate::slot::{SlotCfg, CTCSS, DCS, DCS_FLAG, DCS_INV, DEFAULT_CFG};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

/// A blank radio's store: what a load fills in.
#[derive(Default)]
struct Blank {
    chans: RefCell<BTreeMap<u16, Channel>>,
    zones: RefCell<Vec<(Name, ZoneCfg)>>,
    presets: RefCell<Vec<RangePreset>>,
    busy: Cell<bool>,
    /// Channel positions (CHAN-2), as `ch N pos=` sets them.
    pos: RefCell<BTreeMap<u16, crate::data::Pos>>,
}

impl Apply for Blank {
    fn apply(&self, w: ChanWrite) {
        let mut c = self.chans.borrow_mut();
        match w {
            ChanWrite::Put { ch, from } => {
                if let Some(f) = from {
                    c.remove(&f);
                }
                c.insert(ch.num, ch);
            }
            ChanWrite::PutPos { ch, pos } => {
                c.insert(ch.num, ch);
                match pos {
                    Some(p) => self.pos.borrow_mut().insert(ch.num, p),
                    None => self.pos.borrow_mut().remove(&ch.num),
                };
            }
            ChanWrite::Pos { num, pos } if c.contains_key(&num) => {
                match pos {
                    Some(p) => self.pos.borrow_mut().insert(num, p),
                    None => self.pos.borrow_mut().remove(&num),
                };
            }
            ChanWrite::Pos { .. } => {}
            ChanWrite::Delete(n) => {
                c.remove(&n);
            }
            ChanWrite::DeleteZone(z) => c.values_mut().filter(|x| x.zone == z).for_each(|x| x.zone = NO_ZONE),
            ChanWrite::EraseAll => c.clear(),
        }
    }
}

impl ChannelStore for Blank {
    fn zones(&self) -> crate::chan::ZoneId {
        self.zones.borrow().len() as crate::chan::ZoneId
    }
    fn zone_name(&self, zone: crate::chan::ZoneId) -> Option<Name> {
        self.zones.borrow().get(zone as usize).map(|z| z.0)
    }
    fn zone_len(&self, zone: crate::chan::ZoneId) -> u16 {
        self.chans.borrow().values().filter(|c| c.zone == zone).count() as u16
    }
    fn zone_at(&self, zone: crate::chan::ZoneId, i: u16) -> Option<Channel> {
        self.chans.borrow().values().filter(|c| c.zone == zone).nth(i as usize).copied()
    }
    fn count(&self) -> u16 {
        self.chans.borrow().len() as u16
    }
    fn by_num(&self, num: u16) -> Option<(Channel, u16)> {
        let c = *self.chans.borrow().get(&num)?;
        let pos = self.chans.borrow().values().filter(|x| x.zone == c.zone && x.num < num).count() as u16;
        Some((c, pos))
    }
    fn by_number(&self, i: u16) -> Option<Channel> {
        self.chans.borrow().values().nth(i as usize).copied()
    }
    fn position(&self, num: u16) -> Option<crate::data::Pos> {
        self.chans.borrow().contains_key(&num).then(|| self.pos.borrow().get(&num).copied()).flatten()
    }
    fn data(&self) -> &dyn Data {
        self
    }
}

impl Data for Blank {
    fn presets(&self) -> u8 {
        self.presets.borrow().len() as u8
    }
    fn preset(&self, i: u8) -> Option<RangePreset> {
        self.presets.borrow().get(i as usize).copied()
    }
    fn zone_cfg(&self, zone: crate::chan::ZoneId) -> ZoneCfg {
        self.zones.borrow().get(zone as usize).map_or(ZoneCfg { shown: true, ..ZoneCfg::default() }, |z| z.1)
    }
    fn request(&self, op: Op) -> bool {
        match op {
            Op::PutZone(z, name, cfg) => {
                let mut zs = self.zones.borrow_mut();
                match (z as usize).cmp(&zs.len()) {
                    core::cmp::Ordering::Less => zs[z as usize] = (name, cfg),
                    core::cmp::Ordering::Equal if zs.len() < 254 => zs.push((name, cfg)),
                    _ => return false,
                }
                true
            }
            Op::PutPreset(i, p) => {
                let mut ps = self.presets.borrow_mut();
                let n = ps.len();
                if let Some(o) = ps.get_mut(i as usize) {
                    *o = RangePreset { skips: o.skips, ..p };
                } else if i as usize == n && n < 16 {
                    ps.push(RangePreset { skips: 0, ..p });
                } else {
                    return false;
                }
                true
            }
            Op::DeleteChannel(n) => self.chans.borrow_mut().remove(&n).is_some(),
            _ => false,
        }
    }
    fn busy(&self) -> bool {
        self.busy.get()
    }
}

/// TERM-12, TERM-12.1: `:` opens the command line on the bottom row; set and get use the menu's numbers and values;
/// a page row reads as a page.
#[test]
fn command_line_set_and_get() {
    let mut r = Rig::main();
    r.send(b":set 0 7");
    assert!(r.bottom().starts_with(" :set 0 7_"), "{}", r.bottom());
    r.send(b" 1 night red\r");
    r.poll();
    assert_eq!(r.app.set.theme, 2, "set by the value the radio shows, any case");
    assert!(r.bottom().contains("0 7 1 Theme: Night red"), "{}", r.bottom());
    // TERM-12.1: a row that is a page, not a setting, says so rather than an empty value.
    r.cmd("get 0 5 1");
    assert!(r.bottom().contains("0 5 1 Heard: a page on the radio, not a setting"), "{}", r.bottom());
    r.cmd("get 0 7 1");
    assert!(r.bottom().contains("Theme: Night red (not the default)"), "{}", r.bottom());
    // Up brings the last command back; Esc closes the line without running it.
    r.send(b":\x1b[A");
    assert!(r.bottom().contains(":get 0 7 1_"), "{}", r.bottom());
    r.send(b"\x1b");
    r.now += 100;
    r.poll();
    assert!(!r.term.cmd);
    // Third-level numbers and values with spaces.
    r.cmd("set 0 2 6 1 200 kHz");
    assert_eq!(r.app.set.fm_step, 2);
    r.cmd("set 0 1 1 off");
    assert_eq!(r.app.watch, 0);
    // WATCH-17.3 (decision 313): "A·B first" typed with a full stop for its middle dot
    r.cmd("set 0 5 9 7 A.B first");
    assert_eq!(r.app.settings().aprs_share, 1, "{}", r.bottom());
    r.cmd("set 0 5 9 7 APRS first");
    assert_eq!(r.app.settings().aprs_share, 0, "{}", r.bottom());
    // Typed values: the pager capcode (DEC-4), the custom APRS path (APRS-26), Popups (DEC-7).
    r.cmd("set 0 4 2 3 1234567");
    assert_eq!(r.app.set.pocsag_cap, 1_234_567);
    r.cmd("set 0 4 2 3 none");
    assert_eq!(r.app.set.pocsag_cap, 0);
    r.cmd("set 0 4 2 3 9999999");
    assert!(r.bottom().contains("a capcode up to 2097151"), "{}", r.bottom());
    r.cmd("set 0 5 9 2 wide2-2");
    assert_eq!(r.app.aprs_cfg().custom_text(), b"WIDE2-2");
    assert_eq!(r.app.aprs_cfg().path, crate::aprs::PATH_CUSTOM);
    r.cmd("set 0 5 9 2 WIDE1-1,WIDE2-1,WIDE3-1");
    assert!(r.bottom().contains("one or two entries"), "{}", r.bottom());
    r.cmd("set 0 5 9 2 None");
    assert!(r.app.aprs_cfg().custom_text().is_empty());
    r.cmd("set 0 4 4 off");
    assert!(!r.app.set.popups);
    // A wired value (the backlight timeout) is matched too, and a wrong one lists the choices and changes
    // nothing (on the radio, Oct 12, 2026, it listed them run together and left the timeout one step on).
    r.cmd("set 0 7 4 1 10 s");
    assert!(r.bottom().contains("Timeout: 10 s"), "{}", r.bottom());
    let bl = r.app.set.backlight;
    r.cmd("set 0 7 4 1 7 s");
    assert!(r.bottom().contains("one of 10 s, 15 s"), "{}", r.bottom());
    assert_eq!(r.app.set.backlight, bl, "a refused value changes nothing");
}

/// TERM-12, DISP-15.1, DISP-16.1, DISP-22, DISP-23, DISP-26, DISP-17.3: every light setting takes `set` by the value
/// the radio shows (0 7 4 1 to 0 7 4 5 and 0 7 6), for testing the lights over the cable. Brightness stops at 1
/// and 10, so `set` walks it both ways; a refused level lists 10 to 1 and changes nothing.
#[test]
fn set_every_light_setting() {
    let mut r = Rig::main();
    for (cmd, says) in [
        ("set 0 7 4 1 30 s", "0 7 4 1 Timeout: 30 s"),
        ("set 0 7 4 2 10", "0 7 4 2 Brightness: 10"),
        ("set 0 7 4 2 1", "0 7 4 2 Brightness: 1"),
        ("set 0 7 4 2 4", "0 7 4 2 Brightness: 4"),
        ("set 0 7 4 3 off", "0 7 4 3 On transmit: Off"),
        ("set 0 7 4 4 keys", "0 7 4 4 Wake on: Keys"),
        ("set 0 7 4 4 keys, rx and tx", "0 7 4 4 Wake on: Keys, RX and TX"),
        ("set 0 7 4 5 on key press", "0 7 4 5 Key light: On key press"),
        ("set 0 7 6 rx, tx, heartbeat", "0 7 6 LEDs: RX, TX, heartbeat"),
        ("set 0 7 6 rx and tx", "0 7 6 LEDs: RX and TX"),
    ] {
        r.cmd(cmd);
        assert!(r.bottom().contains(says), "{cmd}: {}", r.bottom());
    }
    let s = &r.app.set;
    assert_eq!((s.backlight, s.brightness, s.tx_light, s.wake_on, s.keypad_light, s.leds), (4, 4, false, 2, 2, 1));
    r.cmd("set 0 7 4 2 11");
    assert!(r.bottom().contains("one of 10, 9, 8, 7, 6, 5, 4, 3, 2, 1"), "{}", r.bottom());
    assert_eq!(r.app.set.brightness, 4, "a refused level changes nothing");
    // Night red: 0 7 4 2 is its own level.
    r.cmd("set 0 7 1 night red");
    r.cmd("set 0 7 4 2 2");
    assert_eq!((r.app.set.brightness_night, r.app.set.brightness), (2, 4));
}

/// TERM-12: a short form names a value when only one matches: a bare number as a whole word of the value
/// (Battery Save's "1" is "Level 1"; on the radio, Oct 13, 2026, it was refused), the start of a word-value;
/// a number still means the number (Brightness 1 is 1, never 10), and an ambiguous one lists the choices.
#[test]
fn set_short_forms() {
    let mut r = Rig::main();
    for (cmd, says) in [
        ("set 0 7 7 1 1", "0 7 7 1 Battery Save: Level 1"),
        ("set 0 7 7 1 3", "0 7 7 1 Battery Save: Level 3"),
        ("set 0 7 7 1 of", "0 7 7 1 Battery Save: Off"),
        ("set 0 7 7 1 level 2", "0 7 7 1 Battery Save: Level 2"),
        ("set 0 7 1 night", "0 7 1 Theme: Night red"),
        ("set 0 7 4 2 1", "0 7 4 2 Brightness: 1"),
        ("set 0 7 4 1 1", "0 7 4 1 Timeout: 1 min"),
        ("set 0 7 4 1 15", "0 7 4 1 Timeout: 15 s"),
    ] {
        r.cmd(cmd);
        assert!(r.bottom().contains(says), "{cmd}: {}", r.bottom());
    }
    assert_eq!(r.app.set.brightness_night, 1, "Night red has its own level");
    let (theme, bs) = (r.app.set.theme, r.app.bsave);
    for (cmd, says) in [
        ("set 0 7 1 d", "Theme: one of Night red, Cyberpunk, Dark, Daylight"),
        ("set 0 7 7 1 level", "Battery Save: one of"),
        ("set 0 7 7 1 4", "Battery Save: one of"),
        ("set 0 7 4 1 7", "Timeout: one of"),
    ] {
        r.cmd(cmd);
        assert!(r.bottom().contains(says), "{cmd}: {}", r.bottom());
    }
    assert_eq!((r.app.set.theme, r.app.bsave), (theme, bs), "a refused short form changes nothing");
}

/// TX-48, TX-48.2: `set` (typed, or a `term` line over the link) turns Dry TX on but never off, and `get` reads it;
/// Extend TX stays radio-only; a receive-only build refuses it.
#[test]
fn dry_tx_on_from_the_pc_never_off() {
    let mut r = Rig::main();
    r.app.radio.tx_built = true;
    for c in ["set 0 8 8 5 Off", "set 0 8 8 4 On"] {
        r.cmd(c);
        assert!(r.bottom().contains("only on the radio"), "{c}: {}", r.bottom());
        assert!(!r.app.dry_tx() && !r.app.set.extend_tx, "{c}");
    }
    r.cmd("set 0 8 8 5 On");
    assert!(r.bottom().contains("Dry TX: On"), "{}", r.bottom());
    assert!(r.app.dry_tx(), "on from the PC");
    r.cmd("set 0 8 8 5 Off");
    assert!(r.bottom().contains("Dry TX goes off only on the radio"), "{}", r.bottom());
    assert!(r.app.dry_tx(), "still on");
    r.cmd("get 0 8 8 5");
    assert!(r.bottom().contains("Dry TX: On"), "{}", r.bottom());
    r.cmd("get 0 8 8 4");
    assert!(r.bottom().contains("0 8 8 4 Extend TX: Off"), "{}", r.bottom());
    let mut rx = Rig::main();
    rx.app.radio.tx_built = false;
    rx.cmd("set 0 8 8 5 On");
    assert!(!rx.app.dry_tx() && rx.bottom().contains("only on the radio"), "{}", rx.bottom());
}

/// Every refusal says what was wrong and how to put it right; nothing changes.
#[test]
fn command_errors() {
    let mut r = Rig::main();
    let theme = r.app.set.theme;
    for (c, says) in [
        ("set 0 7 1 purple", "Theme: one of Dark, Daylight, Night red"),
        ("set 0 7 1", "set needs a value"),
        ("set 7 1 Dark", "a menu number from MENU"),
        ("set 0 9 4 EU", "changes only on the radio"),
        ("set 0 9 3 1 Off", "changes only on the radio"),
        ("frob", "frob: no such command"),
        ("ch 999 power high", "ch 999: no such channel"),
        ("ch 12 power ultra", "power: low, mid, high"),
        ("ch 12 tone 300.0", "300.0 Hz is outside 60.0 to 260.0 Hz"),
        ("ch 12 tone D028N", "a DCS code (D023N, D023I)"),
        ("ch 12 rx 700", "rx: 0.15-64 MHz (HF), 64-630 or 756-1300 MHz"),
        ("ch 12 rx 30", "an HF channel is AM, USB, LSB or CW"),
        ("ch 12 mode usb", "USB, LSB and CW are for HF channels"),
        ("ch 12 zone 9", "zone 9 doesn't exist"),
        ("ch 12 \"Name", "no closing quote"),
        ("ch 4001", "ch: a number from 1 to 4000"),
        ("zone 9 \"Far\"", "the radio has 6, so the next is zone 7"),
        ("clear channels 3", "type: clear channels 21"),
    ] {
        r.cmd(c);
        assert!(r.bottom().contains(says), "{c}: {}", r.bottom());
        assert!(r.term.warn, "{c}: shown as a refusal");
    }
    assert_eq!(r.app.set.theme, theme);
    assert_eq!(r.store.count(), 21);
}

/// `ch`: one field at a time, a whole channel, show, delete; 1.25 m takes any power (TERM-7, TX-9).
#[test]
fn command_channels() {
    let mut r = Rig::main();
    r.cmd("ch 12 power high");
    assert_eq!(r.store.by_num(11).unwrap().0.cfg.power, 2);
    r.cmd("ch 12 tone=D023I");
    let c = r.store.by_num(11).unwrap().0;
    assert_eq!((c.cfg.tx_tone, c.cfg.rx_tone), (DCS_FLAG | DCS_INV | 0o023, DCS_FLAG | DCS_INV | 0o023));
    r.cmd("ch 12 name \"Club \\\"north\\\"\"");
    assert_eq!(r.store.by_num(11).unwrap().0.name(), b"Club -north-"); // CHAN-31: '"' is not a name character
    r.cmd("ch 300 \"Test\" 146.6 offset=-0.6 tone=100.0 power=mid zone=1 skip squelch=6");
    let t = r.store.by_num(299).expect("made").0;
    assert_eq!((t.rx, t.cfg.offset, t.cfg.tx_tone, t.cfg.power, t.zone, t.flags & FLAG_SKIP, t.cfg.squelch), (14_660_000, -60_000, 1000, 1, 0, FLAG_SKIP, 6));
    // TX-9: 1.25 m takes any level, as stock.
    r.cmd("ch 301 \"Two20\" 223.5 power=high");
    assert_eq!(r.store.by_num(300).expect("made").0.cfg.power, 2);
    r.cmd("ch 301 power mid");
    assert_eq!(r.store.by_num(300).unwrap().0.cfg.power, 1);
    // Show: the console, then Enter goes back to the page.
    r.cmd("ch 300");
    assert!(r.raw().contains("ch 300 \"Test\" 146.60000 offset=-0.60000 mode=NFM tone=100.0 power=mid zone=1 squelch=6 skip"), "{}", r.raw());
    r.send(b"\r");
    assert_eq!(r.term.con, Con::Off);
    assert!(r.vt.has("Ferrite terminal"), "the page again");
    r.cmd("ch 300 delete");
    assert!(r.store.by_num(299).is_none());
    // Clearing every channel needs their count typed.
    let n = r.store.count();
    r.cmd(&std::format!("clear channels {n}"));
    assert_eq!(r.store.count(), 0);
}

/// Help and the changed settings on the console.
#[test]
fn command_lists() {
    let mut r = Rig::main();
    r.cmd("help");
    assert!(r.raw().contains("Commands: type : then one of these"));
    assert!(r.raw().contains("Enter or Esc returns to the menu."));
    r.send(b"\x1b");
    r.now += 100;
    r.poll();
    assert_eq!(r.term.con, Con::Off);
    r.cmd("set 0 7 1 Daylight");
    r.cmd("get");
    assert!(r.raw().contains("set 0 7 1 Daylight"), "{}", r.raw());
}

/// TERM-13: the dump starts with its version, has every section, and ends with `end`.
#[test]
fn dump_format() {
    let r = Rig::main();
    let d = dump(&r.app, &r.store, &r.dev);
    assert_eq!(d[0], "# Ferrite dump v1, firmware 0.9.3");
    for l in [
        "set 0 7 1 Dark                    # Theme",
        "set 0 6 6 24 h, GPS               # Clock",
        "zone 2 \"Repeaters\" scan=on shown=on distance=off resume=radio dwell=radio pri1=12",
        "preset 3 \"Airband\" 118.00000 137.00000 step=25kHz mode=AM resume=carrier",
        "ch 12 \"Club repeater\" 443.47500 offset=+5.00000 mode=NFM txtone=100.0 rxtone=off power=low zone=2 pos=45.4075605,-75.6628014",
        "ch 63 \"FRS 1\" 462.56250 offset=simplex mode=NFM tone=off power=low",
    ] {
        assert!(d.iter().any(|x| x == l), "missing: {l}");
    }
    assert_eq!(d.last().map(String::as_str), Some("end"));
    assert!(d.iter().all(|l| l.len() < 200));
    // The console streams it, the same lines.
    let mut r = Rig::main();
    r.cmd("dump");
    for _ in 0..4 {
        r.poll();
    }
    let raw = r.raw();
    for l in &d {
        assert!(raw.contains(&std::format!("{l}\r\n")), "streamed: {l}");
    }
}

/// TERM-13: dump, load on a blank radio, dump again: the same text.
#[test]
fn dump_load_round_trip() {
    let a = Rig::main();
    let text = dump(&a.app, &a.store, &a.dev).join("\n");
    let mut b = Rig::with(Blank::default());
    b.app.set.theme = 1; // a setting the dump puts back
    b.send(b"\r\r");
    b.load(&text);
    assert!(b.raw().contains(" 0 refused."), "{}", b.raw());
    assert_eq!(dump(&b.app, &b.store, &b.dev), dump(&a.app, &a.store, &a.dev));
}

/// A full radio: 1000 channels with every field in use, 30 zones, 16 presets; round trip through the terminal.
#[test]
fn dump_load_1000_channels() {
    let full = Blank::default();
    for z in 0..30u8 {
        let mut n = Name::new();
        let _ = uwrite!(n, "Zone \"{}\\", z);
        let cfg = ZoneCfg { scan_all: z % 2 == 0, shown: z % 3 != 0, by_distance: z % 5 == 0, resume: z % 13, dwell: if z % 4 == 0 { 255 } else { z }, pri: [Some(z as u16 * 3), if z % 2 == 1 { Some(999) } else { None }] };
        assert!(full.request(Op::PutZone(z as crate::chan::ZoneId, n, cfg)));
    }
    for i in 0..16u8 {
        let p = RangePreset { name: Buf::from(&b"Range 1234567"[..12 - (i as usize % 3)]), start: mhz(140) + i as u32 * 1_000_000, end: mhz(150) + i as u32 * 1_000_000, step: i % 14, mode: i % 3, narrow: i % 2 == 0, skips: 0, resume: i % 13 };
        assert!(full.request(Op::PutPreset(i, p)));
    }
    let bases = [mhz(144), mhz(430), mhz(156), mhz(118), mhz(222)];
    for n in 0..1000u16 {
        let k = n as u32;
        let rx = bases[n as usize % 5] + (k * 1250) % 300_000;
        let tone = |m: u32| match m % 4 {
            0 => 0,
            1 => CTCSS[(m % 50) as usize],
            2 => DCS_FLAG | DCS[(m % 104) as usize],
            _ => DCS_FLAG | DCS_INV | DCS[(m * 7 % 104) as usize],
        };
        let off = [0, 60_000, -60_000, 500_000, -500_000, 160_000][n as usize % 6];
        let cfg = SlotCfg {
            mode: (n % 3) as u8,
            narrow: n % 2 == 0,
            rx_tone: tone(k / 3),
            tx_tone: tone(k / 2),
            offset: off,
            power: if bases[n as usize % 5] == mhz(222) { 0 } else { (n % 3) as u8 },
            step: (n % 14) as u8,
            squelch: (n % 10) as u8,
            busy: (n % 3) as u8,
            reverse: n % 5 == 0,
            // every 7th a split (odd: RX on one band, TX on another)
            split: n % 7 == 3,
        };
        let cfg = if cfg.split { SlotCfg { offset: mhz(146) as i32 + (n as i32 % 40) * 1250, power: 0, ..cfg } } else { cfg };
        let mut c = Channel { num: n, zone: if n % 31 == 30 { NO_ZONE } else { n % 31 }, rx, name: [0; 16], cfg, flags: (n % 8) as u8 & (FLAG_SKIP | FLAG_APRS | FLAG_RX_ONLY), tones: (n % 7) as u8 | ((n % 3) as u8) << 3 };
        let mut name = Buf::<16>::new();
        let _ = uwrite!(name, "{}{}", if n % 9 == 0 { "q\"\\ " } else { "Ch " }, n);
        c.set_name(name.as_bytes());
        full.apply(ChanWrite::Put { ch: c, from: None });
    }
    let a = Rig::with(full);
    a.app.set.theme = 2;
    a.app.set.home_ch = Some(500);
    let d = dump(&a.app, &a.store, &a.dev);
    assert_eq!(d.iter().filter(|l| l.starts_with("ch ")).count(), 1000);
    let mut b = Rig::with(Blank::default());
    b.send(b"\r\r");
    b.vt.room = usize::MAX / 2;
    b.load(&d.join("\n"));
    assert!(b.raw().contains(" 0 refused."), "{}", &b.raw()[b.raw().len().saturating_sub(400)..]);
    assert_eq!(dump(&b.app, &b.store, &b.dev), d);
    assert_eq!(DEFAULT_CFG.squelch, 4);
}

/// TERM-13: a bad line is named and skipped whole; the rest goes in; region holds, 1.25 m power goes in (TERM-7).
#[test]
fn load_reports_bad_lines() {
    let mut r = Rig::main();
    let long = "a".repeat(300);
    let text = std::format!(
        "# a note\n\nset 0 7 1 Daylight\nset 0 9 4 EU\nch 5 \"X\" 146.5 tone=999\nch 6 \"Six\" 223.5 power=high\nch 7 \"Seven\" 146.55 bogus=1\nzone 9 \"Far\"\ndump\n{long}\nch 8 \"Eight\" 146.56 power=high\nend\nch 9 \"after the end\" 146.57"
    );
    r.load(&text);
    let raw = r.raw();
    for l in [
        "line 4: Region changes only on the radio",
        "line 5: tone: 999.0 Hz is outside 60.0 to 260.0 Hz",
        "line 7: bogus: no such field",
        "line 8: zone 9: zones are made in order",
        "line 9: dump can't be in a load",
        "line 10: too long",
        "Done: 11 lines read, 3 applied, 6 refused.",
    ] {
        assert!(raw.contains(l), "{l}\n{raw}");
    }
    assert_eq!(r.app.set.theme, 1);
    assert!(r.store.by_num(4).is_none() && r.store.by_num(6).is_none());
    assert_eq!(r.store.by_num(5).expect("1.25 m at high, TX-9").0.cfg.power, 2);
    assert_eq!(r.store.by_num(7).unwrap().0.cfg.power, 2);
    assert!(r.store.by_num(8).is_none(), "nothing after end");
    assert_eq!(r.app.set.region, crate::tests::set().region);
}

/// A load waits for the store before each line (TERM-13): the cable is held, nothing is lost.
#[test]
fn load_waits_for_the_store() {
    let mut r = Rig::with(Blank::default());
    r.send(b"\r\r");
    r.cmd("load");
    r.store.busy.set(true);
    r.send(b"ch 1 \"One\" 146.52\r");
    for _ in 0..5 {
        r.poll();
    }
    assert!(!r.term.ready(), "the line waits");
    assert_eq!(r.store.count(), 0);
    r.store.busy.set(false);
    r.poll();
    assert!(r.term.ready());
    assert_eq!(r.store.count(), 1);
    r.send(b"\x04");
    r.poll();
    assert_eq!(r.term.con, Con::Done);
    assert!(r.raw().contains("Done: 1 lines read, 1 applied, 0 refused."));
}


/// Split from the command line and the dump: offset=split with tx= (either order), the TX band's power cap,
/// squelch=global, and a ch line that loads back the same.
#[test]
fn command_split_and_global() {
    let mut r = Rig::main();
    r.cmd("ch 310 \"Cross\" 446.0 tx=146.52 offset=split squelch=global");
    let c = r.store.by_num(309).expect("made").0;
    assert!(c.cfg.split);
    assert_eq!((c.cfg.offset, c.cfg.squelch), (14_652_000, crate::slot::SQ_GLOBAL));
    r.cmd("ch 311 \"Cross2\" 446.0 offset=split tx=146.52");
    assert!(r.store.by_num(310).expect("either order").0.cfg.split);
    r.cmd("ch 312 \"NoTx\" 446.0 offset=split");
    assert!(r.bottom().contains("needs its TX frequency"), "{}", r.bottom());
    assert!(r.store.by_num(311).is_none());
    // TX on 1.25 m with RX on 70 cm: any level (TX-9).
    r.cmd("ch 313 \"Odd\" 446.0 tx=223.5 power=high");
    assert_eq!(r.store.by_num(312).expect("made").0.cfg.power, 2);
    // The ch line, and back.
    r.cmd("ch 310");
    assert!(r.raw().contains("ch 310 \"Cross\" 446.00000 offset=split tx=146.52000 mode=NFM"), "{}", r.raw());
    assert!(r.raw().contains("squelch=global"), "{}", r.raw());
    r.send(b"\r");
    r.cmd("ch 310 offset=+5");
    let c = r.store.by_num(309).unwrap().0;
    assert!(!c.cfg.split && c.cfg.offset == 500_000, "a shift ends the split");
}

/// The channel editor: Offset stepped onto Split moves to the TX row to type the frequency.
#[test]
fn editor_split() {
    let mut r = Rig::main();
    let first = r.store.by_number(0).unwrap();
    r.send(b"2");
    r.send(b"\x1b[H");
    r.send(b"\r");
    let off = FIELDS.iter().position(|f| *f == F::It(Item::Offset)).unwrap();
    for _ in 0..off {
        r.send(b"\x1b[B");
    }
    // Simplex -> Split is one step left (the 4th choice wraps).
    r.send(b"\x1b[D");
    assert_eq!(FIELDS[r.term.field() as usize], F::Tx, "typing the TX now");
    r.send(b"144.39\r");
    r.poll();
    let c = r.store.by_num(first.num).unwrap().0;
    assert!(c.cfg.split, "{:?}", c.cfg);
    assert_eq!(c.cfg.offset, 14_439_000);
    assert!(r.vt.has("Split 144.390") && r.vt.has("144.39000 MHz, split"), "{}", r.vt.text());
}

/// The last `ch N` line the console printed.
fn ch_shown(r: &Rig<Sample>, n: u16) -> String {
    let raw = r.raw();
    let at = raw.rfind(&std::format!("ch {} \"", n)).expect("shown");
    raw[at..].split(['\r', '\n']).next().unwrap().into()
}

/// CHAN-32, decision 223: `ch` changes one field with the editors' rules (crate::chfield): Skip and Priority exclude
/// each other (decision 63, CHAN-24); a simplex channel above 1000 MHz is kept, a shift that puts TX there isn't
/// (CHAN-22, VFO-21); power is free on 1.25 m like any band (TX-9, decision 224); an RX-only channel's power is
/// refused (CHAN-4).
#[test]
fn ch_field_rules() {
    let mut r = Rig::main();
    r.cmd("ch 330 \"Scan\" 146.52 skip");
    r.cmd("ch 330 priority");
    let c = r.store.by_num(329).unwrap().0;
    assert_eq!((c.scan(), c.flags & FLAG_SKIP), (2, 0), "priority replaces skip");
    r.cmd("ch 330 skip off");
    assert_eq!(r.store.by_num(329).unwrap().0.scan(), 2, "skip off leaves priority");
    r.cmd("ch 330");
    let l = ch_shown(&r, 330);
    assert!(l.ends_with(" priority") && !l.contains("skip"), "{l}");
    r.send(b"\r");
    r.cmd("ch 331 \"Hi\" 1296.0");
    assert!(r.store.by_num(330).is_some(), "receive only up there: {}", r.bottom());
    r.cmd("ch 331 offset=+5");
    assert!(r.bottom().contains("offset: TX outside 64 to 1000 MHz"), "{}", r.bottom());
    r.cmd("ch 332 \"Odd\" 223.5 tx=446.0");
    r.cmd("ch 332 power high");
    assert_eq!(r.store.by_num(331).unwrap().0.cfg.power, 2, "TX on 70 cm: {}", r.bottom());
    r.cmd("ch 333 \"Was cap\" 446.0 tx=223.5");
    r.cmd("ch 333 power high");
    assert_eq!(r.store.by_num(332).unwrap().0.cfg.power, 2, "TX on 1.25 m, as stock: {}", r.bottom());
    r.cmd("ch 333");
    assert!(ch_shown(&r, 333).contains(" power=high"), "as set");
    r.send(b"\r");
    r.cmd("ch 335 \"RO\" 146.52 rxonly");
    r.cmd("ch 335 power high");
    assert!(r.bottom().contains("power: this channel is RX only"), "{}", r.bottom());
}

/// CHAN-32, decision 223: the terminal's editor with the radio's rules: # on Squelch is Global (SQL-6, MENU-18), one
/// Scan row (decision 63), Offset onto Split keeps today's TX and opens the TX row (VFO-16), typed RX checked whole
/// (CHAN-26: NFM isn't an HF channel's mode).
#[test]
fn editor_shared_rules() {
    let mut r = Rig::main();
    r.cmd("ch 340 \"Ed\" 146.52 squelch=7");
    let open = |r: &mut Rig<Sample>, f: F| {
        r.term.set_edit(r.store.get(339));
        r.term.set_field(FIELDS.iter().position(|x| *x == f).unwrap() as u8);
        r.term.go(Page::Edit);
        r.poll();
    };
    open(&mut r, F::It(Item::Squelch));
    r.send(b"#");
    assert_eq!(r.store.get(339).unwrap().cfg.squelch, crate::slot::SQ_GLOBAL);
    open(&mut r, F::Scan);
    r.send(b"\x1b[C");
    assert!(r.vt.has("Skip"), "{}", r.vt.text());
    r.send(b"\x1b[C");
    assert_eq!(r.store.get(339).unwrap().flags & (FLAG_SKIP | FLAG_PRIORITY), FLAG_PRIORITY);
    open(&mut r, F::It(Item::Offset));
    r.send(b"\x1b[D");
    assert_eq!(FIELDS[r.term.field() as usize], F::Tx, "typing the TX now");
    r.send(b"\x1b");
    let c = r.store.get(339).unwrap();
    assert!(c.cfg.split && c.cfg.offset == c.rx as i32, "split on today's TX: {:?}", c.cfg);
    open(&mut r, F::Rx);
    r.send(b"\r7.1\r");
    assert!(r.vt.has("Mode: an HF channel is AM"), "{}", r.vt.text());
    assert_eq!(r.store.get(339).unwrap().rx, 14_652_000);
}

/// TX-37 from the cable: a channel line whose split TX needs the other relay state is refused, typed or loaded.
#[test]
fn split_across_the_relay_refused() {
    let mut r = Rig::main();
    r.cmd("ch 320 \"Cross\" 446.0 offset=split tx=146.52");
    assert!(r.store.by_num(319).expect("allowed: same relay state").0.cfg.split);
    r.cmd("ch 321 \"Six\" 146.52 offset=split tx=50.125");
    assert!(r.bottom().contains("Split TX needs the HF relay"), "{}", r.bottom());
    assert!(r.store.by_num(320).is_none());
    r.load("ch 322 \"Ok\" 146.55\nch 323 \"Bad\" 146.52 offset=split tx=50.125\nend");
    let raw = r.raw();
    assert!(raw.contains("line 2: Split TX needs the HF relay: not allowed with this RX frequency"), "{raw}");
    assert!(r.store.by_num(321).is_some() && r.store.by_num(322).is_none());
}

// ---- Debug commands (TERM-21 to TERM-24) ----

use ferrite_core::msg::{DebugAns, DebugReq};
#[cfg(feature = "bench")]
use ferrite_core::msg::DEBUG_MEM;

impl Rig<Sample> {
    /// Polls, answering each debug request with `f`; every request seen, in order.
    fn serve(&mut self, polls: usize, mut f: impl FnMut(DebugReq) -> DebugAns) -> Vec<DebugReq> {
        let mut seen = Vec::new();
        for _ in 0..polls {
            if let Some(r) = self.term.debug_req() {
                seen.push(r);
                self.term.debug_answer(f(r));
            }
            self.poll();
        }
        seen
    }
}

/// A made-up register file: each register reads as its number twice (REG_30 = 0x3030).
fn regs(r: DebugReq) -> DebugAns {
    match r {
        #[cfg(feature = "bench")]
        DebugReq::BkRead { chip, reg } => DebugAns::Reg { chip, reg, val: reg as u16 * 0x101 },
        #[cfg(feature = "bench")]
        DebugReq::BkWrite { chip, reg, val } => DebugAns::Reg { chip, reg, val },
        #[cfg(feature = "bench")]
        DebugReq::Mem { addr, n, .. } => {
            let mut data = [0u8; DEBUG_MEM];
            for (i, b) in data.iter_mut().enumerate() {
                *b = (addr as usize + i) as u8;
            }
            DebugAns::Mem { addr, data, n }
        }
        #[cfg(feature = "bench")]
        DebugReq::Si { reply, .. } => {
            let mut data = [0u8; 16];
            data[0] = 0x80;
            DebugAns::Si { data, n: reply }
        }
        #[cfg(feature = "bench")]
        // A module that answers its version.
        DebugReq::Bt { .. } => {
            let mut data = [0u8; 48];
            data[..8].copy_from_slice(b"V1.0\r\nOK");
            DebugAns::Bt { data, n: 8 }
        }
        #[cfg(feature = "bench")]
        DebugReq::Linkz => DebugAns::Linkz([0; 11]),
        #[cfg(feature = "bench")]
        DebugReq::BtStat { baud, pace } => DebugAns::BtStat(ferrite_core::msg::BtStats { rx: 12, tx: 340, framing: 1, overrun: 0, dropped: 0, baud: if baud == 0 { 9600 } else { baud }, on: true, pace: pace.unwrap_or_default() }),
        #[cfg(feature = "bench")]
        DebugReq::RxBoth(on) => DebugAns::RxBoth(on.unwrap_or(false)),
        #[cfg(feature = "bench")]
        DebugReq::Crash(_) => unreachable!("never answered: the radio restarts"),
        #[cfg(feature = "bench")]
        DebugReq::Settle(s) => DebugAns::Settle(s.unwrap_or(ferrite_core::settle::Settle::DEFAULT)),
        #[cfg(feature = "bench")]
        DebugReq::Save(c) => DebugAns::Save { cycle: c.flatten(), level: Some((300, 500)), asleep: false },
        #[cfg(feature = "bench")]
        DebugReq::Spi { half, reads, set } => DebugAns::Spi { half: half.unwrap_or(30), reads, rd: [0, if half == Some(10) { 3 } else { 0 }], wr: [0, 0], us: 250, now: set.unwrap_or([30, 15]) },
        #[cfg(feature = "bench")]
        DebugReq::Turns { .. } => DebugAns::Turns(ferrite_core::watch::Turns { n: [27, 27, 0], ms: [4050, 1890, 0], steps: [40, 0, 0], step_ms: [2800, 0, 0], tunes: 94, tune_us: 470_000, tune_max_us: 9_100 }),
        #[cfg(feature = "bench")]
        DebugReq::Lcd(l) => DebugAns::Lcd(ferrite_core::msg::LcdBench { idle: true, ..Default::default() }.apply(l)),
        #[cfg(feature = "bench")]
        DebugReq::RxAf(set) => DebugAns::RxAf(set.flatten()),
        #[cfg(feature = "bench")]
        DebugReq::RxIf(set) => DebugAns::RxIf(set.flatten()),
        #[cfg(feature = "bench")]
        DebugReq::SynthFsk(_) => DebugAns::SynthFsk(ferrite_core::synthfsk::Report { steps: 57600, write_max_x10: 123, write_avg_x10: 45, late: 2, late_max_us: 31, freq_word: 44_647_500 }),
        #[cfg(feature = "bench")]
        DebugReq::Pin { which, mode } => DebugAns::Pin { which, mode, high: mode == ferrite_core::msg::PinMode::High || mode == ferrite_core::msg::PinMode::PullUp },
        #[cfg(feature = "bench")]
        DebugReq::Hold(set) => DebugAns::Hold([set.flatten().unwrap_or((0, 0)), (0, 0), (0, 0), (0, 0)]),
        #[cfg(feature = "bench")]
        DebugReq::Swap(set) => DebugAns::Swap(set.unwrap_or(0)),
        #[cfg(feature = "bench")]
        DebugReq::Meter(set) => {
            let mut r = ferrite_core::rxmeter::Report { on: set != Some(false), windows: 31, ..ferrite_core::rxmeter::Report::OFF };
            r.db10[13] = -123;
            r.db10[24] = -5;
            DebugAns::Meter(r)
        }
        #[cfg(feature = "bench")]
        DebugReq::Fsk(a) => DebugAns::Fsk(fsk_answer(a)),
        #[cfg(feature = "bench")]
        DebugReq::Mic(_) => DebugAns::Mic(ferrite_core::vox::MicCapture::from_samples(&[0; 256], 32_000)),
        #[cfg(feature = "bench")]
        DebugReq::TxTest(ferrite_core::txtest::Ask::Set(a)) => DebugAns::TxTest(a),
        #[cfg(feature = "bench")]
        DebugReq::TxTest(ferrite_core::txtest::Ask::Show) => DebugAns::TxTest(None),
        // The ATGM336H sentence suspected on the black radio: field 9 low, field 11 empty.
        DebugReq::Gga => gga(b"$GNGGA,143210.000,4525.2950,N,07541.8600,W,1,09,0.9,-12.3,M,,M,,*57", 12),
        #[cfg(feature = "bench")]
        // A module that answers its firmware version.
        DebugReq::Gps { .. } => {
            let s = b"$GPTXT,01,01,02,SW=URANUS5,V5.3.0.0*1D";
            let mut data = [0u8; ferrite_core::msg::GGA_MAX];
            data[..s.len()].copy_from_slice(s);
            DebugAns::GpsSent { data, n: s.len() as u8 }
        }
    }
}

/// A made-up `fsk` answer: a receive run with two frames counted, the last frame 3's bytes; a transmit run armed.
#[cfg(feature = "bench")]
fn fsk_answer(a: ferrite_core::fskbench::Ask) -> ferrite_core::fskbench::Report {
    use ferrite_core::fskbench::{payload, Ask, Report, Stats, LEN};
    let off = Report { cfg: None, tx: None, armed: false, chip: None, stats: Stats::default() };
    match a {
        Ask::Show | Ask::Off => off,
        Ask::Rx(cfg) => {
            let stats = Stats { syncs: 3, frames: 2, crc_ok: 1, crc_bad: 1, clean: 1, last: payload(3), last_n: LEN as u8, last_counter: 3, ..Stats::default() };
            Report { cfg: Some(cfg), chip: Some(0), stats, ..off }
        }
        Ask::Tx(cfg, n) => Report { cfg: Some(cfg), tx: Some(n), armed: true, ..off },
    }
}

/// TERM-42: `bk rxaf` asks the receive AF filter override, `bk rxaf HEX` sets REG_2B bits 10:8 (no other bit), `bk
/// rxaf off` ends it; anything else is refused before anything is sent; `bk rxaf` alone only reads (TERM-25). TERM-42.1:
/// a top digit carries REG_47's receive route (9700).
#[cfg(feature = "bench")]
/// TERM-42.3: `bk hold` holds up to four registers. TERM-42.2: `bk rxif` asks the receive IF override, `bk rxif HEX` holds the whole of REG_43, `bk rxif off` ends it;
/// anything else is refused before anything is sent. TERM-44: `meter` shows the bins (a head line and five lines of
/// five), `meter on` and `meter off` set it; `bk rxif` and `meter` alone only read (TERM-25).
#[test]
fn debug_bk_rxif_and_meter() {
    let mut r = Rig::main();
    for (cmd, want) in [("bk rxif", None), ("bk rxif 5a20", Some(Some(0x5A20))), ("bk rxif off", Some(None))] {
        r.send(b"\r");
        r.cmd(cmd);
        assert_eq!(r.serve(4, regs), vec![DebugReq::RxIf(want)], "{cmd}");
    }
    r.send(b"\r");
    r.cmd("bk rxif 7e28");
    r.serve(4, regs);
    assert!(r.raw().contains("REG_43 = 0x7E28") && r.raw().contains("IF 7 / weak 7, x2 on"), "{}", r.raw());
    r.send(b"\r");
    for bad in ["bk rxif 10000", "bk rxif 5a20 1", "bk rxif on"] {
        r.cmd(bad);
        assert!(r.bottom().contains("bk rxif [HEX|off]"), "{bad}: {}", r.bottom());
    }
    for (cmd, want) in [("meter", None), ("meter on", Some(true)), ("METER off", Some(false))] {
        r.send(b"\r");
        r.cmd(cmd);
        assert_eq!(r.serve(8, regs), vec![DebugReq::Meter(want)], "{cmd}");
    }
    r.send(b"\r");
    r.cmd("meter");
    r.serve(8, regs);
    let raw = r.raw();
    assert!(raw.contains("meter on: 31 windows"), "{raw}");
    assert!(raw.contains("  1000  -12.3") && raw.contains(" 12500   -0.5") && raw.contains("    50   --.-"), "{raw}");
    r.send(b"\r");
    r.cmd("meter loud");
    assert!(r.bottom().contains("meter [on|off]"), "{}", r.bottom());
    assert!(!super::cmd::writes(b"bk rxif") && super::cmd::writes(b"bk rxif 5a20"));
    assert!(!super::cmd::writes(b"meter") && super::cmd::writes(b"meter on"));
    // TERM-42.3: `bk hold`
    r.send(b"\r");
    for (cmd, want) in [("bk hold", None), ("bk hold 73 4692", Some(Some((0x73, 0x4692)))), ("bk hold off", Some(None))] {
        r.send(b"\r");
        r.cmd(cmd);
        assert_eq!(r.serve(4, regs), vec![DebugReq::Hold(want)], "{cmd}");
    }
    // TERM-29.4: `bk swap` (the fox-hunt measurement): a slot on the other band's chip, off, or asked
    for (cmd, want) in [("bk swap", None), ("bk swap b", Some(0b010)), ("bk swap off", Some(0))] {
        r.send(b"\r");
        r.cmd(cmd);
        assert_eq!(r.serve(4, regs), vec![DebugReq::Swap(want)], "{cmd}");
    }
    r.send(b"\r");
    r.cmd("bk swap A");
    r.serve(4, regs);
    assert!(r.raw().contains("bk swap: A on the other band's chip"), "{}", r.raw());
    r.send(b"\r");
    r.cmd("bk swap D");
    assert!(r.bottom().contains("bk swap [A|B|C|off]"), "{}", r.bottom());
    assert!(!super::cmd::writes(b"bk swap") && super::cmd::writes(b"bk swap a"));
    r.send(b"\r");
    r.cmd("bk hold 28 0040");
    r.serve(4, regs);
    assert!(r.raw().contains("bk hold: REG_28 = 0x0040 after every FM tune"), "{}", r.raw());
    r.send(b"\r");
    for bad in ["bk hold 0 1", "bk hold 73", "bk hold 73 10000", "bk hold on"] {
        r.cmd(bad);
        assert!(r.bottom().contains("bk hold [REG VALUE|off]"), "{bad}: {}", r.bottom());
    }
    assert!(!super::cmd::writes(b"bk hold") && super::cmd::writes(b"bk hold 73 4692"));
    // TERM-45: `synthfsk`
    r.cmd("synthfsk 3000 4 2400 1290 10 40 70 100");
    assert_eq!(r.serve(4, regs), vec![DebugReq::SynthFsk(ferrite_core::synthfsk::Cfg { cal: false, ms: 3000, sub: 4, shift_hz: 2400, dev: 1290, g: [10, 40, 70, 100] })]);
    assert!(r.raw().contains("synthfsk 4fsk: 57600 steps at 4 a symbol; gain write 4.5 us avg, 12.3 max; 2 late (worst 31 us)"), "{}", r.raw());
    r.send(b"\r");
    r.cmd("synthfsk cal 1800 2400 1290");
    assert_eq!(r.serve(4, regs), vec![DebugReq::SynthFsk(ferrite_core::synthfsk::Cfg { cal: true, ms: 1800, sub: 1, shift_hz: 2400, dev: 1290, g: [0; 4] })]);
    r.send(b"\r");
    // TERM-46: `pin`
    use ferrite_core::msg::PinMode;
    r.cmd("pin PA15 0");
    assert_eq!(r.serve(4, regs), vec![DebugReq::Pin { which: 0, mode: PinMode::Low }]);
    assert!(r.raw().contains("pin pa15 driven low: reads 0"), "{}", r.raw());
    r.send(b"\r");
    for (cmd, which, mode) in [("pin pc12 1", 1, PinMode::High), ("pin pc9 u", 2, PinMode::PullUp), ("pin PB8 d", 3, PinMode::PullDown), ("pin pe1 0", 4, PinMode::Low), ("pin pe4 1", 5, PinMode::High)] {
        r.cmd(cmd);
        assert_eq!(r.serve(4, regs), vec![DebugReq::Pin { which, mode }], "{cmd}");
        r.send(b"\r");
    }
    for bad in ["pin", "pin pa14 0", "pin pa15 2", "pin pc12 1 1", "pin pc9 x"] {
        r.cmd(bad);
        assert!(r.bottom().contains("pin pa15|pc12|pc9|pb8|pe1|pe4 0|1|u|d"), "{bad}: {}", r.bottom());
    }
    for bad in ["synthfsk", "synthfsk cal 50 0 0", "synthfsk 3000 17 0 0 1 2 3 4", "synthfsk 3000 4 0 0 1 2 3 200", "synthfsk 3000 4 0 0 1 2 3"] {
        r.cmd(bad);
        assert!(r.bottom().contains("synthfsk cal MS SHIFT DEV"), "{bad}: {}", r.bottom());
    }
}

#[cfg(feature = "bench")]
#[test]
fn debug_bk_rxaf() {
    let mut r = Rig::main();
    r.cmd("bk rxaf");
    assert_eq!(r.serve(4, regs), vec![DebugReq::RxAf(None)]);
    assert!(r.raw().contains("bk rxaf: off"), "{}", r.raw());
    for (cmd, want) in [("bk rxaf 700", Some(Some(0x0700))), ("BK RXAF 0x400", Some(Some(0x0400))), ("bk rxaf 0", Some(Some(0))), ("bk rxaf 9700", Some(Some(0x9700))), ("bk rxaf off", Some(None))] {
        r.send(b"\r");
        r.cmd(cmd);
        assert_eq!(r.serve(4, regs), vec![DebugReq::RxAf(want)], "{cmd}");
    }
    r.send(b"\r");
    r.cmd("bk rxaf 700");
    r.serve(4, regs);
    assert!(r.raw().contains("REG_2B bits 10:8 = 0x0700 after every FM tune (HPF 300 Hz, LPF 3 kHz, de-emphasis off)"), "{}", r.raw());
    r.send(b"\r");
    for bad in ["bk rxaf 800", "bk rxaf 701", "bk rxaf 700 1", "bk rxaf on"] {
        r.cmd(bad);
        assert!(r.bottom().contains("bk rxaf [HEX|off]"), "{bad}: {}", r.bottom());
        assert_eq!(r.term.debug_req(), None, "{bad}: nothing sent");
    }
    assert!(!super::cmd::writes(b"bk rxaf") && super::cmd::writes(b"bk rxaf 700"));
}

/// TERM-43: `fsk` shows the run; `fsk rx SPEED [nocrc]` and `fsk tx SPEED N [nocrc]` start one, `fsk off` ends it; the
/// speeds are 1200, 2400 (any plain FSK rate 300 to 9600), ffsk1800 and ffsk2400; a bad speed, count or word is
/// refused before anything is sent; `fsk` alone only reads (TERM-25).
#[cfg(feature = "bench")]
#[test]
fn debug_fsk() {
    use ferrite_core::fskbench::{Ask, Cfg, Mode, Speed};
    let sp = |mode, baud| Speed { mode, baud };
    let mut r = Rig::main();
    r.cmd("fsk");
    assert_eq!(r.serve(4, regs), vec![DebugReq::Fsk(Ask::Show)]);
    assert!(r.raw().contains("fsk: off"), "{}", r.raw());
    for (cmd, want) in [
        ("fsk rx 2400", Ask::Rx(Cfg { speed: sp(Mode::Fsk, 2400), crc: true })),
        ("FSK RX ffsk2400 NOCRC", Ask::Rx(Cfg { speed: sp(Mode::Ffsk2400, 1200), crc: false })),
        ("fsk rx 4800", Ask::Rx(Cfg { speed: sp(Mode::Fsk, 4800), crc: true })),
        ("fsk tx ffsk1800 20", Ask::Tx(Cfg { speed: sp(Mode::Ffsk1800, 1200), crc: true }, 20)),
        ("fsk tx 1200 999 nocrc", Ask::Tx(Cfg { speed: sp(Mode::Fsk, 1200), crc: false }, 999)),
        ("fsk off", Ask::Off),
    ] {
        r.send(b"\r");
        r.cmd(cmd);
        assert_eq!(r.serve(6, regs), vec![DebugReq::Fsk(want)], "{cmd}");
    }
    r.send(b"\r");
    r.cmd("fsk rx 4800");
    r.serve(6, regs);
    let raw = r.raw();
    assert!(raw.contains("fsk rx 4800 (chip CRC on) (not a documented speed): chip 0 listening"), "{raw}");
    assert!(raw.contains("syncs 3 (0 inverted), frames 2: chip CRC ok 1 bad 1; clean 1, short 0"), "{raw}");
    assert!(raw.contains("last, #3 (32 bytes): 00 03 07 BE 2E"), "{raw}");
    r.send(b"\r");
    r.cmd("fsk tx 2400 20");
    r.serve(6, regs);
    assert!(r.raw().contains("fsk tx 2400 (chip CRC on): 0 of 20 frames sent, armed for the next PTT key-up (Dry TX on only)"), "{}", r.raw());
    r.send(b"\r");
    for bad in ["fsk rx", "fsk rx 299", "fsk rx 9601", "fsk rx ffsk", "fsk tx 2400", "fsk tx 2400 0", "fsk tx 2400 1000", "fsk rx 2400 crc", "fsk on", "fsk off 1"] {
        r.cmd(bad);
        assert!(r.bottom().contains("fsk [off], fsk rx|tx SPEED"), "{bad}: {}", r.bottom());
        assert_eq!(r.term.debug_req(), None, "{bad}: nothing sent");
    }
    assert!(!super::cmd::writes(b"fsk") && super::cmd::writes(b"fsk off") && super::cmd::writes(b"fsk rx 2400"));
}

#[cfg(feature = "bench")]
/// TERM-36.1, TERM-36.2, TERM-36.3, TERM-36.4, TERM-36.6, TERM-36.9: `gps query N` sends CAS06 and shows the sentence
/// sent and the module's TXT answer; `gps standby N` sends CAS12; `gps nmea off|on` CAS03 (all off, Ferrite's set on,
/// GPS-34.1); `gps standby N quiet` NMEA off then CAS12; anything else (another CAS number, CAS00 save, CAS01 baud, a
/// bad argument) is refused before it reaches the firmware. The query reads (allowed for an unapproved Bluetooth
/// device, TERM-25), standby writes.
#[test]
fn terminal_gps_casic() {
    let mut r = Rig::main();
    r.cmd("gps query");
    assert_eq!(r.serve(4, regs), vec![DebugReq::Gps { id: 6, arg: 0, quiet: false }]);
    let raw = r.raw();
    assert!(raw.contains("sent $PCAS06,0*1B CR LF"), "{raw}");
    assert!(raw.contains("answer: $GPTXT,01,01,02,SW=URANUS5,V5.3.0.0*1D"), "{raw}");
    r.send(b"\r");
    r.cmd("gps standby 60");
    let data = [0u8; ferrite_core::msg::GGA_MAX];
    assert_eq!(r.serve(4, |_| DebugAns::GpsSent { data, n: 0 }), vec![DebugReq::Gps { id: 12, arg: 60, quiet: false }]);
    assert!(r.raw().contains("sent $PCAS12,60*28 CR LF"), "{}", r.raw());
    // TERM-36.1, TERM-36.2, TERM-36.3, TERM-36.4, TERM-36.6, TERM-36.9: NMEA off and Ferrite's set on (CAS03,
    // GPS-34.1), and the quiet standby (NMEA off, then CAS12).
    for (c, want, shown) in [
        ("gps nmea off", DebugReq::Gps { id: 3, arg: 0, quiet: false }, "sent $PCAS03,0,0,0,0,0,0,0,0*02 CR LF"),
        ("gps nmea on", DebugReq::Gps { id: 3, arg: 1, quiet: false }, "sent $PCAS03,1,0,5,5,1,0,0,0*02 CR LF"),
        ("gps standby 40 quiet", DebugReq::Gps { id: 12, arg: 40, quiet: true }, "sent $PCAS03,0,0,0,0,0,0,0,0*02 CR LF, 100 ms later $PCAS12,40*2A CR LF"),
    ] {
        r.send(b"\r");
        r.cmd(c);
        assert_eq!(r.serve(4, |_| DebugAns::GpsSent { data, n: 0 }), vec![want], "{c}");
        assert!(r.raw().contains(shown), "{c}: {}", r.raw());
        assert!(super::cmd::writes(c.as_bytes()), "{c}");
    }
    for bad in ["gps query 3", "gps standby 0", "gps standby 70000", "gps baud 9600", "gps query 0 1", "gps nmea", "gps nmea 1", "gps nmea off on", "gps standby 40 loud", "gps standby 40 quiet 1", "gps query 1 quiet"] {
        r.send(b"\r");
        r.cmd(bad);
        assert!(r.serve(4, regs).is_empty(), "{bad}");
    }
    assert!(!super::cmd::writes(b"gps query 1"));
    assert!(super::cmd::writes(b"gps standby 60"));
}

fn gga(s: &[u8], age_s: u32) -> DebugAns {
    let mut data = [0u8; ferrite_core::msg::GGA_MAX];
    data[..s.len()].copy_from_slice(s);
    DebugAns::Gga { data, n: s.len() as u8, age_s }
}

/// TERM-30: `gps` shows the module's last GGA sentence as received, its age, and fields 9 and 11 as read; with
/// none yet it says so. A read: allowed for an unapproved Bluetooth device (TERM-25).
#[test]
fn debug_gps_gga() {
    let mut r = Rig::main();
    r.cmd("gps");
    assert_eq!(r.serve(4, regs), vec![DebugReq::Gga]);
    let raw = r.raw();
    assert!(raw.contains("GGA, 12 s ago: $GNGGA,143210.000,4525.2950,N,07541.8600,W,1,09,0.9,-12.3,M,,M,,*57"), "{raw}");
    assert!(raw.contains("altitude (field 9) -12.3 m, geoid separation (field 11) empty"), "{raw}");
    r.send(b"\r");
    r.cmd("GPS");
    r.serve(4, |_| gga(b"$GPGGA,1,4525.2950,N,07541.8600,W,1,09,0.9,71.2,M,-34.0,M,,*00", 0));
    assert!(r.raw().contains("altitude (field 9) 71.2 m, geoid separation (field 11) -34.0 m"), "{}", r.raw());
    r.send(b"\r");
    r.cmd("gps");
    r.serve(4, |_| gga(b"", 0));
    assert!(r.raw().contains("no GGA since boot"), "{}", r.raw());
    assert!(!super::cmd::writes(b"gps"));
}

/// TERM-30: the heights as read from fields 9 and 11, in decimetres.
#[test]
fn gga_heights() {
    use super::debug::gga_heights as h;
    assert_eq!(h(b"$GNGGA,143210.000,4525.2950,N,07541.8600,W,1,09,0.9,71.2,M,-34.0,M,,*5E"), (Some(712), Some(-340)));
    assert_eq!(h(b"GNGGA,1,4525.2950,N,07541.8600,W,1,09,0.9,-12.34,M,,M,,"), (Some(-123), None));
    assert_eq!(h(b"GNGGA,143000.000,,,,,0,00,25.5,,,,,,"), (None, None));
    assert_eq!(h(b"GNGGA,1,,,,,1,09,0.9,5,M,0.0,M,,"), (Some(50), Some(0)));
    assert_eq!(h(b"GNGGA,1,,,,,1,09,0.9,5.,M,x,M,,"), (Some(50), None));
    assert_eq!(h(b"GNGGA,1,,,,,1,09,0.9,.,M,-,M"), (None, None));
    assert_eq!(h(b"GNGGA,1"), (None, None));
}

#[cfg(feature = "bench")]
/// SAFE-8.1 (decision 364): `crash fault` and `crash hang` ask the Radio task to fault or hang; anything else is
/// refused here, nothing sent.
#[test]
fn debug_crash() {
    let mut r = Rig::main();
    r.cmd("crash fault");
    assert_eq!(r.term.debug_req(), Some(DebugReq::Crash(true)));
    let mut r = Rig::main();
    r.cmd("CRASH hang");
    assert_eq!(r.term.debug_req(), Some(DebugReq::Crash(false)));
    for c in ["crash", "crash now", "crash fault 2"] {
        let mut r = Rig::main();
        r.cmd(c);
        assert_eq!(r.term.debug_req(), None, "{c}");
        assert!(r.raw().contains("crash fault or crash hang"), "{c}: {}", r.raw());
    }
}

#[cfg(feature = "bench")]
/// TERM-29.1, TERM-29.2: `rx both` and `rx one` ask the Radio task for the hold on both band receive selects and say
/// what is on; `rx` asks; a refusal (keyed, HF mode) is shown as the Radio task gives it; bad words are refused here.
#[test]
fn debug_rx_both() {
    let mut r = Rig::main();
    r.cmd("rx both");
    assert_eq!(r.serve(4, regs), vec![DebugReq::RxBoth(Some(true))]);
    assert!(r.raw().contains("rx both: VHF RX (PB0) and UHF RX (PE12) held on"), "{}", r.raw());
    r.send(b"\r");
    r.cmd("RX one");
    assert_eq!(r.serve(4, regs), vec![DebugReq::RxBoth(Some(false))]);
    assert!(r.raw().contains("rx one: only the receiving band's select on"), "{}", r.raw());
    r.send(b"\r");
    r.cmd("rx");
    assert_eq!(r.serve(4, regs), vec![DebugReq::RxBoth(None)]);
    r.send(b"\r");
    r.cmd("rx both");
    r.serve(4, |_| DebugAns::Refused("HF mode is on: it has the HF path and the relay; leave HF mode first"));
    assert!(r.raw().contains("refused: HF mode is on"), "{}", r.raw());
    r.send(b"\r");
    for bad in ["rx all", "rx both now"] {
        r.cmd(bad);
        assert!(r.bottom().contains("rx both, rx one"), "{bad}: {}", r.bottom());
        assert_eq!(r.term.debug_req(), None, "{bad}: nothing sent");
    }
    assert!(!super::cmd::writes(b"rx") && super::cmd::writes(b"rx both"));
}

#[cfg(feature = "bench")]
/// TERM-32: `mic` asks the Radio task for a PA0 burst and prints its summary in three lines; it only reads.
#[test]
fn debug_mic() {
    use ferrite_core::vox::MicCapture;
    let mut r = Rig::main();
    r.cmd("mic");
    let sine: Vec<u16> = (0..256).map(|i| (2048.0 + 600.0 * (i as f64 * core::f64::consts::TAU / 8.0 + 0.3).sin()) as u16).collect();
    let cap = MicCapture::from_samples(&sine, 32_000);
    assert_eq!(r.serve(6, |_| DebugAns::Mic(cap)), vec![DebugReq::Mic(false)]);
    let raw = r.raw();
    assert!(raw.contains("mic PA0: 256 readings in 32000 us"), "{raw}");
    assert!(raw.contains(" Hz if this is raw audio"), "{raw}");
    assert!(raw.contains("first 16 (8-bit):"), "{raw}");
    assert!(!super::cmd::writes(b"mic"));
}

#[cfg(feature = "bench")]
/// TERM-32.1: `af` is the same burst from PA2, the receive audio; it only reads.
#[test]
fn debug_af() {
    use ferrite_core::vox::MicCapture;
    let mut r = Rig::main();
    r.cmd("af");
    let cap = MicCapture::from_samples(&[1250; 256], 32_000);
    assert_eq!(r.serve(6, |_| DebugAns::Mic(cap)), vec![DebugReq::Mic(true)]);
    assert!(r.raw().contains("af PA2: 256 readings in 32000 us"), "{}", r.raw());
    assert!(!super::cmd::writes(b"af"));
}

#[cfg(feature = "bench")]
/// TERM-33.1, TERM-33.6, TERM-33.9, TERM-33.14: `txtest MODE [FILTERS] [LEVEL]` arms, `txtest off` disarms, `txtest`
/// alone asks (a read); the filters default to data (4fsk, sweep and afsk alike, as an APRS burst goes out); the level
/// to 100 %, 10 to 150 taken, with or without the filters before it; `afsk-bk` takes a level but no filters word and
/// shows its tone gain (66 at 100 %, 99 at 150); anything else is refused before anything is sent; a receive-only
/// build's refusal prints as one.
#[test]
fn debug_txtest() {
    use ferrite_core::txtest::{Arm, Ask, Filters, Signal};
    let mut r = Rig::main();
    r.cmd("txtest");
    assert_eq!(r.serve(4, regs), vec![DebugReq::TxTest(Ask::Show)]);
    assert!(r.raw().contains("txtest: off (nothing armed"), "{}", r.raw());
    r.send(b"\r");
    for (line, signal, filters, level) in [
        ("txtest 4fsk", Signal::Fsk4, Filters::Data, 100),
        ("txtest SWEEP", Signal::Sweep, Filters::Data, 100),
        ("txtest afsk", Signal::Afsk, Filters::Data, 100),
        ("txtest afsk flat", Signal::Afsk, Filters::Flat, 100),
        ("txtest 4fsk voice", Signal::Fsk4, Filters::Voice, 100),
        ("txtest 4fsk flat 80", Signal::Fsk4, Filters::Flat, 80),
        ("txtest afsk 70", Signal::Afsk, Filters::Data, 70),
        ("txtest afsk voice", Signal::Afsk, Filters::Voice, 100),
        ("txtest sweep DATA 150", Signal::Sweep, Filters::Data, 150),
        ("txtest 4fsk 10", Signal::Fsk4, Filters::Data, 10),
        ("txtest afsk-bk", Signal::AfskBk, Filters::Voice, 100),
        ("txtest AFSK-BK 150", Signal::AfskBk, Filters::Voice, 150),
    ] {
        r.cmd(line);
        assert_eq!(r.serve(4, regs), vec![DebugReq::TxTest(Ask::Set(Some(Arm { signal, filters, level, ..Arm::new(signal) })))], "{line}");
        let raw = r.raw();
        assert!(raw.contains(&std::format!("txtest: {} at {level} % armed for the next PTT key-up", signal.name())), "{raw}");
        let want = match filters {
            _ if signal == Signal::AfskBk => "afsk-bk: tone 1 1200/2200 Hz (REG_71) at gain",
            Filters::Flat => "flat: TX HPF, LPF, pre-emphasis",
            Filters::Data => "data: TX HPF off, pre-emphasis and LPF on (REG_2B 2:0 = 100)",
            Filters::Voice => "voice: the chip's TX filters",
        };
        assert!(raw.contains(want), "{line}: {raw}");
        if signal == Signal::AfskBk {
            assert!(raw.contains(&std::format!("at gain {} (REG_70)", if level == 100 { 66 } else { 99 })), "{raw}");
        }
        r.send(b"\r");
    }
    r.cmd("txtest off");
    assert_eq!(r.serve(4, regs), vec![DebugReq::TxTest(Ask::Set(None))]);
    r.send(b"\r");
    for bad in ["txtest fsk", "txtest 4fsk loud", "txtest sweep flat 2", "txtest off now", "txtest 4fsk 9", "txtest 4fsk 151", "txtest afsk data 100 1", "txtest 4fsk 100 flat", "txtest afsk-bk data", "txtest afsk-bk voice 100", "txtest afsk-bk 151"] {
        r.cmd(bad);
        assert!(r.bottom().contains("txtest 4fsk|sweep|afsk"), "{bad}: {}", r.bottom());
        assert_eq!(r.term.debug_req(), None, "{bad}: nothing sent");
    }
    r.cmd("txtest 4fsk");
    assert_eq!(r.serve(4, |_| DebugAns::Refused("the receive-only build has no transmit")), vec![DebugReq::TxTest(Ask::Set(Some(Arm::new(Signal::Fsk4))))]);
    assert!(r.raw().contains("refused: the receive-only build has no transmit"), "{}", r.raw());
    assert!(!super::cmd::writes(b"txtest") && super::cmd::writes(b"txtest 4fsk") && super::cmd::writes(b"txtest off"));
}

#[cfg(feature = "bench")]
/// TERM-33.15, TERM-33.16, TERM-33.17 (decision 308): `fsk BAUD [rc]`, `gmsk BAUD` and `g3ruh` arm the data
/// signals (BAUD 300 to 9600, required for fsk and gmsk; `rc` fsk's only, before FILTERS); `bypass` and `eq`/`eq8`
/// follow FILTERS in any order, once each, never with `voice`; LEVEL stays last. The armed answer names the baud,
/// and a third line the bypass and the equaliser's 1 kHz gain and gain stages.
#[test]
fn debug_txtest_data() {
    use ferrite_core::txtest::{Arm, Ask, Eq, Filters, Signal};
    let mut r = Rig::main();
    let a = |s| Arm::new(s);
    for (line, arm) in [
        ("txtest fsk 1200", Arm { baud: 1200, ..a(Signal::Fsk) }),
        ("txtest FSK 9600 rc", Arm { baud: 9600, shaped: true, ..a(Signal::Fsk) }),
        ("txtest fsk 300 rc flat bypass eq 150", Arm { baud: 300, shaped: true, filters: Filters::Flat, bypass: true, eq: Eq::Eq, level: 150, ..a(Signal::Fsk) }),
        ("txtest gmsk 4800 data eq8 bypass", Arm { baud: 4800, eq: Eq::Eq8, bypass: true, ..a(Signal::Gmsk) }),
        ("txtest g3ruh", Arm { baud: 9600, shaped: true, ..a(Signal::G3ruh) }),
        ("txtest g3ruh bypass 80", Arm { baud: 9600, shaped: true, bypass: true, level: 80, ..a(Signal::G3ruh) }),
        ("txtest sweep data bypass eq", Arm { bypass: true, eq: Eq::Eq, ..a(Signal::Sweep) }),
        ("txtest sweep bypass", Arm { bypass: true, ..a(Signal::Sweep) }),
        ("txtest 4fsk eq", Arm { eq: Eq::Eq, ..a(Signal::Fsk4) }),
    ] {
        let n0 = r.vt.raw.len();
        r.cmd(line);
        assert_eq!(r.serve(4, regs), vec![DebugReq::TxTest(Ask::Set(Some(arm)))], "{line}");
        let raw: std::string::String = std::string::String::from_utf8_lossy(&r.vt.raw[n0..]).into();
        let title = if arm.signal.takes_baud() {
            std::format!("txtest: {} {}{} at {} % armed", arm.signal.name(), arm.baud, if arm.shaped { " rc" } else { "" }, arm.level)
        } else {
            std::format!("txtest: {} at {} % armed", arm.signal.name(), arm.level)
        };
        assert!(raw.contains(&title), "{line}: {raw}");
        assert_eq!(raw.contains("bypass: all AF TX filters off (REG_47 0 = 1), soft limiter off"), arm.bypass, "{line}: {raw}");
        match arm.eq {
            Eq::Eq => assert!(raw.contains("eq: flat 100 Hz-5 kHz, 95 taps, +1.3 dB at 1 kHz; REG_7D 0x3F, REG_40 0x50A;"), "{raw}"),
            Eq::Eq8 => assert!(raw.contains("eq8: flat 100 Hz-8 kHz, 127 taps, -0.6 dB at 1 kHz; REG_7D 0x3F, REG_40 0x57A;"), "{raw}"),
            Eq::Off => assert!(!raw.contains(" taps, "), "{raw}"),
        }
        r.send(b"\r");
    }
    for bad in [
        "txtest fsk", "txtest fsk 299", "txtest fsk 9601", "txtest gmsk", "txtest gmsk 4800 rc", "txtest g3ruh 9600", "txtest g3ruh rc",
        "txtest fsk 1200 data rc", "txtest sweep voice bypass", "txtest sweep voice eq", "txtest sweep bypass bypass", "txtest sweep eq eq8",
        "txtest sweep bypass data", "txtest sweep eq 100 bypass", "txtest afsk-bk bypass", "txtest suite eq", "txtest sweep eq5",
    ] {
        r.cmd(bad);
        assert!(r.bottom().contains("txtest 4fsk|sweep|afsk"), "{bad}: {}", r.bottom());
        assert_eq!(r.term.debug_req(), None, "{bad}: nothing sent");
    }
}

#[cfg(feature = "bench")]
/// TERM-34: `txtest suite` arms the suite (no filters word, no level: refused with either); the armed answer says
/// what it runs; asked while it runs, the section and the time to the end tone, then the hold; the Radio task's
/// refusal for a TX timeout too short prints as one.
#[test]
fn debug_txtest_suite() {
    use ferrite_core::txtest::{Arm, Ask, Filters, Signal};
    let mut r = Rig::main();
    let arm = Arm { filters: Filters::Data, ..Arm::new(Signal::Suite) };
    r.cmd("txtest suite");
    assert_eq!(r.serve(4, regs), vec![DebugReq::TxTest(Ask::Set(Some(arm)))]);
    let raw = r.raw();
    assert!(raw.contains("txtest: suite at 100 % armed for the next PTT key-up"), "{raw}");
    assert!(raw.contains("suite: 7 sections, 60 s, each after a marker tone (400-700 Hz); then 1 kHz until release"), "{raw}");
    r.send(b"\r");
    for bad in ["txtest suite data", "txtest suite 100", "txtest suite voice 50"] {
        r.cmd(bad);
        assert!(r.bottom().contains("txtest 4fsk|sweep|afsk"), "{bad}: {}", r.bottom());
        assert_eq!(r.term.debug_req(), None, "{bad}: nothing sent");
    }
    for (section, left_ms, want) in [
        (0, 59_000, "txtest suite running: the lead-in, 59 s to the end tone"),
        (4, 30_500, "txtest suite running: section 4 of 7 (AFSK from the tone generator per REG_40, then data and flat), 31 s to the end tone"),
        (8, 0, "txtest suite done: holding 1 kHz until you release PTT"),
    ] {
        r.cmd("txtest");
        assert_eq!(r.serve(4, |_| DebugAns::TxSuite { section, left_ms }), vec![DebugReq::TxTest(Ask::Show)]);
        assert!(r.raw().contains(want), "{}", r.raw());
        r.send(b"\r");
    }
    r.cmd("txtest suite");
    r.serve(4, |_| DebugAns::Refused("the suite runs 60 s: set the TX timeout (0 8 1) to 90 s or more first"));
    assert!(r.raw().contains("refused: the suite runs 60 s"), "{}", r.raw());
}

#[cfg(feature = "bench")]
#[test]
fn debug_txtest_rad950() {
    use ferrite_core::txtest::{Arm, Ask, Signal, RAD950_DEV_DEFAULT};
    let mut r = Rig::main();
    r.cmd("txtest rad950");
    assert_eq!(r.serve(4, regs), vec![DebugReq::TxTest(Ask::Set(Some(Arm::new(Signal::Rad950))))]);
    assert!(r.raw().contains(&std::format!("txtest: rad950 (dev {RAD950_DEV_DEFAULT}) at 100 % armed")), "{}", r.raw());
    assert!(r.raw().contains("rad950: BK4829 PLL REG_38/39 (50 Hz/tone; cal +500 Hz steps), REG_30 VCO toggle"), "{}", r.raw());
    r.send(b"\r");

    r.cmd("txtest rad950 220 50");
    assert_eq!(r.serve(4, regs), vec![DebugReq::TxTest(Ask::Set(Some(Arm { baud: 220, level: 50, ..Arm::new(Signal::Rad950) })))]);
    assert!(r.raw().contains("txtest: rad950 (dev 220) at 50 % armed"), "{}", r.raw());
    r.send(b"\r");

    r.cmd("txtest rad950-cal 150");
    assert_eq!(r.serve(4, regs), vec![DebugReq::TxTest(Ask::Set(Some(Arm { baud: 150, ..Arm::new(Signal::Rad950Cal) })))]);
    r.send(b"\r");

    for bad in ["txtest rad950 0", "txtest rad950 4096", "txtest rad950 180 9", "txtest rad950-cal 180 100"] {
        r.cmd(bad);
        assert!(r.bottom().contains("txtest 4fsk|sweep|afsk"), "{bad}: {}", r.bottom());
        assert_eq!(r.term.debug_req(), None, "{bad}: nothing sent");
    }
}

#[cfg(feature = "bench")]
/// TERM-41: `bk save` asks Battery Save's cycle; `lp`, `normal` and ON OFF [HOLD [RECENT]] set it; out of range or
/// extra words are refused before anything is sent; `bk save` and `lcd` alone only read (TERM-25).
#[test]
fn debug_bk_save() {
    use ferrite_core::rxsleep::{Cycle, LOW_POWER};
    let mut r = Rig::main();
    r.cmd("bk save");
    assert_eq!(r.serve(4, regs), vec![DebugReq::Save(None)]);
    assert!(r.raw().contains("bk save: the level's 300/500 ms"), "{}", r.raw());
    for (cmd, want) in [
        ("bk save lp", Some(Some(LOW_POWER))),
        ("bk save normal", Some(None)),
        ("bk save 180 800", Some(Some(Cycle { on_ms: 180, off_ms: 800, hold_ms: 0, recent_ms: 0 }))),
        ("BK SAVE 300 300 10 60", Some(Some(Cycle { on_ms: 300, off_ms: 300, hold_ms: 10_000, recent_ms: 60_000 }))),
    ] {
        r.send(b"\r");
        r.cmd(cmd);
        assert_eq!(r.serve(4, regs), vec![DebugReq::Save(want)], "{cmd}");
    }
    assert!(r.raw().contains("300/300 ms, hold 10 s, then 300/300 for 60 s (override)"), "{}", r.raw());
    r.send(b"\r");
    for bad in ["bk save 5", "bk save 180", "bk save 180 9000", "bk save 180 800 4000", "bk save 1 2 3 4 5", "bk save lp 3"] {
        r.cmd(bad);
        assert!(r.bottom().contains("bk save ["), "{bad}: {}", r.bottom());
        assert_eq!(r.term.debug_req(), None, "{bad}: nothing sent");
    }
    assert!(!super::cmd::writes(b"bk save") && !super::cmd::writes(b"lcd"));
    assert!(super::cmd::writes(b"bk save lp") && super::cmd::writes(b"lcd bl 30"));
}

#[cfg(feature = "bench")]
/// TERM-40: `lcd` asks the display's bench state; each change is one request; bad words are refused before
/// anything is sent.
#[test]
fn debug_lcd() {
    use ferrite_core::msg::LcdSet;
    let mut r = Rig::main();
    r.cmd("lcd");
    assert_eq!(r.serve(4, regs), vec![DebugReq::Lcd(LcdSet::Ask)]);
    assert!(r.raw().contains("lcd: idle on, partial off, sleep off, fps normal, bl normal, keys normal"), "{}", r.raw());
    for (cmd, want) in [
        ("lcd bl 30", LcdSet::Bl(Some(30))),
        ("lcd keys 0", LcdSet::Keys(Some(0))),
        ("lcd bl normal", LcdSet::Bl(None)),
        ("lcd fps 1", LcdSet::Fps(1)),
        ("lcd fps 0", LcdSet::Fps(0)),
        ("LCD IDLE OFF", LcdSet::Idle(false)),
        ("lcd partial on", LcdSet::Partial(true)),
        ("lcd sleep on", LcdSet::Sleep(true)),
        ("lcd reset", LcdSet::Reset),
        ("lcd bus", LcdSet::Bus),
    ] {
        r.send(b"\r");
        r.cmd(cmd);
        assert_eq!(r.serve(4, regs), vec![DebugReq::Lcd(want)], "{cmd}");
    }
    r.send(b"\r");
    r.cmd("lcd bl 30");
    r.serve(4, regs);
    assert!(r.raw().contains("bl 30"), "{}", r.raw());
    r.send(b"\r");
    for bad in ["lcd bl 101", "lcd fps 21", "lcd idle maybe", "lcd bright 3", "lcd sleep", "lcd bl 3 4", "lcd bus 2"] {
        r.cmd(bad);
        assert!(r.bottom().contains("lcd [idle"), "{bad}: {}", r.bottom());
        assert_eq!(r.term.debug_req(), None, "{bad}: nothing sent");
    }
}

#[cfg(feature = "bench")]
/// TERM-40, DISP-29: `lcd paint old` brings back clear-then-paint (and each band drawing its region afresh), `lcd
/// paint new` paints each pixel once, `lcd paint` says which; nothing goes to the firmware; bad words are refused.
#[test]
fn debug_lcd_paint() {
    use ferrite_ui::canvas::single_pass;
    let mut r = Rig::main();
    assert!(single_pass(), "new by default");
    for (cmd, on, says) in [("lcd paint old", false, "paint old"), ("lcd paint", false, "paint old"), ("LCD PAINT NEW", true, "paint new")] {
        r.cmd(cmd);
        assert_eq!(r.term.debug_req(), None, "{cmd}: nothing sent");
        assert_eq!(single_pass(), on, "{cmd}: {}", r.raw());
        assert!(r.bottom().contains(says), "{cmd}: {}", r.bottom());
    }
    for bad in ["lcd paint maybe", "lcd paint old 2"] {
        r.cmd(bad);
        assert!(r.bottom().contains("lcd [idle"), "{bad}: {}", r.bottom());
        assert!(single_pass(), "{bad}: unchanged");
    }
}

#[cfg(feature = "bench")]
/// TERM-31: `bk settle` asks the waits, `bk settle N` sets both to N ms, `bk settle W S` each; 0 is stock's; out of
/// range or extra words are refused here, before anything is sent. `bk settle` alone is a read (TERM-25).
#[test]
fn debug_bk_settle() {
    use ferrite_core::settle::Settle;
    let mut r = Rig::main();
    r.cmd("bk settle");
    assert_eq!(r.serve(4, regs), vec![DebugReq::Settle(None)]);
    assert!(r.raw().contains("bk settle: wake 2 ms, sleep 1 ms"), "{}", r.raw());
    r.send(b"\r");
    r.cmd("bk settle 5");
    assert_eq!(r.serve(4, regs), vec![DebugReq::Settle(Some(Settle { wake_ms: 5, sleep_ms: 5 }))]);
    assert!(r.raw().contains("wake 5 ms, sleep 5 ms"), "{}", r.raw());
    r.send(b"\r");
    r.cmd("BK SETTLE 10 0");
    assert_eq!(r.serve(4, regs), vec![DebugReq::Settle(Some(Settle { wake_ms: 10, sleep_ms: 0 }))]);
    r.send(b"\r");
    r.cmd("bk settle 0");
    assert_eq!(r.serve(4, regs), vec![DebugReq::Settle(Some(Settle::STOCK))]);
    assert!(r.raw().contains("wake 0 ms, sleep 0 ms (stock's: no wait)"), "{}", r.raw());
    r.send(b"\r");
    for bad in ["bk settle 21", "bk settle 2 21", "bk settle 2 1 0", "bk settle x", "bk settle -1"] {
        r.cmd(bad);
        assert!(r.bottom().contains("bk settle [WAKE [SLEEP]]"), "{bad}: {}", r.bottom());
        assert_eq!(r.term.debug_req(), None, "{bad}: nothing sent");
    }
    assert!(!super::cmd::writes(b"bk settle") && super::cmd::writes(b"bk settle 2") && super::cmd::writes(b"bk 0 30"));
}

#[cfg(feature = "bench")]
/// TERM-53: `bk spi` asks the bus's read and write half periods, `bk spi 2.5 500` runs 500 rounds on each chip at
/// 2.5 us (ID read, REG_48 written and read back), `bk spi use 5 5` sets the speeds; out of range or extra words are
/// refused before anything is sent.
#[test]
fn debug_bk_spi() {
    let mut r = Rig::main();
    r.cmd("bk spi");
    assert_eq!(r.serve(4, regs), vec![DebugReq::Spi { half: None, reads: 0, set: None }]);
    assert!(r.raw().contains("bk spi: reads 3.0 us, writes 1.5 us"), "{}", r.raw());
    r.send(b"\r");
    r.cmd("bk spi 2.5 500");
    assert_eq!(r.serve(4, regs), vec![DebugReq::Spi { half: Some(25), reads: 500, set: None }]);
    assert!(r.raw().contains("bk spi: half 2.5 us, 500 rounds a chip: chip 0 reads 0 wrong, writes 0 wrong chip 1 reads 0 wrong, writes 0 wrong; 250 us an operation; now reads 3.0 us, writes 1.5 us"), "{}", r.raw());
    r.send(b"\r");
    r.cmd("bk spi 1");
    assert_eq!(r.serve(4, regs), vec![DebugReq::Spi { half: Some(10), reads: 2000, set: None }]);
    assert!(r.raw().contains("chip 1 reads 3 wrong, writes 0 wrong; 250 us an operation"), "{}", r.raw());
    r.send(b"\r");
    r.cmd("bk spi use 5 4.5");
    assert_eq!(r.serve(4, regs), vec![DebugReq::Spi { half: None, reads: 0, set: Some([50, 45]) }]);
    assert!(r.raw().contains("bk spi: reads 5.0 us, writes 4.5 us"), "{}", r.raw());
    r.send(b"\r");
    assert!(!super::cmd::writes(b"bk spi") && super::cmd::writes(b"bk spi 2") && super::cmd::writes(b"bk spi use 5 5"));
    for bad in ["bk spi 0", "bk spi 6", "bk spi 0.9", "bk spi 2.55", "bk spi 2.", "bk spi .5", "bk spi 2 0", "bk spi 2 6000", "bk spi 2 10 x", "bk spi use", "bk spi use 3", "bk spi use 3 0.5", "bk spi use 3 2 1"] {
        r.cmd(bad);
        assert_eq!(r.serve(4, regs), vec![], "{bad}");
        r.send(b"\r");
    }
}

#[cfg(feature = "bench")]
/// WATCH-17.1: `bk turns` reads each slot's turns and ms, `bk turns reset` reads then zeroes them; extra words are
/// refused before anything is sent.
#[test]
fn debug_bk_turns() {
    let mut r = Rig::main();
    r.cmd("bk turns");
    assert_eq!(r.serve(4, regs), vec![DebugReq::Turns { reset: false }]);
    assert!(r.raw().contains("bk turns: A 27 4050 ms, B 27 1890 ms, C 0 0 ms; steps: A 40 2800 ms, B 0 0 ms, C 0 0 ms; tunes 94, 5000 us each, 9100 us max"), "{}", r.raw());
    r.send(b"\r");
    r.cmd("bk turns reset");
    assert!(!super::cmd::writes(b"bk turns"));
    assert_eq!(r.serve(4, regs), vec![DebugReq::Turns { reset: true }]);
    assert!(r.raw().contains("9100 us max (reset)"), "{}", r.raw());
    r.send(b"\r");
    for bad in ["bk turns x", "bk turns reset now"] {
        r.cmd(bad);
        assert!(r.bottom().contains("bk turns [reset]"), "{bad}: {}", r.bottom());
        assert_eq!(r.term.debug_req(), None, "{bad}: nothing sent");
    }
}

#[cfg(feature = "bench")]
/// TERM-27: `bt` sends only queries; the answer prints as text (control bytes as dots); settings are refused
/// before anything is sent.
#[test]
fn debug_bt_queries_only() {
    let mut r = Rig::main();
    r.cmd("bt AT+VERS?");
    let mut text = [0u8; 16];
    text[..8].copy_from_slice(b"AT+VERS?");
    assert_eq!(r.serve(4, regs), vec![DebugReq::Bt { text, n: 8 }]);
    assert!(r.raw().contains("bt AT+VERS? -> V1.0..OK"), "{}", r.raw());
    r.send(b"\r");
    for bad in ["bt AT+BAUD=9600", "bt AT+RST", "bt AT+NAME=Ferrite"] {
        r.cmd(bad);
        assert!(r.bottom().contains("refused"), "{bad}: {}", r.bottom());
        assert_eq!(r.term.debug_req(), None, "{bad}: nothing sent");
    }
}

#[cfg(feature = "bench")]
/// TERM-27: `bt stat` shows USART1's rate, the module's power and the byte and error counts; `bt baud N` restarts
/// USART1 at one of the listed rates (the MCU's side: nothing goes to the module), anything else is refused.
#[test]
fn debug_bt_stat_and_baud() {
    let mut r = Rig::main();
    r.cmd("bt stat");
    assert_eq!(r.serve(4, regs), vec![DebugReq::BtStat { baud: 0, pace: None }]);
    assert!(r.raw().contains("bt: USART1 9600 baud, module on"), "{}", r.raw());
    assert!(r.raw().contains("in 12 out 340 bytes, framing 1, overrun 0"), "{}", r.raw());
    r.send(b"\r");
    r.cmd("bt baud 115200");
    assert_eq!(r.serve(4, regs), vec![DebugReq::BtStat { baud: 115_200, pace: None }]);
    r.send(b"\r");
    for bad in ["bt baud 12345", "bt baud", "bt baud 9600 x", "bt stat x", "bt pace", "bt pace 20", "bt pace 256 30", "bt pace 20 1001", "bt pace 20 30 x", "bt pace x 30"] {
        r.cmd(bad);
        assert_eq!(r.term.debug_req(), None, "{bad}: nothing sent");
    }
}

#[cfg(feature = "bench")]
/// LINK-36, TERM-27: `bt stat` shows the Bluetooth pacing; `bt pace B G` sets it (RAM only), `bt pace 0 0` sends
/// back to back.
#[test]
fn debug_bt_pace() {
    use ferrite_core::link::BtPace;
    let mut r = Rig::main();
    r.cmd("bt stat");
    assert_eq!(r.serve(4, regs), vec![DebugReq::BtStat { baud: 0, pace: None }]);
    assert!(r.raw().contains("pace: 20 B bursts, 50 ms gaps (~282 B/s)"), "{}", r.raw());
    r.send(b"\r");
    r.cmd("bt pace 20 10");
    assert_eq!(r.serve(4, regs), vec![DebugReq::BtStat { baud: 0, pace: BtPace::new(20, 10) }]);
    assert!(r.raw().contains("pace: 20 B bursts, 10 ms gaps (~648 B/s)"), "{}", r.raw());
    r.send(b"\r");
    r.cmd("bt pace 0 0");
    assert_eq!(r.serve(4, regs), vec![DebugReq::BtStat { baud: 0, pace: BtPace::new(0, 0) }]);
    assert!(r.raw().contains("pace: off, back to back (~960 B/s)"), "{}", r.raw());
}

#[cfg(feature = "bench")]
/// TERM-23: a register read and a write print their value and bits; the read-back is shown.
#[test]
fn debug_bk_read_and_write() {
    let mut r = Rig::main();
    r.cmd("bk 0 30");
    assert_eq!(r.serve(4, regs), vec![DebugReq::BkRead { chip: 0, reg: 0x30 }]);
    assert!(r.raw().contains("bk0 REG_30 = 0x3030   0011 0000 0011 0000"), "{}", r.raw());
    r.send(b"\r");
    r.cmd("bk 1 0x47 6142");
    assert_eq!(r.serve(4, regs), vec![DebugReq::BkWrite { chip: 1, reg: 0x47, val: 0x6142 }]);
    assert!(r.raw().contains("bk1 REG_47 <- 0x6142, reads back 0x6142"), "{}", r.raw());
}

#[cfg(feature = "bench")]
/// TERM-23: the PA bit, the Tx DSP with the PLL on (decision 317), and the GPIO registers, are refused before anything
/// is asked.
#[test]
fn debug_bk_refuses_tx_and_gpio() {
    let mut r = Rig::main();
    r.cmd("bk 0 30 BFF9");
    assert!(r.bottom().contains("refused: REG_30's PA Gain bit"), "{}", r.bottom());
    assert_eq!(r.term.debug_req(), None);
    // decision 317: the Tx DSP only with the PLL off
    r.cmd("bk 0 30 BFF3");
    assert!(r.bottom().contains("refused: REG_30's Tx DSP bit only with the PLL/VCO bits"), "{}", r.bottom());
    assert_eq!(r.term.debug_req(), None);
    r.cmd("bk 1 33 0000");
    assert!(r.bottom().contains("REG_33 and REG_34"), "{}", r.bottom());
    r.cmd("bk 0 30 BFF1");
    assert_eq!(r.serve(2, regs), vec![DebugReq::BkWrite { chip: 0, reg: 0x30, val: 0xBFF1 }], "receive bits are fine");
    r.send(b"\r");
    r.cmd("bk 2 30");
    assert!(r.bottom().contains("bk CHIP REG"), "{}", r.bottom());
    r.cmd("bk 0 80");
    assert!(r.bottom().contains("bk CHIP REG"), "{}", r.bottom());
}

#[cfg(feature = "bench")]
/// TERM-23: `bk N dump` reads all 128 registers, one request at a time, eight to a line.
#[test]
fn debug_bk_dump() {
    let mut r = Rig::main();
    r.cmd("bk 1 dump");
    let seen = r.serve(200, regs);
    assert_eq!(seen.len(), 128);
    assert_eq!(seen[127], DebugReq::BkRead { chip: 1, reg: 0x7F });
    let raw = r.raw();
    assert!(raw.contains("bk1 00: 0000 0101 0202 0303 0404 0505 0606 0707"), "{raw}");
    assert!(raw.contains("bk1 78: 7878 7979 7A7A 7B7B 7C7C 7D7D 7E7E 7F7F"), "{raw}");
    assert!(raw.contains("Enter or Esc returns"), "{raw}");
}

#[cfg(feature = "bench")]
/// TERM-22: memory in 64-byte requests, 16 bytes a line with their characters; words for peripherals.
#[test]
fn debug_mem() {
    let mut r = Rig::main();
    r.cmd("mem 20000000 100");
    let seen = r.serve(20, regs);
    assert_eq!(seen, vec![DebugReq::Mem { addr: 0x2000_0000, n: 64, words: false }, DebugReq::Mem { addr: 0x2000_0040, n: 36, words: false }]);
    let raw = r.raw();
    assert!(raw.contains("20000000  00 01 02 03 04 05 06 07 08 09 0A 0B 0C 0D 0E 0F  |................|"), "{raw}");
    assert!(raw.contains("20000060  60 61 62 63"), "{raw}");
    assert!(!raw.contains("20000070"), "100 bytes end at 0x63: {raw}");
    r.send(b"\r");
    r.cmd("mem32 40010808 2");
    assert_eq!(r.serve(6, regs), vec![DebugReq::Mem { addr: 0x4001_0808, n: 8, words: true }]);
    assert!(r.raw().contains("40010808  0B0A0908 0F0E0D0C"), "{}", r.raw());
    r.send(b"\r");
    r.cmd("mem32 40010809");
    assert!(r.bottom().contains("word-aligned"), "{}", r.bottom());
}

#[cfg(feature = "bench")]
/// TERM-22: an address outside the readable map (`ferrite_core::memmap`) is refused before anything goes to the
/// radio, saying what can be read, all of it within the 80-column bottom row; the AT32's user system data at
/// 1FFFF800 (STM32's option-byte address) reads.
#[test]
fn debug_mem_refuses_unsafe_addresses() {
    let mut r = Rig::main();
    for (c, says) in [
        ("mem 50000000", ferrite_core::memmap::READABLE),
        ("mem32 1FFFF000", ferrite_core::memmap::READABLE),
        ("mem 080FFFF8 16", "only 080xxxxx"),
        ("mem 40010808", "use mem32"),
    ] {
        r.cmd(c);
        assert!(r.serve(4, regs).is_empty(), "{c}: nothing sent");
        assert!(r.bottom().contains(says), "{c}: {}", r.bottom());
    }
    r.cmd("mem 1FFFF800 48");
    assert_eq!(r.serve(6, regs), vec![DebugReq::Mem { addr: 0x1FFF_F800, n: 48, words: false }]);
}

#[cfg(feature = "bench")]
/// TERM-22: `gpio` reads each port's input and output registers, A to E.
#[test]
fn debug_gpio() {
    let mut r = Rig::main();
    r.cmd("gpio");
    let seen = r.serve(20, regs);
    assert_eq!(seen.len(), 5);
    assert_eq!(seen[4], DebugReq::Mem { addr: 0x4001_1808, n: 8, words: true });
    assert!(r.raw().contains("PA  in 0000 1001 0000 1000   out 0000 1101 0000 1100"), "{}", r.raw());
    assert!(r.raw().contains("  PE  in "), "{}", r.raw());
}

#[cfg(feature = "bench")]
/// TERM-24: SI4732 commands and properties go as typed; the reply starts with the status byte.
#[test]
fn debug_si() {
    let mut r = Rig::main();
    r.cmd("si prop 4001");
    let seen = r.serve(4, regs);
    assert!(matches!(seen[0], DebugReq::Si { cmd: [0x13, 0, 0x40, 0x01, ..], n: 4, reply: 4 }), "{seen:?}");
    assert!(r.raw().contains("si 13 00 40 01 -> 80 00 00 00"), "{}", r.raw());
    r.send(b"\r");
    r.cmd("si 10 00 reply 9");
    assert!(matches!(r.serve(4, regs)[0], DebugReq::Si { cmd: [0x10, 0, ..], n: 2, reply: 9 }));
}

/// TERM-21: no answer in 2 s ends the command; Esc stops one, and a late answer is dropped. (`gps`: in every build.)
#[test]
fn debug_no_answer_and_stop() {
    let mut r = Rig::main();
    r.cmd("gps");
    assert!(r.term.debug_req().is_some());
    for _ in 0..25 {
        r.poll();
    }
    assert!(r.raw().contains("no answer"), "{}", r.raw());
    r.send(b"\r");
    r.cmd("gps");
    assert!(r.term.debug_req().is_some());
    r.send(b"\x1b");
    for _ in 0..3 {
        r.poll();
    }
    r.term.debug_answer(gga(b"$GNGGA,1", 0));
    r.poll();
    assert!(!r.raw().contains("GGA, 0 s ago"), "stopped: {}", r.raw());
    assert!(matches!(r.term.con, Con::Done | Con::Off), "{:?}", r.term.con);
}

/// TERM-38 (decision 151): a user build (no `bench` feature) knows the bench commands by name and refuses each,
/// typed or with arguments, saying a bench build has them; nothing goes to the firmware. `gps` alone (the last
/// fix) is in every build, its CASIC sends aren't. The help says where the bench commands are.
#[cfg(not(feature = "bench"))]
#[test]
fn user_build_refuses_bench_commands() {
    let mut r = Rig::main();
    for c in ["mem 20000000 16", "mem32 40010808", "gpio", "bk 0 30", "bk 1 47 6142", "bk 0 dump", "bk settle 2", "si 10 00", "bt AT+VERS?", "rx both", "rx", "mic", "af", "meter", "meter on", "synthfsk cal 1000 0 0", "pin pa15 0", "pin pc9 u", "txtest suite", "txtest", "TXTEST 4fsk", "gps query 1", "gps nmea off", "gps standby 60 quiet", "lcd bl 30", "bk rxaf 700", "fsk", "fsk rx 2400", "fsk tx ffsk1800 5", "crash fault", "crash hang"] {
        r.cmd(c);
        assert!(r.bottom().contains("not in this build: a bench build has it"), "{c}: {}", r.bottom());
        assert_eq!(r.term.debug_req(), None, "{c}: nothing sent");
    }
    assert_eq!(super::cmd::BENCH_ONLY.len(), 16);
    r.cmd("gps");
    assert_eq!(r.serve(4, regs), vec![DebugReq::Gga], "gps alone is every build's");
    r.send(b"\r");
    r.cmd("help");
    for _ in 0..4 {
        r.poll();
    }
    let raw = r.raw();
    assert!(raw.contains("not in this build; a bench build (bring-up, tx-bench) has them"), "{raw}");
    assert!(!raw.contains("txtest suite           every transmit test"), "{raw}");
}

/// TERM-38: a bench build's help lists the bench commands.
#[cfg(feature = "bench")]
#[test]
fn bench_build_lists_bench_commands() {
    let mut r = Rig::main();
    r.cmd("help");
    for _ in 0..4 {
        r.poll();
    }
    let raw = r.raw();
    assert!(raw.contains("Bench builds only") && raw.contains("txtest suite") && raw.contains("bk settle 2 1"), "{raw}");
    assert!(!raw.contains("not in this build"), "{raw}");
}

// ---- The channel list's tools (TERM-14), the radio's editor (TERM-17), colour (TERM-18) ----

impl Rig<Sample> {
    /// The channel list, every channel.
    fn list() -> Self {
        let mut r = Rig::main();
        r.send(b"2");
        assert_eq!(r.term.page(), Page::Channels);
        r
    }

    /// Polls until a change to several channels is done.
    fn settle(&mut self) {
        for _ in 0..200 {
            if !self.term.bulk_on() {
                break;
            }
            self.poll();
        }
        assert!(!self.term.bulk_on(), "the change finished");
        self.poll();
    }

    fn nums(&self) -> Vec<u16> {
        (0..crate::chan::MAX_CHANNELS).filter(|&n| self.store.get(n).is_some()).collect()
    }
}

/// TERM-14: S sorts by the next column, the header says which; the page follows the order.
#[test]
fn list_sorts_by_column() {
    let mut r = Rig::list();
    assert!(r.vt.has("sort: number"), "{}", r.vt.text());
    r.send(b"s");
    assert!(r.vt.has("sort: name"), "{}", r.vt.text());
    let mut names: Vec<Vec<u8>> = r.nums().iter().map(|&n| r.store.get(n).unwrap().name().to_ascii_lowercase()).collect();
    names.sort();
    let first = r.term.selected(&r.store).unwrap();
    assert_eq!(first.name().to_ascii_lowercase(), names[0], "the first row is the first name");
    // Down then End then Home move through the sorted list.
    r.send(b"\x1b[B");
    assert_eq!(r.term.selected(&r.store).unwrap().name().to_ascii_lowercase(), names[1]);
    r.send(b"\x1b[F");
    assert_eq!(r.term.selected(&r.store).unwrap().name().to_ascii_lowercase(), *names.last().unwrap());
    r.send(b"\x1b[H");
    assert_eq!(r.term.selected(&r.store).unwrap().name().to_ascii_lowercase(), names[0]);
    // RX: by frequency.
    r.send(b"s");
    assert!(r.vt.has("sort: RX"));
    let low = r.nums().iter().map(|&n| r.store.get(n).unwrap().rx).min().unwrap();
    assert_eq!(r.term.selected(&r.store).unwrap().rx, low);
}

/// TERM-14: / finds part of a name or a frequency, from the row after the selection; Enter alone finds the
/// same again; no match says so.
#[test]
fn list_finds() {
    let mut r = Rig::list();
    let target = r.store.get(*r.nums().last().unwrap()).unwrap();
    let part = std::str::from_utf8(&target.name()[..3]).unwrap().to_lowercase();
    r.send(b"/");
    assert!(r.vt.has("Find (name or MHz"), "{}", r.vt.text());
    r.send(part.as_bytes());
    r.send(b"\r");
    assert!(r.term.selected(&r.store).unwrap().name().to_ascii_lowercase().windows(3).any(|w| w == part.as_bytes()));
    r.send(b"/");
    r.send(b"zzqq\r");
    assert!(r.bottom().contains("No channel here matches \"zzqq\""), "{}", r.bottom());
    // A frequency's start.
    let f = mhz5(target.rx);
    r.send(b"/");
    r.send(&f.as_bytes()[..7]);
    r.send(b"\r");
    assert!(mhz5(r.term.selected(&r.store).unwrap().rx).as_bytes().starts_with(&f.as_bytes()[..7]));
}

/// TERM-14: Space marks (and moves down), the header counts them; D asks with the count, n keeps, y deletes
/// them one write at a time.
#[test]
fn list_marks_and_deletes() {
    let mut r = Rig::list();
    let before = r.nums();
    r.send(b"  ");
    assert!(r.vt.has("2 marked"), "{}", r.vt.text());
    assert!(r.term.marked(before[0]) && r.term.marked(before[1]));
    assert_eq!(r.term.selected(&r.store).unwrap().num, before[2], "Space moves down");
    r.send(b"d");
    assert!(r.vt.has("Delete 2 marked channels? y deletes"), "{}", r.vt.text());
    r.send(b"n");
    assert!(r.bottom().contains("Nothing deleted"));
    assert_eq!(r.nums(), before);
    r.send(b"d");
    r.send(b"y");
    r.settle();
    assert_eq!(r.nums(), before[2..].to_vec(), "the two marked ones are gone");
    assert!(r.bottom().contains("2 channels deleted"), "{}", r.bottom());
    assert_eq!(r.term.marks_n(), 0);
}

/// TERM-14: M moves the selected channel to a zone (by number or name, or none); C copies it to the first
/// free number.
#[test]
fn list_moves_and_copies() {
    let mut r = Rig::list();
    let c = r.term.selected(&r.store).unwrap();
    r.send(b"m");
    r.send(b"none\r");
    r.settle();
    assert_eq!(r.store.get(c.num).unwrap().zone, NO_ZONE);
    let z1 = r.store.zone_name(0).unwrap();
    r.send(b"m");
    r.send(z1.as_bytes());
    r.send(b"\r");
    r.settle();
    assert_eq!(r.store.get(c.num).unwrap().zone, 0, "by name");
    let free = r.store.free_num(0).unwrap();
    r.send(b"c\r");
    r.settle();
    let copy = r.store.get(free).expect("copied to the first free number");
    assert_eq!((copy.rx, copy.name()), (c.rx, c.name()));
    assert!(r.bottom().contains("1 channel copied"), "{}", r.bottom());
}

/// TERM-14: R renumbers the marked channels from a number, in their order, even over their own old numbers;
/// a number held by a channel that isn't moving is refused.
#[test]
fn list_renumbers() {
    let mut r = Rig::list();
    let before = r.nums();
    let (a, b) = (before[0], before[1]);
    let (ca, cb) = (r.store.get(a).unwrap(), r.store.get(b).unwrap());
    // Onto a channel that isn't moving: refused.
    r.send(b" ");
    r.send(b"r");
    r.send(std::format!("{}\r", before[2] + 1).as_bytes());
    assert!(r.bottom().contains("is taken by a channel that isn't moving"), "{}", r.bottom());
    // Both, to the first free numbers after the last channel.
    r.send(b" ");
    let to = before.last().unwrap() + 1;
    r.send(b"r");
    r.send(std::format!("{}\r", to + 1).as_bytes());
    r.settle();
    assert_eq!(r.store.get(to).unwrap().name(), ca.name());
    assert_eq!(r.store.get(to + 1).unwrap().name(), cb.name());
    assert!(r.store.get(a).is_none() && r.store.get(b).is_none());
    // Shift the two up by one: each lands where the other was (the order makes it safe).
    r.send(b"\x1b[F");
    r.send(b"\x1b[A ");
    r.send(b" ");
    assert_eq!(r.term.marks_n(), 2);
    r.send(b"r");
    r.send(std::format!("{}\r", to + 2).as_bytes());
    r.settle();
    assert_eq!(r.store.get(to + 1).unwrap().name(), ca.name());
    assert_eq!(r.store.get(to + 2).unwrap().name(), cb.name());
}

/// TERM-14: N makes a channel at the first free number and opens its editor.
#[test]
fn list_new_channel() {
    let mut r = Rig::list();
    let free = r.store.free_num(0).unwrap();
    let n = r.store.count();
    r.send(b"n");
    assert_eq!(r.term.page(), Page::Edit);
    assert_eq!(r.store.count(), n + 1);
    assert_eq!(r.term.edit().unwrap().num, free);
    assert!(r.bottom().contains("New channel") || r.vt.has("New channel"), "{}", r.vt.text());
}

/// TERM-17: a channel open in the radio's editor isn't changed from the terminal (the editor, `ch`, a bulk
/// change); a setting highlighted in the radio's menu isn't either.
#[test]
fn radio_editor_wins() {
    let mut r = Rig::list();
    let c = r.term.selected(&r.store).unwrap();
    let (z, pos) = (0..r.store.zones()).chain([NO_ZONE]).find_map(|z| (0..r.store.zone_len(z)).find(|&i| r.store.zone_at(z, i).unwrap().num == c.num).map(|i| (z, i))).unwrap();
    r.app.set_channel(r.app.active, z, pos, &r.store);
    r.app.screen = crate::Screen::ChEdit(crate::screens::chedit::ChEdit { cell: 0 });
    r.send(b"\r");
    for _ in 0..8 {
        r.send(b"\x1b[B");
    }
    r.send(b"\x1b[C");
    assert_eq!(r.store.get(c.num).unwrap(), c, "not written");
    assert!(r.vt.has("Open in the radio's editor"), "{}", r.vt.text());
    r.send(b"\x1b");
    r.cmd(&std::format!("ch {} power high", c.num + 1));
    assert!(r.bottom().contains("open in the radio's editor"), "{}", r.bottom());
    r.send(b"d");
    r.send(b"y");
    assert!(r.bottom().contains("open in the radio's editor"), "{}", r.bottom());
    assert!(r.store.get(c.num).is_some());
    // A setting: 0 7 1 highlighted in the radio's menu.
    r.app.screen = crate::Screen::Group(crate::screens::groups::Group { g: 7, sub: 0, row: 0 });
    r.cmd("set 0 7 1 Daylight");
    assert!(r.bottom().contains("open on the radio's menu"), "{}", r.bottom());
}

/// TERM-18: colour is off by default (reverse video and bold only); 0 9 9 3 turns it on.
#[test]
fn colour_is_optional() {
    let mut r = Rig::main();
    assert!(!r.raw().contains("\x1b[0;97;44m"));
    r.cmd("set 0 9 9 3 On");
    assert!(r.app.set.term_colour);
    r.send(b"\x0c");
    r.poll();
    assert!(r.raw().contains("\x1b[0;97;44m"), "the top line in white on blue");
    assert!(r.raw().contains("\x1b[1;36m"), "headings in cyan");
}

/// The settings plumbing as the menu and the terminal see it, written out in full and compared with
/// `settings.golden` (TERM-12, TERM-13, MENU-12, MENU-16, MENU-18). The golden file was written before the settings
/// moved into one table (Oct 13, 2026), so this pins that refactor and any later one to the same behaviour: every
/// menu path's value and default flag in three states (the sample radio, a changed one, one with out-of-range
/// stored values), the dump and the changed list, `get` on every path, the knob's steps and # on every path with
/// what each sends the Radio task, and `set` with a value that isn't one (its "one of" list walks every value).
/// Bless a deliberate change with FERRITE_BLESS=1. The user build's menu: bench builds add Cable log (TERM-28).
#[test]
#[cfg(not(feature = "bench"))]
fn settings_golden() {
    use crate::screens::groups;
    use std::fmt::Write as _;
    use std::string::ToString;

    // Every menu path: the nine groups' items and the third-level lists' items.
    let mut paths: Vec<Vec<u8>> = Vec::new();
    for g in 1..=9u8 {
        for i in 1..=groups::items(g, 0).len() as u8 {
            paths.push(vec![g, i]);
            for j in 1..=groups::items(g, i).len() as u8 {
                paths.push(vec![g, i, j]);
            }
        }
    }
    let strip = |s: &str| {
        // Escape sequences out, line ends as \n.
        let mut o = String::new();
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            if c == '\x1b' {
                if it.peek() == Some(&'[') {
                    it.next();
                    for d in it.by_ref() {
                        if d.is_ascii_alphabetic() {
                            break;
                        }
                    }
                } else {
                    it.next();
                }
            } else if c != '\r' {
                o.push(c);
            }
        }
        o
    };
    let said = |r: &mut Rig<Sample>, c: &str| {
        let n = r.vt.raw.len();
        r.cmd(c);
        r.poll();
        strip(&String::from_utf8_lossy(&r.vt.raw[n..]))
    };
    let state = |app: &App| std::format!("{:?} aprs {:?}", app.set, app.aprs_cfg());
    let mut out = String::new();
    let wired_all = |app: &App, dev: &Device, out: &mut String| {
        for p in &paths {
            let mut b = Buf::<24>::new();
            let w = groups::wired(app, dev, p, &mut b);
            let _ = writeln!(out, "wired {:?}: {:?} {}", p, w, String::from_utf8_lossy(b.as_bytes()));
        }
    };
    let ptxt = |p: &[u8]| p.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(" ");

    for pass in 0..3 {
        let mut r = Rig::main();
        match pass {
            1 => {
                // A changed radio: every settable path stepped a few times, a few fields set directly.
                for (k, p) in groups::settable().enumerate() {
                    for d in [1, 1, -1, (k % 3) as i32 + 1] {
                        groups::change(&mut r.app, p, d);
                    }
                }
                for &c in b"N0CALL" {
                    r.app.set.callsign.push(c);
                }
                r.app.set.fixed_on = true;
                r.app.set.same_codes[0] = 12345;
                r.app.set.pocsag_cap = 1234;
                r.app.set.pin_len = 4;
                r.app.set.pin_mode = 1;
                r.app.set.region = 2;
            }
            2 => {
                // Out-of-range stored values (a record from elsewhere): every u8 setting the menu shows.
                let s = &mut r.app.set;
                for v in [
                    &mut s.tone_level, &mut s.beep_vol, &mut s.vox_level, &mut s.vox_tail, &mut s.dtmf_ms,
                    &mut s.dtmf_delay, &mut s.scan_resume, &mut s.scan_prio, &mut s.fm_step,
                    &mut s.fm_sleep, &mut s.scope_bars, &mut s.scope_step, &mut s.scope_dwell, &mut s.scope_trig,
                    &mut s.backlight, &mut s.wake_on, &mut s.keypad_light, &mut s.leds, &mut s.auto_off,
                    &mut s.auto_lock, &mut s.tail_mode, &mut s.tail_phase, &mut s.end_tone, &mut s.cw_id,
                    &mut s.gps_rxtx, &mut s.coord_fmt, &mut s.units, &mut s.theme, &mut s.layout, &mut s.pon,
                    &mut s.cable_port, &mut s.kiss_port, &mut s.tz,
                ] {
                    *v = 250;
                }
                s.brightness = 0;
                s.brightness_night = 99;
            }
            _ => {}
        }
        while r.app.pop_cmd().is_some() {}
        let _ = writeln!(out, "== pass {pass}: {}", state(&r.app));
        wired_all(&r.app, &r.dev, &mut out);
        let _ = writeln!(out, "-- dump");
        for l in dump(&r.app, &r.store, &r.dev) {
            if l.starts_with("ch ") || l.starts_with("zone ") || l.starts_with("preset ") {
                continue;
            }
            let _ = writeln!(out, "{l}");
        }
        let _ = writeln!(out, "-- changed");
        let mut i = 0;
        let mut l = cmd::Line::new();
        while cmd::list_line(&r.app, &r.store, &r.dev, &ferrite_core::msg::LiveAll::default(), cmd::List::Changed, i, &mut l) {
            let _ = writeln!(out, "{}", String::from_utf8_lossy(l.as_bytes()));
            i += 1;
        }
        let _ = writeln!(out, "-- get");
        for p in &paths {
            let s = said(&mut r, &std::format!("get 0 {}", ptxt(p)));
            let _ = writeln!(out, "{}", s.trim());
        }
        let _ = writeln!(out, "-- knob and #");
        for p in &paths {
            for d in [1, -1, 2, -2, 3, 0] {
                let ok = groups::change(&mut r.app, p, d);
                let mut cmds = Vec::new();
                while let Some(c) = r.app.pop_cmd() {
                    cmds.push(c);
                }
                let mut b = Buf::<24>::new();
                let w = groups::wired(&r.app, &r.dev, p, &mut b);
                let _ = writeln!(out, "change {:?} {d}: {ok} {:?} {} {:?} {:?}", p, w, String::from_utf8_lossy(b.as_bytes()), cmds, r.app.screen());
            }
            groups::reset(&mut r.app, p);
            let mut cmds = Vec::new();
            while let Some(c) = r.app.pop_cmd() {
                cmds.push(c);
            }
            let _ = writeln!(out, "reset {:?}: {:?} {}", p, cmds, state(&r.app));
            r.app.go_home();
        }
        let _ = writeln!(out, "-- set");
        for p in groups::settable() {
            for v in ["xyzzy", "on", "off", "1", "2", "10", "none"] {
                let s = said(&mut r, &std::format!("set 0 {} {v}", ptxt(p)));
                let _ = writeln!(out, "{}", s.trim());
            }
        }
        let _ = writeln!(out, "-- end {}", state(&r.app));
    }

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/term/settings.golden");
    if std::env::var("FERRITE_BLESS").is_ok() {
        std::fs::write(&path, &out).unwrap();
    }
    let want = std::fs::read_to_string(&path).unwrap_or_default();
    if want != out {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/golden-diff");
        let _ = std::fs::create_dir_all(&dir);
        let new = dir.join("settings.golden.new");
        std::fs::write(&new, &out).unwrap();
        let line = want.lines().zip(out.lines()).position(|(a, b)| a != b).unwrap_or(0);
        panic!("settings differ from {} at line {} (written: {})", path.display(), line + 1, new.display());
    }
}

/// CHAN-2, TERM-12: a channel's position over the terminal: `ch N pos=lat,lon` (degrees), a grid square, or none;
/// the `ch` line writes it back (so a dump and a load keep it), and a whole line without pos= keeps it.
#[test]
fn channel_position_over_the_terminal() {
    use crate::data::Pos;
    let mut r = Rig::main();
    r.cmd("ch 12 pos=45.5,-75.25");
    assert_eq!(r.store.position(11), Some(Pos { lat: 455_000_000, lon: -752_500_000 }), "{}", r.bottom());
    r.send(b"\r");
    r.cmd("ch 12");
    assert!(r.raw().contains("zone=2 pos=45.5,-75.25"), "{}", r.raw());
    r.send(b"\r");
    r.cmd("ch 12 pos=-33.8688197,151.2092955");
    assert_eq!(r.store.position(11), Some(Pos { lat: -338_688_197, lon: 1_512_092_955 }), "seven decimals, south and east");
    r.cmd("ch 12 pos none");
    assert_eq!(r.store.position(11), None, "{}", r.bottom());
    r.cmd("ch 300 \"Hill\" 146.94 offset=-0.6 pos=FN25dk");
    assert_eq!(r.store.position(299), crate::screens::lists::grid_pos(b"FN25dk"), "{}", r.bottom());
    r.cmd("ch 300 \"Hill\" 146.94 offset=-0.6 power=mid");
    assert!(r.store.position(299).is_some(), "a whole line without pos= keeps the position");
    for bad in ["ch 300 pos=91,0", "ch 300 pos=45,181", "ch 300 pos=45.12345678,1", "ch 300 pos=north", "ch 300 pos"] {
        r.cmd(bad);
        assert!(r.bottom().contains("pos"), "{bad}: {}", r.bottom());
    }
    assert!(r.store.position(299).is_some(), "refused lines change nothing");
}

/// GPS-3, GPS-21, TERM-12: `set 0 6 9` types the fixed position (degrees or a grid square), `none` clears it.
#[test]
fn fixed_position_over_the_terminal() {
    use crate::data::Pos;
    let mut r = Rig::main();
    r.cmd("set 0 6 9 45.4215,-75.6972");
    assert!(r.app.set.fixed_on, "{}", r.bottom());
    assert_eq!(r.app.set.fixed, Pos { lat: 454_215_000, lon: -756_972_000 });
    assert!(r.bottom().contains("0 6 9 Fixed position: 45.4215,-75.6972"), "{}", r.bottom());
    r.cmd("get 0 6 9");
    assert!(r.bottom().contains("FN25"), "{}", r.bottom());
    r.cmd("set 0 6 9 FN31");
    assert_eq!(Some(r.app.set.fixed), crate::screens::lists::grid_pos(b"FN31"));
    r.cmd("set 0 6 9 here");
    assert!(r.bottom().contains("a grid square, or none"), "{}", r.bottom());
    r.cmd("set 0 6 9 None");
    assert!(!r.app.set.fixed_on, "{}", r.bottom());
}

/// MENU-31, TERM-12: `set` takes a frequency setting typed in MHz, with the entry box's checks.
#[test]
fn set_frequency_settings() {
    let mut r = Rig::main();
    r.cmd("set 0 5 4 145.495");
    assert!(r.bottom().contains("0 5 4 Frequency: 145.495"), "{}", r.bottom());
    assert_eq!(r.app.aprs_freq(), ferrite_core::freq::mhz(145) + 49_500);
    r.cmd("set 0 5 4 446.000");
    assert!(r.bottom().contains("APRS takes 64-300 MHz"), "{}", r.bottom());
    assert_eq!(r.app.aprs_freq(), ferrite_core::freq::mhz(145) + 49_500, "a refusal changes nothing");
    r.cmd("set 0 4 2 2 439.9875");
    assert!(r.bottom().contains("0 4 2 2 Frequency: 439.987"), "{}", r.bottom());
    assert_eq!(r.app.set.pocsag_freq, ferrite_core::freq::mhz(439) + 98_750);
    r.cmd("set 0 4 2 2 439.987"); // as `get` and `dump` show it: kept, not cut to 439.98700
    assert_eq!(r.app.set.pocsag_freq, ferrite_core::freq::mhz(439) + 98_750);
}

/// SQL-18.3 (decision 269): 0 9 5 says who decides the squelch, software by default; `set 0 9 5 chip` and
/// `set 0 9 5 software` switch it, and it is saved with the settings.
#[test]
fn squelch_decided_by_0_9_5() {
    let mut r = Rig::main();
    assert!(!r.app.settings().sq_chip, "software by default");
    r.cmd("set 0 9 5 chip");
    assert!(r.app.settings().sq_chip, "{}", r.bottom());
    r.cmd("set 0 9 5 software");
    assert!(!r.app.settings().sq_chip, "{}", r.bottom());
}

/// APRS-52.4: 0 5 5 7 set and read by name from the terminal, two-word names included.
#[test]
fn mic_e_by_name() {
    let mut r = Rig::main();
    r.cmd("set 0 5 5 7 en route");
    assert_eq!(r.app.aprs_cfg().mic_e, 2, "{}", r.bottom());
    r.cmd("set 0 5 5 7 Priority");
    assert_eq!(r.app.aprs_cfg().mic_e, 7);
    r.cmd("get 0 5 5 7");
    assert!(r.bottom().contains("Mic-E: Priority"), "{}", r.bottom());
    r.cmd("set 0 5 5 7 off");
    assert_eq!(r.app.aprs_cfg().mic_e, 0);
}

/// Keys and pages: escape sequences, the PIN prompt, the lists' movement keys, help per page.
mod nav;
/// The channel list at its edges (TERM-14).
mod list_more;
/// The command line's refusals and rare answers (TERM-12).
mod cmd_more;
/// Every page's bytes for a key script (TERM-11, TERM-16; decision 342).
mod pages_golden;
/// The pages that match the radio's own screens (TERM-48 to TERM-52).
mod parity;

/// TERM-8: every command's usage text fits the answer it goes out in ([`super::cmd::Msg`], 96 characters), so a
/// refusal is never cut off in the line-mode answer (`gps` lost its "35 s)" once, Oct 28, 2026).
#[test]
fn usage_texts_fit_the_answer() {
    let mut full = super::cmd::Msg::new();
    full.push(&[b'x'; 1000]); // a Buf keeps what fits
    let cap = full.len();
    for src in [include_str!("cmd.rs"), include_str!("debug.rs")] {
        for l in src.lines().filter(|l| l.contains("const USAGE: &str = \"")) {
            let text = l.split("= \"").nth(1).and_then(|t| t.rsplit_once("\";")).map(|t| t.0).unwrap();
            assert!(text.len() <= cap, "{} characters, over {cap}: {text}", text.len());
        }
    }
}

/// TEXT-7: `quick N` shows a quick message ("factory" after the factory text), `quick N "text"` sets it, `quick N
/// factory` puts the factory text back; | ~ { and a 68th character are refused, as is a tenth one; `quick N` alone
/// only reads (TERM-25), and a dump's load doesn't take it.
#[test]
fn quick_messages_in_the_terminal() {
    use crate::data::Data;
    let mut r = Rig::main();
    r.cmd("quick 1");
    assert_eq!(r.bottom().trim(), std::format!("quick 1 \"{}\" factory", std::str::from_utf8(crate::words::quick(0)).unwrap()));
    r.cmd("quick 3 \"Meet at \\\"the\\\" club\"");
    assert!(r.bottom().contains("Quick message 3 saved"), "{}", r.bottom());
    assert_eq!(r.store.quick(2), Some(Buf::from(b"Meet at \"the\" club")));
    r.cmd("quick 3");
    assert_eq!(r.bottom().trim(), "quick 3 \"Meet at \\\"the\\\" club\"");
    r.cmd("quick 3 \"Meet at \\\"the\\\" club\"");
    assert!(r.bottom().contains("unchanged"), "{}", r.bottom());
    for bad in ["quick 3 \"a|b\"", "quick 10 \"x\"", "quick 3 maybe"] {
        r.cmd(bad);
        assert_eq!(r.store.quick(2), Some(Buf::from(b"Meet at \"the\" club")), "{bad}: {}", r.bottom());
    }
    r.cmd(&std::format!("quick 3 \"{}\"", "x".repeat(68)));
    assert!(r.bottom().contains("67 characters"), "{}", r.bottom());
    r.cmd("quick 3 factory");
    assert_eq!(r.store.quick(2), None);
    assert!(!crate::term::cmd::writes(b"quick 3") && crate::term::cmd::writes(b"quick 3 factory"));
}

/// TERM-12: every command's answer, byte for byte, kept in `term/golden/` (typical and refused arguments, typed
/// and in a load), so a rework of the command parser shows any change in what a line says. `UPDATE_GOLDEN=1`
/// writes the file again (look at the diff).
#[test]
fn command_answers_golden() {
    const LINES: &[&str] = &[
        "", "# a note", "\"quoted\"", "frob", "help", "?", "HELP", "Get 0 7 1",
        "get", "get 0 7 1", "get 0 5 1", "get 0 4 2 3", "get 0 5 4", "get 0 6 9", "get 9 9 9", "get x", "get 0", "get 0 7 1 1 1",
        "set 0 7 1 Daylight", "set 0 7 1 night", "set 0 7 1 purple", "set 0 7 1", "set 7 1 Dark", "set 0 9 4 EU",
        "set 0 9 3 1 Off", "set 0 8 8 5 off", "set 0 8 8 5 on", "set 0 4 2 3 1234", "set 0 4 2 3 99999999",
        "set 0 4 2 3 none", "set 0 5 5 3 Hello there", "set 0 5 5 3 none", "set 0 5 4 144.390", "set 0 5 4 abc",
        "set 0 4 2 2 439.9875", "set 0 5 9 2 wide1-1,wide2-1", "set 0 5 9 2 bad,,bad,,", "set 0 5 9 2 none",
        "set 0 6 9 FN25", "set 0 6 9 45.5 -75.6", "set 0 6 9 north", "set 0 6 9 none", "set 0 7 7 1", "set 0 7 7 x",
        "set 0 7 2 10", "set 0 1 1 Off", "set", "set x",
        "ch", "ch 12", "ch 999", "ch 4001", "ch x", "ch 12 power high", "ch 12 power ultra", "ch 12 tone 100.0",
        "ch 12 tone 300.0", "ch 12 tone D023I", "ch 12 tone D028N", "ch 12 tone=D023N", "ch 12 rx 700", "ch 12 rx 30",
        "ch 12 mode usb", "ch 12 mode am", "ch 12 zone 9", "ch 12 zone 1", "ch 12 \"Name", "ch 12 name \"New one\"",
        "ch 12 name \"Club \\\"north\\\"\"", "ch 12 offset +0.6", "ch 12 offset x", "ch 12 offset off", "ch 12 tx 147.0",
        "ch 12 step 12.5", "ch 12 step 7", "ch 12 squelch 6", "ch 12 squelch 99", "ch 12 squelch global",
        "ch 12 busy on", "ch 12 reverse on", "ch 12 skip on", "ch 12 aprs on", "ch 12 rxonly on", "ch 12 hidden on",
        "ch 12 priority on", "ch 12 bw narrow", "ch 12 bw x", "ch 12 rxtone 88.5", "ch 12 txtone off",
        "ch 12 rxtone dcs-n 023", "ch 12 frob 1", "ch 12 busy maybe", "ch 12", "ch 12 end beep", "ch 12 tail on",
        "ch 300 \"Test\" 146.6 offset=-0.6 tone=100.0 power=mid zone=1 skip squelch=6", "ch 300",
        "ch 301 \"Two20\" 223.5 power=high", "ch 301 power mid", "ch 302 \"Bad\" 146.6 frob=1", "ch 303 \"Bad\"",
        "ch 300 delete", "ch 300 delete", "ch 12 split 146.0", "ch 12 rx 146.52 tx 146.0",
        "zone", "zone 1", "zone 9 \"Far\"", "zone 7 \"Next\"", "zone 7", "zone 1 scan=on", "zone 1 frob=1", "zone x",
        "zone 1 resume=5 dwell=3", "zone 1 pri1=12", "zone 1 distance=10",
        "preset", "preset 1", "preset 1 \"2 m\" 144 148", "preset 1 step=5", "preset 99", "preset 1 \"x\" 148 144",
        "home", "home 12", "home none", "home 999", "home x",
        "quick", "quick 3", "quick 3 \"On my way\"", "quick 3", "quick 3 factory", "quick 99", "quick 3 \"",
        "slot", "slot A", "slot B", "slot C", "slot D", "slot A 146.52 nfm high tone 100.0", "slot A vfo",
        "slot B active", "slot A offset -0.6", "slot A offset off", "slot A split 446.0", "slot A 9999", "slot A frob",
        "slot A rxtone ctcss 88.5", "slot A txtone dcs-i 023", "slot A am", "slot A low", "slot",
        "ui", "ui keys", "ui status", "ui frob",
        "key", "key MENU", "key EXIT long", "key FROB", "key MENU sideways", "key EXIT", "key EXIT up",
        "knob", "knob +3", "knob -2", "knob 30", "knob x", "knob +0",
        "xb", "xb start", "xb stop", "xb stop", "xb frob",
        "dump", "load", "gps", "gps query", "gps nmea off", "gps frob",
        "sat", "sat add", "sat del 1", "sat del x", "sat frob",
        "clear", "clear frob", "clear channels 3", "clear channels",
        "mem 20000000 4", "mem32 40010808 1", "gpio", "bk 0 30", "bk settle", "bk save", "si 10 00", "bt AT",
        "rx both", "mic", "af", "txtest sweep", "lcd", "fsk", "meter", "synthfsk", "pin",
    ];
    const LOADING: &[&str] = &["set 0 7 1 Dark", "ch 12 power low", "zone 1", "preset 1", "home 12", "quick 3", "get", "slot", "dump", "sat", "frob"];
    let mut r = Rig::new();
    let mut out = String::new();
    let run = |r: &mut Rig, l: &str, loading: bool, out: &mut String| {
        use core::fmt::Write;
        let _ = writeln!(out, "{} {l}", if loading { "<" } else { ">" });
        if !loading {
            let _ = writeln!(out, "  writes {}", cmd::writes(l.as_bytes()));
        }
        let live = LiveAll::default();
        let res = cmd::run(&mut r.app, &r.store, &r.dev, l.as_bytes(), loading);
        while let Some(w) = r.app.pop_write() {
            r.store.apply(w);
        }
        while r.app.pop_cmd().is_some() {}
        match res {
            Err(m) => {
                let _ = writeln!(out, "  ! {}", String::from_utf8_lossy(m.as_bytes()));
            }
            Ok(cmd::Done::Nothing) => out.push_str("  nothing\n"),
            Ok(cmd::Done::Said(m)) => {
                let _ = writeln!(out, "  = {}", String::from_utf8_lossy(m.as_bytes()));
            }
            Ok(cmd::Done::List(k)) => {
                let _ = writeln!(out, "  list {k:?}");
                let mut i = 0;
                let mut b = cmd::Line::new();
                while cmd::list_line(&r.app, &r.store, &r.dev, &live, k, i, &mut b) {
                    let _ = writeln!(out, "  | {}", String::from_utf8_lossy(b.as_bytes()));
                    b = cmd::Line::new();
                    i += 1;
                }
            }
            Ok(cmd::Done::Dump) => {
                out.push_str("  dump\n");
                for d in dump(&r.app, &r.store, &r.dev) {
                    let _ = writeln!(out, "  | {d}");
                }
            }
            Ok(cmd::Done::Load) => out.push_str("  load\n"),
            Ok(cmd::Done::SatPaste) => out.push_str("  sat paste\n"),
            Ok(cmd::Done::Debug(_)) => out.push_str("  debug job\n"),
        }
    };
    for l in LINES {
        r.now += 100;
        r.app.now = r.now;
        run(&mut r, l, false, &mut out);
    }
    for l in LOADING {
        run(&mut r, l, true, &mut out);
    }
    let n = r.store.count();
    run(&mut r, &std::format!("clear channels {n}"), false, &mut out);
    run(&mut r, "dump", false, &mut out);
    let name = if cfg!(feature = "bench") { "cmds-bench.txt" } else { "cmds.txt" };
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/term/golden").join(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &out).unwrap();
    }
    let want = std::fs::read_to_string(&path).expect("the golden file (UPDATE_GOLDEN=1 writes it)");
    if want != out {
        let at = want.lines().zip(out.lines()).position(|(a, b)| a != b).unwrap_or(0);
        panic!("line {}: want {:?}, got {:?}", at + 1, want.lines().nth(at), out.lines().nth(at));
    }
}
