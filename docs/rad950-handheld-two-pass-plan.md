# RAD950-60 handheld decoder sensitivity plan

Reach the best weak-signal frame recovery the RT-950 Pro can deliver within the firmware's RAM and timing budgets. Keep the RAD950-60 wire format and minute schedule. The current two-pass design is the baseline. If the measurements below show the fold itself costs sensitivity, a different fold or transform is in scope.

Use two comparisons. The desktop full-band dual-pass receiver shows what a second 5-bit fold adds. It shares the handheld's quantized fold, so it cannot show what that fold costs. A host-only floating-point reference supplies that bound.

## Baseline

The handheld already runs two passes from one FFT stream. It retains a 10,875-byte full-band primary fold and 1,236 bytes of tone corrections for the eight nominal channels. The compact pass uses up to two sync timings per channel and makes up to 16 additional LDPC attempts. The primary search covers frequencies between channel centers and includes 2175 Hz.

The Cortex-M4F receiver state occupies 13,164 bytes, leaving 132 bytes in Ferrite's 13 KiB decoder slot after its 16-byte allowance. That figure excludes task stack. The compact pass improved weak-target recovery at 2175 Hz from 15/48 to 30/48 in the Ferrite held-out replay with a neighbor 18 dB stronger. See the [lab notes](rad950-lab-setup.md#handheld-rad950-60-compact-decode-replay) for the conditions and reproduction commands. Physical-radio timing, stack usage, and RF sensitivity remain to be measured.

## 1. Establish the radio's timing, memory budget, and receive-chain sensitivity

Use an instrumented receive-only firmware build. Run controlled audio through the radio's receive path. Buffer counters or timestamps and export them outside the timing-critical path.

- At 24 MHz in Low Power Mode, measure the worst observed service time of the 8 ms audio block that triggers symbol processing, including the FFT and fold updates. Record queue depth and missed or delayed blocks across the five receive subslots. Count ingest overruns separately from ADC half-buffers left unserviced during minute-end decoding. Repeat at the normal clock rate for comparison.
- Measure time from the final audio block to decode completion and readiness for the next `:00`. Record attempt counts and LDPC iterations by pass and measure stack high-water use. Record static receiver, scratch, and stack budgets separately.
- Use quiet audio, noise-only captures, weak signals, and crowded captures. Confirm at least one run reaches all 16 secondary attempts. Repeat across consecutive minutes to exercise reset and turnaround.
- Compare the timings against the actual audio-block cadence and the 10-second turnaround. Use the remaining timing and RAM margin to set budgets before choosing a candidate.

### Measure the receive chain separately from the decoder

Feed a calibrated RAD950-60 signal from a HackRF through a step attenuator into the radio's antenna port. Step the level through the decode threshold and record decode rate against level, for an isolated target and for a target beside a neighbor 18 dB stronger. Sum the target and neighbor digitally in one HackRF waveform so no RF combiner is needed. Confirm the firmware build cannot key the transmitter before cabling the antenna port to the HackRF.

- Capture the radio's raw receive audio over the bench stream during the same runs and replay it through the host decoder. Compare against synthetic audio at the same nominal SNR. The difference is the loss in the receiver IC, AGC, audio filtering, and ADC level, and it is outside the decoder's reach.
- Check the audio passband across 600 to 2325 Hz, the ADC level and clipping margin, and whether the AGC changes gain within a minute when a strong neighbor keys up.
- Retain noise-only radio captures on each rendezvous band. Use them for false-accept runs alongside the synthetic Gaussian noise.
- If the receive chain costs more than the decoder candidates can recover, fix the chain first.

## 2. Measure where the handheld loses decodes

Replay identical 24 kHz mono PCM through primary-only, handheld compact, and desktop full-band dual-pass decoding. Count expected frames recovered, valid frames unique to each receiver, and unexpected frames. Break out weak targets and stronger neighbors. Keep capture and seed sets separate for tuning and final validation.

