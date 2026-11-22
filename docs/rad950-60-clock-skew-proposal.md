# RAD950-60 clock-skew tolerance proposal

Make RAD950-60 decode reliably when the two stations' clocks disagree, without changing the v1 wire format. Figures marked "estimate" are derived from the code and the existing measurements and have not been measured themselves.

## Recommendation

1. Measure the real skew between stations and split it into fixed latency and clock error.
2. Calibrate out the fixed latency in the handheld's transmit and receive paths and in the desktop audio path. This costs no RAM and helps every v1 station.
3. Recentre the handheld's start-time search, which today accepts a sender up to 320 ms early but only 80 ms late.
4. Give the handheld a second FFT timing phase, the fix the desktop already ships. This needs about 8.7 to 10.9 KB of RAM that the decoder slot does not have, so it depends on a firmware memory audit.
5. Until that RAM exists, ship a zero-RAM phase policy so no station is permanently undecodable.

A v2 wire format is not needed to fix skew. It should be judged only on sensitivity, under the gate in the [long-turn proposal](rad950-long-turn-v2-proposal.md).

## The problem

The handheld evaluates one FFT window per 80 ms symbol, on a grid fixed by its own clock. Two separate failures follow from skew.

- **Half-symbol cliff.** When a sender's symbol edges fall 30 to 40 ms from the receiver's grid (modulo 80 ms), each window straddles two symbols. The conducted `WP2Z` capture decoded 4/4 aligned and 0/4 at +40 ms. If skew is spread evenly across the 80 ms symbol, about a quarter of senders sit in the dead zone, with reduced sensitivity on either side of it (estimate).
- **Late-side search limit.** The fold holds 116 symbols for a 110-symbol burst, and the search tries start symbols 1 through 6 with 5 as nominal. That covers −320 ms to +80 ms. A sender more than 80 ms late is never tested at any SNR. The 10-second receiver searches ±320 ms. The transmit gate allows ±150 ms, so a compliant late sender can fall outside the handheld's window.

The desktop receiver already removes the cliff by running phases at 0 and +40 ms. The handheld cannot hold two folds in its 13 KiB slot.

## Requirement

For any relative skew within ±200 ms (the ±150 ms transmit gate plus 50 ms of receiver clock uncertainty):

- no offset at which decode probability collapses
- at most 1.5 dB of threshold loss at the worst offset, relative to an aligned signal
- at most 0.2 dB of loss for an aligned signal, relative to today's `CompactReceiver60`

## Step 1: measure the skew

The plan depends on how large the skew is and where it comes from, and neither is known.

- **Loss curve.** On host replay, sweep the sender's offset from −200 to +200 ms in 5 ms steps at three SNRs. This gives the width of the dead zone and of its shoulders, and confirms the late-side limit.
- **On-air distribution.** The desktop dual-phase receiver knows which phase won. Log that, with the coarse start symbol, for every decode, and build a histogram of offset by sender type: handheld on live GPS, handheld in holdover, desktop.
- **Handheld transmit latency.** Capture a handheld transmission on a HackRF whose sample clock is tied to a GPS time reference. Measure the first symbol edge against the nominal +400 ms. Repeat across power levels and bands, since the relay sequence and PA ramp may differ.
- **Handheld receive latency.** Send a burst with a known start time into the antenna port and find the offset at which the handheld's sync score peaks. This is the group delay of the receiver IC, the audio path, the ADC blocks, and the decimator.
- **GPS timestamp latency.** The handheld disciplines to the NMEA burst edge and budgets 50 ms of uncertainty. Measure the edge against true time over many fixes to separate its fixed delay from its jitter.

A fixed latency matters more than random error. If every handheld transmits the same amount late and receives with the same delay, every handheld-to-handheld contact sees the same offset, and that offset could sit on the cliff every time.

## Step 2: remove the fixed offsets

- Trim the handheld's transmit start so the on-air symbol edge lands on nominal.
- Shift the handheld's receive grid by its measured receive latency.
- Subtract the fixed part of the NMEA edge delay, and lower the uncertainty budget to the measured jitter.
- On the desktop, compensate the audio input and output latency, and warn the operator when measured offsets from other stations are consistently large.

After this step the remaining skew is true clock error. On live GPS that may be small enough that the cliff is rarely reached. Holdover will still drift across it, so the receiver work below is still needed.

## Step 3: recentre the start-time search

