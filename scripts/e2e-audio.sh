#!/usr/bin/env bash
# e2e-audio.sh -- silent virtual-mic end-to-end audio test for CleanMic.
#
# WHY THIS EXISTS. The 2026-09-24 base-latency debug session (see
# .planning/debug/resolved/base-latency-330ms.md) proved CleanMic's
# mic-to-virtual-source latency with a hand-built rig, then discovered mid-
# session that its FIRST design (an Audio/Sink test node) was not actually
# silent: gnome-remote-desktop mirrors every Audio/Sink to a connected RDP
# client. This script is the maintained, repo-owned replacement: it drives
# scripts/nested-run.sh to launch CleanMic in isolation, wires an RDP-safe
# virtual mic through PipeWire, plays known signals through it, records the
# result, and grades the result against one thresholds block below. Every
# future latency/engine-swap/decimation/DC/auto-gain fix should be checkable
# with `make e2e-audio` -- without sound, without touching the owner's
# config, and without ever touching their real CleanMic.
#
# USAGE
#   scripts/e2e-audio.sh [options] <baseline|swaps|toggle|modes|dc|autogain|stress|monitor|all>...
#
# OPTIONS
#   --out DIR             must not exist, or must be empty (default:
#                          target/e2e-audio/<UTC timestamp>)
#   --display :N          nested display (default $E2E_DISPLAY)
#   --lang fr|en           UI language (default $E2E_LANG)
#   --appimage PATH        use this AppImage
#   --binary PATH          use this raw binary
#   --latency-max-ms N     override LATENCY_MAX_MS for this run
#   --monitor-null-sink    required to add the `monitor` scenario
#
# EXIT CODES
#   0  every metric PASSed
#   1  at least one metric FAILed (report was still written)
#   2  bad usage
#   3  missing prerequisite: a tool, numpy, a demo asset, or the binary
#   4  unsafe or busy environment -- refused BEFORE any test audio played
#   5  harness/driver error: nested-run.sh failed, an action was not
#      confirmed, a recording came back empty, or the app died
#   6  cleanup was left incomplete -- this OVERRIDES every other code
#
# SAFETY: see scripts/e2e/README.md. In short -- no Audio/Sink node by
# default, a pw-dump link audit before AND after every playback link, private
# XDG homes, and a refusal (never a kill) if any cleanmic/Xephyr/cmtest_* node
# already exists when this script starts.

set -euo pipefail

SCRIPT_DIR="$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
NESTED_RUN="$SCRIPT_DIR/nested-run.sh"
E2E_DIR="$SCRIPT_DIR/e2e"

log() { echo "[e2e] $*"; }
err() { echo "[e2e] $*" >&2; }

# ---------------------------------------------------------------------------
# TOP CONFIG BLOCK -- every threshold lives here, one rationale line each,
# all env-overridable. NEVER raise a threshold just to make a run pass.
# ---------------------------------------------------------------------------

# Generic fallback for any engine without its own LATENCY_MAX_MS_<ENGINE>
# override below. .planning/debug/resolved/base-latency-330ms.md's
# post-fix E2E measured DPDFNet-2 MaxQuality at 82-98 ms (was 314-336 ms
# pre-fix); this adds margin for the envelope estimator's own +/-5 ms and
# graph-quantum jitter.
: "${LATENCY_MAX_MS:=120}"
# Per-engine overrides, all measured post-fix (29f5397) on 2026-09-24 at
# q1024, MaxQuality, fresh launch -- see the debug file's Evidence section.
# RNNoise: 41-48 ms on the base-latency session's hand-built rig, but THIS
# harness (pw-loopback test mic, Xephyr) measures 63-71 ms on a quiet machine
# with the pre- and post-dfn-panic-under-load binaries alike (8 runs,
# 2026-09-24); 70 sat inside that distribution and failed at random.
# Max measured + 9 ms (estimator +/-5 ms, quantum jitter).
: "${LATENCY_MAX_MS_RNNOISE:=80}"
: "${LATENCY_MAX_MS_DPDFNET2:=120}"         # measured 82-98 ms (algorithmic delay ~50 ms + quanta)
: "${LATENCY_MAX_MS_DPDFNET8:=120}"         # measured 90-100 ms
# DeepFilterNet (vendored LADSPA plugin): every plugin "underrun" adds 10 ms
# it never sheds; a quiet launch in this harness takes 4-6 of them during
# the window-build CPU spike, so quiet runs measure 106-128 ms (8 runs,
# 2026-09-24; ~65 ms + 10 ms per underrun). The underrun guard
# (src/engine/deepfilter.rs) restarts the plugin before an 8th, so the
# plugin can add at most 70 ms: ~65 + 70 = 135 -> 140.
: "${LATENCY_MAX_MS_DEEPFILTERNET:=140}"
# Balanced/LowCpu allowance on top of the engine's MaxQuality threshold.
# Derived from src/engine/dpdfnet.rs's decimation ratios (Balanced=2,
# LowCpu=4): 4*(ratio-1) hops of 10 ms, plus one hop -- Balanced
# 4*1*10+10=50 ms, LowCpu 4*3*10+10=130 ms. Measured deltas were +32..+40 ms
# and +127..+135 ms; these keep a little headroom over the derived value
# without loosening past the measurement.
: "${LATENCY_EXTRA_BALANCED_MS:=40}"
: "${LATENCY_EXTRA_LOWCPU_MS:=140}"
: "${LAG_CORR_MIN:=0.5}"          # below this the envelope xcorr peak is noise, not a real lag
: "${LATENCY_DRIFT_MAX_MS:=15}"   # max spread across 5 s windows within one recording
: "${PEAK_MAX:=0.98}"             # clipping guard on the processed output

# Monitor path (Task 2, --monitor-null-sink only): measured 70-80 ms post-fix
# (was 319 ms pre-fix). NOT `LATENCY_MAX_MS + 40` here -- the measured value
# already sits comfortably under the generic ceiling; a flat, directly-cited
# number is clearer than a formula that happens to also work.
: "${MONITOR_LATENCY_MAX_MS:=110}"

: "${REPEAT_FRAC_MAX:=0.001}"              # decimated-mode "held frame" bug signature; near-zero in healthy audio
: "${HOLES_MAX:=0}"                        # brief silent gaps surrounded by loud audio: never expected
# ...except DeepFilterNet's own: the vendored plugin appends one 10 ms block
# of zeros per underrun (upstream ladspa/src/lib.rs), and the underrun guard
# allows at most 7 per plugin instance before restarting it (which adds one
# prefill block). Quiet runs measured 0-3 (2026-09-24). A per-engine
# HOLES_MAX_<ENGINE> overrides HOLES_MAX for that engine's recordings.
: "${HOLES_MAX_DEEPFILTERNET:=8}"
# Longest stretch of digital silence (< -100 dBFS out) while the mic carried
# speech (> -35 dBFS in), speech pauses skipped: the "dead virtual mic"
# signature. The 2026-09-24 DeepFilterNet crash scored 13150 ms; a healthy
# quiet run scores 0-10 ms (an isolated plugin gap).
: "${DEAD_RUN_MAX_MS:=200}"
: "${SWAP_ZERO_MS_PER_SWAP_MAX:=20}"       # a known ~10 ms silent block per engine swap (crossfade), times headroom
: "${OUT_DC_MAX:=0.001}"                   # the DC blocker should remove essentially all offset
: "${SILENCE_OUT_MAX_DB:=-60}"             # processed silence should stay near the noise floor
: "${AUTOGAIN_MIN_BOOST_DB:=10}"           # auto-gain must audibly help a -40 dBFS mic
: "${AUTOGAIN_OFF_MAX_DEV_DB:=3}"          # auto-gain OFF should be near-unity gain
: "${AUTOGAIN_NOISE_MAX_DIFF_DB:=3}"       # auto-gain must not audibly pump steady noise (pink, no speech gate trigger)
: "${LOG_FELL_BEHIND_MAX:=0}"
: "${LOG_ERROR_MAX:=0}"
: "${LOG_PANIC_MAX:=0}"
: "${BASELINE_ENGINES:=Dpdfnet2 Dpdfnet8 DeepFilterNet RNNoise}"

