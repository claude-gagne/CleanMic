#!/usr/bin/env python3
"""Offline unit tests for analyze.py's metric functions and evaluation
rules. No network, no PipeWire, no X server; seeded RNGs only. Runs both as:

  python3 -m pytest -q -p no:cacheprovider scripts/e2e
  python3 scripts/e2e/test_analyze.py
"""

from __future__ import annotations

import json
import os
import struct
import sys
import tempfile
import wave

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import numpy as np  # noqa: E402

import analyze  # noqa: E402
import contention  # noqa: E402

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
    # quick 260924-n4s: a fallback logged BEFORE the synthetic load even
    # started tells us nothing about the grace -- INCONCLUSIVE, never PASS.
    early = {x["metric"]: x for x in analyze.evaluate_stress(r, True, fb["t"] + 0.7, 10)}
    assert early["recovery_s"]["result"] == "INCONCLUSIVE"
    assert "before the synthetic load" in early["recovery_s"]["note"]
    none = {x["metric"]: x for x in analyze.evaluate_stress({"fallbacks": []}, True, 0.0, 10)}
    assert none["recovery_s"]["result"] == "INFO"

    # recovery_min_s (D-01): a fallback FASTER than the grace is also a FAIL
    # -- the grace was not honoured.
    fast_fallback = {x["metric"]: x for x in analyze.evaluate_stress(r, True, fb["t"] - 3.0, 10, recovery_min_s=5.0)}
    assert fast_fallback["recovery_s"]["value"] == 3.0
    assert fast_fallback["recovery_s"]["result"] == "FAIL"
    ok_fallback = {x["metric"]: x for x in analyze.evaluate_stress(r, True, fb["t"] - 6.0, 10, recovery_min_s=5.0)}
    assert ok_fallback["recovery_s"]["value"] == 6.0
    assert ok_fallback["recovery_s"]["result"] == "PASS"


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


# ---------------------------------------------------------------------------
# Task 1 (R5): contention.py parsers + classification
# ---------------------------------------------------------------------------


def test_contention_parse_stat_aggregate():
    d = contention.parse_stat_aggregate("cpu  100 10 50 800 20 5 5 10")
    assert d["busy"] == 180
    assert d["iowait"] == 20
    assert d["steal"] == 10
    assert d["total"] == 1000


def test_contention_parse_pid_stat_with_parens_and_spaces():
    line = "1234 (Web Content) S 1 1 1 0 -1 4194560 100 0 0 0 200 50 0 0 20 0 4 0 1000 0 0 18446744073709551615"
    info = contention.parse_pid_stat(line)
    assert info["comm"] == "Web Content"
    assert info["ppid"] == 1
    assert info["session"] == 1
    assert info["cpu_ticks"] == 250

    line2 = "55 (a) b) S 1 1 7 0 -1 0 0 0 0 0 11 22 0 0 20 0 4 0 1 0 0 0"
    info2 = contention.parse_pid_stat(line2)
    assert info2["comm"] == "a) b"
    assert info2["session"] == 7
    assert info2["ppid"] == 1


def test_contention_parse_schedstat():
    assert contention.parse_schedstat("123 456 7") == (123, 456, 7)


def test_contention_resolve_descendants():
    ppid_map = {2: 1, 3: 2, 4: 2, 5: 99, 6: 4}
    assert contention.resolve_descendants(ppid_map, 1) == {2, 3, 4, 6}


def test_contention_has_harness_marker():
    assert contention.has_harness_marker(None, "/x") is False  # unreadable environ = no marker, never an error
    env = "A=1\x00CLEANMIC_HARNESS_STATE_ROOT=/x/y\x00B=2"
    assert contention.has_harness_marker(env, "/x/y") is True
    assert contention.has_harness_marker(env, "/x") is False  # exact-line, never a substring match


