# RAD950 Protocol & Operating Specification (v1)

Normative specification of the **RAD950** (`usol950` / `SOL950`) HF weak-signal protocol, Run / Hunter operating modes, Hourly UTC Rendezvous, and Spots API as implemented and over-the-air verified across two independent reference implementations:
- **Embedded handheld firmware:** Ferrite on Radtel RT-950 Pro (`digimodes/usol950`, `crates/ferrite-core/src/sol950.rs`, `crates/ferrite-app/src/sol950.rs`, `crates/ferrite-app/src/rad950_rdv.rs`, `firmware/src/tasks/ui.rs`).
- **Host SDR transceiver / skimmer:** `digimodes/usol950/examples/rad950_hackrf.rs` (HackRF One, 2.4 MS/s USB DDC).
- **Web Spots API:** `rad950-web/src/spots.ts`.

---

## 1. Band Plan & USB Dial Frequencies

All transmissions are **Upper Sideband (USB)** (or equivalent direct-PLL 4-FSK carrier placement at `dial_hz + f_audio`).

| Band | Primary Calling / Rendezvous Dial (USB) | Secondary Dials (USB) |
|---|---|---|
| **10 m** | **28.078 MHz** | 28.082, 28.120, 28.180 MHz |
| **12 m** | **24.919 MHz** | 24.923 MHz |
| **15 m** | **21.078 MHz** | 21.082 MHz |
| **17 m** | **18.104 MHz** | 18.108 MHz |

---

## 2. Physical Layer (PHY) & Slot Timing

### 2.1 UTC Slot Grid & Clock Discipline
- **Slot duration:** `10.000 s` aligned to UTC (`utc_sec % 10 == 0`).
  - **Even slots:** `(utc_sec / 10) % 2 == 0` (`:00`, `:20`, `:40`).
  - **Odd slots:** `(utc_sec / 10) % 2 == 1` (`:10`, `:30`, `:50`).
- **TX burst timing:**
  - Nominal start: **`+400 ms`** (`start_sym = 5`) into the 10 s slot.
  - Duration: **`8.80 s`** (`110` symbols × `80 ms`), ending at `+9.20 s`.
  - RX search window: `start_sym ∈ [1, 9]` (`+80 ms` to `+720 ms`, i.e. **`±320 ms`** around nominal).
  - RX slot decode flush: **`+9.55 s`** (`finish_slot()`). After any TX slot, the DSP receiver MUST be reset at `0.00 s` of the following RX slot so muted TX audio cannot lock out RX acquisition.
- **TX clock gate (`MAX_TX_UNCERT_MS = 150 ms`):**
  - Scheduled TX is permitted only when `utc_sec != 0` and estimated timing uncertainty `<= 150 ms`.
  - **Live GPS (`GPS PNT` / `GPS CLK`):** Disciplined to the NMEA `$` burst edge (`50 ms` base jitter).
  - **Crystal holdover (`GPS HOLD`):** After any valid onboard GPS fix, the MCU's external `HEXT` quartz crystal holds UTC with a conservative `50 ppm` (`1 ms / 20 s`) drift budget, remaining valid (`<= 150 ms`) for **33.3 minutes** (`2,000 s`) if GPS signal drops or GPS sleeps.
  - **Bench fallback (`BENCH CLK`):** Compile-time `bench` builds only; release builds require GPS lock or active holdover (`NO GPS` otherwise).

### 2.2 Modulation & Passband Layout
- **Modulation:** Phase-continuous **4-FSK**, tone spacing **`50 Hz`**, symbol period **`80 ms`** (`10 ms` raised-cosine ramp + `70 ms` steady tone).
- **Gray dibit-to-tone mapping:** `(b0, b1)` → tone `0..3`: `00 → 0`, `01 → 1`, `11 → 2`, `10 → 3` (`GRAY_ENCODE = [0, 1, 3, 2]`).
- **8 TX subchannels (`SUBCHANNELS_HZ`, lowest tone `f0` above USB dial):**
  - `600, 825, 1050, 1275, 1500, 1725, 1950, 2175 Hz` (`225 Hz` step = `150 Hz` tone span / `~200 Hz` occupied bandwidth + `25 Hz` guard; Rendezvous table uses `625..2200 Hz`, `+25 Hz` shifted).
- **Continuous RX search:** `585.9–2,332.0 Hz` (`150` FFT bins at `11.72 Hz`/bin, `6,000 Hz` decimated processing rate), up to **`8` simultaneous tracks/decodes** (`MAX_TRACKS = MAX_DECODED = 8`) per slot, with `±11.72 Hz` linear frequency-drift tracking across the burst tail.

### 2.3 Sync & Interleaving
- **110 symbols total:** `18` sync symbols + `92` data symbols (`184` coded bits).
- **Sync placement (`6 + 6 + 6`):**
  - `S0` at symbols `0..6`: `[0, 3, 0, 1, 3, 2]`
  - `S1` at symbols `18..24`: `[3, 0, 3, 2, 0, 1]`
  - `S2` at symbols `104..110`: `[3, 1, 0, 2, 3, 0]`
