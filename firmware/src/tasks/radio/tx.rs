//! Transmit (the `tx` build).

use super::*;

/// The `tx` build's transmit side (manual chapter 15).
#[cfg(feature = "tx")]
impl Radio {
    /// TX-42: every PTT input (PTT A PE3, PTT B PE2, the connector's PE6; low when pressed) has read released
    /// since boot.
    /// SAFE-14: one held from boot for over 1 s is reported stuck (`TxLive::ptt_stuck`, the UI's notice; a cable
    /// log line with its name when that changes) until let go.
    pub(super) fn ptt_seen_released(&mut self) -> bool {
        use crate::board::{KEY_PTT, KEY_SIDE1, KEY_SIDE2};
        use ferrite_radio::ptt;
        let was = self.tx.ptt.open();
        let pressed = if KEY_PTT.read() { 0 } else { ptt::PTT_A } | if KEY_SIDE1.read() { 0 } else { ptt::PTT_B } | if KEY_SIDE2.read() { 0 } else { ptt::PTT_EXT };
        let ok = self.tx.ptt.look(pressed);
        if ok && !was {
            log(b"ptt released since boot", 0);
        }
        let stuck = self.tx.ptt.stuck(now_ms() as u32);
        // The high nibble keeps what was stuck since boot, so the UI names it even when released before it looked.
        let was = self.live.tx.ptt_stuck;
        self.live.tx.ptt_stuck = ferrite_core::stuck::latch(was, stuck);
        if stuck != was & ferrite_core::stuck::NOW {
            // "ptt stuck: PTT A + side PTT 5" (the bits last), "ptt stuck: let go 0" (no formatter: this task stays
            // in the fast flash).
            let mut t = [0u8; 36];
            t[..11].copy_from_slice(b"ptt stuck: ");
            let k = if stuck == 0 {
                t[11..18].copy_from_slice(b"let go ");
                18
            } else {
                let k = (11 + ptt::names(stuck, &mut t[11..35])).min(35);
                t[k] = b' ';
                k + 1
            };
            log(&t[..k], stuck as u32);
        }
        ok
    }

    /// The battery for transmit (PWR-20): the PA1 reading against the calibration's gauge.
    pub(super) fn battery_ok(&mut self) -> bool {
        use crate::hal::adc;
        if !self.tx.adc {
            adc::init();
            self.tx.adc = true;
        }
        self.tx.gauge.tx_ok(adc::read(adc::CH_PA1))
    }

    /// `Act::ChipTx`: the chip already holds the TX frequency and tone; key it with the microphone and read back
    /// REG_30, the frequency, REG_3B and REG_36 before the PA (TX-7, decision 97), with stock's REG_36 for the level
    /// ({0x7F, 0x57, 0x57}[level], `analysis/calibration.md`).
    pub(super) fn key_chip(&mut self, n: u8) -> bool {
        if !self.has(n) {
            return false;
        }
        // The frequency both key-up paths wrote just before the plan ran (`transmit`, `xb_key`).
        let Some((_, freq)) = self.tuned[n as usize] else {
            return false;
        };
        let high = self.tx.data.unwrap_or(self.tx.power[self.live.tx.slot as usize % 4]) == 2;
        // CAL-12, CAL-13: the chip holds the trimmed word (`tx_chip_setup`).
        let freq = self.tx.cal.map_or(freq, |c| c.tx_word(freq));
        let want = radio_chip::TxExpect { freq, reg3b: self.tx.reg3b[n as usize], narrow: self.tx.narrow };
        match radio_chip::key_mic(&mut self.chip(n), high, &want) {
            Ok(()) => true,
            Err(r) => {
                log(b"tx read-back wrong, register ", r as u32);
                false
            }
        }
    }

