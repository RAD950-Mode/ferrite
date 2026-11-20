//! Multi-station RAD950 network simulator driving real `ferrite_app::rad950::Rad950Host` instances
//! over an empirically calibrated HF propagation, 8-subchannel SINR/capture, and `urad950` decode model.
//!
//! Run with:
//!   cargo run --offline --target-dir /tmp/ferrite-target --release -p ferrite-app --example rad950_net_sim

use ferrite_app::rad950::{AutoAnswer, ClockSource, Rad950Host, Rad950Store, RAD950_DIALS};
use ferrite_core::freq::Freq10;
use ferrite_core::rad950::urad950::{Decoded, Frame, MAX_DECODED, SUBCHANNELS_HZ};
use std::collections::HashSet;

struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn f01(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64) / ((1u64 << 53) as f64)
    }
    fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.f01()
    }
    fn gauss(&mut self, sigma: f64) -> f64 {
        let mut s = 0.0;
        for _ in 0..12 {
            s += self.f01();
        }
        (s - 6.0) * sigma
    }
}

/// Generate a valid unique 5-char amateur callsign (`K0AAA`..`K9ZZZ`, up to 175,760 unique calls).
fn gen_call(idx: usize) -> [u8; 5] {
    let digit = b'0' + ((idx / (26 * 26)) % 10) as u8;
    let c1 = b'A' + ((idx / 26) % 26) as u8;
    let c2 = b'A' + (idx % 26) as u8;
    let pfx = b"KWNA"[(idx / (10 * 26 * 26)) % 4];
    [pfx, digit, c1, c2, b'A' + ((idx * 7) % 26) as u8]
}

/// Generate a spread of realistic North American Maidenhead 4-char grids (`CM..FN`, `30..69`).
fn gen_grid(idx: usize) -> [u8; 4] {
    let fields: [&[u8; 2]; 12] = [
        b"EN", b"FN", b"EM", b"FM", b"DN", b"DM", b"CN", b"CM", b"EL", b"DL", b"EO", b"FO",
    ];
    let f = fields[idx % fields.len()];
    let sq0 = b'0' + ((idx / 3) % 10) as u8;
    let sq1 = b'0' + ((idx / 7) % 10) as u8;
    [f[0], f[1], sq0, sq1]
}

fn make_host(idx: usize, dial_idx: u8, mode60: bool) -> Box<Rad950Host> {
    let mut h = Box::new(Rad950Host::new());
    let call = gen_call(idx);
    h.profile.set_base_call(&call);
    h.profile.manual_grid = gen_grid(idx);
    h.profile.use_gps_grid = false;
    h.profile.dial_idx = dial_idx;
    h.profile.mode60 = mode60;
    h.sync_engine_config();
    h
}

/// Empirically calibrated `urad950` blind decode probability vs SINR (2,500 Hz reference BW):
/// - `10 s` v1: 95% @ -14 dB, 70% @ -16 dB, 32% @ -17 dB, 7% @ -18 dB, 0% @ -19 dB (50% @ -16.5 dB).
/// - `60 s` RAD950-60 (`Receiver60`): shifted by -3.5 dB (50% @ -20.0 dB).
fn decode_prob(sinr_db: f64, mode60: bool) -> f64 {
    let mid = if mode60 { -20.0 } else { -16.5 };
    if sinr_db < mid - 2.6 {
        return 0.0;
    }
    if sinr_db > mid + 5.0 {
        return 1.0;
    }
    1.0 / (1.0 + (-(sinr_db - mid) * 1.45).exp())
}

