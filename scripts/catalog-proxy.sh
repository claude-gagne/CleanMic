#!/usr/bin/env bash
# catalog-proxy.sh -- local reproduction of the AppImageHub catalog's launch
# check, with PipeWire unreachable, without touching the owner's session.
#
# WHY THIS EXISTS. PR #4245 ("Add CleanMic", AppImage/appimage.github.io)
# failed the catalog bot's test: "The application exited within 11 seconds
# instead of showing a window" -- because the catalog's firejail sandbox has
# no audio stack at all (no libpipewire, no PipeWire daemon reachable), and
# CleanMic used to treat a failed PipeWire connect as a fatal `run()` error
# (fixed by D-02: `src/app.rs`/`src/pipewire/live.rs` now degrade to a
# window-only session instead). This script reproduces the catalog worker's
# own launch-check timing -- 10s grace period, then up to 20 x 1s
# `xwininfo -tree -root` polls -- so that fix can be verified locally, in an
# isolated Xvfb this script owns outright, before pushing to GitHub Actions.
# It is this slice's verification layer; Plans 02-03 expand it as the
# self-contained-AppImage work grows (D-01, D-03, D-09, D-10).
#
# USAGE
#   scripts/catalog-proxy.sh --binary PATH [--out DIR] [--lang en|fr]
#
# This script always starts its OWN private Xvfb on a free display it picks
# itself -- never the owner's real X/Xephyr/Wayland session, and never a
# `:N` the caller names (unlike scripts/nested-run.sh's Xephyr harness).
#
# EXIT CODES
#   0   PASS -- a window titled "CleanMic" appeared within the catalog's
#       window, and the app was still alive right after the screenshot.
#   1   FAIL -- the app exited before a window appeared, or died afterward.
#   2   bad usage
#   3   a required tool is missing (Xvfb, xwininfo, import/magick,
#       dbus-run-session), or the private Xvfb never came up.
#   11  REFUSED before anything started: a private dir would resolve under
#       an owner XDG/home path.
#
# SAFETY
#   - Own Xvfb on a free display (never the owner's), started here and
#     stopped only by its recorded pid, only after a /proc/<pid>/comm check.
#   - Private HOME, XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_CACHE_HOME,
#     XDG_STATE_HOME, XDG_RUNTIME_DIR all live under this run's own out dir
#     -- never under the owner's real $HOME/.config, $HOME/.local, or the
#     real $XDG_RUNTIME_DIR. Refuses (exit 11) if any would resolve there.
#   - PIPEWIRE_RUNTIME_DIR points at that private runtime dir and
#     PIPEWIRE_REMOTE names a socket that does not exist, so the app's own
#     PipeWire connect (and any pw-dump/pw-metadata subprocess it might spawn)
#     can never reach a real daemon. ~/.config/cleanmic is never opened.
#   - Cleanup signals ONLY the pids this script itself recorded: the app's
#     setsid session leader (verified alive via its recorded /proc start
#     time before every signal -- immune to the pid being reused by an
#     unrelated process after our own process exits) and Xvfb (verified via
#     /proc/<pid>/comm == "Xvfb"). No `pgrep -f` wait loop, no name-based
#     kill.

set -euo pipefail

SCRIPT_DIR="$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

usage() {
  echo "usage: scripts/catalog-proxy.sh --binary PATH [--out DIR] [--lang en|fr]" >&2
  exit 2
}

need() {
  local missing=()
  command -v Xvfb >/dev/null 2>&1 || missing+=("xvfb")
  command -v xwininfo >/dev/null 2>&1 || missing+=("x11-utils")
  if ! command -v import >/dev/null 2>&1 && ! command -v magick >/dev/null 2>&1; then
    missing+=("imagemagick")
  fi
  command -v dbus-run-session >/dev/null 2>&1 || missing+=("dbus-bin")
  if [ "${#missing[@]}" -gt 0 ]; then
    echo "catalog-proxy: missing tools -- install: ${missing[*]}" >&2
    exit 3
  fi
}

proc_comm() { cat "/proc/$1/comm" 2>/dev/null || true; }
proc_starttime() { awk '{print $22}' "/proc/$1/stat" 2>/dev/null || true; }
pid_running() {
  local st
  st=$(awk '{print $3}' "/proc/$1/stat" 2>/dev/null) || return 1
  [ -n "$st" ] && [ "$st" != "Z" ]
}