    /// LINK-20, TX-47: unkey as a cut (a remote key-up whose keepalive or session was lost): no end tone, CW ID or
    /// carrier hold, as the timeout's.
    pub(super) async fn transmit_cut(&mut self) {
        let now = now_ms() as u32;
        let mut st = Steps::default();
        self.test_audio_off();
        let cfg = self.tx_cfg();
        self.tx.seq.cut(&cfg, &mut self.path, now, &mut st);
        self.run(&st).await;
        self.unkeyed();
    }

    /// Key up or unkey a slot (3 = HF mode's frequency); `data`: a data key-up at that power level (APRS).
    pub(super) async fn transmit(&mut self, slot: u8, on: bool, data: Option<u8>) {
        let now = now_ms() as u32;
        let mut st = Steps::default();
        if !on {
            self.test_audio_off(); // TERM-33: the test signal ends at the release, before the unkey's steps
            let cfg = self.tx_cfg();
            self.tx.seq.release(&cfg, &mut self.path, now, &mut st);
            self.run(&st).await;
            if self.tx.data.is_some() && self.aprs_slots & 1 << slot != 0 {
                self.watcher.after_tx(now_ms() as u32); // APRS-21.4: listen for its digipeat or ack
            }
            self.unkeyed();
            return;
        }
        if let Some(c) = crate::chan::TX_CAL.try_take() {
            (self.tx.cal, self.tx.gauge) = c;
        }
        let Some((f, region, extend, confirmed)) = self.tx.plan.take() else {
            return self.refuse(slot, Refusal::Locked);
        };
        if !self.ptt_seen_released() {
            return self.refuse(slot, Refusal::PttAtBoot);
        }
        let s = slot as usize % 4;
        let home = slot < SLOTS as u8;
        let rx = if home { self.slots[s].freq } else { f };
        self.tx.data = data;
        // TERM-33: a PTT key-up with a test armed goes out as a data key-up (REG_7D 0x34, the audio stage after the
        // PA ramp, no end tone or ID) at the slot's power, still a person's (the emergency prompt holds); the
        // arming is used up only once the key-up is made.
        #[cfg(feature = "bench")]
        let test = if data.is_none() { self.tx.test } else { None };
        // TERM-43: `fsk tx`'s frames armed, Dry TX still on: a data key-up as a test's.
        #[cfg(feature = "bench")]
        let fsk = data.is_none() && test.is_none() && self.fsk_tx_armed();
        #[cfg(feature = "bench")]
        let testing = test.is_some() || fsk;
        #[cfg(not(feature = "bench"))]
        let testing = false; // TERM-38: `txtest` is a bench build's
        // TX audit F11: the UI's refusals made again here from the Radio task's own state. HF mode: AM on chip 1
        // is receive only; the SI4732's frequencies and modes (HF-24) and HF mode already left refuse.
        let hf = if home { None } else { Some(self.hf) };
        let req = Req {
            tx: f,
            rx,
            kind: if data.is_some() || testing { TxKind::Data } else { TxKind::Voice },
            person: data.is_none(),
            confirmed, // only the emergency prompt's second PTT (TX-3, TX-35)
            am: if home { self.slots[s].am } else { self.hf.is_some_and(|h| h.mode == 2) },
            rx_only: home && self.slots[s].rx_only,
            busy: home && self.slots[s].busy && self.live.slots[s].squelch_open,
            hf_si: hf.is_some_and(|h| h.is_none_or(|h| h.si)),
            si_on: self.si.mode().is_some() && self.hf.is_none(),
            region,
            extend,
            cal_ok: self.tx.cal.is_some(),
            cal_window: self.tx.cal.is_some_and(|c| c.in_window(f)),
            batt_ok: self.battery_ok(),
            power: data.unwrap_or(self.tx.power[s]).min(2),
        };
        if let Err(r) = self.tx.seq.check(&req, &self.path, now) {
            return self.refuse(slot, r);
        }
        // TX-11 (TX audit F16): the PA lookup refuses outside its rows, before anything is touched.
        let Some(pa) = self.tx.cal.and_then(|c| c.dac(f, req.power)) else {
            return self.refuse(slot, Refusal::NotCalibrated);
        };
        // The chip for the TX frequency: tuned there with the slot's tone, before the plan keys it.
        let n = Sel::tx(f).chip();
        if !self.has(n) {
            return self.refuse(slot, Refusal::Locked);
        }
        // TX-43 (TX audit F9): the cable stops before the key-up, not after it; the snapshot after this command
        // sets RF_STATE as it turned out. GPS-22 (F15): PB10 low now, as stock's PTT press (0x0801C6D4).
        crate::chan::RF_STATE.fetch_or(crate::chan::RF_BUSY_KEYED, core::sync::atomic::Ordering::Relaxed);
        crate::tasks::gps::pb10_low_now();
        speaker(false);
        // PWR-4: no Battery Save sleep from here on; decision 111: the other chip sleeps while keyed, unless a kept
        // slot (APRS mode's C) has it. The receive tick wakes what it needs after the unkey.
        let a = self.save.activity(now_ms());
        self.apply(a).await;
        let kept = self.kept();
        if !self.tuned[n as usize ^ 1].is_some_and(|(s, _)| (s as usize) < SLOTS && kept & 1 << s != 0) {
            self.tune_chip(n ^ 1, None).await;
        }
        self.tuned[n as usize] = Some((slot, f)); // not the receive frequency: retuned after the unkey
        self.ldo_on(n);
        let tone = self.tx.tone[s];
        // The keyed slot's bandwidth (HF mode's NFM is its mode 1).
        let narrow = if slot < SLOTS as u8 { self.slots[s].narrow } else { self.hf.is_some_and(|h| h.mode == 1) };
        // TX-49 (decision 306): DTMF detection (DEC-6) shares REG_70 with the tone generator; left armed, it went
        // out as 1000 and 3300 Hz tones under every key-up (bench, Oct 27). Off on the keyed chip, always; the
        // receive tick arms it again after the unkey's retune.
        self.chip(n).dtmf_rx(false);
        self.dtmf_key[n as usize] = None;
        let setup = self.tx_chip_setup(f, narrow, tone, data.is_some() || testing);
        radio_chip::tx_setup(&mut self.chip(n), &setup);
        // APRS-14 (decision 135): an APRS burst (a data key-up) goes out with the `data` TX audio set-up, as
        // `txtest`'s `data`; a test with `flat` or `data` its own; both put back at the unkey. `afsk-bk` and `voice`
        // leave the chip's voice settings.
        #[cfg(feature = "bench")]
        let filters = ferrite_core::txtest::key_up_filters(data.is_some(), test);
        #[cfg(not(feature = "bench"))]
        let filters = ferrite_core::txfilters::aprs_filters(data.is_some());
        if let Some(f) = filters {
            self.tx.test_regs = Some((n, radio_chip::tx_test_filters(&mut self.chip(n), f)));
        }
        // TERM-33.15, TERM-33.16: a test's `bypass` (REG_47<0>, the soft limiter off) and its equaliser's gain
        // stages (REG_7D, REG_40), over the set-up just made; put back at the unkey before it.
        #[cfg(feature = "bench")]
        if let Some(t) = test.filter(|t| filters.is_some() && (t.bypass || t.eq != ferrite_core::txeq::Eq::Off)) {
            let dev = (t.eq != ferrite_core::txeq::Eq::Off).then(|| t.eq.deviation());
            self.tx.test_extra = Some((n, radio_chip::tx_test_extra(&mut self.chip(n), t.bypass, t.eq.mic_steps(), dev)));
        }
        self.tx.pa = pa;
        self.tx.fault = false;
        self.live.tx = TxLive { keyed: false, slot, mic: 0, refused: None, dtmf_sent: self.live.tx.dtmf_sent, voice: self.live.tx.voice, ptt_stuck: self.live.tx.ptt_stuck };
        (self.tx.slot, self.tx.dialled) = (slot, false);
        let r = self.tx.seq.key(&req, &mut self.path, now, &mut st);
        if let Err(r) = r {
            self.unkeyed();
            return self.refuse(slot, r);
        }
        log(b"tx key khz ", f / 100);
        self.run(&st).await;
        if self.tx.fault {
            let mut st = Steps::default();
            self.tx.seq.safe(&mut self.path, now_ms() as u32, &mut st);
            self.run(&st).await;
            self.unkeyed();
            return self.refuse(slot, Refusal::Locked);
        }
        self.live.tx.keyed = true;
        self.tx.chip = Some(n);
        // TERM-33: DAC1 plays the test from here to the release (PC12 high meanwhile, PB8 already on); `afsk-bk`
        // is the keyed chip's tone generator instead, timed by TMR5 (`afskbk`), DAC1 and PC12 untouched.
        // TERM-34: the suite runs step by step from here (`suite`), DAC1 and TMR5 as its steps ask.
        #[cfg(feature = "bench")]
        if let Some(t) = test {
            self.tx.test = None;
            if t.signal == ferrite_core::txtest::Signal::Suite {
                self.suite_start(n, &setup);
            } else if let Some(g) = ferrite_core::txtest::Gen::from_arm(&t) {
                self.tx.test_playing = true;
                crate::tasks::dsp::TXTEST.signal(Some((g, t.eq)));
            } else {
                self.bk_test_start(n, &t);
            }
            log(b"txtest playing ", t.signal as u32 + 1);
        }
        #[cfg(feature = "bench")]
        if fsk {
            self.fsk_key_up(n);
        }
    }

