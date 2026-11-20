#!/usr/bin/env python3
"""
TCI-to-HackRF Bridge for WSJT-X (RAD950 / SOL950 / FT8 / FT4).
Speaks WSJT-X TCI v1.8 (WebSocket text CAT + 48 kHz stereo float32 binary audio)
and bridges half-duplex USB audio <-> 2.4 MSPS IQ on a HackRF One via hackrf_transfer.
"""

import argparse
import asyncio
import base64
import hashlib
import math
import signal
import struct
import subprocess
import sys
import threading
import time
from collections import deque
import queue

import numpy as np

WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

FS_RF = 2_400_000          # 2.4 MSPS (exact 50x multiple of 48 kHz)
FS_AUDIO = 48_000          # WSJT-X TCI audio rate
DECIM = FS_RF // FS_AUDIO  # 50
IF_OFFSET_HZ = 240_000     # Tune HackRF LO = dial_hz - 240 kHz (avoids DC spike)
CHUNK_FRAMES = 1024        # 1024 stereo frames = 21.333 ms @ 48 kHz
CHUNK_FLOATS = CHUNK_FRAMES * 2  # 2048 floats (interleaved L, R)
CHUNK_IQ = CHUNK_FRAMES * DECIM  # 51,200 complex samples = 102,400 int8 bytes

# TCI Data_Stream types
STREAM_IQ = 0
STREAM_RX_AUDIO = 1
STREAM_TX_AUDIO = 2
STREAM_TX_CHRONO = 3


def design_lpf(num_taps: int, cutoff_hz: float, fs: float) -> np.ndarray:
    n = np.arange(num_taps) - (num_taps - 1) / 2.0
    fc = cutoff_hz / fs
    h = np.sinc(2.0 * fc * n) * np.hamming(num_taps)
    h /= np.sum(h)
    return h.astype(np.float32)


def pack_tci_header(receiver: int, sample_rate: int, fmt: int, length: int, stream_type: int) -> bytes:
    return struct.pack(
        "<16I",
        receiver,
        sample_rate,
        fmt,
        0,  # codec
        0,  # crc
        length,
        stream_type,
        0, 0, 0, 0, 0, 0, 0, 0, 0,
    )


def ws_encode_frame(opcode: int, payload: bytes) -> bytes:
    header = bytearray()
    header.append(0x80 | (opcode & 0x0F))
    n = len(payload)
    if n < 126:
        header.append(n)
    elif n < 65536:
        header.append(126)
        header.extend(struct.pack("!H", n))
    else:
        header.append(127)
        header.extend(struct.pack("!Q", n))
    return bytes(header) + payload


async def ws_read_frame(reader: asyncio.StreamReader):
    hdr = await reader.readexactly(2)
    b0, b1 = hdr[0], hdr[1]
    opcode = b0 & 0x0F
    masked = (b1 & 0x80) != 0
    length = b1 & 0x7F
    if length == 126:
        length = struct.unpack("!H", await reader.readexactly(2))[0]
    elif length == 127:
        length = struct.unpack("!Q", await reader.readexactly(8))[0]
    mask = await reader.readexactly(4) if masked else b""
    payload = await reader.readexactly(length)
    if masked and length > 0:
        arr = np.frombuffer(payload, dtype=np.uint8).copy()
        m = np.frombuffer(mask, dtype=np.uint8)
        arr ^= m[np.arange(length) & 3]
        payload = arr.tobytes()
    return opcode, payload


