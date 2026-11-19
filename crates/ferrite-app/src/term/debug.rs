//! The terminal's debug commands (manual chapter 37, TERM-21 to TERM-24, TERM-27, TERM-29, TERM-31 to TERM-33): MCU memory, the GPIO
//! ports, BK4829 registers, SI4732 commands, the Bluetooth module's queries, the both-receive-selects bench test, the BK4829 settle waits (TERM-31) the mic tap capture (TERM-32) and the transmit test signal (TERM-33), for bring-up without a custom build each time. A command becomes a [`Job`]:
//! the firmware takes its requests one at a time ([`crate::term::Term::debug_req`]), answers them
//! ([`crate::term::Term::debug_answer`]), and the console prints the answers as lines. Memory is read by the
//! firmware where the terminal runs; registers by the Radio task, which owns the chips and refuses writes that
//! could key the transmitter (ferrite_core::msg::bk_write_refused).

use super::cmd::Line;
use ferrite_core::msg::{DebugAns, DebugReq};
#[cfg(feature = "bench")]
use ferrite_core::msg::DEBUG_MEM;

#[cfg(feature = "bench")]
/// GPIO port A's input data register; ports B to E follow every 0x400, the output register 4 bytes after.
pub const GPIOA_IDR: u32 = 0x4001_0808;
#[cfg(feature = "bench")]
/// Ports A to E.
pub const GPIO_PORTS: u16 = 5;
#[cfg(feature = "bench")]
/// BK4829 registers 0x00 to 0x7F.
pub const BK_REGS: u16 = 0x80;
#[cfg(feature = "bench")]
/// The longest `mem` (bytes) and `mem32` (words) read.
pub const MEM_MAX: u16 = 1024;
/// A request not answered in this long ends the job (a chip that never answers, a build without the path).
pub const ANSWER_MS: u32 = 2000;

/// What a debug command asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    #[cfg(feature = "bench")]
    /// `mem ADDR [LEN]` bytes, `mem32 ADDR [WORDS]` 32-bit words (`len` in bytes).
    Mem { addr: u32, len: u16, words: bool },
    #[cfg(feature = "bench")]
    /// `gpio`: each port's input and output registers.
    Gpio,
    #[cfg(feature = "bench")]
    /// `bk CHIP REG`.
    BkRead { chip: u8, reg: u8 },
    #[cfg(feature = "bench")]
    /// `bk CHIP REG VALUE`.
    BkWrite { chip: u8, reg: u8, val: u16 },
    #[cfg(feature = "bench")]
    /// `bk CHIP dump`: 0x00 to 0x7F.
    BkDump { chip: u8 },
    #[cfg(feature = "bench")]
    /// `si BYTES...`, `si prop P [V]`: a command and how many bytes come back.
    Si { cmd: [u8; 8], n: u8, reply: u8 },
    #[cfg(feature = "bench")]
    /// `bt AT+VERS?`: a read-only query to the Bluetooth module (TERM-27).
    Bt { text: [u8; 16], n: u8 },
    #[cfg(feature = "bench")]
    /// `bt stat`, `bt baud N` (0: only ask), `bt pace B G`: USART1's counters, rate and pacing (TERM-27, LINK-36).
    BtStat(u32, Option<ferrite_core::link::BtPace>),
    #[cfg(feature = "bench")]
    /// `bt linkz`: the link codecs timed (decision 285, step 3).
    Linkz,
    #[cfg(feature = "bench")]
    /// `rx both`, `rx one`, `rx`: the bench test's hold on both band receive selects (TERM-29), or ask.
    RxBoth(Option<bool>),
    #[cfg(feature = "bench")]
    /// `crash fault`, `crash hang`: SAFE-8.1's restart, on purpose.
    Crash(bool),
    /// `gps`: the GPS module's last GGA sentence and its two heights (TERM-30).
    Gga,
    #[cfg(feature = "bench")]
    /// `gps query N`, `gps nmea off|on`, `gps standby N [quiet]`: a CASIC sentence to the module and its TXT
    /// answer (TERM-36).
    Gps { id: u8, arg: u16, quiet: bool },
    #[cfg(feature = "bench")]
    /// `bk settle [WAKE [SLEEP]]`: the BK4829 settle waits (TERM-31), set or asked.
    Settle(Option<ferrite_core::settle::Settle>),
    #[cfg(feature = "bench")]
    /// `mic`: a burst of PA0 (the mic level tap) readings, summed up (TERM-32); `af` (`true`): PA2, the receive
    /// audio (TERM-32.1).
    Mic(bool),
    #[cfg(feature = "bench")]
    /// `txtest [MODE [FILTERS] [LEVEL]]`: the transmit test signal for the next PTT key-up (TERM-33).
    TxTest(ferrite_core::txtest::Ask),
    #[cfg(feature = "bench")]
    /// `bk save ...`: Battery Save's cycle (TERM-41).
    Save(Option<Option<ferrite_core::rxsleep::Cycle>>),
    #[cfg(feature = "bench")]
    /// `lcd ...`: the display's bench state (TERM-40).
    Lcd(ferrite_core::msg::LcdSet),
    #[cfg(feature = "bench")]
    /// `bk turns [reset]`: each slot's turns on a chip (WATCH-17.1).
    Turns { reset: bool },
    #[cfg(feature = "bench")]
    /// `bk spi [HALF [READS]]`, `bk spi use RD WR`: the BK4829 bus speed test and speeds (TERM-53).
    Spi { half: Option<u8>, reads: u16, set: Option<[u8; 2]> },
    #[cfg(feature = "bench")]
    /// `bk rxaf [HEX|off]`: the receive AF filter override (TERM-42).
    RxAf(Option<Option<u16>>),
    #[cfg(feature = "bench")]
    /// `bk rxif [HEX|off]`: the receive IF override (TERM-42.2).
    RxIf(Option<Option<u16>>),
    #[cfg(feature = "bench")]
    /// `bk hold [REG VALUE|off]`: registers held after every FM tune (TERM-42.3).
    Hold(Option<Option<(u8, u16)>>),
    #[cfg(feature = "bench")]
    /// `bk swap [A|B|C|off]`: slots received on the other band's chip (TERM-29.4).
    Swap(Option<u8>),
    #[cfg(feature = "bench")]
    /// `synthfsk ...`: 4FSK from the keyed chip's tone generator (TERM-45).
    SynthFsk(ferrite_core::synthfsk::Cfg),
    #[cfg(feature = "bench")]
    /// `pin PIN 0|1|u|d`: an audio pin (TERM-46).
    Pin { which: u8, mode: ferrite_core::msg::PinMode },
    #[cfg(feature = "bench")]
    /// `meter [on|off]`: the receive level meter (TERM-44).
    Meter(Option<bool>),
    #[cfg(feature = "bench")]
    /// `fsk ...`: the BK4829's FSK modem test (TERM-43).
    Fsk(ferrite_core::fskbench::Ask),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum St {
    /// The next request is ready for the firmware.
    Ask,
    /// Asked at this time (ms); no answer yet.
    Wait(u32),
    /// An answer to print.
    Have,
    Over,
}

