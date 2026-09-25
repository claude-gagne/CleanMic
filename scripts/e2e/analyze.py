#!/usr/bin/env python3
"""analyze.py -- WAV reader, latency/quality metrics and Markdown report
generator for the CleanMic silent E2E harness (scripts/e2e-audio.sh).

Pure, stdlib+numpy functions so they are unit-testable without PipeWire, a
display or any recorded audio (see test_analyze.py). The `measure` and
`report` CLI subcommands are the glue scripts/e2e-audio.sh drives.

USAGE
  analyze.py measure REC.wav --scenario S --recording R --kind K \
      [--meta k=v]... [--contention-samples F.jsonl --rec-link-epoch EPOCH] \
      --json-out OUT.json
  analyze.py stress-check --logscan L.json --app-alive yes|no --load-start EPOCH \
      --recovery-max-s N --scenario S --recording R --json-out OUT.json
  analyze.py active-engine --logscan L.json --started-with ENGINE
  analyze.py attempts --max-attempts K --scenario S --recording R \
      [--threshold k=v]... --json-out OUT.json ATTEMPT.json...
      (prints "done" or "again")
  analyze.py report --out report.md [--threshold k=v]... [--meta k=v]... \
      [--aborted REASON] MEASURED.json...

EXIT CODES (report / measure)
  0  ok (report: every metric PASSed)
  1  report: at least one metric FAILed
  2  bad input (unreadable WAV / JSON, bad CLI usage)
  7  report: no FAIL, but at least one metric is INCONCLUSIVE (R5: the
     machine never got quiet, or a scheduling-sensitive metric only failed
     under contention or host-starvation)
"""

from __future__ import annotations

import argparse
import datetime
import json
import os
import re
import struct
import sys
from collections import Counter, namedtuple
from typing import Any

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import contention as contention_mod  # noqa: E402  (quick 260924-n4s, R5)

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


def _hole_frame_indices(o: np.ndarray, fs: int = 48000, frame_ms: float = 10.0) -> list[int]:
    """Frame indices whose level drops below -90 dB while the frames 3
    positions before AND after it are both above -50 dB -- a brief silent
    gap surrounded by loud audio, distinct from natural silence. Shared by
    `holes()` (count) and `hole_times_s()` (positions, for R5's host/engine
    attribution)."""
    frame = int(fs * frame_ms / 1000)
    if frame <= 0 or len(o) < frame:
        return []
    m = len(o) // frame
    framed = o[: m * frame].reshape(m, frame)
    levels = 20 * np.log10(np.sqrt(np.mean(framed**2, axis=1)) + 1e-12)
    idxs = []
    for k in range(3, m - 3):
        if levels[k] < -90 and levels[k - 3] > -50 and levels[k + 3] > -50:
            idxs.append(k)
    return idxs


def holes(o: np.ndarray, fs: int = 48000, frame_ms: float = 10.0) -> int:
    """Count of `frame_ms` frames whose level drops below -90 dB while the
    frames 3 positions before AND after it are both above -50 dB -- a brief
    silent gap surrounded by loud audio, distinct from natural silence."""
    return len(_hole_frame_indices(o, fs, frame_ms))


def hole_times_s(o: np.ndarray, fs: int = 48000, frame_ms: float = 10.0) -> list[float]:
    """Start time (seconds, relative to the start of `o`) of every frame
    `holes()` counts."""
    frame = int(fs * frame_ms / 1000)
    return [round(k * frame / fs, 3) for k in _hole_frame_indices(o, fs, frame_ms)]


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
# quick 260924-n4s (R1/R3): D-01's exact new log prefixes (src/audio.rs,
# src/engine/deepfilter.rs) -- timestamped, so their span can be mapped to a
# WAV-relative time the same way Task 1 maps holes (epoch_to_wav).
_LOG_TROUBLE_SPAN_RE = re.compile(r"after ([\d.]+) s of sustained trouble")
_LOG_DFN_BYPASS_RE = re.compile(r"DeepFilterNet: bypassing the plugin")
_LOG_DFN_RECOVER_RE = re.compile(r"DeepFilterNet: plugin back to real time")
_LOG_DFN_RESTART_TS_RE = re.compile(r"^\[(\S+) [A-Z]+ +[^\]]*\] DeepFilterNet: plugin fell behind real time", re.MULTILINE)
_LOG_DFN_SHED_TS_RE = re.compile(r"^\[(\S+) [A-Z]+ +[^\]]*\] DeepFilterNet: shed (\d+) ms of accumulated plugin latency", re.MULTILINE)
_LOG_SWAP_STARTED_RE = re.compile(r"^\[(\S+) [A-Z]+ +[^\]]*\] Engine swap started \(crossfading\)", re.MULTILINE)
_LOG_MODE_SET_RE = re.compile(r"^\[(\S+) [A-Z]+ +[^\]]*\] Engine mode set to (\w+)", re.MULTILINE)


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
        "trouble_span_s": [float(x) for x in _LOG_TROUBLE_SPAN_RE.findall(log_text)],
        "dfn_bypass_entries": len(_LOG_DFN_BYPASS_RE.findall(log_text)),
        "dfn_recoveries": len(_LOG_DFN_RECOVER_RE.findall(log_text)),
        "dfn_restart_at": [t for t in (_log_epoch(ts) for ts in _LOG_DFN_RESTART_TS_RE.findall(log_text)) if t is not None],
        "dfn_shed_at": [
            (t, int(ms)) for ts, ms in _LOG_DFN_SHED_TS_RE.findall(log_text) if (t := _log_epoch(ts)) is not None
        ],
        "swap_started_at": [t for t in (_log_epoch(ts) for ts in _LOG_SWAP_STARTED_RE.findall(log_text)) if t is not None],
        "mode_set_at": [
            (t, mode) for ts, mode in _LOG_MODE_SET_RE.findall(log_text) if (t := _log_epoch(ts)) is not None
        ],
    }


