# RAD950 protocol and receiver design review

Two choices can be made before the first transmission. A directed report fits both standard callsigns in the proposed frame, and a three-slot exchange gives both operators a meaningful local completion rule. The first hardware question is whether a known 10 m USB signal reaches the MCU as usable samples. The first decoder question is how early the receiver can acquire a weak caller, given how little of the slot it can retain. These decisions build on the [RAD950 proposal](rad950-design.md).

## Prove the 28 MHz receive route first

Ferrite’s HF task selects SI4732 below 18 MHz and BK4829 at and above 18 MHz. At 28 MHz the normal HF entry point never calls the patched SI4732 USB tuner. Conversely, the firmware records an October bench observation that SI4732 audio reaches PA2 on the shared receive-audio path. That narrows the hardware question considerably. Start with a below-18-MHz USB capture as a control. Then deliberately select SI4732 through the radio task in a bench build, inject at the antenna connector at 28 MHz, and measure the level required to reproduce the same audio SNR. This separates a working audio connection from usable sensitivity through the 10 m front end. [HF selection](../firmware/src/tasks/radio/hf.rs) · [PA2 bench observation](../firmware/src/tasks/radio/debug.rs)

Use the existing RF path state machine for the transition. It owns PA drive and relay sequencing, requires break-before-make, and limits relay switches in a rolling minute. Exercise the intended CQ and retry schedule against that limit, including entry from another band. For a bench build, the existing cable tool can capture decoder-path PCM with `python3 tools/ferrite_link.py PORT audio 15 rad950-rx.wav --raw 16000`. Record the reported lost frames and clipping alongside the WAV. [RF path](../crates/ferrite-core/src/path.rs) · [Capture tool](../tools/ferrite_link.py)

## Use the report bits to identify both stations

| Frame | Proposed packing for a prototype | Bits |
| --- | --- | ---: |
| CQ | 1-bit type, sender 28, grid 15, power 4, reserved 16 | 64 |
| RX | 1-bit type, sender 28, recipient 28, SNR 5, reserved 2 | 64 |

Replacing the 16-bit hash costs 12 of the 14 flag bits. A pouncing station can now address a caller unambiguously and the caller can identify the pouncer even if it never sent CQ. The trade is two remaining report flags. Specify which calls are representable and reject unsupported calls explicitly. A four-character Maidenhead locator has 32,400 possible values, leaving 368 of the 15-bit patterns available, including room for “grid unknown.” Give the five-bit SNR field a defined reference bandwidth, step size, endpoints, and clipping rule.

For a first interoperable frame, publish one CQ and one RX worked all the way from human-readable fields to 110 numbered tones. Freeze packing order, callsign alphabet and padding, CRC polynomial and initialization, LDPC parity matrix and systematic bit positions, interleaving, tone labeling, and the exact positions and values of all 18 sync symbols. Both frame types yield 78 information-plus-CRC bits and need 106 parity bits. The rate alone cannot identify a code.

## Make a three-slot contact work under loss

| Slot | Transmission | Evidence at the receiving station |
| --- | --- | --- |
| 0 | A sends CQ | B has heard A and can send a report. |
| 1 | B sends RX addressed to A | A has received B’s report and owes B its own. |
| 2 | A sends RX addressed to B | If B decodes it, B has both sent and received a report. A can log locally once its transmission has finished. |

Record “my report finished transmitting” and “peer report decoded addressed to me” separately, per peer and window, with timestamps. A queued or interrupted TX cannot satisfy the first condition. Local completion cannot prove that the peer received the last frame. If slot 2 fades, B should be able to repeat its report and A should be able to answer a bounded repeat during cooldown without writing another log entry. Specify a retry limit and expiry so two completed stations eventually go quiet. Test a lost final report, two callers in slot 1, duplicate reception, and an exchange crossing the five-minute expiry.

Scanning also needs a rendezvous rule. With two windows, one slot of dwell each, and an even-slot caller, a scanner visiting that window only on odd slots can miss every CQ indefinitely. A two-slot dwell observes both parities at the cost of revisit time. Simulate every initial scan phase against the proposed CQ schedule and report time to first decode and to mutual completion. Under activity, give an owed directed reply priority over a fresh CQ.

## Specify frequencies and timing at the antenna

Define the window reference as a USB dial frequency and specify a positive audio offset for the lowest tone. For example, a 28.185 MHz dial, 1,000 Hz offset and 50 Hz spacing put tone centers at 28.186000 through 28.186150 MHz. Sending directly on the dial places the first tone at nominal zero beat. Measure the usable lower and upper audio edges and the BFO sign with two known RF tones. Ferrite already separates whole-kHz tuning from the signed BFO adjustment and flags the sign for verification. [SSB tuning](../crates/ferrite-core/src/ssb.rs)

At 80 ms per symbol, 30, 40 and 50 Hz give adjacent tones 2.4, 3.2 and 4 cycles apart. With an ideal rectangular complex correlator the 50 Hz pair is orthogonal at exact timing and frequency. Windowing, drift and synthesizer transients change that result. Sweep all three spacings using identical payloads and noise realizations. Vary initial offset and within-burst drift independently. Record the 99%-power bandwidth and weak-frame decode rate beside a stronger neighbor. Four tone centers span three spacings, but that arithmetic alone cannot predict occupied bandwidth or simultaneous decode capacity.