/// What [`Job::line`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// A line to print is in the buffer.
    Line,
    /// Waiting for the firmware.
    Wait,
    Over,
}

/// A debug command in progress.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Job {
    kind: Kind,
    /// Bytes, ports or registers done.
    done: u16,
    st: St,
    ans: Option<DebugAns>,
    /// Lines printed from the current answer.
    line: u8,
    /// `bk dump`: the current row of eight registers.
    #[cfg(feature = "bench")]
    acc: [u16; 8],
}

impl Job {
    pub fn new(kind: Kind) -> Self {
        Job { kind, done: 0, st: St::Ask, ans: None, line: 0, #[cfg(feature = "bench")]
            acc: [0; 8] }
    }

    /// The next request for the firmware, once; `now` starts its answer time.
    pub fn request(&mut self, now: u32) -> Option<DebugReq> {
        if self.st != St::Ask {
            return None;
        }
        self.st = St::Wait(now);
        Some(match self.kind {
            #[cfg(feature = "bench")]
            Kind::Mem { addr, len, words } => {
                let n = (len - self.done).min(DEBUG_MEM as u16) as u8;
                DebugReq::Mem { addr: addr + self.done as u32, n, words }
            }
            #[cfg(feature = "bench")]
            Kind::Gpio => DebugReq::Mem { addr: GPIOA_IDR + self.done as u32 * 0x400, n: 8, words: true },
            #[cfg(feature = "bench")]
            Kind::BkRead { chip, reg } => DebugReq::BkRead { chip, reg },
            #[cfg(feature = "bench")]
            Kind::BkWrite { chip, reg, val } => DebugReq::BkWrite { chip, reg, val },
            #[cfg(feature = "bench")]
            Kind::BkDump { chip } => DebugReq::BkRead { chip, reg: self.done as u8 },
            #[cfg(feature = "bench")]
            Kind::Si { cmd, n, reply } => DebugReq::Si { cmd, n, reply },
            #[cfg(feature = "bench")]
            Kind::Bt { text, n } => DebugReq::Bt { text, n },
            #[cfg(feature = "bench")]
            Kind::BtStat(baud, pace) => DebugReq::BtStat { baud, pace },
            #[cfg(feature = "bench")]
            Kind::Linkz => DebugReq::Linkz,
            #[cfg(feature = "bench")]
            Kind::RxBoth(on) => DebugReq::RxBoth(on),
            #[cfg(feature = "bench")]
            Kind::Crash(f) => DebugReq::Crash(f),
            Kind::Gga => DebugReq::Gga,
            #[cfg(feature = "bench")]
            Kind::Gps { id, arg, quiet } => DebugReq::Gps { id, arg, quiet },
            #[cfg(feature = "bench")]
            Kind::Settle(s) => DebugReq::Settle(s),
            #[cfg(feature = "bench")]
            Kind::Mic(pa2) => DebugReq::Mic(pa2),
            #[cfg(feature = "bench")]
            Kind::TxTest(a) => DebugReq::TxTest(a),
            #[cfg(feature = "bench")]
            Kind::Lcd(l) => DebugReq::Lcd(l),
            #[cfg(feature = "bench")]
            Kind::Save(c) => DebugReq::Save(c),
            #[cfg(feature = "bench")]
            Kind::Turns { reset } => DebugReq::Turns { reset },
            #[cfg(feature = "bench")]
            Kind::Spi { half, reads, set } => DebugReq::Spi { half, reads, set },
            #[cfg(feature = "bench")]
            Kind::RxAf(set) => DebugReq::RxAf(set),
            #[cfg(feature = "bench")]
            Kind::RxIf(set) => DebugReq::RxIf(set),
            #[cfg(feature = "bench")]
            Kind::Hold(set) => DebugReq::Hold(set),
            #[cfg(feature = "bench")]
            Kind::Swap(set) => DebugReq::Swap(set),
            #[cfg(feature = "bench")]
            Kind::SynthFsk(c) => DebugReq::SynthFsk(c),
            #[cfg(feature = "bench")]
            Kind::Pin { which, mode } => DebugReq::Pin { which, mode },
            #[cfg(feature = "bench")]
            Kind::Meter(set) => DebugReq::Meter(set),
            #[cfg(feature = "bench")]
            Kind::Fsk(a) => DebugReq::Fsk(a),
        })
    }

