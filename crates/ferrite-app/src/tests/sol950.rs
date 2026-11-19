use super::T;
use crate::screens::sol950::*;
use crate::sol950::{SavedContact, Sol950Host, Sol950Store};
use crate::Screen;
use ferrite_core::keys::Key;

#[test]
fn sol950_screens_log_stats_and_worked_grids() {
    let mut t = T::new();
    let host: &'static mut Sol950Host = std::boxed::Box::leak(std::boxed::Box::new(Sol950Host::new()));
    host.profile.set_base_call(b"NQ6N");
    host.utc_sec = 20_001 * 86_400 + 3600;
    let mut c1 = SavedContact::EMPTY;
    c1.utc_sec = 20_000 * 86_400 + 86_000; // yesterday UTC
    c1.peer_id[..5].copy_from_slice(b"K1ABC");
    c1.peer_grid = *b"FN31";
    c1.rx_snr_db = -14;
    c1.tx_snr_db = -12;
    c1.slot = 100;
    host.store.save_contact(&c1);

    let mut c2 = SavedContact::EMPTY;
    c2.utc_sec = 20_001 * 86_400 + 600; // today UTC
    c2.peer_id[..4].copy_from_slice(b"W1AW");
    c2.peer_grid = *b"FK77";
    c2.rx_snr_db = -9;
    c2.tx_snr_db = -10;
    c2.slot = 500;
    host.store.save_contact(&c2);
    t.app.sol950_attach(host);

    let mut fb = ferrite_ui::fb::FrameBuffer::new();
    // Open SOL950 -> Operate
    t.open(crate::screens::nav::Dest::Sol950);
    assert!(matches!(t.app.screen(), Screen::Sol950(s) if s.page == PAGE_OPERATE));
    t.render(&mut fb);

    // Press 2 -> Log -> OK -> Detail -> BACK -> BACK
    t.tap(Key::Digit(2));
    assert!(matches!(t.app.screen(), Screen::Sol950(s) if s.page == PAGE_LOG));
    t.render(&mut fb);
    t.tap(Key::Menu);
    assert!(matches!(t.app.screen(), Screen::Sol950(s) if s.page == PAGE_LOG_DETAIL));
    t.render(&mut fb);
    t.tap(Key::Exit);
    t.tap(Key::Exit);
    assert!(matches!(t.app.screen(), Screen::Sol950(s) if s.page == PAGE_OPERATE));

    // Press 3 -> Stats (All saved -> toggle Today (UTC))
    t.tap(Key::Digit(3));
    assert!(matches!(t.app.screen(), Screen::Sol950(s) if s.page == PAGE_STATS && s.sel == 0));
    t.render(&mut fb);
    t.tap(Key::Right);
    assert!(matches!(t.app.screen(), Screen::Sol950(s) if s.page == PAGE_STATS && s.sel == 1));
    t.render(&mut fb);
    t.tap(Key::Exit);

    // Press 4 -> Worked Grids (1x World -> 3x Region -> 10x Field -> Band filter -> Filtered Log)
    t.tap(Key::Digit(4));
    assert!(matches!(t.app.screen(), Screen::Sol950(s) if s.page == PAGE_GRIDS_OVERVIEW && s.top == 0));
    t.render(&mut fb);
    t.tap(Key::Menu);
    assert!(matches!(t.app.screen(), Screen::Sol950(s) if s.page == PAGE_GRIDS_OVERVIEW && s.top == 1));
    t.render(&mut fb);
    t.tap(Key::Menu);
    assert!(matches!(t.app.screen(), Screen::Sol950(s) if s.page == PAGE_GRIDS_FIELD));
    t.tap(Key::Star); // Band filter -> 10m
    assert!(matches!(t.app.screen(), Screen::Sol950(s) if s.page == PAGE_GRIDS_FIELD && s.sel == 1));
    t.render(&mut fb);
    t.tap(Key::Menu);
    assert!(matches!(t.app.screen(), Screen::Sol950(s) if s.page == PAGE_LOG));
    assert!(t.app.sol950_host().unwrap().log_grid_filter.is_some());
    t.render(&mut fb);
}