# Load flag (every scenario): a recording whose average CPU busy share (all
# cores, /proc/stat user+nice+system+irq+softirq+steal) exceeds this is
# flagged "ran under load" in the report. The 2026-09-24 run in which the
# DeepFilterNet plugin aborted the app had a ~3.7 load average on 12
# threads (~30 %); quiet runs sit well below.
: "${LOAD_FLAG_BUSY_PCT:=25}"

# `stress` scenario (debug session dfn-panic-under-load): controlled
# synthetic CPU load. EVERY thread of the harness's own CleanMic is pinned
# (taskset -a) to ONE CPU, next to STRESS_SPINNERS busy loops pinned there
# too -- deterministic starvation, unlike an unpinned `stress-ng`, whose
# effect depends on the scheduler and the core count. The unguarded plugin
# aborted within ~4 s with 12 spinners and accumulated +860 ms with 6
# (examples/probe_dfn_overload.rs). Spinners are killed and the app's
# affinity restored on every exit path (on_exit).
: "${STRESS_ENGINES:=DeepFilterNet}"
: "${STRESS_SPINNERS:=6}"
: "${STRESS_CPU:=}"                # default: the last CPU
: "${STRESS_SETTLE_S:=3}"          # let the startup/window-build spike pass before loading
# While loaded, the mic may never go dead for longer than this (longest run
# of speech-active 10 ms frames whose output is below -70 dBFS). A guarded
# DeepFilterNet inserts isolated 10 ms plugin gaps before it hands over; the
# pre-fix crash produced a 15 s dead run.
: "${STRESS_DEAD_RUN_MAX_MS:=200}"
# From load start to the logged "Engine fallback: X -> Y", when one happens.
# The guard gives up ~1 s after sustained overload (tests/deepfilter_overload.rs);
# the GTK main loop is starved too, so allow several seconds.
: "${STRESS_RECOVERY_MAX_S:=10}"
# Under deliberate load the audio thread itself may fall behind: informational.
: "${STRESS_LOG_FELL_BEHIND_MAX:=1000000}"

: "${E2E_DISPLAY:=:47}"
: "${E2E_LANG:=fr}"

KNOWN_SCENARIOS="baseline swaps toggle modes dc autogain stress monitor all"

# ---------------------------------------------------------------------------
# Option parsing
# ---------------------------------------------------------------------------

OUT=""
DISPLAY_ARG="$E2E_DISPLAY"
LANG_ARG="$E2E_LANG"
APPIMAGE=""
BINARY=""
MONITOR_NULL_SINK=0
declare -a SCENARIOS=()

while [ "$#" -gt 0 ]; do
  case "$1" in
    --out) OUT="$2"; shift 2 ;;
    --display) DISPLAY_ARG="$2"; shift 2 ;;
    --lang) LANG_ARG="$2"; shift 2 ;;
    --appimage) APPIMAGE="$2"; shift 2 ;;
    --binary) BINARY="$2"; shift 2 ;;
    --latency-max-ms) LATENCY_MAX_MS="$2"; shift 2 ;;
    --monitor-null-sink) MONITOR_NULL_SINK=1; shift ;;
    -h | --help)
      sed -n '2,45p' "$0"
      exit 0
      ;;
    baseline | swaps | toggle | modes | dc | autogain | stress | monitor | all)
      SCENARIOS+=("$1"); shift
      ;;
    *)
      err "unknown option or scenario: '$1'. Known scenarios: $KNOWN_SCENARIOS"
      exit 2
      ;;
  esac
done

if [ "${#SCENARIOS[@]}" -eq 0 ]; then
  err "usage: e2e-audio.sh [options] <${KNOWN_SCENARIOS// /|}>..."
  exit 2
fi
for s in "${SCENARIOS[@]}"; do
  if [ "$s" = "monitor" ] && [ "$MONITOR_NULL_SINK" != 1 ]; then
    err "the 'monitor' scenario requires --monitor-null-sink."
    exit 2
  fi
done

# --appimage/--binary must reach EVERY `nested-run.sh launch`: without this
# they only labelled the report, while nested-run launched the newest
# build/CleanMic-*.AppImage (found during dfn-panic-under-load, when an A/B
# run of a pre-fix AppImage silently ran the fixed one).
declare -a BIN_ARGS=()
if [ -n "$BINARY" ]; then
  BIN_ARGS=(--binary "$BINARY")
elif [ -n "$APPIMAGE" ]; then
  BIN_ARGS=(--appimage "$APPIMAGE")
fi

if [ -z "$OUT" ]; then
  OUT="$REPO_ROOT/target/e2e-audio/$(date -u +%Y%m%dT%H%M%SZ)"
fi
if [ -e "$OUT" ] && [ -n "$(ls -A "$OUT" 2>/dev/null || true)" ]; then
  err "--out '$OUT' already exists and is not empty."
  exit 2
fi
mkdir -p "$OUT"/{signals,rec,logs,shots}

export CLEANMIC_HARNESS_STATE="$OUT/harness"

# ---------------------------------------------------------------------------
# Report accumulation (declared early: render_final_report below needs these
# populated before the EXIT trap can possibly fire).
# ---------------------------------------------------------------------------

declare -a MEASURED_JSON=()
declare -a THRESHOLD_ARGS=(
  --threshold "latency_max_ms=$LATENCY_MAX_MS"
  --threshold "latency_max_ms_rnnoise=$LATENCY_MAX_MS_RNNOISE"
  --threshold "latency_max_ms_dpdfnet2=$LATENCY_MAX_MS_DPDFNET2"
  --threshold "latency_max_ms_dpdfnet8=$LATENCY_MAX_MS_DPDFNET8"
  --threshold "latency_max_ms_deepfilternet=$LATENCY_MAX_MS_DEEPFILTERNET"
  --threshold "latency_extra_balanced_ms=$LATENCY_EXTRA_BALANCED_MS"
  --threshold "latency_extra_lowcpu_ms=$LATENCY_EXTRA_LOWCPU_MS"
  --threshold "lag_corr_min=$LAG_CORR_MIN"
  --threshold "latency_drift_max_ms=$LATENCY_DRIFT_MAX_MS"
  --threshold "peak_max=$PEAK_MAX"
  --threshold "monitor_latency_max_ms=$MONITOR_LATENCY_MAX_MS"
  --threshold "repeat_frac_max=$REPEAT_FRAC_MAX"
  --threshold "holes_max=$HOLES_MAX"
  --threshold "holes_max_deepfilternet=$HOLES_MAX_DEEPFILTERNET"
  --threshold "dead_run_max_ms=$DEAD_RUN_MAX_MS"
  --threshold "out_dc_max=$OUT_DC_MAX"
  --threshold "silence_out_max_db=$SILENCE_OUT_MAX_DB"
  --threshold "autogain_min_boost_db=$AUTOGAIN_MIN_BOOST_DB"
  --threshold "autogain_off_max_dev_db=$AUTOGAIN_OFF_MAX_DEV_DB"
  --threshold "autogain_noise_max_diff_db=$AUTOGAIN_NOISE_MAX_DIFF_DB"
  --threshold "load_flag_busy_pct=$LOAD_FLAG_BUSY_PCT"
  --threshold "stress_dead_run_max_ms=$STRESS_DEAD_RUN_MAX_MS"
  --threshold "stress_recovery_max_s=$STRESS_RECOVERY_MAX_S"
)
declare -a META_ARGS=()

REMOTE_DESKTOP_SESSION="no"