    /// How long to wait for the answer: TERM-45's run blocks the Radio task for its length.
    fn answer_ms(&self) -> u32 {
        match self.kind {
            #[cfg(feature = "bench")]
            Kind::SynthFsk(c) => ANSWER_MS + c.ms as u32,
            _ => ANSWER_MS,
        }
    }

    /// The firmware's answer to the last request.
    pub fn answer(&mut self, a: DebugAns) {
        if let St::Wait(_) = self.st {
            (self.ans, self.line, self.st) = (Some(a), 0, St::Have);
        }
    }

    /// The next line into `out`, if there is one now.
    pub fn line(&mut self, now: u32, out: &mut Line) -> Step {
        out.clear();
        match self.st {
            St::Over => return Step::Over,
            St::Ask => return Step::Wait,
            St::Wait(at) if now.wrapping_sub(at) >= self.answer_ms() => {
                self.st = St::Over;
                out.push(b"  no answer (the firmware didn't reply in 2 s)");
                return Step::Line;
            }
            St::Wait(_) => return Step::Wait,
            St::Have => {}
        }
        let Some(a) = self.ans else { return Step::Over };
        match (self.kind, a) {
            (_, DebugAns::Refused(why)) => {
                out.push(b"  refused: ");
                out.push(why.as_bytes());
                self.st = St::Over;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::Mem { len, words, .. }, DebugAns::Mem { addr, data, n }) => {
                let k = self.line as usize * 16;
                if k < n as usize {
                    mem_line(addr + k as u32, &data[k..(k + 16).min(n as usize)], words, out);
                    self.line += 1;
                    return Step::Line;
                }
                self.done += n as u16;
                self.st = if self.done < len && n > 0 { St::Ask } else { St::Over };
                self.line(now, out)
            }
            #[cfg(feature = "bench")]
            (Kind::Gpio, DebugAns::Mem { data, .. }) => {
                let word = |i: usize| u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
                let (idr, odr) = (word(0) as u16, word(4) as u16);
                put!(out, "  P{}  in ", ["A", "B", "C", "D", "E"][self.done as usize % 5]);
                bits16(idr, out);
                out.push(b"   out ");
                bits16(odr, out);
                self.done += 1;
                self.st = if self.done < GPIO_PORTS { St::Ask } else { St::Over };
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::BkRead { .. }, DebugAns::Reg { chip, reg, val }) => {
                put!(out, "  bk{} REG_", chip);
                hex(reg as u32, 2, out);
                out.push(b" = 0x");
                hex(val as u32, 4, out);
                out.push(b"   ");
                bits16(val, out);
                self.st = St::Over;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::BkWrite { val, .. }, DebugAns::Reg { chip, reg, val: back }) => {
                put!(out, "  bk{} REG_", chip);
                hex(reg as u32, 2, out);
                out.push(b" <- 0x");
                hex(val as u32, 4, out);
                out.push(b", reads back 0x");
                hex(back as u32, 4, out);
                if back != val {
                    out.push(b" (differs: read-only or self-clearing bits)");
                }
                self.st = St::Over;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::BkDump { chip }, DebugAns::Reg { val, .. }) => {
                self.acc[self.done as usize % 8] = val;
                self.done += 1;
                self.st = if self.done < BK_REGS { St::Ask } else { St::Over };
                if !self.done.is_multiple_of(8) {
                    return Step::Wait;
                }
                put!(out, "  bk{} ", chip);
                hex(self.done as u32 - 8, 2, out);
                out.push(b":");
                for v in self.acc {
                    out.push(b" ");
                    hex(v as u32, 4, out);
                }
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::Bt { text, n: tn }, DebugAns::Bt { data, n }) => {
                out.push(b"  bt ");
                out.push(&text[..tn as usize]);
                if n == 0 {
                    out.push(b" -> no answer in 500 ms");
                } else {
                    out.push(b" -> ");
                    for &c in &data[..n as usize] {
                        out.push(&[if (b' '..=b'~').contains(&c) { c } else { b'.' }]);
                    }
                }
                self.st = St::Over;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::Linkz, DebugAns::Linkz(v)) => {
                let us = |c: u32, mhz: u32| c / mhz.max(1);
                match self.line {
                    0 => put!(out, "  linkz: {} B plain, {} MHz now; cycles, then us at 24 and 120 MHz", v[1], v[0]),
                    k @ 1..=2 => {
                        let i = 2 + (k as usize - 1) * 3;
                        let name = ["down", "up  "][k as usize - 1];
                        put!(out, "  {} {} B, encode {} cyc ({} / {} us), decode {} cyc ({} / {} us)", name, v[i], v[i + 1], us(v[i + 1], 24), us(v[i + 1], 120), v[i + 2], us(v[i + 2], 24), us(v[i + 2], 120));
                        if k == 2 {
                            self.st = St::Over;
                        }
                    }
                    _ => self.st = St::Over,
                }
                self.line += 1;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::BtStat(..), DebugAns::BtStat(b)) => {
                match self.line {
                    0 => put!(out, "  bt: USART1 {} baud, module {}", b.baud, if b.on { "on (PB9 high)" } else { "off" }),
                    1 if b.pace.burst == 0 => put!(out, "  pace: off, back to back (~{} B/s)", b.pace.bytes_per_s(b.baud)),
                    1 => put!(out, "  pace: {} B bursts, {} ms gaps (~{} B/s)", b.pace.burst, b.pace.gap_ms, b.pace.bytes_per_s(b.baud)),
                    _ => {
                        put!(out, "  in {} out {} bytes, framing {}, overrun {}, dropped {}", b.rx, b.tx, b.framing, b.overrun, b.dropped);
                        self.st = St::Over;
                    }
                }
                self.line += 1;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::RxBoth(_), DebugAns::RxBoth(on)) => {
                out.push(if on {
                    b"  rx both: VHF RX (PB0) and UHF RX (PE12) held on, until rx one or a restart"
                } else {
                    b"  rx one: only the receiving band's select on (PB0 or PE12), as normal"
                });
                self.st = St::Over;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::Save(_), DebugAns::Save { cycle, level, asleep }) => {
                let now = if asleep { "asleep" } else { "awake" };
                match (cycle, level) {
                    (Some(c), _) => put!(out, "  bk save: {}/{} ms, hold {} s, then 300/300 for {} s (override), {} now", c.on_ms as u32, c.off_ms as u32, c.hold_ms / 1000, c.recent_ms / 1000, now),
                    (None, Some((on, off))) => put!(out, "  bk save: the level's {}/{} ms, hold 10 s, then 300/300 for 30 min, {} now", on, off, now),
                    (None, None) => put!(out, "  bk save: Battery Save off, {} now", now),
                }
                self.st = St::Over;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::Lcd(_), DebugAns::Lcd(l)) => {
                let pct = |p: Option<u8>| p.map_or(Num(None), |p| Num(Some(p)));
                put!(out, "  lcd: idle {}, partial {}, sleep {}, fps {}, bl {}, keys {}", onoff(l.idle), onoff(l.partial), onoff(l.sleep),
                     if l.fps == 0 { Num(None) } else { Num(Some(l.fps)) }, pct(l.bl), pct(l.keys));
                if let Some((us, mhz)) = l.bus {
                    put!(out, "; bus: full-screen fill {} us, {} ns a byte at {} MHz", us, us * 1000 / 153_600, mhz as u32);
                }
                self.st = St::Over;
                Step::Line
            }
            #[cfg(feature = "bench")]
            #[cfg(feature = "bench")]
            (Kind::Spi { .. }, DebugAns::Spi { half, reads, rd, wr, us, now }) => {
                if reads == 0 {
                    put!(out, "  bk spi: reads {}.{} us, writes {}.{} us", now[0] / 10, now[0] % 10, now[1] / 10, now[1] % 10);
                    self.st = St::Over;
                    return Step::Line;
                }
                put!(out, "  bk spi: half {}.{} us, {} rounds a chip:", half / 10, half % 10, reads);
                for n in 0..2 {
                    if rd[n] == u16::MAX {
                        put!(out, " chip {} absent", n);
                    } else {
                        put!(out, " chip {} reads {} wrong, writes {} wrong", n, rd[n], wr[n]);
                    }
                }
                put!(out, "; {} us an operation; now reads {}.{} us, writes {}.{} us", us, now[0] / 10, now[0] % 10, now[1] / 10, now[1] % 10);
                self.st = St::Over;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::Turns { reset }, DebugAns::Turns(t)) => {
                out.push(b"  bk turns:");
                for (s, name) in ["A", "B", "C"].into_iter().enumerate() {
                    put!(out, " {} {} {} ms{}", name, t.n[s], t.ms[s], if s < 2 { "," } else { "" });
                }
                // WATCH-17.4: the steps inside turns (a scan's channels), and the retunes' time
                out.push(b"; steps:");
                for (s, name) in ["A", "B", "C"].into_iter().enumerate() {
                    put!(out, " {} {} {} ms{}", name, t.steps[s], t.step_ms[s], if s < 2 { "," } else { "" });
                }
                put!(out, "; tunes {}, {} us each, {} us max", t.tunes, t.tune_us.checked_div(t.tunes).unwrap_or(0), t.tune_max_us);
                if reset {
                    out.push(b" (reset)");
                }
                self.st = St::Over;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::RxAf(_), DebugAns::RxAf(bits)) => {
                match bits {
                    None => out.push(b"  bk rxaf: off (an FM tune writes REG_2B 0x0000, the APRS slot's 0x0500: APRS-55)"),
                    Some(b) => {
                        out.push(b"  bk rxaf: REG_2B bits 10:8 = 0x");
                        hex(b as u32, 4, out);
                        out.push(b" after every FM tune (");
                        let mut any = false;
                        for (bit, name) in [(0x400, &b"HPF 300 Hz"[..]), (0x200, b"LPF 3 kHz"), (0x100, b"de-emphasis")] {
                            if b & bit != 0 {
                                out.push(if any { b", " } else { b"" });
                                out.push(name);
                                any = true;
                            }
                        }
                        out.push(if any { &b" off); until bk rxaf off or power-off"[..] } else { b"all on); until bk rxaf off or power-off" });
                    }
                }
                self.st = St::Over;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::RxIf(_), DebugAns::RxIf(v)) => {
                match v {
                    None => out.push(b"  bk rxif: off (an FM tune writes REG_43 for the slot's bandwidth)"),
                    Some(v) => {
                        out.push(b"  bk rxif: REG_43 = 0x");
                        hex(v as u32, 4, out);
                        put!(out, " after every FM tune (IF {} / weak {}, x2 {}); until bk rxif off or power-off", v >> 12 & 7, v >> 9 & 7, if v & 0x20 != 0 { "on" } else { "off" });
                    }
                }
                self.st = St::Over;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::SynthFsk(c), DebugAns::SynthFsk(r)) => {
                put!(out, "  synthfsk {}: {} steps at {} a symbol; gain write {}.{} us avg, {}.{} max; {} late (worst {} us); carrier word {}",
                    if c.cal { "cal" } else { "4fsk" }, r.steps, c.sub, r.write_avg_x10 / 10, r.write_avg_x10 % 10, r.write_max_x10 / 10, r.write_max_x10 % 10, r.late, r.late_max_us, r.freq_word);
                self.st = St::Over;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::Pin { .. }, DebugAns::Pin { which, mode, high }) => {
                use ferrite_core::msg::{PinMode, PINS};
                let m = match mode {
                    PinMode::Low => "driven low",
                    PinMode::High => "driven high",
                    PinMode::PullUp => "input, pull-up",
                    PinMode::PullDown => "input, pull-down",
                };
                put!(out, "  pin {} {}: reads {} (until the radio's next path step sets it)", PINS[which as usize % PINS.len()], m, high as u8);
                self.st = St::Over;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::Swap(_), DebugAns::Swap(m)) => {
                out.push(b"  bk swap:");
                for s in 0..3u8 {
                    if m & 1 << s != 0 {
                        out.push(&[b' ', b'A' + s]);
                    }
                }
                out.push(if m != 0 { &b" on the other band's chip; bk swap off ends it"[..] } else { b" none" });
                self.st = St::Over;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::Hold(_), DebugAns::Hold(h)) => {
                out.push(b"  bk hold:");
                let mut any = false;
                for (r, v) in h.iter().filter(|h| h.0 != 0) {
                    out.push(b" REG_");
                    hex(*r as u32, 2, out);
                    out.push(b" = 0x");
                    hex(*v as u32, 4, out);
                    any = true;
                }
                out.push(if any { &b" after every FM tune; until bk hold off or power-off"[..] } else { b" none" });
                self.st = St::Over;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::Meter(_), DebugAns::Meter(r)) => {
                use ferrite_core::rxmeter::{FREQS, N, NONE};
                // TERM-44: a head line, then five bins a line: Hz and dBFS
                const PER: usize = 5;
                if self.line == 0 {
                    put!(out, "  meter {}: {} windows of 100 ms; loudest per bin, dB against full scale", if r.on { "on" } else { "off" }, r.windows);
                } else {
                    let at = (self.line as usize - 1) * PER;
                    // right-aligned: Hz in 6, the level in 6 ("-123.4", " -12.3", "  --.-")
                    fn pad(out: &mut Line, used: usize, width: usize) {
                        for _ in used..width {
                            out.push(b" ");
                        }
                    }
                    let digits = |v: u32| if v >= 10_000 { 5 } else if v >= 1000 { 4 } else if v >= 100 { 3 } else if v >= 10 { 2 } else { 1 };
                    out.push(b" ");
                    for k in at..(at + PER).min(N) {
                        let f = FREQS[k] as u32;
                        pad(out, digits(f), 6);
                        put!(out, "{}", f);
                        match r.db10[k] {
                            NONE => out.push(b"   --.-"),
                            d => {
                                let a = d.unsigned_abs() as u32;
                                pad(out, digits(a / 10) + 2 + (d < 0) as usize, 7);
                                put!(out, "{}{}.{}", if d < 0 { "-" } else { "" }, a / 10, a % 10);
                            }
                        }
                    }
                }
                if self.line as usize >= N.div_ceil(PER) {
                    self.st = St::Over;
                }
                self.line += 1;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::Fsk(_), DebugAns::Fsk(r)) => {
                fsk_line(&r, self.line, out);
                let last = match r.cfg {
                    None => 0,
                    Some(_) if r.tx.is_some() => 0,
                    Some(_) if r.stats.last_n == 0 => 1,
                    Some(_) => 3,
                };
                if self.line >= last {
                    self.st = St::Over;
                }
                self.line += 1;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::Settle(_), DebugAns::Settle(s)) => {
                put!(out, "  bk settle: wake {} ms, sleep {} ms", s.wake_ms, s.sleep_ms);
                if s == ferrite_core::settle::Settle::STOCK {
                    out.push(b" (stock's: no wait)");
                }
                self.st = St::Over;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::Mic(pa2), DebugAns::Mic(c)) => {
                match self.line {
                    0 => put!(out, "  {}: {} readings in {} us: min {}, max {}, mean {} (12-bit)", if pa2 { "af PA2" } else { "mic PA0" }, ferrite_core::vox::MIC_SAMPLES, c.us, c.min, c.max, c.mean),
                    1 => put!(out, "  {} crossings of the mean: {} Hz if this is raw audio (a level or envelope barely crosses); {} at a rail", c.crossings, c.hz(), c.rails),
                    _ => {
                        out.push(b"  first 16 (8-bit):");
                        for v in c.first {
                            put!(out, " {}", v);
                        }
                        self.st = St::Over;
                    }
                }
                self.line += 1;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::TxTest(_), DebugAns::TxSuite { section, left_ms }) => {
                match section {
                    0 => put!(out, "  txtest suite running: the lead-in, {} s to the end tone", left_ms.div_ceil(1000)),
                    1..=7 => put!(out, "  txtest suite running: section {} of 7 ({}), {} s to the end tone", section, ferrite_core::txsuite::SECTION_NAMES[section as usize - 1], left_ms.div_ceil(1000)),
                    _ => out.push(b"  txtest suite done: holding 1 kHz until you release PTT"),
                }
                self.st = St::Over;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::TxTest(_), DebugAns::TxTest(None)) => {
                out.push(b"  txtest: off (nothing armed; PTT transmits the mic as normal)");
                self.st = St::Over;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::TxTest(_), DebugAns::TxTest(Some(a))) => {
                use ferrite_core::txtest::{Eq, Signal};
                if self.line == 0 {
                    let rc = if a.shaped && a.signal == Signal::Fsk { " rc" } else { "" };
                    if a.signal.takes_baud() {
                        put!(out, "  txtest: {} {}{} at {} % armed for the next PTT key-up, from the key-up to the release; once", a.signal.name(), a.baud, rc, a.level);
                    } else if a.signal.is_sol950() {
                        put!(out, "  txtest: {} (dev {}) at {} % armed for the next PTT key-up; once", a.signal.name(), a.baud, a.level);
                    } else {
                        put!(out, "  txtest: {} at {} % armed for the next PTT key-up, from the key-up to the release; once", a.signal.name(), a.level);
                    }
                } else if self.line == 2 {
                    // TERM-33.15, TERM-33.16
                    if a.bypass {
                        out.push(b"  bypass: all AF TX filters off (REG_47 0 = 1), soft limiter off (REG_2F 7:5 = 000);");
                    }
                    if a.eq != Eq::Off {
                        out.push(if a.bypass { b" " } else { b"  " });
                        let g = a.eq.gain_1k_cdb();
                        put!(out, "{}: flat 100 Hz-{} kHz, {} taps, {}{}.{} dB at 1 kHz; REG_7D 0x3F, REG_40 0x", a.eq.name(), a.eq.top_hz() / 1000, a.eq.taps().len(), if g < 0 { "-" } else { "+" }, g.unsigned_abs() / 100, g.unsigned_abs() / 10 % 10);
                        hex(a.eq.deviation() as u32, 3, out);
                        out.push(b";");
                    }
                    out.push(b" put back at unkey");
                    self.st = St::Over;
                } else {
                    use ferrite_core::txtest::{bk_gain, Filters};
                    if a.signal == Signal::Suite {
                        put!(out, "  suite: 7 sections, {} s, each after a marker tone (400-700 Hz); then 1 kHz until release; registers put back", ferrite_core::txsuite::SUITE_MS.div_ceil(1000));
                        self.st = St::Over;
                        self.line += 1;
                        return Step::Line;
                    }
                    if a.signal == Signal::AfskBk {
                        put!(out, "  afsk-bk: tone 1 1200/2200 Hz (REG_71) at gain {} (REG_70), REG_30 0xC3FA (mic off), TMR5 at 1200 Bd; DAC1, PC12 untouched", bk_gain(a.level));
                        self.st = St::Over;
                        self.line += 1;
                        return Step::Line;
                    }
                    if a.signal.is_sol950() {
                        put!(out, "  {}: BK4829 PLL REG_38/39 (50 Hz/tone; cal +500 Hz steps), REG_30 VCO toggle, TMR5 1.25 ms; restored at unkey", a.signal.name());
                        self.st = St::Over;
                        self.line += 1;
                        return Step::Line;
                    }
                    out.push(match a.filters {
                        Filters::Flat => &b"  flat: TX HPF, LPF, pre-emphasis (REG_2B 2:0 = 111), MIC AGC (REG_19 15), ALC (REG_7D 6) off; REG_7D 0x34; put back at unkey"[..],
                        Filters::Data => b"  data: TX HPF off, pre-emphasis and LPF on (REG_2B 2:0 = 100), MIC AGC (REG_19 15), ALC (REG_7D 6) off; REG_7D 0x34; put back at unkey (as an APRS burst)",
                        Filters::Voice => b"  voice: the chip's TX filters, MIC AGC and ALC as for speech; REG_7D 0x34",
                    });
                    if !a.bypass && a.eq == Eq::Off {
                        self.st = St::Over;
                    }
                }
                self.line += 1;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::Gps { id, arg, quiet }, DebugAns::GpsSent { data, n }) => {
                use ferrite_core::gpspower::{casic, CASIC_MAX, CAS_NMEA, CAS_STANDBY};
                if self.line == 0 {
                    let mut b = [0u8; CASIC_MAX];
                    out.push(b"  sent ");
                    if quiet {
                        let k = casic(CAS_NMEA, 0, &mut b);
                        out.push(&b[..k - 2]);
                        out.push(b" CR LF, 100 ms later ");
                    }
                    let k = casic(id, arg, &mut b);
                    out.push(&b[..k - 2]);
                    out.push(b" CR LF");
                } else {
                    if n > 0 {
                        out.push(b"  answer: ");
                        out.push(&data[..(n as usize).min(data.len())]);
                    } else if quiet {
                        out.push(b"  the NMEA set (GPS-34) once NMEA comes back, or the standby + 3 s: cable log");
                    } else if id == CAS_NMEA && arg == 0 {
                        out.push(b"  NMEA off if the cable log's 5 s counts drop to 0 sentences (gps nmea on undoes)");
                    } else if id == CAS_NMEA {
                        out.push(b"  NMEA back if the cable log's 5 s counts climb again (Ferrite's set, GPS-34)");
                    } else if id == CAS_STANDBY {
                        out.push(b"  asleep if the cable log's 5 s counts drop to 0 sentences");
                    } else {
                        out.push(b"  no TXT answer from the module in 1.2 s");
                    }
                    self.st = St::Over;
                }
                self.line += 1;
                Step::Line
            }
            (Kind::Gga, DebugAns::Gga { n: 0, .. }) => {
                out.push(b"  no GGA since boot (GPS off, or no sentence from the module yet)");
                self.st = St::Over;
                Step::Line
            }
            (Kind::Gga, DebugAns::Gga { data, n, age_s }) => {
                let s = &data[..(n as usize).min(data.len())];
                if self.line == 0 {
                    put!(out, "  GGA, {} s ago: ", age_s);
                    out.push(s);
                } else {
                    // Field 9 as the module gives it (said to be above mean sea level) and field 11, the geoid
                    // separation: a module reporting the ellipsoid height leaves 11 empty or 0.
                    let (alt, sep) = gga_heights(s);
                    out.push(b"  altitude (field 9) ");
                    metres(alt, out);
                    out.push(b", geoid separation (field 11) ");
                    metres(sep, out);
                    self.st = St::Over;
                }
                self.line += 1;
                Step::Line
            }
            #[cfg(feature = "bench")]
            (Kind::Si { cmd, n: cn, .. }, DebugAns::Si { data, n }) => {
                out.push(b"  si");
                for b in &cmd[..cn as usize] {
                    out.push(b" ");
                    hex(*b as u32, 2, out);
                }
                out.push(b" ->");
                for b in &data[..n as usize] {
                    out.push(b" ");
                    hex(*b as u32, 2, out);
                }
                self.st = St::Over;
                Step::Line
            }
            #[cfg_attr(not(feature = "bench"), allow(unreachable_patterns))]
            _ => {
                out.push(b"  unexpected answer");
                self.st = St::Over;
                Step::Line
            }
        }
    }
}

