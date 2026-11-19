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

        // Pre-fill 22.4 s (112 cols) with real `usol950::Receiver` waterfall slices from synthesized 4-FSK + AWGN.
        let wf_seq = ferrite_sim::sol950_wf::sample_panafall();
        for i in 0..112usize {
            let (col, edge) = wf_seq[i % wf_seq.len()];
            h.wf.push(col, edge, false);
        }
    }

    // 1. Operate screen (activity list + status header)
    step(&mut app, &mut fb, &mut radio, 100, &mut t);
    crate::save(&fb, "sol950-operate", scale);

    // 2. Active QSO screen (answering selected peer, showing live red TX 4-FSK trace scrolling in!)
    press(&mut app, Key::Menu, &mut t);
    if let Some(h) = app.sol950_host_mut() {
        let tx_frame = Frame::Rx {
            sender: Callsign::encode(b"NQ6N").unwrap(),
            recipient: Callsign::encode(b"K1ABC").unwrap(),
            snr_db: -12,
            ack: false,
            reserved: false,
        };
        let tones = ferrite_core::sol950::usol950::encode_tones(tx_frame);
        for k in 0..35usize {
            let sym = (k * 200) / 80;
            let col = ferrite_app::sol950::tx_wf_col(1525, tones[sym.min(tones.len() - 1)]);
            h.wf.push(col, k == 0, true);
        }
        h.tx_in_flight = true;
    }
    step(&mut app, &mut fb, &mut radio, 100, &mut t);
    crate::save(&fb, "sol950-active", scale);
    if let Some(h) = app.sol950_host_mut() {
        h.tx_in_flight = false;
    }

    // Return to Operate
    press(&mut app, Key::Digit(0), &mut t);
    step(&mut app, &mut fb, &mut radio, 100, &mut t);

    // 3. Setup screen (STAR, scrolled to show Band/Dial + Coop RX + Coop link)
    press(&mut app, Key::Star, &mut t);
    for _ in 0..8 {
        press(&mut app, Key::Down, &mut t);
    }
    press(&mut app, Key::Right, &mut t); // Coop RX -> Capture
    step(&mut app, &mut fb, &mut radio, 100, &mut t);
    crate::save(&fb, "sol950-setup", scale);
    press(&mut app, Key::Left, &mut t); // Restore Coop RX -> Off
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

    // 7. Worked Grids 1x World Map (Digit 4, cursor on FN, All bands)
    press(&mut app, Key::Digit(4), &mut t);
    step(&mut app, &mut fb, &mut radio, 100, &mut t);
    crate::save(&fb, "sol950-grids", scale);

    // 7b. Worked Grids 3x Region Map (OK -> 3x zoom centered on FN, showing worked 4-char squares over North America!)
    press(&mut app, Key::Menu, &mut t);
    step(&mut app, &mut fb, &mut radio, 100, &mut t);
    crate::save(&fb, "sol950-grids-region", scale);

    // 8. Worked Grids 10x FN field drill-down (OK -> 10x zoom over coastline, toggle * to 10m band!)
    press(&mut app, Key::Menu, &mut t);
    press(&mut app, Key::Star, &mut t); // Band filter -> 10m (amber)
    step(&mut app, &mut fb, &mut radio, 100, &mut t);
    crate::save(&fb, "sol950-grids-field", scale);
    // Exit back to Operate (10x -> 3x -> 1x -> Operate)
    press(&mut app, Key::Exit, &mut t);

    // 9. Chat conversation flow via real keypad navigation:
    //    Operate -> OK (answer K1ABC) -> TX report -> peer R+RX saves QSO -> 2 (Chat) -> peer Accept + Question
    press(&mut app, Key::Exit, &mut t);
    press(&mut app, Key::Exit, &mut t);
    step(&mut app, &mut fb, &mut radio, 60, &mut t);
    press(&mut app, Key::Menu, &mut t); // Active QSO with K1ABC (Answering)
    t = (t / 10_000 + 1) * 10_000 + 100; // advance to our TX slot
    step(&mut app, &mut fb, &mut radio, 60, &mut t);
    if let Some(h) = app.sol950_host_mut() {
        h.on_tx_done(true); // our report finished transmitting
    }
    t = (t / 10_000 + 1) * 10_000 + 100; // advance to peer's reply slot
    step(&mut app, &mut fb, &mut radio, 60, &mut t);
    if let Some(h) = app.sol950_host_mut() {
        let peer = Callsign::encode(b"K1ABC").unwrap();
        let me = Callsign::encode(b"NQ6N").unwrap();
        h.on_rx_frame(Decoded {
            frame: Frame::Rx { sender: peer, recipient: me, snr_db: -13, ack: true, reserved: false },
            audio_hz: 1130,
            time_offset_ms: 20,
            snr_db: -14,
        });
    }
    step(&mut app, &mut fb, &mut radio, 60, &mut t);
    press(&mut app, Key::Digit(2), &mut t); // 2 Chat -> sends Invite & opens CHAT
    if let Some(h) = app.sol950_host_mut() {
        use ferrite_core::sol950::usol950::chat::{encode_chat_frame, ChatBody, ChatCtrl, ChatPacket};
        let sref = h.chat.session_ref;
        for (seq, body) in [
            (0u8, ChatBody::Ctrl(ChatCtrl::Accept)),
            (1u8, ChatBody::Question { topic: 0, variant: 0 }),
        ] {
            h.on_rx_frame(Decoded {
                frame: encode_chat_frame(ChatPacket { session_ref: sref, seq, body }),
                audio_hz: 1130,
                time_offset_ms: 20,
                snr_db: -12,
            });
        }
    }
    step(&mut app, &mut fb, &mut radio, 60, &mut t);

    // 10. Chat Reply menu (context-aware: "My station" auto-highlighted because peer asked about antenna!)
    press(&mut app, Key::Menu, &mut t);
    step(&mut app, &mut fb, &mut radio, 100, &mut t);
    crate::save(&fb, "sol950-chat-reply", scale);

    // Queue My station card (OK -> Preview, OK -> Queue TX -> returns to CHAT)
    press(&mut app, Key::Menu, &mut t);
    press(&mut app, Key::Menu, &mut t);
    // Position clock at +4 s into the peer's slot so "TX in 6s" is clearly displayed.
    t = (t / 10_000) * 10_000 + 4_000;
    step(&mut app, &mut fb, &mut radio, 100, &mut t);
    crate::save(&fb, "sol950-chat", scale);

    // 11. Chat Preview screen for multi-burst typed message ("HEY MATT HOW HAVE YOU BEEN?", 6 bursts ~1m 49s)
    press(&mut app, Key::Menu, &mut t); // Reply
    press(&mut app, Key::Menu, &mut t); // Preview
    if let Some(h) = app.sol950_host_mut() {
        h.chat.set_literal_draft(b"HEY MATT HOW HAVE YOU BEEN?");
    }
    step(&mut app, &mut fb, &mut radio, 100, &mut t);
    crate::save(&fb, "sol950-chat-preview", scale);

    // 12. Chat More menu (Weather, About me, Schedule QSO, Move freq)
    press(&mut app, Key::Exit, &mut t); // back to Reply (sel = 0)
    press(&mut app, Key::Down, &mut t);
    press(&mut app, Key::Down, &mut t);
    press(&mut app, Key::Down, &mut t);
    press(&mut app, Key::Down, &mut t); // More... (row 4)
    press(&mut app, Key::Menu, &mut t);
    step(&mut app, &mut fb, &mut radio, 100, &mut t);
    crate::save(&fb, "sol950-chat-more", scale);

    // 13. Standalone Distress preview screen (selected from bottom of More menu, confirmed -> 3x PTT interlock)
    press(&mut app, Key::Down, &mut t);
    press(&mut app, Key::Down, &mut t);
    press(&mut app, Key::Down, &mut t);
    press(&mut app, Key::Down, &mut t); // Send distress... (row 4)
    press(&mut app, Key::Menu, &mut t);
    press(&mut app, Key::Right, &mut t); // Category -> Medical
    press(&mut app, Key::Menu, &mut t); // Confirm -> shows 3x PTT hold prompt
    step(&mut app, &mut fb, &mut radio, 100, &mut t);
    crate::save(&fb, "sol950-distress", scale);

    // 14. Automated Rendezvous visual (10m window active at :01:24 UTC, 10m+12m+17m selected, Follow ON, Call & listen ARMED)
    press(&mut app, Key::Exit, &mut t); // Chat
    press(&mut app, Key::Exit, &mut t); // Operate
    press(&mut app, Key::Digit(5), &mut t); // Rendezvous
    if let Some(h) = app.sol950_host_mut() {
        h.rdv.band_mask = 0b1011; // 10m, 12m, 17m enabled; 15m disabled (no antenna)
        h.rdv.follow_schedule = true;
        h.rdv.call_listen_armed = true;
    }
    // Position UTC clock at 19:01:24Z (inside the 10m :00-:04 window, 4s into a 10s slot!)
    let base_hour = (base_utc / 3600) * 3600;
    let rdv_utc = base_hour + 84; // :01:24 UTC
    if let Some(h) = app.sol950_host_mut() {
        h.update_clock(rdv_utc, 0, 35, Some(*b"CM87"));
    }
    dev.clock_min = Some(((rdv_utc / 60) % 1440) as u16);
    app.render(&mut fb, &radio.live, &dev, &store);
    crate::save(&fb, "sol950-rendezvous", scale);
}