- **Interleaver:** Stride-`37` bit permutation across the `184` coded bits (`gcd(37, 184) = 1`), spreading adjacent codeword bits `~3 s` apart against fading.

---

## 3. FEC & 64-Bit Wire Frame Format

### 3.1 CRC-14 & LDPC(184, 78)
- **CRC-14:** Computed over the 64-bit payload (`CRC14_POLY = 0x2757`, FT8/FT4 family) → `K = 64 + 14 = 78` systematic message bits.
- **LDPC:** Systematic `(184, 78)` PEG LDPC code (`M = 106` parity checks, girth 6, zero 4-cycles, bridged dual-diagonal parity with `MID_ROW = 53`; decoded with integer normalized min-sum BP, `scale = 3/4`, up to `35` iterations).

### 3.2 Field Encodings
- **28-bit amateur callsign (`0 ..= 262,177,559`):** Standard 6-slot `[C0, C1, C2, C3, C4, C5]` with call-area digit at `C2`:
  - `C0`: `' '`, `'A'..'Z'`, `'0'..'9'` (37) · `C1`: `'A'..'Z'`, `'0'..'9'` (36) · `C2`: `'0'..'9'` (10) · `C3..C5`: `' '`, `'A'..'Z'` (27 each).
- **15-bit Maidenhead `grid4`:** `0..=32,399` (`AA00..RR99`); `32,400` (`GRID_UNKNOWN`) = unset/no grid.
- **5-bit `snr_db`:** `[-24, +7] dB` in `1 dB` steps (`code = clamp(snr_db, -24, 7) + 24`, 2,500 Hz reference bandwidth).
- **4-bit `tx_pwr`:** `0` = unspecified, `1..=15` = quantized TX power code.

### 3.3 Frame Layouts (MSB = bit 63)

| Frame | Bit Layout (64 bits, MSB → LSB) | Notes |
|---|---|---|
| **`Frame::Cq`** (Type 0) | `1b type=0` \| `28b sender` \| `15b grid4` \| `4b tx_pwr` \| `16b reserved` | **`reserved & 0x0F`** encodes optional numeric callsign suffix (`0` = none, `1..=15` = `-1`..`-15`, e.g. `NQ6N-1`). |
| **`Frame::Rx`** (Type 1) | `1b type=1` \| `28b sender` \| `28b recipient` \| `5b snr` \| `1b ack` \| `1b reserved` | `ack=0` is initial report (`RX`); `ack=1` is confirmed report (`R+RX`). Carries base 28-bit callsigns only (no suffix). |
| **`Frame::ExtFrag`** | `1b type=1` \| `6b marker=0b111111` \| `2b kind` \| `2b idx` \| `8b tag` \| `45b payload` | Conversation / Distress extension fragments; rejected as invalid callsign (`>= 0x0FC0_0000`) by core `Rx` unpackers. |

---

## 4. QSO State Machine & Operating Modes

### 4.1 Core QSO Invariants
1. **Strict alternating TX/RX turns:** After any TX slot `s`, the station MUST listen during slot `s + 1` (`next_tx >= s + 2`).
2. **Opposite-parity reply rule:** When answering a station last heard in slot `s_heard`, the reply MUST be transmitted on parity `(s_heard + 1) % 2` so replies never collide with an Auto-CQ caller's TX slots.
3. **Self-base-call suppression:**
   - CQs with the same base callsign and a *different* `-1..-15` suffix (`sender == my_call && suffix != my_suffix`) are decoded and shown in Heard.
   - Directed QSOs and auto-answers strictly require `sender != my_call` (a station never works its own base callsign).
4. **3-slot QSO completion & causal `ACK=1`:**
   - **Caller (CQer):** `Slot N`: TX `CQ` → `Slot N+1`: RX `RX(ACK=0)` from responder → `Slot N+2`: TX `R+RX(ACK=1)` → **log QSO upon confirmed clean TX completion** (`confirm_tx_completed`).
   - **Responder:** `Slot N`: RX `CQ` → `Slot N+1`: TX `RX(ACK=0)` → `Slot N+2`: RX `R+RX(ACK=1)` → **log QSO only if `ACK=1` causally follows a completed `RX(ACK=0)` sent to that peer within `EXPIRY_SLOTS = 30` (5 min)**. Cold/unsolicited `ACK=1` frames are ignored.
5. **Post-Completion Linger & Cooldown:**
   - After sending `ACK=1`, linger on the window for `LINGER_SLOTS = 2` slots (20 s) so a faded `ACK=1` can be retried (up to `MAX_COOL_RETRIES = 2` within `COOLDOWN_SLOTS = 30`, never writing a duplicate log entry).
   - Unanswered attempts stop after `MAX_TRIES = 4`.
6. **Hardware relay guard (`SAFE-22`):** On handheld hardware where TX/RX switches an RF relay, at most `MAX_TX_PER_MIN = 3` TX slots per rolling `6`-slot (`60 s`) window are permitted.
7. **Busy-subchannel avoidance:** Any subchannel decoded during an RX slot is marked occupied; `pick_subchannel()` chooses an unoccupied subchannel out of the 8 for the next TX.

