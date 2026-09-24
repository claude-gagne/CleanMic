#!/usr/bin/env bash
# nested-run.sh -- screen-aware nested-Xephyr test harness for CleanMic.
#
# WHY THIS EXISTS. The 2026-09-24 base-latency debug session drove CleanMic
# through a hand-built Xephyr + xdotool + PipeWire rig to get real
# mic-to-virtual-source latency numbers. That rig proved the fix (29f5397)
# but lived entirely in a scratchpad that will be gone by the next session.
# This script is the maintained, repo-owned replacement: it launches CleanMic
# into an isolated nested X server -- sized correctly for whatever screen the
# owner is currently on -- drives its UI reliably, and stops only what IT
# started. scripts/e2e-audio.sh drives this to run the actual audio tests.
#
# USAGE
#   scripts/nested-run.sh xephyr [:N] [--dry-run]
#   scripts/nested-run.sh launch [:N] [--lang fr|en] [--config 'KEY = VALUE']...
#       [--appimage PATH | --binary PATH] [--monitor-sink NAME]
#   scripts/nested-run.sh app-pid [:N]
#   scripts/nested-run.sh status [:N]
#   scripts/nested-run.sh shot [:N] FILE
#   scripts/nested-run.sh click [:N] X Y [--physical]
#   scripts/nested-run.sh key [:N] KEYSYM...
#   scripts/nested-run.sh scroll [:N] top|bottom
#   scripts/nested-run.sh targets [:N]
#   scripts/nested-run.sh click-target [:N] NAME [--expect REGEX] [--timeout S]
#   scripts/nested-run.sh check-layout [:N]
#   scripts/nested-run.sh stop [:N]
#   scripts/nested-run.sh reap
#
# :N defaults to ${CLEANMIC_NESTED_DISPLAY:-:47}. State lives under
# ${CLEANMIC_HARNESS_STATE:-REPO_ROOT/target/harness} (see SAFETY).
#
# EXIT CODES
#   0  ok
#   2  bad usage / bad --config
#   3  xrandr unreadable, or a required tool/locale/binary is missing
#   5  Xephyr did not come up or is not recorded for this display
#   6  the app never mapped its window
#  10  an owner file (config/autostart/desktop entry/icon) changed during a run
#  11  refused BEFORE anything was started or signalled: a foreign or
#      harness-marked CleanMic is already running, the display is in use by
#      something else, the state root or a private dir would be unsafe, or
#      monitor was asked for without a harness sink
#  12  stop/reap left harness processes alive
#  13  layout drift (check-layout): a pixel did not match the live config
#  14  a UI action was not confirmed by the app log within its timeout
#  15  a click-target's target is unreachable at the current viewport, or
#      there is no calibration for the recorded language
#
# SAFETY (see scripts/e2e/README.md for the full model).
#   - Every real CleanMic process is found by a MARKER
#     (CLEANMIC_HARNESS_LAUNCH + CLEANMIC_HARNESS_STATE_ROOT on its dbus-run-
#     session environment, which its bus and activated services inherit too)
#     and a recorded setsid session id, never by process name. `stop`/`reap`
#     never touch a cleanmic that lacks the marker for THIS state root.
#   - Xephyr is stopped by a recorded pid, only after /proc/<pid>/comm
#     confirms it really is Xephyr.
#   - The owner's real ~/.config/cleanmic/config.toml, autostart entry,
#     applications entry and icon are stat-stamped (never opened) at launch
#     and compared at stop; a change means an ALERT and exit 10.
#   - Nothing is ever `pkill`ed or `killall`ed by name.

set -euo pipefail

SCRIPT_DIR="$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

# ---------------------------------------------------------------------------
# need: verify required tools are on PATH, exit 3 with apt package names.
# ---------------------------------------------------------------------------
need() {
  local missing=()
  command -v Xephyr >/dev/null 2>&1 || missing+=("xserver-xephyr")
  command -v xdotool >/dev/null 2>&1 || missing+=("xdotool")
  command -v xdpyinfo >/dev/null 2>&1 || missing+=("x11-utils")
  command -v xrandr >/dev/null 2>&1 || missing+=("x11-xserver-utils")
  if ! command -v convert >/dev/null 2>&1 && ! command -v magick >/dev/null 2>&1; then
    missing+=("imagemagick")
  fi
  command -v pw-record >/dev/null 2>&1 || missing+=("pipewire-bin")
  command -v dbus-run-session >/dev/null 2>&1 || missing+=("dbus-bin")
  if [ "${#missing[@]}" -gt 0 ]; then
    echo "nested-run: missing tools -- install: ${missing[*]}" >&2
    exit 3
  fi
}

