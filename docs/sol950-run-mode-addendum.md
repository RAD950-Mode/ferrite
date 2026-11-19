# SOL950 Run mode proposal

**Proposed v3 addendum.** An attended running station calls CQ, chooses several callers, and assigns their next replies to distinct frequency offsets. The runner sends one ordinary-duration 4-FSK burst per transmit slot. Several callers transmit during the same receive slot. Once the run is underway, each runner burst can acknowledge the previous batch and admit the next. The question for v3 is whether that increases contacts **saved exactly once at both ends per minute** on real radios. [Air interface](sol950-design-v2.md) · [Firmware feature design](sol950-firmware-feature.md)

## Worked exchange

`SEQ` is the ordered list shown to the operator. Reserve `RRR` for confirmation that the runner received a directed report. The example shows four callers to illustrate the intended operation. The first prototype serves two per batch.

| UTC slot | Runner | Callers |
| --- | --- | --- |
| 0 | `CQ RUN NQ6N EN51`. Advertise the reply plan and establish the run context. | Listen and select the runner. |
| 1 | Listen across the window. | K0RF, K9CT, W1AW and W2AV send directed call requests on advertised contention channels. |
| 2 | `SEQ K0RF K9CT W1AW W2AV`. Give each caller an individual received-signal report and a channel for slot 3. | Look for a grant matching the request just sent. |
| 3 | Listen across the assigned channels. | Each granted caller sends its directed report on its channel. In this example, the runner misses W1AW. |
| 4 | Confirm K0RF, K9CT and W2AV. Reassign W1AW and, if space permits, assign new queued callers. | Confirmed callers save their contacts. W1AW prepares to retry. |
| 5 | Listen for the newly assigned batch. | W1AW and newly assigned callers send their reports. |

Use the existing eight nominal **225 Hz** subchannels. Number them from the USB dial, with lowest tones at 600, 825, 1,050, 1,275, 1,500, 1,725, 1,950 and 2,175 Hz. Assign channels in ascending order from an advertised set. For example, if the runner selects 825, 1,275, 1,725 and 2,175 Hz, the caller in the third position gets **+1,725 Hz** for its lowest tone. The UI can show either the ordered sequence or explicit offsets. A caller should never have to interpret what “UP3” means or count its position while the slot is running.

Advertise during which slots unassigned callers may contend and on which channels. For the initial experiment, use dedicated request slots so contention cannot spoil an assigned reply. Test separate request channels during a busy receive slot only after measuring the cost of simultaneous callers. A caller whose request was lost or collided retries with bounded, randomized slot and channel selection. A caller that received a grant transmits only on the assigned channel and slot, subject to the normal TX checks.

## Addressing and the frame budget

A SOL950 burst has **64 payload bits**. Four 28-bit base calls take **112 bits** by themselves. Two full recipient calls plus the runner take **84 bits**. The printed full-call sequence therefore needs a different wire representation. Keep the RF waveform and slot length from v2 for the first experiment and treat the batch format as a protocol extension. [Frame codec](../digimodes/usol950/src/frame.rs)

Prototype a **recent-request reference**. The runner announces a run context, and each initial request establishes the runner identity, caller identity, request slot and a short request reference. A subsequent grant matches that specific request within the run. The grant carries the ordered caller references, a reply-channel plan and each caller’s SNR report. The confirmation can use a bitmap over positions in the immediately preceding batch, with a reference to that batch. Pairing the bitmap with its batch is essential. A caller that missed a control burst must never interpret a later bitmap as an acknowledgment of its own report.

Freeze the bit layout only after the two-caller trial. Account explicitly for context, request references, channel plan, per-caller SNR, previous-batch confirmation, version discrimination and ambiguity handling. Reuse the current five-bit SNR convention for the prototype. A shared number such as `+10` in the display needs an explicit meaning and cannot stand in for separate received-signal reports. Coordinate any reserved codepoints with the compound-callsign extension. [SNR coding and slot rules](sol950-design-v2.md) · [Full identities](sol950-firmware-feature.md)