/// TERM-43: line `k` of `fsk`'s answer: the run, then (receiving) the counts and the last frame's bytes in two rows.
#[cfg(feature = "bench")]
fn fsk_line(r: &ferrite_core::fskbench::Report, k: u8, out: &mut Line) {
    let Some(c) = r.cfg else {
        out.push(b"  fsk: off (fsk rx SPEED or fsk tx SPEED N starts it)");
        return;
    };
    let s = &r.stats;
    let crc = if c.crc { "on" } else { "off" };
    let odd = if c.speed.documented() { "" } else { " (not a documented speed)" };
    match (k, r.tx) {
        (0, Some(n)) => {
            out.push(b"  fsk tx ");
            speed_word(c.speed, out);
            put!(out, " (chip CRC {}){}: {} of {} frames sent", crc, odd, s.sent, n);
            if r.armed {
                out.push(b", armed for the next PTT key-up (Dry TX on only)");
            }
        }
        (0, None) => {
            out.push(b"  fsk rx ");
            speed_word(c.speed, out);
            put!(out, " (chip CRC {}){}: ", crc, odd);
            match r.chip {
                Some(n) => put!(out, "chip {} listening", n),
                None => out.push(b"no chip has the active slot now (asleep, or another slot's turn)"),
            }
        }
        (1, _) => put!(out, "  syncs {} ({} inverted), frames {}: chip CRC ok {} bad {}; clean {}, short {}, low byte first {}, PRBS bit errors {}",
                       s.syncs, s.sync_neg, s.frames, s.crc_ok, s.crc_bad, s.clean, s.short, s.lo_first, s.bit_errors),
        (k, _) => {
            let half = k as usize - 2;
            let n = s.last_n as usize;
            if half == 0 {
                put!(out, "  last, #{} ({} bytes):", s.last_counter, n);
            } else {
                out.push(b"                      ");
            }
            for b in &s.last[(half * 16).min(n)..((half + 1) * 16).min(n)] {
                out.push(b" ");
                hex(*b as u32, 2, out);
            }
        }
    }
}