build_environment_meta() {
  local bin="$1" sha mtime head dirty
  sha="$(sha256sum "$bin" 2>/dev/null | cut -c1-12 || echo unknown)"
  mtime="$(stat -c %y "$bin" 2>/dev/null || echo unknown)"
  head="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  dirty="clean"
  git -C "$REPO_ROOT" diff --quiet 2>/dev/null || dirty="dirty"
  # Built as an ARRAY, not printed for `$(...)` word-splitting: mtime
  # ("2026-09-24 09:23:52 +0000") and similar values contain spaces that
  # word-splitting would silently break into extra, misaligned arguments.
  META_ARGS=(
    --meta "binary=$bin" --meta "sha256_12=$sha" --meta "mtime=$mtime"
    --meta "git_head=$head" --meta "git_dirty=$dirty" --meta "display=$DISPLAY_ARG"
    --meta "lang=$LANG_ARG" --meta "remote_desktop_session=$REMOTE_DESKTOP_SESSION"
  )
}

cmtest_node_count() {
  pw-dump 2>/dev/null | python3 "$E2E_DIR/pwgraph.py" count --prefix cmtest_ 2>/dev/null || echo 0
}

# ---------------------------------------------------------------------------
# TRAP FIRST -- idempotent cleanup for every exit path (preflight refusals,
# a mid-scenario abort(), Ctrl-C/TERM, or a clean finish all go through this).
# ---------------------------------------------------------------------------

LOOP_PID=""
LOOP2_PID=""
RECORDER_PID=""
PLAYER_PID=""
ABORT_CODE=""
ABORT_MESSAGE=""
CLEANUP_DONE=0

# Renders $OUT/report.md from whatever MEASURED_JSON has accumulated so far,
# with an "Aborted" section when ABORT_MESSAGE is set. Called from on_exit so
# EVERY exit path gets a real report, not just runs that reached the end of
# `main` -- an abort mid-scenario used to skip report generation entirely.
render_final_report() {
  local bin_used report_rc=0
  # Same precedence as nested-run.sh launch: --binary, --appimage, newest build.
  bin_used="${BINARY:-${APPIMAGE:-$(ls -t "$REPO_ROOT"/build/CleanMic-*.AppImage 2>/dev/null | head -1 || true)}}"
  build_environment_meta "$bin_used"
  local -a aborted_arg=()
  [ -n "$ABORT_MESSAGE" ] && aborted_arg=(--aborted "$ABORT_MESSAGE")
  # analyze.py's own "analyze: report -> ... (exit N)" line goes to /dev/null
  # here -- NOT to this function's stdout, which the caller captures via
  # `report_rc="$(render_final_report)"` and treats as a single integer.
  # Leaving it un-redirected made that capture two lines ("analyze: report
  # ...(exit 1)\n1"), and the later `exit "$report_rc"` died with bash's
  # "numeric argument required".
  python3 "$E2E_DIR/analyze.py" report --out "$OUT/report.md" \
    "${THRESHOLD_ARGS[@]}" "${META_ARGS[@]}" "${aborted_arg[@]}" \
    "${MEASURED_JSON[@]}" >/dev/null || report_rc=$?
  echo "$report_rc"
}

on_exit() {
  local rc=$?
  set +e
  if [ "$CLEANUP_DONE" = 1 ]; then
    return
  fi
  CLEANUP_DONE=1
  stop_cpu_load
  [ -n "$RECORDER_PID" ] && kill -INT "$RECORDER_PID" 2>/dev/null
  [ -n "$PLAYER_PID" ] && kill -TERM "$PLAYER_PID" 2>/dev/null
  sleep 0.3
  bash "$NESTED_RUN" stop "$DISPLAY_ARG" >>"$OUT/logs/nested-run-stop.log" 2>&1
  if [ -n "$LOOP_PID" ] && [ "$(cat "/proc/$LOOP_PID/comm" 2>/dev/null || true)" = "pw-loopback" ]; then
    kill "$LOOP_PID" 2>/dev/null
  fi
  if [ -n "$LOOP2_PID" ] && [ "$(cat "/proc/$LOOP2_PID/comm" 2>/dev/null || true)" = "pw-loopback" ]; then
    kill "$LOOP2_PID" 2>/dev/null
  fi

  local waited=0 clean=0
  while [ "$waited" -lt 50 ]; do
    local n; n="$(cmtest_node_count)"
    if [ "${n:-1}" = 0 ]; then
      clean=1
      break
    fi
    sleep 0.1
    waited=$((waited + 1))
  done

  local report_rc; report_rc="$(render_final_report)"
  log "report: $OUT/report.md (exit $report_rc)"

  {
    if [ "$clean" = 1 ]; then
      echo "Cleanup: complete (0 cmtest nodes, harness stopped)"
    else
      echo "Cleanup: INCOMPLETE ($(cmtest_node_count) cmtest node(s) remain)"
    fi
  } >>"$OUT/report.md" 2>/dev/null || true

  if [ "$clean" != 1 ]; then
    exit 6
  fi
  if [ -n "$ABORT_CODE" ]; then
    exit "$ABORT_CODE"
  fi
  exit "$report_rc"
}
# ---------------------------------------------------------------------------
# Synthetic CPU load (stress scenario). Declared before the trap so on_exit
# can always call stop_cpu_load, which is idempotent.
# ---------------------------------------------------------------------------

LOAD_PIDS=()
LOAD_APP_PID=""
LOAD_SPINNER_CMD='while :; do :; done'

# start_cpu_load APP_PID -- pin every thread of APP_PID and STRESS_SPINNERS
# busy loops to one CPU.
start_cpu_load() {
  local app_pid="$1" cpu="$STRESS_CPU" i
  [ -z "$cpu" ] && cpu=$(( $(nproc) - 1 ))
  LOAD_APP_PID="$app_pid"
  taskset -a -p -c "$cpu" "$app_pid" >/dev/null
  for i in $(seq 1 "$STRESS_SPINNERS"); do
    taskset -c "$cpu" bash -c "$LOAD_SPINNER_CMD" &
    LOAD_PIDS+=("$!")
  done
  log "stress: app pid $app_pid + $STRESS_SPINNERS spinner(s) pinned to CPU $cpu"
}

# stop_cpu_load -- kill OUR spinners (verified by command line) and give the
# app back every CPU. Safe to call any number of times, on any exit path.
stop_cpu_load() {
  local pid
  for pid in "${LOAD_PIDS[@]}"; do
    if tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null | grep -qF "$LOAD_SPINNER_CMD"; then
      kill "$pid" 2>/dev/null || true
    fi
  done
  for pid in "${LOAD_PIDS[@]}"; do
    wait "$pid" 2>/dev/null || true
  done
  LOAD_PIDS=()
  if [ -n "$LOAD_APP_PID" ] && kill -0 "$LOAD_APP_PID" 2>/dev/null; then
    taskset -a -p -c "0-$(( $(nproc) - 1 ))" "$LOAD_APP_PID" >/dev/null 2>&1 || true
  fi
  LOAD_APP_PID=""
}

trap on_exit EXIT
trap 'ABORT_CODE=130; exit 130' INT
trap 'ABORT_CODE=143; exit 143' TERM

abort() {
  local code="$1"; shift
  err "$*"
  ABORT_CODE="$code"
  ABORT_MESSAGE="$*"
  exit "$code"
}

# ---------------------------------------------------------------------------
# Preflight (R4): refuse BEFORE any test audio plays, never kill anything.
# The trap above is already installed, so every refusal below still renders
# a report and a "Cleanup: complete" line via on_exit.
# ---------------------------------------------------------------------------

need_tools() {
  local missing=()
  command -v pw-loopback >/dev/null 2>&1 || missing+=("pipewire-bin")
  command -v pw-record >/dev/null 2>&1 || missing+=("pipewire-bin")
  command -v pw-play >/dev/null 2>&1 || missing+=("pipewire-bin")
  command -v pw-link >/dev/null 2>&1 || missing+=("pipewire-bin")
  command -v pw-dump >/dev/null 2>&1 || missing+=("pipewire-bin")
  command -v python3 >/dev/null 2>&1 || missing+=("python3")
  if [ "${#missing[@]}" -gt 0 ]; then
    abort 3 "missing tools -- install: ${missing[*]}"
  fi
  if ! python3 -c 'import numpy' >/dev/null 2>&1; then
    abort 3 "python3 numpy is required: sudo apt install python3-numpy"
  fi
}
need_tools

