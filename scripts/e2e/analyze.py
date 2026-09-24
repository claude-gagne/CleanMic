#!/usr/bin/env python3
"""analyze.py -- WAV reader, latency/quality metrics and Markdown report
generator for the CleanMic silent E2E harness (scripts/e2e-audio.sh).

Pure, stdlib+numpy functions so they are unit-testable without PipeWire, a
display or any recorded audio (see test_analyze.py). The `measure` and
`report` CLI subcommands are the glue scripts/e2e-audio.sh drives.

USAGE
  analyze.py measure REC.wav --scenario S --recording R --kind K \
      [--meta k=v]... --json-out OUT.json
  analyze.py stress-check --logscan L.json --app-alive yes|no --load-start EPOCH \
      --recovery-max-s N --scenario S --recording R --json-out OUT.json
  analyze.py active-engine --logscan L.json --started-with ENGINE
  analyze.py report --out report.md [--threshold k=v]... [--meta k=v]... \
      [--aborted REASON] MEASURED.json...

EXIT CODES (report / measure)
  0  ok (report: every metric PASSed)
  1  report: at least one metric FAILed
  2  bad input (unreadable WAV / JSON, bad CLI usage)
"""

from __future__ import annotations

import argparse
import datetime
import json
import re
import struct
import sys
from collections import namedtuple
from typing import Any

import numpy as np

# ---------------------------------------------------------------------------
# WAV reading
# ---------------------------------------------------------------------------