# A private dir must never resolve under an owner XDG/home path. Exits 11.
refuse_if_owner_dir() {
  local path="$1" resolved rd d
  resolved="$(realpath -m -- "$path")"
  for d in "$HOME/.config" "$HOME/.local/share" "$HOME/.cache" "$HOME/.local/state" \
    "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"; do
    rd="$(realpath -m -- "$d")"
    case "$resolved" in
      "$rd" | "$rd"/*)
        echo "catalog-proxy: REFUSING -- private path '$resolved' resolves under the owner path '$d'." >&2
        exit 11
        ;;
    esac
  done
}

BINARY="" OUT_DIR="" LANG_ARG="en"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --binary)
      shift; BINARY="${1:-}" ;;
    --out)
      shift; OUT_DIR="${1:-}" ;;
    --lang)
      shift; LANG_ARG="${1:-}" ;;
    *)
      echo "catalog-proxy: unknown option '$1'" >&2
      usage
      ;;
  esac
  shift
done

[ -n "$BINARY" ] || usage
case "$LANG_ARG" in
  en | fr) : ;;
  *)
    echo "catalog-proxy: --lang must be en or fr, got '$LANG_ARG'" >&2
    usage
    ;;
esac
[ -e "$BINARY" ] || {
  echo "catalog-proxy: binary not found: $BINARY" >&2
  exit 2
}
BINARY="$(realpath -m -- "$BINARY")"

need

if [ -z "$OUT_DIR" ]; then
  OUT_DIR="$REPO_ROOT/target/catalog-proxy/$(date -u +%Y%m%dT%H%M%SZ)"
fi
OUT_DIR="$(realpath -m -- "$OUT_DIR")"
mkdir -p "$OUT_DIR"

HOME_DIR="$OUT_DIR/home"
CONFIG_DIR="$OUT_DIR/xdg/config"
DATA_DIR="$OUT_DIR/xdg/data"
CACHE_DIR="$OUT_DIR/xdg/cache"
STATE_DIR="$OUT_DIR/xdg/state"
RUNTIME_DIR="$OUT_DIR/xdg/runtime"

for d in "$HOME_DIR" "$CONFIG_DIR" "$DATA_DIR" "$CACHE_DIR" "$STATE_DIR" "$RUNTIME_DIR"; do
  refuse_if_owner_dir "$d"
done
rm -rf "$HOME_DIR" "$CONFIG_DIR" "$DATA_DIR" "$CACHE_DIR" "$STATE_DIR" "$RUNTIME_DIR"
mkdir -p "$HOME_DIR" "$CONFIG_DIR" "$DATA_DIR" "$CACHE_DIR" "$STATE_DIR" "$RUNTIME_DIR"
chmod 700 "$RUNTIME_DIR"

# --- pick a free X display, never a caller-named :N -------------------------
DISP_N=""
for n in $(seq 90 199); do
  if [ ! -e "/tmp/.X11-unix/X$n" ]; then
    DISP_N="$n"
    break
  fi
done
if [ -z "$DISP_N" ]; then
  echo "catalog-proxy: could not find a free X display in :90-:199" >&2
  exit 3
fi
DISP=":$DISP_N"

XVFB_PID=""
APP_SID=""
APP_SID_STARTTIME=""

cleanup() {
  set +e
  if [ -n "$APP_SID" ] && [ -n "$APP_SID_STARTTIME" ]; then
    if [ "$(proc_starttime "$APP_SID")" = "$APP_SID_STARTTIME" ]; then
      kill -TERM -- "-$APP_SID" 2>/dev/null
      sleep 0.3
      kill -KILL -- "-$APP_SID" 2>/dev/null
    fi
  fi
  if [ -n "$XVFB_PID" ] && [ "$(proc_comm "$XVFB_PID")" = "Xvfb" ]; then
    kill "$XVFB_PID" 2>/dev/null
  fi
  wait 2>/dev/null
}
trap cleanup EXIT

nohup Xvfb "$DISP" -screen 0 1280x1024x24 -nolisten tcp >"$OUT_DIR/xvfb.log" 2>&1 &
XVFB_PID=$!
disown 2>/dev/null || true

waited=0
while [ "$waited" -lt 50 ]; do
  if DISPLAY="$DISP" xwininfo -root >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
  waited=$((waited + 1))
done
if ! DISPLAY="$DISP" xwininfo -root >/dev/null 2>&1; then
  echo "catalog-proxy: Xvfb did not come up on $DISP within 5s -- see $OUT_DIR/xvfb.log" >&2
  exit 3
fi

# --- env isolation: LANG=C by default (the catalog's own locale) -----------
LANG_ENV="C.UTF-8"
LANGUAGE_ENV=""
if [ "$LANG_ARG" = "fr" ]; then
  LANG_ENV="fr_FR.UTF-8"
  LANGUAGE_ENV="fr"
fi

: >"$OUT_DIR/app.log"
SESSION_PIDFILE="$OUT_DIR/session.pid"
rm -f "$SESSION_PIDFILE"

ENV_ARGS=(
  -u WAYLAND_DISPLAY -u LC_ALL -u LC_MESSAGES
  "HOME=$HOME_DIR"
  "DISPLAY=$DISP" "GDK_BACKEND=x11" "LIBGL_ALWAYS_SOFTWARE=1"
  "XDG_CONFIG_HOME=$CONFIG_DIR" "XDG_DATA_HOME=$DATA_DIR"
  "XDG_CACHE_HOME=$CACHE_DIR" "XDG_STATE_HOME=$STATE_DIR"
  "XDG_RUNTIME_DIR=$RUNTIME_DIR"
  "PIPEWIRE_RUNTIME_DIR=$RUNTIME_DIR"
  "PIPEWIRE_REMOTE=catalog-proxy-no-daemon"
  "LANG=$LANG_ENV"
  "LANGUAGE=$LANGUAGE_ENV"
  "RUST_LOG=${RUST_LOG:-info}"
)

setsid sh -c "echo \$\$ > '$SESSION_PIDFILE'; exec \"\$@\"" -- \
  env "${ENV_ARGS[@]}" dbus-run-session -- "$BINARY" \
  >"$OUT_DIR/app.log" 2>&1 &
disown 2>/dev/null || true

waited=0
while [ "$waited" -lt 30 ] && [ ! -s "$SESSION_PIDFILE" ]; do
  sleep 0.1
  waited=$((waited + 1))
done
if [ ! -s "$SESSION_PIDFILE" ]; then
  echo "catalog-proxy: the session leader never wrote its pid." >&2
  exit 1
fi
APP_SID="$(cat "$SESSION_PIDFILE")"
APP_SID_STARTTIME="$(proc_starttime "$APP_SID")"

# --- catalog worker.sh's own timing: 10s grace, then up to 20 x 1s polls ---
WAIT=0
WIN_FOUND=0
DIED=0
sleep 10
while [ "$WAIT" -lt 20 ]; do
  WAIT=$((WAIT + 1))
  if ! pid_running "$APP_SID"; then
    DIED=1
    break
  fi
  if DISPLAY="$DISP" timeout 5 xwininfo -tree -root 2>/dev/null | grep -qi '"CleanMic"'; then
    WIN_FOUND=1
    break
  fi
  sleep 1
done

TOTAL_SECS=$((10 + WAIT))
VERDICT="FAIL"
DETAIL=""

if [ "$WIN_FOUND" = 1 ]; then
  SHOT="$OUT_DIR/screenshot.png"
  if command -v import >/dev/null 2>&1; then
    DISPLAY="$DISP" import -window root "$SHOT" 2>>"$OUT_DIR/app.log" || true
  else
    DISPLAY="$DISP" magick import -window root "$SHOT" 2>>"$OUT_DIR/app.log" || true
  fi
  if ! pid_running "$APP_SID"; then
    VERDICT="FAIL"
    DETAIL="a window appeared, but the app was no longer alive right after the screenshot"
  else
    VERDICT="PASS"
    DETAIL="a window titled CleanMic appeared within ${TOTAL_SECS}s, and the app was still alive after the screenshot"
  fi
elif [ "$DIED" = 1 ]; then
  VERDICT="FAIL"
  DETAIL="ERROR: The application exited within ${TOTAL_SECS} seconds instead of showing a window"
else
  VERDICT="FAIL"
  DETAIL="no window titled CleanMic appeared within ${TOTAL_SECS}s (the app was still running)"
fi

{
  echo "# catalog-proxy report"
  echo
  echo "- Binary: $BINARY"
  echo "- SHA256: $(sha256sum "$BINARY" 2>/dev/null | cut -d' ' -f1)"
  echo "- Lang: $LANG_ARG"
  echo "- Display: $DISP (private Xvfb, 1280x1024x24)"
  echo "- Grace period: 10s, poll attempts used: ${WAIT}/20"
  echo "- Window found: $([ "$WIN_FOUND" = 1 ] && echo yes || echo no)"
  echo "- App died before a window appeared: $([ "$DIED" = 1 ] && echo yes || echo no)"
  echo "- Verdict: $VERDICT"
  echo "- Detail: $DETAIL"
  if [ "$VERDICT" = "PASS" ]; then
    echo "- Screenshot: screenshot.png"
  fi
  echo "- App log: app.log"
} >"$OUT_DIR/report.md"

echo "catalog-proxy: $VERDICT -- $DETAIL"
echo "catalog-proxy: report written to $OUT_DIR/report.md"

if [ "$VERDICT" = "PASS" ]; then
  exit 0
else
  exit 1
fi