if [ -n "$(pgrep -x cleanmic 2>/dev/null || true)" ]; then
  err "refusing to start -- a cleanmic process is already running:"
  pgrep -a -x cleanmic >&2 || true
  abort 4 "a cleanmic process is already running."
fi
EXISTING_CMTEST="$(cmtest_node_count)"
if [ "${EXISTING_CMTEST:-0}" -gt 0 ]; then
  abort 4 "refusing to start -- $EXISTING_CMTEST cmtest_* node(s) already exist (another harness run, or a leftover)."
fi

if [ -n "$(ss -Htn state established '( sport = :3389 )' 2>/dev/null || true)" ]; then
  REMOTE_DESKTOP_SESSION="yes"
  log "NOTE: an RDP session is established -- the RDP-safe graph handles this, informational only."
fi

# ---------------------------------------------------------------------------
# RDP-safe graph (per the debug session's env_up2.sh): a Stream/Input capture
# side (never autoconnected) feeding an Audio/Source, with NO Audio/Sink.
# ---------------------------------------------------------------------------

start_graph() {
  pw-loopback -n cmtest -m '[ MONO ]' \
    --capture-props='{ media.class=Stream/Input/Audio node.name=cmtest_in node.description=cmtest_in node.autoconnect=false audio.position=[ MONO ] audio.rate=48000 }' \
    --playback-props='{ media.class=Audio/Source node.name=cmtest_mic node.description=cmtest_mic audio.position=[ MONO ] audio.rate=48000 priority.session=0 }' \
    >"$OUT/logs/loopback.log" 2>&1 &
  LOOP_PID=$!
  disown 2>/dev/null || true

  local waited=0
  while [ "$waited" -lt 50 ]; do
    if pw-link -io 2>/dev/null | grep -q "cmtest_in:input_MONO" && pw-link -io 2>/dev/null | grep -q "cmtest_mic:capture_MONO"; then
      break
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  sleep 1.5

  if ! audit_graph; then
    abort 4 "RDP-safe graph audit failed before any test audio played."
  fi
  log "graph up: cmtest_in -> cmtest_mic (pid $LOOP_PID)"
}

audit_graph() {
  local -a allow=()
  [ "$MONITOR_NULL_SINK" = 1 ] && allow=(--allow-sink cmtest_null)
  pw-dump 2>/dev/null | python3 "$E2E_DIR/pwgraph.py" audit "${allow[@]}"
}

# ---------------------------------------------------------------------------
# record_pair NAME WAV [SOURCE_PORT] [DRIVER_FUNC]
#
# DRIVER_FUNC, if given, is backgrounded right after the player is linked
# (concurrently with playback) -- e.g. the swaps scenario's UI-driving swap
# sequence. A nonzero driver exit aborts 5.
# ---------------------------------------------------------------------------

RECORD_START_EPOCH=""
# Set by record_pair, appended to the next measure_and_add: CPU busy/steal
# share and 1-min load average over the recording (the "ran under load" flag).
declare -a LAST_CPU_META=()

# app_alive PID -- 0 when PID exists and is really running. A crashed app
# can linger for many seconds while apport/systemd-coredump drains its core
# (the pre-fix DeepFilterNet abort did), and `kill -0` alone reports it
# alive the whole time; the kernel's CoreDumping flag (Linux >= 4.15) and a
# zombie state both mean "dead".
app_alive() {
  local pid="$1"
  kill -0 "$pid" 2>/dev/null || return 1
  local st
  st="$(awk '{print $3}' "/proc/$pid/stat" 2>/dev/null || echo X)"
  case "$st" in Z | X | x) return 1 ;; esac
  if grep -Eq '^CoreDumping:[[:space:]]*1' "/proc/$pid/status" 2>/dev/null; then
    return 1
  fi
  return 0
}

# Prints "busy total steal" jiffies from the aggregate /proc/stat line.
cpu_sample() {
  local _cpu user nice system idle iowait irq softirq steal _rest
  read -r _cpu user nice system idle iowait irq softirq steal _rest </proc/stat
  local total=$((user + nice + system + idle + iowait + irq + softirq + steal))
  echo "$((total - idle - iowait)) $total $steal"
}

record_pair() {
  local name="$1" wav="$2" source_port="${3:-CleanMic:capture_MONO}" driver_func="${4:-}"
  local rec_wav="$OUT/rec/$name.wav"
  # The app must survive every recording: a dead app used to show up only
  # as an "unmeasurable latency" (dfn-panic-under-load), never as exit 5.
  local app_pid=""
  app_pid="$(bash "$NESTED_RUN" app-pid "$DISPLAY_ARG" 2>/dev/null)" || app_pid=""

  # NOT disowned: `wait "$RECORDER_PID"` below needs bash to still track this
  # as one of its children. A disowned job returns instantly (rc 0) from
  # `wait`, without actually waiting -- which silently truncated every
  # recording to ~1-2 s during development (the recorder was signalled to
  # stop before the player had played more than its startup latency).
  pw-record -P '{ node.autoconnect=false node.name=cmtest_rec media.class=Stream/Input/Audio }' \
    --channels=2 --format=f32 --rate=48000 "$rec_wav" >"$OUT/logs/rec_$name.log" 2>&1 &
  RECORDER_PID=$!

  local waited=0
  while [ "$waited" -lt 30 ]; do
    pw-link -i 2>/dev/null | grep -q "cmtest_rec:input_FR" && break
    sleep 0.1
    waited=$((waited + 1))
  done
  pw-link cmtest_mic:capture_MONO cmtest_rec:input_FL >/dev/null 2>&1 || true
  pw-link "$source_port" cmtest_rec:input_FR >/dev/null 2>&1 || true

  sleep 0.5

  local duration_s
  duration_s="$(python3 -c "
import wave
with wave.open('$wav', 'rb') as w:
    print(round(w.getnframes() / w.getframerate(), 1) + 1)
" 2>/dev/null || echo 30)"
  local timeout_s; timeout_s=$(python3 -c "print(int($duration_s) + 15)" 2>/dev/null || echo 45)

  # NOT disowned -- see the recorder's note above; `wait "$PLAYER_PID"` below
  # requires it.
  timeout "$timeout_s" pw-play -P '{ node.autoconnect=false node.name=cmtest_play }' "$wav" \
    >"$OUT/logs/play_$name.log" 2>&1 &
  PLAYER_PID=$!

  waited=0
  while [ "$waited" -lt 30 ]; do
    pw-link -o 2>/dev/null | grep -q "cmtest_play:output_MONO" && break
    sleep 0.05
    waited=$((waited + 1))
  done

  if ! audit_graph; then
    kill "$PLAYER_PID" 2>/dev/null || true
    abort 4 "RDP-safe graph audit failed before linking the player for '$name'."
  fi

  pw-link cmtest_play:output_MONO cmtest_in:input_MONO >/dev/null 2>&1 || true

  if ! audit_graph; then
    kill "$PLAYER_PID" 2>/dev/null || true
    abort 4 "RDP-safe graph audit failed right after linking the player for '$name'."
  fi

  RECORD_START_EPOCH="$(date +%s.%N)"
  local cpu_a load_a
  cpu_a="$(cpu_sample)"
  load_a="$(cut -d' ' -f1 /proc/loadavg)"
  local driver_pid=""
  if [ -n "$driver_func" ]; then
    "$driver_func" &
    driver_pid=$!
  fi

  wait "$PLAYER_PID" 2>/dev/null || true
  PLAYER_PID=""

  if [ -n "$driver_pid" ]; then
    local driver_rc=0
    wait "$driver_pid" || driver_rc=$?
    if [ "$driver_rc" != 0 ]; then
      kill "$RECORDER_PID" 2>/dev/null || true
      abort 5 "driver for '$name' failed (exit $driver_rc) -- see its click-target error above."
    fi
  fi

  local cpu_b load_b ba ta sa bb tb sb
  cpu_b="$(cpu_sample)"
  load_b="$(cut -d' ' -f1 /proc/loadavg)"
  read -r ba ta sa <<<"$cpu_a"
  read -r bb tb sb <<<"$cpu_b"
  LAST_CPU_META=(
    --meta "cpu_busy_pct=$(python3 -c "print(round(100 * ($bb - $ba) / max(1, $tb - $ta), 1))")"
    --meta "cpu_steal_pct=$(python3 -c "print(round(100 * ($sb - $sa) / max(1, $tb - $ta), 1))")"
    --meta "loadavg_1m=$(python3 -c "print(max($load_a, $load_b))")"
  )

  sleep 0.8
  kill -INT "$RECORDER_PID" 2>/dev/null || true
  wait "$RECORDER_PID" 2>/dev/null || true
  RECORDER_PID=""

  if [ -n "$app_pid" ] && ! app_alive "$app_pid"; then
    local died_json="$OUT/rec/${name}_app_alive.json"
    python3 "$E2E_DIR/analyze.py" check --scenario "${CURRENT_SCENARIO:-unknown}" --recording "$name" \
      --metric app_alive --value DIED --threshold-desc "== yes" --result FAIL \
      --note "cleanmic pid $app_pid exited during the recording" --json-out "$died_json" || true
    [ -s "$died_json" ] && MEASURED_JSON+=("$died_json")
    abort 5 "the app (pid $app_pid) died during recording '$name' -- see its app.log."
  fi

  if [ ! -s "$rec_wav" ]; then
    abort 5 "recording '$name' is missing or empty."
  fi
  local size; size=$(stat -c %s "$rec_wav" 2>/dev/null || echo 0)
  if [ "$size" -lt 96000 ]; then # < ~0.5 s of 2ch f32 @ 48kHz
    abort 5 "recording '$name' is suspiciously short ($size bytes)."
  fi
  log "recorded $name -> $rec_wav"
}

