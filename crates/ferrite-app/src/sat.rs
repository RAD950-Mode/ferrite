//! Satellite mode on the App (manual chapter 28a): the clock and observer it predicts from (SAT-5), the source
//! of satellites and passes ([`SatData`]: on-device prediction or Console's tracks, given by the platform with
//! [`App::sat_attach`]), the pass alarm and its toast (SAT-12, SAT-20), and what the Radio task is told while the
//! satellite screen is open (SAT-1, SAT-8, SAT-10, SAT-11, SAT-14). The screens are [`crate::screens::sat`].

use crate::App;
use ferrite_core::freq::Freq10;
use ferrite_core::msg::RadioCmd;
use ferrite_core::sat::data::{Prefs, SatData, State};
use ferrite_core::sat::look::{Look, Observer};
use ferrite_core::sat::pass::Pass;
use ferrite_core::sat::{Sat, Transponder};

/// The slot number satellite mode transmits as (the Radio task's key-up with the uplink chip, SAT-11).
pub const SAT_SLOT: u8 = 4;
/// SAT-12: the alarm's lead times in minutes, and Off; 5 minutes to start.
pub const LEADS: [u8; 5] = [1, 2, 5, 10, 0];
pub const LEAD_START: u8 = 2;
/// SAT-6: propagations a tick while predicting (the simulator's whole share; the radio adds time slices between
/// keys and redraws, [`App::sat_work`]).
pub const BUDGET: u32 = 150;
/// SAT-6: propagations between two looks at the clock in [`App::sat_work`] (well under a millisecond on the radio).
pub const CHUNK: u32 = 24;
/// SAT-10: the arming tone's length.
pub const ARM_MS: u32 = 2_000;
/// SAT-8: the uplink is retuned this often while keyed (seconds): every 5 s until the bench shows the BK4829
/// retunes cleanly while keyed, then every second.
pub const TX_RETUNE_S: u32 = 5;
/// The uplink's power names and meter bars on the keyed page, by TX-9 level (0 Low, the default, 1 Mid, 2 High:
/// the satellite's choice, SAT-22).
pub const POWER_NAMES: [&str; 3] = ["LOW", "MID", "HIGH"];
pub const POWER_BARS: [i32; 3] = [2, 3, 5];
/// SAT-12: the alarm's toast stays this long (OK on it opens the pass page).
pub const TOAST_MS: u32 = 8_000;

/// What the App keeps for satellite mode (about 60 bytes; the passes are the source's).
#[derive(Clone, Copy, Debug, Default)]
pub struct SatUi {
    /// UTC seconds and the App's clock (ms) when they were given; whether from GPS (else by hand or Console:
    /// SAT-5's "time ±1 min").
    pub utc: Option<(u32, u32)>,
    pub by_gps: bool,
    /// The observer: latitude and longitude in 1e-7 degree, height in metres (SAT-5).
    pub here: Option<(i32, i32, i32)>,
    /// SAT-12: the lead chosen last (an index into [`LEADS`]), and the alarm set: its lead in minutes, fired.
    pub lead: u8,
    pub alarm_lead: u8,
    pub fired: bool,
    /// When the alarm's toast went up (App clock, 0 none): OK on it opens the pass page.
    pub toast_at: u32,
    /// The pass the pass page shows.
    pub cur: Option<Pass>,
    /// SAT-10: the rise time of the pass whose arming tone went out (0 none), and when the arming tone ends.
    pub armed: u32,
    pub arm_until: u32,
    /// What the Radio task has: downlink, uplink, tone, half duplex, power; and the UTC second of the last retune.
    pub sent: (Freq10, Freq10, u16, bool, u8),
    pub sent_s: u32,
}

impl SatUi {
    pub const fn new() -> SatUi {
        SatUi { utc: None, by_gps: false, here: None, lead: LEAD_START, alarm_lead: 0, fired: false, toast_at: 0, cur: None, armed: 0, arm_until: 0, sent: (0, 0, 0, false, 0), sent_s: 0 }
    }
}

/// The corrected frequencies for a transponder at a look (SAT-8): the downlink as heard (with the trim), the
/// uplink to send; a side on 2 m stays where it is with Doppler on the 70 cm side only.
pub fn corrected(t: &Transponder, l: &Look, p: &Prefs) -> (Freq10, Freq10) {
    let uhf = |f: Freq10| f >= 30_000_000;
    let down = if p.uhf_only && !uhf(t.down) { t.down } else { (t.down as i32 + l.down_shift(t.down)) as Freq10 };
    let up = if t.up == 0 || p.uhf_only && !uhf(t.up) { t.up } else { (t.up as i32 + l.up_shift(t.up)) as Freq10 };
    ((down as i32 + p.trim as i32) as Freq10, up)
}

