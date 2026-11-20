# RAD950 conversation extension proposal

This proposed extension gives a completed RAD950 contact a conversational mode. Operators can send a compact station description, ask about the weather, type a short personal message, or agree on where and when to meet again. The receiving radio renders topic codes and values as readable text under the sender’s full call. The first experiment keeps the 10-second RAD950 waveform and 64-bit payload. The same conversation can use a longer turn profile once both stations support it. A separate distress extension lets any compatible listener receive an alert and its location without first making a contact. [RAD950 air interface](rad950-design-v2.md) · [Turn-length presets](rad950-turn-length-addendum.md)

## Operating experience

After a contact, select **Chat** and invite the station to continue. Both sides see the full identities and the selected window. A keypad menu offers **My station**, **Weather**, **About me**, **Ask a question**, **Common phrases**, **Schedule QSO**, **Move to frequency**, and **Type a message**. Preview the message and send it on the next eligible turn. The other operator can answer with a card, a phrase, or literal text.

For example, **My station** could display:

> **KF8QE** · RT-950 Pro, 5 W, barefoot, dipole at 8 m, 18 °C.

A common phrase such as **How have you been?** needs one burst. To make it personal, the operator can type **HEY MATT HOW HAVE YOU BEEN?** The receiver displays it under **KF8QE → NQ6N**. Calls already appear in the session header, so the operator can spend the character budget on the message.

Station fields can be stored as editable presets in Setup (rig, power, amp, antenna type, height). Weather values are entered by the operator. Name, age, and licensing history are optional and shared only when selected for a message. Show pending, delivered, or incomplete status based on receiver acknowledgment, and display active transmission as a solid red **TX** flag with white text (never overlapping status text). In the Reply menu, highlight **My station** automatically when the peer asks about antenna/rig, or **More...** when the peer asks about weather. Keep the session transcript in the chat view during the experiment. A chat message supplies no new QSO or worked-grid credit. [Handheld operating and logging design](rad950-firmware-feature.md)

## Cards and phrases

| Card | Candidate values | Rendering example |
| --- | --- | --- |
| **Station** | Rig model, estimated output power, amp state, antenna type, antenna height, optional temperature | RT-950 Pro · 5 W · barefoot · dipole at 8 m · 18 °C |
| **Weather** | Conditions, temperature, optional wind and direction | Light rain · 12 °C · wind NW |
| **About me** | Optional age and year first licensed | First licensed in 1994 |
| **Question** | Requested topic and optional question variant | What antenna are you using? |
| **Phrase** | Versioned phrase ID | First QSO on RAD950. Thanks for the contact! |
| **Literal** | Name, unlisted equipment, or short free text | My name is MATT |
| **Schedule QSO** | Future UTC date and time, shared dial preset, RAD950 profile and subchannel | Meet 19 Nov at 18:30 UTC · 28.185 MHz · 30 s |
| **Move to frequency** | Near-term UTC turn, destination band and dial, profile and subchannel | Move at 18:30:40 UTC · 28.190 MHz · 10 s |
| **Distress** | Standalone alert with sender, available grid, optional emergency category, and linked precise location | Distress · KF8QE · medical · GPS location received |

The codebook maps IDs to meanings. Translations can change the rendered words while preserving their meaning. Start with a few common antenna types and equipment models plus a short phrase list. Give unknown or unset values a defined representation. Keep measurements numeric with stated units. In particular, distinguish **antenna height above local ground** from elevation above sea level, and describe power as the operator’s indicated RF output. Use literal text for an equipment name that has no code.

### Example single-frame budget

For planning, allow **38 bits** for a card. One candidate shared extension envelope spends 6 bits on the out-of-range sender marker, 2 on the extension family, 3 on the chat frame type, 12 on a session reference, and 3 on a per-direction sequence number. That leaves 38 bits out of 64. This allocation illustrates the tradeoff. It does not assign wire values. Share the extension dispatcher with the compound-identity and Run work before reserving any codepoint. Current v2 decoders should discard extension frames cleanly. [Wire framing](rad950-design-v2.md) · [Compound identities](rad950-firmware-feature.md) · [Run mode](rad950-run-mode-addendum.md)

| Station card value | Bits | Proposed interpretation |
| --- | ---: | --- |
| Rig model | 10 | Versioned catalog. Zero means unspecified. |
| RF power | 7 | One-dB steps in a documented range. Display in watts or dBm with rounding. |
| Amp | 1 | External amplifier off or on. |
| Antenna type | 5 | Catalog with a code for unspecified. |
| Antenna height | 7 | Whole meters above ground, with an unknown code. |
| Temperature | 8 | Whole degrees Celsius, with an unknown code. |
| **Total** | **38** | **One card in one burst.** |