fn nearest_subch(hz: u16) -> usize {
    let mut best = 0usize;
    let mut best_d = u16::MAX;
    for (i, &f) in SUBCHANNELS_HZ.iter().enumerate() {
        let d = f.abs_diff(hz);
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

#[derive(Clone)]
struct TxBurst {
    tx_idx: usize,
    dial: Freq10,
    subch_hz: u16,
    subch_idx: usize,
    frame: Frame,
}

#[allow(dead_code)]
struct SimStats {
    tx_bursts: u64,
    tx_cqs: u64,
    tx_rxs: u64,
    tx_r1s: u64,
    co_chan_collisions: u64,
    decodes_ok: u64,
    both_logged_pairs: usize,
    one_sided_pairs: usize,
    total_logged_records: usize,
    unique_grids_avg: f64,
}

/// Run `n_slots` 10-second UTC slots over `hosts` using directional path SNR matrix `path_snr[tx][rx]`
/// (`None` = no propagation path). Returns detailed aggregate statistics.
fn run_network(
    hosts: &mut [Box<Rad950Host>],
    path_snr: &[Vec<Option<f64>>],
    start_slot: u32,
    n_slots: u32,
    qsb_sigma_db: f64,
    rng: &mut Rng,
) -> SimStats {
    let n = hosts.len();
    // Track every confirmed log `(owner_idx, peer_call_bytes, dial_hz10)` drained from `pending_save`.
    let mut logged: Vec<HashSet<([u8; 6], Freq10)>> = vec![HashSet::new(); n];
    let mut call_to_idx: std::collections::HashMap<[u8; 6], usize> = std::collections::HashMap::new();
    for (i, h) in hosts.iter().enumerate() {
        let mut cb = [0u8; 6];
        let cn = h.profile.callsign().unwrap().decode(&mut cb);
        let mut key = [0u8; 6];
        key[..cn].copy_from_slice(&cb[..cn]);
        call_to_idx.insert(key, i);
    }

    let mut tx_bursts_total = 0u64;
    let mut tx_cqs = 0u64;
    let mut tx_rxs = 0u64;
    let mut tx_r1s = 0u64;
    let mut co_chan_collisions = 0u64;
    let mut decodes_ok = 0u64;

    // For `mode60` stations, buffer sub-slot bursts across sub=0..4 and deliver at sub=4 finish.
    let mut m60_rx_acc: Vec<Vec<(TxBurst, f64)>> = vec![Vec::new(); n];

    for s in start_slot..(start_slot + n_slots) {
        let utc_sec = s * 10;
        let sub = s % 6;
        let mut bursts: Vec<TxBurst> = Vec::new();

        // 1. Slot start: advance UTC clock and collect all TX bursts.
        for (idx, h) in hosts.iter_mut().enumerate() {
            h.update_clock(utc_sec, 0, 10, ClockSource::Gps, None);
            if let Some((rf_freq, _pwr, frame)) = h.pending_tx.take() {
                h.tx_in_flight = true;
                let dial = h.profile.dial();
                let subch_hz = rf_freq.saturating_sub(dial).saturating_mul(10) as u16;
                let subch_idx = nearest_subch(subch_hz);
                tx_bursts_total += 1;
                match frame {
                    Frame::Cq { .. } => tx_cqs += 1,
                    Frame::Rx { ack: false, .. } => tx_rxs += 1,
                    Frame::Rx { ack: true, .. } => tx_r1s += 1,
                    _ => {}
                }
                bursts.push(TxBurst { tx_idx: idx, dial, subch_hz, subch_idx, frame });
            }
        }

        // 2. Slot end: finish TX stations, compute SINR + capture + decode for RX stations.
        for (rx_idx, h) in hosts.iter_mut().enumerate() {
            let mode60 = h.profile.mode60;
            if h.tx_in_flight {
                h.on_tx_done(true);
                if let Some(saved) = h.pending_save.take() {
                    let mut k = [0u8; 6];
                    let pb = saved.peer_bytes();
                    k[..pb.len().min(6)].copy_from_slice(&pb[..pb.len().min(6)]);
                    logged[rx_idx].insert((k, saved.dial_hz10));
                }
                continue;
            }
            if mode60 && sub == 5 {
                // `:50..:59` cooldown/turnaround guard
                m60_rx_acc[rx_idx].clear();
                continue;
            }

            let rx_dial = h.profile.dial();
            // Gather all signals arriving on `rx_dial` with per-slot QSB fading.
            let mut arriving: Vec<(&TxBurst, f64, f64)> = Vec::new(); // (burst, snr_db, lin_pwr)
            let mut subch_pwr = [0.0f64; 8];
            let mut subch_cnt = [0u32; 8];
            for b in &bursts {
                if b.tx_idx == rx_idx || b.dial != rx_dial {
                    continue;
                }
                if let Some(mean_snr) = path_snr[b.tx_idx][rx_idx] {
                    let snr = mean_snr + rng.gauss(qsb_sigma_db);
                    // Power relative to 2,500 Hz noise floor (`N0 = 1.0`, so `P_200Hz = 10^((snr + 11)/10)`).
                    // Working directly in 2,500 Hz ref scale: `N0_2500 = 1.0`, `P_sig = 10^(snr / 10)`.
                    let p_lin = 10.0f64.powf(snr / 10.0);
                    subch_pwr[b.subch_idx] += p_lin;
                    subch_cnt[b.subch_idx] += 1;
                    arriving.push((b, snr, p_lin));
                }
            }

            if mode60 {
                // Record arriving signals for sub=0..4; at sub=4 evaluate averaged SINR across the 5 sub-slots.
                for (b, _snr, p_lin) in &arriving {
                    let co_other = (subch_pwr[b.subch_idx] - p_lin).max(0.0);
                    let adj_other = (if b.subch_idx > 0 { subch_pwr[b.subch_idx - 1] } else { 0.0 }
                        + if b.subch_idx + 1 < 8 { subch_pwr[b.subch_idx + 1] } else { 0.0 })
                        * 0.002; // matches measured 0.27 dB shift at +15 dB adjacent
                    let sinr_lin = *p_lin / (1.0 + co_other * 12.5 + adj_other);
                    m60_rx_acc[rx_idx].push((
                        TxBurst {
                            tx_idx: b.tx_idx,
                            dial: b.dial,
                            subch_hz: b.subch_hz,
                            subch_idx: b.subch_idx,
                            frame: b.frame,
                        },
                        sinr_lin,
                    ));
                }
                if sub == 4 {
                    // Group by `(tx_idx, frame.pack_u64())` and compute 5-subslot average linear SINR.
                    let mut grouped: Vec<(u64, u16, f64, u32, Frame)> = Vec::new();
                    for (b, sinr_lin) in m60_rx_acc[rx_idx].drain(..) {
                        let key = b.frame.pack_u64();
                        if let Some(g) = grouped.iter_mut().find(|x| x.0 == key) {
                            g.2 += sinr_lin;
                            g.3 += 1;
                        } else {
                            grouped.push((key, b.subch_hz, sinr_lin, 1, b.frame));
                        }
                    }
                    grouped.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());
                    let mut dec_count = 0usize;
                    for (_key, subch_hz, sum_lin, cnt, frame) in grouped {
                        if dec_count >= MAX_DECODED || cnt < 3 {
                            continue;
                        }
                        let avg_sinr_db = 10.0 * (sum_lin / 5.0).max(1e-9).log10();
                        if rng.f01() < decode_prob(avg_sinr_db, true) {
                            dec_count += 1;
                            decodes_ok += 1;
                            h.on_rx_frame(Decoded {
                                frame,
                                audio_hz: subch_hz,
                                snr_db: avg_sinr_db.round().clamp(-24.0, 10.0) as i8,
                                time_offset_ms: 0,
                            });
                            if let Some(saved) = h.pending_save.take() {
                                let mut k = [0u8; 6];
                                let pb = saved.peer_bytes();
                                k[..pb.len().min(6)].copy_from_slice(&pb[..pb.len().min(6)]);
                                logged[rx_idx].insert((k, saved.dial_hz10));
                            }
                        }
                    }
                }
                continue;
            }

            // Standard 10 s v1 slot decode: rank by SINR, cap at `MAX_DECODED = 8`.
            let mut cands: Vec<(&TxBurst, f64)> = Vec::with_capacity(arriving.len());
            for &(b, _snr, p_lin) in &arriving {
                let co_other = (subch_pwr[b.subch_idx] - p_lin).max(0.0);
                // Inside one 200 Hz subchannel, noise floor is `200/2500 = 1/12.5` of 2500 Hz noise,
                // so a co-channel interferer at `P_other` degrades SINR in 2500 Hz units by `12.5 * P_other`!
                let adj_other = (if b.subch_idx > 0 { subch_pwr[b.subch_idx - 1] } else { 0.0 }
                    + if b.subch_idx + 1 < 8 { subch_pwr[b.subch_idx + 1] } else { 0.0 })
                    * 0.002;
                let sinr_db = 10.0 * (p_lin / (1.0 + co_other * 12.5 + adj_other)).max(1e-9).log10();
                if subch_cnt[b.subch_idx] > 1 && sinr_db < -16.5 {
                    co_chan_collisions += 1;
                }
                cands.push((b, sinr_db));
            }
            cands.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            let mut dec_count = 0usize;
            for (b, sinr_db) in cands {
                if dec_count >= MAX_DECODED {
                    break;
                }
                if rng.f01() < decode_prob(sinr_db, false) {
                    dec_count += 1;
                    decodes_ok += 1;
                    h.on_rx_frame(Decoded {
                        frame: b.frame,
                        audio_hz: b.subch_hz,
                        snr_db: sinr_db.round().clamp(-24.0, 20.0) as i8,
                        time_offset_ms: 0,
                    });
                    if let Some(saved) = h.pending_save.take() {
                        let mut k = [0u8; 6];
                        let pb = saved.peer_bytes();
                        k[..pb.len().min(6)].copy_from_slice(&pb[..pb.len().min(6)]);
                        logged[rx_idx].insert((k, saved.dial_hz10));
                    }
                }
            }
        }
    }

    // Compute exact both-ends-logged vs one-sided pairs across all `(i < j, dial)`.
    let mut both_logged_pairs = 0usize;
    let mut one_sided_pairs = 0usize;
    let mut total_logged_records = 0usize;
    for s in &logged {
        total_logged_records += s.len();
    }
    let mut seen_undirected: HashSet<(usize, usize, Freq10)> = HashSet::new();
    for i in 0..n {
        for &(peer_call, dial) in &logged[i] {
            if let Some(&j) = call_to_idx.get(&peer_call) {
                let (a, b) = if i < j { (i, j) } else { (j, i) };
                if seen_undirected.insert((a, b, dial)) {
                    let mut ci = [0u8; 6];
                    let ni = hosts[i].profile.callsign().unwrap().decode(&mut ci);
                    let mut ki = [0u8; 6];
                    ki[..ni].copy_from_slice(&ci[..ni]);
                    if logged[j].contains(&(ki, dial)) {
                        both_logged_pairs += 1;
                    } else {
                        one_sided_pairs += 1;
                    }
                }
            }
        }
    }

    let mut grid_sum = 0usize;
    for h in hosts.iter() {
        grid_sum += h.store.stats(None).unique_grids as usize;
    }

    SimStats {
        tx_bursts: tx_bursts_total,
        tx_cqs,
        tx_rxs,
        tx_r1s,
        co_chan_collisions,
        decodes_ok,
        both_logged_pairs,
        one_sided_pairs,
        total_logged_records,
        unique_grids_avg: grid_sum as f64 / n.max(1) as f64,
    }
}

fn build_sparse_graph(n: usize, density: f64, snr_lo: f64, snr_hi: f64, asym_db: f64, rng: &mut Rng) -> Vec<Vec<Option<f64>>> {
    let mut g = vec![vec![None; n]; n];
    for i in 0..n {
        for j in (i + 1)..n {
            if rng.f01() < density {
                let base = rng.uniform(snr_lo, snr_hi);
                let delta = if asym_db > 0.0 { rng.uniform(-asym_db, asym_db) } else { 0.0 };
                g[i][j] = Some(base + delta * 0.5);
                g[j][i] = Some(base - delta * 0.5);
            }
        }
    }
    g
}

fn main() {
    let mut rng = Rng(0x2026_1120_1050_0001);

    // =========================================================================
    // EXPERIMENT 1: Rendezvous Scaling & Congestion (N = 4..200, one 5-min window)
    // =========================================================================
    println!("=== EXP 1: Rendezvous Scaling & Congestion (5-min :00-:04 window, 25% visibility, SNR -18..+4 dB, QSB ±2 dB) ===");
    println!(
        "{:>5} | {:>9} | {:>10} | {:>9} | {:>10} | {:>10} | {:>9}",
        "N", "2-Way QSO", "QSOs/Radio", "1-Sided%", "CoCh Coll", "TX Bursts", "Decodes"
    );
    println!("{}", "-".repeat(78));
    for &n in &[4usize, 10, 25, 50, 100, 200] {
        let mut hosts: Vec<Box<Rad950Host>> = (0..n).map(|i| make_host(i, 0, false)).collect();
        for h in &mut hosts {
            h.auto_answer = AutoAnswer::AnyNeeded;
            h.rdv.follow_schedule = true;
            h.rdv.call_listen_armed = true;
            h.sync_engine_config();
        }
        let g = build_sparse_graph(n, 0.25, -18.0, 4.0, 2.0, &mut rng);
        // Slot 360 = 01:00:00Z (30 slots = 5-min 10m Rendezvous window)
        let st = run_network(&mut hosts, &g, 360, 30, 2.0, &mut rng);
        let tot_pairs = st.both_logged_pairs + st.one_sided_pairs;
        let one_pct = (st.one_sided_pairs * 100).checked_div(tot_pairs).unwrap_or(0);
        println!(
            "{:>5} | {:>9} | {:>10.2} | {:>8}% | {:>10} | {:>10} | {:>9}",
            n,
            st.both_logged_pairs,
            (st.both_logged_pairs as f64 * 2.0) / n as f64,
            one_pct,
            st.co_chan_collisions,
            st.tx_bursts,
            st.decodes_ok
        );
    }

    // =========================================================================
    // EXPERIMENT 2: Run Mode Pileup Throughput (1 Runner vs K Hunters, 10-min run -> QSOs/hr)
    // =========================================================================
    println!("\n=== EXP 2: Run Mode Throughput (1 Runner Auto-CQ vs K Hunters, 10-min window scaled to QSOs/hr) ===");
    println!(
        "{:>10} | {:>14} | {:>3} | {:>10} | {:>8} | {:>9} | {:>9}",
        "Runner SNR", "Caller SNR", "K", "2-Way/hr", "1-Sided%", "CoCh Coll", "Runner TX"
    );
    println!("{}", "-".repeat(78));
    let caller_profiles: [(&str, f64, f64); 3] = [
        ("QRP -17..-13", -17.0, -13.0),
        ("Mix -17..0", -17.0, 0.0),
        ("Str -10..+5", -10.0, 5.0),
    ];
    for &runner_snr in &[-17.0f64, -15.0, -12.0, -6.0] {
        for &(prof_name, c_lo, c_hi) in &caller_profiles {
            for &k in &[5usize, 20, 60] {
                let n = 1 + k;
                let mut hosts: Vec<Box<Rad950Host>> = (0..n).map(|i| make_host(i, 0, false)).collect();
                hosts[0].engine.start_cq(0); // Runner calling Auto-CQ on even slots
                for h in hosts.iter_mut().skip(1) {
                    h.auto_answer = AutoAnswer::AnyNeeded;
                }
                let mut g = vec![vec![None; n]; n];
                for j in 1..n {
                    g[0][j] = Some(runner_snr);
                    g[j][0] = Some(rng.uniform(c_lo, c_hi));
                }
                // 60 slots = 10 minutes (stays under MEM_LOG_CAP=32)
                let st = run_network(&mut hosts, &g, 100, 60, 1.5, &mut rng);
                let tot = st.both_logged_pairs + st.one_sided_pairs;
                let one_pct = (st.one_sided_pairs * 100).checked_div(tot).unwrap_or(0);
                let runner_tx = hosts[0].store.count(); // Runner logged count in 10 min
                let _ = runner_tx;
                println!(
                    "{:>7.0} dB | {:>14} | {:>3} | {:>10} | {:>7}% | {:>9} | {:>9}",
                    runner_snr,
                    prof_name,
                    k,
                    st.both_logged_pairs * 6,
                    one_pct,
                    st.co_chan_collisions,
                    st.tx_cqs + st.tx_r1s
                );
            }
        }
    }

    // =========================================================================
    // EXPERIMENT 3: 10s (v1) vs 60s (RAD950-60) Both-Ends-Logged QSOs/hr Crossover
    // =========================================================================
    println!("\n=== EXP 3: 10s (v1) vs 60s (RAD950-60) Crossover (1 Runner + 10 Hunters, 30-min run -> QSOs/hr) ===");
    println!("{:>10} | {:>14} | {:>14} | {:>12}", "Mean SNR", "10s v1 QSO/hr", "60s WS QSO/hr", "Winner");
    println!("{}", "-".repeat(60));
    for &snr in &[-21.0f64, -20.0, -19.0, -18.0, -17.0, -16.0, -15.0, -12.0] {
        let mut res = [0usize; 2];
        for (m_i, &m60) in [false, true].iter().enumerate() {
            let n = 11usize;
            let mut hosts: Vec<Box<Rad950Host>> = (0..n).map(|i| make_host(i, 0, m60)).collect();
            hosts[0].engine.start_cq(0);
            for h in hosts.iter_mut().skip(1) {
                h.auto_answer = AutoAnswer::AnyNeeded;
            }
            let mut g = vec![vec![None; n]; n];
            for j in 1..n {
                g[0][j] = Some(snr);
                g[j][0] = Some(snr);
            }
            // 180 ten-second slots = 30 minutes
            let st = run_network(&mut hosts, &g, 120, 180, 1.5, &mut rng);
            res[m_i] = st.both_logged_pairs * 2; // scale 30 min -> QSOs/hr
        }
        let winner = if res[1] > res[0] {
            "60s WS"
        } else if res[0] > res[1] {
            "10s v1"
        } else {
            "Tie"
        };
        println!("{:>7.0} dB | {:>14} | {:>14} | {:>12}", snr, res[0], res[1], winner);
    }

    // =========================================================================
    // EXPERIMENT 4: Asymmetric Paths (20% Base/SDR +8 dB vs 80% Handhelds, N=50)
    // =========================================================================
    println!("\n=== EXP 4: Asymmetric Paths (N=50: 10 Base/SDR +8 dB + 40 Handhelds, 5-min Rendezvous) ===");
    {
        let n = 50usize;
        let mut hosts: Vec<Box<Rad950Host>> = (0..n).map(|i| make_host(i, 0, false)).collect();
        for h in &mut hosts {
            h.auto_answer = AutoAnswer::AnyNeeded;
            h.rdv.follow_schedule = true;
            h.rdv.call_listen_armed = true;
            h.sync_engine_config();
        }
        let mut g = vec![vec![None; n]; n];
        for i in 0..n {
            for j in (i + 1)..n {
                if rng.f01() < 0.30 {
                    let base = rng.uniform(-18.5, -8.0);
                    let bonus_i = if i < 10 { 8.0 } else { 0.0 };
                    let bonus_j = if j < 10 { 8.0 } else { 0.0 };
                    g[i][j] = Some(base + bonus_i);
                    g[j][i] = Some(base + bonus_j);
                }
            }
        }
        let st = run_network(&mut hosts, &g, 360, 30, 2.0, &mut rng);
        let tot = st.both_logged_pairs + st.one_sided_pairs;
        let one_pct = (st.one_sided_pairs * 100).checked_div(tot).unwrap_or(0);
        println!(
            "  2-Way QSOs: {}  |  1-Sided: {} ({}%)  |  CoCh Collisions: {}  |  Decodes: {}",
            st.both_logged_pairs, st.one_sided_pairs, one_pct, st.co_chan_collisions, st.decodes_ok
        );
    }

    // =========================================================================
    // EXPERIMENT 5: Full-Hour 4-Band Rendezvous (N=60, 10m/12m/15m/17m)
    // =========================================================================
    println!("\n=== EXP 5: Full-Hour 4-Band Rendezvous (N=60, 360 slots = 60 min, NewGrid vs NewBandGrid vs AnyNeeded) ===");
    println!(
        "{:>12} | {:>10} | {:>10} | {:>11} | {:>9}",
        "Hunter Mode", "2-Way QSOs", "QSOs/Radio", "Avg Grids", "1-Sided%"
    );
    println!("{}", "-".repeat(64));
    for &(lbl, mode) in &[
        ("NewGrid", AutoAnswer::NewGrid),
        ("NewBandGrid", AutoAnswer::NewBandGrid),
        ("AnyNeeded", AutoAnswer::AnyNeeded),
    ] {
        let n = 60usize;
        let mut hosts: Vec<Box<Rad950Host>> = (0..n).map(|i| make_host(i, 0, false)).collect();
        for h in &mut hosts {
            h.auto_answer = mode;
            h.rdv.follow_schedule = true;
            h.rdv.call_listen_armed = true;
            h.sync_engine_config();
        }
        let g = build_sparse_graph(n, 0.25, -17.0, 2.0, 2.0, &mut rng);
        // Re-arm `call_listen_armed` at the start of each 15-min band window (:00, :15, :30, :45)
        // just as an attended operator would for each Rendezvous window.
        let mut total_both = 0usize;
        let mut total_one = 0usize;
        for w in 0..4u32 {
            for h in &mut hosts {
                h.rdv.call_listen_armed = true;
            }
            let start_s = 360 + w * 90;
            let st = run_network(&mut hosts, &g, start_s, 30, 2.0, &mut rng);
            total_both += st.both_logged_pairs;
            total_one += st.one_sided_pairs;
        }
        let tot = total_both + total_one;
        let one_pct = (total_one * 100).checked_div(tot).unwrap_or(0);
        let avg_grids: f64 = hosts.iter().map(|h| h.store.stats(None).unique_grids as f64).sum::<f64>() / n as f64;
        println!(
            "{:>12} | {:>10} | {:>10.2} | {:>11.2} | {:>8}%",
            lbl,
            total_both,
            (total_both as f64 * 2.0) / n as f64,
            avg_grids,
            one_pct
        );
    }
    let _ = RAD950_DIALS;
}
