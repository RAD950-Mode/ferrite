# RAD950 bulk transfer proposal

**Draft for discussion.** Add an optional mode for sending compact messages and records over multiple turns. A sender announces a bounded object, sends numbered fragments, and pauses at agreed times for reception reports. The sender retransmits missing fragments selectively. The receiver verifies the whole object before confirming delivery.

## Use the turn for data or repetition

Keep the 10-second burst grid and the 64-bit payload budget. A data turn occupies one, three, or six consecutive burst slots. The sender can fill those slots with distinct fragments, repeated copies, or both. For example, a 60-second turn can send six fragments, send three fragments twice each, or repeat one fragment six times as in RAD950-60. Copies carry identical protected bits so the receiver can combine soft decisions after estimating timing and frequency separately for each copy.

| Data turn | Burst opportunities | Example allocation |
| --- | ---: | --- |
| 10 s | 1 | Fragment A |
| 30 s | 3 | A, B, C or A, A, A |
| 60 s | 6 | A through F, A, B, C twice, or six copies of A |

At equal power, six equally useful copies offer up to 7.8 dB of ideal combining gain over one copy in independent noise. Actual gains depend on acquisition, fading, interference, and the decoder. Distinct fragments increase throughput. At six bursts per minute the raw ceiling is 48 payload bytes per minute. An illustrative four-byte data field cuts that to 24 object bytes per minute before control traffic and retries. A full 60-second data turn followed by one 10-second ACK slot would deliver about 21 object bytes per minute at that packing and with every fragment decoded on the first pass. [Turn-length experiments](sol950-turn-length-addendum.md)

## Directed transfer

1. **Offer and accept.** On the 10-second grid, the sender sends a short control sequence containing both station identities, a transfer ID, the encoded and decoded object lengths, the content and compression formats, an end-to-end checksum, and the initial schedule. The receiver accepts only a complete offer it can store and decode. Acceptance echoes the transfer and schedule so both endpoints agree on the first data slot and reply slot. Retry a lost acceptance by repeating the offer with the same ID.
2. **Send a window.** A window is one scheduled data turn with up to six burst opportunities. Use stable, object-wide fragment numbers. Store CRC-valid fragments in any order and treat identical repeats as duplicates. The accepted schedule identifies which fragment is expected in each burst slot, so soft combining can work before the fragment number itself decodes. Combine only observations attributable to the same scheduled fragment.
3. **Pause for a bitmap ACK.** The sender listens during the reserved reply slot. The receiver returns the transfer ID, a window identifier, and a received-fragment bitmap relative to a stated base fragment number. The sender fills holes before advancing. A missing ACK leaves reception state unknown. The sender polls or repeats the current window on the agreed retry schedule. The receiver answers a repeated window with its current bitmap.
4. **Verify and confirm.** Once all bytes arrive, the receiver checks the encoded length, verifies the object checksum, and decodes the object within the agreed output bound. It sends a distinct completion ACK after successful verification. The sender declares delivery on receipt of that ACK. For a bounded period, the receiver remembers completion and answers status polls with the same result, without delivering the object twice.

### Timing and adaptation

Anchor every reservation to an absolute slot on the existing UTC 10-second grid. Give the initial exchange explicit offer, acceptance, first-data, and reply slots, including time to decode and switch direction. A 60-second allocation starting at 12:00:00 UTC occupies the six slots through 12:00:50, with the ACK opportunity beginning at 12:01:00. Silence inside a reservation does not open a reply opportunity.

Start with a fixed profile per accepted transfer. An adaptive version can propose a new allocation after an ACK using an explicit effective slot and receiver acceptance. For a profile change, require the sender to receive that acceptance before using the new data schedule, and have the receiver listen during the proposed activation window after accepting. If the acceptance is lost, use prearranged 10-second control opportunities to reacquire agreement. Reserve those opportunities outside either possible data allocation.

