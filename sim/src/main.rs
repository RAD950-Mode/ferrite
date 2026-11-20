//! Renders Ferrite's screens with the firmware's own drawing code into `target/sim/*.png` (240 x 320, as on the
//! radio), from sample data. `cargo sim`; `cargo sim --scale 2` for bigger images.
//!
//! `cargo simw` opens the interactive simulator instead ([`window`]): a window, PC keys as radio keys, a fake
//! radio, and tools to check the drawing against the look review's mock-ups.

use ferrite_app::App;
use ferrite_core::keys::Key;
use ferrite_core::VERSION;
use ferrite_ui::boot::{self, BootInfo, Check, State};
use ferrite_ui::fb::FrameBuffer;
use ferrite_ui::panel::{H, W};
use std::path::Path;

use ferrite_sim::fake;

mod shots_aprs;
mod shots_d;
mod shots_sat;
mod shots_rad950;
mod termport;
mod window;

fn save(fb: &FrameBuffer, name: &str, scale: u32) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/sim");
    save_fb(fb, &dir.join(format!("{name}.png")), scale);
}

/// The frame buffer as a PNG, scaled up by a whole number.
pub(crate) fn save_fb(fb: &FrameBuffer, path: &Path, scale: u32) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    let rgb = fb.rgb();
    let (w, h) = (W as u32 * scale, H as u32 * scale);
    let mut out = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let i = (((y / scale) * W as u32 + x / scale) * 3) as usize;
            out.extend_from_slice(&rgb[i..i + 3]);
        }
    }
    let mut enc = png::Encoder::new(std::fs::File::create(path).unwrap(), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.write_header().unwrap().write_image_data(&out).unwrap();
    println!("{}", path.display());
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    ferrite_sim::parts::from_args(&args); // decision 335: --parts DIR, --no-parts
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1));
    if let Some(secs) = arg("--term-headless").and_then(|s| s.parse::<u64>().ok()) {
        // No window: the radio and its cable only, for scripted terminal checks (`--term-tcp PORT` for TCP).
        let tcp = arg("--term-tcp").and_then(|s| s.parse::<u16>().ok());
        // `--flash FILE`: the flash as FILE holds it ([`ferrite_sim::simcore::FLASH_IMAGE`]'s bytes, as the window keeps
        // them), not the sample: Ferrite Console's end-to-end test starts a radio whose data is in layout 2 (UPD-16)
        // and finishes the update over the link (LINK-59 – LINK-62).
        let mut sim = match arg("--flash") {
            Some(f) => {
                let img = std::fs::read(f).expect("the flash image");
                ferrite_sim::simcore::SimCore::with_flash(1, false, ferrite_sim::simcore::Stored::Image(img))
            }
            None => ferrite_sim::simcore::SimCore::new(1, false),
        };
        // `--cut-restart`: the next restart after a raw write is a power cut (decision 339; Console's e2e test).
        if args.iter().any(|a| a == "--cut-restart") {
            sim.cut_next_restart();
        }
        // `--no-ssb-patch`: a radio without the SSB patch (HF-20, SELF-6, INST-14; the UI tests' sim_args).
        if args.iter().any(|a| a == "--no-ssb-patch") {
            sim.device_mut().selftest.ssb_patch = false;
        }
        // `--sats N`: N satellites, for the pass list's prediction to take a few seconds (SAT-6's UI test).
        if let Some(n) = arg("--sats").and_then(|s| s.parse::<usize>().ok()) {
            sim.many_sats(n);
        }
        let mut cable = match tcp {
            Some(p) => termport::Port::tcp(p),
            None => termport::Port::pty(),
        }
        .expect("a cable");
        println!("{}", cable.describe());
        // `--bt-tcp PORT`: Bluetooth's serial side on a second TCP port (a phone's link or terminal).
        let mut bt = arg("--bt-tcp").and_then(|s| s.parse::<u16>().ok()).map(|p| termport::Port::tcp_bt(p).expect("a Bluetooth port"));
        if let Some(b) = &bt {
            println!("{}", b.describe());
        }
        let start = std::time::Instant::now();
        while start.elapsed().as_secs() < secs {
            sim.step(start.elapsed().as_millis() as u32);
            cable.pump(&mut sim);
            if let Some(b) = &mut bt {
                b.pump(&mut sim);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        return;
    }
    if args.iter().any(|a| a == "--window") {
        // The window: a whole scale (default 1); resizing snaps to whole scales.
        let scale = arg("--scale").and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
        let bus = arg("--bus-mbps").and_then(|s| s.parse().ok());
        let tcp = arg("--term-tcp").and_then(|s| s.parse::<u16>().ok());
        window::run(scale, bus, args.iter().any(|a| a == "--owner-card"), tcp, args.iter().any(|a| a == "--fresh"));
        return;
    }
    let scale = arg("--scale").and_then(|s| s.parse().ok()).unwrap_or(1);
    if args.iter().any(|a| a == "--rad950") {
        shots_rad950::run(scale);
        return;
    }

    let checks = [
        Check { label: "MCU", value: "AT32F403A", state: State::Ok },
        Check { label: "SPI flash", value: "4 MB", state: State::Ok },
        Check { label: "BK4829 VHF", value: "0x4829", state: State::Ok },
        Check { label: "BK4829 UHF", value: "0x4829", state: State::Ok },
        Check { label: "SI4732", value: "not tested", state: State::Off },
        Check { label: "Calibration", value: "", state: State::Pending },
    ];
    let info = BootInfo { version: VERSION, callsign: Some(b"VA7FER"), line: Some(b"VA7FER"), owner: None, checks: &checks, note: None };
    let mut fb = FrameBuffer::new();
    boot::draw(&mut fb, &info);
    save(&fb, "boot", scale);
    let mut failed = checks;
    failed[1] = Check { label: "SPI flash", value: "no answer", state: State::Fail };
    boot::draw(&mut fb, &BootInfo { checks: &failed, ..info });
    save(&fb, "boot-failed", scale);
    // The owner card (MENU 0 7 3 3, BootC mock-up): the sample settings with the sample card on them.
    let mut set = ferrite_app::sample::settings();
    ferrite_app::sample::owner_card(&mut set);
    let mut running = checks;
    running[0..3].iter_mut().for_each(|c| c.state = State::Ok);
    (running[3].state, running[4].state) = (State::Pending, State::Pending);
    boot::draw(&mut fb, &BootInfo { callsign: set.boot_callsign(), line: set.boot_line(), owner: set.boot_owner(), checks: &running, ..info });
    save(&fb, "boot-owner", scale);

    // The app, as in the window: keys pressed at simulated times, the fake radio stepped to match.
    let store = fake::store();
    let mut radio = fake::FakeRadio::new();
    let mut dev = fake::device();
    dev.clock_min = Some(14 * 60 + 32);
    dev.utc = false;
    let mut app = fake::app();
    app.settings_mut().backlight = 0; // shots: the screen never goes dark (lights tested in ferrite-app)
    fake::sample(&mut app, &store, &mut radio);
    let mut t = 0u32;
    // (returns whether A's squelch is open, for a shot of A receiving)
    let mut run = |app: &mut App, fb: &mut FrameBuffer, until: u32, t: &mut u32| -> bool {
        while *t <= until {
            while let Some(c) = app.pop_cmd() {
                radio.cmd(c);
            }
            while let Some(w) = app.pop_write() {
                store.apply(w);
            }
            radio.step(*t as f32 / 1000.0, app.active());
            app.tick(*t, &radio.live, &store);
            app.render(fb, &radio.live, &dev, &store);
            *t += 20;
        }
        radio.live.slots[0].squelch_open
    };
    let press = |app: &mut App, key: Key, hold: u32, t: &mut u32| {
        app.key_down(key, *t, &store);
        *t += hold;
        app.tick(*t, &Default::default(), &store); // long presses fire while held
        app.key_up(key, *t, &store);
    };
    run(&mut app, &mut fb, 7000, &mut t); // slot A receiving (6-9 s of every 10)
    save(&fb, "home", scale);
    // The whole radio, as the window shows it: the front panel around the screen, SIDE1 held, A receiving.
    let face = ferrite_sim::face::Face::new(2);
    let mut px = vec![0u32; face.w * face.h];
    let side1 = face.find(Key::Side(1)).into_iter().collect::<Vec<_>>();
    face.draw(&mut px, &side1, 0, true, false, 2);
    face.put_screen(&mut px, 2, |x, y| ferrite_sim::simcore::rgb565_to_rgb(fb.at(x, y)));
    ferrite_sim::save_rgb(&px, face.w, face.h, &Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/sim/radio.png"));
    press(&mut app, Key::Slot, 800, &mut t); // hold SLOT: edit in place
    press(&mut app, Key::Right, 100, &mut t);
    press(&mut app, Key::Right, 100, &mut t);
    run(&mut app, &mut fb, t, &mut t);
    save(&fb, "home-edit", scale);
    press(&mut app, Key::Exit, 100, &mut t);
    press(&mut app, Key::Menu, 100, &mut t); // quick settings
    run(&mut app, &mut fb, t, &mut t);
    save(&fb, "menu-quick", scale);
    press(&mut app, Key::Digit(0), 100, &mut t); // advanced
    run(&mut app, &mut fb, t, &mut t);
    save(&fb, "menu-top", scale);
    press(&mut app, Key::Exit, 100, &mut t);
    press(&mut app, Key::Exit, 100, &mut t);
    press(&mut app, Key::Hash, 100, &mut t); // Status page
    run(&mut app, &mut fb, t, &mut t);
    save(&fb, "status", scale);
    press(&mut app, Key::Exit, 100, &mut t);
    app.notify_message(b"VE3KPW-9", b"On my way");
    run(&mut app, &mut fb, t + 100, &mut t);
    save(&fb, "toast", scale);
    run(&mut app, &mut fb, t + 3500, &mut t); // the toast times out

    // Group A, entry and editing: each screen as it first shows, with a little typed.
    let tap = |app: &mut App, t: &mut u32, keys: &[Key]| {
        for &k in keys {
            press(app, k, 100, t);
            *t += 150;
        }
    };
    use Key::{Digit as D, Down, Exit, Menu, Right, Slot, Star, VfoMem};
    press(&mut app, Exit, 800, &mut t);
    tap(&mut app, &mut t, &[D(1), D(4), D(6), D(9), D(4)]); // NAV-4: frequency entry
    run(&mut app, &mut fb, t, &mut t);
    save(&fb, "entry-freq", scale);
    press(&mut app, Exit, 800, &mut t);
    tap(&mut app, &mut t, &[D(6), D(5), D(0), D(0), D(0), D(0)]); // VFO-21: 650 MHz is refused, the entry stays
    run(&mut app, &mut fb, t, &mut t);
    save(&fb, "entry-refused", scale);
    press(&mut app, Exit, 800, &mut t);
    run(&mut app, &mut fb, t + 3500, &mut t);
    app.key_down(D(5), t, &store); // HOME-8: hold 5 opens tones at the long press, a popup names it
    run(&mut app, &mut fb, t + 800, &mut t);
    app.key_up(D(5), t, &store);
    run(&mut app, &mut fb, t + 100, &mut t);
    save(&fb, "hold-toast", scale);
    tap(&mut app, &mut t, &[Exit]);
    run(&mut app, &mut fb, t + 3000, &mut t); // the toast times out
    app.key_down(D(2), t, &store); // hold 2 on home: TX power steps, the popup shows the new level
    run(&mut app, &mut fb, t + 800, &mut t);
    app.key_up(D(2), t, &store);
    run(&mut app, &mut fb, t + 100, &mut t);
    save(&fb, "hold-popup-power", scale);
    app.key_down(D(2), t, &store); // and back to Low (2 more steps), then let the popup go
    run(&mut app, &mut fb, t + 800, &mut t);
    app.key_up(D(2), t, &store);
    app.key_down(D(2), t, &store);
    run(&mut app, &mut fb, t + 800, &mut t);
    app.key_up(D(2), t, &store);
    run(&mut app, &mut fb, t + 1500, &mut t);
    save(&fb, "hold-popup-gone", scale);
    app.open(ferrite_app::screens::nav::Dest::ShortcutMap); // the reference page, MENU 0 9 1 3
    run(&mut app, &mut fb, t + 100, &mut t);
    save(&fb, "shortcut-map", scale);
    tap(&mut app, &mut t, &[Exit]);
    tap(&mut app, &mut t, &[Menu, Menu]); // MENU 1, OK: the squelch editor (MenuEdit)
    run(&mut app, &mut fb, t + 3000, &mut t);
    save(&fb, "value-squelch", scale);
    tap(&mut app, &mut t, &[Exit, D(5), Menu]); // MENU 5, OK: offset as a list
    run(&mut app, &mut fb, t, &mut t);
    save(&fb, "value-offset", scale);
    tap(&mut app, &mut t, &[Exit, D(4), Menu]); // MENU 4, OK: the tone picker
    run(&mut app, &mut fb, t, &mut t);
    save(&fb, "tones", scale);
    press(&mut app, Exit, 800, &mut t);
    press(&mut app, VfoMem, 800, &mut t); // hold V/M: save as channel
    run(&mut app, &mut fb, t, &mut t);
    save(&fb, "save-channel", scale);
    tap(&mut app, &mut t, &[Down, Down, Menu, D(4), D(4)]); // its name: text entry, "H"
    t += 1100;
    tap(&mut app, &mut t, &[Star, D(4), D(4), D(4), D(5), D(5), D(5)]);
    run(&mut app, &mut fb, t, &mut t);
    save(&fb, "text", scale);
    press(&mut app, Exit, 800, &mut t);
    press(&mut app, Menu, 800, &mut t); // MENU 0 2 2: add repeater
    tap(&mut app, &mut t, &[D(2), D(2)]);
    run(&mut app, &mut fb, t, &mut t);
    save(&fb, "add-repeater", scale);
    press(&mut app, Exit, 800, &mut t);
    tap(&mut app, &mut t, &[Slot, D(1), D(5)]); // B is on a channel: channel entry
    run(&mut app, &mut fb, t, &mut t);
    save(&fb, "entry-channel", scale);
    press(&mut app, Exit, 800, &mut t);
    press(&mut app, Slot, 800, &mut t); // hold SLOT on a channel: the channel editor (HomeEditCh)
    tap(&mut app, &mut t, &[Down, Down, Down, Right, Right]);
    run(&mut app, &mut fb, t, &mut t);
    save(&fb, "home-edit-ch", scale);
    tap(&mut app, &mut t, &[Exit, Slot, Slot]);
    press(&mut app, Menu, 800, &mut t); // MENU 0 9 7 2: reset asks first
    tap(&mut app, &mut t, &[D(9), D(8), D(2)]);
    run(&mut app, &mut fb, t, &mut t);
    save(&fb, "confirm", scale);
    press(&mut app, Exit, 800, &mut t);
    // Setup, the PIN and the Extend TX warning are group D's ([`shots_d`]).

    // Radio activity (group B). Each from home, the fake radio answering.
    macro_rules! shot {
        ($ms:expr, $name:expr) => {
            run(&mut app, &mut fb, t + $ms, &mut t);
            save(&fb, $name, scale);
        };
    }
    let home = |app: &mut App, t: &mut u32| press(app, Key::Exit, 800, t);
    // Transmit: PTT held on A (145.920 after the -600k offset), 2 s in.
    app.key_down(Key::PTT, t, &store);
    shot!(2800, "transmit");
    app.key_up(Key::PTT, t, &store);
    // Refused: 420.500 is outside 70 cm in Canada (the TxRefused mock-up).
    app.set_vfo(0, 42_050_000);
    app.key_down(Key::PTT, t, &store);
    app.key_up(Key::PTT, t + 200, &store);
    shot!(300, "tx-refused");
    home(&mut app, &mut t);
    // HOME-4: the chip row in fixed slots. Card A in four states side by side (chips-stable.png): tone off,
    // simplex, then DCS, reversed and High. Mode, tone, shift and power keep their places.
    {
        use ferrite_app::slot::{SlotCfg, DCS_FLAG};
        let base = *app.slot(0).cfg();
        let states = [
            base,
            SlotCfg { tx_tone: 0, ..base },
            SlotCfg { tx_tone: 0, offset: 0, ..base },
            SlotCfg { tx_tone: DCS_FLAG | 0o023, reverse: true, power: 2, ..base },
        ];
        let (cw, ch) = (W as usize, 148usize);
        let mut strip = vec![0u8; cw * 4 * ch * 3];
        app.set_vfo(0, 14_652_000);
        for (k, c) in states.iter().enumerate() {
            app.set_vfo_cfg(0, *c);
            run(&mut app, &mut fb, t + 300, &mut t);
            let rgb = fb.rgb();
            for y in 0..ch {
                for x in 0..cw {
                    let i = (y * cw + x) * 3;
                    let o = (y * cw * 4 + k * cw + x) * 3;
                    strip[o..o + 3].copy_from_slice(&rgb[i..i + 3]);
                }
            }
        }
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/sim/chips-stable.png");
        let mut enc = png::Encoder::new(std::fs::File::create(&path).unwrap(), (cw * 4) as u32, ch as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.write_header().unwrap().write_image_data(&strip).unwrap();
        println!("{}", path.display());
        app.set_vfo_cfg(0, base);
    }
    // Split: RX 446.000, TX 146.520 (cross-band, same relay state: allowed): SPLIT in its slot, the TX frequency
    // in the header.
    let split = ferrite_app::slot::SlotCfg { split: true, offset: 14_652_000, ..ferrite_app::slot::DEFAULT_CFG };
    app.set_vfo_cfg(0, split);
    app.set_vfo(0, 44_600_000);
    shot!(300, "home-split");
    // TX-37: RX retuned to 50.125 under the split: TX would need the other relay state, so PTT refuses.
    app.set_vfo(0, 5_012_500);
    app.key_down(Key::PTT, t, &store);
    app.key_up(Key::PTT, t + 200, &store);
    shot!(300, "tx-refused-relay");
    home(&mut app, &mut t);
    // Emergency: 406.025 (EPIRB) with Extend TX on; the first PTT only prompts.
    app.set_region(Some(ferrite_core::txplan::Region::Canada), true);
    app.set_vfo_cfg(0, ferrite_app::slot::DEFAULT_CFG); // simplex: the offset would move it off 406.025
    app.set_vfo(0, 40_602_500);
    app.key_down(Key::PTT, t, &store);
    app.key_up(Key::PTT, t + 200, &store);
    shot!(1500, "tx-emergency");
    home(&mut app, &mut t);
    app.set_region(Some(ferrite_core::txplan::Region::Canada), false);
    fake::sample_app(&mut app, &store);
    app.set_vfo(0, 14_652_000);
    run(&mut app, &mut fb, t, &mut t);
    // Monitor latched on A (SIDE1 tap): MON on its card (SQL-14); tapped again to end it.
    press(&mut app, Key::Side(1), 100, &mut t);
    shot!(600, "home-monitor");
    press(&mut app, Key::Side(1), 100, &mut t);
    run(&mut app, &mut fb, t, &mut t);
    // HOME-15.1 – HOME-15.3: \* cycles A Normal, Muted, Off. Muted while it receives (the fake's carrier on A is up
    // 6-9 s of every 10): "Muted" with the RX tag, large and small; then off, the active card and a small one.
    let rx_at = |t: u32| (t + 3_200).div_ceil(10_000) * 10_000 + 7_500;
    // (A on 446.000 meanwhile: C's airband scan holds the VHF chip)
    app.set_vfo(0, 44_600_000);
    press(&mut app, Key::Star, 100, &mut t);
    let until = rx_at(t);
    shot!(until - t, "home-rxmuted");
    press(&mut app, Key::Slot, 100, &mut t);
    // (watched, A shares its chip with B and holds it only for its turns: the first frame it reads open)
    run(&mut app, &mut fb, rx_at(t) - 1_500, &mut t);
    while !run(&mut app, &mut fb, t, &mut t) {}
    save(&fb, "home-rxmuted-small", scale);
    press(&mut app, Key::Slot, 100, &mut t);
    press(&mut app, Key::Slot, 100, &mut t); // A
    press(&mut app, Key::Star, 100, &mut t);
    shot!(3_500, "home-off-active");
    press(&mut app, Key::Slot, 100, &mut t); // B
    shot!(3_500, "home-off");
    press(&mut app, Key::Slot, 100, &mut t);
    press(&mut app, Key::Slot, 100, &mut t); // A
    press(&mut app, Key::Star, 100, &mut t); // on again
    app.set_vfo(0, 14_652_000);
    run(&mut app, &mut fb, t + 3_500, &mut t);
    // Scan: SIDE2 on A, 9 s in (a hit every 7 s).
    press(&mut app, Key::Side(2), 100, &mut t);
    shot!(9000, "scan");
    press(&mut app, Key::Side(2), 100, &mut t);
    run(&mut app, &mut fb, t, &mut t); // the radio takes the commands (16 queue at most)
    // Band scope: hold 1 with the waterfall row on, the trigger line at 3 (hold 3), the marker moved onto the
    // strong carrier; then SIDE1 listens to it.
    app.settings_mut().scope_wf = true;
    press(&mut app, Key::Digit(1), 800, &mut t);
    run(&mut app, &mut fb, t, &mut t);
    press(&mut app, Key::Digit(3), 800, &mut t);
    run(&mut app, &mut fb, t, &mut t);
    for _ in 0..13 {
        press(&mut app, Key::Right, 60, &mut t);
        run(&mut app, &mut fb, t, &mut t);
    }
    shot!(2500, "scope");
    press(&mut app, Key::Side(1), 100, &mut t);
    shot!(800, "scope-listen");
    press(&mut app, Key::Side(1), 100, &mut t);
    app.settings_mut().scope_wf = false;
    app.settings_mut().scope_line = 0;
    home(&mut app, &mut t);
    // Tone search: MENU 0 3 5, found after 2.5 s.
    app.open(ferrite_app::screens::nav::Dest::ToneSearch);
    shot!(1000, "tone-search");
    shot!(2500, "tone-found");
    home(&mut app, &mut t);
    // Frequency counter: MENU 0 3 3.
    app.open(ferrite_app::screens::nav::Dest::Counter);
    shot!(2500, "counter");
    home(&mut app, &mut t);
    // FM broadcast and HF mode: group D's shots.
    // The alert: group D's shot, as in the mock-up.
    // Squelch: SIDE1 held, the knob turned.
    app.key_down(Key::Side(1), t, &store);
    app.knob(1, t, &store);
    shot!(300, "squelch");
    app.knob(-1, t, &store);
    app.key_up(Key::Side(1), t, &store);
    run(&mut app, &mut fb, t + 100, &mut t);
    // The shortcut map: group A's shot.
    run(&mut app, &mut fb, t + 3500, &mut t);
    // Crossband: start page, then ready, then passing A -> B.
    app.open(ferrite_app::screens::nav::Dest::Crossband);
    shot!(300, "xb-start");
    press(&mut app, Key::Hash, 100, &mut t);
    shot!(14_000, "xb-ready");
    shot!(21_000, "xb-passing");
    home(&mut app, &mut t);
    // Home's other layouts (MENU 0 7 2).
    app.set_layout(1);
    shot!(1500, "home-scope");
    app.set_layout(2);
    shot!(20_000, "home-activity");
    app.set_layout(0);
    // Channel mode puts the name first (HOME-4): B (CH 012 Club repeater) active in each layout, PTT B, then a
    // 16-character name and a channel without one.
    press(&mut app, Key::Slot, 100, &mut t);
    shot!(300, "home-ch");
    app.set_layout(1);
    shot!(1500, "home-scope-ch");
    app.set_layout(2);
    shot!(1500, "home-activity-ch");
    app.set_layout(0);
    app.key_down(Key::Ptt(1), t, &store);
    shot!(1500, "transmit-ch");
    app.key_up(Key::Ptt(1), t, &store);
    home(&mut app, &mut t);
    let cfg = ferrite_app::slot::DEFAULT_CFG;
    app.set_channel_rec(1, ferrite_app::chan::channel(140, 1, 14_694_000, "Mount Seymour RP", cfg));
    shot!(300, "home-ch-long");
    app.set_channel_rec(1, ferrite_app::chan::channel(141, 1, 14_652_000, "", cfg));
    shot!(300, "home-ch-noname");
    fake::sample_app(&mut app, &store);
    press(&mut app, Key::Slot, 100, &mut t);
    press(&mut app, Key::Slot, 100, &mut t);
    run(&mut app, &mut fb, t, &mut t);
    // Group C, lists and data: each opened as its way in does, keys pressed, the page saved.
    use ferrite_app::screens::nav::Dest;
    type Shot<'a> = (&'a str, Dest, &'a [(Key, u32)]);
    let shots: &[Shot] = &[
        ("event-log", Dest::EventLog, &[(Key::Down, 100)]),
        ("event-detail", Dest::EventLog, &[(Key::Down, 100), (Key::Down, 100), (Key::Menu, 100)]),
        ("event-detail-pager", Dest::EventLog, &[(Key::Down, 100), (Key::Menu, 100)]),
        ("event-log-options", Dest::EventLog, &[(Key::Menu, 800)]),
        ("aprs-heard", Dest::AprsHeard, &[]),
        ("aprs-station", Dest::AprsHeard, &[(Key::Menu, 100)]),
        ("aprs-station-waypoint", Dest::AprsHeard, &[(Key::Menu, 100), (Key::Hash, 100)]),
        ("aprs-messages", Dest::AprsMessages, &[]),
        ("aprs-message", Dest::AprsMessages, &[(Key::Menu, 100)]),
        // Predictive compose (TEXT-2 to TEXT-6): the quick list on an empty message, a word with its candidates
        // ("home" after "good" and its short form GD), a short form highlighted (received, QSL).
        ("aprs-compose-quick", Dest::AprsMessages, &[(Key::Menu, 100), (Key::Menu, 100), (Key::Down, 100), (Key::Down, 100)]),
        ("aprs-compose", Dest::AprsMessages, &[(Key::Menu, 100), (Key::Menu, 100), (Key::Digit(6), 100), (Key::Digit(6), 100), (Key::Hash, 100), (Key::Digit(6), 100), (Key::Digit(9), 100), (Key::Hash, 100), (Key::Digit(9), 100), (Key::Digit(2), 100), (Key::Digit(9), 100), (Key::Hash, 100), (Key::Digit(4), 100), (Key::Digit(6), 100), (Key::Digit(6), 100), (Key::Digit(3), 100), (Key::Down, 100), (Key::Down, 100)]),
        ("aprs-compose-short", Dest::AprsMessages, &[(Key::Menu, 100), (Key::Menu, 100), (Key::Digit(7), 100), (Key::Digit(3), 100), (Key::Digit(2), 100), (Key::Digit(3), 100), (Key::Digit(4), 100), (Key::Digit(8), 100), (Key::Digit(3), 100), (Key::Digit(3), 100), (Key::Down, 100)]),
        ("quick-messages", Dest::QuickMessages, &[(Key::Down, 100), (Key::Down, 100)]),
        ("browse", Dest::Browse, &[(Key::Down, 100)]),
        ("browse-zone", Dest::Browse, &[(Key::Down, 100), (Key::Menu, 100), (Key::Down, 100)]),
        ("browse-all", Dest::Browse, &[(Key::Up, 100), (Key::Up, 100), (Key::Up, 100), (Key::Up, 100), (Key::Up, 100), (Key::Up, 100), (Key::Up, 100), (Key::Menu, 100)]),
        ("browse-nearby", Dest::Browse, &[(Key::Up, 100), (Key::Menu, 100)]),
        ("browse-recent", Dest::Browse, &[(Key::Up, 100), (Key::Up, 100), (Key::Menu, 100)]),
        ("browse-actions", Dest::Browse, &[(Key::Down, 100), (Key::Menu, 100), (Key::Down, 100), (Key::Menu, 100)]),
        ("browse-delete", Dest::Browse, &[(Key::Down, 100), (Key::Menu, 100), (Key::Down, 100), (Key::Menu, 100), (Key::Up, 100), (Key::Menu, 100)]),
        ("zones", Dest::Zones, &[(Key::Down, 100)]),
        ("zone-edit", Dest::Zones, &[(Key::Down, 100), (Key::Slot, 100), (Key::Down, 100), (Key::Down, 100), (Key::Down, 100), (Key::Down, 100)]),
        ("zone-delete", Dest::Zones, &[(Key::Down, 100), (Key::Down, 100), (Key::Star, 100)]),
        ("range-presets", Dest::RangePresets, &[(Key::Down, 100)]),
        ("range-preset", Dest::RangePresets, &[(Key::Down, 100), (Key::Down, 100), (Key::Menu, 100), (Key::Down, 100)]),
        ("skip-list", Dest::SkipList, &[]),
        ("hf-recent", Dest::HfRecent, &[]),
        ("gps-position", Dest::GpsPosition, &[]),
        ("gps-satellites", Dest::GpsPosition, &[(Key::Right, 100)]),
        ("waypoints", Dest::Waypoints, &[(Key::Down, 100)]),
        ("waypoint-go", Dest::Waypoints, &[(Key::Down, 100), (Key::Menu, 100)]),
    ];
    for &(name, dest, keys) in shots {
        press(&mut app, Key::Exit, 800, &mut t); // home
        run(&mut app, &mut fb, t + 3500, &mut t); // toasts gone
        app.open(dest);
        for &(k, hold) in keys {
            press(&mut app, k, hold, &mut t);
            t += 150;
        }
        run(&mut app, &mut fb, t, &mut t);
        save(&fb, name, scale);
    }
    // HF mode in AM on a broadcast band (HF-32): 6.070 MHz, 49 m; group D's "hf" is 7.074 USB on 40 m.
    press(&mut app, Key::Exit, 800, &mut t);
    run(&mut app, &mut fb, t + 3500, &mut t);
    app.open(Dest::HfMode);
    run(&mut app, &mut fb, t + 3000, &mut t); // the patch loads
    for k in [Key::Digit(6), Key::Digit(0), Key::Digit(7), Key::Digit(0), Key::Menu] {
        press(&mut app, k, 80, &mut t);
        t += 150;
    }
    run(&mut app, &mut fb, t + 1500, &mut t);
    save(&fb, "hf-am", scale);
    // SCAN-14: the range preset editor refuses a span across 64 MHz (144-148 MHz, Start turned down to 54).
    press(&mut app, Key::Exit, 800, &mut t);
    run(&mut app, &mut fb, t + 3500, &mut t);
    app.open(Dest::RangePresets);
    press(&mut app, Key::Menu, 100, &mut t);
    press(&mut app, Key::Down, 100, &mut t);
    for _ in 0..9 {
        app.knob(-100, t, &store);
        t += 50;
    }
    run(&mut app, &mut fb, t, &mut t);
    save(&fb, "range-preset-refused", scale);
    // GPS while moving (GPS-17, GPS-19): 46 km/h on a course of 120 degrees; the go-to dial turns course up.
    if let Some(f) = store.fix.get() {
        store.fix.set(Some(ferrite_app::data::Fix { speed_kmh: 46, heading: 120, ..f }));
        for &(name, dest, keys) in &[("gps-position-moving", Dest::GpsPosition, &[][..]), ("waypoint-go-moving", Dest::Waypoints, &[(Key::Down, 100), (Key::Menu, 100)][..])] {
            press(&mut app, Key::Exit, 800, &mut t);
            run(&mut app, &mut fb, t + 3500, &mut t);
            app.open(dest);
            for &(k, hold) in keys {
                press(&mut app, k, hold, &mut t);
                t += 150;
            }
            run(&mut app, &mut fb, t, &mut t);
            save(&fb, name, scale);
        }
        store.fix.set(Some(f));
    }
    shots_d::run(scale);
    shots_aprs::run(scale);
    shots_sat::run(scale);
    shots_rad950::run(scale);
    themed(scale);
}

/// A few screens in night red, daylight and cyberpunk (0 7 1): boot, home, quick settings, transmit, refused, scope
/// and the shortcut popup, as `<name>-night.png` / `<name>-day.png` / `<name>-cyber.png`.
fn themed(scale: u32) {
    let checks = [
        Check { label: "MCU", value: "AT32F403A", state: State::Ok },
        Check { label: "SPI flash", value: "4 MB", state: State::Ok },
        Check { label: "BK4829 VHF", value: "0x4829", state: State::Ok },
        Check { label: "BK4829 UHF", value: "0x4829", state: State::Ok },
        Check { label: "SI4732", value: "not tested", state: State::Off },
        Check { label: "Calibration", value: "", state: State::Pending },
    ];
    for (theme, suffix) in [(ferrite_ui::theme::NIGHT_RED, "night"), (ferrite_ui::theme::DAYLIGHT, "day"), (ferrite_ui::theme::CYBERPUNK, "cyber")] {
        let name = |n: &str| format!("{n}-{suffix}");
        let mut fb = FrameBuffer::new();
        ferrite_ui::theme::set(theme);
        boot::draw(&mut fb, &BootInfo { version: VERSION, callsign: Some(b"VA7FER"), line: Some(b"VA7FER"), owner: None, checks: &checks, note: None });
        save(&fb, &name("boot"), scale);
        let store = fake::store();
        let mut radio = fake::FakeRadio::new();
        let mut dev = fake::device();
        dev.clock_min = Some(14 * 60 + 32);
        dev.utc = false;
        let mut app = fake::app();
        app.settings_mut().backlight = 0; // shots: the screen never goes dark (lights tested in ferrite-app)
        fake::sample(&mut app, &store, &mut radio);
        app.settings_mut().theme = theme;
        let mut t = 0u32;
        let mut run = |app: &mut App, fb: &mut FrameBuffer, until: u32, t: &mut u32| {
            while *t <= until {
                while let Some(c) = app.pop_cmd() {
                    radio.cmd(c);
                }
                while let Some(w) = app.pop_write() {
                    store.apply(w);
                }
                radio.step(*t as f32 / 1000.0, app.active());
                app.tick(*t, &radio.live, &store);
                app.render(fb, &radio.live, &dev, &store);
                *t += 20;
            }
        };
        let press = |app: &mut App, key: Key, hold: u32, t: &mut u32| {
            app.key_down(key, *t, &store);
            *t += hold;
            app.tick(*t, &Default::default(), &store);
            app.key_up(key, *t, &store);
        };
        run(&mut app, &mut fb, 7000, &mut t);
        save(&fb, &name("home"), scale);
        press(&mut app, Key::Menu, 100, &mut t);
        run(&mut app, &mut fb, t, &mut t);
        save(&fb, &name("menu-quick"), scale);
        press(&mut app, Key::Exit, 800, &mut t);
        app.key_down(Key::PTT, t, &store);
        run(&mut app, &mut fb, t + 2800, &mut t);
        save(&fb, &name("transmit"), scale);
        app.key_up(Key::PTT, t, &store);
        app.set_vfo(0, 42_050_000);
        app.key_down(Key::PTT, t, &store);
        app.key_up(Key::PTT, t + 200, &store);
        run(&mut app, &mut fb, t + 300, &mut t);
        save(&fb, &name("tx-refused"), scale);
        press(&mut app, Key::Exit, 800, &mut t);
        app.set_vfo(0, 14_652_000);
        run(&mut app, &mut fb, t + 3500, &mut t);
        press(&mut app, Key::Digit(1), 800, &mut t);
        run(&mut app, &mut fb, t + 2500, &mut t);
        save(&fb, &name("scope"), scale);
        press(&mut app, Key::Exit, 800, &mut t);
        run(&mut app, &mut fb, t + 3500, &mut t);
        press(&mut app, Key::Digit(2), 800, &mut t);
        run(&mut app, &mut fb, t + 100, &mut t);
        save(&fb, &name("popup"), scale);
    }
    ferrite_ui::theme::set(ferrite_ui::theme::DARK);
}
