#!/usr/bin/env python3
"""Offline unit tests for analyze.py's metric functions and evaluation
rules. No network, no PipeWire, no X server; seeded RNGs only. Runs both as:

  python3 -m pytest -q -p no:cacheprovider scripts/e2e
  python3 scripts/e2e/test_analyze.py
"""

from __future__ import annotations

import os
import struct
import sys
import tempfile
import wave

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import numpy as np  # noqa: E402

import analyze  # noqa: E402

FS = 48000


# ---------------------------------------------------------------------------
# Synthetic speech-like signal generator (shared by latency tests)
# ---------------------------------------------------------------------------


def _speech_like(duration_s: float, fs: int = FS, seed: int = 0) -> np.ndarray:
    rng = np.random.default_rng(seed)
    n = int(duration_s * fs)
    noise = rng.standard_normal(n)
    freqs = np.fft.rfftfreq(n, 1.0 / fs)
    X = np.fft.rfft(noise)
    X[freqs < 200] = 0
    hp = np.fft.irfft(X, n)
    burst_rate = rng.uniform(3, 8)
    t = np.arange(n) / fs
    env = (0.5 * (1 + np.sin(2 * np.pi * burst_rate * t + rng.uniform(0, 2 * np.pi)))) ** 2
    sig = hp * env
    peak = np.max(np.abs(sig)) + 1e-9
    return sig / peak


def _delayed_output(sig: np.ndarray, fs: int, delay_ms: float, seed: int = 1) -> np.ndarray:
    d = int(round(delay_ms * fs / 1000))
    out = np.zeros_like(sig)
    if d == 0:
        out[:] = sig
    elif d < len(sig):
        out[d:] = sig[: len(sig) - d]
    out = out * 0.5
    freqs = np.fft.rfftfreq(len(out), 1.0 / fs)
    X = np.fft.rfft(out)
    X[freqs > 4000] = 0
    out = np.fft.irfft(X, len(out))
    rng = np.random.default_rng(seed)
    out = out + 1e-4 * rng.standard_normal(len(out))
    return out


def test_known_delay():
    sig = _speech_like(12.0)
    for delay_ms in (0, 37, 120, 330):
        out = _delayed_output(sig, FS, delay_ms)
        lag, corr, _ = analyze.measure_latency(sig, out, FS)
        assert abs(lag - delay_ms) <= 5, f"delay={delay_ms} measured={lag}"
        assert corr >= 0.8, f"delay={delay_ms} corr={corr}"


# ---------------------------------------------------------------------------
# WAV reading
# ---------------------------------------------------------------------------


def _write_unfinalized_float_wav(path: str, frames: np.ndarray, fs: int = FS) -> None:
    """Write a 2-channel float32 WAV whose data chunk declares size
    0xFFFFFFFF, mimicking what an interrupted `pw-record` leaves behind."""
    data = frames.astype("<f4").tobytes()
    channels = frames.shape[1]
    byte_rate = fs * channels * 4
    block_align = channels * 4
    fmt_chunk = struct.pack("<HHIIHH", 3, channels, fs, byte_rate, block_align, 32)
    with open(path, "wb") as fh:
        fh.write(b"RIFF")
        fh.write(struct.pack("<I", 0xFFFFFFFF))  # bogus RIFF size, also unfinalized
        fh.write(b"WAVE")
        fh.write(b"fmt ")
        fh.write(struct.pack("<I", len(fmt_chunk)))
        fh.write(fmt_chunk)
        fh.write(b"data")
        fh.write(struct.pack("<I", 0xFFFFFFFF))  # the unfinalized marker under test
        fh.write(data)


def test_read_wav_unfinalized():
    rng = np.random.default_rng(2)
    frames = rng.uniform(-0.5, 0.5, size=(4800, 2)).astype(np.float32)
    with tempfile.TemporaryDirectory() as td:
        path = os.path.join(td, "unfinalized.wav")
        _write_unfinalized_float_wav(path, frames)
        x, rate = analyze.read_wav(path)
        assert rate == FS
        assert x.shape == (4800, 2)
        assert np.allclose(x, frames.astype(np.float64), atol=1e-6)

        # A 16-bit mono WAV reads back scaled to [-1, 1).
        mono_path = os.path.join(td, "mono16.wav")
        samples16 = (rng.uniform(-0.9, 0.9, size=2000) * 32767).astype(np.int16)
        with wave.open(mono_path, "wb") as wf:
            wf.setnchannels(1)
            wf.setsampwidth(2)
            wf.setframerate(FS)
            wf.writeframes(samples16.tobytes())
        xm, ratem = analyze.read_wav(mono_path)
        assert ratem == FS
        assert xm.shape == (2000, 1)
        expected = samples16.astype(np.float64) / 32768.0
        assert np.allclose(xm[:, 0], expected, atol=1e-6)
        assert np.all(xm >= -1.0) and np.all(xm < 1.0)


# ---------------------------------------------------------------------------
# Latency evaluation rule
# ---------------------------------------------------------------------------


def test_latency_rule():
    thresholds = {
        "latency_max_ms": 80,
        "latency_extra_lowcpu_ms": 150,
        "lag_corr_min": 0.5,
        "latency_drift_max_ms": 15,
    }

    passing = {"latency_ms": 60, "lag_corr": 0.9, "latency_spread_ms": 5}
    rows = {r.metric: r for r in analyze.eval_speech(passing, thresholds)}
    assert rows["latency_ms"].result == "PASS"

    failing = {"latency_ms": 330, "lag_corr": 0.9, "latency_spread_ms": 5}
    rows = {r.metric: r for r in analyze.eval_speech(failing, thresholds)}
    assert rows["latency_ms"].result == "FAIL"

    unmeasurable = {"latency_ms": 60, "lag_corr": 0.3, "latency_spread_ms": 5}
    rows = {r.metric: r for r in analyze.eval_speech(unmeasurable, thresholds)}
    assert rows["latency_ms"].result == "FAIL"
    assert rows["latency_ms"].note == "latency unmeasurable"

    lowcpu = {"latency_ms": 220, "lag_corr": 0.9, "latency_spread_ms": 5, "mode": "LowCpu"}
    rows = {r.metric: r for r in analyze.eval_speech(lowcpu, thresholds)}
    assert rows["latency_ms"].result == "PASS"  # 80 + 150 == 230 >= 220

    lowcpu_over = {"latency_ms": 240, "lag_corr": 0.9, "latency_spread_ms": 5, "mode": "LowCpu"}
    rows = {r.metric: r for r in analyze.eval_speech(lowcpu_over, thresholds)}
    assert rows["latency_ms"].result == "FAIL"


def _run_all():
    failures = []
    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and callable(fn):
            try:
                fn()
                print(f"PASS: {name}")
            except AssertionError as exc:
                failures.append(name)
                print(f"FAIL: {name}: {exc}")
    if failures:
        print(f"\n{len(failures)} failed: {', '.join(failures)}")
        raise SystemExit(1)
    print("\nall tests passed")


if __name__ == "__main__":
    _run_all()
