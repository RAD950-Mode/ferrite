# SOL950 (v2): 10m Weak-Signal Digital Mode Design

**Target Hardware:** Radtel RT-950 Pro running Ferrite  
**Band:** 10 meters (28.000–29.700 MHz, digital/weak-signal segment)  
**Status:** Design Specification v2 (branch `sol950`, incorporating [SOL950 v1](sol950-design.md) and [Design Review](sol950-review.md))

---

## 1. Overview & Hardware Mapping

SOL950 is a standalone, slot-synchronized 10m weak-signal digital mode engineered specifically around the hardware constraints and existing driver infrastructure of the **Radtel RT-950 Pro**:

- **Transmit — BK4829 #1 Direct-PLL 4-FSK (Hardware Baseline) & Smooth Host Reference:**
  - Bench testing (Experiment 2) eliminated both **frozen Tone 1 (`REG_71 = 0`)** (zero RF modulation on BK4829) and **MCU `DAC1` (`PA4 -> PC12 -> BK4829 mic`)** (`~22 Hz` / `~7.2 ms` PCB AC-coupling high-pass capacitor, preventing `80 ms` DC holds).
  - Instead, hardware TX steps BK4829's `10 Hz`-resolution PLL (`REG_38`/`REG_39` = `0, +50, +100, +150 Hz`) and latches each tone change via `REG_30` VCO toggle (`sol950_bk_freq`, paced non-blockingly at `80 ms` symbol boundaries by `TMR5` / `EDGE`, saving and restoring BK4829 registers at unkey; `crates/ferrite-radio/src/chip.rs`, `firmware/src/tasks/radio/afskbk.rs`). Conducted 10 m TX measured `[49.984, 50.010] Hz` tone spacings, `80.0104 ms` symbols, `+0.12 Hz` drift over `8.72 s`, brief `< 0.5 ms` VCO relock transients, and clean decode (`CQ NQ6N FN31`, `+8 dB` SNR).
  - The phase-continuous `10 ms` raised-cosine ramp + `70 ms` steady-hold trajectory (§2.3, `digimodes/usol950/src/phy.rs`) is retained as the **host software reference waveform** (`0.1–0.2 dB` of hardware PLL stepping in AWGN replay).
- **Receive — SI4732 Linear USB at 28 MHz (`PA2` / `ADC1`):**
  - Uses the **SI4732** in **USB mode** (valid up to 30 MHz with the owner-imported SSB patch, `crates/ferrite-core/src/ssb.rs`).
  - Bench testing already confirmed that SI4732 receive audio is summed onto **`PA2` (`ADC1` ch 2)** alongside the BK4829s (`firmware/src/tasks/radio/debug.rs`, Oct 25, 2026). While Ferrite's interactive HF screen currently switches from SI4732 to BK4829 at 18 MHz (`firmware/src/tasks/radio/hf.rs`), SOL950 introduces an explicit 28 MHz SI4732 USB receive mode with both BK4829s muted/asleep during RX so multiple weak signals across a 3.0 kHz passband are captured linearly without FM threshold or capture effects.
- **RF Path & Relay Guard (`SAFE-22` & `TX-37`):**
  - All pin transitions run strictly through `crates/ferrite-core/src/path.rs` (break-before-make, `PA5` zeroed before switching).
  - Because `Rx::Si` requires the Omron G6K band relay (`PC4`) **released** (`false`) while 10m TX (`Sel::HfTx`) requires `PC4` **energized** (`true`), and `Path::key_up` refuses to move the relay (`TX-37`), every TX burst executes:
    1. `Path::rx(Rx::Bk(Sel::HfRx), Cause::HfReceiver, ...)` → mutes, clears `SiInput`, energizes `PC4`, waits `RELAY_SETTLE_MS` (10 ms).
    2. `Path::key_up(Sel::HfTx, ...)` → `PaZero`, clears `HfRx`, sets `HfTx` (`PE14`), `ChipTx(1)`, `PaRamp`.
    3. `Path::unkey(...)` → `PaZero`, waits `UNKEY_WAIT_MS` (100 ms), returns to `HfRx`.
    4. `Path::rx(Rx::Si, Cause::HfReceiver, ...)` → clears `HfRx`, releases `PC4` (10 ms), restores `SiInput`.
  - Each TX slot therefore consumes **2 relay switches**. Because `GUARD_SWITCHES` allows at most **10 relay switches per rolling 60,000 ms** (`SAFE-22`), SOL950 enforces a hard ceiling of **≤ 3 TX slots per rolling 60 seconds** (≤ 6 relay switches/min), leaving 4 switches of margin for mode entry/exit from another band.
