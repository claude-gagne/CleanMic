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


# ---------------------------------------------------------------------------
# Task 2: repeats, zero runs / holes, DC, settled gain, log scanning, rules
# ---------------------------------------------------------------------------


def test_exact_repeat_frac():
    rng = np.random.default_rng(3)
    hop = 480
    n = FS * 10
    x = (rng.standard_normal(n) * 0.1).astype(np.float64)

    frac0, _ev0 = analyze.exact_repeat_frac(x.astype(np.float32), hop=hop)
    assert abs(frac0 - 0.0) < 1e-6, f"no copies should give 0.0, got {frac0}"

    y = x.copy()
    copied_hops = range(100, 120)  # 20 hops, well past the 50-hop warm-up skip
    for k in copied_hops:
        start = k * hop
        y[start : start + hop] = y[start - hop : start]
    frac, evaluated = analyze.exact_repeat_frac(y.astype(np.float32), hop=hop)
    expected = len(list(copied_hops)) / evaluated
    assert abs(frac - expected) < 0.002, f"frac={frac} expected~={expected}"


def test_dc():
    sig = _speech_like(5.0, seed=7)
    dc_with_offset = analyze.dc_offset(sig + 0.1)
    assert abs(dc_with_offset - 0.1) < 1e-3

    rng = np.random.default_rng(8)
    zero_mean = rng.standard_normal(FS * 5)
    zero_mean = zero_mean - zero_mean.mean()
    assert abs(analyze.dc_offset(zero_mean)) < 1e-3


def test_zero_runs_and_holes():
    rng = np.random.default_rng(9)
    n = FS * 2
    x = rng.standard_normal(n) * 0.3
    mid = n // 2
    gap10 = int(0.010 * FS)  # 480 samples, hop/frame-aligned
    x[mid : mid + gap10] = 0.0
    zr = analyze.zero_runs(x)
    assert len(zr) == 1
    assert zr[0][1] == gap10
    assert analyze.holes(x, FS) == 1

    y = rng.standard_normal(n) * 0.3
    gap3 = int(0.003 * FS)  # under the 5 ms (240-sample) minimum
    y[mid : mid + gap3] = 0.0
    assert analyze.zero_runs(y) == []


def test_settled_gain():
    rng = np.random.default_rng(11)
    n = FS * 4
    inp = rng.standard_normal(n) * 0.1
    out = inp.copy()
    half = n // 2
    gain = 10 ** (16 / 20)
    out[half:] *= gain
    g = analyze.settled_gain_db(inp, out)
    assert abs(g - 16) < 0.1


def test_logscan():
    log_text = (
        "[t INFO x] Engine changed to Dpdfnet2 (mode=MaxQuality)\n"
        "[t INFO x] Engine changed to RNNoise (mode=LowCpu)\n"
        "[t WARN x] Audio thread fell 250 ms behind (engine slower than real time?)\n"
        "[t ERROR x] something broke\n"
        "[t INFO x] Discarded 107 ms of capture audio queued while processing was stopped\n"
        "[t INFO x] Discarded 55 ms of capture audio queued while processing was stopped\n"
    )
    result = analyze.logscan(log_text)
    assert result["fell_behind"] == 1
    assert result["errors"] == 1
    assert result["panics"] == 0
    assert result["discarded"] == 2
    assert result["engine_changed"] == [("Dpdfnet2", "MaxQuality"), ("RNNoise", "LowCpu")]


def test_swap_sequence_rule():
    expected = [("RNNoise", "MaxQuality"), ("DeepFilterNet", "MaxQuality"), ("Dpdfnet2", "LowCpu")]

    result, _note = analyze.evaluate_swap_sequence(expected, list(expected))
    assert result == "PASS"

    logged_bad = [("RNNoise", "MaxQuality"), ("DeepFilterNet", "Balanced"), ("Dpdfnet2", "LowCpu")]
    result2, note2 = analyze.evaluate_swap_sequence(expected, logged_bad)
    assert result2 == "FAIL"
    assert "index 1" in note2

    logged_short = [("RNNoise", "MaxQuality")]
    result3, note3 = analyze.evaluate_swap_sequence(expected, logged_short)
    assert result3 == "FAIL"
    assert "index 1" in note3