def test_contention_classify_pid():
    descendants = {10, 11}
    assert contention.classify_pid(10, "bash", 5, descendants, 99, 42, False) == "harness"  # descendant
    assert contention.classify_pid(42, "Xephyr", 5, descendants, 99, 42, False) == "harness"  # xephyr pid
    assert contention.classify_pid(20, "bash", 99, descendants, 99, 42, False) == "harness"  # same session
    assert contention.classify_pid(30, "cleanmic", 5, descendants, 99, 42, True) == "harness"  # marker
    assert contention.classify_pid(50, "pipewire", 5, descendants, 99, 42, False) == "audio_daemon"
    assert contention.classify_pid(51, "wireplumber", 5, descendants, 99, 42, False) == "audio_daemon"
    assert contention.classify_pid(52, "pipewire-pulse", 5, descendants, 99, 42, False) == "audio_daemon"
    assert contention.classify_pid(60, "firefox", 5, descendants, 99, 42, False) == "other"


# ---------------------------------------------------------------------------
# Task 1 (R5): contention.py summarize()
# ---------------------------------------------------------------------------


def _fast(t, run_ns=0, wait_ns=0, busy=0, iowait=0, steal=0, total=0, comm="cleanmic-audio", tid=1):
    return {
        "type": "fast",
        "t": float(t),
        "stat": {"busy": busy, "iowait": iowait, "steal": steal, "total": total},
        "loadavg_1m": 1.23,
        "threads": [{"tid": tid, "comm": comm, "run_ns": run_ns, "wait_ns": wait_ns}],
    }


def _slow(t, busy=0, iowait=0, steal=0, total=0, harness=0, audio_daemon=0, other=0, top_other=None):
    return {
        "type": "slow",
        "t": float(t),
        "stat": {"busy": busy, "iowait": iowait, "steal": steal, "total": total},
        "classes": {"harness": harness, "audio_daemon": audio_daemon, "other": other},
        "top_other": top_other or {},
    }


def test_contention_summarize_core_metrics():
    samples = [_fast(i, run_ns=i * 500_000_000, wait_ns=i * 3_000_000, busy=i * 40, iowait=i * 5, steal=i * 1, total=i * 100) for i in range(11)]
    samples += [_slow(i, total=i * 100, harness=i * 5, audio_daemon=i * 2, other=i * 10, top_other={"stress": i * 8, "syncthing": i * 2}) for i in range(0, 11, 2)]
    result = contention.summarize(samples, 0.0, 10.0)
    assert result["busy_pct"] == 40.0
    assert result["iowait_pct"] == 5.0
    assert result["steal_pct"] == 1.0
    assert result["loadavg_1m_max"] == 1.23
    assert result["other_busy_pct_mean"] == 10.0
    assert result["harness_busy_pct"] == 5.0
    assert result["audio_daemon_busy_pct"] == 2.0
    assert result["audio_thread_cpu_pct"] == 50.0
    assert result["audio_wait_ms_max"] == 3.0
    assert result["audio_wait_ms_total"] == 30.0
    assert result["app_wait_ms_max"] == 3.0
    assert result["top_other"][0] == {"comm": "stress", "ticks": 80}
    assert result["contended"] is False


def test_contention_summarize_contended_boundaries():
    def make(other=0.0, steal=0.0, iowait=0.0, wait_ms=0.0):
        total = 1000
        samples = [
            _fast(0),
            _fast(1, wait_ns=int(wait_ms * 1e6), iowait=int(iowait * 10), steal=int(steal * 10), total=total),
            _slow(0),
            _slow(1, total=total, other=int(other * 10)),
        ]
        return contention.summarize(samples, 0.0, 1.0)

    assert make(other=20.0)["contended"] is False  # exactly at the limit never trips it
    r = make(other=20.1)
    assert r["contended"] is True and "other_busy_pct" in r["reasons"][0]
    assert make(steal=2.0)["contended"] is False
    assert make(steal=2.1)["contended"] is True
    assert make(iowait=10.0)["contended"] is False
    assert make(iowait=10.1)["contended"] is True
    assert make(wait_ms=4.9)["contended"] is False
    assert make(wait_ms=5.0)["contended"] is True  # app_wait_ms_max uses >=


# ---------------------------------------------------------------------------
# Task 1 (R5): pre-flight quiet gate
# ---------------------------------------------------------------------------