class HackRFWorker:
    """Manages half-duplex hackrf_transfer RX / TX subprocesses and 2.4 MSPS <-> 48 kHz USB DSP."""

    def __init__(self, serial: str, dial_hz: int, lna: int, vga: int, tx_gain: int, rx_scale: float, dry_tx: bool):
        self.serial = serial
        self.dial_hz = dial_hz
        self.lna = lna
        self.vga = vga
        self.tx_gain = tx_gain
        self.rx_scale = rx_scale
        self.dry_tx = dry_tx

        self.lock = threading.Lock()
        self.rx_proc = None
        self.tx_proc = None
        self.tx_queue = queue.Queue(maxsize=64)
        self.tx_thread = None
        self.rx_thread = None
        self.running = True
        self.transmitting = False
        self.audio_streaming = False
        self.on_rx_chunk = None  # callback(bytes_tci_frame)
        self.last_rssi_dbm = -95.0

        # Precompute NCOs & LPF
        self.lpf = design_lpf(63, 1450.0, FS_AUDIO)
        self.rx_lpf_state = np.zeros(len(self.lpf) - 1, dtype=np.complex64)
        self.tx_lpf_state = np.zeros(len(self.lpf) - 1, dtype=np.complex64)

        # RX NCO at 2.4 MSPS: shift down by (IF_OFFSET_HZ + 1500 Hz) = 241,500 Hz
        # Exact phase continuity across CHUNK_IQ samples:
        self.rx_nco_phase = 0.0
        self.rx_up_phase = 0.0
        self.tx_dn_phase = 0.0
        self.tx_nco_phase = 0.0
        self.tx_prev_sample = np.complex64(0.0)

        self.t_rf = np.arange(CHUNK_IQ, dtype=np.float64)
        self.t_aud = np.arange(CHUNK_FRAMES, dtype=np.float64)
        self.up_x = np.arange(CHUNK_IQ, dtype=np.float32) / float(DECIM)
        self.up_xp = np.arange(CHUNK_FRAMES + 1, dtype=np.float32)

    def _stop_rx_locked(self):
        proc = self.rx_proc
        self.rx_proc = None
        if proc is not None:
            try:
                proc.terminate()
                proc.wait(timeout=1.0)
            except Exception:
                try:
                    proc.kill()
                except Exception:
                    pass

    def _stop_tx_locked(self):
        proc = self.tx_proc
        self.tx_proc = None
        if proc is not None:
            try:
                if proc.stdin:
                    proc.stdin.close()
                proc.wait(timeout=1.5)
            except Exception:
                try:
                    proc.terminate()
                    proc.wait(timeout=0.5)
                except Exception:
                    try:
                        proc.kill()
                    except Exception:
                        pass

    def _start_rx_locked(self):
        if self.transmitting or not self.running:
            return
        self._stop_rx_locked()
        lo_hz = max(1_000_000, self.dial_hz - IF_OFFSET_HZ)
        cmd = [
            "hackrf_transfer",
            "-d", self.serial,
            "-r", "-",
            "-f", str(lo_hz),
            "-s", str(FS_RF),
            "-l", str(self.lna),
            "-g", str(self.vga),
            "-a", "0",
        ]
        print(f"[HACKRF] RX start: dial={self.dial_hz} Hz (LO={lo_hz} Hz)", flush=True)
        self.rx_proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, bufsize=CHUNK_IQ * 4)
        self.rx_lpf_state[:] = 0
        self.rx_thread = threading.Thread(target=self._rx_loop, args=(self.rx_proc,), daemon=True)
        self.rx_thread.start()

    def start(self):
        with self.lock:
            self._start_rx_locked()

    def stop(self):
        with self.lock:
            self.running = False
            self._stop_tx_locked()
            self._stop_rx_locked()

    def set_dial_hz(self, dial_hz: int):
        with self.lock:
            if dial_hz == self.dial_hz:
                return
            self.dial_hz = dial_hz
            if not self.transmitting:
                self._start_rx_locked()

    def set_ptt(self, on: bool):
        with self.lock:
            if on == self.transmitting:
                return
            self.transmitting = on
            if on:
                self._stop_rx_locked()
                self.tx_lpf_state[:] = 0
                self.tx_prev_sample = np.complex64(0.0)
                if not self.dry_tx:
                    lo_hz = max(1_000_000, self.dial_hz - IF_OFFSET_HZ)
                    print(f"[HACKRF] TX start: dial={self.dial_hz} Hz (LO={lo_hz} Hz, tx_gain={self.tx_gain})", flush=True)
                    self.tx_thread = threading.Thread(target=self._tx_writer_loop, args=(lo_hz,), daemon=True)
                    self.tx_thread.start()
                else:
                    print(f"[HACKRF] DRY-TX start: dial={self.dial_hz} Hz", flush=True)
            else:
                self._stop_tx_locked()
                print(f"[HACKRF] TX stop -> resuming RX", flush=True)
                self._start_rx_locked()

    def feed_tx_audio(self, stereo_floats: np.ndarray):
        """Convert 48 kHz stereo float32 audio from WSJT-X into 2.4 MSPS int8 IQ USB and write to HackRF."""
        if len(stereo_floats) < 2:
            return
        mono = stereo_floats[0::2].astype(np.float32)
        n = len(mono)
        if n == 0:
            return

        # 1) Shift audio down by 1500 Hz -> complex baseband
        t_aud = np.arange(n, dtype=np.float64)
        ph_dn = self.tx_dn_phase - 2.0 * math.pi * 1500.0 * t_aud / FS_AUDIO
        self.tx_dn_phase = (self.tx_dn_phase - 2.0 * math.pi * 1500.0 * n / FS_AUDIO) % (2.0 * math.pi)
        z_dn = mono * np.exp(1j * ph_dn).astype(np.complex64)

        # 2) Low-pass filter ±1450 Hz (selects USB 50..2950 Hz, rejects LSB image)
        buf = np.concatenate((self.tx_lpf_state, z_dn))
        z_filt = np.convolve(buf, self.lpf, mode="valid").astype(np.complex64)
        self.tx_lpf_state = buf[-(len(self.lpf) - 1):].copy()

        # 3) Shift back up by +1500 Hz -> analytic USB signal at 48 kHz
        ph_up = -ph_dn  # exact conjugate frequency shift
        z_usb = (2.0 * z_filt * np.exp(1j * ph_up)).astype(np.complex64)

        # 4) Linear upsample x50 to 2.4 MSPS
        n_rf = n * DECIM
        z_ext = np.empty(n + 1, dtype=np.complex64)
        z_ext[0] = self.tx_prev_sample
        z_ext[1:] = z_usb
        self.tx_prev_sample = z_usb[-1]
        if n == CHUNK_FRAMES:
            x_rf = self.up_x
            xp = self.up_xp
        else:
            x_rf = np.arange(n_rf, dtype=np.float32) / float(DECIM)
            xp = np.arange(n + 1, dtype=np.float32)
        i_up = np.interp(x_rf, xp, z_ext.real).astype(np.float32)
        q_up = np.interp(x_rf, xp, z_ext.imag).astype(np.float32)
        z_rf = i_up + 1j * q_up

        # 5) Shift up to +240 kHz IF
        t_rf = self.t_rf if n_rf == CHUNK_IQ else np.arange(n_rf, dtype=np.float64)
        ph_rf = self.tx_nco_phase + 2.0 * math.pi * IF_OFFSET_HZ * t_rf / FS_RF
        self.tx_nco_phase = (self.tx_nco_phase + 2.0 * math.pi * IF_OFFSET_HZ * n_rf / FS_RF) % (2.0 * math.pi)
        z_out = z_rf * np.exp(1j * ph_rf).astype(np.complex64)

        # 6) Scale to int8 (±100 max to keep HackRF DAC linear)
        iq = np.empty(n_rf * 2, dtype=np.int8)
        iq[0::2] = np.clip(np.round(z_out.real * 100.0), -127, 127).astype(np.int8)
        iq[1::2] = np.clip(np.round(z_out.imag * 100.0), -127, 127).astype(np.int8)

        try:
            self.tx_queue.put_nowait(iq.tobytes())
        except queue.Full:
            pass

    def _tx_writer_loop(self, lo_hz: int):
        # Pre-buffer ~5 chunks (~106 ms) so hackrf_transfer never underruns
        pre = []
        t_start = time.monotonic()
        while self.transmitting and len(pre) < 5 and (time.monotonic() - t_start) < 0.5:
            try:
                pre.append(self.tx_queue.get(timeout=0.05))
            except queue.Empty:
                continue
        if not self.transmitting:
            return
        cmd = [
            "hackrf_transfer",
            "-d", self.serial,
            "-t", "-",
            "-f", str(lo_hz),
            "-s", str(FS_RF),
            "-x", str(self.tx_gain),
            "-a", "0",
        ]
        proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, bufsize=CHUNK_IQ * 8)
        with self.lock:
            self.tx_proc = proc
        n_written = 0
        max_iq = 0
        try:
            for b in pre:
                proc.stdin.write(b)
                n_written += 1
                m = int(np.max(np.abs(np.frombuffer(b, dtype=np.int8))))
                if m > max_iq: max_iq = m
            proc.stdin.flush()
            while self.transmitting and self.tx_proc is proc:
                try:
                    b = self.tx_queue.get(timeout=0.1)
                except queue.Empty:
                    continue
                proc.stdin.write(b)
                n_written += 1
                m = int(np.max(np.abs(np.frombuffer(b, dtype=np.int8))))
                if m > max_iq: max_iq = m
        except Exception as e:
            print(f"[HACKRF] TX writer err after {n_written} chunks: {e}", flush=True)
        err_s = ""
        try:
            if proc.stderr: err_s = proc.stderr.read(200).decode(errors="ignore").replace("\n", " | ")
        except Exception: pass
        print(f"[HACKRF] TX writer done: {n_written} chunks (pre={len(pre)}, max_iq={max_iq}) err={err_s}", flush=True)
        while not self.tx_queue.empty():
            try: self.tx_queue.get_nowait()
            except queue.Empty: break

    def _rx_loop(self, proc: subprocess.Popen):
        need_bytes = CHUNK_IQ * 2
        stdout = proc.stdout
        hdr = pack_tci_header(0, FS_AUDIO, 3, CHUNK_FLOATS, STREAM_RX_AUDIO)
        while self.running and self.rx_proc is proc:
            raw = stdout.read(need_bytes)
            if not raw or len(raw) < need_bytes:
                break
            iq = np.frombuffer(raw, dtype=np.int8).astype(np.float32) * (1.0 / 128.0)
            z = iq[0::2] + 1j * iq[1::2]

            # 1) Shift down by (IF_OFFSET_HZ + 1500 Hz)
            ph = self.rx_nco_phase - 2.0 * math.pi * (IF_OFFSET_HZ + 1500.0) * self.t_rf / FS_RF
            self.rx_nco_phase = (self.rx_nco_phase - 2.0 * math.pi * (IF_OFFSET_HZ + 1500.0) * CHUNK_IQ / FS_RF) % (2.0 * math.pi)
            z_bb = z * np.exp(1j * ph).astype(np.complex64)

            # 2) Decimate by 50 (boxcar -> 48 kHz)
            z_48 = z_bb.reshape(CHUNK_FRAMES, DECIM).mean(axis=1)

            # 3) Low-pass filter ±1450 Hz for clean USB selectivity
            buf = np.concatenate((self.rx_lpf_state, z_48))
            z_filt = np.convolve(buf, self.lpf, mode="valid").astype(np.complex64)
            self.rx_lpf_state = buf[-(len(self.lpf) - 1):].copy()

            # Estimate S-meter power
            pwr = float(np.mean(np.abs(z_filt) ** 2)) + 1e-12
            self.last_rssi_dbm = max(-127.0, min(-10.0, 10.0 * math.log10(pwr) - 35.0))

            # 4) Shift up by +1500 Hz and take real part -> pure 48 kHz USB audio
            ph_up = self.rx_up_phase + 2.0 * math.pi * 1500.0 * self.t_aud / FS_AUDIO
            self.rx_up_phase = (self.rx_up_phase + 2.0 * math.pi * 1500.0 * CHUNK_FRAMES / FS_AUDIO) % (2.0 * math.pi)
            audio = np.real(z_filt * np.exp(1j * ph_up).astype(np.complex64)) * self.rx_scale
            audio = np.clip(audio, -0.95, 0.95).astype(np.float32)

            if self.audio_streaming and self.on_rx_chunk is not None:
                stereo = np.empty(CHUNK_FLOATS, dtype=np.float32)
                stereo[0::2] = audio
                stereo[1::2] = audio
                self.on_rx_chunk(hdr + stereo.tobytes())