def _log_epoch(ts: str) -> float | None:
    """env_logger's `2026-09-24T15:24:07.616Z` -> Unix seconds (None if unparsable)."""
    try:
        return datetime.datetime.strptime(ts, "%Y-%m-%dT%H:%M:%S.%fZ").replace(tzinfo=datetime.timezone.utc).timestamp()
    except ValueError:
        return None


# ---------------------------------------------------------------------------
# Task 1 (R5): load-aware verdict -- WAV<->epoch mapping, hole attribution,
# scheduling-sensitive retries with a majority verdict.
# ---------------------------------------------------------------------------

# Metrics whose value a busy HOST (not the engine) can move: a contended
# machine starves the audio thread's `poll`/mutex wakeups (holes,
# fell_behind) or the graph-quantum jitter the envelope xcorr sees
# (latency_spread_ms). latency_ms is scheduling-sensitive ONLY for
# DeepFilterNet, whose vendored plugin adds +10 ms per underrun -- a
# scheduling effect proven in dfn-panic-under-load -- while every other
# engine's steady-state latency is architectural, not host-load-dependent.
SCHED_SENSITIVE = {"holes", "latency_spread_ms", "fell_behind"}

# quick 260924-n4s (R1/R5, Task 3): every recording inside `stress`/`spike`
# is taken adjacent to DELIBERATE synthetic CPU load by design -- a busy
# host (this shared box's OTHER agents/builds/Syncthing, not just the
# harness's own spinners) can plausibly move latency_ms/lag_corr/
# dead_run_ms here even once the load has stopped, so a FAIL there is
# downgraded to INCONCLUSIVE (see `evaluate_stress`/`evaluate_spike`) when
# that specific recording was itself contended.
_STRESS_SPIKE_SCENARIOS = {"stress", "spike"}
_STRESS_SPIKE_EXTRA_SCHED_SENSITIVE = {"dead_run_ms", "latency_ms", "lag_corr"}

# `baseline` real-world evidence (3-run stability check, 2026-09-24, this
# shared machine): a single contended attempt can make the envelope xcorr
# genuinely unmeasurable (lag_corr collapses), which is a HOST artifact, not
# an engine defect -- yet lag_corr/latency_ms were deterministic for
# baseline, so one unlucky attempt stopped the whole engine with a FAIL no
# retry could recover from. Folding them into `decide_attempts`' own
# majority-of-QUIET-attempts machinery (attempts.md's design, already used
# for holes/latency_spread_ms/fell_behind) fixes this the same way: a
# contended attempt's FAIL is evidence, not a vote, and the loop asks for
# another attempt instead of giving up. This does NOT touch swaps pre/post,
# toggle, modes, dc, or autogain -- their lag_corr/latency_ms stay strict.
_BASELINE_SCENARIOS = {"baseline"}
_BASELINE_EXTRA_SCHED_SENSITIVE = {"latency_ms", "lag_corr"}


def is_sched_sensitive_metric(metric: str, measured: dict[str, Any]) -> bool:
    if metric in SCHED_SENSITIVE:
        return True
    if metric == "latency_ms" and str(measured.get("engine", "")) == "DeepFilterNet":
        return True
    if measured.get("scenario") in _STRESS_SPIKE_SCENARIOS and metric in _STRESS_SPIKE_EXTRA_SCHED_SENSITIVE:
        return True
    if measured.get("scenario") in _BASELINE_SCENARIOS and metric in _BASELINE_EXTRA_SCHED_SENSITIVE:
        return True
    return False


def hole_epoch(t_wav: float, rec_link_epoch: float, source_onset_s: float, input_onset_wav_s: float) -> float:
    """Map a hole's position in the OUTPUT-active-region-relative WAV
    timeline (`hole_times_s`, itself relative to the input's active-region
    start) back to a wall-clock epoch: REC_LINK_EPOCH (when the player was
    linked, i.e. wav-time 0) plus the source signal's own silence lead-in
    (`source_onset_s`) plus the hole's offset from the INPUT's measured
    active-region start in this particular recording."""
    return rec_link_epoch + source_onset_s + (t_wav - input_onset_wav_s)