def test_contention_decide_quiet():
    quiet, streak, lo, hi = contention.decide_quiet([10, 12, 14], 15, 3)
    assert quiet is True and streak == 3

    quiet2, streak2, lo2, hi2 = contention.decide_quiet([10, 20, 12, 14, 13], 15, 3)
    assert quiet2 is True and streak2 == 3  # the 20 resets the streak, then 3 more windows re-qualify
    assert lo2 == 10 and hi2 == 20

    quiet3, streak3, lo3, hi3 = contention.decide_quiet([20, 21, 22], 15, 3)
    assert quiet3 is False and streak3 == 0
    assert lo3 == 20 and hi3 == 22


# ---------------------------------------------------------------------------
# Task 1 (R5): decide_attempts
# ---------------------------------------------------------------------------


def _attempt_metric(result, sched=True, contended=False, host_starved=False, value=1):
    return {"value": value, "result": result, "sched_sensitive": sched, "contended": contended, "host_starved": host_starved}


def test_decide_attempts_all_pass_first_try():
    rows, needs_more = analyze.decide_attempts(
        [{"holes": _attempt_metric("PASS"), "latency_ms": _attempt_metric("PASS", sched=False)}], 3
    )
    assert needs_more is False
    assert all(r["result"] == "PASS" for r in rows)


def test_decide_attempts_deterministic_fail_never_retries():
    rows, needs_more = analyze.decide_attempts([{"exact_repeat_frac": _attempt_metric("FAIL", sched=False)}], 3)
    assert needs_more is False
    assert rows[0]["result"] == "FAIL"


def test_decide_attempts_sched_fail_twice_quiet_stops_early():
    rows, needs_more = analyze.decide_attempts([{"holes": _attempt_metric("FAIL")}], 3)
    assert needs_more is True
    rows, needs_more = analyze.decide_attempts([{"holes": _attempt_metric("FAIL")}, {"holes": _attempt_metric("FAIL")}], 3)
    assert needs_more is False  # majority (2/3) reached without a 3rd attempt
    assert rows[0]["result"] == "FAIL"


def test_decide_attempts_fail_then_pass_pass_is_pass():
    attempts = [{"holes": _attempt_metric("FAIL")}, {"holes": _attempt_metric("PASS")}]
    _rows, needs_more = analyze.decide_attempts(attempts, 3)
    assert needs_more is True
    attempts.append({"holes": _attempt_metric("PASS")})
    rows, needs_more = analyze.decide_attempts(attempts, 3)
    assert needs_more is False
    assert rows[0]["result"] == "PASS"


def test_decide_attempts_mixed_quiet_and_contended_is_inconclusive():
    attempts = [
        {"holes": _attempt_metric("FAIL")},
        {"holes": _attempt_metric("PASS")},
        {"holes": _attempt_metric("FAIL", contended=True)},
    ]
    rows, needs_more = analyze.decide_attempts(attempts, 3)
    assert needs_more is False
    assert rows[0]["result"] == "INCONCLUSIVE"


def test_decide_attempts_only_contended_failures_never_fail():
    rows, _ = analyze.decide_attempts([{"holes": _attempt_metric("FAIL", contended=True)}] * 3, 3)
    assert rows[0]["result"] == "INCONCLUSIVE"
    rows2, _ = analyze.decide_attempts(
        [{"holes": _attempt_metric("FAIL", contended=True)}, {"holes": _attempt_metric("PASS")}, {"holes": _attempt_metric("PASS")}], 3
    )
    assert rows2[0]["result"] == "PASS"


def test_decide_attempts_only_host_starved_holes_never_fail():
    rows, _ = analyze.decide_attempts([{"holes": _attempt_metric("FAIL", host_starved=True)}] * 3, 3)
    assert rows[0]["result"] == "INCONCLUSIVE"


def test_decide_attempts_never_exceeds_max_attempts():
    attempts = [{"holes": _attempt_metric("FAIL")}] * 5
    _rows, needs_more = analyze.decide_attempts(attempts, 3)
    assert needs_more is False


def test_latency_ms_sched_sensitive_only_for_deepfilternet():
    assert analyze.is_sched_sensitive_metric("latency_ms", {"engine": "DeepFilterNet"}) is True
    assert analyze.is_sched_sensitive_metric("latency_ms", {"engine": "RNNoise"}) is False
    assert analyze.is_sched_sensitive_metric("holes", {"engine": "RNNoise"}) is True


