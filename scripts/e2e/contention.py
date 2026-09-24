#!/usr/bin/env python3
"""contention.py -- stdlib-only /proc contention sampling for the CleanMic
silent E2E harness (quick 260924-n4s, R5: "as long as you can run reliable
tests, that's all I care about").

WHY THIS EXISTS. scripts/e2e-audio.sh runs next to the owner's real desktop,
other agents' builds, Syncthing, Firefox and whatever else the shared
machine is doing. A PASS/FAIL verdict recorded while the host itself was
starved is not evidence of a CleanMic defect -- it's noise. This module
gives scripts/e2e-audio.sh (and scripts/e2e/analyze.py) three things:

  1. a background sampler (`run`) that records, at low overhead, the
     aggregate CPU picture plus per-thread scheduling detail for the
     harness's own app process;
  2. `summarize`, which turns those samples plus a [start, end] epoch window
     into contention metrics and a `contended` verdict with reasons;
  3. `wait-quiet`, a pre-flight gate that blocks (briefly) until the machine
     has been quiet for QUIET_WINDOWS consecutive windows, or reports it
     never got there.

Every decision lives in a pure function so it is unit-testable without root,
without a live PipeWire graph and without waiting in real time (see
test_analyze.py, which imports this module). The `run`/`wait-quiet`/
`summarize` subcommands are thin CLI wrappers over those functions.

PRIVACY (T-n4s-03): classification reads only CPU-accounting fields (comm,
ppid, session, utime/stime, schedstat) plus, for the harness-ownership
check, ONE exact line of /proc/<pid>/environ
(`CLEANMIC_HARNESS_STATE_ROOT=<root>`) -- never stored, never printed, and
an unreadable environ (EACCES, gone) is simply "no marker", never an error.
cmdline is never read at all.

USAGE
  contention.py run --harness-pid PID --state-root DIR --out FILE.jsonl \\
      --stop-file PATH [--app-pid PID] [--interval 0.1] [--scan-interval 1.0]
  contention.py summarize FILE.jsonl --start EPOCH --end EPOCH \\
      [--contended-other-busy-pct N] [--contended-steal-pct N] \\
      [--contended-iowait-pct N] [--audio-wait-starved-ms N]
  contention.py wait-quiet --max-s N --window-s N --windows N \\
      --harness-pid PID --state-root DIR [--other-busy-pct N]
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
from typing import Any, Iterable

AUDIO_DAEMON_COMMS = {"pipewire", "wireplumber", "pipewire-pulse"}
AUDIO_THREAD_COMM = "cleanmic-audio"

# ---------------------------------------------------------------------------
# Pure /proc text parsers
# ---------------------------------------------------------------------------


def parse_stat_aggregate(line: str) -> dict[str, int]:
    """Parse the aggregate `cpu  user nice system idle iowait irq softirq
    steal ...` line of /proc/stat into named jiffie counters plus derived
    `busy` (user+nice+system+irq+softirq+steal) and `total` (busy+idle+
    iowait). Missing trailing fields (guest/guest_nice, or an old kernel with
    no `steal`) default to 0."""
    parts = line.split()
    if not parts or not parts[0].startswith("cpu"):
        raise ValueError(f"not a /proc/stat aggregate line: {line!r}")
    fields = [int(x) for x in parts[1:]]
    while len(fields) < 8:
        fields.append(0)
    user, nice, system, idle, iowait, irq, softirq, steal = fields[:8]
    busy = user + nice + system + irq + softirq + steal
    return {
        "user": user,
        "nice": nice,
        "system": system,
        "idle": idle,
        "iowait": iowait,
        "irq": irq,
        "softirq": softirq,
        "steal": steal,
        "busy": busy,
        "total": busy + idle + iowait,
    }


def parse_pid_stat(text: str) -> dict[str, Any]:
    """Parse the text of /proc/<pid>/stat. `comm` may itself contain spaces
    and parentheses (a real "(Web Content)" or a pathological "(a) b)"), so
    split on the FIRST '(' and the LAST ')' -- everything between them is the
    command name, verbatim. Returns comm, ppid, session (both ints) and
    utime+stime (jiffies, this process's own CPU time)."""
    open_paren = text.index("(")
    close_paren = text.rindex(")")
    comm = text[open_paren + 1 : close_paren]
    rest = text[close_paren + 2 :].split()
    # rest[0]=state rest[1]=ppid rest[2]=pgrp rest[3]=session ...
    # rest[11]=utime rest[12]=stime (fields 14/15, 1-indexed including pid)
    ppid = int(rest[1])
    session = int(rest[3])
    utime = int(rest[11])
    stime = int(rest[12])
    return {
        "comm": comm,
        "ppid": ppid,
        "session": session,
        "utime": utime,
        "stime": stime,
        "cpu_ticks": utime + stime,
    }


def parse_schedstat(text: str) -> tuple[int, int, int]:
    """Parse /proc/<pid>/task/<tid>/schedstat: "run_ns wait_ns nr_slices" ->
    (run_ns, wait_ns, slices)."""
    parts = text.split()
    return int(parts[0]), int(parts[1]), int(parts[2])


# ---------------------------------------------------------------------------
# Pure classification
# ---------------------------------------------------------------------------


def resolve_descendants(ppid_map: dict[int, int], root_pid: int) -> set[int]:
    """Every pid whose ppid chain reaches root_pid, given a full pid->ppid
    map of the process table. root_pid itself is never included (it cannot
    be its own ancestor)."""
    children: dict[int, list[int]] = {}
    for pid, ppid in ppid_map.items():
        children.setdefault(ppid, []).append(pid)
    result: set[int] = set()
    stack = list(children.get(root_pid, []))
    while stack:
        pid = stack.pop()
        if pid in result:
            continue
        result.add(pid)
        stack.extend(children.get(pid, []))
    return result


def has_harness_marker(environ_text: str | None, state_root: str) -> bool:
    """environ_text is the NUL-joined text of /proc/<pid>/environ, or None
    when unreadable (EACCES / gone) -- that always means "no marker", never
    an error. Exact-line match only (never a substring)."""
    if environ_text is None:
        return False
    want = f"CLEANMIC_HARNESS_STATE_ROOT={state_root}"
    return want in environ_text.split("\x00")


def classify_pid(
    pid: int,
    comm: str,
    session: int,
    descendants: set[int],
    harness_session: int | None,
    xephyr_pid: int | None,
    has_marker: bool,
) -> str:
    """"harness" / "audio_daemon" / "other". A pid is harness-owned when it
    is the recorded Xephyr, a descendant of the harness (e2e-audio.sh) pid,
    shares the recorded setsid session, or carries the exact
    CLEANMIC_HARNESS_STATE_ROOT marker (covers the launched cleanmic, whose
    parent is a `dbus-run-session`/setsid chain the harness pid may not be a
    direct ancestor of on every desktop)."""
    if (
        (xephyr_pid is not None and pid == xephyr_pid)
        or pid in descendants
        or (harness_session is not None and session == harness_session)
        or has_marker
    ):
        return "harness"
    if comm in AUDIO_DAEMON_COMMS:
        return "audio_daemon"
    return "other"


# ---------------------------------------------------------------------------
# summarize: samples + [start, end] -> contention metrics
# ---------------------------------------------------------------------------

DEFAULT_THRESHOLDS = {
    "contended_other_busy_pct": 20.0,
    "contended_steal_pct": 2.0,
    "contended_iowait_pct": 10.0,
    "audio_wait_starved_ms": 5.0,
}


def _in_window(samples: list[dict[str, Any]], start: float, end: float) -> list[dict[str, Any]]:
    return [s for s in samples if start <= s["t"] <= end]


def _mean(xs: list[float]) -> float:
    return sum(xs) / len(xs) if xs else 0.0


def _p95(xs: list[float]) -> float:
    if not xs:
        return 0.0
    s = sorted(xs)
    idx = min(len(s) - 1, int(round(0.95 * (len(s) - 1))))
    return s[idx]


def summarize(
    samples: Iterable[dict[str, Any]],
    start: float,
    end: float,
    thresholds: dict[str, float] | None = None,
) -> dict[str, Any]:
    """samples is the parsed content of a `run`-produced JSONL file (a list
    of {"type": "fast"|"slow", "t": epoch, ...} dicts, see `run`'s docstring
    for their shape). Returns the fields the plan's behavior list specifies:
    busy/iowait/steal_pct, loadavg_1m_max, other/harness/audio_daemon
    busy_pct, audio_thread_cpu_pct, audio_wait_ms_total/max, app_wait_ms_max,
    top_other, contended + reasons."""
    th = dict(DEFAULT_THRESHOLDS)
    if thresholds:
        th.update(thresholds)

    all_samples = list(samples)
    fast = sorted((s for s in all_samples if s.get("type") == "fast"), key=lambda s: s["t"])
    slow = sorted((s for s in all_samples if s.get("type") == "slow"), key=lambda s: s["t"])

    fw = _in_window(fast, start, end)
    if len(fw) < 2:
        # Not enough samples strictly inside the window (e.g. a very short
        # recording): fall back to the two samples bracketing `end`.
        before_end = [s for s in fast if s["t"] <= end]
        fw = before_end[-2:] if len(before_end) >= 2 else fast[:2]
    sw = _in_window(slow, start, end)
    if len(sw) < 2:
        before_end = [s for s in slow if s["t"] <= end]
        sw = before_end[-2:] if len(before_end) >= 2 else slow[:2]

    if len(fw) >= 2:
        stat0, stat1 = fw[0]["stat"], fw[-1]["stat"]
        d_total = stat1["total"] - stat0["total"]
        busy_pct = 100.0 * (stat1["busy"] - stat0["busy"]) / d_total if d_total else 0.0
        iowait_pct = 100.0 * (stat1["iowait"] - stat0["iowait"]) / d_total if d_total else 0.0
        steal_pct = 100.0 * (stat1["steal"] - stat0["steal"]) / d_total if d_total else 0.0
    else:
        busy_pct = iowait_pct = steal_pct = 0.0
    loadavg_1m_max = max((s.get("loadavg_1m", 0.0) for s in fw), default=0.0)

    other_series: list[float] = []
    harness_series: list[float] = []
    audio_daemon_series: list[float] = []
    other_totals: dict[str, int] = {}
    for a, b in zip(sw, sw[1:]):
        d_total = b["stat"]["total"] - a["stat"]["total"]
        if d_total <= 0:
            continue
        other_series.append(100.0 * (b["classes"].get("other", 0) - a["classes"].get("other", 0)) / d_total)
        harness_series.append(100.0 * (b["classes"].get("harness", 0) - a["classes"].get("harness", 0)) / d_total)
        audio_daemon_series.append(
            100.0 * (b["classes"].get("audio_daemon", 0) - a["classes"].get("audio_daemon", 0)) / d_total
        )
        for comm, ticks in b.get("top_other", {}).items():
            prev = a.get("top_other", {}).get(comm, 0)
            other_totals[comm] = other_totals.get(comm, 0) + max(0, ticks - prev)

    other_busy_pct_mean = _mean(other_series)
    other_busy_pct_p95 = _p95(other_series)
    harness_busy_pct = _mean(harness_series)
    audio_daemon_busy_pct = _mean(audio_daemon_series)
    top_other = sorted(other_totals.items(), key=lambda kv: -kv[1])[:3]

    audio_run_ns_total = 0.0
    audio_wall_s_total = 0.0
    audio_wait_ms_values: list[float] = []
    app_wait_ms_max = 0.0
    for a, b in zip(fw, fw[1:]):
        dt = b["t"] - a["t"]
        if dt <= 0:
            continue
        a_threads = {t["tid"]: t for t in a.get("threads", [])}
        for tb in b.get("threads", []):
            ta = a_threads.get(tb["tid"])
            if ta is None:
                continue
            d_run = tb["run_ns"] - ta["run_ns"]
            d_wait_ms = (tb["wait_ns"] - ta["wait_ns"]) / 1e6
            app_wait_ms_max = max(app_wait_ms_max, d_wait_ms)
            if tb.get("comm") == AUDIO_THREAD_COMM:
                audio_run_ns_total += d_run
                audio_wall_s_total += dt
                audio_wait_ms_values.append(d_wait_ms)

    audio_thread_cpu_pct = 100.0 * (audio_run_ns_total / 1e9) / audio_wall_s_total if audio_wall_s_total else 0.0
    audio_wait_ms_total = sum(audio_wait_ms_values)
    audio_wait_ms_max = max(audio_wait_ms_values, default=0.0)

    reasons: list[str] = []
    if other_busy_pct_mean > th["contended_other_busy_pct"]:
        reasons.append(f"other_busy_pct {other_busy_pct_mean:.1f} > {th['contended_other_busy_pct']:g}")
    if steal_pct > th["contended_steal_pct"]:
        reasons.append(f"steal_pct {steal_pct:.1f} > {th['contended_steal_pct']:g}")
    if iowait_pct > th["contended_iowait_pct"]:
        reasons.append(f"iowait_pct {iowait_pct:.1f} > {th['contended_iowait_pct']:g}")
    if app_wait_ms_max >= th["audio_wait_starved_ms"]:
        reasons.append(f"app_wait_ms_max {app_wait_ms_max:.1f} >= {th['audio_wait_starved_ms']:g}")

    return {
        "busy_pct": round(busy_pct, 2),
        "iowait_pct": round(iowait_pct, 2),
        "steal_pct": round(steal_pct, 2),
        "loadavg_1m_max": round(loadavg_1m_max, 2),
        "other_busy_pct_mean": round(other_busy_pct_mean, 2),
        "other_busy_pct_p95": round(other_busy_pct_p95, 2),
        "harness_busy_pct": round(harness_busy_pct, 2),
        "audio_daemon_busy_pct": round(audio_daemon_busy_pct, 2),
        "audio_thread_cpu_pct": round(audio_thread_cpu_pct, 2),
        "audio_wait_ms_total": round(audio_wait_ms_total, 2),
        "audio_wait_ms_max": round(audio_wait_ms_max, 2),
        "app_wait_ms_max": round(app_wait_ms_max, 2),
        "top_other": [{"comm": c, "ticks": t} for c, t in top_other],
        "contended": bool(reasons),
        "reasons": reasons,
    }


# ---------------------------------------------------------------------------
# wait-quiet: pure decision + a thin real-time CLI loop
# ---------------------------------------------------------------------------


def decide_quiet(window_values: list[float], limit: float, windows_needed: int) -> tuple[bool, int, float, float]:
    """window_values is the sequence of per-window other_busy_pct
    observations, in order. `windows_needed` CONSECUTIVE windows at or below
    `limit` means quiet; one window above resets the streak. Returns
    (quiet, best_streak_reached, lowest_observed, highest_observed)."""
    streak = 0
    best_streak = 0
    lowest = min(window_values) if window_values else 0.0
    highest = max(window_values) if window_values else 0.0
    for v in window_values:
        if v <= limit:
            streak += 1
        else:
            streak = 0
        best_streak = max(best_streak, streak)
        if streak >= windows_needed:
            return True, streak, lowest, highest
    return False, best_streak, lowest, highest


# ---------------------------------------------------------------------------
# Live /proc reading helpers (used only by the `run`/`wait-quiet` CLIs)
# ---------------------------------------------------------------------------


def _read_text(path: str) -> str | None:
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as fh:
            return fh.read()
    except OSError:
        return None


def _read_environ(pid: int) -> str | None:
    try:
        with open(f"/proc/{pid}/environ", "rb") as fh:
            return fh.read().decode("utf-8", errors="replace")
    except OSError:
        return None


def _all_pids() -> list[int]:
    out = []
    for name in os.listdir("/proc"):
        if name.isdigit():
            out.append(int(name))
    return out


def _live_classification(harness_pid: int, state_root: str, xephyr_pid: int | None) -> tuple[set[int], int | None]:
    """Returns (descendants, harness_session) computed once per sample pass
    from a fresh /proc scan -- deliberately NOT cached across samples, since
    the harness spawns/reaps helper processes throughout a run."""
    ppid_map: dict[int, int] = {}
    harness_session: int | None = None
    for pid in _all_pids():
        text = _read_text(f"/proc/{pid}/stat")
        if not text:
            continue
        try:
            info = parse_pid_stat(text)
        except (ValueError, IndexError):
            continue
        ppid_map[pid] = info["ppid"]
        if pid == harness_pid:
            harness_session = info["session"]
    descendants = resolve_descendants(ppid_map, harness_pid)
    return descendants, harness_session


def _classify_live(pid: int, comm: str, session: int, descendants: set[int], harness_session: int | None, xephyr_pid: int | None, state_root: str) -> str:
    marker = has_harness_marker(_read_environ(pid), state_root)
    return classify_pid(pid, comm, session, descendants, harness_session, xephyr_pid, marker)


def _sample_fast(app_pid: int | None) -> dict[str, Any]:
    stat_text = _read_text("/proc/stat") or "cpu 0 0 0 0 0 0 0 0"
    first_line = stat_text.splitlines()[0]
    stat = parse_stat_aggregate(first_line)
    loadavg_text = _read_text("/proc/loadavg") or "0 0 0"
    try:
        loadavg_1m = float(loadavg_text.split()[0])
    except (ValueError, IndexError):
        loadavg_1m = 0.0
    threads: list[dict[str, Any]] = []
    if app_pid is not None:
        task_dir = f"/proc/{app_pid}/task"
        try:
            tids = os.listdir(task_dir)
        except OSError:
            tids = []
        for tid_s in tids:
            comm = (_read_text(f"{task_dir}/{tid_s}/comm") or "").strip()
            sched_text = _read_text(f"{task_dir}/{tid_s}/schedstat")
            if not sched_text:
                continue
            try:
                run_ns, wait_ns, _slices = parse_schedstat(sched_text)
            except (ValueError, IndexError):
                continue
            threads.append({"tid": int(tid_s), "comm": comm, "run_ns": run_ns, "wait_ns": wait_ns})
    return {"type": "fast", "t": time.time(), "stat": stat, "loadavg_1m": loadavg_1m, "threads": threads}


def _sample_slow(harness_pid: int, state_root: str, xephyr_pid: int | None) -> dict[str, Any]:
    stat_text = _read_text("/proc/stat") or "cpu 0 0 0 0 0 0 0 0"
    stat = parse_stat_aggregate(stat_text.splitlines()[0])
    descendants, harness_session = _live_classification(harness_pid, state_root, xephyr_pid)
    classes = {"harness": 0, "audio_daemon": 0, "other": 0}
    other_ticks: dict[str, int] = {}
    for pid in _all_pids():
        text = _read_text(f"/proc/{pid}/stat")
        if not text:
            continue
        try:
            info = parse_pid_stat(text)
        except (ValueError, IndexError):
            continue
        cls = _classify_live(pid, info["comm"], info["session"], descendants, harness_session, xephyr_pid, state_root)
        classes[cls] += info["cpu_ticks"]
        if cls == "other":
            other_ticks[info["comm"]] = other_ticks.get(info["comm"], 0) + info["cpu_ticks"]
    top_other = dict(sorted(other_ticks.items(), key=lambda kv: -kv[1])[:3])
    return {"type": "slow", "t": time.time(), "stat": stat, "classes": classes, "top_other": top_other}


def cmd_run(args: argparse.Namespace) -> int:
    xephyr_pid: int | None = None
    if args.display_dir:
        text = _read_text(os.path.join(args.display_dir, "xephyr.pid"))
        if text:
            try:
                xephyr_pid = int(text.strip())
            except ValueError:
                xephyr_pid = None
    last_slow = 0.0
    with open(args.out, "a", encoding="utf-8") as fh:
        while True:
            if os.path.exists(args.stop_file):
                break
            if not os.path.exists(f"/proc/{os.getppid()}"):
                break
            now = time.time()
            try:
                fh.write(json.dumps(_sample_fast(args.app_pid)) + "\n")
            except OSError:
                pass
            if now - last_slow >= args.scan_interval:
                last_slow = now
                try:
                    fh.write(json.dumps(_sample_slow(args.harness_pid, args.state_root, xephyr_pid)) + "\n")
                except OSError:
                    pass
            fh.flush()
            time.sleep(args.interval)
    return 0


def cmd_summarize(args: argparse.Namespace) -> int:
    samples: list[dict[str, Any]] = []
    with open(args.samples, encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                samples.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    thresholds = {
        "contended_other_busy_pct": args.contended_other_busy_pct,
        "contended_steal_pct": args.contended_steal_pct,
        "contended_iowait_pct": args.contended_iowait_pct,
        "audio_wait_starved_ms": args.audio_wait_starved_ms,
    }
    result = summarize(samples, args.start, args.end, thresholds)
    if args.json_out:
        with open(args.json_out, "w", encoding="utf-8") as fh:
            json.dump(result, fh, indent=2)
    else:
        print(json.dumps(result, indent=2))
    return 0


def cmd_wait_quiet(args: argparse.Namespace) -> int:
    deadline = time.time() + args.max_s
    values: list[float] = []
    xephyr_pid: int | None = None
    if args.display_dir:
        text = _read_text(os.path.join(args.display_dir, "xephyr.pid"))
        if text:
            try:
                xephyr_pid = int(text.strip())
            except ValueError:
                xephyr_pid = None
    while time.time() < deadline:
        a = _sample_slow(args.harness_pid, args.state_root, xephyr_pid)
        time.sleep(args.window_s)
        b = _sample_slow(args.harness_pid, args.state_root, xephyr_pid)
        d_total = b["stat"]["total"] - a["stat"]["total"]
        pct = 100.0 * (b["classes"].get("other", 0) - a["classes"].get("other", 0)) / d_total if d_total else 0.0
        values.append(pct)
        quiet, streak, lowest, highest = decide_quiet(values, args.other_busy_pct, args.windows)
        if quiet:
            print(f"contention: quiet after {len(values)} window(s) (range {lowest:.1f}-{highest:.1f}%)")
            return 0
    _, _streak, lowest, highest = decide_quiet(values, args.other_busy_pct, args.windows)
    print(f"contention: never quiet within {args.max_s:g}s (observed {lowest:.1f}-{highest:.1f}%)", file=sys.stderr)
    return 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_run = sub.add_parser("run")
    p_run.add_argument("--harness-pid", type=int, required=True)
    p_run.add_argument("--state-root", required=True)
    p_run.add_argument("--display-dir", default=None)
    p_run.add_argument("--app-pid", type=int, default=None)
    p_run.add_argument("--interval", type=float, default=0.1)
    p_run.add_argument("--scan-interval", type=float, default=1.0)
    p_run.add_argument("--out", required=True)
    p_run.add_argument("--stop-file", required=True)

    p_sum = sub.add_parser("summarize")
    p_sum.add_argument("samples")
    p_sum.add_argument("--start", type=float, required=True)
    p_sum.add_argument("--end", type=float, required=True)
    p_sum.add_argument("--contended-other-busy-pct", type=float, default=DEFAULT_THRESHOLDS["contended_other_busy_pct"])
    p_sum.add_argument("--contended-steal-pct", type=float, default=DEFAULT_THRESHOLDS["contended_steal_pct"])
    p_sum.add_argument("--contended-iowait-pct", type=float, default=DEFAULT_THRESHOLDS["contended_iowait_pct"])
    p_sum.add_argument("--audio-wait-starved-ms", type=float, default=DEFAULT_THRESHOLDS["audio_wait_starved_ms"])
    p_sum.add_argument("--json-out", default=None)

    p_wq = sub.add_parser("wait-quiet")
    p_wq.add_argument("--max-s", type=float, required=True)
    p_wq.add_argument("--window-s", type=float, required=True)
    p_wq.add_argument("--windows", type=int, required=True)
    p_wq.add_argument("--other-busy-pct", type=float, required=True)
    p_wq.add_argument("--harness-pid", type=int, required=True)
    p_wq.add_argument("--state-root", required=True)
    p_wq.add_argument("--display-dir", default=None)

    args = parser.parse_args(argv)
    if args.cmd == "run":
        return cmd_run(args)
    if args.cmd == "summarize":
        return cmd_summarize(args)
    if args.cmd == "wait-quiet":
        return cmd_wait_quiet(args)
    parser.error(f"unknown subcommand {args.cmd!r}")
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