    /// TERM-33: the test signal stopped (the DSP task puts DAC1 to mid-scale and PC12 low; `afsk-bk`'s timer
    /// stops and the tones go off); TERM-34: the suite ended and the registers it changed put back.
    /// Nothing in a user build (TERM-38).
    fn test_audio_off(&mut self) {
        #[cfg(feature = "bench")]
        {
            self.suite_stop();
            if core::mem::take(&mut self.tx.test_playing) {
                crate::tasks::dsp::TXTEST.signal(None);
            }
            self.afsk_bk_stop();
            self.fsk_tx_stop(); // TERM-43
        }
    }

    /// The chip set-up for a key-up (TX-23; TX audit F1, F2, F7, F14): the calibration's sub-tone deviation at the
    /// Subtone level (TONE-1, TONE-16), the mic gain (VOX-15) or V0.29's fixed 0x34 for a data burst (VOX-6). Notes
    /// the bandwidth for the keying steps (F3). The calibration is there: `TxSeq::check` refused before this if not.
    pub(super) fn tx_chip_setup(&mut self, freq: Freq10, narrow: bool, tone: u16, data: bool) -> radio_chip::TxSetup {
        self.tx.narrow = narrow;
        let tone_dev = self.tx.cal.map_or(0, |c| c.tone_dev(tone, self.tone_level));
        let mic = if data { ferrite_core::vox::MIC_DATA } else { ferrite_core::vox::mic_reg(self.tx.mic) };
        let freq = self.tx.cal.map_or(freq, |c| c.tx_word(freq)); // CAL-12: the factory crystal trim
        radio_chip::TxSetup { freq, narrow, tone, tone_dev, mic }
    }

