# SOL950 cooperative reception experiment

**Proposal.** Two or more RT-950 Pros within roughly a mile listen to the same 10 m window. One radio acts as leader and handles the contact. Helpers report what they heard, initially over USB for measurement and eventually over a local VHF or UHF packet link. The distant station sends ordinary SOL950 frames. The experiment asks two questions: how often does another location hear a frame the leader misses, and how often can the team recover a frame that *every* radio missed individually? [SOL950 design](sol950-design-v2.md)

## Compare three ways to receive the same frame

| Method | Leader’s decision | What the comparison measures |
| --- | --- | --- |
| Leader alone | Run the existing decoder on the leader’s samples. | Baseline for the same slot and RF conditions. |
| Either radio | Accept a valid decode from either site, counting the frame once. | Benefit obtainable with a short payload report. |
| Either radio plus soft combining | Keep those valid decodes. For a joint miss, combine matched candidate observations and try the decoder again. | Additional recovery attributable to sharing evidence about individual bits. |

The receiver already computes signed confidence values for the 184 coded bits of an acquired candidate. If a local noise burst obscures some symbols at the leader, the helper may have better evidence for those symbols. Each radio tracks timing and frequency on its own. Combining bit evidence requires agreement on the frame and bit order, without requiring phase coherence between oscillators. A mile is a deployment example; actual benefit depends on antenna placement, local interference, and how often the stations fade together. [Receiver](../digimodes/usol950/src/rx.rs) · [LDPC decoder](../digimodes/usol950/src/ldpc.rs)

## What a helper reports

Start with one leader, one helper, and one window. For a successful local decode, send the 64-bit payload with helper ID, UTC slot, dial frequency, estimated tone offset, and measured local SNR. On a CRC failure with convincing sync, send one candidate’s 184 soft values plus its observation metadata. Include sync timing, frequency-calibration uncertainty, sync quality, drift hypothesis, and quantizer scale. Give the local packet its own integrity check. Identify an observation by helper, slot, and candidate so retransmission cannot count the same evidence twice.

For the host experiment, export the selected hypothesis in canonical LDPC codeword order before decoding. Freeze sign convention, bit order, and the meaning of zero in a small reference vector. Start with full receiver precision; then repeat with a signed four-bit quantizer, reserving zero for no evidence. Compare equal weighting with weighting calibrated on independent noise and gain conditions. Add in a wider accumulator and saturate only when converting to decoder input. The existing metrics come from tone-energy contrasts; their magnitude across two radios with different gain settings needs measurement.

Associate candidates by window, UTC slot, corrected tone frequency, and sync timing. Determine matching tolerances from paired captures. Test two callers in the same slot, including a loud caller 225 Hz away. Leave ambiguous matches uncombined. Log separately when neither radio acquired the candidate, when only one acquired it, and when both tracked it but individual decoding failed. Only the last group tests whether combining can rescue a joint miss.

## Experiments and decisions

1. **Host replay.** Start with the retained Radtel burst and the existing replay runner. Feed two receiver instances the same transmitted frame with independent noise draws. Sweep through the decode transition around −15 dB, recording all three outcomes above for each trial. Repeat with unequal site SNRs, independent timing/frequency offsets, shared interference, and a +15 dB neighbor primarily audible at one site. Include identical receiver inputs as a duplicate-evidence control. Run noise-only and unrelated-caller slots through the full candidate search to count false accepts. Save seeds, payloads, capture hashes, and per-slot outcomes. [Replay tooling](../tools/sol950_replay.py) · [Existing sensitivity measurements](sol950-lab-setup.md)
2. **Two-radio USB capture.** Capture PA2 audio from both radios for the same sequence. A common injected signal can verify association and bit ordering on real hardware. Then use separate antennas at the intended sites to measure actual diversity. Record each radio’s frequency calibration, gain settings, slot times, sample gaps, and clipping. Score *all* transmitted slots, including acquisition failures. Report leader-only, helper-only, both, neither, rescued joint misses, and false accepts. Give both the percentage of all frames rescued and the percentage of joint misses rescued, with counts and uncertainty. If most gains come from helper-only decodes, take only the short payload report into the first radio-link prototype.
3. **Local packet link.** If four-bit reports materially improve recovery over either-radio decoding in paired captures, test one helper on one local VHF or UHF channel. Timestamp observation, packet arrival, combined decode, and earliest eligible 10 m reply. Measure packet loss, missed 10 m receive slots, and any desensitization of a nearby listener while a helper transmits. Add helpers only after measuring that cost. The operating decision is whether cooperation increases the fraction of contacts logged at both ends within 60 seconds, including the time spent exchanging reports.

## Browser host over Bluetooth LE

For nearby radios, use **Tools → Cooperative RX** on RAD950.org in desktop
Chrome. Turn on Bluetooth, set **Cooperative RX → Capture** and **Link →
Bluetooth**, and leave each radio in RAD950 Monitor with UTC ready and the same
dial. The computer connects separately to each radio over Ferrite Link on the
BT513 BLE UART. Choose one receiver as the comparison leader.

The radios report ordinary decodes and at most one failed soft candidate per
completed slot. The browser groups by exact UTC slot and dial, then checks tone
frequency and timing before adding 4-bit soft evidence. Keep only one report
per receiver per slot. Show CRC-checked combined decodes alongside individual
decodes. The first browser version uses equal weighting and skips ambiguous
timing/frequency groups. A combined decode remains a candidate result until
checked against a known transmitted frame.

The browser retains observations in the open session and can download them as
JSON. Start the hardware trial with two radios, then repeat with three to
measure the third receiver's contribution and Bluetooth report completeness.
The tool is receive-only.

## Airtime and reply scheduling

Four bits × 184 coded bits gives **92 bytes per soft candidate**. The figures below are lower bounds for that vector alone. The example packet adds an illustrative 20 bytes for metadata and integrity. Preamble, link framing, key-up, turnaround, contention, and retries add wall-clock time.

| Assumed gross link rate | 92-byte vector | Illustrative 112-byte packet |
| --- | ---: | ---: |
| 1,200 bit/s | 0.61 s | 0.75 s |
| 2,400 bit/s | 0.31 s | 0.37 s |

SOL950 already budgets its 1.2 s inter-burst guard for decode and radio turnaround. Start with scheduled reporting periods, one opportunity per helper, prioritizing valid payloads over failed candidates. Keep the leader on the contact window and allow a later eligible reply slot when necessary. A report received after its session expires can inform the reception log only. Measure RF switching, decoder RAM, queued-report storage, and end-of-slot processing on target before promising immediate replies or simultaneous HF reception during local transmissions. [Slot and contact timing](sol950-design-v2.md) · [RF path ownership](../crates/ferrite-core/src/path.rs)

Only the leader transmits 10 m replies for its callsign. Preserve whether each reception was local, helper-only, or combined, and retain the contributing station IDs and local SNRs. In the first prototype, score combined receptions offline. Before enabling automatic replies based on them, define which SNR goes in the on-air report and how the operator’s log identifies the receiving sites.