def attribute_holes(
    hole_times_wav: list[float],
    rec_link_epoch: float,
    source_onset_s: float,
    input_onset_wav_s: float,
    fast_samples: list[dict[str, Any]],
    audio_wait_starved_ms: float,
) -> list[dict[str, Any]]:
    """Classify each hole as host_starved (some app thread waited
    >= audio_wait_starved_ms inside [t-250ms, t+150ms]), else engine_slow
    (the cleanmic-audio thread's CPU share in that window is >= 90%), else
    unexplained."""
    fast = sorted(fast_samples, key=lambda s: s["t"])
    out: list[dict[str, Any]] = []
    for t_wav in hole_times_wav:
        t_epoch = hole_epoch(t_wav, rec_link_epoch, source_onset_s, input_onset_wav_s)
        lo, hi = t_epoch - 0.25, t_epoch + 0.15
        host_starved = False
        audio_run_ns = 0.0
        audio_wall_s = 0.0
        for a, b in zip(fast, fast[1:]):
            if b["t"] < lo or a["t"] > hi:
                continue
            dt = b["t"] - a["t"]
            if dt <= 0:
                continue
            a_threads = {th["tid"]: th for th in a.get("threads", [])}
            for tb in b.get("threads", []):
                ta = a_threads.get(tb["tid"])
                if ta is None:
                    continue
                d_wait_ms = (tb["wait_ns"] - ta["wait_ns"]) / 1e6
                if d_wait_ms >= audio_wait_starved_ms:
                    host_starved = True
                if tb.get("comm") == contention_mod.AUDIO_THREAD_COMM:
                    audio_run_ns += tb["run_ns"] - ta["run_ns"]
                    audio_wall_s += dt
        if host_starved:
            cls = "host_starved"
        elif audio_wall_s > 0 and (audio_run_ns / 1e9) / audio_wall_s >= 0.90:
            cls = "engine_slow"
        else:
            cls = "unexplained"
        out.append({"t_wav": round(t_wav, 3), "t_epoch": round(t_epoch, 3), "class": cls})
    return out


def decide_attempts(attempts: list[dict[str, dict[str, Any]]], max_attempts: int) -> tuple[list[dict[str, Any]], bool]:
    """attempts is one dict per attempt made SO FAR (in order); each maps
    metric name -> {"value", "result" ("PASS"/"FAIL"), "sched_sensitive",
    "contended", "host_starved" (holes only, meaningful on FAIL)}.

    Deterministic metrics never retry: any FAIL anywhere is a final FAIL.
    Scheduling-sensitive metrics that never FAIL are resolved PASS on the
    first attempt (no retry needed to confirm a clean run). Once one FAILs,
    a FAIL under contention (or, for `holes`, a FAIL whose failing holes are
    ALL host_starved) is excluded from the "quiet" tally -- it's evidence,
    but not counted toward a majority. The metric resolves the moment either
    side of the quiet tally reaches a majority of `max_attempts`, or, once
    `max_attempts` is reached, by comparing the quiet tally (a tie, including
    0-0, is INCONCLUSIVE -- never FAIL when every failure was excluded).

    Returns (rows, needs_more)."""
    majority = max_attempts // 2 + 1
    metrics: list[str] = []
    seen: set[str] = set()
    for a in attempts:
        for m in a:
            if m not in seen:
                seen.add(m)
                metrics.append(m)

    rows: list[dict[str, Any]] = []
    any_unresolved = False
    any_deterministic_fail = False
    for metric in metrics:
        per_attempt = [a[metric] for a in attempts if metric in a]
        sched_sensitive = any(p.get("sched_sensitive") for p in per_attempt)

        if not sched_sensitive:
            fails = [p for p in per_attempt if p["result"] == "FAIL"]
            result = "FAIL" if fails else "PASS"
            if result == "FAIL":
                any_deterministic_fail = True
            rows.append(
                {
                    "metric": metric,
                    "result": result,
                    "sched_sensitive": False,
                    "values": [p["value"] for p in per_attempt],
                    "classes": [p.get("contended") and "contended" or "quiet" for p in per_attempt],
                }
            )
            continue

        def excluded(p: dict[str, Any]) -> bool:
            return bool(p.get("contended")) or (metric == "holes" and p["result"] == "FAIL" and p.get("host_starved"))

        quiet_pass = sum(1 for p in per_attempt if p["result"] == "PASS" and not excluded(p))
        quiet_fail = sum(1 for p in per_attempt if p["result"] == "FAIL" and not excluded(p))
        any_fail_at_all = any(p["result"] == "FAIL" for p in per_attempt)

        unresolved = False
        if not any_fail_at_all:
            result = "PASS"
        elif quiet_fail >= majority:
            result = "FAIL"
        elif quiet_pass >= majority:
            result = "PASS"
        elif len(attempts) >= max_attempts:
            if quiet_pass > quiet_fail:
                result = "PASS"
            elif quiet_fail > quiet_pass:
                result = "FAIL"
            else:
                result = "INCONCLUSIVE"
        else:
            result = "INCONCLUSIVE"
            unresolved = True

        if unresolved:
            any_unresolved = True
        rows.append(
            {
                "metric": metric,
                "result": result,
                "sched_sensitive": True,
                "values": [p["value"] for p in per_attempt],
                "classes": [
                    "contended" if p.get("contended") else ("host_starved" if p.get("host_starved") else "quiet")
                    for p in per_attempt
                ],
            }
        )

    needs_more = (not any_deterministic_fail) and any_unresolved and len(attempts) < max_attempts
    return rows, needs_more


