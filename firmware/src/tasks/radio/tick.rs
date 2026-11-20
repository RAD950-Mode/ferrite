//! The receive tick: the engines, the watch hop and the audio; the ADC reads and VOX.

use super::*;

impl Radio {
    /// The engines' tick: each listening chip's squelch, the watch turn, the scans, the chips and the path, and
    /// the audio.
    /// SBAR-20: the MCU's die temperature every 10 s, for the Status page (informational; read here because the
    /// Radio task owns the ADC's software reads).
    fn mcu_temp(&mut self) {
        use crate::hal::adc;
        let now = now_ms() as u32;
        // PWR-20: the battery (PA1) every 2 s, here too, as the Radio task owns the ADC's software reads.
        if self.bat_at.is_none_or(|t| now.wrapping_sub(t) >= 2_000) {
            self.bat_at = Some(now);
            adc::init();
            crate::chan::BATTERY_RAW.store(adc::read_avg(adc::CH_PA1, 8), core::sync::atomic::Ordering::Relaxed);
        }
        if self.temp.0.is_some_and(|t| now.wrapping_sub(t) < 10_000) {
            return;
        }
        self.temp.0 = Some(now);
        adc::init();
        // The battery's PA1 read just before (every 10 s the two fall together) leaves its divider's voltage on the
        // sample capacitor: one conversion of each internal channel thrown away first.
        adc::sensors(true);
        let _ = adc::read(adc::CH_TEMP);
        let ts = adc::read_avg(adc::CH_TEMP, 4);
        let _ = adc::read(adc::CH_VREF);
        let vref = adc::read_avg(adc::CH_VREF, 4);
        adc::sensors(false);
        crate::chan::MCU_TEMP_RAW.store((ts as u32) << 16 | vref as u32, core::sync::atomic::Ordering::Relaxed);
        if let Some(c10) = ferrite_core::mcutemp::temp_c10(ts, vref) {
            let s = ferrite_core::mcutemp::smooth(self.temp.1, c10);
            self.temp.1 = Some(s);
            crate::chan::MCU_TEMP.store(s, core::sync::atomic::Ordering::Relaxed);
        }
    }

    /// VOX (VOX-2, VOX-3, VOX-5, VOX-14): every 20 ms, PA0's mic level against the level's threshold; speech
    /// (with its tail) goes to the UI in `TxLive::voice`, which keys the active slot (VOX-13). Blocked while the
    /// active slot receives and nothing is keyed, while the input task debounces a speaker-mic change, or while the
    /// UI shows anything but home (VOX-3).
    #[cfg(feature = "tx")]
    fn vox_poll(&mut self) {
        use crate::hal::adc;
        let (level, tail) = self.tx.vox;
        if level == 0 {
            return;
        }
        let now = now_ms() as u32;
        if now.wrapping_sub(self.tx.vox_at) < 20 {
            return;
        }
        self.tx.vox_at = now;
        adc::init();
        let mic = adc::read(adc::CH_PA0);
        let changing = crate::chan::ACCESSORY.load(core::sync::atomic::Ordering::Relaxed) & ferrite_core::acc::CHANGING != 0;
        // VOX-3, as stock (0x08025994): never while any audio plays (a slot heard, the broadcast, a beep: the
        // speaker on, which also turns the tap off through PB8) or any slot's squelch is open; then the 1.5 s hold.
        let audio = SPEAKER_ON.load(core::sync::atomic::Ordering::Relaxed) || self.si.mode().is_some();
        // And, as stock's UI mode (state+1 != 0), any screen but home: the detector reset, 1.5 s after home again.
        let blocked = changing || !self.tx.vox_screen || self.path.keyed().is_none() && (audio || self.live.slots.iter().any(|s| s.squelch_open));
        self.live.tx.voice = self.tx.vox_det.step(mic, level, tail as u32 * 100, blocked, now);
    }

