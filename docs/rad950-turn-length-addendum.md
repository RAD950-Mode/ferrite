# RAD950 turn-length and frequency presets

**Proposed v3 addendum.** Give operators a small list of USB dial frequency presets, each with a named turn length. Keep 10 seconds as the main calling preset, add a 30-second weak-signal experiment, and evaluate 60 seconds for sustained paths. Leave room for a fourth preset, with 120 seconds and 7.5 seconds serving different purposes. **Every turn length supports both ordinary contacts and Run mode.** Frequency, turn length, and whether the operator is running a pileup are independent settings. [Current air interface](rad950-design-v2.md) · [Run mode proposal](rad950-run-mode-addendum.md)

## Candidate profiles and link budget

The baseline sends 110 symbols in **8.8 s** in a 10 s UTC turn. The table assumes the same average power during each burst and a receiver that combines aligned evidence without loss. Gains are reductions in the input SNR required at the same decode probability, referenced to **2,500 Hz** noise bandwidth and to one current 8.8 s burst. They are planning estimates. Actual decode gains need measurement.

| Turn | Example burst pattern | Active waveform time | Ideal sensitivity change | Three-turn slot budget | Ideal Run contacts/min, two callers per batch |
| --- | --- | ---: | ---: | ---: | ---: |
| 10 s | One existing frame | 8.8 s | 0 dB | 30 s | 6 |
| 30 s | Three copies of the same frame | 26.4 s | +4.8 dB | 90 s | 2 |
| 60 s | Six copies of the same frame | 52.8 s | +7.8 dB | 3 min | 1 |
| 120 s | Twelve copies of the same frame | 105.6 s | +10.8 dB | 6 min | 0.5 |
| 7.5 s | Shorter or faster waveform within a 6.3 s active budget | Up to 6.3 s | −1.45 dB energy change at 6.3 s | 22.5 s | 8 by timing alone, 6 under the current rolling TX cap |

The Run column assumes an ideal pipeline with one runner TX every other turn and a full batch confirmed on each runner TX. CQ announcements, initial requests, retries, and missed reports reduce that rate. Four-caller batches would double the ceiling where the receiver and batch encoding support them. A longer turn can still produce more *completed* contacts at a signal level where the 10 s mode frequently fails. Measure actual contacts logged at both ends per minute. [Run exchange and scheduling](rad950-run-mode-addendum.md)

### Where the decibels come from