The BK4829 driver writes two frequency registers and documents when a new value takes effect on receive. Verify on a dummy load with an independent receiver that each commanded keyed transition actually moves the carrier, including alternating lowest and highest tones. Record step latency, envelope dips and transient spectrum. Readback of the requested frequency cannot establish any of these. [BK4829 driver](../drivers/bk4829/src/lib.rs)

Give each slot a specified first-symbol time relative to UTC. Budget the 1.2 s guard from the last useful RX sample through final decode, reply selection, radio switching and RF readiness. For scanning, schedule retuning early enough to capture the first sync symbol of the next window. Ferrite polls its 9,600-baud GPS stream every 50 ms, publishes fixes at a limited rate and can put the module to sleep. Timestamp the timing observation against a monotonic clock and measure its offset and jitter against an independent reference, including after wake. Define permitted clock uncertainty and holdover age before enabling scheduled TX. [GPS task](../firmware/src/tasks/gps.rs)

## Fit acquisition into the receiver

One 8.8 s slot at 24 kHz occupies 422,400 bytes as signed 16-bit PCM. Even a 3 kHz-wide spectrogram with 12.5 Hz bins, one byte per bin and just one timing hypothesis needs about 26,400 bytes for 110 symbols. Ferrite’s decoder limit is 13,312 bytes. A streaming receiver must decide which candidates to keep while the frame is arriving. Sync placement therefore affects memory as well as sensitivity. [Audio pipeline](../crates/ferrite-core/src/audio.rs) · [Decoder budget](../crates/ferrite-core/src/decoders.rs)

The input arrives in 8 ms blocks, ten blocks per symbol. At 24 kHz a symbol has 1,920 samples. For a 2,048-point spectrum, window exactly those samples and zero-pad the rest. Consuming 2,048 new samples per symbol would slip by a full symbol after 15 symbols. Ferrite’s FFT returns a block scaling exponent. Account for it when comparing energies across windows so a strong neighbor cannot distort a weak caller’s soft metrics through rescaling alone. [DSP task](../firmware/src/tasks/dsp.rs) · [FFT implementation](../digimodes/udsp/src/fft.rs)

HF mode returns from the radio tick before normal BK decoder routing. Model RAD950 as an explicit receive mode that captures continuously through quiet slots and keeps the other audio sources muted. Check each DSP block deadline as well as the end-of-slot deadline. The SI SSB configuration enables automatic volume control and disables AFC, so include an abrupt strong neighbor in the test and measure how long the weak signal takes to recover. [Radio tick](../firmware/src/tasks/radio/tick.rs) · [SI configuration and measured audio levels](../firmware/src/tasks/radio/mod.rs)

Prototype an early sync cluster for acquisition and later clusters for drift tracking. Use a short rolling history to recover initial data, then retain soft bit metrics for a bounded set of candidates. At one byte per coded-bit metric, ten tracks cost 1,840 bytes before search state, history, and LDPC scratch. Decode candidates serially with reusable scratch. Test what happens when a loud false candidate fills the last track before a weak real caller appears. Compare this streaming receiver against a host decoder with the whole recording available. The difference measures what early admission costs.

## Experiments that decide the next revision

1. **Establish the receive path.** Select SI4732 USB, feed a known RF tone, move it by a known offset, and verify the corresponding move in captured ADC samples. Repeat with RF removed and with the BK receiver isolated to distinguish routing from leakage. Save sample indices, gain settings, dial frequency and input levels. The existing raw-audio transport exposes sample indices and flags. A clean speaker signal alone cannot establish this path. [ADC capture](../firmware/src/hal/audio.rs) · [Raw audio format](../crates/ferrite-core/src/rawaudio.rs)
2. **Measure sensitivity before choosing a code or spacing.** Sweep SNR referenced to 2,500 Hz and report 50% and 90% correct-frame thresholds for unknown arrival offset and frequency across the usable window. Compare known timing/frequency, blind sync acquisition, and complete blind decode on identical data. Include noise-only slots, unrelated carriers, measured drift and unequal-power callers. Count false frames per searched slot and per hour. For example, zero false frames in 10,000 independent slots gives an approximate one-sided 95% upper bound of 0.11 per hour at six slots per minute. The published FT4 single-symbol decoder and its more expensive decoder differed by 2.2 dB in white noise, enough to cover RAD950’s entire proposed target range. [FT4 and FT8 protocol paper, Table 5](https://wsjt.sourceforge.io/FT4_FT8_QEX.pdf)
3. **Measure the cost of answering in the next slot.** Replay weak and crowded captures on the target. Record peak live memory including stack, missed audio blocks, candidate count, LDPC iterations and final-sample-to-reply-ready latency. Combine those results with measured RF turnaround. If a busy window overruns the guard, specify which later slot carries the reply.
4. **Validate both logs.** Replay the same three-slot exchange with each slot individually erased, then with competing callers. Score time to first contact, fraction with matching logs at both ends, duplicates and extra transmissions after completion. These outcomes expose problems a single station’s decode count will miss.
