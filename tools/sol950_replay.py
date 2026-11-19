#!/usr/bin/env python3
"""Reproducible RF-to-WAV processing, shifted burst preparation, and HackRF-to-SI4732 RX sweep tool."""

import argparse
import hashlib
import subprocess
import sys
import threading
import time
import wave
from pathlib import Path

import numpy as np

SR24 = 24_000
FS8M = 8_000_000
RADTEL_CS8_SHA256 = "d8e8cadb7098a19cd3c3f3d6ddd6c70236b72ff2ba5048a0a467a560d6299c86"
RADTEL_WAV_SHA256 = "d2b60669f9c7cde7541ad2322bf2e9d24ebc7ec2e531281da7890988d99a1732"

# Exact `RX K1ABC -> NQ6N (snr=-15, ack=0)` 110-tone reference sequence from `usol950::encode_tones`
RX_K1ABC_TONES = [
    0, 3, 0, 1, 3, 2, 3, 3, 0, 3, 2, 0, 3, 1, 2, 1, 3, 0, 3, 0, 3, 2, 0, 1, 2, 2, 0, 1,
    3, 1, 3, 1, 2, 1, 2, 1, 0, 1, 0, 3, 0, 0, 2, 3, 0, 1, 3, 3, 2, 0, 1, 0, 1, 0, 0, 0,
    2, 1, 3, 2, 2, 1, 0, 0, 2, 1, 3, 3, 3, 1, 3, 1, 0, 3, 2, 0, 2, 0, 0, 3, 2, 0, 3, 3,
    2, 0, 1, 3, 0, 1, 3, 0, 2, 1, 0, 3, 0, 2, 1, 2, 3, 2, 0, 2, 3, 1, 0, 2, 3, 0,
]


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        while chunk := f.read(1 << 20):
            h.update(chunk)
    return h.hexdigest()


def read_wav(path: Path) -> tuple[int, np.ndarray]:
    with wave.open(str(path), "rb") as wf:
        assert wf.getnchannels() == 1 and wf.getsampwidth() == 2
        sr = wf.getframerate()
        x = np.frombuffer(wf.readframes(wf.getnframes()), dtype="<i2").astype(np.float64)
    return sr, x


def write_wav(path: Path, sr: int, x: np.ndarray) -> None:
    pcm = np.clip(np.round(x), -32767, 32767).astype("<i2")
    with wave.open(str(path), "wb") as wf:
        wf.setnchannels(1)
        wf.setsampwidth(2)
        wf.setframerate(sr)
        wf.writeframes(pcm.tobytes())


def downconvert_cs8_to_24k(cs8_path: Path, wav_path: Path) -> None:
    """Mix 8 MS/s HackRF I/Q down by -200 kHz, LPF to 3 kHz, decimate to 24 kHz mono WAV."""
    raw = np.fromfile(cs8_path, dtype=np.int8)
    dc = raw[0::200].mean() + 1j * raw[1::200].mean()
    chunk = 4_000_000
    t_c = np.arange(chunk, dtype=np.float64) / FS8M
    lo = np.exp(-2j * np.pi * 200_000.0 * t_c)
    parts80k = []
    n_samp = len(raw) // 2
    for off in range(0, n_samp - chunk + 1, chunk):
        s = raw[2 * off : 2 * (off + chunk)]
        c = (s[0::2].astype(np.float64) + 1j * s[1::2].astype(np.float64)) - dc
        parts80k.append((c * lo).reshape(-1, 100).mean(axis=1))
    bb80k = np.concatenate(parts80k)

    up = np.zeros(len(bb80k) * 3, dtype=np.complex128)
    up[::3] = bb80k * 3.0
    fc = 4000.0 / 240_000.0
    ntaps = 241
    n = np.arange(ntaps) - (ntaps - 1) / 2.0
    h = 2.0 * fc * np.sinc(2.0 * fc * n) * np.blackman(ntaps)
    h /= h.sum()
    bb24k = np.convolve(up, h, mode="same")[::10]
    audio = bb24k.real
    scale = 16000.0 / max(float(np.max(np.abs(audio))), 1e-9)
    write_wav(wav_path, SR24, audio * scale)