def test_swaps_during_rule_ignores_latency_and_holes():
    # swaps_during is a 15-swap recording: latency/lag_corr/spread/holes are
    # all expected to look "bad" by steady-speech standards (crossfades),
    # and must NOT be evaluated -- only the repeat-bug signature is real.
    thresholds = {"repeat_frac_max": 0.001, "holes_max": 0, "latency_max_ms": 120}
    measured = {
        "latency_ms": 109,
        "lag_corr": 0.3,
        "latency_spread_ms": 161.0,
        "holes": 2,
        "exact_repeat_frac": 0.0002,
    }
    rows = analyze.eval_swaps_during(measured, thresholds)
    metrics = {r.metric for r in rows}
    assert metrics == {"exact_repeat_frac"}
    assert rows[0].result == "PASS"


def test_dc_and_autogain_rules():
    assert analyze.eval_dc_within(3e-7, 1e-3) == "PASS"
    assert analyze.eval_dc_within(0.05, 1e-3) == "FAIL"

    assert analyze.eval_autogain_boost(16, 10) == "PASS"
    assert analyze.eval_autogain_off_deviation(-1.5, 3) == "PASS"
    assert analyze.eval_autogain_noise_diff(-40, -41, 3) == "PASS"
    assert analyze.eval_autogain_noise_diff(-40, -46, 3) == "FAIL"


def test_dead_speech():
    rng = np.random.default_rng(21)
    n = FS * 3
    inp = rng.standard_normal(n) * 0.2  # speech-level input throughout
    out = inp * 0.5
    assert analyze.dead_speech(inp, out, FS) == (0.0, 0.0)
    # One 10 ms plugin hole: dead, but a 10 ms run.
    out1 = out.copy()
    out1[FS : FS + 480] = 0.0
    assert analyze.dead_speech(inp, out1, FS) == (10.0, 10.0)
    # The old crash: output gone for the last 2 s.
    out2 = out.copy()
    out2[FS:] = 0.0
    run, total = analyze.dead_speech(inp, out2, FS)
    assert run == total == 2000.0
    # Deep but real suppression (-79 dBFS out for a loud noise-only input,
    # as RNNoise did in the first stress run) is NOT dead output.
    out_supp = out.copy()
    out_supp[FS : 2 * FS] = 1.1e-4 * rng.standard_normal(FS)  # ~ -79 dBFS
    assert analyze.dead_speech(inp, out_supp, FS) == (0.0, 0.0)
    # Denoised pauses (quiet input, near-silent output) are not dead, and do
    # not break a dead run that spans them.
    inp3 = inp.copy()
    inp3[FS : FS + 4800] *= 1e-4  # 100 ms pause at -94 dBFS
    out3 = np.zeros_like(inp3)
    run3, total3 = analyze.dead_speech(inp3, out3, FS)
    assert run3 == total3 == 2900.0


def test_logscan_fallback_and_plugin_abort():
    log_text = (
        "[2026-09-24T15:24:14.000Z WARN  cleanmic::engine::deepfilter] DeepFilterNet: plugin fell behind real time 8 times\n"
        "[2026-09-24T15:24:14.500Z WARN  cleanmic::engine::deepfilter] DeepFilterNet cannot keep up with real time on this computer\n"
        "[2026-09-24T15:24:14.600Z WARN  cleanmic::app] Engine fallback: DeepFilterNet -> RNNoise (Overloaded)\n"
        "thread '<unnamed>' panicked at 'DF 1 | Processing too slow! Please upgrade your CPU.', ladspa/src/lib.rs:444:17\n"
    )
    r = analyze.logscan(log_text)
    assert r["dfn_restarts"] == 1 and r["dfn_gave_up"] == 1 and r["plugin_aborts"] == 1
    assert len(r["fallbacks"]) == 1
    fb = r["fallbacks"][0]
    assert (fb["from"], fb["to"], fb["reason"]) == ("DeepFilterNet", "RNNoise", "Overloaded")
    assert abs(fb["t"] - 1790263454.6) < 1e-3
    assert analyze.active_engine_after(r, "DeepFilterNet") == "RNNoise"
    assert analyze.active_engine_after({"fallbacks": []}, "DeepFilterNet") == "DeepFilterNet"

    rows = {x["metric"]: x for x in analyze.evaluate_stress(r, True, fb["t"] - 3.0, 10)}
    assert rows["app_alive"]["result"] == "PASS"
    assert rows["plugin_abort_lines"]["result"] == "FAIL"
    assert rows["recovery_s"]["value"] == 3.0 and rows["recovery_s"]["result"] == "PASS"
    slow = {x["metric"]: x for x in analyze.evaluate_stress(r, False, fb["t"] - 30.0, 10)}
    assert slow["app_alive"]["result"] == "FAIL" and slow["recovery_s"]["result"] == "FAIL"
    early = {x["metric"]: x for x in analyze.evaluate_stress(r, True, fb["t"] + 0.7, 10)}
    assert early["recovery_s"]["value"] == 0.0 and early["recovery_s"]["result"] == "PASS"
    assert "before the synthetic load" in early["recovery_s"]["note"]
    none = {x["metric"]: x for x in analyze.evaluate_stress({"fallbacks": []}, True, 0.0, 10)}
    assert none["recovery_s"]["result"] == "INFO"