If amp state also needs an unknown value, use a second bit and shrink another field. Before implementation, publish the exact numeric ranges and rounding, a wire vector for the example station card, and vectors for boundary, unknown and reserved values.

## Scheduling a QSO and moving a multiplier

Both cards are directed offers within an agreed session. A delivery ACK means the radio received the offer. **Accept** means the operator agreed to its terms. **Counteroffer** sends the full replacement proposal, and **Decline** closes it. Bind each response to a specific outstanding offer and show the peer, UTC time, USB dial, profile and subchannel on both screens. Start with one outstanding offer per direction. A duplicate retransmission updates the same offer. A stale or conflicting reply leaves the current agreement pending or expired.

### Schedule QSO

Enter a UTC date and time or choose a shortcut such as **Tomorrow at this time**. Select a destination from the stations’ shared preset list. Preview the expanded UTC date, dial frequency and subchannel before sending. The receiving station can accept or propose a different time or preset. Save the accepted plan locally as a reminder with a **Go to preset** action. The reminder opens or tunes the selected receive preset when the operator chooses it. The operator starts the contact separately. Receiving or accepting a schedule does not add a QSO or a worked grid. [Turn-length presets](rad950-turn-length-addendum.md)

A possible **38-bit body** uses one rendezvous subtype bit, two action bits, seven days from the chat’s agreed UTC date, eleven bits for UTC minute of day, eight for the shared dial-preset ID, three for the profile, three for the subchannel, and three for the offer reference. For an initial offer, the reference can be the offer sender’s sequence number. A response echoes that reference and the proposed coordinates. Reject minute codes above 1,439. Resolve the date against the pinned session date so the same frame means the same thing on both sides of midnight. Use a shared, versioned preset table and display the full frequency. An unknown preset stays pending until the operators choose a common destination.

### Move to frequency

The operator can ask a station to try a second frequency, including moving a potential multiplier during a contest. Keep the source contact and destination contact separate. Once the source report is safely acknowledged, the runner can offer a destination and a near-term **rendezvous turn**. The other station accepts only if it can operate there. Both see the move countdown. An operator-approved move can retune in time to listen for the named station. Each operator then starts or answers a normal directed contact at the destination. If nobody is heard by the expiry, show **Peer not heard** and offer a return to the source window. A failed move never changes the source log or credits the destination.

Use a separate destination window on 10 m for the first hardware trial. Cross-band moves are useful for multipliers, but require a supported destination band and mode at both ends. An offer can display a band the current radio cannot tune. In that case the radio can save the proposed details for manual coordination. It cannot arm a retune to that destination. Keep the assisted move configurable for contests and let the operator decide when it is permitted.

A possible **38-bit body** uses one rendezvous subtype bit, two action bits, five band-table bits, eighteen bits for USB dial offset from that band’s defined reference in 10 Hz steps, three profile bits, six bits for a UTC rendezvous turn modulo 64 on a 10 s clock, and three for subchannel. The short clock field is suitable for moves negotiated on the 10 s profile. Resolve it against a recorded offer slot, require a unique future occurrence inside a short acceptance window, and choose a time aligned to the destination profile. Use **Schedule QSO** for a later rendezvous or when a slower source profile cannot fit the offer and acceptance before that window closes. The move reply echoes the proposed location and time. Keep one move outstanding and bind it to the session and offer slot so a late acceptance cannot confirm a different move. Commit no wire assignment until both direction and offer-reference semantics are exercised under loss.

An offer to one caller in Run mode carries that caller’s full session association. Send it as a directed follow-up after clearing the caller’s run contact. Other callers keep their existing assignments. The same runner may remain at the source until it explicitly chooses the move, so the proposed turn must allow time to finish any outstanding acknowledgments. [Run mode proposal](rad950-run-mode-addendum.md)

## Distress alert and precise location

Provide a dedicated **Send distress** action reachable without a prior QSO from Operate (`#` Menu → bottom item **Send distress...**, or Setup → bottom item **Send distress...**) as well as from Chat (`Reply → More... → Send distress...`). Do **not** bind Distress to a single-key digit shortcut so it cannot be opened accidentally. Preview the full identity, available grid, exact position if present, fix age or manual source, and the chosen emergency category. Let the operator leave the category unspecified or select a broad type such as medical, fire, or stranded. A fresh onboard fix can populate location. Allow manual latitude and longitude entry when needed. Show a stale fix explicitly and require the operator to select it as a last known position before sending it. A grid-only alert can go out immediately when finer location is unavailable. A manually entered distress position does not overwrite the station’s normal on-air grid.

### Accidental-activation interlock