/// TERM-43: a speed as typed (`2400`, `ffsk1800`).
#[cfg(feature = "bench")]
fn speed_word(sp: ferrite_core::fskbench::Speed, out: &mut Line) {
    use ferrite_core::fskbench::Mode;
    match sp.mode {
        Mode::Fsk => put!(out, "{}", sp.baud),
        Mode::Ffsk1800 => out.push(b"ffsk1800"),
        Mode::Ffsk2400 => out.push(b"ffsk2400"),
    }
}

/// A GGA sentence's heights (TERM-30), as received (`$` and `*hh` may be on it): field 9, the altitude the module
/// gives as above mean sea level, and field 11, the geoid separation (mean sea level above the WGS-84 ellipsoid),
/// in decimetres; None where a field is empty or bad. A module that puts the ellipsoid height in field 9 sends
/// field 11 empty or 0.
pub fn gga_heights(s: &[u8]) -> (Option<i32>, Option<i32>) {
    let body = s.split(|&c| c == b'*').next().unwrap_or(s);
    let mut f = body.split(|&c| c == b',');
    let alt = f.nth(9).and_then(dm);
    (alt, f.nth(1).and_then(dm))
}

/// "-12.34" as decimetres (-123), tenths kept and the rest dropped; None if empty or not a number.
fn dm(f: &[u8]) -> Option<i32> {
    let (neg, f) = match f {
        [b'-', r @ ..] => (true, r),
        _ => (false, f),
    };
    let (mut v, mut dot, mut tenth) = (0i32, false, false);
    for &c in f {
        match c {
            b'.' if !dot => dot = true,
            b'0'..=b'9' if dot && tenth => {}
            b'0'..=b'9' => {
                v = v.checked_mul(10)?.checked_add((c - b'0') as i32)?;
                tenth |= dot;
            }
            _ => return None,
        }
    }
    if f.is_empty() || f == b"." {
        return None;
    }
    if !tenth {
        v = v.checked_mul(10)?;
    }
    Some(if neg { -v } else { v })
}