Two callers can transmit on the same request subchannel and one may capture the decoder. Channel number alone cannot identify which call was decoded. Include a caller-specific reference and expire it promptly. Reject ambiguous or stale matches. A reply must positively identify the run, the correct grant and both station identities before it can supply contact evidence. Test the case where two callers locally believe they have the same short reference and only one was heard. A collision must never let the other caller log a contact. Measure the residual probability of a wrong match before relying on shortened references for automatic operation.

The protocol must distinguish `NQ6N-1`, `NQ6N-2` and `KP2/NQ6N-1` where decorated calls are supported. Key the queue, reports, confirmations and log by the complete identity. A plain-call test can establish the throughput before the decorated-call format is available.

## Completion, retries and operator control

- **Grant.** The runner holds a request in the queue until it grants a specific reply slot and channel. The grant delivers the runner’s report for that caller. Grant transmission alone earns no completion.
- **Directed reply.** A caller that received its grant sends its own report to the runner. The runner validates the decoded identity and current grant before marking the caller ready for confirmation. A wrong-channel or wrong-slot reply supplies no evidence for that grant.
- **Confirmation.** The runner saves the contact after receiving the valid reply and finishing transmission of its confirmation. The caller saves after finishing its directed report and decoding the confirmation for that exact batch and position. Preserve the exchanged reports and any grids received with the full identities.
- **Lost reply.** Leave that caller’s confirmation clear and give it another eligible reply. Reassign a channel if interference keeps blocking the first one. Bound the attempts. A missing acknowledgment bit supplies no permission to transmit.
- **Lost confirmation.** Retain the completed entry during cooldown. A caller still waiting for confirmation can request it again during an advertised opportunity, referencing its previous grant. The runner can confirm again using the same contact record. Keep enough state to answer this request even after admitting a new batch, and expire it explicitly.
- **Scheduling.** Give confirmations and bounded retries room alongside fresh grants. Age the waiting queue so a quiet station can get a turn. Reannounce CQ periodically so newly arriving stations learn the run context. The radio still enforces its existing limit of three TX slots in a rolling minute. If timing, relay availability or RF readiness prevents a slot, leave the affected grant pending or let it expire visibly. [Existing completion rules and TX limit](../crates/ferrite-core/src/sol950.rs)

The operator sees waiting callers, assigned offsets, received replies and saved contacts. A pause stops new grants while the operator can see outstanding confirmations. Stop cancels queued TX through the normal radio path. The worked-grid map and statistics consume durable QSO records from the run as they do for individual contacts. [Logging and controls](sol950-firmware-feature.md)

## Experiment before freezing v3

1. **Model two callers per batch.** Compare ordinary SOL950 and Run mode under the same offered calls, waveform, window, clock uncertainty, decode outcomes and TX limit. Sweep request collisions, a missed grant, a missed reply, a missed final confirmation, queue overflow and two runners audible on one window. Assert at most one log per contact at each end and no contact for an ungranted caller. Verify successful recovery of both missing-reply and missing-confirmation cases.
2. **Replay parallel replies.** Use measured Radtel bursts and the existing 225 Hz offsets. Include independent timing and frequency errors, a +15 dB neighbor, and two callers that initially contend on the same channel. Measure how many expected replies are found, decoded and correctly attributed. Confirm the runner can decode and pack its next control burst inside the **1.2 s** turnaround budget on target. Move from two callers to four only after measuring acquisition and processing under load. [Bench measurements](sol950-lab-setup.md)
3. **Make attended contacts using the actual UI.** Measure contacts saved exactly once at both ends per minute, time from request to confirmation, and the fraction left pending or expired. Count time spent on CQ, retries and repeated confirmations. At the current three-transmit-slots-per-minute ceiling, an ideal pipeline with two confirmations per runner slot has an upper bound of six completions per minute before overhead and losses. Four raises that bound to twelve. Keep Run mode if it delivers a measurable throughput improvement without increasing wrong or duplicate logs.

The ordered assignment and combined confirmation are the v3 proposal. Batch size, exact packet layout, compact reference width and retry scheduling remain experimental.

[Related design precedent in the WSJT-X FT8 DXpedition Mode guide](https://wsjt.sourceforge.io/FT8_DXpedition_Mode.pdf)
