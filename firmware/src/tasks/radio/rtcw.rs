//! RTCW ("Radtel CW") radio-task path: SI4732 narrow-filter USB receive (`500 Hz` / `1.0 kHz`)
//! and semi-break-in BK4829 CW transmit with smooth `DAC2` PA envelope ramping.

use super::*;
use embassy_time::Timer;
use ferrite_core::freq::Freq10;
use ferrite_core::msg::{BroadcastCmd, ModeLive};
use ferrite_core::path::{Cause, Rx, Sel, Steps};
#[cfg(feature = "tx")]
use ferrite_core::txseq::Req;
#[cfg(feature = "tx")]
use ferrite_radio::chip as radio_chip;
use si4732::Sideband;

pub(super) const RTCW_SLOT: u8 = 5;
const SI_FRONT_DB: i16 = 20;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct RtcwRun {
    pub dial: Freq10,
    pub rx_corr_hz: i16,
    pub bw_500hz: bool,
    pub rx_ready: bool,
    pub tx_active: bool,
    pub pa_target: u8,
    pub pa_cur: u8,
}

impl Radio {
    /// Enter, retune, or exit RTCW mode on the SI4732 narrow USB receiver.
    pub(super) async fn rtcw_rx(&mut self, dial: Freq10, rx_corr_hz: i16, bw_500hz: bool, on: bool) {
        self.seek_cancel();
        if !on {
            #[cfg(feature = "tx")]
            if self.rtcw.is_some_and(|r| r.tx_active) {
                self.rtcw_tx_stop().await;
            }
            if self.rtcw.take().is_some() {
                let _ = broadcast(&mut self.si, BroadcastCmd::Off, self.si_opts).await;
                self.tuned = [None; 2];
                self.live.mode = ModeLive::Off;
                let f = self.slots[self.active as usize].freq;
                let mut s = Steps::default();
                let _ = self.path.rx(Rx::Bk(Sel::rx(f)), Cause::HfReceiver, now_ms() as u32, &mut s);
                self.run(&s).await;
                log(b"rtcw off", 0);
            }
            return;
        }
        for n in 0..2u8 {
            self.tune_chip(n, None).await;
        }
        let mut s = Steps::default();
        let _ = self.path.rx(Rx::Si, Cause::HfReceiver, now_ms() as u32, &mut s);
        self.run(&s).await;
        let ok = self.si_ssb_rtcw(dial, rx_corr_hz, bw_500hz).await;
        if ok.is_ok() {
            speaker(true);
        }
        self.rtcw = Some(RtcwRun {
            dial,
            rx_corr_hz,
            bw_500hz,
            rx_ready: ok.is_ok(),
            tx_active: false,
            pa_target: 0,
            pa_cur: 0,
        });
        self.rtcw_poll();
    }

