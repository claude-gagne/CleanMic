#!/usr/bin/env python3
"""analyze.py -- WAV reader, latency/quality metrics and Markdown report
generator for the CleanMic silent E2E harness (scripts/e2e-audio.sh).

Pure, stdlib+numpy functions so they are unit-testable without PipeWire, a
display or any recorded audio (see test_analyze.py). The `measure` and
`report` CLI subcommands are the glue scripts/e2e-audio.sh drives.

USAGE
  analyze.py measure REC.wav --scenario S --recording R --kind K \
      [--meta k=v]... --json-out OUT.json
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


def eval_speech(measured: dict[str, Any], thresholds: dict[str, Any]) -> list[Row]:
    """Task 1 subset of the "speech" kind's rules: latency only. Task 2 adds
    repeat/holes/peak on top of this (see the extended registry below, if
    present in a later revision of this file)."""
    engine = str(measured.get("engine", ""))
    mode = str(measured.get("mode", ""))
    base_key = f"latency_max_ms_{engine.lower()}"
    base = thresholds.get(base_key, thresholds.get("latency_max_ms", 80))
    extra = 0.0
    if mode == "Balanced":
        extra = thresholds.get("latency_extra_balanced_ms", 0)
    elif mode == "LowCpu":
        extra = thresholds.get("latency_extra_lowcpu_ms", 0)
    limit = base + extra
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

    return rows


RULES_BY_KIND = {
    "speech": eval_speech,
}


def evaluate(measured: dict[str, Any], thresholds: dict[str, Any]) -> list[Row]:
    kind = measured.get("kind", "speech")
    rule = RULES_BY_KIND.get(kind, eval_speech)
    return rule(measured, thresholds)


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
    measurements = []
    for path in args.json_files:
        with open(path, encoding="utf-8") as fh:
            measurements.append(json.load(fh))
    text, exit_code = render_report(measurements, thresholds, meta, args.aborted)
    with open(args.out, "w", encoding="utf-8") as fh:
        fh.write(text)
        if not text.endswith("\n"):
            fh.write("\n")
    print(f"analyze: report -> {args.out} (exit {exit_code})")
    return exit_code


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

    args = parser.parse_args(argv)

    try:
        if args.cmd == "measure":
            return cmd_measure(args)
        if args.cmd == "report":
            return cmd_report(args)
    except (OSError, ValueError) as exc:
        print(f"analyze: {exc}", file=sys.stderr)
        return 2

    parser.error(f"unknown subcommand {args.cmd!r}")
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