After reviewing the Distress preview and pressing **OK** to confirm, **OK** alone must not initiate transmission. Require the operator to press **PTT three times in a row and hold the third press for 0.7 s** (`0/3 → 1/3 → 2/3 → hold 3/3`) to queue the alert; releasing the third press early steps back to `2/3`, and **BACK** cancels at any point. While on the Distress screen, PTT is intercepted and does not leave RAD950 or key FM voice. Distress bursts always transmit on the **calling dial**, never on a private chat QSY offset.

Use a self-contained alert and an optional position burst on the same dial, profile, and subchannel. A listener displays the alert immediately. The position burst follows in the next eligible transmit turn. For the first prototype use the **10 s** profile, with the position burst two slots after the alert when the radio can transmit on that slot. The alert gets through even if the second burst is erased. A position burst that cannot be associated with an alert supplies no displayed coordinates.

| Proposed frame | Fields after the 10-bit extension header | Total payload |
| --- | --- | ---: |
| **Alert** | 28-bit sender, 15-bit GRID4 or unknown, 4-bit emergency category, 2-bit location source, 5-bit alert sequence | 64 bits |
| **Position** | 21-bit latitude, 22-bit longitude, 11-bit association checksum | 64 bits |

The proposed header allocates six bits to the out-of-range sender marker, two to the extension family, and two to the distress frame type. Coordinate fields use unsigned offsets from −90° latitude and −180° longitude in **0.0001°** steps. This represents roughly **11 m** per step north–south, with east–west spacing decreasing with latitude. The step size describes the encoding. GPS accuracy depends on the fix. Validate coordinate ranges and normalize +180° longitude to −180°. The alert’s location source distinguishes **unavailable**, **fresh GPS**, **manual**, and **last known GPS**. The emergency category is optional and defaults to **unspecified**.

Validate the normal CRC on each burst. Compute the association checksum over the complete alert payload, alert UTC slot, window and subchannel, and encoded coordinates. Accept a position only for an unambiguous alert on the expected receive turn. Display its decimal degrees, GPS/manual/last-known source and map marker if available. Check that the position falls in the advertised GRID4 when both are present, using the same canonical position for grid and coordinate encoding. Treat a disagreement as a conflict and keep the original alert visible. An 11-bit association checksum reduces accidental pairing. It is no proof of the sender’s identity. Test intentional collisions and strengthen the format if the observed association risk warrants more bits or a further frame.

The 28-bit sender field in this two-burst experiment carries a standard call. A decorated identity requires a separately validated identity extension. Show the actual outgoing identity in the preview and never silently drop the prefix or suffix. For the first bench tests use a standard call and clearly labeled test data.

On receive, mark the alert visibly even while the operator is monitoring a chat or Run batch. Show **Alert received**, **Position pending**, or **Position linked**. Repeated alerts refresh the existing entry. Keep any receive acknowledgment operator-initiated so a group of listeners does not pile up on the sender. A receive acknowledgment means the alert or position was copied. It does not mean anyone has contacted emergency services. Keep distress reception separate from worked-grid and contact statistics. Use the existing timing, RF path, and TX limits, with an eligible distress alert taking priority over queued optional conversation traffic. [Firmware operating design](rad950-firmware-feature.md) · [Existing TX and slot limits](rad950-design-v2.md)

## Literal names and short text

For the initial experiment, use a defined 6-bit alphabet with uppercase Latin letters, digits, spaces and common punctuation. Show the normalized text before sending. Document which characters the alphabet supports. Use a literal subtype to distinguish a name from a chat line.

One illustrative 38-bit fragment uses 3 bits for index, 1 for final-fragment flag, 3 for character count, 30 for **five characters**, and 1 for literal subtype. There is room for eight fragments and **40 characters** per message. Zero-pad unused character positions, validate the count, and retain incomplete fragments only within the active session. At five characters per burst, **HEY MATT HOW HAVE YOU BEEN?** takes **six bursts**. With one sender transmitting every other 10 s slot, the sixth burst ends about **109 seconds** after the first begins, plus delivery acknowledgment and any retries. The phrase **How have you been?** uses one burst through the codebook.

Acknowledge a complete message once. The receiver can return a bitmap of received fragments so a sender can resend missing pieces, with a bounded retry count. Validate fragment index, final-fragment position, session, and sequence before assembling text. Repeated fragments must neither redisplay a delivered message nor replace a different valid fragment at the same position. Expire incomplete messages visibly. The first prototype keeps one message outstanding per direction.

## Session binding, Chat QSY, and coexistence

Establish chat by invitation and acceptance in the context of an addressed contact on the **calling dial** (e.g. `28.078 MHz` on 10 m, `24.919 MHz` on 12 m, `21.078 MHz` on 15 m, `18.104 MHz` on 17 m). Negotiating `Invite` and `Accept` on the calling dial ensures neither radio leaves the calling channel if the peer declines or misses the invitation.