    /// While keyed: the sequencer's own unkeys (timeout backstop, duty limit, battery under load).
    pub(super) async fn tx_tick(&mut self, now: u32) {
        #[cfg(feature = "bench")]
        self.fsk_tx_poll(); // TERM-43: the next frame at each transmit-done flag
        let batt = self.battery_ok();
        // TX-12's mic meter: PA0, the mic-level tap stock's VOX reads (decision 76).
        self.live.tx.mic = tonegen::mic_percent(crate::hal::adc::read(crate::hal::adc::CH_PA0));
        let mut st = Steps::default();
        let cfg = self.tx_cfg();
        if let Some(e) = self.tx.seq.tick(&cfg, batt, &mut self.path, now, &mut st) {
            self.run(&st).await;
            if e != txseq::Event::PreAlert {
                log(b"tx unkeyed by the sequencer ", e as u32);
                self.unkeyed();
                self.live.tx.refused = Some(match e {
                    txseq::Event::Hot => Refusal::Hot,
                    txseq::Event::Battery => Refusal::Battery,
                    _ => Refusal::ReleasePtt,
                });
            }
        }
    }

    /// The sequencer's settings from what the UI has sent: the keyed slot's end tone and tail, CW ID with a
    /// callsign (VOX-10); none of the end tone or ID after a dialled sequence (TONE-11).
    pub(super) fn tx_cfg(&self) -> txseq::Cfg {
        let s = self.tx.slot as usize % 4;
        let voice = !self.tx.dialled;
        txseq::Cfg {
            end_tone: voice && self.tx.end[s] != 0,
            cw_id: voice && self.tx.cw != 0 && self.tx.cw_len != 0,
            cw_each: self.tx.cw == 2,
            tail: self.tx.tail[s],
            timeout_s: self.tx.timeout_s + TX_BACKSTOP_S,
            ..TX_CFG
        }
    }