def evaluate_stress(
    logscan_result: dict[str, Any],
    app_alive: bool,
    load_start_epoch: float,
    recovery_max_s: float,
    recovery_min_s: float = 0.0,
) -> list[dict[str, Any]]:
    """Log/process-level verdicts for one `stress` run (the recording-level
    dead-output verdict is the `stress_load` measure rule). Returns check
    dicts (kind=check) for the report.

    quick 260924-n4s (R1, D-01): `recovery_min_s` (== ENGINE_FALLBACK_GRACE
    when the grace is in effect) makes an EARLY fallback a FAIL too -- a
    recovery faster than the grace means the grace was not honoured. A
    fallback logged BEFORE the synthetic load even started is INCONCLUSIVE
    (the machine was already overloaded by something else), never PASS."""
    rows: list[dict[str, Any]] = []

    def row(metric: str, value: Any, desc: str, result: str, note: str = "") -> None:
        rows.append({"kind": "check", "metric": metric, "value": value, "threshold_desc": desc, "result": result, "note": note})

    row("app_alive", "yes" if app_alive else "DIED", "== yes", "PASS" if app_alive else "FAIL")
    aborts = logscan_result.get("plugin_aborts", 0)
    row("plugin_abort_lines", aborts, "== 0", "PASS" if aborts == 0 else "FAIL", "vendored DeepFilterNet 'Processing too slow!' panic")
    desc = f"{recovery_min_s} <= x <= {recovery_max_s}"
    fallbacks = logscan_result.get("fallbacks", [])
    if fallbacks:
        first = fallbacks[0]
        t = first.get("t")
        if t is None:
            row("recovery_s", "unparsable", desc, "FAIL")
        elif t < load_start_epoch:
            # The machine was already too busy for the engine before the
            # synthetic load began (e.g. someone else's build): this run's
            # recovery timing tells us nothing about the grace.
            note = f"{first['from']} -> {first['to']} ({first['reason']}); before the synthetic load started (machine already overloaded)"
            row("recovery_s", "n/a", desc, "INCONCLUSIVE", note)
        else:
            note = f"{first['from']} -> {first['to']} ({first['reason']})"
            rec = round(t - load_start_epoch, 2)
            ok = recovery_min_s <= rec <= recovery_max_s
            row("recovery_s", rec, desc, "PASS" if ok else "FAIL", note)
    else:
        row("recovery_s", "no fallback", desc, "INFO", "engine kept up (no runtime fallback logged)")
    row("dfn_restarts", logscan_result.get("dfn_restarts", 0), "(informational)", "INFO")
    spans = logscan_result.get("trouble_span_s", [])
    if spans:
        row("trouble_sustained_s", spans[0], "(informational)", "INFO")
    row("dfn_bypass_entries", logscan_result.get("dfn_bypass_entries", 0), "(informational)", "INFO")
    row("dfn_recoveries", logscan_result.get("dfn_recoveries", 0), "(informational)", "INFO")
    row("dfn_sheds", len(logscan_result.get("dfn_shed_at", [])), "(informational)", "INFO")
    return rows


def evaluate_spike(
    logscan_result: dict[str, Any],
    app_alive: bool,
    launched_engine: str,
    contended: bool = False,
) -> list[dict[str, Any]]:
    """Log/process-level verdicts for the `spike` scenario (R1): a short
    1.3 s burst must never trigger a runtime fallback, never abort the
    plugin, and must leave the launched engine still active.

    `contended` (the load recording's OWN contention verdict, R5): a
    fallback IS the grace mechanism correctly reacting to genuinely
    sustained trouble -- if that trouble outlasted the synthetic burst
    because the shared host was ALSO busy, this isn't evidence the grace
    failed on a clean, isolated 1.3 s spike. Downgrades a fallback/wrong-
    end-engine FAIL to INCONCLUSIVE rather than PASS -- it's still not a
    confirmed clean spike."""
    rows: list[dict[str, Any]] = []

    def row(metric: str, value: Any, desc: str, result: str, note: str = "") -> None:
        rows.append({"kind": "check", "metric": metric, "value": value, "threshold_desc": desc, "result": result, "note": note})

    fallbacks = logscan_result.get("fallbacks", [])
    note = "; ".join(f"{f['from']} -> {f['to']} ({f['reason']})" for f in fallbacks)
    fb_result = "PASS" if not fallbacks else "FAIL"
    if fallbacks and contended:
        fb_result = "INCONCLUSIVE"
        note += "; recording was contended -- cannot isolate the synthetic burst from real ambient load"
    row("fallback_during_spike", len(fallbacks), "== 0", fb_result, note)
    row("app_alive", "yes" if app_alive else "DIED", "== yes", "PASS" if app_alive else "FAIL")
    aborts = logscan_result.get("plugin_aborts", 0)
    row("plugin_abort_lines", aborts, "== 0", "PASS" if aborts == 0 else "FAIL")
    active = active_engine_after(logscan_result, launched_engine)
    end_result = "PASS" if active == launched_engine else "FAIL"
    end_note = ""
    if active != launched_engine and contended:
        end_result = "INCONCLUSIVE"
        end_note = "recording was contended -- cannot isolate the synthetic burst from real ambient load"
    row("end_engine", active, f"== {launched_engine}", end_result, end_note)
    return rows


def active_engine_after(logscan_result: dict[str, Any], started_with: str) -> str:
    """The engine running at the end of a log: the last runtime fallback's
    target, else `started_with`."""
    fallbacks = logscan_result.get("fallbacks", [])
    return fallbacks[-1]["to"] if fallbacks else started_with


# ---------------------------------------------------------------------------
# Task 3 (R3): per-swap zero-run attribution over the full swap sequence.
# ---------------------------------------------------------------------------


