//! SOL950 weak-signal HF mode UI preview gallery (`target/sim/sol950-*.png`):
//! Operate, Active QSO, Setup, Log, Log Detail, Stats, Worked Grids overview, and Field drill-down.

use crate::fake;
use ferrite_app::sol950::{GridSource, SavedContact, Sol950Host, Sol950Store};
use ferrite_app::App;
use ferrite_core::keys::Key;
use ferrite_core::sol950::usol950::frame::encode_grid4;
use ferrite_core::sol950::usol950::{Callsign, Decoded, Frame};
use ferrite_ui::fb::FrameBuffer;

pub fn run(scale: u32) {
    let store = fake::store();
    let mut radio = fake::FakeRadio::new();
    let mut dev = fake::device();
    dev.clock_min = Some(19 * 60 + 24);
    dev.tx_built = true;
    let mut app = fake::app();
    app.settings_mut().backlight = 0;
    fake::sample(&mut app, &store, &mut radio);
    app.settings_mut().tz = 14; // UTC

    let host = Box::leak(Box::new(Sol950Host::new()));
    host.profile.set_base_call(b"NQ6N");
    host.profile.manual_grid = *b"CM87";
    host.profile.use_gps_grid = true;
    host.sync_engine_config();
    app.sol950_attach(host);

    let mut fb = FrameBuffer::new();
    let mut t = 0u32;
    let base_utc = 1_763_580_240u32; // 19:24:00 UTC

    let mut step = |app: &mut App, fb: &mut FrameBuffer, radio: &mut fake::FakeRadio, ms: u32, t: &mut u32| {
        let end = *t + ms;
        while *t <= end {
            while let Some(c) = app.pop_cmd() {
                radio.cmd(c);
            }
            radio.step(*t as f32 / 1000.0, app.active());
            if let Some(h) = app.sol950_host_mut() {
                h.update_clock(base_utc + *t / 1000, (*t % 1000) as u16, 35, Some(*b"CM87"));
            }
            dev.clock_min = Some(((base_utc + *t / 1000) / 60 % 1440) as u16);
            app.tick(*t, &radio.live, &store);
            app.render(fb, &radio.live, &dev, &store);
            *t += 20;
        }
    };
    let press = |app: &mut App, key: Key, t: &mut u32| {
        app.key_down(key, *t, &store);
        *t += 60;
        app.key_up(key, *t, &store);
        *t += 80;
    };

    step(&mut app, &mut fb, &mut radio, 200, &mut t);
    app.open(ferrite_app::screens::nav::Dest::Sol950);
    step(&mut app, &mut fb, &mut radio, 200, &mut t);

    // Populate realistic heard peers & saved contacts across 10m / 12m / 15m / 17m.
    if let Some(h) = app.sol950_host_mut() {
        let peers = [
            (b"K1ABC".as_slice(), *b"FN31", -12i8, 1130u16),
            (b"W6XYZ".as_slice(), *b"CM87", -16i8, 1380u16),
            (b"G4DEF".as_slice(), *b"IO91", -18i8, 1620u16),
            (b"JA1GHI".as_slice(), *b"PM95", -19i8, 1850u16),
        ];
        for (call, grid, snr, hz) in peers {
            h.on_rx_frame(Decoded {
                frame: Frame::Cq {
                    sender: Callsign::encode(call).unwrap(),
                    grid4: encode_grid4(&grid).unwrap(),
                    tx_pwr: 10,
                    reserved: 0,
                },
                audio_hz: hz,
                time_offset_ms: 40,
                snr_db: snr,
            });
        }

        let make_saved = |utc: u32, dial: u32, peer: &[u8], pgrid: [u8; 4], rx: i8, tx: i8, slot: u32| {
            let mut peer_id = [0u8; 16];
            peer_id[..peer.len()].copy_from_slice(peer);
            let mut my_id = [0u8; 16];
            my_id[..4].copy_from_slice(b"NQ6N");
            SavedContact {
                utc_sec: utc,
                dial_hz10: dial,
                peer_id,
                my_id,
                peer_grid: pgrid,
                my_grid: *b"CM87",
                rx_snr_db: rx,
                tx_snr_db: tx,
                my_grid_src: GridSource::Gps,
                slot,
                seq: 0,
            }
        };
        let saved = [
            make_saved(base_utc - 3600, 2_807_800, b"K1ABC", *b"FN31", -14, -12, 100),
            make_saved(base_utc - 2400, 2_491_900, b"W1AW", *b"FN31", -11, -10, 200),
            make_saved(base_utc - 1200, 2_107_800, b"N2JKL", *b"FN20", -16, -15, 300),
            make_saved(base_utc - 600, 1_810_400, b"G4DEF", *b"IO91", -17, -18, 400),
            make_saved(base_utc - 300, 2_807_800, b"JA1GHI", *b"PM95", -19, -17, 500),
        ];
        for c in &saved {
            h.store.save_contact(c);
        }
    }

    // 1. Operate screen (activity list + status header)
    step(&mut app, &mut fb, &mut radio, 100, &mut t);
    crate::save(&fb, "sol950-operate", scale);

    // 2. Active QSO screen (answering selected peer)
    press(&mut app, Key::Menu, &mut t);
    step(&mut app, &mut fb, &mut radio, 100, &mut t);
    crate::save(&fb, "sol950-active", scale);

    // Return to Operate
    press(&mut app, Key::Digit(0), &mut t);
    step(&mut app, &mut fb, &mut radio, 100, &mut t);

    // 3. Setup screen (STAR)
    press(&mut app, Key::Star, &mut t);
    step(&mut app, &mut fb, &mut radio, 100, &mut t);
    crate::save(&fb, "sol950-setup", scale);
    press(&mut app, Key::Exit, &mut t);
    step(&mut app, &mut fb, &mut radio, 100, &mut t);

    // 4. Log screen (Digit 2)
    press(&mut app, Key::Digit(2), &mut t);
    step(&mut app, &mut fb, &mut radio, 100, &mut t);
    crate::save(&fb, "sol950-log", scale);

    // 5. Log Detail screen (OK)
    press(&mut app, Key::Menu, &mut t);
    step(&mut app, &mut fb, &mut radio, 100, &mut t);
    crate::save(&fb, "sol950-log-detail", scale);
    press(&mut app, Key::Exit, &mut t);
    press(&mut app, Key::Exit, &mut t);
    step(&mut app, &mut fb, &mut radio, 100, &mut t);

    // 6. Stats screen (Digit 3)
    press(&mut app, Key::Digit(3), &mut t);
    step(&mut app, &mut fb, &mut radio, 100, &mut t);
    crate::save(&fb, "sol950-stats", scale);
    press(&mut app, Key::Exit, &mut t);
    step(&mut app, &mut fb, &mut radio, 100, &mut t);

    // 7. Worked Grids 18x18 overview (Digit 4, cursor on FN)
    press(&mut app, Key::Digit(4), &mut t);
    step(&mut app, &mut fb, &mut radio, 100, &mut t);
    crate::save(&fb, "sol950-grids", scale);

    // 8. Worked Grids 10x10 FN field drill-down (OK)
    press(&mut app, Key::Menu, &mut t);
    step(&mut app, &mut fb, &mut radio, 100, &mut t);
    crate::save(&fb, "sol950-grids-field", scale);
}