impl App {
    /// The platform's satellite data (the firmware's build choice, the simulator's sample).
    pub fn sat_attach(&mut self, d: &'static mut dyn SatData) {
        self.satd = Some(d);
    }

    /// The clock and the position, every tick (SAT-5): UTC seconds (None without a time source), whether that
    /// came from GPS, and the GPS fix or the fixed position (latitude, longitude in 1e-7°, height in m).
    pub fn sat_clock(&mut self, utc: Option<u32>, by_gps: bool, here: Option<(i32, i32, i32)>) {
        self.sat.utc = utc.map(|u| (u, self.now));
        self.sat.by_gps = by_gps;
        self.sat.here = here;
        if let (Some((lat, lon, _)), Some(h)) = (here, self.rad950d.as_deref_mut()) {
            h.chat.distress.update_gps_pos(lat, lon, by_gps);
        }
    }

    /// UTC seconds now.
    pub(crate) fn sat_now(&self) -> Option<u32> {
        self.sat.utc.map(|(u, at)| u.wrapping_add(self.now.wrapping_sub(at) / 1000))
    }

    pub(crate) fn sat_data(&self) -> &dyn SatData {
        match &self.satd {
            Some(d) => &**d,
            None => &ferrite_core::sat::data::NoData,
        }
    }

    pub(crate) fn sat_data_mut(&mut self) -> Option<&mut (dyn SatData + 'static)> {
        self.satd.as_deref_mut()
    }

    /// The satellite screen up, or under the screen up (its options).
    pub(crate) fn sat_open(&self) -> bool {
        use crate::Screen;
        matches!(self.screen, Screen::Sat(_)) || self.back.iter().flatten().any(|s| matches!(s, Screen::Sat(_)))
    }

    /// SAT-22: the pass list's minimum elevation, whole degrees.
    pub(crate) fn min_el(&self) -> u8 {
        crate::settings::SAT_MIN_EL[self.set.sat_min_el as usize % crate::settings::SAT_MIN_EL.len()]
    }

    /// SAT-6: more of the prediction under way, in chunks while `more()` says there is time (the radio's UI task
    /// gives it a few milliseconds a pass, at its top clock, between keys and redraws). True while still predicting.
    pub fn sat_work(&mut self, more: &mut dyn FnMut() -> bool) -> bool {
        while self.sat_busy() && more() {
            if let Some(d) = self.sat_data_mut() {
                d.work(CHUNK);
            }
        }
        self.sat_busy()
    }

    /// SAT-6: satellites held and the last prediction's propagations (the firmware's speed line).
    pub fn sat_stats(&self) -> (u8, u32) {
        (self.sat_data().sats(), self.sat_data().propagations())
    }

    /// PWR-26.1: predicting wants the build's speed (SAT-6).
    pub fn sat_busy(&self) -> bool {
        self.sat_data().state() == State::Busy
    }

    /// Every tick: predicting while the screen is open (SAT-6), the alarm (SAT-12), the radio's retunes (SAT-8).
    #[inline(never)]
    pub(crate) fn sat_tick(&mut self) {
        let now = self.sat_now();
        let obs = self.sat.here.map(|(a, o, h)| Observer::new(a, o, h));
        let open = self.sat_open();
        let min = self.min_el();
        if let Some(d) = self.sat_data_mut() {
            d.set_min_el(min);
            d.tick(now, obs, open, BUDGET);
        }
        let Some(now) = now else { return };
        self.alarm_tick(now);
        if open && self.radio.mode == crate::screens::activity::Mode::Sat {
            self.sat_retune(now, false);
        }
        if self.sat.toast_at != 0 && self.now.wrapping_sub(self.sat.toast_at) >= TOAST_MS {
            self.sat.toast_at = 0;
        }
    }

