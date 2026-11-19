//! SOL950 10 m weak-signal mode: SI4732 USB receive (`AVC ON`, 3.0 kHz filter, `SI_VOLUME = 56`, signed
//! `rx_corr_hz` BFO trim) and, in the `tx` build, BK4829 direct-PLL-stepped 4-FSK transmit (`REG_38/39` +
//! `REG_30` VCO toggle timed by TMR5).

use super::*;

/// SOL950's own index in `Radio::tuned` / `tx.slot` (distinct from `HF_SLOT = 3` and `SAT_SLOT = 4`).
#[cfg(feature = "tx")]
pub(super) const SOL950_SLOT: u8 = 5;

/// SI4732 front-end LNA gain offset (same as `hf.rs`).
const SI_FRONT_DB: i16 = 20;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Sol950Run {
    pub dial: Freq10,
    pub rx_corr_hz: i16,
    pub rx_ready: bool,
    pub tx_active: bool,
}

impl Radio {
    /// Enter, retune, or exit SOL950 mode on the SI4732 USB receiver.
    pub(super) async fn sol950_rx(&mut self, dial: Freq10, rx_corr_hz: i16, on: bool) {
        self.seek_cancel();
        if !on {
            #[cfg(feature = "tx")]
            if self.sol950.is_some_and(|r| r.tx_active) {
                self.sol950_tx_finish(false).await;
            }
            if self.sol950.take().is_some() {
                let _ = broadcast(&mut self.si, BroadcastCmd::Off, self.si_opts).await;
                self.tuned = [None; 2];
                self.live.mode = ModeLive::Off;
                let f = self.slots[self.active as usize].freq;
                let mut s = Steps::default();
                let _ = self.path.rx(Rx::Bk(Sel::rx(f)), Cause::HfReceiver, now_ms() as u32, &mut s);
                self.run(&s).await;
                log(b"sol950 off", 0);
            }
            return;
        }
        // Sleep both BK4829s and mute the speaker while SOL950 owns the SI4732 receive path.
        for n in 0..2u8 {
            self.tune_chip(n, None).await;
        }
        speaker(false);
        let mut s = Steps::default();
        let _ = self.path.rx(Rx::Si, Cause::HfReceiver, now_ms() as u32, &mut s);
        self.run(&s).await;
        let ok = self.si_ssb_sol950(dial, rx_corr_hz).await;
        log(if ok.is_ok() { b"sol950 si rx ok, khz " } else { b"sol950 si rx FAILED, khz " }, dial / 100);
        self.sol950 = Some(Sol950Run { dial, rx_corr_hz, rx_ready: ok.is_ok(), tx_active: false });
        self.sol950_poll();
    }

    /// Tune the SI4732 to USB at `dial` with `AVC ON` (`0x9012`, 3.0 kHz filter), `SI_VOLUME = 56`, and
    /// `SSB_BFO = base_bfo + rx_corr_hz`.
    async fn si_ssb_sol950(&mut self, dial: Freq10, rx_corr_hz: i16) -> Result<(), si4732::Error> {
        use si4732::prop;
        let d = &mut BusyDelay;
        let (khz, _usb, base_bfo) = ferrite_core::ssb::tune(dial, true, false);
        let bfo = (base_bfo as i32 + rx_corr_hz as i32).clamp(-16_000, 16_000) as i16;
        if self.si.mode() != Some(SiMode::Ssb) {
            self.ssb_at = None;
            if self.si.mode().is_some() {
                self.si.power_down(d)?;
            }
            si_input(SiMode::Ssb);
            SI_RST.write(false);
            Timer::after_millis(10).await;
            SI_RST.write(true);
            Timer::after_millis(10).await;
            self.si.power_up_start(SiMode::Ssb)?;
            let t0 = now_ms();
            while !self.si.ready()? {
                if now_ms() - t0 > SI_TUNE_MAX_MS {
                    return Err(si4732::Error::Timeout);
                }
                Timer::after_millis(5).await;
            }
            let up = now_ms();
            self.ssb_patch(dial, true).await?;
            let settled = up + 550;
            if now_ms() < settled {
                Timer::after_millis(settled - now_ms()).await;
            }
            self.si.volume(SI_VOLUME, d)?;
        }
        // Bench-verified SOL950 RX baseline: 3.0 kHz USB, sideband cutoff, AVC ON (0x9012), soft-mute off.
        let cfg = si4732::SsbConfig {
            audio_bw: 2, // 3.0 kHz
            sideband_cutoff: 1,
            avc_divider: 0,
            avc: true,
            soft_mute_snr: false,
            afc_off: true,
        };
        self.si.set_property(prop::SSB_MODE, cfg.bits(), d)?;
        self.si.set_property(prop::AM_AVC_MAX_GAIN, 0x7800, d)?;
        self.si.set_property(prop::AM_SOFT_MUTE_MAX_ATTENUATION, 0, d)?;
        self.si.agc(SiOpts::agc_index(self.si_opts.am_agc, 37), d)?;
        self.si.mute(false, d)?;
        if self.ssb_at != Some((khz, true)) {
            self.si.tune_start(khz, Sideband::Usb, d)?;
            let t0 = now_ms();
            while self.si.tune_poll(d)?.is_none() {
                if now_ms() - t0 > SI_TUNE_MAX_MS {
                    return Err(si4732::Error::Timeout);
                }
                Timer::after_millis(10).await;
            }
            self.ssb_at = Some((khz, true));
        }
        self.si.bfo(bfo, d)
    }