def epoch_to_wav(epoch: float, rec_link_epoch: float, source_onset_s: float) -> float:
    """Inverse of `hole_epoch` (input_onset_wav_s=0 convention): map a
    wall-clock log epoch back to the WAV-relative timeline `zero_runs`/
    `hole_times_s` use (seconds since the recording's input-active-region
    start)."""
    return epoch - rec_link_epoch - source_onset_s


def swap_attribution(
    zero_runs_list: list[tuple[int, int]],
    fs: int,
    swap_events: list[tuple[float, str, str]],
    dfn_restart_t_wav: list[float],
    dfn_shed_t_wav: list[float],
    swap_window_s: float = 0.6,
    dfn_window_s: float = 0.15,
    hole_attribution: list[dict[str, Any]] | None = None,
    hole_match_window_s: float = 0.05,
) -> list[dict[str, Any]]:
    """Classify every zero run (start_sample, length) from `zero_runs()`
    against the (t_wav, engine, mode) of every swap INSIDE the speech,
    already excluding swaps whose instant lies outside it:

    - a run starting at sample 0 is `leading` (pipeline latency, not a swap);
    - one within [swap_t, swap_t + swap_window_s] of a swap is `swap`,
      attributed to that swap's (engine, mode);
    - a <= 480-sample run within +/- dfn_window_s of a DFN restart is
      `dfn_underrun`;
    - one within +/- dfn_window_s of a DFN shed is `dfn_shed`;
    - anything else is `unattributed`, annotated with Task 1's own host
      evidence (`hole_attribution`, from `attribute_holes()`) for the
      nearest hole within `hole_match_window_s`, when one is given.
    """
    out: list[dict[str, Any]] = []
    swaps_sorted = sorted(swap_events, key=lambda s: s[0])
    holes_sorted = sorted(hole_attribution or [], key=lambda h: h["t_wav"])
    for start, length in zero_runs_list:
        t_wav = round(start / fs, 3)
        ms = round(length / fs * 1000, 1)
        if start == 0:
            out.append({"t": t_wav, "ms": ms, "class": "leading", "engine": None, "mode": None})
            continue
        matched = next((s for s in swaps_sorted if s[0] <= t_wav <= s[0] + swap_window_s), None)
        if matched is not None:
            out.append({"t": t_wav, "ms": ms, "class": "swap", "engine": matched[1], "mode": matched[2]})
            continue
        if length <= 480 and any(abs(t_wav - r) <= dfn_window_s for r in dfn_restart_t_wav):
            out.append({"t": t_wav, "ms": ms, "class": "dfn_underrun", "engine": None, "mode": None})
            continue
        if any(abs(t_wav - r) <= dfn_window_s for r in dfn_shed_t_wav):
            out.append({"t": t_wav, "ms": ms, "class": "dfn_shed", "engine": None, "mode": None})
            continue
        nearest = min(holes_sorted, key=lambda h: abs(h["t_wav"] - t_wav), default=None)
        host_evidence = (
            nearest["class"] if nearest is not None and abs(nearest["t_wav"] - t_wav) <= hole_match_window_s else None
        )
        out.append({"t": t_wav, "ms": ms, "class": "unattributed", "engine": None, "mode": None, "host_evidence": host_evidence})
    return out


def swap_attribution_summary(attributed: list[dict[str, Any]]) -> dict[str, Any]:
    """Total zero ms per attribution class, plus the swap-class-only total
    the budget check applies to."""
    by_class: dict[str, float] = {}
    for a in attributed:
        by_class[a["class"]] = round(by_class.get(a["class"], 0.0) + a["ms"], 1)
    return {"by_class_ms": by_class, "swap_class_ms": by_class.get("swap", 0.0)}


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


def _load_jsonl(path: str) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    with open(path, encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                out.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    return out


def measure_recording(
    path: str,
    scenario: str,
    recording: str,
    kind: str,
    meta: dict[str, str],
    *,
    contention_samples: str | None = None,
    rec_link_epoch: float | None = None,
    source_onset_s: float = 0.0,
    audio_wait_starved_ms: float = 5.0,
    contended_thresholds: dict[str, float] | None = None,
) -> dict[str, Any]:
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
        "hole_times_s": hole_times_s(out_active, fs),
    }
    if contention_samples is not None and rec_link_epoch is not None:
        samples = _load_jsonl(contention_samples)
        recording_span_s = len(inp) / fs
        contention_summary = contention_mod.summarize(
            samples, rec_link_epoch, rec_link_epoch + recording_span_s, contended_thresholds
        )
        result["contention"] = contention_summary
        fast_samples = [s for s in samples if s.get("type") == "fast"]
        # hole_times_s() is already relative to the INPUT's active-region
        # start (it runs on out_active = out[start:end]), so the input onset
        # offset here is 0 -- source_onset_s is the sole calibration knob
        # for any residual link-vs-first-sample latency.
        input_onset_wav_s = 0.0
        attributed = attribute_holes(
            result["hole_times_s"], rec_link_epoch, source_onset_s, input_onset_wav_s, fast_samples, audio_wait_starved_ms
        )
        result["hole_classes"] = [h["class"] for h in attributed]
        result["hole_attribution"] = attributed
    result.update(meta)
    return result