    /// SAT-12: the alarm's toast at its lead time, with the alert tone when Alert tones (0 7 5 3) is on.
    fn alarm_tick(&mut self, now: u32) {
        let Some(p) = self.sat_data().alarm() else {
            self.sat.alarm_lead = 0;
            return;
        };
        if self.sat.fired || now + self.sat.alarm_lead as u32 * 60 < p.aos {
            return;
        }
        self.sat.fired = true;
        if now >= p.aos {
            return; // risen already (the alarm set late, or the clock moved): no toast
        }
        let d = self.sat_data();
        let name = d.sat(p.sat).map(|s| s.name).unwrap_or_default();
        let mut head = ferrite_ui::text::Buf::<20>::new();
        head.set(&name[..name.iter().position(|&c| c == 0).unwrap_or(16)]);
        let mins = p.aos.saturating_sub(now).div_ceil(60);
        let _ = ufmt::uwrite!(head, " {} min", mins);
        let mut body = ferrite_ui::text::Buf::<28>::new();
        let _ = ufmt::uwrite!(body, "{}\u{b0}, rises {}", p.max_el, ferrite_core::sat::look::point8(p.aos_az as f32 * 2.0));
        self.show_toast(head.as_bytes(), body.as_bytes(), true);
        if let Some(t) = self.toast.as_mut() {
            t.until = self.now.wrapping_add(TOAST_MS);
        }
        self.sat.toast_at = self.now.max(1);
        if self.set.alert_tones {
            self.out.push(RadioCmd::Beep { hz: crate::ALERT_HZ, ms: crate::ALERT_MS });
        }
    }

    /// OK while the alarm's toast is up: the pass page (SAT-12). True when it took the key.
    pub(crate) fn sat_toast_ok(&mut self) -> bool {
        if self.sat.toast_at == 0 || self.toast.is_none() {
            return false;
        }
        self.sat.toast_at = 0;
        self.toast = None;
        if let Some(p) = self.sat_data().alarm() {
            self.sat.cur = Some(p);
            self.open(crate::screens::nav::Dest::Satellites);
            if let crate::Screen::Sat(s) = &mut self.screen {
                *s = crate::screens::sat::SatScr::pass_of(&p);
            }
        }
        true
    }

    /// SAT-20: the status bar's alarm mark ([`ferrite_ui::widgets::StatusBar::sat`]).
    pub(crate) fn sat_mark(&self) -> u16 {
        use ferrite_ui::widgets::{SAT_TEXT, SAT_UP};
        let (Some(p), Some(now)) = (self.sat_data().alarm(), self.sat_now()) else { return 0 };
        let m = if p.up(now) { SAT_UP } else { (p.aos.saturating_sub(now).div_ceil(60) as u16).clamp(1, SAT_UP - 1) };
        m | if (self.now / 2000) % 2 == 1 { SAT_TEXT } else { 0 }
    }

    /// SAT-12: set or clear the alarm on a pass with a lead (an index into [`LEADS`]); the lead becomes the next
    /// default.
    pub(crate) fn sat_set_alarm(&mut self, p: Pass, lead: u8) {
        let lead = lead.min(LEADS.len() as u8 - 1);
        self.sat.lead = lead;
        let mins = LEADS[lead as usize];
        let on = mins != 0;
        (self.sat.alarm_lead, self.sat.fired) = (mins, false);
        if let Some(d) = self.sat_data_mut() {
            d.set_alarm(on.then_some(p));
        }
    }

    /// The pass page's satellite, its transponder and its choices.
    pub(crate) fn sat_cur(&self) -> Option<(Pass, Sat, Transponder, Prefs)> {
        let p = self.sat.cur?;
        let d = self.sat_data();
        let s = d.sat(p.sat)?;
        let pr = d.prefs(p.sat);
        let t = *s.tp(pr.tp).or(s.tp(0))?;
        Some((p, s, t, pr))
    }