def test_lag_corr_and_latency_ms_sched_sensitive_for_baseline_only():
    # quick 260924-n4s: real-world evidence (a contended baseline attempt
    # can make lag_corr genuinely unmeasurable) folded lag_corr/latency_ms
    # into baseline's own majority-of-quiet-attempts retry -- but NOT for
    # a normal steady-state recording elsewhere (swaps pre/post, etc).
    assert analyze.is_sched_sensitive_metric("lag_corr", {"engine": "RNNoise", "scenario": "baseline"}) is True
    assert analyze.is_sched_sensitive_metric("latency_ms", {"engine": "RNNoise", "scenario": "baseline"}) is True
    assert analyze.is_sched_sensitive_metric("lag_corr", {"engine": "RNNoise", "scenario": "swaps"}) is False
    assert analyze.is_sched_sensitive_metric("latency_ms", {"engine": "RNNoise", "scenario": "toggle"}) is False


def test_dead_run_ms_latency_lag_corr_sched_sensitive_for_stress_and_spike_only():
    for scenario in ("stress", "spike"):
        assert analyze.is_sched_sensitive_metric("dead_run_ms", {"scenario": scenario}) is True
        assert analyze.is_sched_sensitive_metric("latency_ms", {"engine": "RNNoise", "scenario": scenario}) is True
        assert analyze.is_sched_sensitive_metric("lag_corr", {"engine": "RNNoise", "scenario": scenario}) is True
    assert analyze.is_sched_sensitive_metric("dead_run_ms", {"scenario": "baseline"}) is False


# ---------------------------------------------------------------------------
# Task 1 (R5): WAV<->epoch mapping + hole attribution
# ---------------------------------------------------------------------------


def test_hole_epoch_mapping_with_synthetic_offset():
    got = analyze.hole_epoch(t_wav=5.0, rec_link_epoch=1000.0, source_onset_s=2.0, input_onset_wav_s=1.0)
    assert got == 1000.0 + 2.0 + (5.0 - 1.0)


def test_attribute_holes_classes():
    fast_samples_starved = [
        {"type": "fast", "t": 1004.9, "threads": [{"tid": 1, "comm": "cleanmic-audio", "run_ns": 0, "wait_ns": 0}]},
        {"type": "fast", "t": 1005.0, "threads": [{"tid": 1, "comm": "cleanmic-audio", "run_ns": 0, "wait_ns": 6_000_000}]},
    ]
    res = analyze.attribute_holes([5.0], 1000.0, 0.0, 0.0, fast_samples_starved, 5.0)
    assert res[0]["class"] == "host_starved"
    assert res[0]["t_epoch"] == 1005.0

    fast_samples_slow = [
        {"type": "fast", "t": 1004.9, "threads": [{"tid": 1, "comm": "cleanmic-audio", "run_ns": 0, "wait_ns": 0}]},
        {"type": "fast", "t": 1005.0, "threads": [{"tid": 1, "comm": "cleanmic-audio", "run_ns": 95_000_000, "wait_ns": 0}]},
    ]
    res2 = analyze.attribute_holes([5.0], 1000.0, 0.0, 0.0, fast_samples_slow, 5.0)
    assert res2[0]["class"] == "engine_slow"

    fast_samples_none = [
        {"type": "fast", "t": 1004.9, "threads": [{"tid": 1, "comm": "cleanmic-audio", "run_ns": 0, "wait_ns": 0}]},
        {"type": "fast", "t": 1005.0, "threads": [{"tid": 1, "comm": "cleanmic-audio", "run_ns": 5_000_000, "wait_ns": 0}]},
    ]
    res3 = analyze.attribute_holes([5.0], 1000.0, 0.0, 0.0, fast_samples_none, 5.0)
    assert res3[0]["class"] == "unexplained"


# ---------------------------------------------------------------------------
# Task 1 (R5): downgrade + report exit codes (0 / 1 / 7)
# ---------------------------------------------------------------------------