# Trim leading/trailing whitespace WITHOUT touching quote characters --
# `echo "$s" | xargs` looked equivalent but xargs re-tokenizes its input like
# a shell word list, silently stripping the quotes out of a TOML string
# literal such as `"Dpdfnet2"` and turning it into the bare (invalid-TOML)
# word `Dpdfnet2`.
trim() {
  local s="$1"
  s="${s#"${s%%[![:space:]]*}"}"
  s="${s%"${s##*[![:space:]]}"}"
  printf '%s' "$s"
}

# ---------------------------------------------------------------------------
# State root: never /, $HOME, or under an owner XDG dir (T-bjk-05).
# ---------------------------------------------------------------------------
STATE_ROOT="$(realpath -m -- "${CLEANMIC_HARNESS_STATE:-$REPO_ROOT/target/harness}")"

refuse_unsafe_state_root() {
  local bad=0 d rd
  [ "$STATE_ROOT" = "/" ] && bad=1
  [ "$STATE_ROOT" = "$HOME" ] && bad=1
  for d in "$HOME/.config" "$HOME/.local/share" "$HOME/.cache" "$HOME/.local/state"; do
    rd="$(realpath -m -- "$d")"
    case "$STATE_ROOT" in
      "$rd" | "$rd"/*) bad=1 ;;
    esac
  done
  if [ "$bad" = 1 ]; then
    echo "nested-run: REFUSING -- state root '$STATE_ROOT' is unsafe (/, \$HOME, or an owner XDG dir)." >&2
    exit 11
  fi
}
refuse_unsafe_state_root
mkdir -p "$STATE_ROOT"
[ -f "$STATE_ROOT/.cleanmic-harness-owned" ] || : >"$STATE_ROOT/.cleanmic-harness-owned"

# A private dir must resolve under STATE_ROOT and never collide with an owner
# XDG dir. Exits 11 on violation.
refuse_if_owner_dir() {
  local path="$1" resolved rd d
  resolved="$(realpath -m -- "$path")"
  case "$resolved" in
    "$STATE_ROOT" | "$STATE_ROOT"/*) : ;;
    *)
      echo "nested-run: REFUSING -- private path '$resolved' is not under the state root '$STATE_ROOT'." >&2
      exit 11
      ;;
  esac
  for d in "$HOME/.config" "$HOME/.local/share" "$HOME/.cache" "$HOME/.local/state"; do
    rd="$(realpath -m -- "$d")"
    case "$resolved" in
      "$rd" | "$rd"/*)
        echo "nested-run: REFUSING -- private path '$resolved' resolves under the owner's $d." >&2
        exit 11
        ;;
    esac
  done
}

display_dir() { echo "$STATE_ROOT/display-${1#:}"; }

# Parse an optional leading ":N" display argument; sets DISP and REST[].
# Falls back to CLEANMIC_NESTED_DISPLAY, then :47.
parse_display() {
  DISP="${CLEANMIC_NESTED_DISPLAY:-:47}"
  REST=("$@")
  if [ "${#REST[@]}" -gt 0 ] && [[ "${REST[0]}" =~ ^:[0-9]+$ ]]; then
    DISP="${REST[0]}"
    REST=("${REST[@]:1}")
  fi
}

# ---------------------------------------------------------------------------
# Process ownership helpers (T-bjk-03): marker + recorded session, never name.
# ---------------------------------------------------------------------------

harness_marker_n() { echo "nested-run:${1#:}"; }

# Read /proc/<pid>/environ (NUL-separated) as newline-separated lines, or
# nothing at all. Uses `cat FILE 2>/dev/null` rather than a shell input
# redirection: a shell `<FILE` redirection failure (permission denied on
# another user's or a zombie's /proc/<pid>/environ) is reported by the shell
# itself BEFORE any `2>/dev/null` written after it takes effect (redirections
# apply strictly left-to-right), which leaked "Permission denied" onto the
# terminal during a routine marker sweep. `cat`'s own open() failure is a
# normal command failure that its own stderr redirection suppresses cleanly.
read_environ() { cat "/proc/$1/environ" 2>/dev/null | tr '\0' '\n'; }

# Does pid $1 carry BOTH harness marker values for display $2 (":N")? An
# unreadable environ (zombie, gone, another user's) is never ours.
has_harness_mark() {
  local pid="$1" disp="$2" want_launch want_root env_text
  env_text="$(read_environ "$pid")"
  [ -n "$env_text" ] || return 1
  want_launch="CLEANMIC_HARNESS_LAUNCH=$(harness_marker_n "$disp")"
  want_root="CLEANMIC_HARNESS_STATE_ROOT=$STATE_ROOT"
  printf '%s\n' "$env_text" | grep -qxF "$want_launch" &&
    printf '%s\n' "$env_text" | grep -qxF "$want_root"
}

# Marker sweep across every own-uid process, ignoring display (used by reap
# and the marker-sweep step of stop).
has_harness_mark_any_display() {
  local pid="$1" want_root env_text
  env_text="$(read_environ "$pid")"
  [ -n "$env_text" ] || return 1
  want_root="CLEANMIC_HARNESS_STATE_ROOT=$STATE_ROOT"
  printf '%s\n' "$env_text" | grep -q '^CLEANMIC_HARNESS_LAUNCH=nested-run:' &&
    printf '%s\n' "$env_text" | grep -qxF "$want_root"
}

pid_running() {
  local st
  st=$(awk '{print $3}' "/proc/$1/stat" 2>/dev/null) || return 1
  [ -n "$st" ] && [ "$st" != "Z" ]
}

proc_starttime() { awk '{print $22}' "/proc/$1/stat" 2>/dev/null || true; }

proc_comm() { cat "/proc/$1/comm" 2>/dev/null || true; }

# ---------------------------------------------------------------------------
# xephyr [:N] [--dry-run]  (R1)
# ---------------------------------------------------------------------------
cmd_xephyr() {
  parse_display "$@"
  need
  local dry_run=0 a
  for a in "${REST[@]:-}"; do [ "$a" = "--dry-run" ] && dry_run=1; done

  local dd; dd="$(display_dir "$DISP")"
  mkdir -p "$dd"

  # Step 0: is EVERY connected output (with a geometry) named XWAYLAND*? If
  # so this is a nested/virtual X session with no real screen to detect --
  # fall back to the largest at scale 1 and tell the caller how to override.
  local xw_only xw_out xw_screen
  xw_only=$(xrandr 2>/dev/null | awk '
    / connected/ {
      has=0
      for (i = 1; i <= NF; i++) if ($i ~ /^[0-9]+x[0-9]+\+/) has=1
      if (has) { total++; if ($1 ~ /^XWAYLAND/) xw++ }
    }
    END { if (total > 0 && total == xw) print "yes"; else print "no" }')

  local screen_out="" screen="" kind="" auto_scale=""
  if [ "$xw_only" = "yes" ]; then
    read -r xw_out xw_screen <<XRANDR_XW
$(xrandr 2>/dev/null | awk '
  / connected/ {
    for (i = 1; i <= NF; i++) if ($i ~ /^[0-9]+x[0-9]+\+/) {
      split($i, a, "+"); split(a[1], d, "x")
      if (d[1] * d[2] > best) { best = d[1] * d[2]; nm = $1; geo = a[1] }
    }
  }
  END { if (nm != "") print nm, geo }')
XRANDR_XW
    screen_out="$xw_out"; screen="$xw_screen"; kind="xwayland-fallback"; auto_scale=1
    echo "nested-run: NOTE -- every connected output is XWAYLAND*; using ${screen_out} at scale 1. Set CLEANMIC_GDK_SCALE=2 (with CLEANMIC_ALLOW_TINY=1 if smaller) when on the big screen." >&2
  else
    read -r screen_out screen <<XRANDR_EXT
$(xrandr 2>/dev/null | awk '
  / connected/ && $1 !~ /^(eDP|LVDS)/ {
    for (i = 1; i <= NF; i++) if ($i ~ /^[0-9]+x[0-9]+\+/) {
      split($i, a, "+"); split(a[1], d, "x")
      if (d[1] * d[2] > best) { best = d[1] * d[2]; nm = $1; geo = a[1] }
    }
  }
  END { if (nm != "") print nm, geo }')
XRANDR_EXT
    if [ -n "${screen:-}" ]; then
      kind="external"; auto_scale=2
    else
      read -r screen_out screen <<XRANDR_LAPTOP
$(xrandr 2>/dev/null | awk '
  / connected/ {
    for (i = 1; i <= NF; i++) if ($i ~ /^[0-9]+x[0-9]+\+/) {
      split($i, a, "+"); print $1, a[1]; exit
    }
  }')
XRANDR_LAPTOP
      kind="laptop"; auto_scale=1
    fi
  fi

  if [ -z "${screen:-}" ]; then
    echo "nested-run: could not read a usable screen from xrandr -- refusing to guess." >&2
    exit 3
  fi

  local sw sh
  sw="${screen%x*}"; sh="${screen#*x}"
  case "$sw$sh" in
    *[!0-9]*)
      echo "nested-run: unreadable screen geometry '$screen' from xrandr." >&2
      exit 3
      ;;
  esac

  # Scale override: a smaller-than-auto CLEANMIC_GDK_SCALE is ignored unless
  # CLEANMIC_ALLOW_TINY=1 (BetterWeather's tiny-window guard).
  local scale="${CLEANMIC_GDK_SCALE:-$auto_scale}"
  if [ "$scale" -lt "$auto_scale" ] && [ "${CLEANMIC_ALLOW_TINY:-0}" != "1" ]; then
    echo "nested-run: IGNORING CLEANMIC_GDK_SCALE=${scale} -- this is the ${kind} screen, whose rule is scale ${auto_scale}." >&2
    echo "  A smaller render here is almost always an override left over from another run." >&2
    echo "  If you really mean it, also set CLEANMIC_ALLOW_TINY=1." >&2
    scale="$auto_scale"
  fi

  # Logical size 520x1300 (420-wide window + combo-popover headroom), clamped
  # to the screen -- never the scale.
  local lw=520 lh=1300
  local pw=$((lw * scale)) ph=$((lh * scale))
  local max_w=$((sw - 40)) max_h=$((sh - 120))
  [ "$pw" -gt "$max_w" ] && pw="$max_w"
  [ "$ph" -gt "$max_h" ] && ph="$max_h"

  if [ $((ph / scale)) -lt 700 ]; then
    echo "nested-run: NOTE -- physical height ${ph} at scale ${scale} gives only $((ph / scale)) logical px; some named targets near the bottom may be UNREACHABLE (exit 15)." >&2
  fi

  echo "nested-run: ${screen_out} (${kind}) screen ${sw}x${sh} | scale ${scale} | physical ${pw}x${ph} | logical $((pw / scale))x$((ph / scale))"

  if [ "$dry_run" = 1 ]; then
    exit 0
  fi

  # Refuse a foreign display already listening on :N (something we did not
  # start), unless it is our own recorded Xephyr at the same scale/geometry.
  local xpidfile="$dd/xephyr.pid"
  local existing_pid="" existing_scale="" existing_geom=""
  [ -f "$xpidfile" ] && existing_pid="$(cat "$xpidfile" 2>/dev/null || true)"
  [ -f "$dd/scale" ] && existing_scale="$(cat "$dd/scale" 2>/dev/null || true)"
  [ -f "$dd/geometry" ] && existing_geom="$(cat "$dd/geometry" 2>/dev/null || true)"

  local ours_alive=0
  if [ -n "$existing_pid" ] && [ "$(proc_comm "$existing_pid")" = "Xephyr" ] && DISPLAY="$DISP" xdpyinfo >/dev/null 2>&1; then
    ours_alive=1
  fi

  if [ "$ours_alive" = 1 ]; then
    if [ "$existing_scale" = "$scale" ] && [ "$existing_geom" = "${pw}x${ph}" ]; then
      echo "nested-run: reusing existing Xephyr on $DISP at ${pw}x${ph} scale ${scale}."
      exit 0
    else
      echo "nested-run: REFUSING -- Xephyr already up on $DISP at ${existing_geom} scale ${existing_scale}, asked ${pw}x${ph} scale ${scale}. stop $DISP first." >&2
      exit 11
    fi
  fi

  if [ -e "/tmp/.X11-unix/X${DISP#:}" ] || DISPLAY="$DISP" xdpyinfo >/dev/null 2>&1; then
    echo "nested-run: REFUSING -- $DISP is already in use by something this harness did not start." >&2
    exit 11
  fi

  nohup Xephyr "$DISP" -screen "${pw}x${ph}" -ac -noreset -nolisten tcp \
    -title "CleanMic harness $DISP" >"$dd/xephyr.log" 2>&1 &
  local xpid=$!
  disown 2>/dev/null || true
  echo "$xpid" >"$xpidfile"
  echo "$scale" >"$dd/scale"
  echo "${pw}x${ph}" >"$dd/geometry"
  echo "${sw}x${sh}" >"$dd/screen"

  local waited=0
  while [ "$waited" -lt 50 ]; do
    if DISPLAY="$DISP" xdpyinfo >/dev/null 2>&1; then
      echo "nested-run: Xephyr up on $DISP at ${pw}x${ph} (scale ${scale})."
      return 0
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  echo "nested-run: Xephyr did not come up on $DISP within 5s -- see $dd/xephyr.log" >&2
  exit 5
}

# ---------------------------------------------------------------------------
# Xephyr liveness check used by launch/click/etc: pid + comm + xdpyinfo.
# ---------------------------------------------------------------------------
xephyr_alive() {
  local disp="$1" dd xpid
  dd="$(display_dir "$disp")"
  [ -f "$dd/xephyr.pid" ] || return 1
  xpid="$(cat "$dd/xephyr.pid" 2>/dev/null || true)"
  [ -n "$xpid" ] || return 1
  [ "$(proc_comm "$xpid")" = "Xephyr" ] || return 1
  DISPLAY="$disp" xdpyinfo >/dev/null 2>&1
}

# ---------------------------------------------------------------------------
# launch [:N] [--lang fr|en] [--config 'KEY = VALUE']... [--appimage PATH |
#        --binary PATH] [--monitor-sink NAME]   (R2, R4)
# ---------------------------------------------------------------------------

# The known top-level Config fields (src/config.rs) a --config override may
# touch, plus the strengths.<Engine> form.
CONFIG_TOP_KEYS="input_device engine mode monitor_enabled enabled autostart khip_library_path tray_hint_shown tray_absent_notified autostart_hidden_notified last_seen_update_version auto_gain_enabled dpdfnet_default_migration_complete"

cmd_launch() {
  parse_display "$@"
  local lang="en" appimage="" binary="" monitor_sink=""
  local -a config_kvs=()
  local i=0 args=("${REST[@]:-}")
  while [ "$i" -lt "${#args[@]}" ]; do
    case "${args[$i]}" in
      --lang)
        i=$((i + 1)); lang="${args[$i]:-}"
        ;;
      --config)
        i=$((i + 1)); config_kvs+=("${args[$i]:-}")
        ;;
      --appimage)
        i=$((i + 1)); appimage="${args[$i]:-}"
        ;;
      --binary)
        i=$((i + 1)); binary="${args[$i]:-}"
        ;;
      --monitor-sink)
        i=$((i + 1)); monitor_sink="${args[$i]:-}"
        ;;
      "")
        ;;
      *)
        echo "nested-run: launch: unknown option '${args[$i]}'" >&2
        exit 2
        ;;
    esac
    i=$((i + 1))
  done

  case "$lang" in
    fr | en) : ;;
    *)
      echo "nested-run: launch: --lang must be fr or en, got '$lang'" >&2
      exit 2
      ;;
  esac

  # --- Step 1: FOREIGN-CLEANMIC GUARD FIRST -------------------------------
  local pid
  for pid in $(pgrep -x cleanmic 2>/dev/null || true); do
    if has_harness_mark "$pid" "$DISP"; then
      echo "nested-run: REFUSING -- a marked cleanmic (pid $pid) is already running on $DISP. stop $DISP first." >&2
      exit 11
    fi
    if has_harness_mark_any_display "$pid"; then
      echo "nested-run: REFUSING -- a marked cleanmic (pid $pid) is running on another harness display. stop that display first." >&2
      exit 11
    fi
    local cmdline
    cmdline="$(cat "/proc/$pid/cmdline" 2>/dev/null | tr '\0' ' ' || true)"
    echo "nested-run: REFUSING -- an unmarked cleanmic is already running: pid $pid, cmdline: $cmdline" >&2
    echo "  The runtime lock \$XDG_RUNTIME_DIR/cleanmic.lock would block a second instance anyway; the harness does not touch it." >&2
    exit 11
  done

  # --- Step 2: ARGUMENT AND CONFIG VALIDATION, no side effects yet -------
  local kv key value seen_monitor_true=0
  for kv in "${config_kvs[@]:-}"; do
    [ -z "$kv" ] && continue
    if [[ "$kv" != *"="* ]]; then
      echo "nested-run: launch: bad --config '$kv' (expected KEY = VALUE)" >&2
      exit 2
    fi
    key="$(trim "${kv%%=*}")"
    value="$(trim "${kv#*=}")"
    case "$key" in
      strengths.*) : ;;
      *)
        local ok=0 tk
        for tk in $CONFIG_TOP_KEYS; do [ "$tk" = "$key" ] && ok=1; done
        if [ "$ok" != 1 ]; then
          echo "nested-run: launch: unknown --config key '$key'. Known keys: $CONFIG_TOP_KEYS strengths.<Engine>" >&2
          exit 2
        fi
        ;;
    esac
    if ! python3 -c "
import tomllib, sys
try:
    tomllib.loads('x = ' + sys.argv[1])
except Exception as e:
    sys.exit(1)
" "$value" >/dev/null 2>&1; then
      echo "nested-run: launch: --config value for '$key' is not a valid TOML literal: $value" >&2
      exit 2
    fi
    if [ "$key" = "monitor_enabled" ] && [ "$value" = "true" ]; then
      seen_monitor_true=1
    fi
  done
  if [ "$seen_monitor_true" = 1 ] && [ -z "$monitor_sink" ]; then
    echo "nested-run: REFUSING -- monitor_enabled=true without --monitor-sink would play into the owner's real default sink." >&2
    exit 11
  fi
  if [ -n "$monitor_sink" ] && [[ "$monitor_sink" != cmtest_* ]]; then
    echo "nested-run: REFUSING -- --monitor-sink '$monitor_sink' does not start with cmtest_." >&2
    exit 11
  fi

  # --- Step 3: recorded Xephyr for :N must be alive -----------------------
  if ! xephyr_alive "$DISP"; then
    echo "nested-run: launch: no recorded, live Xephyr on $DISP -- run 'xephyr $DISP' first." >&2
    exit 5
  fi

  # --- Step 4: pick the binary ---------------------------------------------
  local bin=""
  if [ -n "$binary" ]; then
    bin="$binary"
  elif [ -n "$appimage" ]; then
    bin="$appimage"
  else
    bin="$(ls -t "$REPO_ROOT"/build/CleanMic-*.AppImage 2>/dev/null | head -1 || true)"
  fi
  if [ -z "$bin" ] || [ ! -e "$bin" ]; then
    echo "nested-run: launch: no binary found (looked for --binary, --appimage, build/CleanMic-*.AppImage)." >&2
    exit 3
  fi
  echo "nested-run: binary: $bin"

  # --- Step 5: private homes -----------------------------------------------
  local dd; dd="$(display_dir "$DISP")"
  local home_dir="$dd/home"
  rm -rf "$home_dir"
  mkdir -p "$home_dir/config" "$home_dir/data" "$home_dir/cache" "$home_dir/state"
  refuse_if_owner_dir "$home_dir/config"
  refuse_if_owner_dir "$home_dir/data"
  refuse_if_owner_dir "$home_dir/cache"
  refuse_if_owner_dir "$home_dir/state"

  # --- Step 6: write config.toml -------------------------------------------
  mkdir -p "$home_dir/config/cleanmic"
  local cfg="$home_dir/config/cleanmic/config.toml"
  python3 - "$cfg" "${config_kvs[@]:-}" <<'PYEOF' || exit 2
import sys

cfg_path = sys.argv[1]
kvs = [kv for kv in sys.argv[2:] if kv]

BASE_TOP = {
    "input_device": '"cmtest_mic"',
    "engine": '"Dpdfnet2"',
    "mode": '"MaxQuality"',
    "monitor_enabled": "false",
    "enabled": "true",
    "autostart": "false",
    "tray_hint_shown": "true",
    "tray_absent_notified": "true",
    "autostart_hidden_notified": "true",
    "auto_gain_enabled": "true",
    "dpdfnet_default_migration_complete": "true",
}
STRENGTHS = {"RNNoise": "0.5", "DeepFilterNet": "0.5", "Dpdfnet2": "0.5", "Dpdfnet8": "0.5", "Khip": "0.5"}

top_overrides = {}
strength_overrides = {}
for kv in kvs:
    if "=" not in kv:
        continue
    key, value = kv.split("=", 1)
    key = key.strip()
    value = value.strip()
    if key.startswith("strengths."):
        strength_overrides[key.split(".", 1)[1]] = value
    else:
        top_overrides[key] = value

top = dict(BASE_TOP)
top.update(top_overrides)
strengths = dict(STRENGTHS)
strengths.update(strength_overrides)

lines = [f"{k} = {v}" for k, v in top.items()]
lines.append("")
lines.append("[strengths]")
lines.extend(f"{k} = {v}" for k, v in strengths.items())
text = "\n".join(lines) + "\n"

with open(cfg_path, "w", encoding="utf-8") as fh:
    fh.write(text)

try:
    import tomllib
    with open(cfg_path, "rb") as fh:
        tomllib.load(fh)
except Exception as exc:  # noqa: BLE001
    print(f"nested-run: launch: written config.toml is invalid TOML: {exc}", file=sys.stderr)
    sys.exit(1)
PYEOF

  # --- Step 7: monitor-sink shim --------------------------------------------
  if [ -n "$monitor_sink" ]; then
    local real_pw_metadata shim_dir
    real_pw_metadata="$(command -v pw-metadata || true)"
    if [ -z "$real_pw_metadata" ]; then
      echo "nested-run: launch: pw-metadata not found on PATH (needed for --monitor-sink)." >&2
      exit 3
    fi
    shim_dir="$dd/shim"
    mkdir -p "$shim_dir"
    cat >"$shim_dir/pw-metadata" <<SHIM
#!/usr/bin/env bash
set -euo pipefail
if [ "\$#" -ge 4 ] && [ "\$1" = "0" ] && { [ "\$3" = "default.configured.audio.sink" ] || [ "\$3" = "default.audio.sink" ]; }; then
  echo 'Found "default" metadata 0'
  echo "update: id:0 key:'\$3' value:'{\"name\":\"${monitor_sink}\"}' type:'Spa:String:JSON'"
  exit 0
fi
exec "$real_pw_metadata" "\$@"
SHIM
    chmod +x "$shim_dir/pw-metadata"
    echo "$monitor_sink" >"$dd/monitor-sink"
  else
    rm -f "$dd/monitor-sink"
  fi

  # --- Step 8: owner-file stamps --------------------------------------------
  stamp_owner_files >"$dd/owner-files.stamp"

  # --- Step 9: start in a new session --------------------------------------
  local -a env_args=(
    -u WAYLAND_DISPLAY -u LC_ALL -u LC_MESSAGES -u LANGUAGE
    "DISPLAY=$DISP" "GDK_BACKEND=x11" "GDK_SCALE=$(cat "$dd/scale")"
    "GTK_A11Y=none" "LIBGL_ALWAYS_SOFTWARE=1"
    "XDG_CONFIG_HOME=$home_dir/config" "XDG_DATA_HOME=$home_dir/data"
    "XDG_CACHE_HOME=$home_dir/cache" "XDG_STATE_HOME=$home_dir/state"
    "CLEANMIC_HARNESS_LAUNCH=$(harness_marker_n "$DISP")"
    "CLEANMIC_HARNESS_STATE_ROOT=$STATE_ROOT"
    "RUST_LOG=${RUST_LOG:-info}"
  )
  if [ -n "$monitor_sink" ]; then
    env_args+=("PATH=$dd/shim:$PATH")
  fi
  if [ "$lang" = "fr" ]; then
    if ! locale -a 2>/dev/null | grep -qi '^fr_FR\.utf8$'; then
      echo "nested-run: launch: fr_FR.utf8 locale is not installed." >&2
      exit 3
    fi
    env_args+=("LANG=fr_FR.UTF-8" "LANGUAGE=fr")
  else
    env_args+=("LANG=C.UTF-8")
  fi

  : >"$dd/app.log"
  local session_pidfile="$dd/session.pid"
  rm -f "$session_pidfile"
  setsid sh -c "echo \$\$ > '$session_pidfile'; exec \"\$@\"" -- \
    env "${env_args[@]}" dbus-run-session -- "$bin" \
    >"$dd/app.log" 2>&1 &
  disown 2>/dev/null || true

  local waited=0
  while [ "$waited" -lt 30 ] && [ ! -s "$session_pidfile" ]; do
    sleep 0.1; waited=$((waited + 1))
  done
  if [ ! -s "$session_pidfile" ]; then
    echo "nested-run: launch: the session leader never wrote its pid." >&2
    exit 11
  fi
  local sid; sid="$(cat "$session_pidfile")"
  proc_starttime "$sid" >"$dd/session.starttime"
  echo "$lang" >"$dd/lang"
  echo "$bin" >"$dd/binary"

  # --- Step 10: readiness ---------------------------------------------------
  waited=0
  local win=""
  while [ "$waited" -lt 400 ]; do
    if ! pid_running "$sid"; then
      echo "nested-run: launch: the session leader died before the window appeared. Last log lines:" >&2
      tail -n 15 "$dd/app.log" >&2 || true
      exit 11
    fi
    if grep -q "Another CleanMic instance is already running" "$dd/app.log" 2>/dev/null; then
      echo "nested-run: launch: app.log shows the single-instance lock refused us. Last log lines:" >&2
      tail -n 15 "$dd/app.log" >&2 || true
      exit 11
    fi
    win="$(DISPLAY="$DISP" xdotool search --onlyvisible --name '^CleanMic$' 2>/dev/null | head -1 || true)"
    [ -n "$win" ] && break
    sleep 0.1
    waited=$((waited + 1))
  done
  if [ -z "$win" ]; then
    echo "nested-run: launch: CleanMic's window never mapped on $DISP within 40s. Last log lines:" >&2
    tail -n 15 "$dd/app.log" >&2 || true
    exit 6
  fi
  echo "$win" >"$dd/window.id"
  echo "nested-run: launched $bin on $DISP (scale $(cat "$dd/scale"), lang $lang) -- log $dd/app.log, private config $cfg"
}

# Stat every owner file we must never modify (used by launch's stamp and
# stop's compare). Literal $HOME paths, never opened.
stamp_owner_files() {
  local f
  for f in \
    "$HOME/.config/cleanmic/config.toml" \
    "$HOME/.config/autostart/com.cleanmic.CleanMic.desktop" \
    "$HOME/.local/share/applications/com.cleanmic.CleanMic.desktop" \
    "$HOME/.local/share/icons/hicolor/scalable/apps/com.cleanmic.CleanMic.svg"; do
    if [ -e "$f" ]; then
      echo "$f $(stat -c '%Y:%s' "$f" 2>/dev/null || echo ABSENT)"
    else
      echo "$f ABSENT"
    fi
  done
}

# ---------------------------------------------------------------------------
# app-pid [:N]
# ---------------------------------------------------------------------------
cmd_app_pid() {
  parse_display "$@"
  local dd; dd="$(display_dir "$DISP")"
  [ -f "$dd/session.pid" ] || { echo "nested-run: app-pid: no session recorded for $DISP" >&2; exit 1; }
  local sid; sid="$(cat "$dd/session.pid")"
  local pid
  for pid in $(pgrep -x cleanmic 2>/dev/null || true); do
    local psid; psid="$(ps -o sid= -p "$pid" 2>/dev/null | tr -d ' ' || true)"
    if [ "$psid" = "$sid" ] && has_harness_mark "$pid" "$DISP"; then
      echo "$pid"
      return 0
    fi
  done
  echo "nested-run: app-pid: no marked cleanmic in session $sid on $DISP" >&2
  exit 1
}

# ---------------------------------------------------------------------------
# status [:N]
# ---------------------------------------------------------------------------
cmd_status() {
  parse_display "$@"
  local dd; dd="$(display_dir "$DISP")"
  echo "nested-run: status $DISP (state root $STATE_ROOT)"
  if xephyr_alive "$DISP"; then
    echo "  Xephyr: alive (pid $(cat "$dd/xephyr.pid" 2>/dev/null), scale $(cat "$dd/scale" 2>/dev/null), geometry $(cat "$dd/geometry" 2>/dev/null))"
  else
    echo "  Xephyr: not running"
  fi
  if [ -f "$dd/session.pid" ]; then
    local sid; sid="$(cat "$dd/session.pid")"
    if pid_running "$sid"; then
      echo "  session: alive (sid $sid, lang $(cat "$dd/lang" 2>/dev/null))"
    else
      echo "  session: recorded but dead (sid $sid)"
    fi
  else
    echo "  session: none recorded"
  fi
  if app_pid_out=$(cmd_app_pid "$DISP" 2>/dev/null); then
    echo "  app: alive (pid $app_pid_out)"
  else
    echo "  app: not running"
  fi
}

# ---------------------------------------------------------------------------
# reap_xephyr: stop the Xephyr recorded for $1 (":N"), or every recorded
# Xephyr when $1 is empty. Killed by recorded pid, only after the comm check.
# ---------------------------------------------------------------------------
reap_xephyr() {
  local only="${1:-}" dir disp xpid
  for dir in "$STATE_ROOT"/display-*; do
    [ -d "$dir" ] || continue
    disp=":${dir##*display-}"
    [ -n "$only" ] && [ "$disp" != "$only" ] && continue
    [ -f "$dir/xephyr.pid" ] || continue
    xpid="$(cat "$dir/xephyr.pid" 2>/dev/null || true)"
    if [ -n "$xpid" ] && [ "$(proc_comm "$xpid")" = "Xephyr" ]; then
      kill "$xpid" 2>/dev/null || true
      echo "nested-run: stopped Xephyr on $disp (pid $xpid)."
    fi
    rm -f "$dir/xephyr.pid"
  done
}

# Compare the owner-file stamps recorded at launch against the current
# state. Prints an ALERT and returns 1 on a genuine change. A foreign
# cleanmic being active at compare time downgrades ALERT to a NOTE (still
# returns 1 so the caller can decide, but does not imply harness tampering).
compare_owner_stamps() {
  local dd="$1" before after label
  [ -f "$dd/owner-files.stamp" ] || return 0
  before="$dd/owner-files.stamp"
  after="$(stamp_owner_files)"
  if [ "$after" = "$(cat "$before")" ]; then
    return 0
  fi
  local foreign_running=0
  local pid
  for pid in $(pgrep -x cleanmic 2>/dev/null || true); do
    has_harness_mark_any_display "$pid" || foreign_running=1
  done
  if [ "$foreign_running" = 1 ]; then
    echo "nested-run: NOTE -- an owner file changed, but a foreign (non-harness) cleanmic is running -- likely theirs, not this run's." >&2
  else
    echo "nested-run: ALERT -- an owner file (config/autostart/desktop entry/icon) changed during this run!" >&2
    echo "  before: $(cat "$before")" >&2
    echo "  after:  $after" >&2
  fi
  return 1
}

# TERM then KILL every marked cleanmic for display $1 (":N"), waiting for
# session end in between. Returns the count of processes it signalled.
kill_harness_app_session() {
  local disp="$1" dd sid n=0 pid live
  dd="$(display_dir "$disp")"
  [ -f "$dd/session.pid" ] || return 0
  sid="$(cat "$dd/session.pid")"
  [ -n "$sid" ] || return 0
  if [ "$(proc_starttime "$sid")" != "$(cat "$dd/session.starttime" 2>/dev/null || true)" ]; then
    return 0 # pid recycled onto something else -- not ours
  fi
  for pid in $(pgrep -x cleanmic 2>/dev/null || true); do
    local psid; psid="$(ps -o sid= -p "$pid" 2>/dev/null | tr -d ' ' || true)"
    if [ "$psid" = "$sid" ] && has_harness_mark "$pid" "$disp"; then
      kill "$pid" 2>/dev/null && n=$((n + 1))
    fi
  done
  local waited=0
  while [ "$waited" -lt 100 ] && pid_running "$sid"; do sleep 0.1; waited=$((waited + 1)); done
  if ps -s "$sid" -o pid= >/dev/null 2>&1 && [ -n "$(ps -s "$sid" -o pid= 2>/dev/null)" ]; then
    kill -TERM -- "-$sid" 2>/dev/null || true
    sleep 0.3
    if [ "$(proc_starttime "$sid")" = "$(cat "$dd/session.starttime" 2>/dev/null || true)" ]; then
      kill -KILL -- "-$sid" 2>/dev/null || true
    fi
  fi
  echo "$n"
}

# TERM/KILL every own-uid process still carrying the marker for this state
# root (any display) -- the marker sweep. Never matches by name.
marker_sweep_kill() {
  local pid n=0
  for pid in $(ls /proc 2>/dev/null | grep -E '^[0-9]+$' || true); do
    has_harness_mark_any_display "$pid" || continue
    kill "$pid" 2>/dev/null && n=$((n + 1))
  done
  if [ "$n" -gt 0 ]; then
    sleep 0.5
    for pid in $(ls /proc 2>/dev/null | grep -E '^[0-9]+$' || true); do
      has_harness_mark_any_display "$pid" || continue
      kill -KILL "$pid" 2>/dev/null || true
    done
  fi
  echo "$n"
}

# NOTE ON set -e HERE: this function (and several above/below it) is always
# invoked via `x="$(fn ...)"`. A bash command substitution runs in its own
# subshell WITHOUT `errexit` unless `shopt -s inherit_errexit` is set (it is
# not, here) -- so a failing command mid-body never aborts early. What DOES
# matter is the subshell's FINAL exit status, which becomes the whole
# substitution's exit status; the caller's plain assignment (not an if/&&/||
# condition) IS subject to `set -e`, so a function ending on a command that
# can fail (like a bare `for` loop whose last match condition is false) can
# silently abort the entire script. Every such function below ends on an
# explicit, always-succeeding statement for this reason.
harness_survivors() {
  local pid
  for pid in $(ls /proc 2>/dev/null | grep -E '^[0-9]+$' || true); do
    has_harness_mark_any_display "$pid" && echo "$pid"
  done
  return 0
}

# ---------------------------------------------------------------------------
# stop [:N]   (R4)
# ---------------------------------------------------------------------------
cmd_stop() {
  parse_display "$@"
  local dd; dd="$(display_dir "$DISP")"
  local closed_app; closed_app="$(kill_harness_app_session "$DISP")"
  echo "nested-run: stop $DISP -- closed ${closed_app:-0} app process(es)."
  local closed_marker; closed_marker="$(marker_sweep_kill)"
  [ "${closed_marker:-0}" -gt 0 ] && echo "nested-run: stop $DISP -- marker sweep closed ${closed_marker} process(es)."
  reap_xephyr "$DISP"

  local stamp_rc=0
  compare_owner_stamps "$dd" || stamp_rc=$?

  local survivors; survivors="$(harness_survivors)"
  rm -f "$dd/session.pid" "$dd/session.starttime" "$dd/window.id"

  if [ "$stamp_rc" != 0 ]; then
    local foreign=0 pid
    for pid in $(pgrep -x cleanmic 2>/dev/null || true); do
      has_harness_mark_any_display "$pid" || foreign=1
    done
    [ "$foreign" = 0 ] && return 10
  fi
  if [ -n "$survivors" ]; then
    echo "nested-run: stop $DISP -- survivors remain: $survivors" >&2
    return 12
  fi
  return 0
}

# ---------------------------------------------------------------------------
# reap: stop every recorded display.
#
# cmd_stop RETURNS its exit code rather than calling `exit` directly (unlike
# the other cmd_* functions) precisely so this loop can call it repeatedly in
# the SAME process and keep going after a non-fatal per-display code -- an
# `exit` inside cmd_stop would have ended `reap` after its first display.
# ---------------------------------------------------------------------------
cmd_reap() {
  local dir disp rc=0
  for dir in "$STATE_ROOT"/display-*; do
    [ -d "$dir" ] || continue
    disp=":${dir##*display-}"
    if ! cmd_stop "$disp"; then rc=1; fi
  done
  exit "$rc"
}

# ---------------------------------------------------------------------------
# UI driving (R3): logical<->physical coordinates, geometry, calibrated
# targets. All coordinates on the command line are LOGICAL unless --physical
# is given; physical = logical * the recorded per-display scale.
# ---------------------------------------------------------------------------

TARGETS_TSV="$SCRIPT_DIR/nested-run-targets.tsv"

image_tool() {
  if command -v magick >/dev/null 2>&1; then
    echo "magick"
  else
    echo "convert"
  fi
}

crop_pixel_txt() {
  local file="$1" x="$2" y="$3"
  if [ "$(image_tool)" = "magick" ]; then
    magick "$file" -crop "1x1+${x}+${y}" +repage -depth 8 txt:- 2>/dev/null
  else
    convert "$file" -crop "1x1+${x}+${y}" +repage -depth 8 txt:- 2>/dev/null
  fi
}

# Exits 5 (Xephyr not recorded) if $1's scale/geometry were never recorded --
# deliberate: every UI subcommand needs a live, recorded Xephyr first.
recorded_scale() {
  local dd; dd="$(display_dir "$1")"
  if [ ! -f "$dd/scale" ]; then
    echo "nested-run: no recorded scale for $1 -- run 'xephyr $1' first." >&2
    exit 5
  fi
  cat "$dd/scale"
}

recorded_geometry() {
  local dd; dd="$(display_dir "$1")"
  if [ ! -f "$dd/geometry" ]; then
    echo "nested-run: no recorded geometry for $1 -- run 'xephyr $1' first." >&2
    exit 5
  fi
  cat "$dd/geometry"
}

# All non-comment, non-blank data rows for LANG (columns: name lang kind
# index x top_y bottom_off probe_dx probe_dy state).
targets_for() {
  awk -F'\t' -v lang="$1" '!/^#/ && NF >= 10 && $2 == lang {print}' "$TARGETS_TSV"
}

lookup_target() {
  awk -F'\t' -v lang="$1" -v name="$2" '!/^#/ && NF >= 10 && $2 == lang && $1 == name {print; exit}' "$TARGETS_TSV"
}

# shot [:N] FILE  (R3)
cmd_shot() {
  parse_display "$@"
  local file="${REST[0]:-}"
  [ -n "$file" ] || { echo "usage: nested-run.sh shot [:N] FILE" >&2; exit 2; }
  mkdir -p "$(dirname "$file")"
  DISPLAY="$DISP" import -window root "$file"
  local size; size="$(DISPLAY="$DISP" identify -format '%wx%h' "$file" 2>/dev/null || echo '?')"
  echo "nested-run: shot $file ($size)"
}

# click [:N] X Y [--physical]  (R3)
cmd_click() {
  parse_display "$@"
  local x="${REST[0]:-}" y="${REST[1]:-}" physical=0 i
  for i in "${REST[@]:2}"; do [ "$i" = "--physical" ] && physical=1; done
  if [ -z "$x" ] || [ -z "$y" ]; then
    echo "usage: nested-run.sh click [:N] X Y [--physical]" >&2
    exit 2
  fi
  local scale; scale="$(recorded_scale "$DISP")"
  local geom; geom="$(recorded_geometry "$DISP")"
  local sw="${geom%x*}" sh="${geom#*x}"
  local px py
  if [ "$physical" = 1 ]; then
    px="$x"; py="$y"
  else
    px="$(python3 -c "print(int(round($x * $scale)))")"
    py="$(python3 -c "print(int(round($y * $scale)))")"
  fi
  if [ "$px" -lt 0 ] || [ "$py" -lt 0 ] || [ "$px" -gt "$sw" ] || [ "$py" -gt "$sh" ]; then
    echo "nested-run: click: ($px,$py) falls outside the recorded Xephyr geometry ${sw}x${sh}." >&2
    exit 15
  fi
  DISPLAY="$DISP" xdotool mousemove "$px" "$py" click 1
}

# key [:N] KEYSYM...  (R3)
cmd_key() {
  parse_display "$@"
  if [ "${#REST[@]}" -eq 0 ]; then
    echo "usage: nested-run.sh key [:N] KEYSYM..." >&2
    exit 2
  fi
  DISPLAY="$DISP" xdotool key "${REST[@]}"
}

# scroll [:N] top|bottom  (R3, P1)
cmd_scroll() {
  parse_display "$@"
  local dir="${REST[0]:-}" btn
  case "$dir" in
    top) btn=4 ;;
    bottom) btn=5 ;;
    *)
      echo "usage: nested-run.sh scroll [:N] top|bottom" >&2
      exit 2
      ;;
  esac
  local scale; scale="$(recorded_scale "$DISP")"
  local geom; geom="$(recorded_geometry "$DISP")"
  local sw="${geom%x*}" sh="${geom#*x}"
  local px py
  px="$(python3 -c "print(min(int(round(210 * $scale)), $sw))")"
  py="$(python3 -c "print(min(int(round(400 * $scale)), $sh))")"
  DISPLAY="$DISP" xdotool mousemove "$px" "$py"
  local i=0
  while [ "$i" -lt 40 ]; do
    DISPLAY="$DISP" xdotool click "$btn"
    sleep 0.005
    i=$((i + 1))
  done
  sleep 0.5
}

# targets [:N]  (R3)
cmd_targets() {
  parse_display "$@"
  local dd; dd="$(display_dir "$DISP")"
  local lang
  if [ ! -f "$dd/lang" ]; then
    echo "nested-run: targets: no recorded lang for $DISP -- launch first." >&2
    exit 5
  fi
  lang="$(cat "$dd/lang")"
  targets_for "$lang"
}

# Read the recorded window's current HEIGHT/Y (physical px) via xdotool.
# Sets WIN_HEIGHT and WIN_Y; exits 15 if the window/geometry is unreadable.
read_window_geometry() {
  local dd; dd="$(display_dir "$1")"
  local win_id
  if [ ! -f "$dd/window.id" ]; then
    echo "nested-run: no recorded window for $1." >&2
    exit 15
  fi
  win_id="$(cat "$dd/window.id")"
  local geom_line; geom_line="$(DISPLAY="$1" xdotool getwindowgeometry --shell "$win_id" 2>/dev/null || true)"
  WIN_HEIGHT="$(echo "$geom_line" | awk -F= '/^HEIGHT=/{print $2}')"
  WIN_Y="$(echo "$geom_line" | awk -F= '/^Y=/{print $2}')"
  if [ -z "$WIN_HEIGHT" ] || [ -z "$WIN_Y" ]; then
    echo "nested-run: could not read window geometry for $1 (window id $win_id)." >&2
    exit 15
  fi
}

# click-target [:N] NAME [--expect REGEX] [--timeout S]  (R3)
cmd_click_target() {
  parse_display "$@"
  local name="${REST[0]:-}"
  if [ -z "$name" ]; then
    echo "usage: nested-run.sh click-target [:N] NAME [--expect REGEX] [--timeout S]" >&2
    exit 2
  fi
  local expect="" timeout_s=5 i=1
  while [ "$i" -lt "${#REST[@]}" ]; do
    case "${REST[$i]}" in
      --expect) i=$((i + 1)); expect="${REST[$i]:-}" ;;
      --timeout) i=$((i + 1)); timeout_s="${REST[$i]:-5}" ;;
    esac
    i=$((i + 1))
  done

  local dd; dd="$(display_dir "$DISP")"
  if [ ! -f "$dd/lang" ]; then
    echo "nested-run: click-target: no recorded lang for $DISP." >&2
    exit 15
  fi
  local lang; lang="$(cat "$dd/lang")"

  local row; row="$(lookup_target "$lang" "$name")"
  if [ -z "$row" ]; then
    echo "nested-run: click-target: no calibration for '$name' in lang '$lang'." >&2
    exit 15
  fi

  if [ "$name" = "monitor" ] && [ ! -f "$dd/monitor-sink" ]; then
    echo "nested-run: click-target: 'monitor' needs a harness sink -- launch with --monitor-sink." >&2
    exit 11
  fi

  local t_name t_lang t_kind t_index t_x t_top_y t_bottom_off t_probe_dx t_probe_dy t_state
  IFS=$'\t' read -r t_name t_lang t_kind t_index t_x t_top_y t_bottom_off t_probe_dx t_probe_dy t_state <<<"$row"

  read_window_geometry "$DISP"
  local scale; scale="$(recorded_scale "$DISP")"
  local viewport; viewport="$(python3 -c "print($WIN_HEIGHT / $scale - 50)")"

  # BEFORE acting, per R3: the confirmation count is a baseline, not a
  # post-hoc grep -- a log line already present before this call must never
  # be mistaken for confirmation of THIS action.
  local app_log="$dd/app.log" before_count=0
  if [ -n "$expect" ]; then
    # NOT `grep -c ... || echo 0`: on zero matches `grep -c` prints "0" AND
    # exits 1, so that fallback ran TOO, appending a second "0\n0" and
    # breaking the numeric `-gt` test below with bash's "integer expected".
    # The bare `|| true` (no second echo) absorbs grep's exit-1-on-no-match
    # without adding a second line, and without letting `set -e` see a
    # "failed" plain assignment and abort the whole script right here.
    before_count="$(grep -Ec "$expect" "$app_log" 2>/dev/null)" || true
    [ -z "$before_count" ] && before_count=0
  fi

  local click_y=""
  if [ "$t_top_y" != "-" ] && python3 -c "raise SystemExit(0 if $t_top_y + 24 <= $viewport else 1)" 2>/dev/null; then
    cmd_scroll "$DISP" top
    click_y="$t_top_y"
  elif [ "$t_bottom_off" != "-" ] && python3 -c "raise SystemExit(0 if $t_bottom_off + 24 <= $viewport else 1)" 2>/dev/null; then
    cmd_scroll "$DISP" bottom
    click_y="$(python3 -c "print(($WIN_Y + $WIN_HEIGHT) / $scale - $t_bottom_off)")"
  else
    echo "nested-run: click-target: '$name' is unreachable at this viewport (${viewport} logical px)." >&2
    exit 15
  fi

  cmd_click "$DISP" "$t_x" "$click_y"

  if [ "$t_kind" = "combo" ]; then
    sleep 0.6
    cmd_key "$DISP" Home
    local k=0
    while [ "$k" -lt "$t_index" ]; do
      cmd_key "$DISP" Down
      k=$((k + 1))
    done
    cmd_key "$DISP" Return
    sleep 0.4
  fi

  if [ -n "$expect" ]; then
    local waited=0 after_count="$before_count"
    while [ "$waited" -lt "$((timeout_s * 10))" ]; do
      after_count="$(grep -Ec "$expect" "$app_log" 2>/dev/null)" || true
      [ -z "$after_count" ] && after_count=0
      [ "$after_count" -gt "$before_count" ] && return 0
      sleep 0.1
      waited=$((waited + 1))
    done
    echo "nested-run: click-target: '$name' was not confirmed by app.log within ${timeout_s}s (expected /$expect/). Last 5 lines:" >&2
    tail -n 5 "$app_log" >&2 || true
    exit 14
  fi
}

# check-layout [:N]  (R3)
cmd_check_layout() {
  parse_display "$@"
  local dd; dd="$(display_dir "$DISP")"
  if [ ! -f "$dd/lang" ]; then
    echo "nested-run: check-layout: no recorded lang for $DISP." >&2
    exit 5
  fi
  local lang; lang="$(cat "$dd/lang")"
  local cfg="$dd/home/config/cleanmic/config.toml"
  if [ ! -f "$cfg" ]; then
    echo "nested-run: check-layout: no private config for $DISP." >&2
    exit 5
  fi

  read_window_geometry "$DISP"
  local scale; scale="$(recorded_scale "$DISP")"
  local viewport; viewport="$(python3 -c "print($WIN_HEIGHT / $scale - 50)")"
  mkdir -p "$dd/shots"

  local need_top=0 need_bottom=0
  while IFS=$'\t' read -r t_name t_lang t_kind t_index t_x t_top_y t_bottom_off t_probe_dx t_probe_dy t_state; do
    [ "$t_state" = "-" ] && continue
    if [ "$t_top_y" != "-" ] && python3 -c "raise SystemExit(0 if $t_top_y + 24 <= $viewport else 1)" 2>/dev/null; then
      need_top=1
    elif [ "$t_bottom_off" != "-" ] && python3 -c "raise SystemExit(0 if $t_bottom_off + 24 <= $viewport else 1)" 2>/dev/null; then
      need_bottom=1
    fi
  done < <(targets_for "$lang")

  if [ "$need_top" = 1 ]; then
    cmd_scroll "$DISP" top
    cmd_shot "$DISP" "$dd/shots/check-top.png" >/dev/null
  fi
  if [ "$need_bottom" = 1 ]; then
    cmd_scroll "$DISP" bottom
    cmd_shot "$DISP" "$dd/shots/check-bottom.png" >/dev/null
  fi

  local drift=0
  while IFS=$'\t' read -r t_name t_lang t_kind t_index t_x t_top_y t_bottom_off t_probe_dx t_probe_dy t_state; do
    [ "$t_state" = "-" ] && continue
    local shot_file="" click_y=""
    if [ "$t_top_y" != "-" ] && python3 -c "raise SystemExit(0 if $t_top_y + 24 <= $viewport else 1)" 2>/dev/null; then
      shot_file="$dd/shots/check-top.png"
      click_y="$t_top_y"
    elif [ "$t_bottom_off" != "-" ] && python3 -c "raise SystemExit(0 if $t_bottom_off + 24 <= $viewport else 1)" 2>/dev/null; then
      shot_file="$dd/shots/check-bottom.png"
      click_y="$(python3 -c "print(($WIN_Y + $WIN_HEIGHT) / $scale - $t_bottom_off)")"
    else
      echo "UNREACHABLE: $t_name"
      continue
    fi

    local px py
    px="$(python3 -c "print(int(round(($t_x + $t_probe_dx) * $scale)))")"
    py="$(python3 -c "print(int(round(($click_y + $t_probe_dy) * $scale)))")"
    local pixel_txt; pixel_txt="$(crop_pixel_txt "$shot_file" "$px" "$py")"
    local hex; hex="$(echo "$pixel_txt" | grep -Eo '#[0-9A-Fa-f]{6,8}' | head -1)"
    local classification="AMBIGUOUS"
    if [ -n "$hex" ]; then
      classification="$(python3 -c "
h = '$hex'.lstrip('#')
r, g, b = int(h[0:2], 16), int(h[2:4], 16), int(h[4:6], 16)
spread = max(r, g, b) - min(r, g, b)
print('ACCENT' if spread >= 50 else ('NEUTRAL' if spread <= 20 else 'AMBIGUOUS'))
")"
    fi

    local expected
    expected="$(python3 -c "
import tomllib
with open('$cfg', 'rb') as f:
    cfg = tomllib.load(f)
kind, name = '$t_kind', '$t_name'
if kind == 'switch':
    key = {'enable': 'enabled', 'autostart': 'autostart', 'monitor': 'monitor_enabled', 'autogain': 'auto_gain_enabled'}.get(name)
    print('ACCENT' if cfg.get(key) else 'NEUTRAL')
elif kind == 'radio':
    engine = {'engine-rnnoise': 'RNNoise', 'engine-deepfilternet': 'DeepFilterNet', 'engine-dpdfnet2': 'Dpdfnet2', 'engine-dpdfnet8': 'Dpdfnet8'}.get(name)
    print('ACCENT' if cfg.get('engine') == engine else 'NEUTRAL')
else:
    print('-')
")"

    if [ "$expected" = "-" ]; then
      echo "OK: $t_name (kind $t_kind has no state check)"
      continue
    fi
    if [ "$classification" = "AMBIGUOUS" ] || [ "$classification" != "$expected" ]; then
      echo "DRIFT(expected=$expected, got=$classification, pixel=$hex, at=$px,$py): $t_name"
      drift=1
    else
      echo "OK: $t_name"
    fi
  done < <(targets_for "$lang")

  [ "$drift" = 1 ] && exit 13
  exit 0
}

# ---------------------------------------------------------------------------
# dispatch
# ---------------------------------------------------------------------------
sub="${1:-}"
[ "$#" -gt 0 ] && shift || true

case "$sub" in
  xephyr) cmd_xephyr "$@" ;;
  launch) cmd_launch "$@" ;;
  app-pid) cmd_app_pid "$@" ;;
  status) cmd_status "$@" ;;
  stop)
    stop_rc=0
    cmd_stop "$@" || stop_rc=$?
    exit "$stop_rc"
    ;;
  reap) cmd_reap "$@" ;;
  shot) cmd_shot "$@" ;;
  click) cmd_click "$@" ;;
  key) cmd_key "$@" ;;
  scroll) cmd_scroll "$@" ;;
  targets) cmd_targets "$@" ;;
  click-target) cmd_click_target "$@" ;;
  check-layout) cmd_check_layout "$@" ;;
  *)
    echo "usage: nested-run.sh {xephyr|launch|app-pid|status|shot|click|key|scroll|targets|click-target|check-layout|stop|reap} ..." >&2
    exit 2
    ;;
esac