    /// SAT-8: the downlink retuned once a second; the uplink and tone sent with it (the Radio task uses them at
    /// the next key-up and, keyed, every [`TX_RETUNE_S`]). `force`: now, whatever changed (a key).
    #[inline(never)]
    pub(crate) fn sat_retune(&mut self, now: u32, force: bool) {
        let Some((p, _, t, pr)) = self.sat_cur() else { return };
        let keyed = self.radio.keyed == Some(SAT_SLOT);
        // SAT-10: the arming tone for its first seconds on the first key-up of a pass (its end retunes at once)
        let tone = if keyed && self.now.wrapping_sub(self.sat.arm_until) > u32::MAX / 2 { t.arm } else { t.tone };
        if !force && now == self.sat.sent_s && tone == self.sat.sent.2 {
            return;
        }
        let l = self.sat_data().look(p.sat, now).unwrap_or_default();
        let (down, up) = corrected(&t, &l, &pr);
        let was = self.sat.sent;
        self.sat.sent_s = now;
        if down != was.0 || force {
            self.out.push(RadioCmd::SatRx(down));
        }
        let up_due = !keyed || force || now.is_multiple_of(TX_RETUNE_S);
        let up = if up_due { up } else { was.1 };
        if up != was.1 || force {
            self.out.push(RadioCmd::SatUp(up));
        }
        if tone != was.2 || pr.hd != was.3 || pr.power != was.4 || force {
            self.out.push(RadioCmd::SatTone { tone, hd: pr.hd, power: pr.power });
        }
        self.sat.sent = (down, up, tone, pr.hd, pr.power);
    }

    /// The mode's start and stop (MODE-2): both chips taken with the downlink tuned, or given back (SAT-1).
    pub(crate) fn sat_start(&mut self) {
        self.sat.sent = (ferrite_core::msg::SAT_IDLE, 0, 0, false, 0);
        match self.sat_now() {
            Some(now) if self.sat.cur.is_some() => self.sat_retune(now, true),
            _ => self.out.push(RadioCmd::SatRx(ferrite_core::msg::SAT_IDLE)),
        }
    }

    /// PTT on the pass page (SAT-10, SAT-13): the uplink keyed with the access tone (the arming tone first on a
    /// pass's first key-up), or refused as any key-up.
    pub(crate) fn sat_ptt(&mut self) {
        use crate::screens::transmit;
        use ferrite_core::txplan::{self, Refusal};
        let Some((p, _, t, _)) = self.sat_cur() else { return };
        if !t.tx() {
            return transmit::refuse(self, SAT_SLOT, Refusal::ReceiveOnlyHere);
        }
        if self.qmenu.is_some() || self.remote_tx_refused() {
            return;
        }
        let up = self.sat.sent.1.max(t.up);
        let refusal = if self.radio.keyed.is_some() {
            Some(Refusal::InProgress)
        } else if !self.radio.tx_built {
            Some(Refusal::RxOnlyBuild)
        } else if self.radio.batt_cut {
            Some(Refusal::Battery)
        } else {
            txplan::check(up, self.set.tx_region(), self.set.extend_tx).err()
        };
        if let Some(r) = refusal {
            return transmit::refuse(self, SAT_SLOT, r);
        }
        if t.arm != 0 && self.sat.armed != p.aos {
            self.sat.armed = p.aos;
            self.sat.arm_until = self.now.wrapping_add(ARM_MS);
        } else {
            self.sat.arm_until = self.now;
        }
        self.radio.keyed = Some(SAT_SLOT);
        self.radio.t0 = self.now;
        if let Some(now) = self.sat_now() {
            self.sat_retune(now, true);
        }
        let region = txplan::Region::ALL.iter().position(|&r| r == self.set.tx_region()).unwrap_or(0) as u8;
        self.out.push(RadioCmd::TxPlan { tx: self.sat.sent.1, region, extend: self.set.extend_tx, confirmed: false });
        self.out.push(RadioCmd::Transmit { slot: SAT_SLOT, on: true });
    }
}