    /// TONE-5: the tail at unkey, still keyed, for stock's 400 ms: the 55 Hz tone with no sub-tone, the phase
    /// shift (0 8 8 3) on CTCSS, the 134.4 Hz turn-off code on DCS. The 55 Hz tone at the calibration's CTCSS byte
    /// (0xF210 byte 1), as stock (TX audit F7).
    pub(super) async fn tail_elim(&mut self) {
        let Some(n) = self.tx.chip.filter(|&n| self.has(n)) else { return };
        let dev = self.tx.cal.map_or(0, |c| c.tail_dev());
        let kind = radio_chip::tail(self.tx.tone[self.tx.slot as usize % 4], self.tx.phase, dev);
        self.chip(n).tail_tx(Some(kind));
        Timer::after_millis(tonegen::TAIL_MS as u64).await;
        self.chip(n).tail_tx(None);
        log(b"tx tail", 0);
    }

    /// DTMF-1, DTMF-3: the dialled sequence on the transmission just keyed: the first digit delay, then each
    /// symbol on and off for the speed; then the UI is told and unkeys. Started here and stepped by the tick
    /// ([`Radio::dial_step`], at most 1 s + 16 x 400 ms), so an unkey or power off meanwhile is taken at once
    /// (MODE-7); it ends with the transmission.
    pub(super) fn dial(&mut self, len: u8) {
        if self.path.keyed().is_none() {
            return;
        }
        self.tx.dialled = true;
        let at = (now_ms() as u32).wrapping_add(self.tx.dtmf.1 as u32 * 10);
        self.tx.dial = Some(Dial { k: 0, len: len.min(16), on: false, at });
    }

    /// When the dial's next step is due (the main loop's wait), while one runs.
    pub(super) fn dial_due(&self) -> Option<u32> {
        self.tx.dial.map(|d| d.at)
    }

    /// The dial's next step, when due: a symbol on, its off time, or the end.
    pub(super) fn dial_step(&mut self) {
        let Some(mut d) = self.tx.dial else { return };
        let now = now_ms() as u32;
        if now.wrapping_sub(d.at) > u32::MAX / 2 {
            return; // not due
        }
        if self.path.keyed().is_none() {
            self.tx.dial = None; // cut off (unkey, timeout, battery)
            return;
        }
        let ms = self.tx.dtmf.0 as u32;
        let sym = (d.k < d.len).then(|| tonegen::dtmf_symbol(self.tx.digits[d.k as usize]));
        match (d.on, sym) {
            (false, Some(Some(c))) => {
                self.keyed_tone(None, Some(c));
                d.on = true;
            }
            (_, Some(_)) => {
                if let Some(n) = self.path.keyed().map(|s| s.chip()).filter(|&n| self.has(n)) {
                    self.chip(n).tone_tx(0);
                }
                (d.on, d.k) = (false, d.k + 1);
            }
            (_, None) => {
                self.tx.dial = None;
                self.keyed_tone(None, None);
                self.live.tx.dtmf_sent = self.live.tx.dtmf_sent.wrapping_add(1);
                log(b"tx dtmf sent ", d.len as u32);
                return;
            }
        }
        d.at = now.wrapping_add(ms);
        self.tx.dial = Some(d);
    }