# Poll app.log for a regex, waiting up to $2 seconds.
wait_for_log() {
  local pattern="$1" timeout_s="$2" logfile="$3" waited=0
  while [ "$waited" -lt "$((timeout_s * 10))" ]; do
    grep -Eq "$pattern" "$logfile" 2>/dev/null && return 0
    sleep 0.1
    waited=$((waited + 1))
  done
  return 1
}

# nested-run.sh call wrapper: maps its exit 11 to abort 4, everything else
# non-zero to abort 5.
run_nested() {
  local rc=0
  bash "$NESTED_RUN" "$@" || rc=$?
  if [ "$rc" = 0 ]; then
    return 0
  elif [ "$rc" = 11 ]; then
    abort 4 "nested-run.sh $* refused (exit 11)."
  else
    abort 5 "nested-run.sh $* failed (exit $rc)."
  fi
}

# click-target wrapper for the UI actions scenarios drive directly (not
# through record_pair's driver mechanism): 14/15 map to abort 5, everything
# else not 0 is also abort 5 (an unconfirmed/unreachable UI action is always
# a harness/driver problem here, never a metric FAIL).
click_target() {
  if ! bash "$NESTED_RUN" click-target "$DISPLAY_ARG" "$@"; then
    abort 5 "click-target $* failed."
  fi
}

# Launches with the given engine/mode (plus any extra --config args) and
# waits for startup confirmation. Sets the global APP_LOG. NEVER call this
# via `$(...)` -- it can call abort(), whose `exit` would only kill a
# command-substitution subshell instead of the whole script.
APP_LOG=""
launch_and_wait() {
  local engine="$1" mode="$2"; shift 2
  # `stop` (always run at the end of the PREVIOUS scenario/iteration) closes
  # the recorded Xephyr along with the app -- reopen it first. A no-op reuse
  # when it is already alive (e.g. this run's very first launch).
  run_nested xephyr "$DISPLAY_ARG"
  run_nested launch "$DISPLAY_ARG" --lang "$LANG_ARG" "${BIN_ARGS[@]}" \
    --config "engine = \"$engine\"" --config "mode = \"$mode\"" "$@"
  APP_LOG="$CLEANMIC_HARNESS_STATE/display-${DISPLAY_ARG#:}/app.log"
  if ! wait_for_log "Audio processing started" 30 "$APP_LOG"; then
    tail -n 15 "$APP_LOG" >&2 || true
    abort 5 "launch ($engine/$mode): app.log did not confirm startup within 30s."
  fi
  sleep 2
}

# measure_and_add SCENARIO RECORDING KIND [--meta k=v]...
measure_and_add() {
  local scenario="$1" recording="$2" kind="$3"; shift 3
  local json="$OUT/rec/${recording}.json"
  if ! python3 "$E2E_DIR/analyze.py" measure "$OUT/rec/${recording}.wav" \
    --scenario "$scenario" --recording "$recording" --kind "$kind" \
    "$@" "${LAST_CPU_META[@]}" --json-out "$json"; then
    abort 5 "analyze.py measure failed for $recording."
  fi
  MEASURED_JSON+=("$json")
}

# Runs logscan + the fell_behind/errors/panics log-check for a scenario's
# app.log and appends the resulting checks to MEASURED_JSON. Never aborts:
# a log-scan hiccup shouldn't take down an otherwise-complete run.
scenario_log_checks() {
  local scenario="$1" recording="$2" app_log="$3" fell_behind_max="${4:-$LOG_FELL_BEHIND_MAX}"
  local logscan_json="$OUT/logs/${recording}.logscan.json"
  python3 "$E2E_DIR/analyze.py" logscan "$app_log" --json-out "$logscan_json" || true
  local check_json="$OUT/rec/${recording}_logcheck.json"
  python3 "$E2E_DIR/analyze.py" log-check --logscan "$logscan_json" \
    --fell-behind-max "$fell_behind_max" --error-max "$LOG_ERROR_MAX" --panic-max "$LOG_PANIC_MAX" \
    --scenario "$scenario" --recording "$recording" --json-out "$check_json" || true
  [ -s "$check_json" ] && MEASURED_JSON+=("$check_json")
}

# diff_check_add A_JSON B_JSON A_FIELD B_FIELD MAX_DIFF METRIC SCENARIO RECORDING
diff_check_add() {
  local a="$1" b="$2" af="$3" bf="$4" maxd="$5" metric="$6" scenario="$7" recording="$8"
  local json="$OUT/rec/${recording}.json"
  python3 "$E2E_DIR/analyze.py" diff-check --a "$a" --b "$b" --a-field "$af" --b-field "$bf" \
    --max-diff "$maxd" --metric "$metric" --scenario "$scenario" --recording "$recording" \
    --json-out "$json" || true
  [ -s "$json" ] && MEASURED_JSON+=("$json")
}

# ---------------------------------------------------------------------------
# SCENARIO: baseline -- one fresh launch + recording per $BASELINE_ENGINES
# entry; SKIP (not FAIL) an engine this build doesn't have.
# ---------------------------------------------------------------------------

scenario_baseline() {
  local engine mode="MaxQuality"
  for engine in $BASELINE_ENGINES; do
    log "scenario baseline: launching $engine/$mode"
    # `stop` (below, end of the previous iteration) closes the recorded
    # Xephyr as well as the app -- re-open it before every launch but the
    # first (a no-op reuse when it's still alive, e.g. this loop's first
    # pass right after start_graph's own xephyr).
    run_nested xephyr "$DISPLAY_ARG"
    run_nested launch "$DISPLAY_ARG" --lang "$LANG_ARG" "${BIN_ARGS[@]}" \
      --config "engine = \"$engine\"" --config "mode = \"$mode\""
    APP_LOG="$CLEANMIC_HARNESS_STATE/display-${DISPLAY_ARG#:}/app.log"
    if ! wait_for_log "Audio processing started" 30 "$APP_LOG"; then
      tail -n 15 "$APP_LOG" >&2 || true
      abort 5 "baseline: app.log did not confirm startup within 30s for $engine."
    fi

    local actual_engine=""
    actual_engine="$(grep -Eo 'Engine set to [A-Za-z0-9]+' "$APP_LOG" 2>/dev/null | tail -1 | awk '{print $NF}')" || true
    if [ "$actual_engine" != "$engine" ]; then
      log "baseline: $engine not available in this build (got '${actual_engine:-none}') -- SKIP"
      run_nested stop "$DISPLAY_ARG"
      local skip_json="$OUT/rec/baseline_${engine}_skip.json"
      python3 "$E2E_DIR/analyze.py" check --scenario baseline --recording "baseline_${engine}" \
        --metric availability --value "${actual_engine:-none}" --threshold-desc "== $engine" \
        --result SKIP --note "not available in this build" --json-out "$skip_json"
      MEASURED_JSON+=("$skip_json")
      continue
    fi
    wait_for_log "Linked cmtest_mic:capture_MONO -> CleanMic-capture:input_MONO" 10 "$APP_LOG" || true
    sleep 2

    record_pair "baseline_${engine}" "$OUT/signals/speech.wav" "CleanMic:capture_MONO"
    measure_and_add baseline "baseline_${engine}" speech --meta "engine=$engine" --meta "mode=$mode"

    run_nested stop "$DISPLAY_ARG"
    cp "$APP_LOG" "$OUT/logs/baseline_${engine}.log" 2>/dev/null || true
    scenario_log_checks baseline "baseline_${engine}" "$APP_LOG"
  done
}

