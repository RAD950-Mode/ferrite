# RAD950 ad-hoc rendezvous

**Proposed design addendum.** Give a small group of RAD950 operators a predictable place to meet without arranging a contact in advance. Simultaneous presence on the same dial and profile is the main benefit while adoption is low. Use the ordinary 10-second RAD950 frames and contact exchange. [Air interface](sol950-design-v2.md) · [Turn-length and frequency presets](sol950-turn-length-addendum.md)

## Hourly UTC schedule

| UTC minutes of each hour | Band | Profile |
| --- | --- | --- |
| :00 through :04 | 10 m | 10 s |
| :15 through :19 | 12 m | 10 s |
| :30 through :34 | 15 m | 10 s |
| :45 through :49 | 17 m | 10 s |

A window opens at the named quarter hour. The next five minutes provide 30 turns. Publish one shared USB calling dial per band for the pilot and put the same preset names, numeric dials, and schedule in Ferrite, desktop software, and the operating guide. Share the full preset when coordinating a trial. Keep the calling dial available for ordinary RAD950 contacts throughout the hour.

## Calling and listening

1. Enter the window in **Monitor**. Listen across the existing RAD950 passband and show stations as they are decoded. The operator can answer a CQ directly.
2. With **Call & listen** armed and no directed exchange pending, each eligible even UTC slot is an independent **50/50 call-or-listen decision**. For a 10-second profile, the possible CQ slot starts in each minute are seconds 00, 20, and 40. Make a fresh choice each time. Radios need independent draws so a simultaneous call does not repeat in lockstep. On a CQ turn, use an existing RAD950 subchannel. Prefer an observed clear one and vary the choice where several are clear.
3. When an operator selects a decoded CQ, send the directed reply on the next eligible odd slot. Directed traffic takes priority over a new CQ. Keep the transmit parity of an established exchange through completion and retries. Leave an intervening receive turn between transmissions, including when switching between calling and answering.
4. Close fresh rendezvous CQs when the window ends. Stay on the dial while a directed exchange or its retry linger is active, then resume monitoring. For example, a CQ at 14:19:40 UTC can receive a 14:19:50 reply and send its final report at 14:20:00.

Two stations that both transmit on every even slot cannot hear each other. Under an ideal two-station model with independent call probability $p$ and reliable reception when one listens, an idle even slot gives one transmitter and one listener with probability $2p(1-p)$. The maximum is 50% at $p=0.5$. Actual discovery also depends on propagation, interference, and decoding. Eight audio subchannels give listeners room to hear simultaneous callers. A transmitting station still needs a receive turn to discover someone else.

Use the half-duplex turn rule and the radio's existing RF-path interlocks. Alternate 10-second slots already give a full-rate cadence of three bursts per rolling minute. The separate firmware counter supplies no additional rendezvous scheduling constraint. Leave the existing relay guard in the RF path. [RAD950 scheduler](../crates/ferrite-core/src/sol950.rs) · [RF-path guard](../crates/ferrite-core/src/path.rs)

## Radio setup

Add **Rendezvous** to RAD950 Operate. Show **Now** or **Next**, the selected band and USB dial, and a UTC countdown. The setup offers four independent band selections so an operator can include only bands for which they have a suitable antenna. A 10 m and 17 m operator sees :00 and :45 in their schedule. The 12 m and 15 m turns remain available to operators who selected those bands.

**Follow schedule** tunes and monitors selected bands while RAD950 is open and the operator has enabled it. Skip unselected bands and leave the current dial alone during their windows. Opening a scheduled window brings up its activity list. **Call & listen** is an explicit, attended action for the current window, with a visible state and an immediate stop control. End the arm when that window closes. Completing an in-flight contact takes precedence over automatic retuning. Display UTC readiness and use the existing transmit checks for a call or reply.

An operator can also follow the published schedule manually, tune the calling dial, and make an ordinary RAD950 contact with current equipment.

## Launch trial

Start with two operators, then repeat with three to five and with at least one operator who selects only some of the bands. Record time to first CQ decode and first directed reply, contacts completed at both stations within a window, and slots lost to simultaneous calls. Include late arrivals, a missed final report, an exchange that crosses the window boundary, and an unselected band. Verify normal calling and listening outside rendezvous windows.

The pilot uses a fixed call probability and no load-dependent backoff. Revisit that choice when observed contention warrants it. If the calling frequencies gain enough routine activity to make stations easy to find without by-the-clock coordination, the schedule can remain an optional meetup. Higher-volume tools such as Run mode can evolve separately. [Run mode proposal](sol950-run-mode-addendum.md)