def cmd_measure(args: argparse.Namespace) -> int:
    meta = _parse_kv_list(args.meta or [])
    contended_thresholds = None
    if args.contended_other_busy_pct is not None:
        contended_thresholds = {
            "contended_other_busy_pct": args.contended_other_busy_pct,
            "contended_steal_pct": args.contended_steal_pct,
            "contended_iowait_pct": args.contended_iowait_pct,
            "audio_wait_starved_ms": args.audio_wait_starved_ms,
        }
    result = measure_recording(
        args.wav,
        args.scenario,
        args.recording,
        args.kind,
        meta,
        contention_samples=args.contention_samples,
        rec_link_epoch=args.rec_link_epoch,
        source_onset_s=args.source_onset_s,
        audio_wait_starved_ms=args.audio_wait_starved_ms,
        contended_thresholds=contended_thresholds,
    )
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


def eval_aggregate(measured: dict[str, Any], thresholds: dict[str, Any]) -> list[Row]:
    """An `attempts`-produced `aggregate` JSON: one row per metric, its final
    verdict, and the per-attempt values (never downgraded -- decide_attempts
    already resolved contention/host_starved exclusions)."""
    rows = []
    for m in measured.get("metrics", []):
        classes = m.get("classes", [])
        values_str = " / ".join(f"{v} ({c})" for v, c in zip(m.get("values", []), classes))
        note = f"{measured.get('attempts_used', '?')}/{measured.get('max_attempts', '?')} attempts: {values_str}"
        rows.append(Row(m["metric"], values_str, "(majority of quiet attempts)", m["result"], note))
    return rows


def _rule_rows(measured: dict[str, Any], thresholds: dict[str, Any]) -> list[Row]:
    """The raw (undamped by contention/host_starved downgrade) rows for this
    recording's `kind` -- what `evaluate()` starts from, and what
    `decide_attempts` needs (it does its OWN exclusion bookkeeping)."""
    kind = measured.get("kind", "speech")
    rule = RULES_BY_KIND.get(kind, eval_speech)
    return rule(measured, thresholds)


def _contention_row(measured: dict[str, Any]) -> list[Row]:
    """INFO row: every recording taken alongside a contention.py sampler
    gets its other/harness/audio_daemon busy share, steal/iowait, loadavg,
    the cleanmic-audio thread's own CPU share and max scheduling wait, and
    the contended verdict with its reasons."""
    c = measured.get("contention")
    if not c:
        return []
    note = (
        f"other_busy {c.get('other_busy_pct_mean')}% (p95 {c.get('other_busy_pct_p95')}%), "
        f"steal {c.get('steal_pct')}%, iowait {c.get('iowait_pct')}%, loadavg {c.get('loadavg_1m_max')}, "
        f"audio_thread_cpu {c.get('audio_thread_cpu_pct')}%, audio_wait_max {c.get('audio_wait_ms_max')}ms"
    )
    if c.get("reasons"):
        note += "; " + "; ".join(c["reasons"])
    return [Row("contention", "contended" if c.get("contended") else "quiet", "(informational)", "INFO", note)]


def _holes_attribution_row(measured: dict[str, Any]) -> list[Row]:
    """INFO row: how many holes this recording's `hole_classes` attributes
    to host_starved / engine_slow / unexplained."""
    classes = measured.get("hole_classes")
    if not classes:
        return []
    counts = Counter(classes)
    note = ", ".join(f"{k}={v}" for k, v in sorted(counts.items()))
    return [Row("holes_attribution", len(classes), "(informational)", "INFO", note)]


def downgrade_row(row: Row, measured: dict[str, Any]) -> Row:
    """A FAIL on a scheduling-sensitive metric is downgraded to INCONCLUSIVE
    when this recording was contended, or -- for `holes` specifically --
    when every failing hole was attributed to host_starved. Deterministic
    metrics, PASS/INFO rows, and non-`holes` sched-sensitive metrics under a
    host_starved (but not contended) run are never downgraded by this path
    (holes is the only metric with its own per-instance attribution)."""
    if row.result != "FAIL" or not is_sched_sensitive_metric(row.metric, measured):
        return row
    if measured.get("contention", {}).get("contended"):
        note = f"{row.note}; downgraded: recording was contended".strip("; ")
        return row._replace(result="INCONCLUSIVE", note=note)
    if row.metric == "holes":
        classes = measured.get("hole_classes") or []
        if classes and all(c == "host_starved" for c in classes):
            note = f"{row.note}; downgraded: every failing hole was host_starved".strip("; ")
            return row._replace(result="INCONCLUSIVE", note=note)
    return row


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
    "aggregate": eval_aggregate,
}


def evaluate(measured: dict[str, Any], thresholds: dict[str, Any]) -> list[Row]:
    kind = measured.get("kind", "speech")
    rows = _rule_rows(measured, thresholds) + _cpu_row(measured, thresholds)
    rows += _contention_row(measured) + _holes_attribution_row(measured)
    if kind != "aggregate":
        rows = [downgrade_row(r, measured) for r in rows]
    return rows


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