/// Decimetres as metres with one decimal ("-12.3 m"), or "empty".
fn metres(dm: Option<i32>, out: &mut Line) {
    match dm {
        None => out.push(b"empty"),
        Some(v) => put!(out, "{}{}.{} m", if v < 0 { "-" } else { "" }, v.unsigned_abs() / 10, v.unsigned_abs() % 10),
    }
}

#[cfg(feature = "bench")]
/// `digits` hex digits of v, capitals.
pub fn hex(v: u32, digits: u32, out: &mut Line) {
    for i in (0..digits).rev() {
        out.push(&[b"0123456789ABCDEF"[(v >> (i * 4)) as usize & 15]]);
    }
}

#[cfg(feature = "bench")]
/// 16 bits in four groups, bit 15 first: "0001 0010 0011 0100".
fn bits16(v: u16, out: &mut Line) {
    for i in (0..16).rev() {
        out.push(if v >> i & 1 != 0 { b"1" } else { b"0" });
        if i % 4 == 0 && i != 0 {
            out.push(b" ");
        }
    }
}

#[cfg(feature = "bench")]
/// One dump line: the address, then 16 bytes and their characters, or 4 words.
fn mem_line(addr: u32, d: &[u8], words: bool, out: &mut Line) {
    out.push(b"  ");
    hex(addr, 8, out);
    out.push(b" ");
    if words {
        for w in d.chunks(4) {
            let mut b = [0u8; 4];
            b[..w.len()].copy_from_slice(w);
            out.push(b" ");
            hex(u32::from_le_bytes(b), 8, out);
        }
        return;
    }
    for b in d {
        out.push(b" ");
        hex(*b as u32, 2, out);
    }
    for _ in d.len()..16 {
        out.push(b"   ");
    }
    out.push(b"  |");
    for &b in d {
        out.push(&[if (0x20..0x7F).contains(&b) { b } else { b'.' }]);
    }
    out.push(b"|");
}

#[cfg(feature = "bench")]
fn onoff(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// TERM-40: a number, or "normal" for the setting's own.
#[cfg(feature = "bench")]
struct Num(Option<u8>);
#[cfg(feature = "bench")]
impl From<Num> for super::cmd::Arg<'_> {
    fn from(n: Num) -> Self {
        match n.0 {
            Some(n) => super::cmd::Arg::N(n as u32),
            None => super::cmd::Arg::S("normal"),
        }
    }
}