- **Timing — ATGM336H GPS (`USART3`):**
  - The RT-950 Pro wires `USART3` (`PB10`/`PB11` at 9,600 baud) and `PA8` power to the ATGM336H without a hardware PPS pin, and `firmware/src/tasks/gps.rs` polls the UART ring every `50 ms` (`POLL_MS`).
  - SOL950 timestamps the start-of-burst (`$` after `> 400 ms` UART idle) against the MCU monotonic clock and explicitly carries **±150 ms per-station UTC uncertainty (±300 ms / ±3.75 symbols relative skew)** into both slot guard timing and receiver sync search (§2.5, §5).
- **Compute & Memory — AT32F403A Cortex-M4F:**
  - Uses a streaming 6 kHz / 512-point FFT + candidate-track architecture (`digimodes/udsp`) that fits strictly inside `crates/ferrite-core/src/decoders.rs::BUDGET` (**13,312 bytes**).

---

## 2. RF, Frequency Conventions & Physical Layer

### 2.1 Antenna-Referenced Window & Non-Overlapping Subchannel Grid
- **Window Definition:** A SOL950 window is defined by its **USB suppressed-carrier dial frequency** $f_{\text{dial}}$ (e.g., `28.180`, `28.185`, `28.190` MHz).
- **Usable Audio Passband & BFO Sign:** **`600 Hz` to `2,400 Hz`** above $f_{\text{dial}}$ (keeping clear of low-frequency audio roll-off near zero beat and upper-edge droop on the SI4732's 3.0 kHz SSB filter). Gatekeeper Experiment 1 settled the USB `SSB_BFO` sign (`+100 Hz` BFO lowers zero-beat RF by `100 Hz` and raises USB audio by `+100 Hz`), and `crates/ferrite-core/src/ssb.rs` now negates the sub-kHz remainder accordingly (`LSB` retains the same carrier-offset sign pending LSB bench confirmation; per-radio crystal calibration is separate).
- **Non-Overlapping Subchannel Grid (Anti-Collision):**
  - At $\Delta f = 50\text{ Hz}$ tone spacing, one 4-FSK signal occupies `4 × 50 = 200 Hz` (tone centers at $+0, +50, +100, +150\text{ Hz}$ above its lowest tone $f_{\text{off}}$). A 50 Hz subchannel grid would cause adjacent callers to share three of their four tone centers.
  - Nominal subchannels use a uniform **`225 Hz`** spacing across `600–2,400 Hz`—giving **8 nominal subchannels** at $f_{\text{off}} \in \{600, 825, 1050, 1275, 1500, 1725, 1950, 2175\}\text{ Hz}$ (a `75 Hz` / `1.5`-tone guard between adjacent 4-tone groups, costing only `~0.27 dB` in replay with a `+15 dB` captured Radtel neighbor).
  - Example: On dial `28.185000` MHz, subchannel 2 ($f_{\text{off}} = 1,050\text{ Hz}$) radiates tone centers at `28.186050`, `28.186100`, `28.186150`, and `28.186200` MHz.
  - Responder subchannel index is chosen from clear subchannels observed in the prior RX slot or hashed from `(MYCALL, slot_index)`. Even with nominal 225 Hz subchannel steps, the receiver searches all FFT bins continuously and is tested against an unaligned stronger neighbor displaced by **1 tone spacing (`±50 Hz`)**.

### 2.2 Physical Layer Parameters

| Parameter | Baseline v2 Value | Notes / Verification |
| --- | --- | --- |
| **Slot Duration** | **10.00 s** | 6 slots/min (`:00, :10, :20, :30, :40, :50` UTC) |
| **Nominal First-Symbol Time** | **+0.40 s after UTC slot edge** | Even a peer `300 ms` early starts at `+0.10 s` (never clipped at `0.00 s`) |
| **Active Burst / Guard** | **8.80 s TX / 1.20 s total guard** | `0.40 s` pre-guard + `8.80 s` burst (110 syms) + `0.80 s` post-guard = `10.00 s` |
| **Max TX Duty Cycle** | **≤ 3 TX slots / rolling 60 s** | ≤ 6 relay switches/min (`SAFE-22` limit is 10/min) |
| **Modulation** | **4-FSK** (2 bits/sym, Gray `00,01,11,10`) | Hardware baseline: BK4829 PLL stepping (`80 ms` hold, `< 0.5 ms` relock); host ref: `10 ms` RC + `70 ms` hold (§2.3) |
| **Symbol Rate** | **12.5 baud** ($T_s = 80\text{ ms}$) | Exact 10 × 8 ms DSP audio blocks per symbol (`480` samples @ 6 kHz) |
| **Tone Spacing ($\Delta f$)** | **50 Hz** *(sweep 30 / 40 / 50 Hz)* | At $T_s = 80\text{ ms}$, 50 Hz is $4 / T_s$; sweep 30/40/50 Hz on bench/sim |
| **Frame Symbols** | **110 symbols** (`18 sync + 92 data`) | **`6 + 6 + 6`** 4-ary sync clusters at `0..6`, `18..24`, and `104..110` (§2.4) |
| **FEC & Integrity** | **(184, 78) LDPC + CRC-14** | 64 payload bits + 14-bit CRC = 78 systematic bits + 106 parity bits (rate `0.424`) |

### 2.3 Host Software Reference Waveform Trajectory
For host simulations, synthetic test vectors, and reference WAV generation (`digimodes/usol950/src/phy.rs`):
- Each symbol $n \in [0, 109]$ lasts $T_s = 80\text{ ms}$ with target tone index $t_n \in \{0, 1, 2, 3\}$ and instantaneous frequency offset $f(t)$ above $f_{\text{dial}} + f_{\text{off}}$:
  - **Transition interval ($\tau \in [0, T_{\text{ramp}})$, $T_{\text{ramp}} = 10\text{ ms} = T_s / 8$):**
    $$f(n T_s + \tau) = \Delta f \cdot \left( t_{n-1} + (t_n - t_{n-1}) \cdot \frac{1 - \cos(\pi \tau / T_{\text{ramp}})}{2} \right)$$
    (with $t_{-1} = t_0$ on symbol 0 so the burst starts cleanly on $t_0$ after PA ramp-up).
  - **Steady interval ($\tau \in [T_{\text{ramp}}, T_s)$, $70\text{ ms} = 7 T_s / 8$):**
    $$f(n T_s + \tau) = \Delta f \cdot t_n$$
- Phase $\phi(t) = 2\pi \int_0^t f(u)\,du$ is continuous across all 110 symbols. On RT-950 Pro hardware, direct BK4829 `REG_38/39` PLL stepping replaces the `10 ms` ramp with a single `< 0.5 ms` VCO relock transition per tone change (§1), performing within `~0.1 dB` of this smooth reference in AWGN replay (§7).

### 2.4 Exact `6 + 6 + 6` Four-Tone Sync Sequences
Literal order-7 Costas arrays require 7 distinct tones. For 4-FSK (`tones 0..3`), exhaustive search over all $4^6 = 4,096$ sequences selects three balanced, non-repeating 6-symbol words (each containing all 4 tones, zero adjacent repeated tones $w_i \neq w_{i+1}$ for sharp fractional-symbol timing peaks, and optimal 2D auto-ambiguity $\le 1$ across all $(\Delta t, \Delta f) \neq (0, 0)$):

- **`S0` (symbols `0..6`):** `[0, 3, 0, 1, 3, 2]`
- **Data block 0 (symbols `6..18`):** 12 data symbols (24 coded bits)
- **`S1` (symbols `18..24`):** `[3, 0, 3, 2, 0, 1]`
- **Data block 1 (symbols `24..104`):** 80 data symbols (160 coded bits)
- **`S2` (symbols `104..110`):** `[3, 1, 0, 2, 3, 0]`

**Ambiguity Properties:**
- **Aligned frequency-shift rejection ($\Delta t = 0, \Delta f \in \{\pm 1, \pm 2, \pm 3\}$):** **0 / 18** hits.
- **Joint early-acquisition (`S0 + S1`, 12 symbols):** Peak = **12**; max 2D sidelobe across all $(\Delta t \in [-24, +24], \Delta f \in [-3, +3]) \neq (0, 0)$ is **2 / 12** (including at $\Delta f = \pm 1$ tone spacing).
- **Full-frame (`S0 + S1 + S2`, 18 symbols):** Peak = **18**; max 2D sidelobe across all $(\Delta t, \Delta f) \neq (0, 0)$ is **3 / 18**; pairwise cross-ambiguity between `S0`, `S1`, `S2` is **≤ 2 / 6**.

### 2.5 Guard Budget & NMEA UTC Uncertainty
- **Clock anchoring:** In `USART3` RX (`firmware/src/hal/uart.rs` / `firmware/src/tasks/gps.rs`), latch the monotonic timestamp of the first `$` character after `> 400 ms` of UART idle, subtracting the bench-measured ATGM336H NMEA emission delay.
- **TX Gate:** Scheduled TX is permitted only when total local UTC uncertainty is **`≤ ±150 ms`** (accounting for NMEA burst jitter + MCU crystal drift during GPS standby holdover).
- **End-of-Slot Guard Budget (`1.20 s` total between nominal end `t = 9.20 s` and next nominal start `t = 10.40 s`):**
  - Even if a late peer arrives `+150 ms` relative to UTC (`+300 ms` relative to an early local clock, ending at `t = 9.50 s`), **`900 ms`** remains before `t = 10.40 s`—sufficient for serial LDPC decode (`≤ 250 ms`), RF/relay transition (`≤ 350 ms`), and scanner SI4732 retune (`≤ 250 ms`).

---

## 3. Frame Packing & Bit-Level Specification

Both frame types carry **64 payload bits + 14-bit CRC = 78 bits**, encoded to **184 LDPC coded bits** (92 4-FSK data symbols). Using two full 28-bit callsigns in `RX` frames allows a silent pouncer to reply cold without ever having transmitted a `CQ`.

| Frame | Bit Layout (MSB-first, 64 payload bits) | Total |
| --- | --- | ---: |
| **Type 0 (`CQ`)** | `TYPE = 0` (1b) \| `SENDER` (28b) \| `GRID4` (15b) \| `TX_PWR` (4b) \| `RESERVED` (16b) | **64 b** |
| **Type 1 (`RX`)** | `TYPE = 1` (1b) \| `SENDER` (28b) \| `RECIPIENT` (28b) \| `SNR` (5b) \| `ACK` (1b) \| `RESERVED` (1b) | **64 b** |

### Field Definitions
- **`SENDER` / `RECIPIENT` (28 bits each):** Standard 28-bit amateur callsign encoding (6-character slot `[C0..C5]` aligned around the area digit at `C2`: `C0` in `' ', 'A'..'Z', '0'..'9'` [37], `C1` in `'A'..'Z', '0'..'9'` [36], `C2` in `'0'..'9'` [10], `C3..C5` in `' ', 'A'..'Z'` [27³]; total $37 \times 36 \times 10 \times 27^3 = 262,177,560 < 2^{28}$). Non-standard/compound calls outside this space are explicitly rejected in UI setup.
- **`GRID4` (15 bits, `0..32767`):** `0..32399` = `AA00`–`RR99`; `32400` = explicit **"Grid Unknown"** sentinel; `32401..32767` = reserved.
- **`TX_PWR` (4 bits, `0..15`):** `0` = unknown; `1..15` = `0 dBm` to `+42 dBm` in `3 dB` steps.
- **`SNR` (5 bits, `0..31`):** SNR referenced to **2,500 Hz** noise bandwidth, clipped to **`[-24 dB, +7 dB]`** in `1 dB` steps (`code = clamp(round(snr_db), -24, 7) + 24`).
- **`ACK` (1 bit in Type 1):**
  - `0` = **`RX` (Initial Report):** *"I decoded you; here is your SNR."*
  - `1` = **`R+RX` (Confirmed Report):** *"I decoded your report addressed to me; here is your SNR."*
- **Worked Reference Test Vectors (Frozen in `digimodes/usol950`):**
  1. **`CQ` (`NQ6N`, `FN31`, `tx_pwr = 10`, `reserved = 0`):**
     - `payload_u64`: `0x30DD_C102_85BA_0000` · `crc14` (`poly 0x2757`, `init 0x3FFF`): `0x0648`
     - `tones[110]`: `03013201033000332130320110001201300301003132130333202301032213222031210100301212221033100130032203101110310230`
  2. **`RX` (`K1ABC -> NQ6N`, `snr_db = -14`, `ack = 0`, `reserved = 0`):**
     - `payload_u64`: `0x80F2_C50B_0DDC_1028` · `crc14`: `0x2375`
     - `tones[110]`: `03013233032031213030320122013131212101030023013320101000213221002133313103202003203320130130210302123202310230`

---

## 4. Contact Protocol, Loss Recovery & Scanner Linger

### 4.1 Three-Slot Contact Exchange

| Slot | Transmission | Evidence at Receiving Station |
| --- | --- | --- |
| **0** | **A** sends `CQ(A, grid, pwr)` | **B** decodes A's `CQ` and queues a directed report to A. |
| **1** | **B** sends `RX(sender=B, rcpt=A, snr, ACK=0)` | **A** decodes B's `RX` addressed to A; A now owes B an `R+RX`. |
| **2** | **A** sends `RX(sender=A, rcpt=B, snr, ACK=1)` | **B** decodes A's `R+RX` (`ACK=1`) → **B logs QSO**. **A logs QSO** once Slot 2's TX finishes unkeying cleanly. |

### 4.2 State Tracking, Bounded Retries & Cooldowns
For each `(peer_call, window_freq)` entry in the **5-minute (`300 s`) Session Table**:
1. **Separate Local Evidence:**
   - `rx_report_at`: Timestamp when a valid `RX` (`ACK=0` or `ACK=1`) **addressed to `MYCALL`** from `peer_call` was decoded.
   - `tx_report_done_at`: Timestamp when our `RX`/`R+RX` addressed to `peer_call` **finished transmitting and unkeyed** (queued or aborted TX never sets this).
2. **Local Log Rule (Write-Once per Cooldown):** Log QSO once when both `rx_report_at` and `tx_report_done_at` are valid within `300 s` **and** either:
   - We decoded `ACK=1` (`R+RX`) from `peer_call` after `tx_report_done_at`, **or**
   - We decoded `ACK=0` (`RX`) from `peer_call` and subsequently completed transmitting `ACK=1` (`R+RX`) to `peer_call`.
3. **Retry Limits & Priority:**
   - Unanswered outbound `RX`/`R+RX` attempts cap at **`MAX_TRIES = 4`** before dropping the session.
   - During the 5-minute post-log cooldown, if `peer_call` repeats `RX(ACK=0)` (indicating our `ACK=1` was lost), we answer with `ACK=1` up to **`MAX_COOL_RETRIES = 2`** times without writing a duplicate log entry, then stay silent toward `peer_call`.
   - Owed directed `RX`/`R+RX` replies always preempt fresh `CQ`s (subject to the `≤ 3 TX / 60 s` relay guard ceiling).

### 4.3 Scanner Rendezvous & Post-Completion Linger Rule
- **2-Slot Minimum Dwell (`20 s`):** Idle scanning dwells **2 consecutive slots** per window so every visited window observes both even and odd TX parities.
- **Activity Lock:** Decoding an actionable `CQ` or an `RX` addressed to `MYCALL` locks the scanner on that window.
- **Post-Completion Linger (Resolving Unlock vs. Lost `ACK=1`):**
  - If Station A sends its final `ACK=1` in Slot $k$, logs locally, and immediately unlocks to scan another window in Slot $k+1$, A would miss B's Slot $k+1$ retry if Slot $k$ faded—leaving A logged and B unlogged.
  - **Rule:** After completing `ACK=1` in Slot $k$, A **remains locked on the same window for a mandatory 2-slot (`20 s`) linger** covering B's immediate retry slot ($k+1$, opposite parity) and one full cycle.
    - If B does **not** repeat `RX(B→A, ACK=0)` during the linger window, A unlocks the scanner and moves on.
    - If B **does** repeat `RX(B→A, ACK=0)` in Slot $k+1$, A transmits `ACK=1` in Slot $k+2$ (consuming 1 of `MAX_COOL_RETRIES = 2`) and resets the 2-slot linger—repeating until B goes quiet or `MAX_COOL_RETRIES` is exhausted.
- **Required Protocol Verification Matrix:** Automated tests across a multi-window scan list must assert expected logs at **both** radios for: (a) clean 3-slot exchange, (b) Slot 0 erased, (c) Slot 1 erased, (d) Slot 2 (`ACK=1`) erased while scanning is enabled, (e) two simultaneous Slot 1 callers, and (f) sustained complete path loss after Slot 1 (asserting bounded retries and zero duplicate logs).

---

## 5. Streaming Receiver Architecture (With ±3.75-Symbol Skew Inside 13,312 Bytes)

Two stations each within `±150 ms` of UTC can disagree by **`±300 ms` (`±3.75` symbols)**. The streaming receiver accommodates **`±4` integer symbols** plus half-symbol fractional phases while staying inside `decoders::BUDGET` (**13,312 bytes**):

1. **4:1 Decimation (`24 kHz → 6 kHz`):**
   - Decimates incoming `8 ms` (`192`-sample) DSP blocks 4:1 to **6,000 Hz** (`48` samples/block; **480 samples per 80 ms symbol**).
2. **Half-Symbol Sliding 512-Point Real FFT & Block-Exponent Scaling:**
   - Every `40 ms` (half-symbol = `240` new samples at 6 kHz; phase `p ∈ {0, 1}`), windows the most recent **480 samples** and zero-pads 32 samples into a **512-point real FFT** (`m = 256` in `digimodes/udsp/src/fft.rs`, `11.72 Hz` bin spacing). Advancing by `240` samples per half-symbol (`480` per symbol) guarantees zero timing slip across all 110 symbols.
   - Magnitudes are rescaled by `1 << shift` from `udsp::fft::cfft` before storage/quantization so a loud neighbor never distorts weak-signal soft metrics via block-exponent shifts.
3. **30-Symbol Early-Acquisition Ring Buffer (`±4` Symbol Skew):**
   - `S0` (`0..6`) and `S1` (`18..24`) span 24 symbols. Allowing arrival skew $\Delta k \in [-4, +4]$ symbols around nominal start (`t = 0.40 s`, symbol 5 of the 10 s slot) means `S1` finishes no later than slot-symbol `5 + 24 + 4 = 33`, while the earliest `S0` began at slot-symbol `5 - 4 = 1`.
   - Retaining a **30-symbol × 2-phase** ring buffer over the `550–2,450 Hz` bins (`162` bins × `1 B` log-magnitude × `30` symbols × `2` phases = **`9,720 bytes`**) captures the entire `S0 + data[0..12] + S1` span for every timing offset in `[-320 ms, +320 ms]`.
4. **Candidate Tracking & Shared-Scratch Decode:**
   - As each `(phase, start_sym)` hypothesis completes `S1`, the 2D `S0 + S1` correlator admits top candidates into **`K = 8` tracks** (`184 B` soft LLRs + `24 B` state = **`1,664 bytes`** total), backfilling `data[0..12]` from the ring buffer and streaming `data[12..92]` + `S2` live.
   - **Admission Fairness:** At least 2 of the 8 tracks are reserved for local sync peaks separated by `≥ 150 Hz` from stronger admitted tracks so sidelobes or a `+20 dB` caller cannot starve a weak caller.
   - **Memory Total:** `9,720 B` (ring / reused at end-of-slot for LDPC BP scratch + FFT workspace) + `1,664 B` (tracks) + `~1,200 B` (decimator + PCM hop ring) ≈ **`12.6 KB < 13,312 B`**.

---

## 6. Ferrite Codebase Touchpoints

| Component | Path | Responsibility |
| --- | --- | --- |
| **Digimode Crate** | `digimodes/usol950` | Frame codec, CRC-14, `(184, 78)` LDPC, `6+6+6` sync, reference TX PCM/IQ waveform, 13 KB streaming receiver |
| **Decoder Slot** | `crates/ferrite-core/src/decoders.rs` | `Decoder::Sol950` variant within `BUDGET` (13,312 B); continuous capture across quiet slots |
| **Protocol & Scanner** | `crates/ferrite-core/src/sol950.rs` | 10 s UTC scheduler, 3-slot QSO state machine, `≤ 3 TX / 60 s` limiter, 2-slot scan dwell + 2-slot Post-Completion Linger |
| **Radio & GPS Tasks** | `firmware/src/tasks/radio/`, `gps.rs` | 28 MHz SI4732 USB RX bench/mode hook, `Path` relay sequencing, non-blocking TX trajectory pacing, NMEA `$` timestamp |

---

## 7. Gatekeeper Experiments

1. **28 MHz SI4732 Receive Route, USB BFO Sign & Through-Radio RX Sweep (`docs/sol950-lab-setup.md`):**
   - Verified SI4732 USB RX at 28.185 MHz with complete `CQ NQ6N FN31` host decodes and settled the USB `SSB_BFO` sign (`ssb::tune()` updated accordingly).
   - Paired host-vs-`PA2` sensitivity sweep (`-14, -15, -16 dB`, 8 slots/point): `alone` decoded `7/8, 5/8, 2/8` on Host vs `7/8, 2/8, 2/8` through SI4732 `PA2`; with a `+15 dB` captured Radtel neighbor `+225 Hz` away, Host decoded `8/8, 4/8, 0/8` vs `4/8, 2/8, 0/8` through SI4732 `PA2` (`0x9012` AVC pumping between slots + `~1 dB` receive noise/tilt; exact `16k -> 24k` sinc resampling matched linear interpolation).
2. **Overlapping-Caller Blind Acquisition & Captured-TX Subchannel Sweep — PASSED:**
   - Host `usol950` passes overlapping-caller blind acquisition (`+18 dB` neighbor at `225 Hz` and `+15 dB` at `200 Hz`), `Receiver` fits in `<= 13,312 B`, and 100-trial AWGN replay (`sol950_wav sweep`) puts the 50% threshold at `-15.38 dB` (captured Radtel alone) / `-15.37 dB` (synthetic weak alone) / `-15.10 dB` with a `+15 dB` captured Radtel neighbor `+225 Hz` away (`0.27 dB` penalty), justifying retention of the 8-channel `225 Hz` grid. Closing the remaining `0.6 dB` (to `-16 dB`) / `2.6 dB` (to `-18 dB`) decoder gap and tuning SI4732 AVC/volume remain open.
3. **28 MHz BK4829 PLL TX Trajectory (`docs/sol950-lab-setup.md`) & NMEA UTC Jitter:**
   - Conducted 28.18605 MHz TX decoded `CQ NQ6N FN31 (+8 dB)` with `[49.984, 50.010] Hz` tone spacings, `80.0104 ms` symbols, `+0.12 Hz` drift over `8.72 s`, and `291 Hz` (`±500 Hz`) / `427 Hz` (`±2 kHz` noise-subtracted) 99% OBW (`REG_38/39` + `REG_30` VCO toggle). `USART3` `$` burst-start jitter vs. GPS PPS (including `$PCAS12` standby wake) remains open.
4. **Target Replay & Multi-Window Erasure Matrix (Pending):**
   - Replay crowded WAVs on AT32F403A to verify zero missed 8 ms blocks and guard-window completion, and run the full §4.3 single-erasure + sustained-loss multi-window scan matrix.
