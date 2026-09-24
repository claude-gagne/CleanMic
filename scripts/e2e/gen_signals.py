#!/usr/bin/env python3
"""gen_signals.py -- deterministic test-signal generation for the CleanMic
silent E2E harness, built from assets/demo/*-before.wav plus one synthetic
pink-noise signal.

USAGE
  gen_signals.py --out DIR [--demo-dir REPO_ROOT/assets/demo]

Writes 16-bit mono 48 kHz WAVs into DIR: speech.wav, speech_m40.wav,
speech_dc.wav, silence_dc.wav, pink_m45.wav, speech_loop60.wav, plus a
signals.json manifest (name, duration_s, rms_dbfs, dc per file).

EXIT CODES
  0  ok
  3  numpy is missing, or a demo asset is not 48 kHz / mono / 16-bit
"""

from __future__ import annotations

import argparse
import json
import os
import struct
import sys

try:
    import numpy as np
except ImportError:
    print("python3 numpy is required: sudo apt install python3-numpy", file=sys.stderr)
    raise SystemExit(3)

FS = 48000


def _read_pcm16_mono_48k(path: str) -> np.ndarray:
    with open(path, "rb") as fh:
        blob = fh.read()
    if blob[0:4] != b"RIFF" or blob[8:12] != b"WAVE":
        print(f"gen_signals: not a RIFF/WAVE file: {path}", file=sys.stderr)
        raise SystemExit(3)
    i = 12
    n = len(blob)
    fmt = None
    data = None
    while i + 8 <= n:
        chunk_id = blob[i : i + 4]
        size = struct.unpack("<I", blob[i + 4 : i + 8])[0]
        body_start = i + 8
        if chunk_id == b"fmt ":
            fmt = struct.unpack("<HHIIHH", blob[body_start : body_start + 16])
        elif chunk_id == b"data":
            data = blob[body_start : body_start + size]
            break
        i = body_start + size + (size & 1)
    if fmt is None or data is None:
        print(f"gen_signals: missing fmt/data chunk: {path}", file=sys.stderr)
        raise SystemExit(3)
    _audio_format, channels, rate, _byte_rate, _block_align, bits = fmt
    if rate != FS or channels != 1 or bits != 16:
        print(
            f"gen_signals: {path} is {rate} Hz / {channels}ch / {bits}-bit, "
            f"expected {FS} Hz / 1ch / 16-bit",
            file=sys.stderr,
        )
        raise SystemExit(3)
    return np.frombuffer(data, dtype="<i2").astype(np.float64) / 32768.0


def _write_pcm16(path: str, x: np.ndarray, fs: int = FS) -> None:
    clipped = np.clip(x, -1.0, 32767.0 / 32768.0)
    samples = (clipped * 32768.0).astype("<i2")
    data = samples.tobytes()
    fmt_chunk = struct.pack("<HHIIHH", 1, 1, fs, fs * 2, 2, 16)
    with open(path, "wb") as fh:
        fh.write(b"RIFF")
        fh.write(struct.pack("<I", 4 + (8 + len(fmt_chunk)) + (8 + len(data))))
        fh.write(b"WAVE")
        fh.write(b"fmt ")
        fh.write(struct.pack("<I", len(fmt_chunk)))
        fh.write(fmt_chunk)
        fh.write(b"data")
        fh.write(struct.pack("<I", len(data)))
        fh.write(data)


def _rms_dbfs(x: np.ndarray) -> float:
    return float(20 * np.log10(np.sqrt(np.mean(np.square(x, dtype=np.float64))) + 1e-12))


def _scale_to_dbfs(x: np.ndarray, target_db: float) -> np.ndarray:
    current = _rms_dbfs(x)
    gain_db = target_db - current
    return x * (10 ** (gain_db / 20.0))


def _pink_noise(duration_s: float, fs: int = FS, seed: int = 1) -> np.ndarray:
    n = int(duration_s * fs)
    rng = np.random.default_rng(seed)
    white = rng.standard_normal(n)
    X = np.fft.rfft(white)
    freqs = np.fft.rfftfreq(n, 1.0 / fs)
    shaping = np.ones_like(freqs)
    nonzero = freqs > 0
    shaping[nonzero] = 1.0 / np.sqrt(freqs[nonzero])
    shaping[~nonzero] = shaping[nonzero][0] if np.any(nonzero) else 1.0
    pink = np.fft.irfft(X * shaping, n)
    return pink / (np.max(np.abs(pink)) + 1e-9)


def generate(out_dir: str, demo_dir: str) -> dict:
    os.makedirs(out_dir, exist_ok=True)
    keyboard = _read_pcm16_mono_48k(os.path.join(demo_dir, "deepfilter-keyboard-before.wav"))
    fan = _read_pcm16_mono_48k(os.path.join(demo_dir, "rnnoise-fan-before.wav"))
    speech = np.concatenate([keyboard, fan])

    speech_m40 = _scale_to_dbfs(np.tile(speech, 2), -40.0)
    speech_dc = np.clip(speech + 0.1, -1.0, 32767.0 / 32768.0)
    silence_dc = np.full(int(10 * FS), 0.1)
    pink_m45 = _scale_to_dbfs(_pink_noise(20.0), -45.0)
    speech_loop60 = np.tile(speech, 3)

    files = {
        "speech.wav": speech,
        "speech_m40.wav": speech_m40,
        "speech_dc.wav": speech_dc,
        "silence_dc.wav": silence_dc,
        "pink_m45.wav": pink_m45,
        "speech_loop60.wav": speech_loop60,
    }

    manifest = {}
    for name, x in files.items():
        path = os.path.join(out_dir, name)
        _write_pcm16(path, x)
        manifest[name] = {
            "duration_s": round(len(x) / FS, 2),
            "rms_dbfs": round(_rms_dbfs(x), 2),
            "dc": round(float(np.mean(x)), 5),
        }

    with open(os.path.join(out_dir, "signals.json"), "w", encoding="utf-8") as fh:
        json.dump(manifest, fh, indent=2)

    return manifest


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", required=True)
    repo_root = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    parser.add_argument("--demo-dir", default=os.path.join(repo_root, "assets", "demo"))
    args = parser.parse_args(argv)

    manifest = generate(args.out, args.demo_dir)
    for name, info in manifest.items():
        print(f"gen_signals: {name}: {info['duration_s']}s rms={info['rms_dbfs']}dBFS dc={info['dc']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