def read_wav(path: str) -> tuple[np.ndarray, int]:
    """Read a WAV file into (float64 array shape (frames, channels), rate).

    Walks RIFF chunks by hand (no `wave` module) because an interrupted
    `pw-record` leaves a data-chunk size of 0 or 0xFFFFFFFF, which the
    standard library's reader rejects; a size that overruns the file is
    treated the same way -- both mean "read to end of file". Supports
    16-bit PCM and 32-bit IEEE float / WAVE_FORMAT_EXTENSIBLE float.
    """
    with open(path, "rb") as fh:
        blob = fh.read()
    if len(blob) < 12 or blob[0:4] != b"RIFF" or blob[8:12] != b"WAVE":
        raise ValueError(f"not a RIFF/WAVE file: {path}")

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
            if size in (0, 0xFFFFFFFF) or body_start + size > n:
                data = blob[body_start:]
            else:
                data = blob[body_start : body_start + size]
            break
        i = body_start + size + (size & 1)

    if fmt is None or data is None:
        raise ValueError(f"missing fmt/data chunk: {path}")

    audio_format, channels, rate, _byte_rate, _block_align, bits = fmt
    channels = max(1, channels)

    if bits == 32 or audio_format in (3, 0xFFFE):
        frame_bytes = 4 * channels
        usable = (len(data) // frame_bytes) * frame_bytes
        x = np.frombuffer(data[:usable], dtype="<f4").astype(np.float64)
    elif bits == 16:
        frame_bytes = 2 * channels
        usable = (len(data) // frame_bytes) * frame_bytes
        x = np.frombuffer(data[:usable], dtype="<i2").astype(np.float64) / 32768.0
    else:
        raise ValueError(f"unsupported bits-per-sample {bits}: {path}")

    return x.reshape(-1, channels), rate


# ---------------------------------------------------------------------------
# Core metric primitives
# ---------------------------------------------------------------------------


def rms_db(x: np.ndarray) -> float:
    return float(20 * np.log10(np.sqrt(np.mean(np.square(x, dtype=np.float64))) + 1e-12))


def active_region(inp: np.ndarray) -> tuple[int, int]:
    """First and last sample index (as a [start, end) slice) with |x| > 1e-6."""
    nz = np.where(np.abs(inp) > 1e-6)[0]
    if len(nz) == 0:
        return 0, len(inp)
    return int(nz[0]), int(nz[-1]) + 1


def log_envelope(x: np.ndarray, fs: int) -> np.ndarray:
    """FFT band-pass (200 Hz high-pass, 40 Hz low-pass on the rectified
    signal) envelope, decimated by 48 (48 kHz -> 1 kHz), then ln(max(e, 1e-6))."""
    n = len(x)
    freqs = np.fft.rfftfreq(n, 1.0 / fs)
    X = np.fft.rfft(x)
    X[freqs < 200] = 0
    y = np.abs(np.fft.irfft(X, n))
    Y = np.fft.rfft(y)
    Y[freqs > 40] = 0
    e = np.fft.irfft(Y, n)[::48]
    return np.log(np.maximum(e, 1e-6))


def xcorr_lag(a: np.ndarray, b: np.ndarray, max_lag: int) -> tuple[int, float]:
    """Mean-removed FFT cross-correlation. Returns (lag, corr): lag is in
    envelope samples (== ms, since log_envelope decimates to 1 kHz);
    positive means b is later than a."""
    a = a - a.mean()
    b = b - b.mean()
    n = len(a)
    if n == 0:
        return 0, 0.0
    A = np.fft.rfft(a, 2 * n)
    B = np.fft.rfft(b, 2 * n)
    c = np.fft.irfft(np.conj(A) * B)
    c = np.concatenate([c[-max_lag:], c[: max_lag + 1]])
    lags = np.arange(-max_lag, max_lag + 1)
    k = int(np.argmax(c))
    denom = np.linalg.norm(a) * np.linalg.norm(b) + 1e-12
    return int(lags[k]), float(c[k] / denom)


def measure_latency(inp: np.ndarray, out: np.ndarray, fs: int):
    """Global lag/corr (max_lag=2000) over the active region, plus a list of
    5 s window (lag, corr) pairs (max_lag=1600 each)."""
    start, end = active_region(inp)
    i = inp[start:end]
    o = out[start:end]
    ei = log_envelope(i, fs)
    eo = log_envelope(o, fs)
    lag, corr = xcorr_lag(ei, eo, 2000)
    window = 5000  # envelope samples == ms; 5000 == 5 s at the 1 kHz envelope rate
    window_lags: list[tuple[int, float]] = []
    n = len(ei)
    for s in range(0, max(n - window + 1, 0), window):
        wl, wc = xcorr_lag(ei[s : s + window], eo[s : s + window], 1600)
        window_lags.append((wl, round(wc, 3)))
    return lag, corr, window_lags


def latency_spread_ms(window_lags: list[tuple[int, float]]) -> float:
    qualifying = [lag for lag, corr in window_lags if corr >= 0.5]
    if not qualifying:
        return 0.0
    return float(max(qualifying) - min(qualifying))


# ---------------------------------------------------------------------------
# Task 2 metrics: repeats, zero runs, holes, DC, settled gain, log scanning.
# ---------------------------------------------------------------------------


def exact_repeat_frac(o: np.ndarray, hop: int = 480, thr: float = 1e-2) -> tuple[float, int]:
    """Fraction of hop-aligned hops that are near-exact copies of the
    previous hop (a decimated-mode "held frame" bug signature). Mirrors the
    debug session's `ana.py::repeat_frac`: for every hop OFFSET (so the
    comparison isn't blind to a fixed alignment), compare hop t against hop
    t-hop via cumulative sums; only consider hops with rms > 1e-4 and skip
    the first 50 (warm-up). Returns (best fraction, hops evaluated at that
    offset)."""
    o = np.asarray(o, dtype=np.float64)
    if len(o) <= hop:
        return 0.0, 0
    d = np.concatenate([[0], np.cumsum((o[hop:] - o[:-hop]) ** 2)])
    e = np.concatenate([[0], np.cumsum(o[hop:] ** 2)])
    n = len(o) - hop
    m = n // hop
    best_frac = 0.0
    best_evaluated = 0
    for off in range(hop):
        ks = off + np.arange(max(m - 1, 0)) * hop
        ks = ks[ks + hop <= n]
        if len(ks) == 0:
            continue
        dd = d[ks + hop] - d[ks]
        ee = e[ks + hop] - e[ks]
        rms = np.sqrt(ee / hop)
        ok = rms > 1e-4
        ok[:50] = False
        evaluated = int(ok.sum())
        repeats = (np.sqrt(np.maximum(dd, 0)) / np.sqrt(np.maximum(ee, 1e-30)) < thr) & ok
        frac = float(repeats.sum() / evaluated) if evaluated else 0.0
        if frac > best_frac or best_evaluated == 0:
            best_frac = frac
            best_evaluated = evaluated
    return best_frac, best_evaluated


def zero_runs(o: np.ndarray, min_len: int = 240) -> list[tuple[int, int]]:
    """Runs of exact (|x| < 1e-9) zero at least `min_len` samples long (5 ms
    at 48 kHz by default). Returns a list of (start_index, length)."""
    z = (np.abs(o) < 1e-9).astype(np.int8)
    d = np.diff(np.concatenate([[0], z, [0]]))
    starts = np.where(d == 1)[0]
    ends = np.where(d == -1)[0]
    lengths = ends - starts
    return [(int(s), int(length)) for s, length in zip(starts, lengths) if length >= min_len]


def holes(o: np.ndarray, fs: int = 48000, frame_ms: float = 10.0) -> int:
    """Count of `frame_ms` frames whose level drops below -90 dB while the
    frames 3 positions before AND after it are both above -50 dB -- a brief
    silent gap surrounded by loud audio, distinct from natural silence."""
    frame = int(fs * frame_ms / 1000)
    if frame <= 0 or len(o) < frame:
        return 0
    m = len(o) // frame
    framed = o[: m * frame].reshape(m, frame)
    levels = 20 * np.log10(np.sqrt(np.mean(framed**2, axis=1)) + 1e-12)
    count = 0
    for k in range(3, m - 3):
        if levels[k] < -90 and levels[k - 3] > -50 and levels[k + 3] > -50:
            count += 1
    return count


def dead_speech(inp: np.ndarray, out: np.ndarray, fs: int = 48000, frame_ms: float = 10.0) -> tuple[float, float]:
    """(longest_run_ms, total_ms) of "dead output": 10 ms frames where the
    INPUT carries speech-level signal (> -35 dBFS) but the output is digital
    silence (< -100 dBFS). Every observed failure is exact zeros -- the
    dfn-panic-under-load crash (process died -> the virtual source vanished
    -> 15 s of zeros), the plugin's inserted 10 ms blocks, output-ring
    underruns -- while legitimate suppression bottoms out far higher: in the
    first stress run RNNoise took loud noise-only stretches down to -70..-79
    dBFS and a fresh RNNoise's onset frames to -89 dBFS, so -70 dBFS would
    have flagged real denoising as "dead". Frames with a quiet input neither
    extend nor break a run (a dead engine stays dead through speech pauses).
    `inp` and `out` must already be scoped to the same (active) region."""
    frame = int(fs * frame_ms / 1000)
    m = min(len(inp), len(out)) // frame
    if frame <= 0 or m == 0:
        return 0.0, 0.0
    lvl = lambda x: 20 * np.log10(np.sqrt(np.mean(x[: m * frame].reshape(m, frame) ** 2, axis=1)) + 1e-12)  # noqa: E731
    in_db, out_db = lvl(inp), lvl(out)
    run = longest = total = 0
    for k in range(m):
        if in_db[k] <= -35:
            continue
        if out_db[k] < -100:
            run += 1
            total += 1
            longest = max(longest, run)
        else:
            run = 0
    return longest * frame_ms, total * frame_ms


def dc_offset(x: np.ndarray, fs: int = 48000) -> float:
    """Mean over the active region, excluding its first 1 s (onset
    transients / DC-blocker settling should not bias the measurement)."""
    start, end = active_region(x)
    skip_to = min(start + fs, end)
    region = x[skip_to:end]
    if len(region) == 0:
        region = x[start:end]
    if len(region) == 0:
        return 0.0
    return float(np.mean(region))


def rms_curve(x: np.ndarray, fs: int, window_s: float = 0.5) -> list[float]:
    win = int(fs * window_s)
    if win <= 0 or len(x) < win:
        return []
    n = len(x) // win
    framed = x[: n * win].reshape(n, win)
    return [round(float(v), 1) for v in 20 * np.log10(np.sqrt(np.mean(framed**2, axis=1)) + 1e-12)]


def settled_gain_db(inp: np.ndarray, out: np.ndarray) -> float:
    """out - in RMS(dB) over the LAST 50% of the input's active region --
    the gain once any onset ramp (e.g. auto-gain attack) has settled."""
    start, end = active_region(inp)
    mid = start + (end - start) // 2
    return rms_db(out[mid:end]) - rms_db(inp[mid:end])


_LOG_ENGINE_CHANGED_RE = re.compile(r"Engine changed to (\w+) \(mode=(\w+)\)")
_LOG_MODE_CHANGED_RE = re.compile(r"Mode changed to (\w+)")
_LOG_FELL_BEHIND_RE = re.compile(r"fell \d+ ms behind")
_LOG_DISCARDED_RE = re.compile(r"Discarded \d+ ms")
_LOG_ERROR_RE = re.compile(r" ERROR ")
_LOG_PANIC_RE = re.compile(r"panicked")
_LOG_WARN_RE = re.compile(r".*\bWARN\b.*")
_LOG_UNDERRUN_RE = re.compile(r"underrun|xrun", re.IGNORECASE)
# dfn-panic-under-load: the runtime engine fallback (src/app.rs), the vendored
# DeepFilterNet plugin's own abort message (must never appear again), and the
# DeepFilterNet underrun guard's restart / give-up lines (src/engine/deepfilter.rs).
_LOG_FALLBACK_RE = re.compile(r"^\[(\S+) [A-Z]+ +[^\]]*\] Engine fallback: (\w+) -> (\w+) \((\w+)\)", re.MULTILINE)
_LOG_PLUGIN_ABORT_RE = re.compile(r"Processing too slow! Please upgrade your CPU")
_LOG_DFN_RESTART_RE = re.compile(r"DeepFilterNet: plugin fell behind real time")
_LOG_DFN_GAVE_UP_RE = re.compile(r"DeepFilterNet cannot keep up with real time")


def logscan(log_text: str) -> dict[str, Any]:
    """Scan an app.log's text for the counts/sequences scripts/e2e-audio.sh's
    scenarios need: fell-behind/error/panic/Discarded counts, the ordered
    engine-swap and mode-change sequences, the top 10 unique WARN lines
    (informational), and an underrun/xrun mention count (informational)."""
    warn_lines = _LOG_WARN_RE.findall(log_text)
    unique_warns: list[str] = []
    seen: set[str] = set()
    for w in warn_lines:
        if w not in seen:
            seen.add(w)
            unique_warns.append(w)
        if len(unique_warns) >= 10:
            break
    return {
        "fell_behind": len(_LOG_FELL_BEHIND_RE.findall(log_text)),
        "errors": len(_LOG_ERROR_RE.findall(log_text)),
        "panics": len(_LOG_PANIC_RE.findall(log_text)),
        "discarded": len(_LOG_DISCARDED_RE.findall(log_text)),
        "engine_changed": [tuple(m) for m in _LOG_ENGINE_CHANGED_RE.findall(log_text)],
        "mode_changed": _LOG_MODE_CHANGED_RE.findall(log_text),
        "warn_top10": unique_warns,
        "underrun_mentions": len(_LOG_UNDERRUN_RE.findall(log_text)),
        "fallbacks": [
            {"t": _log_epoch(ts), "from": a, "to": b, "reason": why}
            for ts, a, b, why in _LOG_FALLBACK_RE.findall(log_text)
        ],
        "plugin_aborts": len(_LOG_PLUGIN_ABORT_RE.findall(log_text)),
        "dfn_restarts": len(_LOG_DFN_RESTART_RE.findall(log_text)),
        "dfn_gave_up": len(_LOG_DFN_GAVE_UP_RE.findall(log_text)),
    }


def _log_epoch(ts: str) -> float | None:
    """env_logger's `2026-09-24T15:24:07.616Z` -> Unix seconds (None if unparsable)."""
    try:
        return datetime.datetime.strptime(ts, "%Y-%m-%dT%H:%M:%S.%fZ").replace(tzinfo=datetime.timezone.utc).timestamp()
    except ValueError:
        return None


def evaluate_stress(
    logscan_result: dict[str, Any],
    app_alive: bool,
    load_start_epoch: float,
    recovery_max_s: float,
) -> list[dict[str, Any]]:
    """Log/process-level verdicts for one `stress` run (the recording-level
    dead-output verdict is the `stress_load` measure rule). Returns check
    dicts (kind=check) for the report."""
    rows: list[dict[str, Any]] = []

    def row(metric: str, value: Any, desc: str, result: str, note: str = "") -> None:
        rows.append({"kind": "check", "metric": metric, "value": value, "threshold_desc": desc, "result": result, "note": note})

    row("app_alive", "yes" if app_alive else "DIED", "== yes", "PASS" if app_alive else "FAIL")
    aborts = logscan_result.get("plugin_aborts", 0)
    row("plugin_abort_lines", aborts, "== 0", "PASS" if aborts == 0 else "FAIL", "vendored DeepFilterNet 'Processing too slow!' panic")
    fallbacks = logscan_result.get("fallbacks", [])
    if fallbacks:
        first = fallbacks[0]
        t = first.get("t")
        if t is None:
            row("recovery_s", "unparsable", f"<= {recovery_max_s}", "FAIL")
        else:
            note = f"{first['from']} -> {first['to']} ({first['reason']})"
            if t < load_start_epoch:
                # The machine was already too busy for the engine before the
                # synthetic load began (e.g. someone else's build): recovery
                # happened even earlier.
                note += "; before the synthetic load started (machine already overloaded)"
            rec = round(max(0.0, t - load_start_epoch), 2)
            row("recovery_s", rec, f"<= {recovery_max_s}", "PASS" if rec <= recovery_max_s else "FAIL", note)
    else:
        row("recovery_s", "no fallback", f"<= {recovery_max_s}", "INFO", "engine kept up (no runtime fallback logged)")
    row("dfn_restarts", logscan_result.get("dfn_restarts", 0), "(informational)", "INFO")
    return rows


def active_engine_after(logscan_result: dict[str, Any], started_with: str) -> str:
    """The engine running at the end of a log: the last runtime fallback's
    target, else `started_with`."""
    fallbacks = logscan_result.get("fallbacks", [])
    return fallbacks[-1]["to"] if fallbacks else started_with


def evaluate_swap_sequence(expected: list[tuple[str, str]], logged: list[tuple[str, str]]) -> tuple[str, str]:
    """Compare the expected (engine, mode) sequence against the logged one.
    PASS on an exact match; FAIL naming the first differing index otherwise."""
    if list(logged) == list(expected):
        return "PASS", ""
    for i, exp in enumerate(expected):
        got = logged[i] if i < len(logged) else None
        if got != exp:
            return "FAIL", f"index {i}: expected {exp}, got {got}"
    return "FAIL", f"logged has {len(logged)} entries, expected {len(expected)}"


def eval_dc_within(value: float, max_abs: float) -> str:
    return "PASS" if abs(value) <= max_abs else "FAIL"


def eval_autogain_boost(settled_db: float, min_boost_db: float) -> str:
    return "PASS" if settled_db >= min_boost_db else "FAIL"


def eval_autogain_off_deviation(settled_db: float, max_dev_db: float) -> str:
    return "PASS" if abs(settled_db) <= max_dev_db else "FAIL"


def eval_autogain_noise_diff(on_db: float, off_db: float, max_diff_db: float) -> str:
    return "PASS" if abs(on_db - off_db) <= max_diff_db else "FAIL"


# ---------------------------------------------------------------------------
# `measure` CLI
# ---------------------------------------------------------------------------


def _parse_kv_list(pairs: list[str]) -> dict[str, str]:
    out: dict[str, str] = {}
    for pair in pairs:
        if "=" not in pair:
            raise ValueError(f"expected KEY=VALUE, got {pair!r}")
        k, v = pair.split("=", 1)
        out[k] = v
    return out


def _coerce_threshold_value(v: str):
    try:
        if "." in v:
            return float(v)
        return int(v)
    except ValueError:
        return v


def measure_recording(path: str, scenario: str, recording: str, kind: str, meta: dict[str, str]) -> dict[str, Any]:
    x, fs = read_wav(path)
    if x.shape[1] < 2:
        raise ValueError(f"expected a 2-channel recording (input, output): {path}")
    inp = x[:, 0]
    out = x[:, 1]
    start, end = active_region(inp)
    lag, corr, window_lags = measure_latency(inp, out, fs)
    # Scoped to the INPUT's active region, not the whole recording: a
    # record_pair driver (e.g. the swaps scenario's 15-swap sequence) can
    # keep the recorder running well past the point where the source WAV
    # (and therefore the mirrored `cmtest_mic` input) has gone silent, and a
    # multi-second trailing silence is neither a "held frame" repeat bug nor
    # a playback gap -- it's just the recorder still running. Without this,
    # a single ~70 s tail of exact zero was scored as one giant zero_run.
    out_active = out[start:end]
    repeat_frac, evaluated_hops = exact_repeat_frac(out_active.astype(np.float32))
    zr = zero_runs(out_active)
    result: dict[str, Any] = {
        "scenario": scenario,
        "recording": recording,
        "kind": kind,
        "duration_s": round(len(inp) / fs, 3),
        "active_s": round((end - start) / fs, 3),
        "in_rms_db": round(rms_db(inp), 2),
        "out_rms_db": round(rms_db(out), 2),
        "in_peak": round(float(np.max(np.abs(inp))) if len(inp) else 0.0, 4),
        "out_peak": round(float(np.max(np.abs(out))) if len(out) else 0.0, 4),
        "latency_ms": lag,
        "lag_corr": round(corr, 3),
        "window_lags": window_lags,
        "latency_spread_ms": round(latency_spread_ms(window_lags), 2),
        "exact_repeat_frac": round(repeat_frac, 4),
        "evaluated_hops": evaluated_hops,
        "holes": holes(out_active, fs),
        "dead_run_ms": dead_speech(inp[start:end], out_active, fs)[0],
        "dead_ms": dead_speech(inp[start:end], out_active, fs)[1],
        "zero_runs": len(zr),
        "zero_run_ms": round(sum(length for _, length in zr) / fs * 1000, 1),
        "in_dc": round(dc_offset(inp, fs), 6),
        "out_dc": round(dc_offset(out, fs), 6),
        "settled_gain_db": round(settled_gain_db(inp, out), 2),
        "in_curve": rms_curve(inp, fs),
        "out_curve": rms_curve(out, fs),
    }
    result.update(meta)
    return result


def cmd_measure(args: argparse.Namespace) -> int:
    meta = _parse_kv_list(args.meta or [])
    result = measure_recording(args.wav, args.scenario, args.recording, args.kind, meta)
    with open(args.json_out, "w", encoding="utf-8") as fh:
        json.dump(result, fh, indent=2)
    print(f"analyze: measured {args.recording} -> {args.json_out} (latency_ms={result['latency_ms']})")
    return 0


# ---------------------------------------------------------------------------
# `report` CLI: rule registry + Markdown renderer
# ---------------------------------------------------------------------------

Row = namedtuple("Row", "metric value threshold result note")


def _latency_limit_ms(measured: dict[str, Any], thresholds: dict[str, Any]) -> float:
    """Per-engine (or, for the monitor path, per-scenario) MaxQuality
    latency ceiling, plus the decimated-mode allowance for Balanced/LowCpu."""
    kind = measured.get("kind", "speech")
    mode = str(measured.get("mode", ""))
    if kind == "monitor_path":
        base = thresholds.get("monitor_latency_max_ms", thresholds.get("latency_max_ms", 80))
    else:
        engine = str(measured.get("engine", ""))
        base_key = f"latency_max_ms_{engine.lower()}"
        base = thresholds.get(base_key, thresholds.get("latency_max_ms", 80))
    extra = 0.0
    if mode == "Balanced":
        extra = thresholds.get("latency_extra_balanced_ms", 0)
    elif mode == "LowCpu":
        extra = thresholds.get("latency_extra_lowcpu_ms", 0)
    return base + extra


def eval_speech(measured: dict[str, Any], thresholds: dict[str, Any]) -> list[Row]:
    """The "speech" (and "monitor_path") kind's rules: latency, lag_corr,
    latency_spread_ms, plus repeat/holes/peak whenever their thresholds are
    supplied (Task 1 only ever supplies latency-related thresholds, so those
    extra rows are silently absent from a Task-1-only run's report)."""
    limit = _latency_limit_ms(measured, thresholds)
    corr_min = thresholds.get("lag_corr_min", 0.5)
    lag_corr = measured.get("lag_corr")
    latency_ms = measured.get("latency_ms")

    rows: list[Row] = []
    unmeasurable = lag_corr is not None and lag_corr < corr_min
    if unmeasurable:
        rows.append(Row("latency_ms", latency_ms, f"<= {limit}", "FAIL", "latency unmeasurable"))
    else:
        ok = latency_ms is not None and latency_ms <= limit
        rows.append(Row("latency_ms", latency_ms, f"<= {limit}", "PASS" if ok else "FAIL", ""))
    corr_ok = lag_corr is not None and lag_corr >= corr_min
    rows.append(Row("lag_corr", lag_corr, f">= {corr_min}", "PASS" if corr_ok else "FAIL", ""))

    spread_max = thresholds.get("latency_drift_max_ms", 15)
    spread = measured.get("latency_spread_ms")
    spread_ok = spread is not None and spread <= spread_max
    rows.append(Row("latency_spread_ms", spread, f"<= {spread_max}", "PASS" if spread_ok else "FAIL", ""))

    peak_max = thresholds.get("peak_max")
    if peak_max is not None:
        peak = measured.get("out_peak")
        peak_ok = peak is not None and peak <= peak_max
        rows.append(Row("out_peak", peak, f"<= {peak_max}", "PASS" if peak_ok else "FAIL", ""))

    repeat_max = thresholds.get("repeat_frac_max")
    if repeat_max is not None:
        repeat = measured.get("exact_repeat_frac")
        repeat_ok = repeat is not None and repeat <= repeat_max
        rows.append(Row("exact_repeat_frac", repeat, f"<= {repeat_max}", "PASS" if repeat_ok else "FAIL", ""))

    # Per-engine override (holes_max_<engine>), e.g. DeepFilterNet's plugin
    # inserts one 10 ms gap per underrun by design.
    engine_key = f"holes_max_{str(measured.get('engine', '')).lower()}"
    holes_max = thresholds.get(engine_key, thresholds.get("holes_max"))
    if holes_max is not None:
        h = measured.get("holes")
        holes_ok = h is not None and h <= holes_max
        rows.append(Row("holes", h, f"<= {holes_max}", "PASS" if holes_ok else "FAIL", ""))

    dead_max = thresholds.get("dead_run_max_ms")
    if dead_max is not None and "dead_run_ms" in measured:
        d = measured.get("dead_run_ms")
        dead_ok = d is not None and d <= dead_max
        rows.append(Row("dead_run_ms", d, f"<= {dead_max}", "PASS" if dead_ok else "FAIL", ""))

    return rows


def eval_dc_speech(measured: dict[str, Any], thresholds: dict[str, Any]) -> list[Row]:
    """dc_speech kind: the full speech rule set, plus |out_dc| <= OUT_DC_MAX."""
    rows = eval_speech(measured, thresholds)
    dc_max = thresholds.get("out_dc_max", 0.001)
    out_dc = measured.get("out_dc")
    ok = out_dc is not None and abs(out_dc) <= dc_max
    rows.append(Row("out_dc", out_dc, f"|x| <= {dc_max}", "PASS" if ok else "FAIL", ""))
    return rows


def eval_dc_silence(measured: dict[str, Any], thresholds: dict[str, Any]) -> list[Row]:
    """dc_silence kind: no meaningful latency signal (it's DC + silence), so
    just |out_dc| <= OUT_DC_MAX and out_rms_db <= SILENCE_OUT_MAX_DB."""
    dc_max = thresholds.get("out_dc_max", 0.001)
    out_dc = measured.get("out_dc")
    ok = out_dc is not None and abs(out_dc) <= dc_max
    rows = [Row("out_dc", out_dc, f"|x| <= {dc_max}", "PASS" if ok else "FAIL", "")]
    silence_max_db = thresholds.get("silence_out_max_db", -60)
    out_rms = measured.get("out_rms_db")
    ok2 = out_rms is not None and out_rms <= silence_max_db
    rows.append(Row("out_rms_db", out_rms, f"<= {silence_max_db}", "PASS" if ok2 else "FAIL", ""))
    return rows


def eval_ag_on(measured: dict[str, Any], thresholds: dict[str, Any]) -> list[Row]:
    """autogain-ON recording (e.g. ag_m40_on): must boost quiet speech by at
    least AUTOGAIN_MIN_BOOST_DB, without clipping."""
    min_boost = thresholds.get("autogain_min_boost_db", 10)
    gain = measured.get("settled_gain_db")
    ok = gain is not None and gain >= min_boost
    rows = [Row("settled_gain_db", gain, f">= {min_boost}", "PASS" if ok else "FAIL", "")]
    peak_max = thresholds.get("peak_max")
    if peak_max is not None:
        peak = measured.get("out_peak")
        ok2 = peak is not None and peak <= peak_max
        rows.append(Row("out_peak", peak, f"<= {peak_max}", "PASS" if ok2 else "FAIL", ""))
    return rows


def eval_ag_off(measured: dict[str, Any], thresholds: dict[str, Any]) -> list[Row]:
    """autogain-OFF recording: near-unity gain (no boost applied)."""
    max_dev = thresholds.get("autogain_off_max_dev_db", 3)
    gain = measured.get("settled_gain_db")
    ok = gain is not None and abs(gain) <= max_dev
    return [Row("settled_gain_db", gain, f"|x| <= {max_dev}", "PASS" if ok else "FAIL", "")]


def eval_ag_pink(measured: dict[str, Any], thresholds: dict[str, Any]) -> list[Row]:
    """autogain pink-noise recording: informational per-recording (the ON
    vs OFF noise-floor comparison itself is a scenario-level `check`, since
    it needs both recordings at once -- see `diff-check`)."""
    return [Row("out_rms_db", measured.get("out_rms_db"), "(compared at scenario level)", "INFO", "")]


def eval_swaps_during(measured: dict[str, Any], thresholds: dict[str, Any]) -> list[Row]:
    """The recording taken WHILE 15 live engine swaps + 3 mode changes are
    happening. Unlike a steady "speech" recording, latency_ms/lag_corr/
    latency_spread_ms are structurally meaningless here (each swap's brief
    crossfade breaks the single fixed-lag correlation the envelope xcorr
    assumes), and `holes` is EXPECTED (one brief gap per swap) rather than a
    defect -- both are covered instead by e2e-audio.sh's own swap-count-
    scaled `zero_run_ms`/`holes` budget `check`s. Only the decimated-mode
    "held frame" repeat-bug signature is a real invariant here."""
    repeat_max = thresholds.get("repeat_frac_max")
    if repeat_max is None:
        return []
    repeat = measured.get("exact_repeat_frac")
    ok = repeat is not None and repeat <= repeat_max
    return [Row("exact_repeat_frac", repeat, f"<= {repeat_max}", "PASS" if ok else "FAIL", "")]


def eval_stress_load(measured: dict[str, Any], thresholds: dict[str, Any]) -> list[Row]:
    """The recording taken WHILE the app runs under synthetic CPU load
    (`stress` scenario). Latency/holes are expected to suffer (an overloaded
    engine inserts 10 ms gaps before the fallback lands), so they are
    INFO; the invariant is that the virtual mic never goes dead."""
    run_max = thresholds.get("stress_dead_run_max_ms", 200)
    run = measured.get("dead_run_ms")
    ok = run is not None and run <= run_max
    return [
        Row("dead_run_ms", run, f"<= {run_max}", "PASS" if ok else "FAIL", "longest dead-output stretch while the mic carried speech"),
        Row("dead_ms", measured.get("dead_ms"), "(informational)", "INFO", ""),
        Row("holes", measured.get("holes"), "(informational)", "INFO", ""),
        Row("zero_run_ms", measured.get("zero_run_ms"), "(informational)", "INFO", ""),
    ]


def _cpu_row(measured: dict[str, Any], thresholds: dict[str, Any]) -> list[Row]:
    """INFO row flagging a recording that ran under CPU load (average busy
    share of all cores during the recording, from /proc/stat)."""
    busy = measured.get("cpu_busy_pct")
    if busy is None:
        return []
    flag = float(thresholds.get("load_flag_busy_pct", 25))
    try:
        loaded = float(busy) > flag
    except ValueError:
        loaded = False
    extra = f", steal {measured.get('cpu_steal_pct', '?')}%, loadavg {measured.get('loadavg_1m', '?')}"
    notes = []
    if loaded:
        notes.append("ran under load")
    if measured.get("stress_spinners"):
        # The stress scenario's load is ONE saturated CPU (the app pinned next
        # to busy loops), which barely moves the all-core average.
        notes.append(f"synthetic load: app pinned to one CPU with {measured['stress_spinners']} busy loops")
    return [Row("cpu_busy_pct", f"{busy}{extra}", f"flag > {flag:g}", "INFO", "; ".join(notes))]


def eval_check(measured: dict[str, Any], thresholds: dict[str, Any]) -> list[Row]:
    """A precomputed cross-recording or log-based check (see `check`,
    `diff-check`, `swap-check`, `log-check`): just echo its own verdict."""
    return [
        Row(
            measured.get("metric", "check"),
            measured.get("value"),
            measured.get("threshold_desc", ""),
            measured.get("result", "FAIL"),
            measured.get("note", ""),
        )
    ]


RULES_BY_KIND = {
    "speech": eval_speech,
    "monitor_path": eval_speech,
    "swaps_during": eval_swaps_during,
    "dc_speech": eval_dc_speech,
    "dc_silence": eval_dc_silence,
    "ag_on": eval_ag_on,
    "ag_off": eval_ag_off,
    "ag_pink": eval_ag_pink,
    "check": eval_check,
    "stress_load": eval_stress_load,
}


def evaluate(measured: dict[str, Any], thresholds: dict[str, Any]) -> list[Row]:
    kind = measured.get("kind", "speech")
    rule = RULES_BY_KIND.get(kind, eval_speech)
    return rule(measured, thresholds) + _cpu_row(measured, thresholds)


def load_summary(measurements: list[dict[str, Any]], thresholds: dict[str, Any]) -> tuple[str, str]:
    """(max cpu_busy_pct across recordings, "yes"/"no" ran-under-load flag)
    for the report's Environment table."""
    flag = float(thresholds.get("load_flag_busy_pct", 25))
    vals = []
    for m in measurements:
        try:
            vals.append(float(m["cpu_busy_pct"]))
        except (KeyError, TypeError, ValueError):
            continue
    if not vals:
        return "n/a", "unknown"
    peak = max(vals)
    return f"{peak:g}", "yes" if peak > flag else "no"


def render_report(
    measurements: list[dict[str, Any]],
    thresholds: dict[str, Any],
    meta: dict[str, Any],
    aborted: str | None,
) -> tuple[str, int]:
    now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d %H:%M:%SZ")
    lines = [f"# CleanMic E2E audio report -- {now}", ""]

    lines.append("## Environment")
    lines.append("")
    lines.append("| Key | Value |")
    lines.append("| --- | --- |")
    for key in (
        "binary", "sha256_12", "mtime", "git_head", "git_dirty",
        "display", "scale", "lang", "remote_desktop_session",
    ):
        if key in meta:
            lines.append(f"| {key} | {meta[key]} |")
    peak_busy, under_load = load_summary(measurements, thresholds)
    lines.append(f"| max_cpu_busy_pct | {peak_busy} |")
    lines.append(f"| ran_under_load | {under_load} |")
    lines.append("")

    lines.append("## Thresholds")
    lines.append("")
    lines.append("| Threshold | Value |")
    lines.append("| --- | --- |")
    for key in sorted(thresholds):
        lines.append(f"| {key} | {thresholds[key]} |")
    lines.append("")

    total_fail = 0
    by_scenario: dict[str, list[dict[str, Any]]] = {}
    for m in measurements:
        by_scenario.setdefault(m.get("scenario", "unknown"), []).append(m)

    for scenario in sorted(by_scenario):
        lines.append(f"## Scenario: {scenario}")
        lines.append("")
        lines.append("| Recording | Metric | Value | Threshold | Result |")
        lines.append("| --- | --- | --- | --- | --- |")
        for m in by_scenario[scenario]:
            rows = evaluate(m, thresholds)
            for row in rows:
                if row.result == "FAIL":
                    total_fail += 1
                note = f" ({row.note})" if row.note else ""
                lines.append(
                    f"| {m.get('recording', '?')} | {row.metric} | {row.value}{note} | "
                    f"{row.threshold} | {row.result} |"
                )
        lines.append("")

    if aborted:
        lines.append("## Aborted")
        lines.append("")
        lines.append(aborted)
        lines.append("")

    if total_fail:
        summary = f"Result: FAIL ({total_fail} failed)"
    else:
        summary = "Result: PASS"
    lines.append(f"**{summary}**")
    lines.append("")

    lines.append("## Caveats")
    lines.append("")
    lines.append("- Synthetic mic (pw-loopback), not a physical microphone or hardware clock.")
    lines.append("- No perceptual (subjective quality) judgement is made here.")
    lines.append("- The monitor path is covered only when run with `--monitor-null-sink`.")
    lines.append("")

    exit_code = 1 if total_fail else 0
    return "\n".join(lines), exit_code


def cmd_report(args: argparse.Namespace) -> int:
    thresholds = {k: _coerce_threshold_value(v) for k, v in _parse_kv_list(args.threshold or []).items()}
    meta = _parse_kv_list(args.meta or [])
    measurements: list[dict[str, Any]] = []
    for path in args.json_files:
        with open(path, encoding="utf-8") as fh:
            data = json.load(fh)
        # `log-check` writes a LIST of checks (fell_behind/errors/panics) in
        # one file; everything else writes a single dict.
        measurements.extend(data if isinstance(data, list) else [data])
    text, exit_code = render_report(measurements, thresholds, meta, args.aborted)
    with open(args.out, "w", encoding="utf-8") as fh:
        fh.write(text)
        if not text.endswith("\n"):
            fh.write("\n")
    print(f"analyze: report -> {args.out} (exit {exit_code})")
    return exit_code


def cmd_logscan(args: argparse.Namespace) -> int:
    with open(args.log, encoding="utf-8", errors="replace") as fh:
        text = fh.read()
    result = logscan(text)
    with open(args.json_out, "w", encoding="utf-8") as fh:
        json.dump(result, fh, indent=2)
    print(
        f"analyze: logscan {args.log} -> {args.json_out} "
        f"(fell_behind={result['fell_behind']} errors={result['errors']} panics={result['panics']})"
    )
    return 0


def cmd_check(args: argparse.Namespace) -> int:
    result = {
        "scenario": args.scenario,
        "recording": args.recording,
        "kind": "check",
        "metric": args.metric,
        "value": _coerce_threshold_value(args.value),
        "threshold_desc": args.threshold_desc,
        "result": args.result,
        "note": args.note or "",
    }
    with open(args.json_out, "w", encoding="utf-8") as fh:
        json.dump(result, fh, indent=2)
    return 0


def cmd_diff_check(args: argparse.Namespace) -> int:
    """Generic |a.FIELD - b.FIELD| <= max check between two `measure`d JSON
    files -- used for swaps/toggle pre-vs-post latency drift and the
    autogain pink-noise on-vs-off floor comparison."""
    with open(args.a, encoding="utf-8") as fh:
        a = json.load(fh)
    with open(args.b, encoding="utf-8") as fh:
        b = json.load(fh)
    av = a.get(args.a_field)
    bv = b.get(args.b_field)
    diff = abs(av - bv) if av is not None and bv is not None else None
    ok = diff is not None and diff <= args.max_diff
    result = {
        "scenario": args.scenario,
        "recording": args.recording,
        "kind": "check",
        "metric": args.metric,
        "value": round(diff, 4) if diff is not None else None,
        "threshold_desc": f"<= {args.max_diff}",
        "result": "PASS" if ok else "FAIL",
        "note": args.note or f"{a.get('recording')} vs {b.get('recording')}",
    }
    with open(args.json_out, "w", encoding="utf-8") as fh:
        json.dump(result, fh, indent=2)
    return 0


def cmd_swap_check(args: argparse.Namespace) -> int:
    with open(args.logscan, encoding="utf-8") as fh:
        scanned = json.load(fh)
    logged = [tuple(x) for x in scanned.get("engine_changed", [])]
    expected = [tuple(p.split(":", 1)) for p in args.expected.split(",") if p]
    result_str, note = evaluate_swap_sequence(expected, logged)
    result = {
        "scenario": args.scenario,
        "recording": args.recording,
        "kind": "check",
        "metric": "engine_swaps_confirmed",
        "value": f"{len(logged)}/{len(expected)}",
        "threshold_desc": f"== {len(expected)} matching, in order",
        "result": result_str,
        "note": note,
    }
    with open(args.json_out, "w", encoding="utf-8") as fh:
        json.dump(result, fh, indent=2)
    return 0


def cmd_log_check(args: argparse.Namespace) -> int:
    with open(args.logscan, encoding="utf-8") as fh:
        scanned = json.load(fh)
    checks = []
    for metric, key, maxval in (
        ("fell_behind", "fell_behind", args.fell_behind_max),
        ("errors", "errors", args.error_max),
        ("panics", "panics", args.panic_max),
    ):
        v = scanned.get(key, 0)
        ok = v <= maxval
        checks.append(
            {
                "scenario": args.scenario,
                "recording": args.recording,
                "kind": "check",
                "metric": metric,
                "value": v,
                "threshold_desc": f"<= {maxval}",
                "result": "PASS" if ok else "FAIL",
                "note": "",
            }
        )
    with open(args.json_out, "w", encoding="utf-8") as fh:
        json.dump(checks, fh, indent=2)
    return 0


def cmd_stress_check(args: argparse.Namespace) -> int:
    with open(args.logscan, encoding="utf-8") as fh:
        scanned = json.load(fh)
    rows = evaluate_stress(scanned, args.app_alive == "yes", args.load_start, args.recovery_max_s)
    for r in rows:
        r["scenario"] = args.scenario
        r["recording"] = args.recording
    with open(args.json_out, "w", encoding="utf-8") as fh:
        json.dump(rows, fh, indent=2)
    return 0


def cmd_active_engine(args: argparse.Namespace) -> int:
    with open(args.logscan, encoding="utf-8") as fh:
        scanned = json.load(fh)
    print(active_engine_after(scanned, args.started_with))
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_measure = sub.add_parser("measure")
    p_measure.add_argument("wav")
    p_measure.add_argument("--scenario", required=True)
    p_measure.add_argument("--recording", required=True)
    p_measure.add_argument("--kind", required=True)
    p_measure.add_argument("--meta", action="append", default=[])
    p_measure.add_argument("--json-out", required=True)

    p_report = sub.add_parser("report")
    p_report.add_argument("--out", required=True)
    p_report.add_argument("--threshold", action="append", default=[])
    p_report.add_argument("--meta", action="append", default=[])
    p_report.add_argument("--aborted", default=None)
    p_report.add_argument("json_files", nargs="*")

    p_logscan = sub.add_parser("logscan")
    p_logscan.add_argument("log")
    p_logscan.add_argument("--json-out", required=True)

    p_check = sub.add_parser("check")
    p_check.add_argument("--scenario", required=True)
    p_check.add_argument("--recording", required=True)
    p_check.add_argument("--metric", required=True)
    p_check.add_argument("--value", required=True)
    p_check.add_argument("--threshold-desc", required=True)
    p_check.add_argument("--result", required=True, choices=["PASS", "FAIL", "INFO", "SKIP"])
    p_check.add_argument("--note", default="")
    p_check.add_argument("--json-out", required=True)

    p_diff = sub.add_parser("diff-check")
    p_diff.add_argument("--a", required=True)
    p_diff.add_argument("--b", required=True)
    p_diff.add_argument("--a-field", required=True)
    p_diff.add_argument("--b-field", required=True)
    p_diff.add_argument("--max-diff", type=float, required=True)
    p_diff.add_argument("--metric", required=True)
    p_diff.add_argument("--scenario", required=True)
    p_diff.add_argument("--recording", required=True)
    p_diff.add_argument("--note", default="")
    p_diff.add_argument("--json-out", required=True)

    p_swap = sub.add_parser("swap-check")
    p_swap.add_argument("--logscan", required=True)
    p_swap.add_argument("--expected", required=True, help="comma-separated Engine:Mode pairs")
    p_swap.add_argument("--scenario", required=True)
    p_swap.add_argument("--recording", required=True)
    p_swap.add_argument("--json-out", required=True)

    p_logcheck = sub.add_parser("log-check")
    p_logcheck.add_argument("--logscan", required=True)
    p_logcheck.add_argument("--fell-behind-max", type=int, default=0)
    p_logcheck.add_argument("--error-max", type=int, default=0)
    p_logcheck.add_argument("--panic-max", type=int, default=0)
    p_logcheck.add_argument("--scenario", required=True)
    p_logcheck.add_argument("--recording", default="log")
    p_logcheck.add_argument("--json-out", required=True)

    p_stress = sub.add_parser("stress-check")
    p_stress.add_argument("--logscan", required=True)
    p_stress.add_argument("--app-alive", required=True, choices=["yes", "no"])
    p_stress.add_argument("--load-start", type=float, required=True, help="Unix epoch the load started")
    p_stress.add_argument("--recovery-max-s", type=float, required=True)
    p_stress.add_argument("--scenario", required=True)
    p_stress.add_argument("--recording", required=True)
    p_stress.add_argument("--json-out", required=True)

    p_active = sub.add_parser("active-engine")
    p_active.add_argument("--logscan", required=True)
    p_active.add_argument("--started-with", required=True)

    args = parser.parse_args(argv)

    try:
        if args.cmd == "measure":
            return cmd_measure(args)
        if args.cmd == "report":
            return cmd_report(args)
        if args.cmd == "logscan":
            return cmd_logscan(args)
        if args.cmd == "check":
            return cmd_check(args)
        if args.cmd == "diff-check":
            return cmd_diff_check(args)
        if args.cmd == "swap-check":
            return cmd_swap_check(args)
        if args.cmd == "log-check":
            return cmd_log_check(args)
        if args.cmd == "stress-check":
            return cmd_stress_check(args)
        if args.cmd == "active-engine":
            return cmd_active_engine(args)
    except (OSError, ValueError) as exc:
        print(f"analyze: {exc}", file=sys.stderr)
        return 2

    parser.error(f"unknown subcommand {args.cmd!r}")
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