- Start with the existing 1275 and 2175 Hz fixtures. Include isolated targets, a neighbor 18 dB stronger, aligned and offset burst starts, and crowded captures.
- Sweep callers between nominal centers, including fixed offsets of ±5, ±10, ±15, and ±30 Hz. Give the smaller offsets priority in the decision and retain the larger offsets as stress cases. Separately test frequency ramps across the five bursts. A single summed fold cannot correct in-minute frequency drift across the five bursts; if ramps dominate on-air misses, the fix is transmit-side rather than in the fold, though small drift should be read alongside ±1-bin pooling. Use radio recordings to assess which offsets and drift rates are representative. Include separate half-symbol timing cases so a timing mismatch does not get attributed to frequency drift.
- Add fading, noise-only minutes, and captures with an impulsive static event or overlapping 10-second RAD950 transmission in one of the five subslots. Rotate the affected subslot. Retain clean weak-signal controls.

### Attribute each miss with a replay-only oracle

For each transmitted frame, record whether its matching timing and frequency hypothesis was scored, shortlisted, attempted, successfully decoded, and returned. Log the candidate rank and any suppression, output-cap, or deduplication decision. Distinguish a true lost frame from a duplicate whose payload was already returned.

Force the closest representable timing and frequency hypothesis through the same fold, LLR calculation, LDPC settings, and frame checks, bypassing candidate ranking and output limits. For the compact pass, record whether its retained nominal-channel data can represent the hypothesis. An oracle success on a missed frame points to acquisition, candidate selection, or result handling. An oracle failure directs attention to the stored evidence, soft metrics, or grid mismatch. Test a second FFT timing phase separately so the initial oracle measures the data already retained.

### Bound the achievable threshold with a floating-point reference

Add a host-only reference decoder to the replay. It sums linear power across the five bursts in floating point, evaluates each tone at its exact frequency over the full 80 ms symbol, applies no quantization, and uses the same sync, LDPC code, and frame checks. Sweep SNR in 0.5 dB steps and record the 50% decode threshold for the reference and the handheld receiver on identical inputs.

Then move from the reference to the handheld design one compromise at a time and record the threshold cost of each step in dB:

- log accumulation in place of linear power
- the 0 to 6 per-burst quantizer with its mean-log floor and peak-driven span
- the 70 ms steady-hold window in place of the full symbol
- 11.72 Hz bins with linear interpolation in place of exact tone frequencies
- belief propagation alone in place of a stronger decoder on the same LLRs

The existing review places the handheld's isolated 50% point between −22 and −21 dB. Rough estimates from the code suggest the bin grid costs about 1 dB on average and up to 2.5 dB for a mid-bin tone, and the shortened window about 0.6 dB. Treat these as hypotheses for the ladder to confirm.

Run the ladder on isolated signals in Gaussian noise first for the clean sensitivity bound. Then repeat it with the +18 dB neighbor and with a contaminated subslot. Log accumulation and clipping can beat unclipped linear power under interference, so a step that costs threshold in clean noise may be a deliberate robustness trade. Judge each step on both sets. The ladder ranks sensitivity candidates by measured cost. The oracle ranks acquisition and result-handling candidates.

## 3. Prototype changes driven by the replay