def contention_summary_stats(measurements: list[dict[str, Any]]) -> tuple[float | None, int]:
    """(max other_busy_pct_mean across recordings with a contention summary,
    count of contended recordings) for the report's Environment table."""
    vals = []
    contended_count = 0
    for m in measurements:
        c = m.get("contention")
        if not c:
            continue
        v = c.get("other_busy_pct_mean")
        if isinstance(v, (int, float)):
            vals.append(v)
        if c.get("contended"):
            contended_count += 1
    return (max(vals) if vals else None), contended_count


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
    if "nproc" in meta:
        lines.append(f"| nproc | {meta['nproc']} |")
    if "preflight_wait_s" in meta:
        lines.append(f"| preflight_wait_s | {meta['preflight_wait_s']} |")
    if "preflight_other_busy_pct" in meta:
        lines.append(f"| preflight_other_busy_pct | {meta['preflight_other_busy_pct']} |")
    max_other_busy, contended_count = contention_summary_stats(measurements)
    if max_other_busy is not None:
        lines.append(f"| max_other_busy_pct | {max_other_busy:g} |")
        lines.append(f"| contended_recordings | {contended_count} |")
    lines.append("")

    lines.append("## Thresholds")
    lines.append("")
    lines.append("| Threshold | Value |")
    lines.append("| --- | --- |")
    for key in sorted(thresholds):
        lines.append(f"| {key} | {thresholds[key]} |")
    lines.append("")

    total_fail = 0
    total_inconclusive = 0
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
                elif row.result == "INCONCLUSIVE":
                    total_inconclusive += 1
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
        summary = f"Result: FAIL ({total_fail} failed, {total_inconclusive} inconclusive)"
        exit_code = 1
    elif total_inconclusive:
        summary = f"Result: INCONCLUSIVE ({total_inconclusive} inconclusive)"
        exit_code = 7
    else:
        summary = "Result: PASS"
        exit_code = 0
    lines.append(f"**{summary}**")
    lines.append("")

    lines.append("## Caveats")
    lines.append("")
    lines.append("- Synthetic mic (pw-loopback), not a physical microphone or hardware clock.")
    lines.append("- No perceptual (subjective quality) judgement is made here.")
    lines.append("- The monitor path is covered only when run with `--monitor-null-sink`.")
    lines.append("- Exit 7 (INCONCLUSIVE) means the machine never got quiet, or a scheduling-sensitive")
    lines.append("  metric only failed under contention/host-starvation -- not a confirmed defect.")
    lines.append("")

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
    rows = evaluate_stress(scanned, args.app_alive == "yes", args.load_start, args.recovery_max_s, args.recovery_min_s)
    for r in rows:
        r["scenario"] = args.scenario
        r["recording"] = args.recording
    with open(args.json_out, "w", encoding="utf-8") as fh:
        json.dump(rows, fh, indent=2)
    return 0


def cmd_spike_check(args: argparse.Namespace) -> int:
    with open(args.logscan, encoding="utf-8") as fh:
        scanned = json.load(fh)
    rows = evaluate_spike(scanned, args.app_alive == "yes", args.launched_engine, args.contended)
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


def cmd_swap_attribution(args: argparse.Namespace) -> int:
    with open(args.logscan, encoding="utf-8") as fh:
        scanned = json.load(fh)
    x, fs = read_wav(args.wav)
    if x.shape[1] < 2:
        raise ValueError(f"expected a 2-channel recording (input, output): {args.wav}")
    inp, out = x[:, 0], x[:, 1]
    start, end = active_region(inp)
    out_active = out[start:end]
    zr = zero_runs(out_active)
    duration_s = len(out_active) / fs

    # 15 (engine, mode) pairs, in order -- the ENGINE swaps only (matches
    # `swap-check`'s --expected; scenario_swaps' swaps.expected file).
    engine_pairs = [tuple(p.split(":", 1)) for p in args.expected.split(",") if p]
    swap_wavs = [epoch_to_wav(t, args.rec_link_epoch, args.source_onset_s) for t in scanned.get("swap_started_at", [])]
    swap_events: list[tuple[float, str, str]] = [
        (t_wav, engine, mode)
        for t_wav, (engine, mode) in zip(swap_wavs, engine_pairs)
        if 0 <= t_wav <= duration_s
    ]

    # Mode changes: --mode-change-after N means "after N completed engine
    # swaps" (1-indexed) -- the engine active at that point is engine_pairs
    # [N-1][0]; the target mode itself comes straight from the log line.
    mode_events = scanned.get("mode_set_at", [])
    for (t_epoch, logged_mode), after_n in zip(mode_events, args.mode_change_after):
        t_wav = epoch_to_wav(t_epoch, args.rec_link_epoch, args.source_onset_s)
        if 0 <= t_wav <= duration_s and 1 <= after_n <= len(engine_pairs):
            swap_events.append((t_wav, engine_pairs[after_n - 1][0], logged_mode))

    dfn_restart_t_wav = [epoch_to_wav(t, args.rec_link_epoch, args.source_onset_s) for t in scanned.get("dfn_restart_at", [])]
    dfn_shed_t_wav = [epoch_to_wav(t, args.rec_link_epoch, args.source_onset_s) for t, _ms in scanned.get("dfn_shed_at", [])]

    hole_attribution = None
    if args.measured_json:
        with open(args.measured_json, encoding="utf-8") as fh:
            hole_attribution = json.load(fh).get("hole_attribution")

    attributed = swap_attribution(zr, fs, swap_events, dfn_restart_t_wav, dfn_shed_t_wav, hole_attribution=hole_attribution)
    summary = swap_attribution_summary(attributed)
    result = {
        "scenario": args.scenario,
        "recording": args.recording,
        "kind": "swap_attribution",
        "swaps_in_speech": len(swap_events),
        "rows": attributed,
        **summary,
    }
    with open(args.json_out, "w", encoding="utf-8") as fh:
        json.dump(result, fh, indent=2)
    return 0