    /// Periodic poll while SOL950 is active: publishes `ModeLive::Sol950`.
    pub(super) fn sol950_poll(&mut self) {
        let Some(r) = self.sol950 else { return };
        let rssi = if r.rx_ready && !r.tx_active {
            match self.si.rsq(&mut BusyDelay) {
                Ok(q) => q.rssi as i16 - 107 - SI_FRONT_DB,
                Err(_) => -127,
            }
        } else {
            -127
        };
        self.live.mode = ModeLive::Sol950 {
            rx_ready: r.rx_ready,
            tx_active: r.tx_active,
            rssi,
            loading: 255,
        };
    }

    /// Abort any in-flight SOL950 burst immediately (PTT takeover, Stop, or mode exit).
    pub(super) async fn sol950_abort(&mut self) {
        #[cfg(feature = "tx")]
        if self.sol950.is_some_and(|r| r.tx_active) {
            self.sol950_tx_finish(false).await;
        }
    }
}

#[cfg(feature = "tx")]
impl Radio {
    /// Transmit the staged `chan::SOL950_TX` frame on 10 m (`freq`, `power`) via direct BK4829 PLL stepping.
    pub(super) async fn sol950_tx(&mut self, freq: Freq10, power: u8) {
        let Some(frame) = crate::chan::SOL950_TX.try_take() else {
            crate::chan::SOL950_TX_DONE.signal(false);
            crate::chan::TERM_WAKE.signal(());
            return;
        };
        let Some(run) = self.sol950 else {
            crate::chan::SOL950_TX_DONE.signal(false);
            crate::chan::TERM_WAKE.signal(());
            return;
        };
        let now = now_ms() as u32;
        if let Some(c) = crate::chan::TX_CAL.try_take() {
            (self.tx.cal, self.tx.gauge) = c;
        }
        let Some((plan_f, region, extend, _)) = self.tx.plan.take() else {
            self.refuse(SOL950_SLOT, Refusal::Locked);
            crate::chan::SOL950_TX_DONE.signal(false);
            crate::chan::TERM_WAKE.signal(());
            return;
        };
        let f = if plan_f != 0 { plan_f } else { freq };
        if !self.ptt_seen_released() {
            self.refuse(SOL950_SLOT, Refusal::PttAtBoot);
            crate::chan::SOL950_TX_DONE.signal(false);
            crate::chan::TERM_WAKE.signal(());
            return;
        }
        let power = power.min(2);
        let n = Sel::tx(f).chip();
        if !self.has(n) {
            self.refuse(SOL950_SLOT, Refusal::Locked);
            crate::chan::SOL950_TX_DONE.signal(false);
            crate::chan::TERM_WAKE.signal(());
            return;
        }
        let Some(pa) = self.tx.cal.and_then(|c| c.dac(f, power)) else {
            self.refuse(SOL950_SLOT, Refusal::NotCalibrated);
            crate::chan::SOL950_TX_DONE.signal(false);
            crate::chan::TERM_WAKE.signal(());
            return;
        };
        // Mute SI4732 and swing the RF path from `Rx::Si` to `Rx::Bk(Sel::HfRx)` (energizing the band relay).
        let _ = self.si.mute(true, &mut BusyDelay);
        let mut st = Steps::default();
        if self.path.rx(Rx::Bk(Sel::HfRx), Cause::HfReceiver, now, &mut st).is_err() {
            let _ = self.si.mute(false, &mut BusyDelay);
            self.refuse(SOL950_SLOT, Refusal::Locked);
            crate::chan::SOL950_TX_DONE.signal(false);
            crate::chan::TERM_WAKE.signal(());
            return;
        }
        self.run(&st).await;

        let req = Req {
            tx: f,
            rx: run.dial,
            kind: TxKind::Data,
            person: false,
            confirmed: false,
            am: false,
            rx_only: false,
            busy: false,
            hf_si: false,
            si_on: false,
            region,
            extend,
            cal_ok: self.tx.cal.is_some(),
            cal_window: self.tx.cal.is_some_and(|c| c.in_window(f)),
            batt_ok: self.battery_ok(),
            power,
        };
        if let Err(why) = self.tx.seq.check(&req, &self.path, now_ms() as u32) {
            self.sol950_restore_rx().await;
            self.refuse(SOL950_SLOT, why);
            crate::chan::SOL950_TX_DONE.signal(false);
            crate::chan::TERM_WAKE.signal(());
            return;
        }

        crate::chan::RF_STATE.fetch_or(crate::chan::RF_BUSY_KEYED, core::sync::atomic::Ordering::Relaxed);
        crate::tasks::gps::pb10_low_now();
        speaker(false);
        self.ldo_on(n);
        self.tuned[n as usize] = Some((SOL950_SLOT, f));
        self.chip(n).dtmf_rx(false);
        self.dtmf_key[n as usize] = None;
        let setup = self.tx_chip_setup(f, false, 0, true);
        radio_chip::tx_setup(&mut self.chip(n), &setup);
        self.tx.pa = pa;
        self.tx.fault = false;
        (self.tx.data, self.tx.slot, self.tx.dialled) = (Some(power), SOL950_SLOT, false);
        self.live.tx = TxLive {
            keyed: false,
            slot: SOL950_SLOT,
            mic: 0,
            refused: None,
            dtmf_sent: self.live.tx.dtmf_sent,
            voice: self.live.tx.voice,
            ptt_stuck: self.live.tx.ptt_stuck,
        };
        let mut st = Steps::default();
        if let Err(why) = self.tx.seq.key(&req, &mut self.path, now_ms() as u32, &mut st) {
            self.unkeyed();
            self.sol950_restore_rx().await;
            self.refuse(SOL950_SLOT, why);
            crate::chan::SOL950_TX_DONE.signal(false);
            crate::chan::TERM_WAKE.signal(());
            return;
        }
        self.run(&st).await;
        if self.tx.fault {
            let mut st = Steps::default();
            self.tx.seq.safe(&mut self.path, now_ms() as u32, &mut st);
            self.run(&st).await;
            self.unkeyed();
            self.sol950_restore_rx().await;
            self.refuse(SOL950_SLOT, Refusal::Locked);
            crate::chan::SOL950_TX_DONE.signal(false);
            crate::chan::TERM_WAKE.signal(());
            return;
        }
        self.tx.chip = Some(n);
        self.live.tx.keyed = true;
        if let Some(r) = self.sol950.as_mut() {
            r.tx_active = true;
        }
        let tones = ferrite_core::sol950::usol950::encode_tones(frame);
        self.sol950_live_start(n, tones);
        self.sol950_poll();
    }