# ---------------------------------------------------------------------------
# SCENARIO: swaps -- pre/during(15 live swaps + 3 mode changes)/post, plus
# the logged swap-sequence check and the pre-vs-post latency drift check.
# ---------------------------------------------------------------------------

SWAPS_ENGINE_SEQUENCE="RNNoise DeepFilterNet Dpdfnet2 Dpdfnet8 RNNoise Dpdfnet2 DeepFilterNet Dpdfnet8 Dpdfnet2 RNNoise DeepFilterNet Dpdfnet8 RNNoise DeepFilterNet Dpdfnet2"

engine_target_name() {
  case "$1" in
    RNNoise) echo engine-rnnoise ;;
    DeepFilterNet) echo engine-deepfilternet ;;
    Dpdfnet2) echo engine-dpdfnet2 ;;
    Dpdfnet8) echo engine-dpdfnet8 ;;
  esac
}

# Backgrounded by record_pair while speech_loop60.wav plays. Exits nonzero
# (never abort() -- this runs in its own subshell, where `exit` cannot reach
# the main script) on the first unconfirmed click-target, which record_pair
# turns into an abort 5 after `wait`ing on this function's pid.
_swaps_driver() {
  local events_file="$OUT/rec/swaps_during.events.tsv" expected_file="$OUT/rec/swaps.expected"
  : >"$events_file"
  local mode="MaxQuality" i=0 engine target t new_mode mode_target
  local -a expected_pairs=()
  for engine in $SWAPS_ENGINE_SEQUENCE; do
    i=$((i + 1))
    target="$(engine_target_name "$engine")"
    if ! bash "$NESTED_RUN" click-target "$DISPLAY_ARG" "$target" \
      --expect "Engine changed to $engine \\(mode=$mode\\)" --timeout 5; then
      return 1
    fi
    t="$(python3 -c "import time; print(round(time.time() - $RECORD_START_EPOCH, 2))" 2>/dev/null || echo "?")"
    printf '%s\tengine\t%s\t%s\n' "$t" "$engine" "$mode" >>"$events_file"
    expected_pairs+=("$engine:$mode")
    sleep 1.5

    new_mode=""
    case "$i" in
      4) new_mode=LowCpu ;;
      9) new_mode=Balanced ;;
      14) new_mode=MaxQuality ;;
    esac
    if [ -n "$new_mode" ]; then
      case "$new_mode" in
        LowCpu) mode_target=mode-lowcpu ;;
        Balanced) mode_target=mode-balanced ;;
        MaxQuality) mode_target=mode-maxquality ;;
      esac
      if ! bash "$NESTED_RUN" click-target "$DISPLAY_ARG" "$mode_target" \
        --expect "Mode changed to $new_mode" --timeout 5; then
        return 1
      fi
      t="$(python3 -c "import time; print(round(time.time() - $RECORD_START_EPOCH, 2))" 2>/dev/null || echo "?")"
      printf '%s\tmode\t-\t%s\n' "$t" "$new_mode" >>"$events_file"
      mode="$new_mode"
    fi
  done
  (IFS=,; echo "${expected_pairs[*]}") >"$expected_file"
  return 0
}

scenario_swaps() {
  launch_and_wait Dpdfnet2 MaxQuality

  record_pair swaps_pre "$OUT/signals/speech.wav" "CleanMic:capture_MONO"
  measure_and_add swaps swaps_pre speech --meta "engine=Dpdfnet2" --meta "mode=MaxQuality"

  # kind=swaps_during, NOT speech: latency_ms/lag_corr/latency_spread_ms are
  # structurally meaningless across 15 live engine swaps (each crossfade
  # breaks the single fixed-lag correlation), and a `holes` count > 0 is
  # EXPECTED (one brief gap per swap) rather than a defect -- both are
  # covered instead by the swap-count-scaled budget checks right below.
  record_pair swaps_during "$OUT/signals/speech_loop60.wav" "CleanMic:capture_MONO" _swaps_driver
  measure_and_add swaps swaps_during swaps_during --meta "engine=Dpdfnet2" --meta "mode=MaxQuality"

  local logscan_json="$OUT/logs/swaps_during.logscan.json"
  python3 "$E2E_DIR/analyze.py" logscan "$APP_LOG" --json-out "$logscan_json" || true
  local expected=""; expected="$(cat "$OUT/rec/swaps.expected" 2>/dev/null || true)"
  python3 "$E2E_DIR/analyze.py" swap-check --logscan "$logscan_json" --expected "$expected" \
    --scenario swaps --recording swaps_sequence --json-out "$OUT/rec/swaps_sequence.json" || true
  [ -s "$OUT/rec/swaps_sequence.json" ] && MEASURED_JSON+=("$OUT/rec/swaps_sequence.json")

  local num_swaps; num_swaps="$(echo "$SWAPS_ENGINE_SEQUENCE" | wc -w)"
  local zero_budget; zero_budget=$(python3 -c "print($num_swaps * $SWAP_ZERO_MS_PER_SWAP_MAX)")
  local zero_ms=0
  zero_ms="$(python3 -c "
import json
print(json.load(open('$OUT/rec/swaps_during.json')).get('zero_run_ms', 0))
" 2>/dev/null || echo 0)"
  local zresult="PASS"
  python3 -c "raise SystemExit(0 if $zero_ms <= $zero_budget else 1)" 2>/dev/null || zresult="FAIL"
  python3 "$E2E_DIR/analyze.py" check --scenario swaps --recording swaps_during \
    --metric zero_run_ms_budget --value "$zero_ms" --threshold-desc "<= $zero_budget" \
    --result "$zresult" --json-out "$OUT/rec/swaps_zero_budget.json"
  MEASURED_JSON+=("$OUT/rec/swaps_zero_budget.json")

  # Holes budget: at most one brief crossfade gap per swap is expected here
  # (unlike a steady recording, where HOLES_MAX=0 applies).
  local holes_count=0
  holes_count="$(python3 -c "
import json
print(json.load(open('$OUT/rec/swaps_during.json')).get('holes', 0))
" 2>/dev/null || echo 0)"
  local hresult="PASS"
  python3 -c "raise SystemExit(0 if $holes_count <= $num_swaps else 1)" 2>/dev/null || hresult="FAIL"
  python3 "$E2E_DIR/analyze.py" check --scenario swaps --recording swaps_during \
    --metric holes_budget --value "$holes_count" --threshold-desc "<= $num_swaps (one crossfade gap per swap)" \
    --result "$hresult" --json-out "$OUT/rec/swaps_holes_budget.json"
  MEASURED_JSON+=("$OUT/rec/swaps_holes_budget.json")

  sleep 2
  record_pair swaps_post "$OUT/signals/speech.wav" "CleanMic:capture_MONO"
  measure_and_add swaps swaps_post speech --meta "engine=Dpdfnet2" --meta "mode=MaxQuality"

  diff_check_add "$OUT/rec/swaps_pre.json" "$OUT/rec/swaps_post.json" latency_ms latency_ms \
    "$LATENCY_DRIFT_MAX_MS" swaps_latency_drift_ms swaps swaps_drift

  run_nested stop "$DISPLAY_ARG"
  cp "$APP_LOG" "$OUT/logs/swaps.log" 2>/dev/null || true
  scenario_log_checks swaps swaps "$APP_LOG"
}