/// The simulator's and bench builds' satellites: data.ferritefw.dev's fixture (CelesTrak elements of Oct 31,
/// 2026, SatNOGS transponders; CC BY-SA 4.0), as Console would send them; SO-50's arming tone added (SAT-10).
pub fn sample() -> [Sat; 4] {
    use ferrite_core::sat::{tle, Kind, Transponder as T};
    let el = |a: &[u8], b: &[u8]| tle::parse(a, b).unwrap_or_default();
    let name = |n: &[u8]| {
        let mut b = [0u8; 16];
        b[..n.len()].copy_from_slice(n);
        b
    };
    let tp = |up: Freq10, down: Freq10, tone: u16, kind: Kind| T { up, down, tone, arm: 0, kind };
    let none = T::default();
    [
        Sat {
            id: 25_544,
            name: name(b"ISS"),
            el: el(
                b"1 25544U 98067A   26304.75703384  .00006360  00000+0  12291-3 0  9994",
                b"2 25544  51.6332 344.2755 0007028 332.5119  27.5497 15.49131714592920",
            ),
            tps: [
                tp(14_599_000, 43_780_000, 670, Kind::Voice),
                tp(14_449_000, 14_580_000, 0, Kind::Voice),
                tp(14_520_000, 14_580_000, 0, Kind::Voice),
                tp(14_582_500, 14_582_500, 0, Kind::Aprs),
                tp(43_782_500, 43_782_500, 0, Kind::Aprs),
            ],
            ntp: 5,
            ..Default::default()
        },
        Sat {
            id: 27_607,
            name: name(b"SO-50"),
            el: el(
                b"1 27607U 02058C   26304.82911449  .00000904  00000+0  12403-3 0  9995",
                b"2 27607  64.5527  66.1378 0068979 237.1936 122.2515 14.83296838284727",
            ),
            tps: [T { arm: 744, ..tp(14_585_000, 43_679_500, 670, Kind::Voice) }, none, none, none, none],
            ntp: 1,
            ..Default::default()
        },
        Sat {
            id: 40_908,
            name: name(b"LILACSAT-2"),
            el: el(
                b"1 40908U 15049K   26304.94554550  .00018681  00000+0  27069-3 0  9992",
                b"2 40908  97.4479 347.2901 0007925 107.5381 252.6740 15.55884743616983",
            ),
            tps: [tp(14_439_000, 14_439_000, 0, Kind::Aprs), none, none, none, none],
            ntp: 1,
            ..Default::default()
        },
        Sat {
            id: 63_213,
            name: name(b"TEVEL2-4"),
            el: el(
                b"1 63213U 25052D   26304.45667330  .00018502  00000+0  47709-3 0  9993",
                b"2 63213  97.3717 201.3703 0003020  36.1670 323.9779 15.39230540 91041",
            ),
            tps: [tp(14_597_000, 43_640_000, 0, Kind::Voice), none, none, none, none],
            ntp: 1,
            ..Default::default()
        },
    ]
}

/// The simulator's satellite time when its clock isn't set (headless runs): 2026-11-01 14:00 UTC, a day after
/// the sample's elements.
pub const SIM_T0: u32 = 1_793_541_600;

/// The declination at the sample's fix (Ottawa, about 13° W), 0.1° east positive (SAT-18).
pub const SIM_DECL: i16 = -130;

/// The sample in RAM, its choices with it (the App's tests; the simulator's `--sats` fixture), predicted on the
/// host as on the radio (SAT-6). The simulator itself keeps its satellites in its store, as the radio does.
pub struct SimSats(pub [Sat; 4], core::cell::Cell<[Option<Prefs>; 4]>);

impl SimSats {
    pub fn new(list: [Sat; 4]) -> SimSats {
        SimSats(list, core::cell::Cell::new([None; 4]))
    }
}

impl ferrite_core::sat::SatSource for SimSats {
    fn count(&self) -> u8 {
        4
    }
    fn get(&self, i: u8) -> Option<Sat> {
        self.0.get(i as usize).copied()
    }
    fn declination(&self) -> Option<i16> {
        Some(SIM_DECL)
    }
    fn prefs(&self, i: u8) -> Option<Prefs> {
        self.1.get().get(i as usize).copied().flatten()
    }
    fn set_prefs(&self, i: u8, p: Prefs) {
        let mut a = self.1.get();
        if let Some(x) = a.get_mut(i as usize) {
            *x = Some(p);
        }
        self.1.set(a);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SAT-8: both sides corrected, or the 70 cm side only; the trim on the downlink.
    #[test]
    fn doppler_rules() {
        let so50 = Transponder { up: 14_585_000, down: 43_679_500, tone: 670, arm: 744, kind: ferrite_core::sat::Kind::Voice };
        let l = Look { rate: -6.85, ..Default::default() }; // coming closer
        let both = Prefs::default();
        assert_eq!(corrected(&so50, &l, &both), (43_680_498, 14_584_667));
        // SAT-8: the 70 cm side only leaves the 2 m uplink alone; the trim moves the downlink
        let uhf = Prefs { uhf_only: true, trim: 20, ..Default::default() };
        assert_eq!(corrected(&so50, &l, &uhf), (43_680_518, 14_585_000));
        // V/U: the 2 m downlink stays, the 70 cm uplink moves
        let ao91 = Transponder { up: 43_525_000, down: 14_596_000, ..so50 };
        let (d, u) = corrected(&ao91, &l, &uhf);
        assert_eq!(d, 14_596_020);
        assert!(u < 43_525_000);
    }
}