| Candidate | What to test |
| --- | --- |
| Candidate diversity and sync-score ties | If the oracle recovers shortlisted-out frames, look for a strong caller occupying several entries at adjacent frequencies or start times. Compare candidate suppression, a larger shortlist with measured stack and decode costs, and choosing the center of a flat score plateau. Check that suppression preserves distinct nearby callers. |
| Bin-aligned transform | If the ladder shows a meaningful bin-grid or window cost, replay a 480-point transform at 6 kHz. Its 12.5 Hz bins put tones exactly 4 bins and channels exactly 18 bins apart, and it integrates the full symbol. The 480 real samples can use a 240-point complex transform (16 × 3 × 5). Without zero-padding, a tone midway between bins (±6.25 Hz) loses about 3.9 dB, against about 2.6 dB for the current zero-padded grid, so the gain holds only for callers close to nominal. On-air offsets of −7 to +11 Hz have been observed, so the ±5 and ±10 Hz sweeps decide this candidate. Measure the isolated threshold, the off-nominal offsets, and the strong-neighbor cases, since the ramp samples enter the window. The fold needs about 140 bins, which would free roughly 700 bytes. Measure the Cortex-M4F cost of a mixed-radix transform against the current radix-2 one and confirm bit-exact host and target output. The 480-sample buffer could be transformed in place, which interacts with the FFT workspace row below. |
| Stronger decoding after belief propagation | If the ladder shows a decoder gap and Step 1 shows idle minute-end time, test ordered-statistics decoding or additional LLR scalings on candidates that fail belief propagation. Measure the threshold gain, the stack needed, the worst-case decode time, and the false-accept rate, which rises with every added attempt. |
| A-priori callsign decoding | For directed frames, constrain additional decode attempts with the known local callsign and, where the QSO state supplies it, the peer callsign. Preserve normal frame and CRC checks. Measure the threshold gain on directed frames. Measure the false-accept rate with wrong peers, unrelated traffic, and noise, since pinned bits weaken the CRC-14. |
| Combining across retransmitted minutes | First establish whether a peer's retried frame is bit-identical. If it is, test adding the stored LLRs of a failed directed candidate to the next minute's attempt. Measure the bytes retained outside the decoder slot and the false-accept rate. |
| Half-symbol timing | If misses track burst timing, compare a second FFT phase in replay. Measure the persistent state, stack, and ingest cost of retaining or evaluating that phase. |
| Local normalization in a single fold | Compare global scaling with local floors and spans. Test hard sub-band boundaries and smoothly blended estimates. Sweep the full four-tone signal across boundaries, since its outer tone centers are 150 Hz apart. Evaluate local normalization first as a replacement for the current primary-plus-compact design, which would free the 1,236-byte correction table. If testing it with the compact pass retained, report that configuration separately. Measure scratch usage and the added per-symbol work. |
| Wider compact frequency coverage | If most misses cluster near nominal centers, test same-size ±1-bin tone pooling before adding stored offsets. Because taking the max of three bins raises the noise floor in the stored corrections, check for isolated weak-signal regression alongside sidelobe pickup and changes in strong-neighbor recovery. Compare bytes and cycles per additional valid frame. Validate any tighter quantization against clean weak signals as well as interference. |
| Fold precision versus coverage | If the oracle shows that coverage is limiting recovery, replay a 4-bit primary fold with per-burst contributions capped at 3. It frees 2,175 bytes for wider residual coverage or another timing hypothesis. Measure the loss in soft-metric resolution before spending the saved bytes. |
| FFT workspace reuse | If ingest stack is limiting, evaluate replacing the 480-sample `pcm_ring` with a persistent 512-sample FFT buffer (+64 bytes static, leaving 68 bytes of slot margin, and removing the 1,024-byte stack `z` array and copy). Re-zero samples `420..512` before each transform because in-place `cfft` overwrites its input, verify decimation and windowing see identical samples, and note that this assumes single-phase symbol framing (incompatible with half-symbol overlap). |
| Impulse and short-burst protection | The primary fold already caps each cell's contribution at 6 per burst. Compare the current behavior with tighter clipping or down-weighting when one subslot is contaminated. Check that clean weak-signal recovery remains acceptable. |
| Two full folds from one FFT | Drop this candidate if the oracle shows the relevant misses are primarily in acquisition or result handling. If a meaningful gap persists across the band and points to lost soft information, audit Ferrite's linked RAM layout for space available throughout reception. Two 10,875-byte folds need 21,750 bytes before receiver state, scratch, and stack. If the RAM is available, prototype both folds from the same FFT. |

Two changes save processing time and add no sensitivity. Take them up only if Step 1 shows a timing deficit.

- **Secondary sync gate:** log which candidates a threshold would skip while still decoding them. Use noise-only replay to propose thresholds, then use held-out weak and interfered captures to measure valid frames that would be lost. Measure radio savings before enabling a gate.
- **FFT stage-maximum tracking:** the host experiment was bit-exact but slower than separate scans. Compare Cortex-M4F cycles and bit-exact output before adopting it.

## 4. Decide what to ship

