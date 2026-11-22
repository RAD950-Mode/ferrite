//! The way back to stock over the cable (BACKUP-7, LINK-8, decision 261): once the link session has accepted
//! `STOCK_RESTORE`'s `START`, the UI task comes here and never goes back. The radio is quiet, the store's queue is
//! drained and nothing of Ferrite's is saved again; the cable's frames go to [`ferrite_store::restore::serve`],
//! which writes only inside the driver's own writable ranges (never calibration or the update area), until
//! `FINISH` resets into the bootloader for the stock `.BTF`. Turning the radio off is the only other way out.

use crate::board::{LED_ACTIVE_HIGH, LED_GREEN};
use crate::chan::{TERM_IN, TERM_OUT};
use crate::store::{self, Store};
use embassy_futures::block_on;
use embassy_time::{block_for, Duration};
use ferrite_core::link::{self as wire, msg, Status, MAX_PAYLOAD, MAX_WIRE};
use ferrite_app::lights::{xfer_flash, XFER_LINGER_MS};
use ferrite_core::VERSION;
use ferrite_store::restore::{serve, start, After};
use ferrite_ui::boot::{self, BootInfo};
use ferrite_ui::panel::Panel;

fn now_ms() -> u32 {
    embassy_time::Instant::now().as_millis() as u32
}

fn screen(lcd: &mut impl Panel, note: &[u8]) {
    // The line holds about 24 characters in its font ("Back to stock: keep the cable in" was cut, Nov 16, 2026):
    // the cable goes in the status line below.
    let b = BootInfo { version: VERSION, callsign: None, line: Some(b"Back to stock"), owner: None, checks: &[], note: Some(note) };
    boot::draw(lcd, &b);
}

/// One answer out on the cable, whole (the ring has room for it: answers are short and the PC waits for each).
#[inline(never)]
fn send(ty: u8, seq: u8, ans: &[u8]) {
    let mut w = [0u8; MAX_WIRE];
    let Some(k) = wire::build(ty | wire::ANSWER, seq, ans, &mut w) else { return };
    while (TERM_OUT.room() as usize) < k {
        block_for(Duration::from_millis(1));
    }
    for &c in &w[..k] {
        TERM_OUT.put(c);
    }
    crate::hal::uart::tx_ring(crate::report::UART_CPS, &TERM_OUT); // decision 290: the interrupt sends it
}

/// Serves the restore from the accepted `START` (its seq) on; never returns.
/// Kept synchronous (`#[inline(never)]`) so its 3+ KiB wire buffers do not inflate `ui::task`'s
/// async state machine or steady-state poll stack frame; caller drains `store.idle()` first.
/// `leds`: 0 7 6 isn't Off, so the green LED flashes while frames come (DISP-30).
#[inline(never)]
pub fn run(store: &'static Store, lcd: &mut impl Panel, seq: u8, leds: bool) -> ! {
    screen(lcd, b"Restoring: keep the cable in");
    let (chip, ranges, uid) = (store.nor().size(), store::WRITE_OK, crate::hal::sys::uid());
    let mut ans = [0u8; MAX_PAYLOAD];
    let n = block_on(start(&mut *store.nor(), chip, ranges, uid, &mut ans));
    send(msg::STOCK_RESTORE, seq, &ans[..n]);

    let mut rx = [0u8; MAX_WIRE];
    let mut rn = 0usize;
    let mut overflow = false;
    let mut last = now_ms();
    loop {
        let Some(b) = TERM_IN.get() else {
            // DISP-30: the transfer flash while frames come, off XFER_LINGER_MS after the last
            let now = now_ms();
            let on = leds && now.wrapping_sub(last) < XFER_LINGER_MS && xfer_flash(now).0;
            LED_GREEN.write(on == LED_ACTIVE_HIGH);
            block_for(Duration::from_millis(1));
            continue;
        };
        if b != 0 {
            if rn < rx.len() {
                rx[rn] = b;
                rn += 1;
            } else {
                overflow = true;
            }
            continue;
        }
        let len = core::mem::take(&mut rn);
        if len == 0 || core::mem::take(&mut overflow) {
            continue; // a sync 0x00, or a frame too long for any request
        }
        let (ty, seq, n, after) = match wire::parse(&mut rx[..len]) {
            Err(e) => {
                ans[0] = Status::BadFrame as u8;
                (e.ty, e.seq, 1, After::Next)
            }
            // Only the restore's own requests from here on.
            Ok(f) if f.ty != msg::STOCK_RESTORE => {
                ans[0] = Status::Busy as u8;
                (f.ty, f.seq, 1, After::Next)
            }
            Ok(f) => {
                let (ty, seq) = (f.ty, f.seq);
                let mut p = [0u8; MAX_PAYLOAD];
                let pl = f.payload.len();
                p[..pl].copy_from_slice(f.payload);
                let (n, after) = block_on(serve(&mut *store.nor(), chip, ranges, uid, &p[..pl], &mut ans));
                (ty, seq, n, after)
            }
        };
        send(ty, seq, &ans[..n]);
        last = now_ms();
        if after == After::Reset {
            screen(lcd, b"Done: restarting");
            while TERM_OUT.count() > 0 {
                block_for(Duration::from_millis(1));
            }
            // The answer's last bytes leave the UART, and the PC has its probes going before the bootloader's
            // ~200 ms window opens: a missed window would start Ferrite over the restored data.
            block_for(Duration::from_millis(300));
            cortex_m::peripheral::SCB::sys_reset();
        }
    }
}