    /// While keyed (TONE-13, DTMF-7, DTMF-11): the tone burst or a DTMF symbol on the keyed chip, as stock sends any
    /// internal tone: the generator set, REG_30 to tone transmit (mode 4, 0xC3FA: the microphone off) and the
    /// sidetone heard (DTMF-8, for DTMF as 0 8 5 4 says); both None goes back to
    /// the microphone (0xC1FE).
    pub(super) fn keyed_tone(&mut self, hz: Option<u16>, dtmf: Option<char>) {
        let Some(sel) = self.path.keyed() else { return };
        let n = sel.chip();
        if !self.has(n) {
            return;
        }
        let heard = dtmf.is_none() || self.tx.sidetone; // DTMF-8's setting; the burst is always heard
        let narrow = self.tx.narrow;
        let (sending, mic_ok) = radio_chip::keyed_tone(&mut self.chip(n), hz, dtmf, heard, narrow);
        if !mic_ok {
            log(b"tx mic read-back failed, chip ", n as u32);
        }
        speaker(sending && heard);
    }

    /// A tone sequence on the keyed chip, still keyed (TONE-8: end tone, CW ID): tone transmit with the microphone
    /// off and the tones' REG_40 (TONE-19), silent for [`tonegen::TONE_LEAD_MS`] while the generator settles
    /// (TONE-10), then each step's tone (0 = silence) for its time; REG_40 back after.
    pub(super) async fn play_keyed(&mut self, steps: impl Iterator<Item = tonegen::Step>) {
        let Some(n) = self.tx.chip.filter(|&n| self.has(n)) else { return };
        let narrow = self.tx.narrow;
        radio_chip::tones_begin(&mut self.chip(n), narrow);
        Timer::after_millis(tonegen::TONE_LEAD_MS as u64).await;
        for (hz, ms) in steps {
            self.chip(n).tone_tx(hz as u32);
            Timer::after_millis(ms as u64).await;
        }
        radio_chip::tones_end(&mut self.chip(n));
    }

    /// After any unkey: the chips back to their slots on the next tick, the UI told.
    pub(super) fn unkeyed(&mut self) {
        // TERM-33: a test's audio off and the `flat` or `data` registers back (an APRS burst's too), on the chip
        // that was keyed.
        self.test_audio_off();
        #[cfg(feature = "bench")]
        if let Some((n, r)) = self.tx.test_extra.take().filter(|(n, _)| self.has(*n)) {
            radio_chip::tx_test_extra_restore(&mut self.chip(n), &r);
        }
        if let Some((n, r)) = self.tx.test_regs.take().filter(|(n, _)| self.has(*n)) {
            radio_chip::tx_test_restore(&mut self.chip(n), &r);
        }
        self.tuned = [None; 2];
        self.tx.chip = None;
        self.tx.dial = None;
        self.live.tx.keyed = false;
        log(b"tx unkey", 0);
    }

    pub(super) fn refuse(&mut self, slot: u8, why: Refusal) {
        self.live.tx = TxLive { keyed: false, slot, mic: 0, refused: Some(why), dtmf_sent: self.live.tx.dtmf_sent, voice: self.live.tx.voice, ptt_stuck: self.live.tx.ptt_stuck };
        log(b"tx refused ", why as u32);
    }
}