Prioritize the radio timing, stack, and receive-chain measurements, the floating-point reference ladder, and the oracle replay. Let the ladder's per-step costs and the observed loss stages determine whether the transform, the decoder, candidate handling, local normalization, coverage, or timing gets prototyped first. The host replay can rank candidates by recovery, but final selection of any candidate that spends stack or decode time (a larger shortlist, stronger decoding, a-priori attempts, a mixed-radix transform, or a second FFT phase) waits until Step 1 establishes the on-radio budgets.

Pre-committed acceptance criteria (fixed before running the ladder and oracle sweeps; only RAM, stack, and timing margins are deferred to Step 1):

- **Metric:** the 50% decode threshold in dB, from an SNR sweep in 0.5 dB steps on held-out inputs. Estimate it with one fixed method, a logistic fit across the transition, for every receiver and condition. Use at least 200 paired trials per SNR step within 1 dB of the knee so the estimate's Monte Carlo error stays well below the 0.2 dB regression limit, and report its confidence interval. A percentage-point change at one SNR depends on where it sits on the curve, so report it only as supporting detail.
- **Minimum worthwhile gain:** a threshold improvement of at least 0.5 dB on the isolated weak-signal condition or on the target interference condition, with the paired counts at the nearest swept SNR significant at `p < 0.05` (two-sided McNemar's test on independent matched pairs). A meaningful RAM, stack, or cycle reduction with no threshold loss also qualifies.
- **Tolerated regression:** at most 0.2 dB of threshold loss, and not statistically significant, on isolated clean weak-signal controls and on nominal-channel strong-neighbor (+18 dB) recovery.
- **False-accept bound:** 0 unexpected frames in at least 1,000 independent noise-only minutes (one-sided 95% upper bound ≤ 0.30% per minute under that noise model) and 0 unexpected frames on held-out signal captures. Re-evaluate this bound for any candidate that adds attempts or uses prior information (for a-priori decoding, include wrong-peer and unrelated-traffic tests).
- **Hardware margins (set from Step 1):** zero ingest overruns at 24 MHz LPM, minute-end decode ready comfortably before the next `:00`, and documented static RAM and task-stack headroom.

Evaluate candidates on held-out captures. Compare paired outcomes on identical inputs and report both gains and losses. Use McNemar's test for independent matched pairs. Report conditions separately or account for seed clustering when multiple conditions reuse the same noise. Report unexpected frames and independent noise-only minutes alongside recovery counts (for reference, zero unexpected frames in the baseline 656 noise-only minutes gives a one-sided 95% upper bound of about 0.46% per minute). Confirm promising candidates on the radio under the same ingest, turnaround, RAM, and stack checks as the baseline, and repeat the conducted sensitivity run to confirm the threshold gain through the real receive chain.

Put portable algorithm changes and replay tests in the MIT `urad950` crate in RAD950-Mode/rad950. Keep radio instrumentation, task scheduling, and memory placement in Ferrite. If no candidate meets the checks, retain the current compact decoder and document the measured limits.

## 5. Measured Results & Decisions (2026-11-21)

### Step 1: RT-950 Pro timing, memory, and receive-chain measurements

- **120 MHz receive-only bench build (`ferrite-rx-bench-120`, full worst-case 10 primary + 16 compact LDPC attempts × 35 iterations = 910 iterations):**
  - Worst 8 ms audio-block service time (`RAD950_WORK_HI`): **`3.565 ms`** (`44.6%` of 8 ms cadence).
  - Worst minute-end decode (`RAD950_FINISH_MAX`): **`736.5 ms`** (`7.4%` of 10 s turnaround; `729.5 ms` last).
  - Active-ingest overruns (`RAD950_LATE_IN`): **`0`** across `17,628` blocks (`~141 s`); minute-end overruns (`RAD950_LATE_FIN`): **`0`**.
  - Linked static RAM: **`68,124 B` / `98,304 B`** (`30,180 B` main stack budget); peak main stack used: **`25,812 B`**; high-water free margin: **`4,368 B` (`4.27 KiB`)**.
  - Fixed-24 MHz (`clock-24`) build hit a watchdog reset (`RESTARTED = HANG`) during mode entry/operation; normal builds keep 120 MHz active via `clock::for_audio()` (`0` blocks at 24 MHz during RAD950-60).
- **Conducted SI4732 USB receive chain (`28.078 MHz` USB, `AVC ON`, `SI_VOLUME = 56`, `84 dB` inline attenuation from HackRF):**
  - RF-off ADC noise floor: `RMS = 3,652`, `peak = 15,280 / 32,767` (`-6.6 dBFS`), `DC = -4.4`, `0` clips.
  - 9-tone equal-amplitude passband comb (`600..2325 Hz`): flat within `3.6 dB`, zero upper-band roll-off (`2175 Hz: +0.92 dB`, `2325 Hz: +0.64 dB` vs. `1275 Hz`).
  - `+18 dB` neighbor AGC pumping (`1950 Hz` keying beside `2175 Hz`): AVC pulls gain down `10–13 dB` cleanly (`0` clips, `~18.7 dB` ratio preserved, `~1.5 s` recovery).
  - Linearity (`tx_x = 0, 8, 16 dB`): `SNR_2500 = -7.97, +0.04, +8.12 dB` (`0.05 dB` error across `16 dB`).
  - Real-radio conducted `WP2Z` capture (`~-18.5 dB` `SNR_2500`, `1957 Hz`): `CompactReceiver60` decoded `4/4` on `0 ms`-aligned phases and `0/4` on `+40 ms` half-symbol phases, confirming the half-symbol timing cliff on real hardware.

### Step 2: 200-trial/SNR floating-point ablation ladder & oracle (`urad950/examples/sensitivity_study.rs`)

- **Largest single sensitivity step:** `q6_70ms_exact` → `hh_prim_oracle` (`512`-pt grid + Q4 linear interpolation) = **`+0.58` to `+0.83 dB`** (`p < 0.0001`).
- **Rectified `0..6` quantizer vs. unclipped log:** **improves** sensitivity by `0.35–0.53 dB` (and prevents collapse under impulsive subslots where unclipped linear power gets `0%`).
- **70 ms steady-hold window vs. full 80 ms:** costs `0.08–0.22 dB` (not significant at knee).
- **OSD-1 after BP:** recovers `0.10–0.15 dB` (`p ≤ 0.015`), below the `0.5 dB` bar on its own.
- **`CompactReceiver60` vs. desktop dual-pass:** `CompactReceiver60` beats desktop dual-pass on all four ladder conditions (`-21.23` vs. `-21.10 dB` isolated, `-20.84` vs. `-20.57 dB` `+18 dB` neighbor, and `48/64` vs. `0/64` on 5-caller crowded), triggering the kill criterion for two full folds on the handheld (also independently ruled out by the `4,368 B` stack margin).

### Step 3: Prototype outcomes & shipped decisions

- **Rejected:**
  - Plain 480-pt transform (`+1.22` to `+1.92 dB` mid-bin scalloping regression at `+5/+6 Hz`).
  - `±1`-bin compact pooling (`+0.63 dB` regression on `nbr18_2175_0Hz`).
  - Single-fold local/fixed-span normalization replacing global peak + compact (strong-caller regression `1400 → 943`, crowded `166 → 93`, aligned `103 → 23`).
  - 4-bit primary fold (`+0.28 dB` isolated, `+0.84 dB` strong-neighbor regression; also missed real conducted `WP2Z` at `1957 Hz`).
- **Shipped:**
  - **Handheld (`urad950::rx60`):** Retain `CompactReceiver60` (`13,164 B`, 5-bit primary + `1,236 B` compact) inside Ferrite's `13 KiB` slot, with the zero-cost **`sup_only`** shortlist dedup (`|Δstart_sym| ≤ 1 && |Δf| ≤ 4` at `MAX_CAND = 10`: `-0.26 dB` / `+104` frames on `nbr18_2175_+5Hz`, `0` losses across `4,200` trial-minutes, `0/1000` noise-only FAR, `0 B` extra RAM/stack/CPU).
  - **Desktop (`rad950-node`):** Added dual-phase (`0 ms` + `+40 ms`) support to `Receiver60` (`set_half_symbol_phase`) and `decode_minute_60_two_pass`, eliminating the `±30..40 ms` half-symbol timing cliff across the full band.