Delay the fold's time origin so the six start slots sit evenly around nominal, giving roughly −200 to +200 ms once the second phase is included. The fold stays 116 symbols, so this costs no RAM. Confirm the last folded symbol still ends before the subslot boundary: 116 symbols take 9.28 s, which leaves room for a 120 to 160 ms delay.

## Step 4: a second timing phase on the handheld

Run a second FFT per symbol, half a symbol later, into a second fold, and search both. This is the desktop design, which has already been shown to remove the cliff.

- **Samples:** the existing 480-sample ring already holds what the second window needs.
- **Ingest time:** the second FFT lands five audio blocks after the first, so the worst block should stay near the measured 3.6 ms of 8 ms.
- **Minute-end time:** roughly doubles from the measured 0.74 s to about 1.5 s of a 10 s turnaround (estimate).
- **RAM:** this is the whole cost. Non-fold state is 1,053 bytes today.

| Layout | Decoder state | Extra over today |
| --- | ---: | ---: |
| Today: one 5-bit fold and the compact table | 13,164 B | 0 |
| A: two 5-bit folds, no compact table | 22,803 B | 9,639 B |
| B: 5-bit fold, 4-bit second-phase fold, compact table | 21,864 B | 8,700 B |
| C: two 5-bit folds and the compact table | 24,039 B | 10,875 B |

Layout B is the first choice. The 4-bit fold measured 0.28 dB worse isolated and 0.84 dB worse beside a strong neighbor. As a second phase, that loss applies only to senders that currently do not decode at all. Aligned senders keep today's fold and compact table unchanged.

### Memory audit

The measured stack margin is 4,368 bytes, so layout B needs about 4.3 KB found elsewhere. Peak main-stack use is 25,812 bytes, which is the first place to look.

- Identify the call path that sets the stack high-water mark and whether it can occur while RAD950-60 is receiving.
- List static allocations that are idle in RAD950-60 mode and could share storage with the second fold.
- Move the 1,024-byte FFT input buffer out of the ingest stack frame if it is on the peak path.
- Set the gate before starting: layout B fits with at least 2 KB of measured stack margin remaining, or this step stops and the fallback below is evaluated.

## Interim: a zero-RAM phase policy

Until the second fold fits, choose one phase per minute.

- **In a contact:** keep the phase at which the peer last decoded.
- **Monitoring or calling CQ:** alternate between 0 and +40 ms each minute.

A sender on the cliff then decodes every other minute, where today it never decodes. The cost is that an aligned sender is also missed every other minute while monitoring. If step 2 concentrates senders near zero offset, weight the alternation toward phase 0. This is a mitigation with a real cost and should be removed once step 4 ships.

## Fallback if the RAM cannot be found

Replace the 80 ms window with two 40 ms windows per symbol, using a 256-point transform. The fold becomes 232 half-symbols by 75 bins, which is the same 17,400 cells and the same 10,875 bytes. Any symbol offset then has at least one clean half-symbol window, so the cliff disappears with no extra RAM.

The costs are real and need measuring before this is considered:

- about 1 to 1.5 dB of sensitivity for aligned senders, from combining two half-symbol windows non-coherently (estimate)
- weaker rejection of a strong neighbor, because the bins are twice as wide
- the compact table would double in size and no longer fit, so it would need redesign or removal

This trades sensitivity for skew tolerance everywhere. Take it only if step 4 is impossible and steps 2 and 3 leave too many misses.

## Acceptance

Evaluate on held-out replay with at least 200 paired trials per point, then confirm on the radio.

- **Skew:** the 50% decode threshold at every offset from −200 to +200 ms in 10 ms steps meets the requirement above.
- **Aligned regression:** at most 0.2 dB against today's handheld receiver, isolated and beside a +18 dB neighbor.
- **False accepts:** zero unexpected frames in 1,000 independent noise-only minutes. The second phase doubles the decode attempts, so this must be re-run.
- **Radio:** zero ingest overruns, minute-end decode complete well before the next minute, and at least 2 KB of measured stack margin.
- **End to end:** two handhelds on independent GPS fixes, and one in holdover for 30 minutes, complete contacts across the full offset range the clocks produce.

## Order of work

1. Host loss curve and the late-side check (step 1, first bullet). This needs no hardware.
2. Latency and on-air skew measurements (rest of step 1).
3. Latency calibration and the search recentring (steps 2 and 3). Both are small and ship independently.
4. Interim phase policy, if step 1 shows meaningful residual skew.
5. Memory audit, then the second phase (step 4).
6. Fallback evaluation only if the audit fails.