### 4.2 Operating Modes
- **Monitor (`OpMode::Monitor`):** RX-only; decodes all frames into the Heard list with live award classification (`★ NEW` = unworked grid all-time, `+ BND` = unworked grid on current band, `✓ WKD` = worked).
- **Run / Auto-CQ (`OpMode::AutoCq { parity }`):** Calls `CQ` on slots matching `parity` (even or odd) whenever no directed peer reply is pending; automatically replies `RX(ACK=0)` to incoming CQs and `R+RX(ACK=1)` to incoming `RX(ACK=0)` answers.
- **Manual Answer (`OpMode::Pounce`):** Operator selects a heard station and triggers `Answer`; queues opposite-parity `RX(ACK=0)` (or `R+RX` if the peer already sent `RX(ACK=0)`).
- **Attended Hunter / Auto-Answer (`AutoAnswer`):**
  - Modes: `Off` → `NewGrid` (`HUNT:NEW`) → `NewBandGrid` (`HUNT:BND`) → `AnyNeeded` (`HUNT:ALL`).
  - Priority when multiple CQs decode in the same slot: **`NewGrid` > `NewBandGrid` > `NewCall` > highest SNR**.
  - When armed and idle in `Monitor`, automatically answers matching CQs (and replies `R+RX` if called directly), logs the completed QSO, and immediately releases back to `Monitor` to hunt the next needed station.
  - Never duplicates an already-worked callsign on the same band (or an already-worked grid when restricted to `NewGrid` / `NewBandGrid`).

---

## 5. Hourly UTC Rendezvous Mode

Published 5-minute (`30`-slot) meetup windows every hour so low-power portable stations find each other without internet spotting:

| UTC Minutes | Band | USB Calling Dial |
|---|---|---|
| **`:00` – `:04`** | **10 m** | `28.078 MHz` |
| **`:15` – `:19`** | **12 m** | `24.919 MHz` |
| **`:30` – `:34`** | **15 m** | `21.078 MHz` |
| **`:45` – `:49`** | **17 m** | `18.104 MHz` |

### 5.1 Rendezvous Rules
1. **Schedule following (`follow_schedule`):** Automatically retunes to the active enabled band when its 5-minute window opens. Retuning is **deferred while a directed QSO is in progress** (`has_active_qso == true`).
2. **Independent 50/50 Call & Listen (`call_listen_armed`):**
   - Only **even slots** (`slot % 2 == 0`) are eligible for Rendezvous CQs; **odd slots** are always `Listen` (reserved for directed replies).
   - Seed: `seed = (call.raw() ^ ((suffix as u32) << 24)).wrapping_mul(0x9E37_79B1) ^ 0x5A3C_9E37` (or `0x1357_9BDF` if 0).
   - Turn draw: `r = mix32(seed ^ slot.wrapping_mul(0x9E37_79B1))`; transmit CQ iff `(r & 1) == 1`.
   - Subchannel pick: `mix32(r ^ 0x85EB_CA6B) % clear_count` over unoccupied subchannels.
   - `mix32(x)`: `x ^= x >> 16; x *= 0x7FEB_352D; x ^= x >> 15; x *= 0x846C_A68B; x ^ (x >> 16)`.
   - `call_listen_armed` automatically clears when the 5-minute window closes.
3. **Answering Rendezvous CQs:** When a Rendezvous station in `Monitor` receives an incoming `RX(ACK=0)` addressed to `my_call`, it promotes out of `Monitor` to transmit `R+RX(ACK=1)`, logs the QSO, and returns to `Monitor` so it can answer subsequent callers or resume Rendezvous turns.

---

## 6. Live Spotting API (`POST /api/spots`)

Skimmers and host SDRs (`rad950_hackrf --spot`) upload decoded slots asynchronously to `POST /api/spots` (default `https://rad950.com/api/spots`):

- **Authentication:** `Authorization: Bearer <r950_...>` (personal skimmer key from `POST /api/spots/key`, SHA-256 hashed server-side) or signed-in browser session.
- **Request body:** JSON `{"spots": [ ... ]}` (up to `100` spots per batch):
  ```json
  {
    "spots": [
      {
        "spotter_call": "NQ6N",
        "spotter_grid": "EN61",
        "tx_call": "W2BBB",
        "tx_grid": "FN31",
        "utc_sec": 1795154400,
        "freq_hz": 28079050,
        "band": "10m",
        "snr_db": -6,
        "tx_pwr_code": 10,
        "frame_type": "CQ"
      }
    ]
  }
  ```
- **Rules:**
  - `freq_hz = dial_hz + audio_hz`; `frame_type` is `"CQ"`, `"RX"`, or `"R+RX"`.
  - `spotter_grid`, `tx_grid`, and `tx_pwr_code` are omitted when unknown/`0`.
  - Self-base-call spots (`base(tx_call) == base(spotter_call)`) are skipped client-side and ignored server-side.