# ---------------------------------------------------------------------------
# SCENARIO: toggle -- pre, Activer off/on, post, plus the drift check and
# the "at least 2 Discarded lines after restart" check.
# ---------------------------------------------------------------------------

scenario_toggle() {
  launch_and_wait Dpdfnet2 MaxQuality

  record_pair toggle_pre "$OUT/signals/speech.wav" "CleanMic:capture_MONO"
  measure_and_add toggle toggle_pre speech --meta "engine=Dpdfnet2" --meta "mode=MaxQuality"

  click_target enable --expect "Audio processing stopped" --timeout 5
  sleep 5.8
  click_target enable --expect "Audio processing started" --timeout 5
  sleep 1

  if ! bash "$NESTED_RUN" check-layout "$DISPLAY_ARG"; then
    abort 5 "toggle: check-layout did not confirm Activer/Enable is back ON."
  fi

  record_pair toggle_post "$OUT/signals/speech.wav" "CleanMic:capture_MONO"
  measure_and_add toggle toggle_post speech --meta "engine=Dpdfnet2" --meta "mode=MaxQuality"

  diff_check_add "$OUT/rec/toggle_pre.json" "$OUT/rec/toggle_post.json" latency_ms latency_ms \
    "$LATENCY_DRIFT_MAX_MS" toggle_latency_drift_ms toggle toggle_drift

  local discarded_count=0
  discarded_count="$(grep -Ec 'Discarded [0-9]+ ms' "$APP_LOG" 2>/dev/null)" || true
  [ -z "$discarded_count" ] && discarded_count=0
  local result="FAIL"
  [ "$discarded_count" -ge 2 ] && result="PASS"
  python3 "$E2E_DIR/analyze.py" check --scenario toggle --recording toggle_discarded \
    --metric discarded_after_restart --value "$discarded_count" --threshold-desc ">= 2" \
    --result "$result" --json-out "$OUT/rec/toggle_discarded.json"
  MEASURED_JSON+=("$OUT/rec/toggle_discarded.json")

  run_nested stop "$DISPLAY_ARG"
  cp "$APP_LOG" "$OUT/logs/toggle.log" 2>/dev/null || true
  scenario_log_checks toggle toggle "$APP_LOG"
}

# ---------------------------------------------------------------------------
# SCENARIO: modes -- LowCpu, Balanced, LowCpu, Balanced; repeats of the same
# mode must agree within LATENCY_DRIFT_MAX_MS.
# ---------------------------------------------------------------------------

scenario_modes() {
  launch_and_wait Dpdfnet2 MaxQuality

  local -a mode_run=(LowCpu Balanced LowCpu Balanced)
  declare -A mode_counts=()
  local m target k rec
  for m in "${mode_run[@]}"; do
    case "$m" in
      LowCpu) target=mode-lowcpu ;;
      Balanced) target=mode-balanced ;;
    esac
    click_target "$target" --expect "Mode changed to $m" --timeout 5
    sleep 2
    k=$(( ${mode_counts[$m]:-0} + 1 ))
    mode_counts[$m]=$k
    rec="modes_${m}_${k}"
    record_pair "$rec" "$OUT/signals/speech.wav" "CleanMic:capture_MONO"
    measure_and_add modes "$rec" speech --meta "engine=Dpdfnet2" --meta "mode=$m"
  done

  diff_check_add "$OUT/rec/modes_LowCpu_1.json" "$OUT/rec/modes_LowCpu_2.json" latency_ms latency_ms \
    "$LATENCY_DRIFT_MAX_MS" modes_lowcpu_repeat_drift_ms modes modes_lowcpu_repeat
  diff_check_add "$OUT/rec/modes_Balanced_1.json" "$OUT/rec/modes_Balanced_2.json" latency_ms latency_ms \
    "$LATENCY_DRIFT_MAX_MS" modes_balanced_repeat_drift_ms modes modes_balanced_repeat

  run_nested stop "$DISPLAY_ARG"
  cp "$APP_LOG" "$OUT/logs/modes.log" 2>/dev/null || true
  scenario_log_checks modes modes "$APP_LOG"
}

# ---------------------------------------------------------------------------
# SCENARIO: dc -- speech+DC and silence+DC, both must show the DC blocker
# working; the silence recording also gets an informational meters shot.
# ---------------------------------------------------------------------------

scenario_dc() {
  launch_and_wait Dpdfnet2 MaxQuality

  record_pair dc_speech "$OUT/signals/speech_dc.wav" "CleanMic:capture_MONO"
  measure_and_add dc dc_speech dc_speech --meta "engine=Dpdfnet2" --meta "mode=MaxQuality"

  bash "$NESTED_RUN" scroll "$DISPLAY_ARG" bottom >/dev/null 2>&1 || true
  bash "$NESTED_RUN" shot "$DISPLAY_ARG" "$OUT/shots/dc_silence_meters.png" >/dev/null 2>&1 || true

  record_pair dc_silence "$OUT/signals/silence_dc.wav" "CleanMic:capture_MONO"
  measure_and_add dc dc_silence dc_silence

  run_nested stop "$DISPLAY_ARG"
  cp "$APP_LOG" "$OUT/logs/dc.log" 2>/dev/null || true
  scenario_log_checks dc dc "$APP_LOG"
}

# ---------------------------------------------------------------------------
# SCENARIO: autogain -- ON boosts a quiet mic, OFF is near-unity, and ON vs
# OFF must not audibly pump steady pink noise.
# ---------------------------------------------------------------------------

scenario_autogain() {
  launch_and_wait Dpdfnet2 MaxQuality --config 'auto_gain_enabled = true'

  record_pair ag_m40_on "$OUT/signals/speech_m40.wav" "CleanMic:capture_MONO"
  measure_and_add autogain ag_m40_on ag_on --meta "engine=Dpdfnet2" --meta "mode=MaxQuality"

  click_target autogain --expect "Input auto-gain disabled" --timeout 5
  record_pair ag_m40_off "$OUT/signals/speech_m40.wav" "CleanMic:capture_MONO"
  measure_and_add autogain ag_m40_off ag_off

  record_pair ag_pink_off "$OUT/signals/pink_m45.wav" "CleanMic:capture_MONO"
  measure_and_add autogain ag_pink_off ag_pink

  click_target autogain --expect "Input auto-gain enabled" --timeout 5
  record_pair ag_pink_on "$OUT/signals/pink_m45.wav" "CleanMic:capture_MONO"
  measure_and_add autogain ag_pink_on ag_pink

  diff_check_add "$OUT/rec/ag_pink_on.json" "$OUT/rec/ag_pink_off.json" out_rms_db out_rms_db \
    "$AUTOGAIN_NOISE_MAX_DIFF_DB" autogain_noise_diff_db autogain ag_pink_diff

  run_nested stop "$DISPLAY_ARG"
  cp "$APP_LOG" "$OUT/logs/autogain.log" 2>/dev/null || true
  scenario_log_checks autogain autogain "$APP_LOG"
}

# ---------------------------------------------------------------------------
# SCENARIO: stress -- controlled synthetic CPU load (debug session
# dfn-panic-under-load). Per $STRESS_ENGINES entry: fresh launch, settle,
# pin the app next to busy loops (start_cpu_load), record while loaded,
# unload, record again. Invariants: the app survives, the vendored
# DeepFilterNet plugin never reaches its abort, the virtual mic never goes
# dead for longer than STRESS_DEAD_RUN_MAX_MS, a runtime fallback (if any)
# lands within STRESS_RECOVERY_MAX_S of the load starting, and the post-load
# recording passes the normal speech rules for whichever engine is active.
# ---------------------------------------------------------------------------