Bound retries, inactivity, and total transfer time in seconds. On an interruption, retain partial data for a bounded period and resume only after a fresh agreement that identifies the same source, destination, and object. Use a fresh over-the-air session identifier or a defined reuse delay to keep stale ACKs from advancing a new transfer.

## Wire format and compatibility

Use a dedicated bulk-transfer frame family with a distinguishable sync or other discriminator that legacy decoders demonstrably ignore. The existing extension kinds are already in use, including an overlap between decorated identity and chat at kind 2. Treat selection of a discriminator as a prototype decision and test it against legacy CQ, report, identity, chat, and distress handling, including receivers that enter mid-transfer. Hold the selected dial and subchannel for the accepted exchange. [Current frame formats](rad950-spec.md) · [Extension kinds](../digimodes/usol950/src/ext.rs) · [Chat framing](../digimodes/usol950/src/chat.rs)

An illustrative data layout uses four bits for the message type, twelve for a short session handle, sixteen for the fragment number, and thirty-two for object bytes. The final fragment is trimmed using the accepted encoded length. Bind the short handle to full endpoints, object identity, and the active slot reservation. Resolve a handle collision before accepting data. Control messages may span several bursts because the two full station identities alone consume 56 bits. Define explicit control fragment numbering and integrity checking before adopting a bit layout.

Use a bounded byte string as the object, with an uncompressed encoding alongside any published compression format. Compress before fragmenting so retransmissions use stable byte offsets. The receiver must know the codec and maximum decoded size before it accepts. Favor small objects whose encoded bytes plus metadata fit the airtime budget. Keep fragment storage and soft-combining state bounded independently.

### Bulk throughput and coding rate

Keep LDPC(184,78) and CRC-14 for the robust bulk profile. First evaluate shortening the data header by using the agreed UTC burst schedule to infer the fragment number, including the mapping for repair bursts. Preserve enough explicit session information to reject stale or unrelated bursts. If this saves 16 header bits, each burst carries six object bytes instead of four. With six distinct fragments per 60-second turn and a 10-second ACK slot, a 1 KiB transfer takes 33 min 50 s across whole windows, compared with 50 min 10 s for four-byte fragments. Both estimates exclude setup, separate completion traffic, and retries.

Evaluate LDPC(184,86) as an optional profile for reliable links. At the same 184 coded bits and with CRC-14, it adds one payload byte per burst and reduces parity from 106 to 98 bits. The profile requires its own code and explicit agreement before use. Compare both codes with the same fragment header and delivery target, measuring verified object bytes per minute and per mW·min, including ACKs and retransmissions. A measured gain at strong signal levels would justify the higher-rate profile.

## Multiple receivers

Begin with one designated ACK station. Other receivers can collect the same broadcast fragments, and the completion ACK applies to the designated station. A later group mode can schedule one reply opportunity per receiver for confirmed delivery to named stations. Broadcast repair can instead use assigned opportunities for receivers to request missing fragments. Define group completion before deciding how many reply slots to spend.

## Prototype and evaluation

- Start on the host with one sender, one receiver, an uncompressed object, 10-second bursts, and fixed data and reply reservations. Exercise missing control and data fragments, duplicate and delayed ACKs, mid-transfer restart, and replay of the completion ACK.
- Compare 10-, 30-, and 60-second allocations under blind acquisition with timing error, drift, fading, and adjacent-channel interference. Report verified object bytes per minute including setup, ACKs, and retries. Also record completion probability, latency, and peak receiver storage. Compare distinct fragments with repetition at each signal level before adding adaptive scheduling.
- Verify legacy behavior on a shared channel before field use. On the handheld, count keyed intervals, RF-on time, relay transitions, and decode-to-reply latency separately. The current SAFE-22 rule caps TX slots in a rolling minute. A longer logical turn grants no exception to that rule. Test radio feasibility into a conducted load and qualify each supported burst schedule against the actual timing, duty, and thermal limits. [Radio limits](rad950-spec.md) · [Conducted setup](sol950-lab-setup.md)
