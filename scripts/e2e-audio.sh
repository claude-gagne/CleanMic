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
#   scripts/e2e-audio.sh [options] <baseline|swaps|toggle|modes|dc|autogain|monitor|all>...
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
: "${LATENCY_MAX_MS_RNNOISE:=70}"           # measured 41-48 ms
: "${LATENCY_MAX_MS_DPDFNET2:=120}"         # measured 82-98 ms (algorithmic delay ~50 ms + quanta)
: "${LATENCY_MAX_MS_DPDFNET8:=120}"         # measured 90-100 ms
: "${LATENCY_MAX_MS_DEEPFILTERNET:=90}"     # measured 71-73 ms (LADSPA, pre-existing RTF underrun warnings aside)
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

: "${E2E_DISPLAY:=:47}"
: "${E2E_LANG:=fr}"

KNOWN_SCENARIOS="baseline swaps toggle modes dc autogain monitor all"

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
    baseline | swaps | toggle | modes | dc | autogain | monitor | all)
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
# Preflight (R4): refuse BEFORE any test audio plays, never kill anything.
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
    err "missing tools -- install: ${missing[*]}"
    exit 3
  fi
  if ! python3 -c 'import numpy' >/dev/null 2>&1; then
    err "python3 numpy is required: sudo apt install python3-numpy"
    exit 3
  fi
}
need_tools

cmtest_node_count() {
  pw-dump 2>/dev/null | python3 "$E2E_DIR/pwgraph.py" count --prefix cmtest_ 2>/dev/null || echo 0
}

if [ -n "$(pgrep -x cleanmic 2>/dev/null || true)" ]; then
  err "refusing to start -- a cleanmic process is already running:"
  pgrep -a -x cleanmic >&2 || true
  exit 4
fi
EXISTING_CMTEST="$(cmtest_node_count)"
if [ "${EXISTING_CMTEST:-0}" -gt 0 ]; then
  err "refusing to start -- $EXISTING_CMTEST cmtest_* node(s) already exist (another harness run, or a leftover)."
  exit 4
fi

REMOTE_DESKTOP_SESSION="no"
if [ -n "$(ss -Htn state established '( sport = :3389 )' 2>/dev/null || true)" ]; then
  REMOTE_DESKTOP_SESSION="yes"
  log "NOTE: an RDP session is established -- the RDP-safe graph handles this, informational only."
fi

# ---------------------------------------------------------------------------
# TRAP FIRST -- idempotent cleanup for every exit path.
# ---------------------------------------------------------------------------

LOOP_PID=""
RECORDER_PID=""
PLAYER_PID=""
ABORT_CODE=""
CLEANUP_DONE=0

on_exit() {
  local rc=$?
  set +e
  if [ "$CLEANUP_DONE" = 1 ]; then
    return
  fi
  CLEANUP_DONE=1
  [ -n "$RECORDER_PID" ] && kill -INT "$RECORDER_PID" 2>/dev/null
  [ -n "$PLAYER_PID" ] && kill -TERM "$PLAYER_PID" 2>/dev/null
  sleep 0.3
  bash "$NESTED_RUN" stop "$DISPLAY_ARG" >>"$OUT/logs/nested-run-stop.log" 2>&1
  if [ -n "$LOOP_PID" ] && [ "$(cat "/proc/$LOOP_PID/comm" 2>/dev/null || true)" = "pw-loopback" ]; then
    kill "$LOOP_PID" 2>/dev/null
  fi

  local waited=0 clean=0
  while [ "$waited" -lt 50 ]; do
    local n; n="$(cmtest_node_count)"
    local survivors; survivors="$(bash "$NESTED_RUN" status "$DISPLAY_ARG" 2>&1 | grep -c 'alive' || true)"
    if [ "${n:-1}" = 0 ]; then
      clean=1
      break
    fi
    sleep 0.1
    waited=$((waited + 1))
  done

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
  exit "$rc"
}
trap on_exit EXIT
trap 'ABORT_CODE=130; exit 130' INT
trap 'ABORT_CODE=143; exit 143' TERM

abort() {
  local code="$1"; shift
  err "$*"
  ABORT_CODE="$code"
  exit "$code"
}

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
# record_pair NAME WAV [SOURCE_PORT]
# ---------------------------------------------------------------------------