class TCIBridgeServer:
    def __init__(self, hackrf: HackRFWorker):
        self.hackrf = hackrf
        self.loop = None
        self.client_writer = None
        self.write_lock = None
        self.vfo0 = hackrf.dial_hz
        self.vfo1 = hackrf.dial_hz
        self.mode = "digu"
        self.split = False
        self.trx = False
        self.audio_on = False
        self.chrono_task = None

    def send_rx_from_thread(self, payload: bytes):
        if self.loop is None or self.client_writer is None:
            return
        self.loop.call_soon_threadsafe(self._enqueue_binary, payload)

    def _enqueue_binary(self, payload: bytes):
        if self.client_writer is None or self.client_writer.is_closing():
            return
        try:
            self.client_writer.write(ws_encode_frame(0x2, payload))
        except Exception:
            pass

    async def send_text(self, writer: asyncio.StreamWriter, text: str):
        if writer.is_closing():
            return
        writer.write(ws_encode_frame(0x1, text.encode("utf-8")))
        await writer.drain()

    async def send_binary(self, writer: asyncio.StreamWriter, payload: bytes):
        if writer.is_closing():
            return
        writer.write(ws_encode_frame(0x2, payload))
        await writer.drain()

    async def tx_chrono_loop(self, writer: asyncio.StreamWriter):
        hdr = pack_tci_header(0, FS_AUDIO, 3, CHUNK_FLOATS, STREAM_TX_CHRONO)
        interval = CHUNK_FRAMES / float(FS_AUDIO)
        # Prime 5 chunks (~106 ms) immediately so HackRF TX pipe never underruns
        for _ in range(5):
            if not self.trx or writer.is_closing():
                return
            await self.send_binary(writer, hdr)
            await asyncio.sleep(0.002)
        next_t = time.monotonic()
        try:
            while self.trx and not writer.is_closing():
                await self.send_binary(writer, hdr)
                next_t += interval
                dt = next_t - time.monotonic()
                if dt > 0:
                    await asyncio.sleep(dt)
                else:
                    next_t = time.monotonic()
        except asyncio.CancelledError:
            pass

    async def handle_tci_command(self, writer: asyncio.StreamWriter, raw_cmd: str):
        for item in raw_cmd.split(";"):
            item = item.strip()
            if not item:
                continue
            if ":" in item:
                name, arg_str = item.split(":", 1)
                args = [a.strip() for a in arg_str.split(",")]
            else:
                name, args = item, []
            name_l = name.lower()

            if name_l == "start":
                await self.send_text(writer, "start;")
            elif name_l == "stop":
                await self.send_text(writer, "stop;")
            elif name_l == "audio_start":
                self.audio_on = True
                self.hackrf.audio_streaming = True
                await self.send_text(writer, f"audio_start:{args[0] if args else 0};")
            elif name_l == "audio_stop":
                self.audio_on = False
                self.hackrf.audio_streaming = False
                await self.send_text(writer, f"audio_stop:{args[0] if args else 0};")
            elif name_l == "vfo" and len(args) >= 3:
                rx_i, ch_i, freq_s = args[0], args[1], args[2]
                freq = int(float(freq_s))
                if ch_i == "0":
                    self.vfo0 = freq
                    asyncio.get_running_loop().run_in_executor(None, self.hackrf.set_dial_hz, freq)
                else:
                    self.vfo1 = freq
                await self.send_text(writer, f"vfo:{rx_i},{ch_i},{freq};")
            elif name_l == "modulation" and len(args) >= 2:
                self.mode = args[1].lower()
                await self.send_text(writer, f"modulation:{args[0]},{self.mode};")
            elif name_l == "split_enable" and len(args) >= 2:
                self.split = (args[1].lower() == "true")
                await self.send_text(writer, f"split_enable:{args[0]},{"true" if self.split else "false"};")
            elif name_l in ("trx", "tune") and len(args) >= 2:
                on = (args[1].lower() == "true")
                self.trx = on
                asyncio.get_running_loop().run_in_executor(None, self.hackrf.set_ptt, on)
                if self.chrono_task is not None:
                    self.chrono_task.cancel()
                    self.chrono_task = None
                await self.send_text(writer, f"{name_l}:{args[0]},{"true" if on else "false"};")
                if on:
                    self.chrono_task = asyncio.create_task(self.tx_chrono_loop(writer))
            elif name_l == "rx_smeter":
                s_val = int(round(self.hackrf.last_rssi_dbm))
                await self.send_text(writer, f"rx_smeter:0,0,{s_val};")
            elif name_l in ("drive", "tune_drive", "volume", "rx_sensors_enable", "tx_sensors_enable", "rx_enable"):
                await self.send_text(writer, f"{item};")

    async def handle_client(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
        peer = writer.get_extra_info("peername")
        print(f"[TCI] Client connected from {peer}", flush=True)
        try:
            # HTTP WebSocket Upgrade handshake
            req_lines = []
            while True:
                line = await reader.readline()
                if not line or line in (b"\r\n", b"\n"):
                    break
                req_lines.append(line.decode("latin1").strip())
            headers = {}
            for line in req_lines[1:]:
                if ":" in line:
                    k, v = line.split(":", 1)
                    headers[k.strip().lower()] = v.strip()
            ws_key = headers.get("sec-websocket-key", "")
            if not ws_key:
                writer.close()
                return
            accept = base64.b64encode(hashlib.sha1((ws_key + WS_GUID).encode("ascii")).digest()).decode("ascii")
            resp = (
                "HTTP/1.1 101 Switching Protocols\r\n"
                "Upgrade: websocket\r\n"
                "Connection: Upgrade\r\n"
                f"Sec-WebSocket-Accept: {accept}\r\n\r\n"
            )
            writer.write(resp.encode("ascii"))
            await writer.drain()

            self.client_writer = writer
            self.hackrf.on_rx_chunk = self.send_rx_from_thread

            # Send initial TCI state snapshot + ready;
            init_cmds = [
                "protocol:esdr,1.8;",
                "device:HackRF-RAD950;",
                "receive_only:false;",
                "trx_count:1;",
                "channels_count:2;",
                "vfo_limits:1000000,6000000000;",
                "if_limits:-1000000,1000000;",
                "modulations_list:am,cw,usb,lsb,digl,digu,wfm,nfm;",
                f"audio_samplerate:{FS_AUDIO};",
                f"vfo:0,0,{self.vfo0};",
                f"vfo:0,1,{self.vfo1};",
                f"modulation:0,{self.mode};",
                "trx:0,false;",
                "rx_enable:0,true;",
                "tx_enable:0,true;",
                "split_enable:0,false;",
                "start;",
                "ready;",
            ]
            for c in init_cmds:
                await self.send_text(writer, c)

            # Main WebSocket frame loop
            while True:
                opcode, payload = await ws_read_frame(reader)
                if opcode == 0x8:  # Close
                    break
                elif opcode == 0x9:  # Ping
                    writer.write(ws_encode_frame(0xA, payload))
                    await writer.drain()
                elif opcode == 0x1:  # Text (TCI CAT command)
                    msg = payload.decode("utf-8", errors="ignore")
                    await self.handle_tci_command(writer, msg)
                elif opcode == 0x2:  # Binary (TxAudioStream)
                    if len(payload) >= 64:
                        hdr = struct.unpack("<16I", payload[:64])
                        length, stype = hdr[5], hdr[6]
                        if stype == STREAM_TX_AUDIO and self.trx:
                            n_floats = min(length, (len(payload) - 64) // 4)
                            if n_floats > 0:
                                floats = np.frombuffer(payload[64:64 + n_floats * 4], dtype=np.float32)
                                self.hackrf.feed_tx_audio(floats)
        except (asyncio.IncompleteReadError, ConnectionResetError, BrokenPipeError):
            pass
        except Exception as e:
            print(f"[TCI] Client error: {e}", flush=True)
        finally:
            print(f"[TCI] Client disconnected: {peer}", flush=True)
            if self.chrono_task is not None:
                self.chrono_task.cancel()
                self.chrono_task = None
            if self.trx:
                self.trx = False
                self.hackrf.set_ptt(False)
            self.hackrf.audio_streaming = False
            self.hackrf.on_rx_chunk = None
            if self.client_writer is writer:
                self.client_writer = None
            try:
                writer.close()
                await writer.wait_closed()
            except Exception:
                pass


async def main_async(args):
    hackrf = HackRFWorker(
        serial=args.serial,
        dial_hz=args.freq,
        lna=args.lna,
        vga=args.vga,
        tx_gain=args.tx_gain,
        rx_scale=args.rx_scale,
        dry_tx=args.dry_tx,
    )
    bridge = TCIBridgeServer(hackrf)
    bridge.loop = asyncio.get_running_loop()
    hackrf.start()

    server = await asyncio.start_server(bridge.handle_client, args.bind, args.port)
    print(f"[TCI] Listening on ws://{args.bind}:{args.port} (HackRF {args.serial}, dial {args.freq} Hz)", flush=True)

    stop_ev = asyncio.Event()
    for sig in (signal.SIGINT, signal.SIGTERM):
        bridge.loop.add_signal_handler(sig, stop_ev.set)
    try:
        await stop_ev.wait()
    finally:
        server.close()
        await server.wait_closed()
        hackrf.stop()


def main():
    p = argparse.ArgumentParser(description="WSJT-X TCI-to-HackRF Bridge")
    p.add_argument("--serial", default="0000000000000000644064dc2a9f68cd", help="HackRF serial number")
    p.add_argument("--bind", default="0.0.0.0", help="Bind IP address")
    p.add_argument("--port", type=int, default=50001, help="TCI WebSocket port (default 50001)")
    p.add_argument("--freq", type=int, default=18104000, help="Initial USB dial frequency in Hz")
    p.add_argument("--lna", type=int, default=32, help="HackRF RX LNA gain (0..40 dB)")
    p.add_argument("--vga", type=int, default=24, help="HackRF RX VGA gain (0..62 dB)")
    p.add_argument("--tx-gain", type=int, default=24, help="HackRF TX VGA gain (0..47 dB)")
    p.add_argument("--rx-scale", type=float, default=8.0, help="RX audio scale factor into WSJT-X")
    p.add_argument("--dry-tx", action="store_true", help="Do not key HackRF TX (receive-only test)")
    args = p.parse_args()
    asyncio.run(main_async(args))


if __name__ == "__main__":
    main()
