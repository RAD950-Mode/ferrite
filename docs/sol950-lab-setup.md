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
  - Decoded 15 s injected `CQ NQ6N FN31` captures cleanly (`0` lost frames, `0` missing samples, `0` clips) at both `-x 20` (`t=2360 ms | 1049 Hz | +6 dB` reported / `+23.6 dB` measured in-band `SNR_2500`) and `-x 0` (`t=2360 ms | 1049 Hz | +2 dB` reported / `+4.7 dB` measured `SNR_2500`).
- **Experiment 2 (Radtel TX via BK4829 #1 PLL -> directional coupler -> 30 dB pad -> HackRF):**
  - Eliminated frozen Tone 1 (`REG_71 = 0`, zero modulation) and `DAC1` (`PA4 -> PC12 -> BK4829 mic`, ~22 Hz / ~7.2 ms AC-coupling decay).
  - Verified direct `REG_38/39` PLL stepping in `sol950-cal` (`bit15_only` `0x43FA -> 0xC3FA` on steps `0..4` latched `+500 Hz` steps cleanly with `0.36..0.65` min 0.5 ms narrowband power ratio; `tx_on` on steps `4..9` measured `0.27..0.38`).
  - `txtest sol950` (`slot B 28.18605 MHz` LOW, HackRF `27.985 MHz`, `24 kHz` mono WAV) decoded `t=1400 ms | 1037 Hz | +8 dB | CQ NQ6N FN31 (pwr_code=10)`, measuring tones `1036.93 / 1086.93 / 1136.94 / 1186.92 Hz` (spacings `49.997 / 50.010 / 49.984 Hz`, `0.086 Hz` RMS error), `80.0104 ms`/symbol, `+0.12 Hz` linear relative drift over `8.72 s`, `291.25 Hz` (±500 Hz span) / `426.82 Hz` (noise-subtracted ±2 kHz span) 99% OBW, and brief `< 0.5 ms` VCO re-lock transients at symbol changes (`1 ms` dips `0.30..0.47` at `80 ms` boundaries). Adjacent-channel skirt power in `[1262..1462 Hz]` (`+225 Hz` offset) measured `-23.46 dB` relative to in-band power (`[1000..1220 Hz]`).
- **Reproducible Capture Provenance, Host Replay & `+225 Hz` `+15 dB` Sensitivity Sweep:**
  - **Capture & normalization (`tools/sol950_replay.py`, `sol950_wav sweep`):**
    - Source capture on bench host `ronald:/tmp/sol950-lab/`: `radtel-tx.cs8` (`208,000,000 B`, sha256 `d8e8cadb7098a19cd3c3f3d6ddd6c70236b72ff2ba5048a0a467a560d6299c86`), downconverted by `-200 kHz`, Blackman-windowed sinc LPF (`fc = 4 kHz`), decimated `8 MS/s -> 24 kHz` mono PCM WAV `radtel-tx.wav` (sha256 `d2b60669f9c7cde7541ad2322bf2e9d24ebc7ec2e531281da7890988d99a1732`, `off_rms = 1232.8`, `on_rms = 7717.6`, capture-noise-subtracted `sig_rms = 7618.5`).
    - Extracted `9.00 s` active burst (`1.2154 s .. 10.2154 s`, `5 ms` edge taper), normalized active signal RMS to `10,000` (`radtel_burst_0.wav`, sha256 `787e5f6768de5cfb61164377094115250e87207b6334aeea22adf62a98682169`), and FFT-analytic-shifted by `+225 Hz` (`radtel_burst_+225.wav`, sha256 `a5183359624c8397e8ca35a6f55e6df8b1552a0962c54cca580af07652cc0548`) and `+250 Hz` (`radtel_burst_+250.wav`, sha256 `8f6b33a09c27f438d3cd5d8efed524f3269eb7b8fbde992f33a8b79d88f31710`).
    - `sol950_wav sweep` (`digimodes/usol950/examples/sol950_wav.rs`) uses `P_n(2500) = sigma^2 * 2500/12000` (`sigma = 1200` @ `24 kHz`), distinct payloads (`CQ NQ6N FN31` vs `RX K1ABC -> NQ6N`), weak-payload-only scoring, and deterministic seeds `0x2000_0000_0000_0001 + si * 997`:
    - `-12.0 dB`: `100/100` Radtel alone, `100/100` Synth alone, `100/100` Synth+Radtel(`+225 Hz, +15 dB`), `100/100` Radtel+Synth(`+225 Hz, +15 dB`), `100/100` Synth+Radtel(`+250 Hz, +15 dB`)
    - `-13.0 dB`: `100/100` Radtel alone, `100/100` Synth alone, `99/100` (`+225 Hz`), `99/100` (`Radtel+Synth`), `99/100` (`+250 Hz`)
    - `-14.0 dB`: `97/100` Radtel alone, `91/100` Synth alone, `91/100` (`+225 Hz`), `96/100` (`Radtel+Synth`), `88/100` (`+250 Hz`)
    - `-15.0 dB`: `68/100` Radtel alone, `66/100` Synth alone, `54/100` (`+225 Hz`), `71/100` (`Radtel+Synth`), `62/100` (`+250 Hz`)
    - `-16.0 dB`: `21/100` Radtel alone, `23/100` Synth alone, `13/100` (`+225 Hz`), `21/100` (`Radtel+Synth`), `17/100` (`+250 Hz`)
    - `-17.0 dB`: `1/100` Radtel alone, `7/100` Synth alone, `3/100` (`+225 Hz`), `0/100` (`Radtel+Synth`), `3/100` (`+250 Hz`)
    - `-18.0 dB`: `0/100` across all configurations
    - **Interpolated 50% thresholds:** `-15.38 dB` (captured Radtel alone), `-15.37 dB` (synthetic weak alone), `-15.10 dB` (with `+15 dB` Radtel neighbor at `+225 Hz` -> **`0.27 dB`** estimated penalty), `-15.27 dB` (`+250 Hz`). With `100` trials per point, the `~0.17 dB` gap between `225 Hz` and `250 Hz` is within sampling variance; retaining `225 Hz` preserves `8` nominal subchannels in `600–2,400 Hz`.
- **Through-the-Radio SI4732 `PA2` vs. Direct Host Replay (`tools/sol950_replay.py`, `HackRF -a 1 -x 44` into `28.185 MHz` USB `BFO +1350 Hz`):**
  - **Initial unpaired sweep (8 slots/point, `-14 / -15 / -16 dB`, `0` lost frames/samples/clips):**
    - `alone` (`CQ NQ6N FN31`, `seed = 0x5019_5000 + si`): Direct Host `7/8, 5/8, 2/8` vs. Radtel `PA2` (`0x9012`) `7/8, 2/8, 2/8` (identical with exact FFT `16k -> 24k` sinc resampling).
    - `adj225` (`RX K1ABC -> NQ6N RX -14 dB` weak payload from `RX_K1ABC_TONES` + `+15 dB` captured Radtel `CQ` at `+225 Hz`, `seed = 0x5019_5064 + si`): Direct Host `8/8, 4/8, 0/8` vs. Radtel `PA2` (`0x9012`) `4/8, 2/8, 0/8` (+ `8/8` loud `CQ`; identical with FFT `16k -> 24k` sinc resampling). Note these two initial suites varied weak payload, waveform source, and noise seed together, so each supports its own Host-vs-`PA2` comparison rather than isolating neighbor impact.
  - **Diagnostic Experiment 1 — Host Blind vs. Grid-Quantized Oracle Acquisition (`sol950_wav sweep --oracle`, 100 trials/point, `seed_base = 0x2000_0000_0000_0001`):**
    - Pins one known track on the current `40 ms` / half-bin (`5.86 Hz`) receiver grid (does not remove symbol-boundary or frequency-grid quantization):
    - Synthetic `RX K1ABC -> NQ6N -15 dB` alone: blind `-15.37 dB` (`91/100` at `-14`, `66/100` at `-15`, `23/100` at `-16`) vs. oracle **`-15.71 dB`** (`100/100`, `87/100`, `35/100`; recovered `9/9`, `23/34`, `17/77` of blind misses -> `+0.34 dB`).
    - Captured Radtel `CQ NQ6N FN31` alone: blind `-15.38 dB` (`97/100`, `68/100`, `21/100`) vs. oracle **`-15.60 dB`** (`100/100`, `83/100`, `28/100`; recovered `3/3`, `17/32`, `8/79` -> `+0.22 dB`).
    - Weak `RX` + `+15 dB` captured Radtel `@ +225 Hz`: blind `-15.10 dB` (`91/100`, `54/100`, `13/100`) vs. oracle **`-15.45 dB`** (`98/100`, `68/100`, `28/100`; recovered `8/9`, `15/46`, `17/87` -> `+0.35 dB`).
  - **Diagnostic Experiment 2 — Strictly Paired Weak-Only vs. Weak+Neighbor & AVC On vs. Off (`make-paired` / `run-paired`, `-14.0 dB`, 8 slots, `seed = 0x5019_6000`):**
    - Reused one Rust-encoded weak burst (`sol950_wav rx K1ABC NQ6N -15 0 1037`) and one `[200..2800 Hz]` AWGN realization per slot across `paired_weak` and `paired_adj` (adding only `+15 dB` captured Radtel `CQ` at `+225 Hz`, same `iq_gain`):
      - **Direct Host:** `weak_only` `7/8` (missed slot `3`); `weak+adj` `8/8` weak (+ `8/8` loud `CQ`).
      - **Radtel `PA2` AVC ON (`0x9012`, `vol=56`):** `weak_only` **`8/8`** (`rms=3449`, `crest=8.69`, `0` clips); `weak+adj` **`7/8`** weak (+ `8/8` loud `CQ`, `rms=4180`, `crest=7.61`, `0` clips). On the single missed weak slot (slot `1`), the neighbor's sync lag is `+255 ms`, so its `~200 ms` tone-0 lead-in arrives near `+55 ms` into the weak frame: aligning `100 ms` RMS to weak `S0` shows `Host` rising `0.64x -> 0.84x -> 0.97x` (RF onset), while `PA2` rises `0.74x -> 0.92x -> 1.16x` (`+1.3 dB` AVC overshoot at `+0.10..+0.20 s`) before settling to `1.02x` by `+0.50 s`.
      - **Radtel `PA2` AVC OFF (`0x8012`, `vol=46`):** `weak_only` `5/8` (`rms=5409`, `max=12448`, **crest factor `2.30`**); `weak+adj` `0/8` weak (+ `8/8` loud `CQ`, `rms=5865`, `max=12240`, **crest factor `2.09`**). The low crest factor and flat tops are consistent with upstream limiting/saturation prior to `PA2` (exact internal stage inside SI4732 cannot be located from `PA2` alone), establishing **AVC ON (`0x9012`)** for this setup.
  - **Diagnostic Experiment 3 — Single-Radio Bit-Confidence, Blind->Ref Decomposition & Two-Radio Cooperative Soft Combining (`sol950_wav coop`, 100 trials/point, `seed_base = 0x3000_0000_0000_0001`):**
    - Compared current `Receiver` grid-oracle LLRs (`cur_oracle`) against an exact-boundary 4-tone matched-filter reference demodulator with local sync-estimated `N0` and `ln I_0` soft LLRs (`ref_demod`), both feeding the identical `(184, 78)` LDPC BP decoder:
      - **Single-radio known-track (`-15 / -16 / -17 / -18 / -19 / -20 dB`, `0` frames lost):**
        - Synthetic `RX`: `cur_oracle` `83, 37, 4, 0, 0, 0` vs. `ref_demod` **`100, 100, 96, 42, 7, 0`** (`interp_50pct = -17.85 dB`).
        - Captured Radtel `CQ`: `cur_oracle` `82, 44, 3, 0, 0, 0` vs. `ref_demod` **`100, 100, 99, 69, 11, 1`** (`interp_50pct = -18.33 dB`).
      - **Blind acquisition -> Reference demodulation decomposition (`blind_cur` / `in_cands` / `ref@blind` / `ref@refined` / `ref@oracle`):**
        - Synthetic `RX`: `-15 dB`: `63 / 100 / 57 / 92 / 100`; `-16 dB`: `26 / 97 / 41 / 81 / 100`; `-17 dB`: `1 / 85 / 26 / 41 / 96`; `-18 dB`: `0 / 71 / 8 / 9 / 42`; `-19 dB`: `0 / 48 / 1 / 1 / 7`.
        - Captured Radtel `CQ`: `-15 dB`: `68 / 100 / 51 / 92 / 100`; `-16 dB`: `28 / 97 / 40 / 82 / 100`; `-17 dB`: `1 / 87 / 22 / 50 / 99`; `-18 dB`: `0 / 71 / 9 / 13 / 69`; `-19 dB`: `0 / 63 / 1 / 0 / 11`.
        - Shows blind `S0+S1` acquisition already admits the true caller `85–87%` of the time at `-17 dB` (`71%` at `-18 dB`); local `S0+S1` timing/frequency refinement (`±20 ms`, `±4 Hz`) + `ref_demod` lifts blind recovery at `-17 dB` from `1/100` to **`50/100`** on the captured Radtel burst (`82/100` at `-16 dB`).
      - **Priority-ordered blind refinement work budget (`sol950_wav budget`, captured Radtel `CQ`, 100 trials/pt, `0` false frames across all budgets):**
        - `-15 dB`: `K=1: 87`, `K=2: 91`, `K=4: 92`, `K=8: 92` (winning rank `[87, 4, 1, 0, 0, 0, 0, 0]`, `5.9–46.5 ms/slot` host).
        - `-16 dB`: `K=1: 72`, `K=2: 77`, `K=4: 81`, `K=8: 82` (winning rank `[72, 5, 4, 0, 1, 0, 0, 0]`).
        - `-17 dB`: `K=1: 33`, `K=2: 42`, `K=4: 47`, `K=8: 50` (winning rank `[33, 9, 2, 3, 3, 0, 0, 0]`).
        - `-18 dB`: `K=1: 7`, `K=2: 12`, `K=4: 13`, `K=8: 13` (winning rank `[7, 5, 0, 1, 0, 0, 0, 0]`).
        - Refining the top `K = 2..4` blind tracks recovers `94–99%` of the `K = 8` blind ceiling with `0` false frames.
      - **Two independently impaired receivers (`leader` / `either` / `joint_miss` -> `rescued_full` / `rescued_4bit` -> `total_4bit`; false-accept controls across `100` pairs each: noise `cur(full/4b)=0/0`, `ref(full/4b)=0/0`, wrong-caller 4-bit fabricated third `0/100`, misattributed contributor `2/100` — confirming cooperative replies must bind to the decoded frame's own sender rather than track A's prior attribution):**
        - `cur` (Synthetic `RX`): `-16 dB`: `44 / 62 / 38 -> 38/38 / 38/38 -> 100/100`; `-17 dB`: `8 / 10 / 90 -> 68/90 / 69/90 -> 79/100`; `-18 dB`: `0 / 0 / 100 -> 24/100 / 22/100 -> 22/100`.
        - `cur` (Captured Radtel `CQ`): `-16 dB`: `27 / 53 / 47 -> 47/47 / 47/47 -> 100/100`; `-17 dB`: `4 / 9 / 91 -> 72/91 / 69/91 -> 78/100`; `-18 dB`: `0 / 0 / 100 -> 15/100 / 13/100 -> 13/100`.
        - `ref` (Synthetic `RX`): `-18 dB`: `41 / 74 / 26 -> 26/26 / 26/26 -> 100/100`; `-19 dB`: `9 / 18 / 82 -> 72/82 / 68/82 -> 86/100`.
        - `ref` (Captured Radtel `CQ`): `-18 dB`: `64 / 90 / 10 -> 10/10 / 10/10 -> 100/100`; `-19 dB`: `7 / 17 / 83 -> 80/83 / 80/83 -> 97/100`.

### Live over-the-air two-way verification (`NQ6N` RT-950 Pro ↔ `NQ6NA` HackRF One, 2026-11-19)

- **Setup:** Radtel RT-950 Pro running Ferrite `sol950` (`tx-bench`, layout 3 + SPI parts installed, `dry=0`, TX power `Low`, manual grid `FN31`) placed near a HackRF One (r9, fw `2026.01.3`) driven directly by `digimodes/usol950/examples/rad950_hackrf.rs` (`2.4 MS/s` ↔ `24 kHz` USB DDC).
- **Frequency calibration:** Carrier probe on 12m (`24.919 MHz`) measured `24,921,300 Hz` (`+2,300 Hz` dial offset) arriving at `1,210 Hz` (`218.3×` SNR ratio), establishing **`+43.7 ppm`** (`--ppm 43.7` on `rad950_hackrf`) between this HackRF One and RT-950 Pro across 12m and 10m.
- **HackRF One (`NQ6NA`) → RT-950 Pro (`NQ6N`) live OTA RX:**
  - **12m (`24.919 MHz`, `--ppm 43.7`, `subch 2 = 1050 Hz`):** Decoded **`NQ6NA FN31 NEW +12 dB`** live onto the RT-950 Pro's `RAD950` call list.
  - **10m (`28.078 MHz`, `--ppm 43.7`, `subch 2 = 1050 Hz`):** Decoded **`NQ6NA FN31 NEW +7 dB`** (and `-6 dB` on repeat), plus `R+RX` (`NQ6N de NQ6NA R+RX -24 dB`) at `-8 dB` on the RT-950 Pro.
- **RT-950 Pro (`NQ6N`) → HackRF One (`NQ6NA`) live OTA TX:**
  - Pressed `OK` (Answer) on `NQ6NA` on the RT-950 Pro (`RAD950 QSO`, `28.078 MHz`, `Low` power, `dry=0`).
  - HackRF One (`rad950_hackrf --band 10m --ppm 43.7 --lna 0 --vga 16 --rx-once`, RF amp off) decoded live over the air: `Rx { sender: NQ6N, recipient: NQ6NA, snr_db: +7, ack: false }` at `938 Hz`, `-24 dB` SNR.
- **Uninterrupted interactive OTA QSO with matching logs at both ends (2026-11-19 22:32Z):**
  - **Root causes fixed during interactive end-to-end testing:**
    1. **Firmware post-TX RX reset (`DspCmd::Sol950Reset`):** Previously, `Sol950Host::update_clock()` suppressed `Sol950Finish` at `+9.55 s` during TX slots (`finish_sent_slot = slot`), leaving `usol950::Receiver` un-reset after TX while muted TX-slot audio advanced `sym` past the 18-symbol acquisition window (`acquired = true`), blinding the radio during the RX slot immediately following any TX. Added `DspCmd::Sol950Reset` (`crates/ferrite-core/src/msg.rs`, `crates/ferrite-core/src/decoders.rs`, `crates/ferrite-app/src/sol950.rs`, `firmware/src/tasks/dsp.rs`, `firmware/src/tasks/ui.rs`) so the DSP receiver resets cleanly at `0.00 s` of every RX slot following a TX slot.
    2. **Interactive HackRF TUI slot turnaround (`digimodes/usol950/examples/rad950_hackrf.rs`):** Added `--slot-offset-ms` for indoor non-GPS slot alignment, lowered `RX_SLOT_MS` (`9_450 -> 9_050`) so the `350 ms` TX pre-trigger fires on time immediately after an RX slot, and bypassed the HackRF RX RF amp for `--lna < 32` so `--lna 8 --vga 24` decodes the nearby handheld cleanly without front-end overload (`-3 dB` SNR vs `-24 dB` at `--lna 0 --vga 16`).
  - **Single uninterrupted interactive session (`rad950_hackrf` TUI `c` ↔ RT-950 Pro `RAD950` `OK`, `28.078 MHz` 10m, `--ppm 43.7 --lna 8 --vga 24 --tx-gain 47 --amp --slot-offset-ms -2690`):**
    1. `22:32:00Z` — HackRF (`NQ6NA`, interactive `c`) transmitted `CQ NQ6NA FN31` (`1050 Hz`).
    2. RT-950 Pro (`NQ6N`) decoded `NQ6NA FN31 NEW 9dB` (`dec=1`); operator pressed `OK` (Answer).
    3. `22:32:40Z` — RT-950 Pro (`NQ6N`, Low power, `dry=0`) transmitted `RX NQ6NA de NQ6N +7dB`; HackRF decoded it live (`rx=-3dB tx=+7dB`), logged `NQ6N`, and automatically queued `R+RX -3dB` in the same session.
    4. `22:32:50Z` — HackRF (`NQ6NA`) transmitted `NQ6N de NQ6NA R+RX -3dB` (`1050 Hz`); RT-950 Pro (`NQ6N`) decoded it live (`dec=2`, `sym=119`, `-9 dB` RX SNR, `-3 dB` reported TX SNR), updated `Operate` to `WKD*`, displayed `Saved to flash log` / `Saved - NEW GRID!`, and persisted the QSO to SPI flash.
  - **Matching logged entries at both ends:**
    - **HackRF (`/tmp/sol950-lab/ota_qso.log`):**
      `22:32:40Z  28.078 MHz  NQ6N        grid=----  rx=-3dB  tx=+7dB`
    - **RT-950 Pro (`RAD950 Log` / `Log Detail`, SPI flash):**
      `Saved contacts: 1` · `NQ6NA FN31  -9/-3dB` · `My: NQ6N (FN31)` · `SNR RX -9 / TX -3 dB` · `Dial: 28.078 MHz` · `UTC 21:15:59 #1` (bench monotonic UTC; `TX -3 dB` matches HackRF's live `rx=-3dB` report).
- **3-station live OTA multi-signal & opposite-parity verification (`NQ6N` RT-950 Pro + `NQ6NA` HackRF #0 + `NQ6NB` HackRF #1, 2026-11-20 04:56Z):**
  - **Setup:** RT-950 Pro (`76a914c`, `dry=0`, Low power, `/dev/ttyUSB2`) + two HackRF Ones on `ronald` calibrated via ATSC TV pilots (`#0 ...2a9f68cd`: `-0.28 ppm`, `#1 ...3648550f`: `-0.20 ppm`), both running interactive `rad950_hackrf` with Auto-CQ ON (`a`) on `28.078 MHz` (`--ppm 43.7 --lna 8 --vga 24 --tx-gain 47 --amp --slot-offset-ms -3528`): `NQ6NA` on subch 1 (`825 Hz`, `EN61`) and `NQ6NB` on subch 4 (`1500 Hz`, `EM79`).
  - **Simultaneous dual-signal on-MCU decode:** RT-950 Pro decoded both simultaneous HackRF CQs in the same 10 s slot (`dec=8 -> 10`): `NQ6NA EN61 WKD -5dB` and `NQ6NB EM79 NEW -10dB`.
  - **Opposite-parity Auto-CQ QSO under active adjacent-subchannel traffic (`76a914cb`):**
    1. `04:56:10Z` — Both `NQ6NA` (`825 Hz`) and `NQ6NB` (`1500 Hz`) transmitted Auto-CQ on the odd slot.
    2. `04:56:20Z` — RT-950 Pro (`NQ6N`) automatically waited for the opposite-parity even slot and transmitted `NQ6NB de NQ6N RX -10dB`. Both HackRFs decoded it simultaneously: `NQ6NA` logged `RX +0dB 938Hz NQ6NB de NQ6N RX -10dB` and ignored it (not addressed to `NQ6NA`); `NQ6NB` logged `QSO logged NQ6N (rx=-8dB tx=-10dB) -> R+RX queued`.
    3. `04:56:30Z` — `NQ6NA` transmitted `CQ NQ6NA EN61` (`825 Hz`) simultaneously with `NQ6NB` transmitting `NQ6N de NQ6NB R+RX -8dB` (`1500 Hz`). RT-950 Pro decoded both in the same slot (`NQ6NA -3dB` + `NQ6NB R+RX -8dB`), updated `NQ6NB EM79` from `NEW` to `WKD*`, and saved the contact to SPI flash.
  - **Matching logged entries:**
    - **`NQ6NB` (`/tmp/sol950-lab/qso_nq6nb.log`):** `04:56:20Z  28.078 MHz  NQ6N        grid=----  rx=-8dB  tx=-10dB`
    - **`NQ6N` (RT-950 Pro `RAD950 Log`, SPI flash):** `Saved contacts: 2` · `NQ6NB EM79  -8/-8dB` · `NQ6NA FN31  -9/-3dB`
- **3-radio live OTA Rendezvous band-hopping test (`NQ6N`, `NQ6N-1`, `NQ6N-2`, 2026-11-20 05:35Z):**
  - **Callsign suffix & Rendezvous support (`ec83c543`):** Added 4-bit `-1..-15` numeric suffix encoding in `Frame::Cq.reserved & 0x0F` (so multiple stations under one callsign identify as `NQ6N`, `NQ6N-1`, `NQ6N-2` without dropping each other as self-echo) and hourly UTC Rendezvous schedule following (`--rendezvous` / `[v]`, `:00` 10m / `:15` 12m / `:30` 15m / `:45` 17m, 50/50 even-slot turn draw) in `rad950_hackrf.rs`.
  - **Synchronized band hop (`10m 28.078 MHz -> 12m 24.919 MHz`):** Armed `Follow schedule = ON` and `Call & listen = ARMED` on RT-950 Pro (`NQ6N`, `ec83c54`, `dry=0`) and launched `NQ6N-1` (HackRF #0) + `NQ6N-2` (HackRF #1) in `--rendezvous` mode synced to the radio's clock (`--slot-offset-ms -1194133`). At `:15:00Z`, all three radios hopped automatically from `28.078 MHz` to `24.919 MHz` (RT-950 Pro `Operate` switched `28.078 RX BENCH CLK` → `24.919 RX BENCH CLK`).
  - **Independent 50/50 turns & decodes on 12m (`24.919 MHz`):**
    - `:15:00Z`: `NQ6N-2` TX `1725 Hz` → `NQ6N-1` decoded `CQ NQ6N-2 EN61` (`+14 dB`, `1723 Hz`).
    - `:15:20Z`: `NQ6N-1` TX `600 Hz` → `NQ6N-2` decoded `CQ NQ6N-1 EN61` (`+14 dB`, `598 Hz`).
    - `:15:40Z`: `NQ6N-2` TX `1275 Hz` → `NQ6N-1` decoded `CQ NQ6N-2 EN61` (`+2 dB`, `1277 Hz`).
    - `:16:00Z`: `NQ6N-1` TX `2175 Hz` → `NQ6N-2` decoded `CQ NQ6N-1 EN61` (`+8 dB`, `2168 Hz`).
    - RT-950 Pro (`NQ6N`) decoded **`NQ6N-1 EN61 NEW -15dB`** live on `24.919 MHz` (confirming `-1` suffix display and 12m RX after automatic band hop).
    - Neither HackRF decoded a 12m TX from `NQ6N` during the 85 s window (to be checked in the multi-band RT-950 Pro TX verification below).
- **On-MCU stability:** Resolved stack headroom (`ui::task` `12,096 B -> 6,560 B`, `dsp::feed` `5,536 B -> <1,600 B`, `Decoder::switch` `4,800 B -> <100 B`), Low Power Mode 24 MHz DSP starvation, World Map `lcd.rows()` band clipping, and PTT exit behavior; verified 231+ consecutive live 10-second RX slots with zero crashes.

## Conducted RAD950 transmitter measurements with the Siglent SSA3021X

Use an ordinary RAD950 CQ so the capture includes the behavior of the operating firmware from key-up through unkey. Start on 10 m at Low power and use the same CQ payload and subchannel for repeats. The earlier 10 m HackRF capture supplies a baseline for tone spacing, symbol timing, and occupied bandwidth. This measurement pass adds calibrated burst power, a wider spur survey, and comparable traces across the rendezvous bands. [Earlier TX measurements](#measured-bench-results-nq6n-fn31-2026-11-18) · [SSA3000X user manual](https://siglentna.com/wp-content/uploads/dlm_uploads/2017/10/SSA3000X_User-Manual_UM0703X_E02B.pdf)

### Connect and calibrate the sample path

```text
RT-950 Pro RF out ──► IN [directional coupler] THRU ──► rated 50 Ω dummy load
                              FWD sample ──► fixed attenuator ──► SSA3021X RF INPUT
```

- Use 50 Ω RF components. Verify the coupler orientation, the dummy load's rating for the intended burst power and repetition rate, and the termination required at the unused coupler port.
- Calculate the maximum at the analyzer from the highest credible radio output minus the minimum coupling loss and minimum external pad loss. With **10 W (+40 dBm)**, a nominal **20 dB coupler** and **30 dB external pad** give **−10 dBm nominal** at the analyzer. The first pad sees **+20 dBm (100 mW) nominal** in that example. Rate it with margin for the actual coupling tolerance. Recalculate for the coupler and pads on the bench.
- Start with **preamp off**, **tracking generator off**, **20 dB manual input attenuation**, and **0 dBm reference level**. Keep the external pad in the sample line. Capture a radio-unkeyed baseline before measuring the first burst.
- Measure or record the loss from the coupler input to the analyzer at the fundamental and the frequencies where harmonics or spurs will be measured. Use that frequency-specific loss to refer readings back to the radio jack. Where the path is uncharacterized, record the analyzer reading and mark the correction as unknown. [Coupler and load setup](#wire-the-load-and-forward-sample)

### Measurement sequence

| Measurement | Analyzer starting settings | What to save |
| --- | --- | --- |
| **Burst power and envelope** | Center on the actual RF burst, midway between the lowest and highest tones. Use zero span, **10 kHz RBW**, about **12 s** sweep time, and a video trigger if convenient. | Trace from key-up through unkey. Record flat-top power during the burst, corrected to the radio jack, in dBm and watts. Record lead-in, burst duration, any droop, and unkey behavior. Compare the symbol portion with the nominal **8.8 s** frame. Repeat at each supported power setting. |
| **Harmonics and other spurs** | Start with a **10–200 MHz** overview in max hold across repeated identical bursts. Zoom in on the second through fifth harmonics and any other suspect peaks. Adjust RBW and sweep time to resolve the peak from the noise floor. | For each suspect emission, record RF frequency, RBW, measured input level, loss correction, and level at the radio jack. Report dBc relative to the corrected fundamental burst power, with the reference identified. Repeat with **10 dB more attenuation** and correct for the added loss. A genuine transmitter spur should stay near its prior corrected level. Use the radio-unkeyed baseline to identify signals already present. |
| **Close-in spectrum** | Center on the actual RF burst. Start with **5 kHz span** and **10–30 Hz RBW**. Repeat the same CQ for max-hold screening. | Tone centers and spacing, spectral skirts, and excursions toward the **225 Hz** neighboring subchannels. Save span, RBW, detector, and sweep time with each trace. For quantitative adjacent-subchannel power, **99% occupied bandwidth**, or sub-millisecond PLL relock, analyze complete HackRF IQ bursts. The swept max-hold trace serves as a peak survey. |
| **Frequency offset and warm-up drift** | Use a steady test tone at a known commanded RF frequency. Compare a cold measurement with one after several minutes of typical operation. Use a known **10 MHz reference** at the analyzer if available. | Commanded and measured RF in Hz, offset, elapsed time, band, and reference used. Treat an internal-reference reading as relative to the analyzer. At **28 MHz**, a 1 ppm reference error corresponds to **28 Hz**. |

Once 10 m produces a repeatable baseline, repeat the envelope, harmonic survey, and close-in capture on **12 m, 15 m, and 17 m** wherever the firmware has a supported, calibrated TX path. Start at Low power on each band. Record a refusal or unavailable calibration directly in the test log. The current live 12 m result establishes handheld RX, while the conducted measurements will establish handheld TX behavior there. [Live OTA results](#live-over-the-air-two-way-verification-nq6n-rt-950-pro--nq6na-hackrf-one-2026-11-19)

For each run, log **firmware revision, UTC, band, USB dial, subchannel, power setting, battery or supply voltage, coupler and pad IDs, loss calibration, analyzer settings, and trace filenames**. Save a CSV trace where available and a screenshot of the instrument settings. Use the same signal and settings when comparing firmware, bands, or power levels.