def test_downgrade_contended_sched_sensitive_fail_to_inconclusive():
    measured = {
        "kind": "speech", "engine": "DeepFilterNet", "latency_ms": 200, "lag_corr": 0.9, "latency_spread_ms": 2,
        "contention": {"contended": True, "other_busy_pct_mean": 30},
    }
    th = {"latency_max_ms_deepfilternet": 140, "lag_corr_min": 0.5, "latency_drift_max_ms": 15}
    rows = {r.metric: r for r in analyze.evaluate(measured, th)}
    assert rows["latency_ms"].result == "INCONCLUSIVE"


def test_downgrade_all_host_starved_holes_to_inconclusive():
    th = {"latency_max_ms": 120, "lag_corr_min": 0.5, "latency_drift_max_ms": 15, "holes_max": 0}
    measured_all = {
        "kind": "speech", "engine": "RNNoise", "latency_ms": 50, "lag_corr": 0.9, "latency_spread_ms": 2,
        "holes": 3, "hole_classes": ["host_starved", "host_starved", "host_starved"],
    }
    rows = {r.metric: r for r in analyze.evaluate(measured_all, th)}
    assert rows["holes"].result == "INCONCLUSIVE"

    measured_mixed = dict(measured_all, hole_classes=["host_starved", "engine_slow", "host_starved"])
    rows2 = {r.metric: r for r in analyze.evaluate(measured_mixed, th)}
    assert rows2["holes"].result == "FAIL"  # not EVERY failing hole was host_starved


def test_report_exit_codes_include_inconclusive_7():
    th = {"latency_max_ms": 120, "lag_corr_min": 0.5, "latency_drift_max_ms": 15, "holes_max": 0}
    all_starved = {
        "kind": "speech", "engine": "RNNoise", "latency_ms": 50, "lag_corr": 0.9, "latency_spread_ms": 2,
        "holes": 3, "hole_classes": ["host_starved", "host_starved", "host_starved"],
    }
    text, code = analyze.render_report([all_starved], th, {}, None)
    assert code == 7 and "Result: INCONCLUSIVE" in text

    real_fail = dict(all_starved, hole_classes=["host_starved", "engine_slow", "host_starved"])
    _text2, code2 = analyze.render_report([real_fail], th, {}, None)
    assert code2 == 1

    ok = {"kind": "speech", "latency_ms": 50, "lag_corr": 0.9, "latency_spread_ms": 2}
    _text3, code3 = analyze.render_report([ok], {"latency_max_ms": 120, "lag_corr_min": 0.5, "latency_drift_max_ms": 15}, {}, None)
    assert code3 == 0


def test_report_environment_contention_stats():
    measurements = [
        {"kind": "speech", "latency_ms": 50, "lag_corr": 0.9, "latency_spread_ms": 2, "contention": {"contended": False, "other_busy_pct_mean": 5.0}},
        {"kind": "speech", "latency_ms": 50, "lag_corr": 0.9, "latency_spread_ms": 2, "contention": {"contended": True, "other_busy_pct_mean": 25.0}},
    ]
    th = {"latency_max_ms": 120, "lag_corr_min": 0.5, "latency_drift_max_ms": 15}
    max_other, contended_count = analyze.contention_summary_stats(measurements)
    assert max_other == 25.0 and contended_count == 1
    text, _code = analyze.render_report(measurements, th, {"nproc": "8"}, None)
    assert "max_other_busy_pct" in text and "contended_recordings" in text and "| nproc | 8 |" in text


def test_check_cli_accepts_inconclusive_result():
    # Regression (real E2E run, quick 260924-n4s): the preflight_quiet row is
    # written via `analyze.py check --result INCONCLUSIVE`; argparse's
    # --result choices must include it or the row (and the exit-7 verdict)
    # silently never gets recorded.
    with tempfile.TemporaryDirectory() as td:
        out_json = os.path.join(td, "check.json")
        rc = analyze.main(
            [
                "check", "--scenario", "preflight", "--recording", "preflight_quiet",
                "--metric", "preflight_quiet", "--value", "never quiet",
                "--threshold-desc", "quiet within 20s", "--result", "INCONCLUSIVE",
                "--json-out", out_json,
            ]
        )
        assert rc == 0
        with open(out_json, encoding="utf-8") as fh:
            data = json.load(fh)
        assert data["result"] == "INCONCLUSIVE"