    async fn si_ssb_rtcw(&mut self, dial: Freq10, rx_corr_hz: i16, bw_500hz: bool) -> Result<(), si4732::Error> {
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
        // SI4732 SSB filter: 4 = 0.5 kHz (500 Hz CW), 3 = 1.0 kHz
        let cfg = si4732::SsbConfig {
            audio_bw: if bw_500hz { 4 } else { 3 },
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

    pub(super) fn rtcw_poll(&mut self) {
        let Some(r) = self.rtcw else { return };
        let rssi = if r.rx_ready && !r.tx_active {
            match self.si.rsq(&mut BusyDelay) {
                Ok(q) => q.rssi as i16 - 107 - SI_FRONT_DB,
                Err(_) => -127,
            }
        } else {
            -127
        };
        self.live.mode = ModeLive::Rtcw {
            rx_ready: r.rx_ready,
            tx_active: r.tx_active,
            rssi,
            loading: 255,
        };
    }

    /// Step the smooth `DAC2` CW envelope ramp while semi-break-in TX is latched.
    #[cfg(feature = "tx")]
    pub(super) fn rtcw_step_key(&mut self) {
        let Some(r) = self.rtcw.as_mut() else { return };
        if !r.tx_active {
            return;
        }
        let key_down = crate::chan::RTCW_KEY_DOWN.load(core::sync::atomic::Ordering::Relaxed);
        let goal = if key_down { r.pa_target } else { 0 };
        if r.pa_cur != goal {
            let step = (r.pa_target / 4).max(1);
            r.pa_cur = if r.pa_cur < goal {
                r.pa_cur.saturating_add(step).min(goal)
            } else {
                r.pa_cur.saturating_sub(step).max(goal)
            };
            if r.pa_cur == 0 {
                crate::hal::pa::zero();
            } else {
                crate::hal::pa::set(r.pa_cur);
            }
        }
    }
}

#[cfg(feature = "tx")]
impl Radio {
    /// Semi-break-in CW TX latch (`on = true`) or release back to SI4732 RX (`on = false`).
    pub(super) async fn rtcw_tx(&mut self, freq: Freq10, power: u8, on: bool) {
        if !on {
            self.rtcw_tx_stop().await;
            return;
        }
        let Some(run) = self.rtcw else { return };
        if run.tx_active {
            return;
        }
        let now = now_ms() as u32;
        if let Some(c) = crate::chan::TX_CAL.try_take() {
            (self.tx.cal, self.tx.gauge) = c;
        }
        let Some((plan_f, region, extend, _)) = self.tx.plan.take() else {
            return;
        };
        let f = if plan_f != 0 { plan_f } else { freq };
        if !self.ptt_seen_released() {
            return;
        }
        let power = power.min(2);
        let n = Sel::tx(f).chip();
        if !self.has(n) {
            return;
        }
        let Some(pa) = self.tx.cal.and_then(|c| c.dac(f, power)) else {
            return;
        };
        let _ = self.si.mute(true, &mut BusyDelay);
        speaker(false);
        let mut st = Steps::default();
        if self.path.rx(Rx::Bk(Sel::HfRx), Cause::HfReceiver, now, &mut st).is_err() {
            let _ = self.si.mute(false, &mut BusyDelay);
            speaker(true);
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
        if self.tx.seq.check(&req, &self.path, now_ms() as u32).is_err() {
            self.rtcw_restore_rx().await;
            return;
        }

        crate::chan::RF_STATE.fetch_or(crate::chan::RF_BUSY_KEYED, core::sync::atomic::Ordering::Relaxed);
        crate::tasks::gps::pb10_low_now();
        self.ldo_on(n);
        self.tuned[n as usize] = Some((RTCW_SLOT, f));
        self.chip(n).dtmf_rx(false);
        let setup = self.tx_chip_setup(f, false, 0, true);
        radio_chip::tx_setup(&mut self.chip(n), &setup);
        self.tx.pa = 0; // Start at 0 V; `rtcw_step_key()` ramps DAC2 smoothly on each dit/dah!
        self.tx.fault = false;
        (self.tx.data, self.tx.slot, self.tx.dialled) = (Some(power), RTCW_SLOT, false);
        let mut st = Steps::default();
        if self.tx.seq.key(&req, &mut self.path, now_ms() as u32, &mut st).is_err() {
            self.unkeyed();
            self.rtcw_restore_rx().await;
            return;
        }
        self.run(&st).await;
        if self.tx.fault {
            let mut st = Steps::default();
            self.tx.seq.safe(&mut self.path, now_ms() as u32, &mut st);
            self.run(&st).await;
            self.unkeyed();
            self.rtcw_restore_rx().await;
            return;
        }
        self.tx.chip = Some(n);
        self.live.tx.keyed = true;
        crate::hal::pa::zero();
        if let Some(r) = self.rtcw.as_mut() {
            r.tx_active = true;
            r.pa_target = pa;
            r.pa_cur = 0;
        }
        self.rtcw_poll();
    }

    pub(super) async fn rtcw_tx_stop(&mut self) {
        let Some(run) = self.rtcw else { return };
        if !run.tx_active {
            return;
        }
        crate::hal::pa::zero();
        let now = now_ms() as u32;
        let mut cfg = self.tx_cfg();
        (cfg.end_tone, cfg.tail, cfg.cw_id) = (false, false, false);
        let mut st = Steps::default();
        self.tx.seq.release(&cfg, &mut self.path, now, &mut st);
        self.run(&st).await;
        self.tx.data = None;
        self.unkeyed();
        if let Some(r) = self.rtcw.as_mut() {
            r.tx_active = false;
            r.pa_target = 0;
            r.pa_cur = 0;
        }
        self.rtcw_restore_rx().await;
        self.rtcw_poll();
    }

    async fn rtcw_restore_rx(&mut self) {
        let Some(run) = self.rtcw else { return };
        let mut s = Steps::default();
        let _ = self.path.rx(Rx::Si, Cause::HfReceiver, now_ms() as u32, &mut s);
        self.run(&s).await;
        let _ = self.si_ssb_rtcw(run.dial, run.rx_corr_hz, run.bw_500hz).await;
        speaker(true);
    }
}