def test_stress_load_rule_and_cpu_flag():
    th = {"stress_dead_run_max_ms": 200, "load_flag_busy_pct": 25}
    ok = analyze.evaluate({"kind": "stress_load", "dead_run_ms": 20.0, "cpu_busy_pct": "61.5"}, th)
    by = {r.metric: r for r in ok}
    assert by["dead_run_ms"].result == "PASS"
    assert by["cpu_busy_pct"].result == "INFO" and by["cpu_busy_pct"].note == "ran under load"
    pinned = analyze.evaluate({"kind": "stress_load", "dead_run_ms": 0.0, "cpu_busy_pct": "15", "stress_spinners": "6"}, th)
    assert "synthetic load" in {r.metric: r for r in pinned}["cpu_busy_pct"].note
    bad = analyze.evaluate({"kind": "stress_load", "dead_run_ms": 15380.0}, th)
    assert {r.metric: r for r in bad}["dead_run_ms"].result == "FAIL"
    quiet = analyze.evaluate({"kind": "speech", "latency_ms": 50, "lag_corr": 0.6, "latency_spread_ms": 2, "cpu_busy_pct": "12"}, th)
    assert {r.metric: r for r in quiet}["cpu_busy_pct"].note == ""
    assert analyze.load_summary([{"cpu_busy_pct": "12"}, {"cpu_busy_pct": "61.5"}], th) == ("61.5", "yes")
    assert analyze.load_summary([{}], th) == ("n/a", "unknown")


def test_speech_rule_flags_a_dead_mic():
    th = {"latency_max_ms": 120, "lag_corr_min": 0.5, "dead_run_max_ms": 200}
    base = {"kind": "speech", "latency_ms": 60, "lag_corr": 0.6, "latency_spread_ms": 2}
    ok = {r.metric: r for r in analyze.evaluate(dict(base, dead_run_ms=10.0), th)}
    assert ok["dead_run_ms"].result == "PASS"
    crashed = {r.metric: r for r in analyze.evaluate(dict(base, dead_run_ms=13150.0), th)}
    assert crashed["dead_run_ms"].result == "FAIL"
    # Older measurement JSON without the field: no row, no false FAIL.
    assert "dead_run_ms" not in {r.metric for r in analyze.evaluate(base, th)}


def test_per_engine_holes_override():
    th = {"latency_max_ms": 120, "lag_corr_min": 0.5, "holes_max": 0, "holes_max_deepfilternet": 8}
    base = {"kind": "speech", "latency_ms": 60, "lag_corr": 0.6, "latency_spread_ms": 2, "holes": 3}
    dfn = {r.metric: r for r in analyze.evaluate(dict(base, engine="DeepFilterNet"), th)}
    assert dfn["holes"].result == "PASS" and dfn["holes"].threshold == "<= 8"
    other = {r.metric: r for r in analyze.evaluate(dict(base, engine="Dpdfnet8"), th)}
    assert other["holes"].result == "FAIL" and other["holes"].threshold == "<= 0"


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