def test_aggregate_kind_renders_in_report():
    aggregate = {
        "scenario": "baseline", "recording": "baseline_RNNoise", "kind": "aggregate",
        "attempts_used": 3, "max_attempts": 3,
        "metrics": [{"metric": "holes", "result": "PASS", "sched_sensitive": True, "values": [1, 0, 0], "classes": ["quiet", "quiet", "quiet"]}],
    }
    rows = analyze.evaluate(aggregate, {})
    assert rows[0].metric == "holes" and rows[0].result == "PASS"
    text, code = analyze.render_report([aggregate], {}, {}, None)
    assert code == 0 and "holes" in text


# ---------------------------------------------------------------------------
# Task 3 (R1): spike-check
# ---------------------------------------------------------------------------


def test_spike_check_fallback_fails():
    logscan = {"fallbacks": [{"from": "DeepFilterNet", "to": "RNNoise", "reason": "Overloaded"}], "plugin_aborts": 0}
    rows = {r["metric"]: r for r in analyze.evaluate_spike(logscan, True, "DeepFilterNet")}
    assert rows["fallback_during_spike"]["result"] == "FAIL"


def test_spike_check_fallback_under_contention_is_inconclusive_not_pass():
    # R5: a fallback that only shows up under real ambient contention isn't
    # evidence the grace failed on a clean, isolated burst -- but it's still
    # not a confirmed clean spike either, so INCONCLUSIVE, never PASS.
    logscan = {"fallbacks": [{"from": "Dpdfnet8", "to": "RNNoise", "reason": "Overloaded"}], "plugin_aborts": 0}
    rows = {r["metric"]: r for r in analyze.evaluate_spike(logscan, True, "Dpdfnet8", contended=True)}
    assert rows["fallback_during_spike"]["result"] == "INCONCLUSIVE"
    assert rows["end_engine"]["result"] == "INCONCLUSIVE"
    assert "contended" in rows["fallback_during_spike"]["note"]
    # Not contended: the fallback stays a real FAIL.
    rows_quiet = {r["metric"]: r for r in analyze.evaluate_spike(logscan, True, "Dpdfnet8", contended=False)}
    assert rows_quiet["fallback_during_spike"]["result"] == "FAIL"
    assert rows_quiet["end_engine"]["result"] == "FAIL"


def test_spike_check_no_fallback_alive_end_engine_matches_passes():
    logscan = {"fallbacks": [], "plugin_aborts": 0}
    rows = {r["metric"]: r for r in analyze.evaluate_spike(logscan, True, "Dpdfnet8")}
    assert rows["fallback_during_spike"]["result"] == "PASS"
    assert rows["app_alive"]["result"] == "PASS"
    assert rows["plugin_abort_lines"]["result"] == "PASS"
    assert rows["end_engine"]["result"] == "PASS" and rows["end_engine"]["value"] == "Dpdfnet8"


def test_spike_check_dead_app_and_abort_fail():
    logscan = {"fallbacks": [], "plugin_aborts": 1}
    rows = {r["metric"]: r for r in analyze.evaluate_spike(logscan, False, "Dpdfnet8")}
    assert rows["app_alive"]["result"] == "FAIL"
    assert rows["plugin_abort_lines"]["result"] == "FAIL"


# ---------------------------------------------------------------------------
# Task 3 (R3): swap_attribution
# ---------------------------------------------------------------------------


def test_epoch_to_wav_is_the_inverse_of_hole_epoch():
    epoch = analyze.hole_epoch(t_wav=5.0, rec_link_epoch=1000.0, source_onset_s=2.0, input_onset_wav_s=0.0)
    assert abs(analyze.epoch_to_wav(epoch, 1000.0, 2.0) - 5.0) < 1e-9


