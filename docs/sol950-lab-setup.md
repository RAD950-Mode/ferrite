# SOL950 conducted RF lab setup

Use the HackRF One as the source for the Radtel receive test, then as the receiver for a sample of the Radtel transmitter. The host controls both instruments over USB. The 50 Ω coax carries RF. The first result to obtain is a measured audio-frequency shift when the SI4732 BFO changes. A decoded CQ is the second result. The Radtel transmit waveform is a separate experiment. [SOL950 specification](sol950-design-v2.md)

## Wire the load and forward sample

With both transmitters idle, identify the coupler ports from the model’s diagram. Wire the main line once and leave the dummy load connected for both experiments.

```text
RT-950 Pro antenna jack ──► IN [directional coupler] OUT / THRU ──► 50 Ω dummy load
                                      │
                                    FWD / CPL ──► fixed pad(s) ──► HackRF ANT
                                      │
                        ISO / REV, if exposed ──► rated 50 Ω termination
```

Arrows show power flow during **Radtel TX**. Most power goes straight through to the dummy load. The FWD port supplies a reduced sample to the HackRF. A three-port coupler commonly terminates its isolated port internally. On a four-port model terminate the unused sample port as its datasheet specifies. A wattmeter bridge with a rectified detector or DC meter output cannot supply the required RF sample. [Mini-Circuits directional coupler application note](https://www.minicircuits.com/pages/pdfs/coup7-2.pdf)

For **Radtel RX**, a passive reciprocal coupler can use the same wiring in reverse along the sample path. The HackRF feeds the pads and FWD port, and a coupled signal emerges at IN toward the Radtel. Verify that the model supports this use and covers **both 14.075 and 28.186 MHz**, including its coupled-port input rating. If only 28 MHz is characterized, the 14 MHz control requires a separately characterized injection path. Coupling can change with frequency. An apparent loss at 28 MHz is meaningful only after accounting for that change.

Before connecting the HackRF, record the coupler model, port labels, coupling and tolerance at each frequency, main-line power rating, pad values and ratings, and dummy-load rating and permitted key-down time. The exact parts on this bench have yet to be identified. A low-power through measurement with a calibrated source and meter or a calibrated VNA can establish the assembled path loss; terminate the other ports as drawn. Confirm the coupler and load can handle the radio’s maximum output for the planned burst and repetition rate. Use 50 Ω coax, pads and adapters throughout.

## Select the attenuation

**For a 10 W radio and a 20 dB forward coupler, start with another 50 dB of fixed attenuation on the sample branch.** That gives 70 dB nominal loss and about **−30 dBm at the HackRF**. The 10 W figure is a planning assumption based on Radtel’s product rating. Actual 28 MHz output depends on the radio, power setting and supply. Great Scott Gadgets specifies **−5 dBm maximum input** for HackRF One. Keep the external attenuation in place even with receive gain at minimum. [Radtel RT-950 Pro rating](https://www.radtels.com/products/radtel-rt-950-pro-27mhz-tx-rx-10w-walkie-talkie-am-fm-usb-lsb-cw-receive-gps-aprs-wireless-bluetooth-phone-programming-noaa-two-way-radio) · [HackRF One input limit](https://hackrf.readthedocs.io/en/latest/hackrf_one.html#maximum-input-power)

| Forward coupling at 28 MHz | Add on sample branch | First pad sees at nominal 10 W | Nominal HackRF input |
| --- | --- | --- | --- |
| 10 dB | 60 dB | +30 dBm (1 W) | −30 dBm |
| 20 dB | 50 dB (e.g. 30 + 20) | +20 dBm (100 mW) | −30 dBm |
| 30 dB | 40 dB (e.g. 20 + 20) | +10 dBm (10 mW) | −30 dBm |
| 40 dB | 30 dB | 0 dBm (1 mW) | −30 dBm |

Use the **smallest guaranteed losses** and highest credible transmitter power to check the assembled branch. In dBm, transmitter power is `30 + 10 log10(watts)`. The upper bound at the HackRF is `P_TX,max − coupling_min − pad_loss_min`. Give cable loss zero credit for this check. As a bench design target, keep that upper bound at or below **−20 dBm**, leaving at least 15 dB below the input limit. For example, +40 dBm with a 20 ±2 dB coupler and pads totaling at least 48 dB gives at most **−26 dBm**. Use rated continuous power with margin on the first pad, where the RF is largest. A 0.25 W pad cannot be first in the 10 dB coupler example. A pad on the THRU port would face nearly the full radio power.

**For HackRF TX into Radtel RX**, start with the same 70 dB total loss, RF amplifier off, antenna-port bias off and low TX gain. For example, a **measured** −10 dBm HackRF carrier through 70 dB arrives at approximately **−80 dBm**. Gain settings alone do not establish output in dBm. For weaker signals add known attenuation in 10 dB steps. Record the measured source level, path loss and resulting level at the radio jack before quoting sensitivity. Check that another 10 dB pad produces the expected change in the linear region; a signal that persists with the radio input terminated can indicate leakage around the attenuators.

## Radtel receive test

1. **Connect control.** Power the Radtel with a Ferrite bench build, the owner-imported SSB patch and cable mode Auto or Link. Use its USB **programming cable**. Connect the HackRF to a second USB port. From the repo root, set `PORT` to the programming cable’s serial device. Run `hackrf_info`, then `python3 tools/ferrite_link.py "$PORT" hello`. Use the link’s `keys` and `ui` commands to select HF USB at **14.074000 MHz** and confirm the screen. Keep the Radtel receive-only throughout this experiment. Use separate host terminals for HackRF and Ferrite; only one process owns the radio cable at a time. [Ferrite link controls](../tools/ferrite_link.py) · [HF receiver selection](../firmware/src/tasks/radio/hf.rs)
2. **Record the control before retuning.** Set zero BFO and apply a **14.075050 MHz** carrier to put the expected audio tone at 1,050 Hz. Capture a source-off baseline, then five seconds with the carrier on. Example commands below use the local HackRF CLI’s CW mode. Run the source command in its own terminal and stop it with Ctrl-C after the capture.

   ```sh
   python3 tools/ferrite_link.py "$PORT" term "si prop 0100 0000"
   hackrf_transfer -c 64 -f 14075050 -s 8000000 -a 0 -p 0 -x 0
   ```

   ```sh
   python3 tools/ferrite_link.py "$PORT" audio 5 control14.wav --raw 16000
   cargo run -p usol950 --example sol950_wav -- tone control14.wav
   ```

3. **Measure 28 MHz and the BFO sign.** Keep the HF screen on its patched SI path. Issue the raw SI tune below. Repeat the carrier capture with the HackRF source frequency changed to **28186050**. Save the result as `bfo0.wav`. Keep the RF source fixed, issue `si prop 0100 0064` through `term`, then record `bfo100.wav`. Run `tone` on both. A clear source-dependent peak shifting down about 100 Hz establishes that positive BFO lowers the observed USB audio tone; a rise establishes the opposite sign. Compare the two peaks directly to cancel a fixed oscillator offset. The analyzer always selects a strongest bin. Record the source-off result and the signed shift.

   ```sh
   python3 tools/ferrite_link.py "$PORT" term "si 40 80 6e 19 00 00 reply 1" "si prop 0100 0000"
   ```

4. **Inject the CQ.** Restore BFO to zero. Generate the reference with `cargo run -p usol950 --example sol950_wav -- cq NQ6N FN31 1050 cq.wav`. Use an SDR transmit chain to form the analytic signal from this real mono WAV (real audio as I, its Hilbert transform as Q), interpolate to the HackRF sample rate and transmit the **upper sideband** relative to **28.185000 MHz**. Tone 0 should radiate at **28.186050 MHz** and tone 3 at **28.186200 MHz**. A HackRF file transmitter takes interleaved signed 8-bit I/Q; a WAV passed directly to `hackrf_transfer -t` would have the wrong format and spectrum. Launch the WAV about two seconds into a 15 s capture to keep the entire burst comfortably inside the WAV decoder’s search.

   ```sh
   python3 tools/ferrite_link.py "$PORT" audio 15 sol950-rx.wav --raw 16000
   cargo run -p usol950 --example sol950_wav -- decode sol950-rx.wav
   ```

   Save the original WAV, decoded sender/grid, measured tone frequency and SNR, and the cable tool’s dropped-frame, missing-sample and clipping counts. The CQ pass is recovery of **NQ6N FN31**. Characterize weak-signal performance separately with repeated bursts at known levels. [WAV tools](../digimodes/usol950/examples/sol950_wav.rs)

### Prepare a HackRF CQ file

Run the following from the directory containing the generated `cq.wav`, using Python with NumPy and SciPy. It converts the reference to analytic USB, interpolates each block with filter overlap and moves the signal 200 kHz above the SDR center. At 8 MS/s the ten-second file is 160 MB. Generate it before starting the capture.

```python
import numpy as np
from scipy.io import wavfile
from scipy.signal import hilbert, resample_poly

rate, pcm = wavfile.read("cq.wav")
assert rate == 24_000 and pcm.dtype == np.int16 and pcm.shape == (240_000,)
audio = hilbert(pcm.astype(np.float64) / 32768)
guard = 96  # input samples on each side, aligned to the 3-sample decimation phase
padded = np.pad(audio, guard)
with open("cq-plus200k.cs8", "wb") as out:
    for start in range(0, len(audio), 2400):
        part = padded[start:start + 2400 + 2 * guard]
        up = resample_poly(part, 1000, 3)
        iq = up[32000:832000]  # 0.1 s, discard both guard regions
        # 200 kHz makes exactly 20,000 cycles per block at 8 MS/s.
        iq *= np.exp(2j * np.pi * np.arange(len(iq)) / 40)
        pairs = np.column_stack((iq.real, iq.imag))
        np.rint(100 * pairs).clip(-127, 127).astype(np.int8).tofile(out)
```

With the Radtel listening at 28.185000 MHz, start its 15 s capture first, then run the one-shot transmit command in the HackRF terminal. The 27.985 MHz center plus the digital shift puts the suppressed carrier at 28.185 MHz. Keep the same attenuated fixture used for the carrier test; record the CQ signal level separately if measuring sensitivity.

```sh
hackrf_transfer -t cq-plus200k.cs8 -f 27985000 -s 8000000 -a 0 -p 0 -x 0
```

## Radtel transmit test

Stop HackRF TX. Inspect the main line from the radio through the coupler to the load and check the calculated sample power. Start **HackRF RX first**, with RF amp and antenna bias disabled and both receive gains at zero. For example, the following records 15 s of signed 8-bit interleaved I/Q at 8 MS/s (240 MB). Centering 200 kHz below the USB dial puts the expected tones away from the SDR’s center spike.

```sh
hackrf_transfer -r radtel-tx.cs8 -f 27985000 -s 8000000 -n 120000000 -a 0 -p 0 -l 0 -g 0
```

With the radio configured for conducted 10 m TX, use its lowest supported TX power and a short ordinary carrier first. A carrier at 28.186050 MHz should appear at **+201.050 kHz** in the recording. Check that it disappears on unkey and that an additional known pad changes the measured level accordingly. Look for clipping and check capture overruns before increasing receiver gain. Keep the fixed protective pads installed. Switching Ferrite Dry TX off requires a local action on the radio; subsequent remote PTT uses the normal TX checks. [Ferrite Dry TX controls](../crates/ferrite-app/src/term/cmd.rs) · [Remote PTT behavior](../tools/ferrite_link.py)

A carrier capture proves the fixture. For the **SOL950 TX waveform** measurement, `txtest` includes two non-blocking `TMR5`-paced BK4829 direct-PLL modes (respecting all normal TX band/power/Dry-TX checks and restoring registers on unkey). Hardware testing on the RT-950 Pro established that neither frozen Tone 1 (`REG_71 = 0`) nor MCU `DAC1` (`PA4 -> PC12 -> BK4829 mic`, which has an ~22 Hz / ~7.2 ms AC-coupling high-pass) holds static DC across 80 ms symbols, whereas stepping BK4829's `10 Hz`-resolution PLL (`REG_38`/`REG_39`) and latching via `REG_30` VCO toggle produces exact `50.00 Hz` tone steps (`< 0.1 Hz` RMS error):

1. **Verify PLL latching (`sol950-cal`):** Tune the active slot to `28.186000 MHz`, start a 7 s HackRF capture, arm the `500 ms`/step `+500 Hz` staircase (steps `0..4` toggle `REG_30` bit 15 `0x43FA -> 0xC3FA`; steps `4..9` use `tx_on`), and key:

   ```sh
   python3 tools/ferrite_link.py "$PORT" term "txtest sol950-cal 180"
   python3 tools/ferrite_link.py "$PORT" key --seconds 5.2 --slot b
   ```

2. **Radiate and decode the 110-symbol `CQ NQ6N FN31` reference burst (`sol950`):** Tune slot B to `28.18605 MHz` (placing tones `0..3` at `+201,037 .. +201,187 Hz` relative to `27.985 MHz`), start a 13 s HackRF RX capture at `-f 27985000`, arm `txtest sol950`, and key for `10.5 s` (`0.20 s` tone-0 lead-in + `8.80 s` 110-symbol burst at `80 ms`/symbol, `50 Hz`/tone):

   ```sh
   python3 tools/ferrite_link.py "$PORT" term "txtest sol950"
   python3 tools/ferrite_link.py "$PORT" key --seconds 10.5 --slot b
   ```

   Mix the captured `radtel-tx.cs8` down by `-200 kHz` (putting `28.185 MHz` at `0 Hz` USB audio), low-pass, decimate to `24 kHz` mono WAV, and run `cargo run -p usol950 --example sol950_wav -- decode radtel-tx.wav` alongside direct inspection of the four tone centers, symbol duration, power envelope, 99%-power occupied bandwidth, and start-to-end frequency drift. [BK4829 TX driver](../firmware/src/tasks/radio/afskbk.rs) · [Reference modulator](../digimodes/usol950/src/phy.rs)

### Measured bench results (`NQ6N FN31`, 2026-11-18)

- **Experiment 1 (Radtel RX via SI4732 USB + HackRF):**
  - Measured a proportional `+48..+54 ppm` **relative bench frequency offset** between the RT-950 Pro SI4732 clock and the HackRF One clock (`+766 Hz` at 14.074 MHz, `+1,349 Hz` at 28.185 MHz; attributing the split between the two oscillators requires an independently referenced source) and settled the USB `SSB_BFO` sign (`+100 Hz` BFO lowers zero-beat RF by `100 Hz` and raises USB audio by `+100 Hz`; [ssb.rs](../crates/ferrite-core/src/ssb.rs) updated accordingly, keeping per-radio crystal calibration separate):
    - `14.074 MHz` control (`BFO +766 Hz`, HackRF `14.075050 MHz`): source-off noise `-26.1 dB`, carrier `1047.27 Hz` at `+9.6 dB`.
    - `28.185 MHz` (`HackRF 28.186050 MHz`): source-off noise `-26.2 dB`; `BFO +1350 Hz` -> `1050.61 Hz` at `+6.4 dB`; `BFO +1450 Hz` -> `1152.65 Hz` at `+6.0 dB`.
  - Decoded 15 s injected `CQ NQ6N FN31` captures cleanly (`0` lost frames, `0` missing samples, `0` clips) at both `-x 20` (`t=2360 ms | 1049 Hz | +6 dB | CQ NQ6N FN31 (pwr_code=10)`) and `-x 0` (`t=2360 ms | 1049 Hz | +2 dB | CQ NQ6N FN31 (pwr_code=10)`). Hardware sensitivity near the `-16..-18 dB` weak-signal target remains to be measured.
- **Experiment 2 (Radtel TX via BK4829 #1 PLL -> directional coupler -> 30 dB pad -> HackRF):**
  - Eliminated frozen Tone 1 (`REG_71 = 0`, zero modulation) and `DAC1` (`PA4 -> PC12 -> BK4829 mic`, ~22 Hz / ~7.2 ms AC-coupling decay).
  - Verified direct `REG_38/39` PLL stepping in `sol950-cal` (`bit15_only` `0x43FA -> 0xC3FA` on steps `0..4` latched `+500 Hz` steps cleanly with `0.36..0.65` min 0.5 ms narrowband power ratio; `tx_on` on steps `4..9` measured `0.27..0.38`).
  - `txtest sol950` (`slot B 28.18605 MHz` LOW, HackRF `27.985 MHz`, `24 kHz` mono WAV) decoded `t=1400 ms | 1037 Hz | +8 dB | CQ NQ6N FN31 (pwr_code=10)`, measuring tones `1036.93 / 1086.93 / 1136.94 / 1186.92 Hz` (spacings `49.997 / 50.010 / 49.984 Hz`, `0.086 Hz` RMS error), `80.0104 ms`/symbol, `+0.12 Hz` linear relative drift over `8.72 s`, `291.25 Hz` (±500 Hz span) / `426.82 Hz` (noise-subtracted ±2 kHz span) 99% OBW, and brief `< 0.5 ms` VCO re-lock transients at symbol changes (`1 ms` dips `0.30..0.47` at `80 ms` boundaries). Adjacent-channel skirt power in `[1262..1462 Hz]` (`+225 Hz` offset) measured `-23.46 dB` relative to in-band power (`[1000..1220 Hz]`).
- **Captured-TX Replay & `+225 Hz` `+15 dB` Adjacent-Caller Sensitivity Sweep (`usol950::Receiver`, 100 AWGN trials/point with randomized `[-160, +240] ms` slot timing):**
  - Captured Radtel alone reaches a **~`-15.3 dB`** 50% decode threshold (within `~0.1 dB` of the smooth `Modulator` reference, `1–2 dB` short of the `-16..-18 dB` target). Adding a `+15 dB` captured Radtel neighbor `+225 Hz` away degrades weak-frame recovery by only `~0.3 dB` (consistent with `-23.46 + 15 = -8.46 dB` neighbor leakage into the weak passband), supporting retention of the **`225 Hz` subchannel grid**:
    - `-12.0 dB`: `100/100` Radtel alone, `100/100` Synth alone, `100/100` Synth+Radtel(`+225 Hz, +15 dB`), `100/100` Radtel+Synth(`+225 Hz, +15 dB`), `100/100` Synth+Radtel(`+250 Hz, +15 dB`)
    - `-13.0 dB`: `100/100` Radtel alone, `100/100` Synth alone, `99/100` (`+225 Hz`), `99/100` (`Radtel+Synth`), `99/100` (`+250 Hz`)
    - `-14.0 dB`: `97/100` Radtel alone, `91/100` Synth alone, `91/100` (`+225 Hz`), `96/100` (`Radtel+Synth`), `88/100` (`+250 Hz`)
    - `-15.0 dB`: `68/100` Radtel alone, `66/100` Synth alone, `54/100` (`+225 Hz`), `71/100` (`Radtel+Synth`), `62/100` (`+250 Hz`)
    - `-16.0 dB`: `21/100` Radtel alone, `23/100` Synth alone, `13/100` (`+225 Hz`), `21/100` (`Radtel+Synth`), `17/100` (`+250 Hz`)
    - `-17.0 dB`: `1/100` Radtel alone, `7/100` Synth alone, `3/100` (`+225 Hz`), `0/100` (`Radtel+Synth`), `3/100` (`+250 Hz`)
    - `-18.0 dB`: `0/100` across all configurations