    /// Finish or abort a live SOL950 burst, unkey cleanly, and return the RF path to SI4732 USB RX.
    pub(super) async fn sol950_tx_finish(&mut self, completed: bool) {
        self.afsk_bk_stop();
        let now = now_ms() as u32;
        let mut cfg = self.tx_cfg();
        (cfg.end_tone, cfg.tail, cfg.cw_id) = (false, false, false);
        let mut st = Steps::default();
        self.tx.seq.release(&cfg, &mut self.path, now, &mut st);
        self.run(&st).await;
        self.tx.data = None;
        self.unkeyed();
        if let Some(r) = self.sol950.as_mut() {
            r.tx_active = false;
        }
        self.sol950_restore_rx().await;
        self.sol950_poll();
        crate::chan::SOL950_TX_DONE.signal(completed);
        crate::chan::TERM_WAKE.signal(());
        log(if completed { b"sol950 tx done" } else { b"sol950 tx aborted" }, 0);
    }

    async fn sol950_restore_rx(&mut self) {
        if self.sol950.is_none() {
            return;
        }
        for n in 0..2u8 {
            self.tune_chip(n, None).await;
        }
        let mut st = Steps::default();
        let _ = self.path.rx(Rx::Si, Cause::HfReceiver, now_ms() as u32, &mut st);
        self.run(&st).await;
        let _ = self.si.mute(false, &mut BusyDelay);
    }
}