- **Wait longer with one frame.** **0 dB** of additional signal energy. More time can permit a finer candidate search, more decoder iterations, or a less rushed turnaround. Any sensitivity gain comes from that processing and must be measured separately.
- **Repeat and combine soft decisions.** For equally useful copies with independent noise, the ideal gain is `10 × log10(N)` dB. Two copies give **3.0 dB**, three **4.8 dB**, six **7.8 dB**, and twelve **10.8 dB**. Demodulate each copy on its own timing and frequency estimate and combine signed bit confidence before LDPC. This approach avoids requiring carrier phase continuity between copies. Weight each observation by quality, and count a lost or erased copy as missing evidence. One strong copy plus several copies corrupted by the same interferer can yield much less gain. [Soft combining proposal](rad950-cooperative-reception.md)
- **Repeat and accept the first valid CRC.** This improves the probability of completion at a given SNR. It has no fixed dB value. With independent per-copy probability `p`, the probability of at least one decode is `1 − (1 − p)^N`. For example, at 20% per copy, three copies give about **49%** and six about **74%**. The measured single-frame curve gives the equivalent threshold shift.
- **Use one slower or longer coded waveform.** At equal power, three times the signal duration buys the same **+4.8 dB ideal energy gain**, and six times buys **+7.8 dB**. These figures are alternatives to the repetition gains. A changed code, tone spacing, or detector can change how much of that gain is usable. Longer symbols put more pressure on frequency stability and fading within a symbol. Start with repeated frames and compare a longer waveform only after establishing that baseline. [WSJT-X discussion of long sequences](https://wsjt.sourceforge.io/wsjtx-main_en.html#_fst4)
- **Shorten the turn.** Reserving the present 1.2 s guard in a 7.5 s turn leaves 6.3 s. At fixed power that gives about **1.45 dB less transmitted energy** than 8.8 s. This is an energy comparison. A higher symbol rate or shorter code changes actual sensitivity, occupied bandwidth and adjacent-channel performance. Verify the shorter profile with the handheld rather than deriving its threshold from that ratio.

## Presets and receive behavior

Each preset binds a **USB dial frequency** to a profile ID, turn length, UTC epoch, burst schedule, and reply-channel plan. Use shared, editable presets and defer the numeric dial-frequency list to coordination for the operating region. Keep the familiar 225 Hz grid for profiles that repeat the v2 waveform. A faster or slower waveform may need its own validated tone spacing or channel plan. In the UI, show dial, turn length, turnaround countdown and the operator’s mode. Hold the selected profile through a contact or batch.

Align turns to multiples of the profile period from a published UTC epoch, including the 120 s profile. As an initial 30 s experiment, use the existing nominal start at **+0.40 s**, then starts at **+10.40 s** and **+20.40 s**. The 60 s and 120 s variants extend the same pattern. Keep repetition index separate from UTC turn number. Bind received evidence to the selected profile, turn, window, caller candidate and, in Run mode, batch and assigned channel. Define profile discrimination in the wire amendment so a 10 s listener cannot treat a repeated 30 s burst as a new request and transmit in the middle of the longer turn. Test this explicitly with overlapping profiles and receivers that did not hear the opening CQ.

Use one logical CQ, report, confirmation, or Run control message per turn. Every copy within that turn carries that same message. In **Run mode** the runner can acknowledge the preceding batch and grant the next batch in the same logical message. All granted callers reply in the same next turn, each on its assigned channel and following the profile’s repeat schedule. Combine separately per caller. Finish and log according to the Run addendum, counting each logical acknowledgment once. Evaluate contention-slot spacing and periodically reannounce a long run so new callers know its schedule. [Run mode proposal](rad950-run-mode-addendum.md)

Keep the existing per-burst streaming receiver and retain a bounded confidence accumulator per associated candidate between repetitions. Prototype exact association over known tracks first. Then test blind acquisition, missing copies, multiple callers on one channel and independent drift. A receiver that never acquires any copy cannot combine its bit confidence. Compare real loss against the ideal gain using the same 2,500 Hz SNR reference throughout.

## Turnaround and radio limits

Express contact timing as **eligible turns plus elapsed-time limits**. The existing five-minute session behavior was designed for 10 s turns. A 120 s profile needs enough time for the clean exchange and a bounded retry. Derive explicit per-profile timeouts and confirmation retention from the schedule, and maintain real-time radio limits independently. A receiver may have a valid early decode but still waits for the advertised reply turn.

Distinguish one **logical turn** from the hardware’s keyed intervals, RF airtime and relay movements. Three repeats can occupy **26.4 s** of active waveform in a 30 s turn, but real key-up time also includes any gaps during which TX stays keyed. Conversely, releasing and re-entering RX for every copy can multiply relay moves. Measure both wall-clock key-down and RF-on time, verify PLL and PA behavior under sustained load, and enforce the existing hardware limits for all transitions. In particular, the table’s six and twelve repeats are energy models pending key-down, ramp, battery and temperature validation. Use a conducted load for these tests. The shorter profile also needs a measured decode-to-reply deadline within its guard. [RF path state machine](../crates/ferrite-core/src/path.rs) · [Conducted bench setup](rad950-lab-setup.md)

## Experiment order

1. **Host replay, 10 s versus 30 s.** Replay the same captured Radtel waveform at common input SNRs. On paired trials compare one burst, three copies with first-valid selection, and three with soft combining. Measure 50% and 90% blind decode thresholds and false accepts. Start with independent noise, then add shared interference, a nearby strong caller, and bursts that fade out for one or more copies.
2. **Two handhelds, ordinary and Run.** Compare 10 s and 30 s at several conducted levels and on a sustained path. Score matching contacts per minute, time to completion, missed deadlines, and false or duplicate logs. Run the two-caller batch on both profiles using the same offered load. On target, record peak RAM, per-block DSP latency, key-down time, relay count, battery sag and temperature.
3. **Evaluate 60 s and the fourth choice.** Advance 60 s if its measured gain over 30 s is worth the additional latency and key-down time. Choose between 120 s and 7.5 s using measured demand: sustained weak paths for the former and higher completion throughput at comfortable SNR for the latter. Include a Run trial for whichever profile advances.

The first implementation target is **10 s and experimental 30 s**. Profile timing and the operator’s choice of ordinary or Run mode should be independent from the start.