    pub(super) async fn tick(&mut self) {
        // SAFE-14: the PTT inputs looked at every tick, so the guard learns they were let go after boot. Looked at
        // only at key-up (as until Oct 13, 2026), PTT is pressed then and every key-up was refused on the radio.
        #[cfg(feature = "tx")]
        self.ptt_seen_released();
        self.mcu_temp();
        self.seek_poll(); // HF-31: a seek runs here, never inside a command (MODE-7)
        self.fm_poll();
        // SQL-1: the calibration's squelch tables, once the UI task has read them: every tuned chip again; with
        // a factory crystal trim (CAL-12), its frequency too.
        if let Some(c) = crate::chan::SQ_CAL.try_take() {
            let retune = c.trim() != self.sqcal.trim();
            self.sqcal = c;
            for n in 0..2u8 {
                if let Some((s, f)) = self.tuned[n as usize] {
                    if retune && self.path.keyed().is_none() {
                        let w = self.sqcal.rx_word(f);
                        let mut bk = self.chip(n);
                        bk.set_freq(w * 10);
                        bk.rx_on();
                    }
                    let level = self.slots.get(s as usize).map_or(self.slots[self.active as usize].sq, |sl| sl.sq);
                    let regs = self.sqcal.regs(f, level);
                    radio_chip::squelch(&mut self.chip(n), regs);
                    self.sq_regs[n as usize] = Some(regs);
                }
            }
        }
        #[cfg(feature = "tx")]
        self.vox_poll();
        #[cfg(feature = "tx")]
        self.dial_step(); // DTMF-1: the dialled sequence's next step
        if self.rad950.is_some() {
            #[cfg(feature = "tx")]
            if self.path.keyed().is_some() {
                return self.tx_tick(now_ms() as u32).await;
            }
            self.rad950_poll();
            return;
        }
        if self.hf.is_some() {
            // Keyed from HF mode (HF-23): the keyed tick as from home (TX-30 backstop, TX-31 duty limit, TX-32
            // battery under load); the HF receiver's report waits for the unkey (TX audit M5).
            #[cfg(feature = "tx")]
            if self.path.keyed().is_some() {
                return self.tx_tick(now_ms() as u32).await;
            }
            if self.seek.is_none() {
                self.hf_poll();
            }
            return;
        }
        // Satellite mode owns both chips while its screen is open (SAT-1, SAT-14): no watch, scan or decoders.
        if self.sat.is_some() {
            return self.sat_tick().await;
        }
        // The crossband repeater owns both chips while it runs (XB-13: no watch, scan or Battery Save); local PTT
        // holds it (XB-14), and that transmission runs as any other.
        #[cfg(feature = "tx")]
        if let Some(held) = self.xb.as_ref().map(|x| x.rep.held()) {
            // XB-13: Battery Save stepped as busy, so its receivers stay awake and its deadline stays ahead (decision
            // 163: left unstepped, it went past 10 s after the start and the Radio task woke every 1 ms).
            let a = self.save.tick(now_ms(), true);
            self.apply(a).await;
            let now = now_ms() as u32;
            if !held {
                return self.xb_tick(now).await;
            }
            if self.path.keyed().is_some() {
                return self.tx_tick(now).await;
            }
            return;
        }
        // Battery Save asleep: only its phase change runs (PWR-*). Returning before it, as this did, left its wake
        // time in the past, so the main loop's wait fired at once forever: the Radio task spun above the UI and the
        // receivers never woke (on the black radio, Oct 12, 2026, once real squelch thresholds let it sleep).
        if self.save.asleep() {
            let a = self.save.tick(now_ms(), false);
            self.apply(a).await;
            // A kept slot (APRS mode's C) listens on through the sleep: the turn runs for it alone (PWR-4).
            if self.save.asleep() && self.kept() == 0 {
                self.bcast_audio(now_ms() as u32, false); // BCR-19: the broadcast plays on, and returns on time
                return;
            }
        }
        let now = now_ms() as u32;
        if self.scope.is_some() {
            return self.scope_tick(now);
        }
        if self.counter.is_some() {
            return self.counter_tick(now).await;
        }
        if self.tsearch.is_some() {
            return self.tsearch_tick().await;
        }
        // Keyed: the chips and paths are the transmission's (TX-29); only the sequencer's timeout, duty limit
        // and battery under load run.
        #[cfg(feature = "tx")]
        if self.path.keyed().is_some() {
            return self.tx_tick(now).await;
        }
        let listening = self.hop.chip();
        // 1. What each chip heard for the slot it was on: the carrier squelch (watch) and the tone (audio); for a
        // scan's fast step, the RSSI over the floor (SCAN-21).
        let mut sq = [false; 2];
        let mut heard = [false; 2]; // the chips read this tick: the others keep their slot's last state (HOME-15)
        let mut toned = [false; 2];
        let mut scan_open = [false; SLOTS];
        for n in 0..2u8 {
            let Some((s, _)) = self.tuned[n as usize] else { continue };
            if listening != Some(n) || s >= SLOTS as u8 {
                continue;
            }
            // SQL-18 (decision 269): the software squelch on FM, with the thresholds the chip was given; AM, or
            // `sq chip`, keeps the chip's own decision, as does the APRS slot (SQL-18.4: a packet needs the chip's
            // hold through a noisy reading and its tail; its quick close bought nothing on a muted slot).
            let aprs = self.aprs_slots & 1 << s != 0;
            let soft = (!ferrite_core::softsq::chip_decides() && !self.slots[s as usize].am && !aprs).then_some(()).and(self.sq_regs[n as usize]);
            let mut bk = self.chip(n);
            let (raw, st) = (bk.rssi(), bk.status());
            let noise = if soft.is_some() { bk.noise() } else { 0 };
            let rssi = raw as i16 / 2 - 160;
            if self.sq_key[n as usize] != self.tuned[n as usize] {
                // retuned: a fresh squelch, closed
                (self.soft_sq[n as usize], self.sq_key[n as usize]) = (Default::default(), self.tuned[n as usize]);
                self.aprs_sq[n as usize].retuned(now); // SQL-19.2: settling
            }
            let decided = match soft {
                Some(regs) => {
                    let fast = self.scans.get(s as usize).is_some_and(|sc| sc.listening()) || self.aprs_slots & 1 << s != 0;
                    #[cfg(feature = "bench")]
                    let was = self.soft_sq[n as usize].open();
                    let now_open = self.soft_sq[n as usize].step(&ferrite_core::softsq::Limits::from_regs(regs), raw, noise, fast);
                    #[cfg(feature = "bench")]
                    if now_open != was {
                        // the bench's check against the chip: each change, its readings, and what the chip said
                        log(if now_open { b"sq open, noise " } else { b"sq shut, noise " }, noise as u32);
                        log(b"  rssi dBm + 200: ", (rssi + 200).max(0) as u32);
                        log(b"  chip says open: ", st.squelch_open as u32);
                    }
                    now_open
                }
                // SQL-19: the APRS slot over the chip's decision: no spikes, nothing while the retune settles
                None if aprs => {
                    let (o, _ended) = self.aprs_sq[n as usize].step(now, st.squelch_open);
                    crate::tasks::dsp::SQ_LAG_MS.store(self.aprs_sq[n as usize].lag_ms, core::sync::atomic::Ordering::Relaxed); // APRS-56.4
                    #[cfg(feature = "bench")]
                    if let Some(ms) = _ended {
                        log(b"aprs sq open ms ", ms);
                    }
                    o
                }
                None => st.squelch_open,
            };
            let open = decided || self.slots[s as usize].sq == 0;
            let fl = &mut self.floor[n as usize];
            if !decided {
                *fl += (rssi - *fl) / 8; // a slow average of the quiet RSSI
            }
            sq[n as usize] = open;
            heard[n as usize] = true;
            // TSRCH-12: while the tone hint probes, the detectors scan: their reading, not a match, so the audio
            // stays shut this tick.
            let probing = self.hints[s as usize].probing();
            toned[n as usize] = !probing && self.slots[s as usize].toned(open, &st);
            let sl = &mut self.live.slots[s as usize];
            sl.rssi = rssi;
            scan_open[s as usize] = match self.scans[s as usize].check() {
                Check::Rssi => rssi > *fl + FAST_MARGIN_DB,
                Check::Squelch => open,
            };
            if probing {
                let (c, d) = self.chip(n).subtone_scan();
                let heard = radio_chip::heard_tone(c, d, &ferrite_app::slot::CTCSS, &ferrite_app::slot::DCS);
                if heard.is_some_and(|w| self.hints[s as usize].found(now, w) == HintAct::Restore) {
                    let sl = self.slots[s as usize];
                    radio_chip::subtone(&mut self.chip(n), sl.rx_tone, sl.rx_tail);
                }
            }
        }
        // PWR-26.9: the APRS slot's squelch to the DSP task, whose AFSK capture (and so the build's speed) runs only
        // while it is open: the chip read above, or the one APRS mode keeps awake beside the hop (APRS-16).
        let mut aprs_open = false;
        for n in 0..2u8 {
            let Some((s, _)) = self.tuned[n as usize] else { continue };
            if self.aprs_slots & 1 << s != 0 {
                // APRS-56.5: the capture from the chip's first open reading, a tick before the slot opens
                aprs_open |= if listening == Some(n) { sq[n as usize] || self.aprs_sq[n as usize].arming() } else { self.kept() & 1 << s != 0 && self.chip(n).status().squelch_open };
            }
        }
        if aprs_open != self.aprs_open_sent && crate::chan::DSP.try_send(ferrite_core::msg::DspCmd::Squelch(aprs_open)).is_ok() {
            self.aprs_open_sent = aprs_open;
        }
        // SAME on a weather channel's chip (DEC-19), then DTMF on the others (DEC-6).
        self.same_tick(now);
        self.dtmf_tick(now, &sq);
        // 2. Scans step while their slot has a chip, on the answer from that chip. Before the watch turn, so the
        // turn gives a step to the other band that band's chip in this tick (decision 166): after it, as until Oct
        // 15, 2026, the old band's chip was tuned to the new frequency for a tick and the right one a tick late,
        // so a 70 cm channel after a 2 m one had 10 ms less of its step, from a chip just woken.
        for i in 0..SLOTS {
            if !self.scans[i].running() || !self.tuned.iter().any(|t| t.is_some_and(|(s, _)| s as usize == i)) {
                continue;
            }
            let o = if self.chans.is_on(i) {
                self.chans.tick(i, &mut self.scans[i], now, scan_open[i])
            } else {
                self.scans[i].tick(&self.ranges[i], now, scan_open[i])
            };
            if o.ended {
                self.chans.off(i);
            }
            let sl = &mut self.live.slots[i];
            if let Some(f) = o.tune {
                self.slots[i].freq = f;
                sl.freq = f;
            }
            sl.held = self.scans[i].on_hit();
            sl.chan = if self.chans.is_on(i) { self.chans.chan(&self.scans[i]) + 1 } else { 0 };
            if o.hit.is_some() {
                sl.hits = sl.hits.saturating_add(1);
                sl.last_hit = sl.freq;
                sl.hit_chan = sl.chan;
                log([b"scan hit A ", b"scan hit B ", b"scan hit C "][i % 3], sl.freq); // its frequency, 10 Hz
            }
            sl.skips = self.scans[i].skips.max(sl.skips);
            if o.ended {
                sl.scanning = false;
            }
        }
        // 3. The watch turn, on the frequencies the scans just stepped to.
        let asleep = if self.save.asleep() { !self.kept() } else { 0 };
        let cfg = watch::Cfg { set: self.watch_set, active: self.active, freq: self.slots.map(|s| s.freq), muted: self.user_mute, off: self.bcast_off() | asleep | self.user_off, held: self.monitor, prio: self.kept(), aprs: self.aprs_slots, scan: self.range_scans(), first: watch::aprs_first(), swap: self.swap };
        let keyed = self.path.keyed().is_some();
        let out = self.watcher.turn(now, &cfg, sq, heard, keyed);
        self.live.dropped = out.dropped;
        // 2a. The tone hint (TSRCH-12, TSRCH-15): on the chip listening, for the slot the watch holds for audio.
        self.hint_tick(now, listening, &sq, &toned, out.audio);
        // The channel the channel scans need next, for the UI to answer (LiveAll::want).
        self.live.set_want(self.chans.want(&self.scans));
        // 4. The path on the chip to listen to now, and that chip on the slot the engine names; the other asleep
        // unless a kept slot has it (WATCH-2, PWR-7, decision 111: one BK4829 awake at a time, as stock).
        let path = self.hop.next(now, &out, self.active);
        let awake = watch::awake(&out, path, self.kept());
        // PWR-21: the chip the hop turns to by the next tick has its supplies on now, so their settle overlaps
        // this dwell and the hop isn't delayed; it is tuned (receive on) at the hop as before.
        let early = if self.settle.wake_ms > 0 { self.hop.due(now, &out, self.active, TICK_MS as u32) } else { None };
        // The chips to listen with first, those to sleep after: a sleep's settle never holds up the other chip.
        for listen in [true, false] {
            for n in 0..2u8 {
                let want = out.chip[n as usize].filter(|_| awake[n as usize]).map(|s| (s, self.slots[s as usize].rx_freq()));
                if want.is_some() != listen {
                    continue;
                }
                if want.is_none() && early == Some(n) && self.has(n) && self.tuned[n as usize].is_none() {
                    self.ldo_on(n);
                    continue;
                }
                self.tune_chip(n, want).await;
            }
        }
        if let Some(n) = path {
            if let Some((_, f)) = self.tuned[n as usize] {
                self.select_rx(f, Cause::Auto).await;
            }
        }
        // SQL-17: a chip on a scanning slot's step or priority look, or on the APRS slot (decision 181: it must
        // catch a packet's carrier within its turn), opens at once; ordinary receive keeps stock's open delay.
        for n in 0..2u8 {
            if let Some((s, _)) = self.tuned[n as usize] {
                let fast = self.scans.get(s as usize).is_some_and(|sc| sc.listening()) || self.aprs_slots & 1 << s != 0;
                self.open_delay(n, fast);
            }
        }
        // 5. Audio (WATCH-3): the slot the engine picks, if its tone matches; a latched monitor on the active
        // slot opens it whatever it hears (SQL-2).
        let heard_chip = |s: u8| (0..2u8).find(|&n| self.tuned[n as usize].is_some_and(|(t, _)| t == s));
        let monitor_active = self.monitor & !(self.user_mute | self.user_off) & 1 << self.active != 0; // HOME-15: muted or off, not heard
        let audio = if monitor_active { Some(self.active) } else { out.audio };
        let audio_chip = audio.and_then(heard_chip).filter(|&n| monitor_active || toned[n as usize]);
        // APRS-21.1, APRS-16: a priority slot holding its chip for a packet with nothing on the speaker (muted, or
        // another slot's tone) still sends its audio to the decoder (EARO, PA2); the speaker stays off. Not beside a
        // broadcast, whose audio shares the speaker's path.
        // APRS-56.5: and the APRS slot's chip from its first open reading, before the slot opens (the capture starts
        // then too): muted until the watch picked it, the decoder lost a packet's first 40-50 ms of flags.
        let aprs_arming = (0..2u8).find(|&n| {
            listening == Some(n) && self.tuned[n as usize].is_some_and(|(s, _)| self.aprs_slots & 1 << s != 0) && (sq[n as usize] || self.aprs_sq[n as usize].arming())
        });
        let decode_chip = audio_chip.or(out.hold).or(aprs_arming).filter(|_| self.si.mode().is_none() || audio_chip.is_some());
        for n in 0..2u8 {
            if self.tuned[n as usize].is_some() {
                // TERM-42.1: a bench `bk rxaf` route (its top nibble) in place of the normal one
                #[cfg(feature = "bench")]
                let normal = match self.rxaf.map(|b| (b >> 12) as u8) {
                    Some(r @ 1..) => AfOut::Raw(r),
                    _ => AfOut::Normal,
                };
                #[cfg(not(feature = "bench"))]
                let normal = AfOut::Normal;
                self.chip(n).af_out(if Some(n) == decode_chip { normal } else { AfOut::Mute });
            }
        }
        self.bcast_audio(now, audio_chip.is_some());
        // 6. The snapshot.
        let [a, b] = self.aprs_sq.map(|q| q.counts);
        let add = |x: u16, y: u16| x.saturating_add(y);
        self.live.aprs_sq = ferrite_core::aprssq::Counts { opens: add(a.opens, b.opens), short: add(a.short, b.short), spikes: add(a.spikes, b.spikes), settle: add(a.settle, b.settle) };
        let active_relay = Sel::rx(self.slots[self.active as usize].freq).relay();
        for (i, sl) in self.live.slots.iter_mut().enumerate() {
            let open = self.watcher.open(i as u8) || self.monitor & 1 << i != 0;
            if !open {
                sl.rssi = sl.rssi.min(-121);
            }
            sl.squelch_open = open;
            sl.heard_tone = self.hints[i].heard;
            sl.relay = out.cancelled && self.watch_set & 1 << i != 0 && Sel::rx(self.slots[i].freq).relay() != active_relay;
        }
        // APRS-21.2: a slot deferring to the APRS slot shows its signal (the watch already gave it its squelch).
        if let Some(p) = out.defer_to {
            let rssi = self.live.slots[p as usize].rssi;
            for (i, sl) in self.live.slots.iter_mut().enumerate() {
                if out.defer & 1 << i != 0 {
                    sl.rssi = rssi;
                }
            }
        }
        // Battery Save: anything open or scanning keeps the receivers awake. Broadcast doesn't (BCR-19): the
        // BK4829s keep the level's cycle for the slots watched, and the sleep step leaves the SI4732 playing.
        let busy = self.live.slots.iter().any(|s| s.squelch_open || s.scanning);
        // SQL-8: the squelch opening or closing holds the receiver on for 10 s.
        let a = self.save.squelch(now_ms(), self.live.slots.iter().any(|s| s.squelch_open));
        self.apply(a).await;
        let a = self.save.tick(now_ms(), busy);
        self.apply(a).await;
    }

    /// The tone hint's step for the slot on the listening chip (TSRCH-12, TSRCH-15): eligible while the watch
    /// holds it for audio, it has a receive tone, and no scan or latched monitor runs on it (transmit, HF mode and
    /// the exclusive modes never reach the receive tick). The chip's detectors switch to scan, or back.
    fn hint_tick(&mut self, now: u32, listening: Option<u8>, sq: &[bool; 2], toned: &[bool; 2], audio: Option<u8>) {
        for h in &mut self.hints {
            h.age(now);
        }
        let Some(n) = listening else { return };
        let Some((s, _)) = self.tuned[n as usize].filter(|&(s, _)| (s as usize) < SLOTS) else { return };
        let i = s as usize;
        let sl = self.slots[i];
        let eligible = audio == Some(s) && !self.scans[i].running() && self.monitor & 1 << s == 0;
        match self.hints[i].tick(now, sl.freq, sl.rx_tone, sq[n as usize], toned[n as usize], eligible) {
            HintAct::Start => radio_chip::tone_probe(&mut self.chip(n)),
            HintAct::Restore => radio_chip::subtone(&mut self.chip(n), sl.rx_tone, sl.rx_tail),
            HintAct::None => {}
        }
    }
}