def synth_4fsk_slot(tones: list[int], f0_hz: float, amp: float) -> np.ndarray:
    """Reference continuous-phase 10 ms RC + 70 ms hold 4-FSK modulator (`usol950::phy` @ 24 kHz)."""
    sub_samp = 30  # 1.25 ms @ 24 kHz (64 substeps per 80 ms symbol = 1920 samples)
    out = np.empty(len(tones) * 64 * sub_samp, dtype=np.float64)
    phase = 0.0
    pos = 0
    for sym, t_cur in enumerate(tones):
        t_prev = tones[0] if sym == 0 else tones[sym - 1]
        for sub in range(64):
            if sub < 8:
                u = 0.5 * (1.0 - np.cos(np.pi * (sub + 0.5) / 8.0))
                tone_pos = t_prev + (t_cur - t_prev) * u
            else:
                tone_pos = float(t_cur)
            f_inst = f0_hz + tone_pos * 50.0
            dphi = 2.0 * np.pi * f_inst / SR24
            ph = phase + dphi * (np.arange(sub_samp) + 1)
            out[pos : pos + sub_samp] = amp * np.sin(ph)
            phase = float(ph[-1] % (2.0 * np.pi))
            pos += sub_samp
    return out


def upconvert_24k_to_plus200k_cs8(audio: np.ndarray, cs8_path: Path, iq_gain: float) -> float:
    """Form analytic USB and interpolate 24 kHz real audio (+200 kHz offset) to 8 MS/s signed int8 I/Q."""
    assert len(audio) % 2400 == 0
    guard = 96
    padded = np.pad(audio * iq_gain, guard)
    taper = np.ones(2400 + 2 * guard)
    edge = 0.5 * (1.0 - np.cos(np.pi * np.arange(32) / 32.0))
    taper[:32] = edge
    taper[-32:] = edge[::-1]
    lo = np.exp(2j * np.pi * np.arange(800_000) / 40.0)
    peak_i8 = 0
    with cs8_path.open("wb") as out:
        for start in range(0, len(audio), 2400):
            part = padded[start : start + 2400 + 2 * guard] * taper
            Xp = np.fft.rfft(part)
            Z = np.zeros(864_000, dtype=np.complex128)
            Z[0] = Xp[0]
            Z[1:1296] = 2.0 * Xp[1:1296]
            Z[1296] = Xp[1296]
            iq = (np.fft.ifft(Z) * (864_000.0 / 2592.0))[32_000:832_000] * lo
            pairs = np.rint(np.column_stack((iq.real, iq.imag))).clip(-127, 127).astype(np.int8)
            peak_i8 = max(peak_i8, int(np.max(np.abs(pairs))))
            pairs.tofile(out)
    return peak_i8