record_pair() {
  local name="$1" wav="$2" source_port="${3:-CleanMic:capture_MONO}"
  local rec_wav="$OUT/rec/$name.wav"

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

  wait "$PLAYER_PID" 2>/dev/null || true
  PLAYER_PID=""
  sleep 0.8
  kill -INT "$RECORDER_PID" 2>/dev/null || true
  wait "$RECORDER_PID" 2>/dev/null || true
  RECORDER_PID=""

  if [ ! -s "$rec_wav" ]; then
    abort 5 "recording '$name' is missing or empty."
  fi
  local size; size=$(stat -c %s "$rec_wav" 2>/dev/null || echo 0)
  if [ "$size" -lt 96000 ]; then # < ~0.5 s of 2ch f32 @ 48kHz
    abort 5 "recording '$name' is suspiciously short ($size bytes)."
  fi
  log "recorded $name -> $rec_wav"
}

# Poll app.log for a regex, waiting up to $2 seconds. Aborts 5 on timeout.
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

# ---------------------------------------------------------------------------
# Report accumulation
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
)

declare -a META_ARGS=()
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

# ---------------------------------------------------------------------------
# SCENARIO: baseline (Task 1 tracer subset -- Dpdfnet2 only)
# ---------------------------------------------------------------------------

scenario_baseline() {
  local engine="Dpdfnet2" mode="MaxQuality"
  log "scenario baseline: launching $engine/$mode"
  run_nested launch "$DISPLAY_ARG" --lang "$LANG_ARG" \
    --config "engine = \"$engine\"" --config "mode = \"$mode\""

  local app_log="$CLEANMIC_HARNESS_STATE/display-${DISPLAY_ARG#:}/app.log"
  if ! wait_for_log "Audio processing started" 30 "$app_log" ||
    ! wait_for_log "Linked cmtest_mic:capture_MONO -> CleanMic-capture:input_MONO" 30 "$app_log" ||
    ! wait_for_log "Engine set to $engine" 30 "$app_log"; then
    tail -n 15 "$app_log" >&2 || true
    abort 5 "baseline: app.log did not confirm startup within 30s."
  fi
  sleep 2

  record_pair "baseline_${engine}" "$OUT/signals/speech.wav" \
    "CleanMic:capture_MONO"

  local json="$OUT/rec/baseline_${engine}.json"
  if ! python3 "$E2E_DIR/analyze.py" measure "$OUT/rec/baseline_${engine}.wav" \
    --scenario baseline --recording "baseline_${engine}" --kind speech \
    --meta "engine=$engine" --meta "mode=$mode" --json-out "$json"; then
    abort 5 "analyze.py measure failed for baseline_${engine}."
  fi
  MEASURED_JSON+=("$json")

  run_nested stop "$DISPLAY_ARG"
  cp "$app_log" "$OUT/logs/baseline.log" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------

log "generating signals into $OUT/signals"
if ! python3 "$E2E_DIR/gen_signals.py" --out "$OUT/signals"; then
  abort 3 "gen_signals.py failed -- check assets/demo/*.wav."
fi

log "starting Xephyr on $DISPLAY_ARG"
declare -a XEPHYR_ARGS=("$DISPLAY_ARG")
run_nested xephyr "${XEPHYR_ARGS[@]}"

start_graph

RUN_LIST="${SCENARIOS[*]}"
if [[ " $RUN_LIST " == *" all "* ]]; then
  RUN_LIST="baseline"
  [ "$MONITOR_NULL_SINK" = 1 ] && RUN_LIST="$RUN_LIST"
fi

for s in $RUN_LIST; do
  case "$s" in
    baseline) scenario_baseline ;;
    all) : ;;
    swaps | toggle | modes | dc | autogain | monitor)
      abort 2 "scenario '$s' is implemented in Task 2 of this harness."
      ;;
    *) abort 2 "unknown scenario '$s'." ;;
  esac
done

BIN_USED="${APPIMAGE:-${BINARY:-$(ls -t "$REPO_ROOT"/build/CleanMic-*.AppImage 2>/dev/null | head -1 || true)}}"
build_environment_meta "$BIN_USED"
REPORT_RC=0
python3 "$E2E_DIR/analyze.py" report --out "$OUT/report.md" \
  "${THRESHOLD_ARGS[@]}" "${META_ARGS[@]}" \
  "${MEASURED_JSON[@]}" || REPORT_RC=$?

log "report: $OUT/report.md (exit $REPORT_RC)"
if [ -z "$ABORT_CODE" ]; then
  ABORT_CODE="$REPORT_RC"
fi
exit "$REPORT_RC"
