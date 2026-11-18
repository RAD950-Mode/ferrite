# SOL950: 10m Weak-Signal Digital Mode Design

**Target Hardware:** Radtel RT-950 Pro running Ferrite  
**Band:** 10 meters (28.000–29.700 MHz, digital/weak-signal segment)  
**Status:** Design proposal (branch `sol950`)

---

## 1. Overview & Motivation

SOL950 is a standalone, FT4-inspired weak-signal digital mode designed specifically around the RF and compute capabilities of the **Radtel RT-950 Pro**:

- **Transmit (BK4829 #1):** Uses the BK4829's **10 Hz** frequency-register resolution (`crates/ferrite-core/src/freq.rs`) to generate direct-RF **4-FSK** on 10m via the HF TX path (`PE14`) and Omron G6K band relay (`PC4`) (`firmware/src/board.rs`). Because it steps the carrier directly rather than modulating FM audio, 100% of TX power stays in a single constant-envelope narrowband signal.
- **Receive (SI4732):** Uses the **SI4732** in **USB mode** (up to 30 MHz, using the owner-imported SSB patch in `crates/ferrite-core/src/ssb.rs`) as a linear receiver. Multiple narrowband signals within a 3.0 kHz passband are downconverted cleanly without FM capture or FM threshold degradation.
- **Timing (GPS):** Uses the built-in GPS receiver (`USART3`) for UTC slot synchronization.
- **DSP (AT32F403A Cortex-M4F):** Fits within Ferrite's existing **13 KB** shared `Decoder` RAM budget (`crates/ferrite-core/src/decoders.rs`) and leverages `digimodes/udsp` FFT/filter primitives.

---

## 2. RF & Physical Layer

| Parameter | Proposed Value | Notes |
| --- | --- | --- |
| **Slot Duration** | **10.0 s** | 6 slots/min (`:00, :10, :20, :30, :40, :50` UTC) |
| **TX / Guard Split** | **8.8 s TX / 1.2 s guard** | 1.2 s covers GPS jitter, `PC4` relay switch, PA ramp-down, and SI4732 RX settle |
| **Modulation** | **4-FSK** (2 bits/symbol) | Constant-envelope direct carrier stepping on BK4829 #1 |
| **Tone Spacing** | **30 Hz** (or **40 Hz**) | Exact multiples of BK4829's 10 Hz step; total width **90–120 Hz** (or **120–160 Hz**) |
| **Symbol Rate** | **12.5 baud** (80 ms/symbol) | 110 symbols in 8.8 s |
| **Frame Structure** | **18 sync + 92 data symbols** | Sync via Costas arrays (time, frequency, and drift estimation) |
| **FEC & Integrity** | **LDPC (rate ~0.42) + 14-bit CRC** | 184 coded bits → **~64 payload bits** + 14-bit CRC (78 bits) |
| **Passband Capacity** | **10+ simultaneous signals** | Inside one 3.0 kHz SI4732 SSB window (`ssb.rs`) |
| **Target Sensitivity** | **~ -16 to -18 dB SNR** | Referenced to 2.5 kHz noise bandwidth |

### Why 30–40 Hz Tone Spacing?
The RT-950 Pro's 26 MHz reference crystal can exhibit tens of Hz of offset/drift at 28 MHz. A 30–40 Hz tone spacing (with Costas-array sync for coarse/fine frequency and drift tracking in the receiver) tolerates handheld crystal drift while remaining narrow enough to stack many signals in a 3 kHz SSB passband.

---

## 3. Protocol: "Mutual RX Report" QSO Model

Traditional FT4/FT8 uses a rigid 4-to-6-exchange sequence (`CQ` → `CALL` → `RPT` → `R+RPT` → `RR73` → `73`) optimized for high-rate contesting. On 10m Sporadic-E, openings often fade in and out over 1–3 minutes. SOL950 uses a **stateless, 2-phase Mutual RX Report** model within a **5-minute rolling window**:

### Frame Types (~64 payload bits + 14-bit CRC)

1. **Type 0 — `CQ` / Beacon Frame**
   - `TYPE` (1 bit: `0`)
   - `MYCALL` (28 bits: standard amateur callsign packing)
   - `GRID4` (15 bits: 4-character Maidenhead grid square)
   - `TX_PWR` (4 bits: quantized dBm / power level)
   - `FLAGS` (16 bits: e.g., mode/auto-reply capability, spare)

2. **Type 1 — `RX` Confirmation Frame**
   - `TYPE` (1 bit: `1`)
   - `MYCALL` (28 bits)
   - `THEIR_HASH` (16 bits: hash of the heard station's callsign)
   - `SNR_DB` (5 bits: quantized SNR report)
   - `FLAGS` (14 bits: e.g., QSO-confirmed bit, spare)

*(Using a 16-bit callsign hash in Type 1 keeps both frames identical in size; full callsigns are already learned from Type 0.)*

### QSO Completion Rule
1. Station A and Station B each transmit `CQ` (`MYCALL GRID4`) on their selected TX slots.
2. When Station A decodes Station B's `CQ`, A adds B to its **5-minute Heard Table** and begins sending `RX <HASH(B)> <SNR>` (or alternating `CQ` and `RX` if multiple callers are active).
3. When Station A has **both**:
   - Decoded B (`CQ` or `RX`) within the last **5 minutes** on the current frequency window, **and**
   - Decoded `RX <HASH(A)>` from B within that same 5-minute window,
   
   **the QSO is complete and logged** on A's side (and symmetrically on B's side).
4. Once logged, Station A places B on a **cooldown** (e.g., 5 minutes) so automated stations do not loop endlessly on the same QSO.

---

## 4. Multi-Frequency Scanning & Operating Modes

### Slot-Aligned Frequency Scanning
Because the SI4732 requires settle time when retuning, frequency scanning occurs strictly **on 10-second slot boundaries**:

- **Scan List:** A configurable list of 3 kHz USB windows on 10m (e.g., `28.180`, `28.185`, `28.190` MHz).
- **Dwell:** 10 s (1 slot) or 20 s (2 slots) per window while idle.
- **Lock-on-Activity:** Upon decoding a `CQ` (or `RX` addressed to `MYCALL`), the scanner **locks onto that window** for up to **5 minutes** (or until the QSO completes) before resuming the scan.

### Operating Modes
- **Monitor (RX-Only):** Scans the window list and logs heard callsigns, grids, and SNRs.
- **Auto-CQ:** Transmits `CQ` on a chosen window (e.g., alternating even/odd slots or pseudo-random slot selection to avoid permanent collisions) and automatically replies with `RX` to heard callers.
- **Pounce / Auto-Reply:** Listens silently across the scan list and only transmits `RX` when a `CQ` is decoded (maximizes battery life and minimizes QRM).

> **Regulatory Note:** Unattended automatic operation on 10m is subject to local amateur licensing rules (e.g., subband restrictions and control-operator presence requirements). The UI should distinguish attended auto-respond from unattended beacon/responder operation.

---

## 5. Ferrite Architecture Integration

1. **Audio / DSP Prerequisite (Milestone 6):**
   - Route **SI4732 analog audio** into `ADC1` / the DSP task (`firmware/src/hal/audio.rs` currently captures BK4829 `EARO` on `PA2` at 48 kHz, decimated to 24 kHz in `crates/ferrite-core/src/audio.rs`).
2. **New Digimode Crate (`digimodes/usol950`):**
   - Uses `digimodes/udsp` for windowed FFTs and Costas sync correlation.
   - Implements the LDPC encoder/decoder and CRC-14.
   - Decoder state must fit within `crates/ferrite-core/src/decoders.rs::BUDGET` (13 KB).
3. **Core State Machine (`crates/ferrite-core`):**
   - Manages the 10-second GPS slot scheduler, 5-minute Heard Table, QSO log, per-callsign cooldowns, and slot-aligned scanner.
4. **Radio Task (`firmware/src/tasks/radio`):**
   - Handles the 1.2 s slot-boundary transition between **SI4732 USB RX** (`SI_IN`, `PC4` relay) and **BK4829 #1 direct-FSK TX** (`PE14` HF TX, `PA5` PA drive).

---

## 6. Bench Verification Checklist (Before Full Implementation)

Before building the full DSP/UI stack, the following hardware behaviors must be verified on a real RT-950 Pro with an SDR / spectrum analyzer:

1. **BK4829 Mid-TX Frequency Stepping:** Confirm that writing the BK4829 frequency registers in 30–40 Hz steps at 12.5 Hz (every 80 ms) while keyed does not cause PLL unlock, spurs, or amplitude dropouts.
2. **28 MHz TCXO Stability:** Measure short-term frequency drift over a 9-second TX burst and across temperature to validate the 30–40 Hz tone spacing and Costas drift-tracking range.
3. **RX↔TX Turnaround Timing:** Measure actual settle time for `PC4` band relay + `PE14` HF TX + SI4732 retune/unmute to confirm the 1.2 s guard interval is sufficient.
4. **SI4732 Audio Path:** Verify SI4732 → `ADC1` sample quality, DC offset, and dynamic range for weak-signal FFTs.