def cmd_prep(args: argparse.Namespace) -> None:
    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    wav_path = out_dir / "radtel-tx.wav"
    if args.input.endswith(".cs8"):
        cs8_path = Path(args.input)
        print(f"sha256({cs8_path.name}) = {sha256_file(cs8_path)}")
        downconvert_cs8_to_24k(cs8_path, wav_path)
    else:
        wav_path = Path(args.input)
    print(f"sha256({wav_path.name}) = {sha256_file(wav_path)}")

    sr, x = read_wav(wav_path)
    assert sr == SR24
    p_off = float(np.mean(x[:SR24] ** 2))
    p_on = float(np.mean(x[int(1.45 * SR24) : int(10.18 * SR24)] ** 2))
    sig_rms = float(np.sqrt(max(p_on - p_off, 1.0)))
    print(f"off_rms={np.sqrt(p_off):.1f}, on_rms={np.sqrt(p_on):.1f}, sig_rms={sig_rms:.1f} (cap SNR={10*np.log10((p_on-p_off)/p_off):.1f} dB)")

    i0, i1 = int(round(1.2154 * SR24)), int(round(10.2154 * SR24))
    burst = x[i0:i1].copy()
    fade = int(0.005 * SR24)
    burst[:fade] *= np.hanning(2 * fade)[:fade]
    burst[-fade:] *= np.hanning(2 * fade)[fade:]

    N = len(burst)
    X = np.fft.fft(burst)
    H = np.zeros(N)
    H[0] = 1.0
    if N % 2 == 0:
        H[1 : N // 2] = 2.0
        H[N // 2] = 1.0
    else:
        H[1 : (N + 1) // 2] = 2.0
    analytic = np.fft.ifft(X * H)
    t = np.arange(N, dtype=np.float64) / SR24
    scale = 10_000.0 / sig_rms

    p0 = out_dir / "radtel_burst_0.wav"
    write_wav(p0, SR24, burst * scale)
    print(f"{p0.name}: sha256={sha256_file(p0)}")
    for sh in [int(v) for v in args.shifts.split(",")]:
        shifted = (analytic * np.exp(2j * np.pi * sh * t)).real
        ps = out_dir / f"radtel_burst_{sh:+d}.wav"
        write_wav(ps, SR24, shifted * scale)
        print(f"{ps.name}: sha256={sha256_file(ps)}")


def cmd_make_rx_suite(args: argparse.Namespace) -> None:
    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    _, b0 = read_wav(Path(args.burst0))
    _, b225 = read_wav(Path(args.burst225))
    snrs = [float(s) for s in args.snrs.split(",")]
    trials = args.trials
    sigma_24k = 1.0
    p_n_2500 = sigma_24k**2 * (2500.0 / 12_000.0)
    # Bandlimit AWGN to [200..2800 Hz] so 2500 Hz PSD is exact while keeping HackRF DAC peaks well inside [-127, 127].
    noise_rms_2600 = float(np.sqrt(2600.0 / 12_000.0)) * sigma_24k
    iq_gain = 18.0 / noise_rms_2600
    wav_gain = 1_200.0  # puts AWGN RMS at ~558 i16 on host WAVs (unclipped even with +15 dB adj)

    for adj_on in [False, True]:
        tag = "adj225" if adj_on else "alone"
        for si, snr_db in enumerate(snrs):
            name = f"rx_{tag}_m{int(round(abs(snr_db)))}"
            p_weak = p_n_2500 * (10.0 ** (snr_db / 10.0))
            weak_rms = float(np.sqrt(p_weak))
            adj_rms = weak_rms * (10.0 ** (15.0 / 20.0))
            slot_len = SR24 * 10
            # Pad 0.5 s before and 0.5 s after the N*10 s sequence (total multiple of 2400 = 0.1 s)
            pad = SR24 // 2
            full = np.zeros(pad + trials * slot_len + pad, dtype=np.float64)

            rng = np.random.default_rng(0x5019_5000 + (100 if adj_on else 0) + si)
            raw_noise = rng.normal(0.0, sigma_24k, len(full))
            Xn = np.fft.rfft(raw_noise)
            fn = np.fft.rfftfreq(len(raw_noise), 1.0 / SR24)
            Xn[(fn < 200.0) | (fn > 2800.0)] = 0.0
            full += np.fft.irfft(Xn, n=len(full))

            for ti in range(trials):
                off0 = pad + ti * slot_len
                dt_w_ms = -160 + ((ti * 53) % 401)
                if not adj_on:
                    # Alone condition: captured Radtel CQ NQ6N FN31 burst
                    st = off0 + ((200 + dt_w_ms) * SR24) // 1000
                    full[st : st + len(b0)] += (b0 / 10_000.0) * weak_rms
                else:
                    # Adjacent condition: distinct weak RX K1ABC -> NQ6N + +15 dB captured Radtel CQ NQ6N FN31 at +225 Hz
                    st_w = off0 + ((400 + dt_w_ms) * SR24) // 1000
                    w_sig = synth_4fsk_slot(RX_K1ABC_TONES, 1037.0, np.sqrt(2.0) * weak_rms)
                    full[st_w : st_w + len(w_sig)] += w_sig
                    dt_a_ms = -160 + (((ti * 97) + 211) % 401)
                    st_a = off0 + ((200 + dt_a_ms) * SR24) // 1000
                    full[st_a : st_a + len(b225)] += (b225 / 10_000.0) * adj_rms

            hwav = out_dir / f"host_{name}.wav"
            write_wav(hwav, SR24, full * wav_gain)
            cs8 = out_dir / f"{name}.cs8"
            pk = upconvert_24k_to_plus200k_cs8(full, cs8, iq_gain)
            print(f"{name:16s}: dur={len(full)/SR24:.1f}s peak_i8={pk}/127 host_wav={hwav.name} cs8={cs8.name}")


def cmd_run_rx_suite(args: argparse.Namespace) -> None:
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    import ferrite_link as fl

    out_dir = Path(args.out_dir)
    port = fl.Port(args.port)
    link = fl.Link(port)

    def term_cmd(line: str) -> str:
        s, r = link.ask(fl.CMD, line.encode("ascii"))
        chunks = []
        while True:
            if s != 0:
                raise RuntimeError(f"term {line!r}: {fl.why(s, r)}")
            chunks.append(r[1:].decode("utf-8", "replace"))
            if r[0] == 0:
                break
            s, r = link.ask(fl.MORE)
        return "".join(chunks).strip()

    try:
        link.hello()
        term_cmd("si prop 0101 9012")
        term_cmd("si 40 80 6e 19 00 00 reply 1")
        term_cmd("si prop 0100 0546")  # +1350 Hz BFO -> 1037..1187 Hz

        time.sleep(0.2)
        link.answers()
        link.raw.clear()
        fl.raw_audio(link, 5.0, str(out_dir / "pa2_rf_off.wav"), 16000)

        for cs8 in sorted(out_dir.glob("rx_*.cs8")):
            name = cs8.stem
            # Infer duration from cs8 byte count (2 bytes/complex sample at 8 MS/s)
            tx_secs = cs8.stat().st_size / (2.0 * FS8M)
            rx_secs = tx_secs + 1.5
            holder = {}

            def tx() -> None:
                time.sleep(0.6)
                holder["p"] = subprocess.Popen(
                    ["hackrf_transfer", "-t", str(cs8), "-f", "27985000", "-s", "8000000", "-a", str(args.amp), "-p", "0", "-x", str(args.x)],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                )

            th = threading.Thread(target=tx)
            th.start()
            pa2_wav = out_dir / f"pa2_{name}.wav"
            try:
                time.sleep(0.15)
                link.answers()
                link.raw.clear()
                fl.raw_audio(link, rx_secs, str(pa2_wav), 16000)
            finally:
                th.join()
                p = holder.get("p")
                if p is not None:
                    _, err = p.communicate(timeout=20)
                    if p.returncode not in (0, None):
                        print(f"{name}: hackrf rc={p.returncode} {err.decode('utf-8', 'replace').strip()}", flush=True)
            print(f"captured {pa2_wav}", flush=True)
        term_cmd("si prop 0100 0000")
    finally:
        port.close()


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest="cmd", required=True)

    p_prep = sub.add_parser("prep")
    p_prep.add_argument("input")
    p_prep.add_argument("out_dir")
    p_prep.add_argument("--shifts", default="+200,+225,-225,+250,+275")

    p_mk = sub.add_parser("make-rx-suite")
    p_mk.add_argument("burst0")
    p_mk.add_argument("burst225")
    p_mk.add_argument("out_dir")
    p_mk.add_argument("--trials", type=int, default=8)
    p_mk.add_argument("--snrs", default="-14,-15,-16")

    p_run = sub.add_parser("run-rx-suite")
    p_run.add_argument("out_dir")
    p_run.add_argument("--port", required=True)
    p_run.add_argument("--x", type=int, default=42)
    p_run.add_argument("--amp", type=int, default=1)

    args = ap.parse_args()
    if args.cmd == "prep":
        cmd_prep(args)
    elif args.cmd == "make-rx-suite":
        cmd_make_rx_suite(args)
    elif args.cmd == "run-rx-suite":
        cmd_run_rx_suite(args)


if __name__ == "__main__":
    main()