scenario_stress() {
  command -v taskset >/dev/null 2>&1 || abort 3 "the stress scenario needs taskset (util-linux)."
  local engine mode="MaxQuality"
  for engine in $STRESS_ENGINES; do
    log "scenario stress: launching $engine/$mode"
    launch_and_wait "$engine" "$mode"
    local actual_engine=""
    actual_engine="$(grep -Eo 'Engine set to [A-Za-z0-9]+' "$APP_LOG" 2>/dev/null | tail -1 | awk '{print $NF}')" || true
    if [ "$actual_engine" != "$engine" ]; then
      log "stress: $engine not available in this build (got '${actual_engine:-none}') -- SKIP"
      run_nested stop "$DISPLAY_ARG"
      local skip_json="$OUT/rec/stress_${engine}_skip.json"
      python3 "$E2E_DIR/analyze.py" check --scenario stress --recording "stress_${engine}" \
        --metric availability --value "${actual_engine:-none}" --threshold-desc "== $engine" \
        --result SKIP --note "not available in this build" --json-out "$skip_json"
      MEASURED_JSON+=("$skip_json")
      continue
    fi
    wait_for_log "Linked cmtest_mic:capture_MONO -> CleanMic-capture:input_MONO" 10 "$APP_LOG" || true
    sleep "$STRESS_SETTLE_S"

    local app_pid=""
    app_pid="$(bash "$NESTED_RUN" app-pid "$DISPLAY_ARG" 2>/dev/null)" || abort 5 "stress: no harness app pid for $engine."
    local load_start; load_start="$(date +%s.%N)"
    start_cpu_load "$app_pid"
    record_pair "stress_${engine}_load" "$OUT/signals/speech.wav" "CleanMic:capture_MONO"
    stop_cpu_load
    measure_and_add stress "stress_${engine}_load" stress_load --meta "engine=$engine" --meta "mode=$mode" \
      --meta "stress_spinners=$STRESS_SPINNERS"

    sleep 2
    local mid_scan="$OUT/logs/stress_${engine}_mid.logscan.json"
    python3 "$E2E_DIR/analyze.py" logscan "$APP_LOG" --json-out "$mid_scan" >/dev/null || true
    local active="$engine"
    active="$(python3 "$E2E_DIR/analyze.py" active-engine --logscan "$mid_scan" --started-with "$engine" 2>/dev/null)" || active="$engine"
    log "stress: active engine after load: $active"
    record_pair "stress_${engine}_post" "$OUT/signals/speech.wav" "CleanMic:capture_MONO"
    measure_and_add stress "stress_${engine}_post" speech --meta "engine=$active" --meta "mode=$mode"
    # Informational: the engine selector must show the engine really running
    # (a runtime fallback is session-only, so config.toml keeps the user's
    # choice and check-layout would rightly disagree -- not run here).
    bash "$NESTED_RUN" scroll "$DISPLAY_ARG" top >/dev/null 2>&1 || true
    bash "$NESTED_RUN" shot "$DISPLAY_ARG" "$OUT/shots/stress_${engine}_after.png" >/dev/null 2>&1 || true

    local alive="yes"
    app_alive "$app_pid" || alive="no"
    run_nested stop "$DISPLAY_ARG"
    cp "$APP_LOG" "$OUT/logs/stress_${engine}.log" 2>/dev/null || true
    local scan="$OUT/logs/stress_${engine}.logscan.json"
    python3 "$E2E_DIR/analyze.py" logscan "$APP_LOG" --json-out "$scan" >/dev/null || true
    local check_json="$OUT/rec/stress_${engine}_check.json"
    python3 "$E2E_DIR/analyze.py" stress-check --logscan "$scan" --app-alive "$alive" \
      --load-start "$load_start" --recovery-max-s "$STRESS_RECOVERY_MAX_S" \
      --scenario stress --recording "stress_${engine}" --json-out "$check_json" || true
    [ -s "$check_json" ] && MEASURED_JSON+=("$check_json")
    scenario_log_checks stress "stress_${engine}" "$APP_LOG" "$STRESS_LOG_FELL_BEHIND_MAX"
  done
}

# ---------------------------------------------------------------------------
# SCENARIO: monitor (--monitor-null-sink only) -- CleanMic-monitor routed
# ONLY to a second, audited, RDP-safe null-sink loopback.
# ---------------------------------------------------------------------------

scenario_monitor() {
  pw-loopback -n cmtestnull -c 2 -m '[ FL FR ]' \
    --capture-props='{ media.class=Audio/Sink node.name=cmtest_null node.description=cmtest_null audio.position=[ FL FR ] }' \
    --playback-props='{ media.class=Audio/Source node.name=cmtest_null_src node.description=cmtest_null_src priority.session=0 }' \
    >"$OUT/logs/loopback_null.log" 2>&1 &
  LOOP2_PID=$!

  local waited=0
  while [ "$waited" -lt 50 ]; do
    pw-link -io 2>/dev/null | grep -q "cmtest_null_src:capture_FL" && break
    sleep 0.1
    waited=$((waited + 1))
  done
  sleep 1.5

  if ! audit_graph; then
    kill "$LOOP2_PID" 2>/dev/null || true
    LOOP2_PID=""
    abort 4 "REFUSED: remote-desktop or foreign capture attached to cmtest_null -- monitor path cannot be tested silently in this session."
  fi

  launch_and_wait Dpdfnet2 MaxQuality --monitor-sink cmtest_null

  click_target monitor --expect "Monitor enabled" --timeout 5

  if ! audit_graph; then
    bash "$NESTED_RUN" click-target "$DISPLAY_ARG" monitor --expect "Monitor disabled" --timeout 5 || true
    run_nested stop "$DISPLAY_ARG"
    abort 4 "REFUSED: monitor path audit found a foreign link right after enabling."
  fi
  if ! wait_for_log "Pinning CleanMic monitor stream to configured sink cmtest_null" 3 "$APP_LOG"; then
    bash "$NESTED_RUN" click-target "$DISPLAY_ARG" monitor --expect "Monitor disabled" --timeout 5 || true
    run_nested stop "$DISPLAY_ARG"
    abort 4 "monitor: pin-to-sink log line not seen within 3s."
  fi

  record_pair monitor_path "$OUT/signals/speech.wav" "cmtest_null_src:capture_FL"
  measure_and_add monitor monitor_path monitor_path --meta "engine=Dpdfnet2" --meta "mode=MaxQuality"

  click_target monitor --expect "Monitor disabled" --timeout 5

  run_nested stop "$DISPLAY_ARG"
  cp "$APP_LOG" "$OUT/logs/monitor.log" 2>/dev/null || true
  scenario_log_checks monitor monitor "$APP_LOG"

  if [ -n "$LOOP2_PID" ] && [ "$(cat "/proc/$LOOP2_PID/comm" 2>/dev/null || true)" = "pw-loopback" ]; then
    kill "$LOOP2_PID" 2>/dev/null || true
  fi
  LOOP2_PID=""
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------

log "generating signals into $OUT/signals"
if ! python3 "$E2E_DIR/gen_signals.py" --out "$OUT/signals"; then
  abort 3 "gen_signals.py failed -- check assets/demo/*.wav."
fi

log "starting Xephyr on $DISPLAY_ARG"
run_nested xephyr "$DISPLAY_ARG"

start_graph

RUN_LIST="${SCENARIOS[*]}"
if [[ " $RUN_LIST " == *" all "* ]]; then
  RUN_LIST="baseline swaps toggle modes dc autogain stress"
  [ "$MONITOR_NULL_SINK" = 1 ] && RUN_LIST="$RUN_LIST monitor"
fi

for s in $RUN_LIST; do
  CURRENT_SCENARIO="$s"
  case "$s" in
    baseline) scenario_baseline ;;
    swaps) scenario_swaps ;;
    toggle) scenario_toggle ;;
    modes) scenario_modes ;;
    dc) scenario_dc ;;
    autogain) scenario_autogain ;;
    stress) scenario_stress ;;
    monitor) scenario_monitor ;;
    all) : ;;
    *) abort 2 "unknown scenario '$s'." ;;
  esac
done

exit 0
