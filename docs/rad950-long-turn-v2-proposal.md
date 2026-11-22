# RAD950 Long-Turn v2: Feasibility & Evaluation Proposal

This note evaluates whether a new long-turn wire format (**RAD950-60 v2**) is justified to improve handheld weak-signal sensitivity and timing resilience within Ferrite's `13 KiB` (`13,312 B`, `13,296 B` usable after the `16 B` allowance) decoder slot.

## 1. Decision Principle: Separate Timing Recovery from a New Wire Format

The v1 handheld measurements ([rad950-handheld-two-pass-plan.md](rad950-handheld-two-pass-plan.md#5-measured-results--decisions-2026-11-21)) established two key facts:

1. **v1 `CompactReceiver60` (`13,164 B`) is already a strong baseline on aligned `0 ms` signals** (`-21.23 dB` isolated, `-20.84 dB` beside an `+18 dB` neighbor, beating the desktop dual-pass receiver on all four ladder conditions, `0` ingest overruns at `120 MHz`, `4,368 B` stack margin).
2. **v1's single-phase `80 ms` FFT has a real timing cliff at `±30..40 ms (mod 80 ms)`** (confirmed on both host replay and conducted RT-950 Pro SI4732 audio). Desktop `rad950-node` now eliminates this cliff with full-band dual-phase (`0 ms` + `+40 ms`), but two full-band `10,875 B` v1 folds do not fit in Ferrite's `13 KiB` slot (or in the remaining `4,368 B` of SRAM stack margin).

**Rule for justifying v2:**
- Keep waveform choice and v1 timing recovery as separate decisions.
- If a v2 waveform only recovers v1's `40 ms` timing loss, retaining v1 `CompactReceiver60` on the handheld (with desktop dual-phase) is the better engineering trade—because v1 preserves interoperability with `RAD950-10` and avoids splitting the ecosystem.
- A new long-turn v2 mode is justified **only if** it delivers a repeatable, statistically significant sensitivity gain over v1 at **equal transmit power, equal total airtime (`~44 s` inside a 60 s minute), and equal useful payload (`64` payload bits + `14`-bit CRC-14)**, fits strictly inside `13,296 B` with measured stack and 8 ms-block headroom, and survives the RT-950 Pro's measured TX frequency drift.

---

## 2. Fixed Handheld Slot Budget

Every candidate must account for **all** persistent state inside `13,296 B`:

| Component | Budget | Notes |
|---|---:|---|
| Ferrite `Decoder` slot usable limit | `13,296 B` | `13,312 B` slot minus `16 B` allowance |
| Non-fold state (decimator, ring buffer, waterfall, metadata) | `~1,050–1,500 B` | Ring buffer size scales with symbol length and decimated sample rate; must be counted explicitly per candidate |
| **Available for folds / evidence tables** | **`~11,800–12,240 B`** | Must include all timing guard symbols, all stored phases, and any secondary contrast table |

---

## 3. Direction 1 (Primary): Narrow-Search Long-Symbol v2

A narrow weak-signal rendezvous sub-band (`200–400 Hz`, analogous to WSPR's `200 Hz` window in Ferrite's `uwspr`) gives the most direct memory advantage: the receiver retains evidence only where the mode listens, freeing enough RAM for both symbol phases and higher soft-metric resolution inside `13 KiB`.

### Two Concrete Candidates (Equal `44.0 s` Airtime & `64 + 14`-bit Payload)

Both candidates reuse the existing `(184, 78)` LDPC code (`92` 4-FSK data symbols) + `18` sync symbols = `110` symbols (`FOLD_SYMS = 116`, covering 6 coarse start-symbol slots):

| Parameter | **B1: Semi-Coherent `200 ms` (2× repeats)** | **B2: Coherent `400 ms` (1× burst)** | **v1 Baseline (`80 ms`, 5× repeats)** |
|---|---:|---:|---:|
| Symbol duration / baud | `200 ms` / `5.0 baud` | `400 ms` / `2.5 baud` | `80 ms` / `12.5 baud` |
| Tone spacing / 4-FSK BW | `20` or `25 Hz` / `80–100 Hz` | `10` or `12.5 Hz` / `40–50 Hz` | `50 Hz` / `200 Hz` |
| Burst duration × repeats | `22.0 s × 2` (`44.0 s` TX in `:00..:49`) | `44.0 s × 1` (`44.0 s` TX in `:00..:49`) | `8.8 s × 5` (`44.0 s` TX in `:00..:49`) |
| Search window / bin width | `300 Hz` (`60` bins @ `5 Hz`) | `200 Hz` (`80` bins @ `2.5 Hz`) | `1,758 Hz` (`150` bins @ `11.72 Hz`) |
| Decimated rate / ring buffer | `3,000 Hz` / `600` samp (`1,200 B`) | `1,500 Hz` / `600` samp (`1,200 B`) | `6,000 Hz` / `480` samp (`960 B`) |
| **Dual-phase fold RAM** | **`2 × 116 × 60 × 5b = 8,700 B`** (`0` & `+100 ms`) | **`2 × 116 × 80 × 4b = 9,280 B`** (`0` & `+200 ms`) | `1 × 116 × 150 × 5b = 10,875 B` (`0 ms` only) |
| Est. total persistent state | **`~10,200 B`** (`~3,100 B` spare) | **`~10,780 B`** (`~2,500 B` spare) | `13,164 B` (`132 B` spare w/ compact) |
| Half-symbol timing step | `±100 ms` (`50 ms` = 1/4 sym) | `±200 ms` (`50 ms` = 1/8 sym) | `±40 ms` (`40 ms` = 1/2 sym cliff) |

### Hypotheses to Verify (Not Assumed)

- **Net sensitivity at equal airtime:** Longer coherent symbols reduce non-coherent combining loss relative to `5 × 8.8 s` folding, but actual net gain depends on acquisition, sync overhead, quantization, and FEC. Measure on the same equal-airtime, equal-power replay ladder used in Step 2.
- **Handheld TX frequency drift (mandatory hardware gate):** Narrow tone spacings (`10–25 Hz`) are sensitive to BK4829 thermal/crystal drift over a `22 s` or `44 s` key-up. Measure the RT-950 Pro's actual frequency trajectory across full `22 s` and `44 s` key-ups (at full and reduced PA power) before committing to any tone spacing. Notice **B1 (`200 ms`, `2 × 22 s`)** halves continuous key-up duration and doubles tone spacing relative to B2, making it a natural middle ground if `44 s` continuous drift is too large.
- **Timing tolerance:** Longer symbols widen the timing window, but `50 ms` is still `1/8` of a `400 ms` symbol (and `1/4` of a `200 ms` symbol). Both B1 and B2 therefore budget **two phases** (`0` and half-symbol) from the start.

---

## 4. Direction 2 (Fallback for Frequency Tolerance): Wide-Spaced `8-FSK` Short-Burst v2

If measured BK4829 TX drift rules out narrow tones, a wide-spaced (`50 Hz`, `80 ms` symbol) `8-FSK` short-burst format retains full frequency-drift tolerance while shrinking `FOLD_SYMS` enough to fit two full-band folds.

### Explicit Frame & RAM Arithmetic

- **Why the existing `(184, 78)` codeword does not fit:** At 3 coded bits per 8-FSK symbol, `184` coded bits require `62` data symbols. Keeping `18` sync symbols yields `80` symbols (`6.4 s`), and two `80 × 150` 5-bit folds take **`15,000 B`** before timing guard symbols, ring buffer, or decoder state—exceeding `13,296 B`.
- **What *does* fit:** To fit two full-band 5-bit folds (`N_BINS = 150`) plus `~1,050 B` of non-fold state inside `13,296 B`, `FOLD_SYMS` must stay `≤ 64` (`2 × 64 × 150 × 5 / 8 = 12,000 B`, total `~13,050 B`, leaving `~246 B` margin).
  - With `6` timing-guard symbols, that allows **`58` burst symbols (`4.64 s`)**, repeated `9×` (`41.8 s` airtime inside `:00..:49`).
  - Example complete frame budget: a new **`(144, 78)` rate-0.542 LDPC** codeword (`48` 8-FSK data symbols) + `10` sync symbols = `58` symbols.
- **Hypotheses & tradeoffs to test:**
  - Moving from rate-`0.424` `(184, 78)` to rate-`0.542` `(144, 78)` trades away FEC coding gain against any non-coherent 8-FSK modulation advantage. Net threshold at equal airtime (`~44 s`) must be measured—not assumed.
  - Each 8-FSK channel occupies `400 Hz` (`8 × 50 Hz`), halving channel capacity across the passband (`4` channels vs. `8`).
  - Leaves almost zero margin (`~246 B`) for a secondary strong-neighbor table unless passband width is also trimmed.

---

## 5. Proposed Work Plan & Decision Gates

1. **Parallel Track A — v1 Timing Check (no wire-format change):**
   - Document the smallest viable v1 second-phase representation (or confirm that v1 handheld stays single-phase `13 KiB` while desktop runs dual-phase `+40 ms`, as currently implemented).
2. **Parallel Track B — Hardware TX Drift Measurement (`ronald` + HackRF):**
   - Measure the RT-950 Pro BK4829 TX instantaneous frequency trajectory over `22 s` and `44 s` key-ups at full and low PA power (cold start and warm soak). This sets the minimum viable tone spacing and symbol duration.
3. **Parallel Track C — Equal-Airtime Host Simulation (`urad950`):**
   - Implement B1 (`200 ms`, `2 × 22 s`), B2 (`400 ms`, `1 × 44 s`), and—if drift requires wide tones—the `(144, 78)` 8-FSK candidate in a host replay harness.
   - Compare all candidates against v1 `CompactReceiver60` at **identical TX power, `44.0 s` total airtime, and `64 + 14`-bit payload**, injecting the **measured BK4829 drift**, signed timing offsets (`±50 ms`), `+18 dB` neighbors, and `1,000` independent noise-only minutes.
4. **Ship / Kill Gate:**
   - **Proceed to v2** only if a candidate achieves a repeatable **`≥ 1.5 dB`** 50% threshold gain over v1 at equal airtime (`p < 0.05` McNemar, `0/1000` noise-only FAR), survives the measured handheld TX drift trajectory, and fits inside `13,296 B` with healthy on-radio timing and stack margins.
   - **Retain v1** if the gain is marginal or only recovers the `40 ms` timing loss.