def cmd_attempts(args: argparse.Namespace) -> int:
    """Fold ATTEMPT.json (one `measure`d recording per attempt taken so far,
    in order) into a decide_attempts() verdict, write the `aggregate` JSON,
    and print "done" (stop -- the aggregate is ready to add to the report)
    or "again" (record one more attempt and re-run this command with it
    appended)."""
    thresholds = {k: _coerce_threshold_value(v) for k, v in _parse_kv_list(args.threshold or []).items()}
    attempts: list[dict[str, dict[str, Any]]] = []
    for path in args.attempt_json:
        with open(path, encoding="utf-8") as fh:
            measured = json.load(fh)
        rows = _rule_rows(measured, thresholds)
        contended = bool(measured.get("contention", {}).get("contended"))
        entry: dict[str, dict[str, Any]] = {}
        for row in rows:
            if row.result not in ("PASS", "FAIL"):
                continue  # INFO rows carry no pass/fail verdict to retry on
            host_starved = False
            if row.metric == "holes" and row.result == "FAIL":
                classes = measured.get("hole_classes") or []
                host_starved = bool(classes) and all(c == "host_starved" for c in classes)
            entry[row.metric] = {
                "value": row.value,
                "result": row.result,
                "sched_sensitive": is_sched_sensitive_metric(row.metric, measured),
                "contended": contended,
                "host_starved": host_starved,
            }
        attempts.append(entry)

    rows, needs_more = decide_attempts(attempts, args.max_attempts)
    result: dict[str, Any] = {
        "scenario": args.scenario,
        "recording": args.recording,
        "kind": "aggregate",
        "attempts_used": len(attempts),
        "max_attempts": args.max_attempts,
        "metrics": rows,
    }
    result.update(_parse_kv_list(args.meta or []))
    with open(args.json_out, "w", encoding="utf-8") as fh:
        json.dump(result, fh, indent=2)
    print("again" if needs_more else "done")
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
    p_measure.add_argument("--contention-samples", default=None, help="contention.py `run` JSONL for this recording")
    p_measure.add_argument("--rec-link-epoch", type=float, default=None)
    p_measure.add_argument("--source-onset-s", type=float, default=0.0)
    p_measure.add_argument("--contended-other-busy-pct", type=float, default=None)
    p_measure.add_argument("--contended-steal-pct", type=float, default=2.0)
    p_measure.add_argument("--contended-iowait-pct", type=float, default=10.0)
    p_measure.add_argument("--audio-wait-starved-ms", type=float, default=5.0)

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
    p_check.add_argument("--result", required=True, choices=["PASS", "FAIL", "INFO", "SKIP", "INCONCLUSIVE"])
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
    p_stress.add_argument("--recovery-min-s", type=float, default=0.0)
    p_stress.add_argument("--scenario", required=True)
    p_stress.add_argument("--recording", required=True)
    p_stress.add_argument("--json-out", required=True)

    p_spike = sub.add_parser("spike-check")
    p_spike.add_argument("--logscan", required=True)
    p_spike.add_argument("--app-alive", required=True, choices=["yes", "no"])
    p_spike.add_argument("--launched-engine", required=True)
    p_spike.add_argument("--contended", action="store_true", help="the load recording's own contention verdict (R5)")
    p_spike.add_argument("--scenario", required=True)
    p_spike.add_argument("--recording", required=True)
    p_spike.add_argument("--json-out", required=True)

    p_swapattr = sub.add_parser("swap-attribution")
    p_swapattr.add_argument("wav")
    p_swapattr.add_argument("--logscan", required=True)
    p_swapattr.add_argument("--expected", required=True, help="comma-separated Engine:Mode pairs")
    p_swapattr.add_argument(
        "--mode-change-after", type=lambda s: [int(x) for x in s.split(",") if x], default=[],
        help="1-indexed counts of completed engine swaps after which a mode change happened",
    )
    p_swapattr.add_argument("--rec-link-epoch", type=float, required=True)
    p_swapattr.add_argument("--source-onset-s", type=float, default=0.0)
    p_swapattr.add_argument("--measured-json", default=None, help="the recording's own `measure`d JSON, for Task 1 host evidence on unattributed runs")
    p_swapattr.add_argument("--scenario", required=True)
    p_swapattr.add_argument("--recording", required=True)
    p_swapattr.add_argument("--json-out", required=True)

    p_active = sub.add_parser("active-engine")
    p_active.add_argument("--logscan", required=True)
    p_active.add_argument("--started-with", required=True)

    p_attempts = sub.add_parser("attempts")
    p_attempts.add_argument("--max-attempts", type=int, required=True)
    p_attempts.add_argument("--scenario", required=True)
    p_attempts.add_argument("--recording", required=True)
    p_attempts.add_argument("--threshold", action="append", default=[])
    p_attempts.add_argument("--meta", action="append", default=[])
    p_attempts.add_argument("--json-out", required=True)
    p_attempts.add_argument("attempt_json", nargs="+")

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
        if args.cmd == "spike-check":
            return cmd_spike_check(args)
        if args.cmd == "swap-attribution":
            return cmd_swap_attribution(args)
        if args.cmd == "active-engine":
            return cmd_active_engine(args)
        if args.cmd == "attempts":
            return cmd_attempts(args)
    except (OSError, ValueError) as exc:
        print(f"analyze: {exc}", file=sys.stderr)
        return 2

    parser.error(f"unknown subcommand {args.cmd!r}")
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