### Chat QSY convention (off the calling frequency)

To keep the calling channel clear for CQs, short QSOs, and standalone Distress alerts, accepted conversations automatically QSY to a paired chat dial above the calling passband:

| Band | Calling dial (CQ / QSO / Distress) | Default chat dial (`+4 kHz`) | Overflow chat channels |
| --- | --- | --- | --- |
| 10 m | 28.078 MHz | **28.082 MHz** | +8 kHz (28.086), +12 kHz (28.090) |
| 12 m | 24.919 MHz | **24.923 MHz** | +8 kHz (24.927), +12 kHz (24.931) |
| 15 m | 21.078 MHz | **21.082 MHz** | +8 kHz (21.086), +12 kHz (21.090) |
| 17 m | 18.104 MHz | **18.108 MHz** | +8 kHz (18.112), +12 kHz (18.116) |

`ChatCtrl::Invite` carries a 3-bit `qsy_step` (`0` = stay on current dial for single-window bench tests, `1` = `+4 kHz` default, `2` = `+8 kHz`, `3` = `+12 kHz`) and a 3-bit `subch` (`0..=7`, defaulting to `session_ref % 8` so concurrent chats on `+4 kHz` spread across the eight 225 Hz subchannels). Once `Accept` is exchanged, both radios automatically retune to `calling_dial + qsy_step × 4 kHz` and display the active chat dial in the header. On `End chat` (or session expiry), both radios automatically return to the calling dial. Standalone **Distress** alerts always transmit on the calling dial, never on a private chat offset.

Bind the chat session to both full identities, the window, profile, an agreed channel and turn, and the codebook version. Both radios know which direction may transmit on each parity. A 12-bit wire reference is a compact label within that context. Collisions are possible. Require a validated opening exchange before accepting short references, test concurrent chats and ambiguity, and discard a frame when more than one session matches. If field tests show that the reference is too short, widen it and reduce the card budget. A reference or physical CRC supplies no sender authentication.

With three sequence bits, allocate at most eight message numbers per direction before renewing the session. Do not recycle a sequence within the period in which a delayed or repeated fragment could still arrive. Acknowledge topic cards too, with duplicate suppression. An unsupported codebook, card type, or phrase ID should produce a clear status without guessing its meaning. Older radios continue to receive ordinary v2 traffic.

The first prototype uses an explicit **Stop chat** action and normal RAD950 TX and timing checks. Use even and odd turns on the 10 s profile to stay within the existing TX slot budget. Longer profiles may combine repeated copies of the **same logical message** and should derive expiry from eligible turns as well as elapsed time. Keep chat participation separate from a Run-mode batch so conversation traffic cannot turn a batch grant into a chat invitation. [Run mode proposal](rad950-run-mode-addendum.md) · [Turn-length proposal](rad950-turn-length-addendum.md)

## Prototype acceptance

1. **Host interoperability.** Publish a one-burst station card, question, phrase, name and multi-fragment greeting as worked vectors. Decode them in Ferrite and the WSJT-X RAD950 branch. Verify unknown codes, malformed values and older decoders’ handling of extension frames.
2. **Delivery under loss.** Erase and reorder greeting fragments, lose the final acknowledgment, and restart one end of a chat. Verify selective retransmission, exact reassembly, one display entry per delivered message, and no log or grid credit. Exercise two contacts on the same window and an intentionally colliding short reference, including stations sharing a base call with different suffixes.
3. **Two-radio experience.** Complete a normal contact, send a station card in one turn, request and answer a weather card, then exchange a literal introduction. Record elapsed delivery time, turns used, and what the screen shows when a message cannot be delivered. Compare an ordinary typed greeting with the equivalent phrase plus name to decide which topics deserve codebook space.
4. **Rendezvous behavior.** Agree to a QSO across UTC midnight and verify both reminders name the same dial, profile and subchannel after restart. Lose an offer or an acceptance and verify the operator can retry without creating duplicate reminders. Move a completed test contact to a second 10 m window, then complete a fresh QSO there. Check late accepts, unsupported bands or profiles, a late or absent peer at the destination, and a pending move while other Run callers stay assigned. Verify that only completed QSOs affect the log and worked-grid count.
5. **Distress reception.** Decode a standalone alert in Monitor, then link its precise position without a chat session. Exercise category omitted, no GPS, stale GPS and manually entered location. Erase either burst, introduce a different alert on the next subchannel, and replay a stale position. Verify that the initial alert stays visible, ambiguous coordinates stay unlinked, and duplicate bursts create one incident entry. Check coordinate boundaries and the source label on the map. Use a conducted or host replay fixture for these trials.