def test_swap_attribution_classes():
    fs = 48000
    # leading run at sample 0, 100ms
    leading = (0, int(0.1 * fs))
    # a swap-adjacent run at t=10.0s, within the 0.6s window of a swap at 9.8s
    swap_run = (int(10.0 * fs), 480)
    # a run at t=20.0s, within 150ms of a dfn restart at 19.95s, <=480 samples
    dfn_run = (int(20.0 * fs), 480)
    # a run at t=30.0s, within 150ms of a dfn shed at 30.05s
    shed_run = (int(30.0 * fs), 480)
    # a run at t=50.0s with no nearby evidence
    mystery_run = (int(50.0 * fs), 480)

    zr = [leading, swap_run, dfn_run, shed_run, mystery_run]
    swap_events = [(9.8, "RNNoise", "MaxQuality")]
    dfn_restart_t_wav = [19.95]
    dfn_shed_t_wav = [30.05]

    rows = analyze.swap_attribution(zr, fs, swap_events, dfn_restart_t_wav, dfn_shed_t_wav)
    by_t = {r["t"]: r for r in rows}
    assert by_t[0.0]["class"] == "leading"
    assert by_t[10.0]["class"] == "swap" and by_t[10.0]["engine"] == "RNNoise"
    assert by_t[20.0]["class"] == "dfn_underrun"
    assert by_t[30.0]["class"] == "dfn_shed"
    assert by_t[50.0]["class"] == "unattributed"

    summary = analyze.swap_attribution_summary(rows)
    assert summary["swap_class_ms"] == 10.0  # 480 samples @ 48kHz == 10ms


def test_swap_attribution_unattributed_gets_task1_host_evidence():
    fs = 48000
    mystery_run = (int(50.0 * fs), 480)
    far_run = (int(80.0 * fs), 480)
    hole_attribution = [
        {"t_wav": 50.02, "t_epoch": 0.0, "class": "host_starved"},
        {"t_wav": 10.0, "t_epoch": 0.0, "class": "engine_slow"},
    ]
    rows = analyze.swap_attribution([mystery_run, far_run], fs, [], [], [], hole_attribution=hole_attribution)
    by_t = {r["t"]: r for r in rows}
    assert by_t[50.0]["class"] == "unattributed"
    assert by_t[50.0]["host_evidence"] == "host_starved"  # nearest hole, within the match window
    assert by_t[80.0]["host_evidence"] is None  # nearest hole (10.0s) is far outside the match window


def test_swap_attribution_summary_totals_by_class():
    rows = [
        {"class": "swap", "ms": 10.0},
        {"class": "swap", "ms": 5.0},
        {"class": "dfn_underrun", "ms": 10.0},
        {"class": "unattributed", "ms": 3.0},
    ]
    summary = analyze.swap_attribution_summary(rows)
    assert summary["swap_class_ms"] == 15.0
    assert summary["by_class_ms"]["dfn_underrun"] == 10.0
    assert summary["by_class_ms"]["unattributed"] == 3.0


def test_logscan_captures_new_d01_and_swap_log_lines():
    log_text = (
        "[2026-09-24T15:24:07.000Z WARN  cleanmic::audio] Audio thread: active engine (#1) reported "
        "Overloaded after 5.0 s of sustained trouble — asking the app for a lighter engine\n"
        "[2026-09-24T15:24:08.000Z WARN  cleanmic::engine::deepfilter] DeepFilterNet: bypassing the plugin "
        "(overloaded) — passing audio through, retry in 1.0 s (restarts 1/10)\n"
        "[2026-09-24T15:24:09.000Z INFO  cleanmic::engine::deepfilter] DeepFilterNet: plugin back to real time "
        "— processing resumed (restart 1/10)\n"
        "[2026-09-24T15:24:10.000Z INFO  cleanmic::engine::deepfilter] DeepFilterNet: shed 30 ms of accumulated "
        "plugin latency (one-time restart 2/10)\n"
        "[2026-09-24T15:24:11.000Z INFO  cleanmic::audio] Engine swap started (crossfading)\n"
        "[2026-09-24T15:24:12.000Z INFO  cleanmic::audio] Engine mode set to LowCpu\n"
    )
    r = analyze.logscan(log_text)
    assert r["trouble_span_s"] == [5.0]
    assert r["dfn_bypass_entries"] == 1
    assert r["dfn_recoveries"] == 1
    assert len(r["dfn_shed_at"]) == 1 and r["dfn_shed_at"][0][1] == 30
    assert len(r["swap_started_at"]) == 1
    assert r["mode_set_at"] == [(r["mode_set_at"][0][0], "LowCpu")]


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
