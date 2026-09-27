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
#
# Plan 02 (D-08/D-10) extends this from a bare-binary-only tool into a real
# AppImage-launching proxy that can also HIDE parts of the host's own
# PipeWire stack in a bwrap sandbox, so the fallback-only bundling behavior
# (D-10) can be proven end-to-end: with the host's PipeWire stack hidden,
# the bundled fallback copy must be the one that resolves; with it visible,
# the host's own copy must win.
#
# USAGE
#   scripts/catalog-proxy.sh (--binary PATH | --appimage PATH) [--out DIR]
#     [--lang en|fr] [--sandbox bwrap|none] [--hide-lib SONAME]...
#     [--hide-pipewire-stack] [--expect-lib SONAME=appdir|fallback|host]...
#
# --binary PATH      Launch a bare binary directly (no AppDir, no AppRun).
# --appimage PATH    Extract PATH via `--appimage-extract` (no FUSE needed)
#                     and launch the extracted squashfs-root/AppRun.
# --sandbox MODE     bwrap (default for --appimage, when bwrap exists) or
#                     none (default for --binary, and the only valid value
#                     when bwrap is unavailable). --hide-lib and
#                     --hide-pipewire-stack require bwrap.
# --hide-lib SONAME  Repeatable. Inside the bwrap sandbox, `--ro-bind
#                     /dev/null` every host path that resolves that soname
#                     (ldd/ldconfig's view plus the default lib dirs, symlink
#                     targets included), so the dynamic linker reports it
#                     "not found" exactly like a host that never had it.
# --hide-pipewire-stack
#                     Shorthand for hiding libpipewire-0.3.so.0 AND its SPA
#                     plugin dir, module dir, and /usr/share/pipewire -- a
#                     host with no PipeWire installed at all.
# --expect-lib SONAME=appdir|fallback|host
#                     Repeatable. After a window appears, reads
#                     /proc/<app-pid>/maps and asserts which tree the given
#                     soname was actually mapped from. A mismatch FAILs the
#                     run just like a missing window would.
#
# This script always starts its OWN private Xvfb on a free display it picks
# itself -- never the owner's real X/Xephyr/Wayland session, and never a
# `:N` the caller names (unlike scripts/nested-run.sh's Xephyr harness).
#
# EXIT CODES
#   0   PASS -- a window titled "CleanMic" appeared within the catalog's
#       window, the app was still alive right after the screenshot, and
#       every --expect-lib assertion matched.
#   1   FAIL -- the app exited before a window appeared, died afterward, or
#       an --expect-lib assertion did not match.
#   2   bad usage
#   3   a required tool is missing (Xvfb, xwininfo, import/magick,
#       dbus-run-session, bwrap when --sandbox bwrap is in effect), the
#       private Xvfb never came up, or `--appimage-extract` failed.
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
#   - The bwrap sandbox (when active) is unprivileged: a read-only bind of
#     the whole host root, `--dev`/`--proc`, only this run's own out dir
#     bound writable, a private /tmp with only /tmp/.X11-unix re-bound, and
#     `--unshare-net`. The PID namespace is deliberately NOT unshared, so
#     /proc/<pid>/maps lookups from outside the sandbox keep working.
#   - Cleanup signals ONLY the pids this script itself recorded: the app's
#     setsid session leader (verified alive via its recorded /proc start
#     time before every signal -- immune to the pid being reused by an
#     unrelated process after our own process exits) and Xvfb (verified via
#     /proc/<pid>/comm == "Xvfb"). No `pgrep -f` wait loop, no name-based
#     kill. `--die-with-parent` on bwrap is a second, independent line of
#     defense for anything the sandbox itself spawned.

set -euo pipefail

SCRIPT_DIR="$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

usage() {
  echo "usage: scripts/catalog-proxy.sh (--binary PATH | --appimage PATH) [--out DIR] [--lang en|fr] [--sandbox bwrap|none] [--hide-lib SONAME]... [--hide-pipewire-stack] [--expect-lib SONAME=appdir|fallback|host]..." >&2
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
  if [ "$SANDBOX_MODE" = "bwrap" ]; then
    command -v bwrap >/dev/null 2>&1 || missing+=("bubblewrap")
  fi
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

# resolve_host_lib_paths SONAME -- prints every host path (one per line)
# that could resolve that soname: ldconfig's cache, the default lib dirs
# checked directly, and (for anything found) its readlink -f target too, so
# both a versioned symlink and its real file get masked.
LIB_SEARCH_DIRS=(/usr/lib/x86_64-linux-gnu /lib/x86_64-linux-gnu /usr/lib /lib /usr/local/lib)

resolve_host_lib_paths() {
  local soname="$1" p rp
  local -a found=()
  if command -v ldconfig >/dev/null 2>&1; then
    while IFS= read -r p; do
      [ -n "$p" ] && found+=("$p")
    done < <(ldconfig -p 2>/dev/null | awk -v s="$soname" '$1==s {print $NF}')
  fi
  for p in "${LIB_SEARCH_DIRS[@]}"; do
    [ -e "$p/$soname" ] && found+=("$p/$soname")
  done
  local -a all=() seen=()
  for p in "${found[@]}"; do
    case " ${seen[*]:-} " in *" $p "*) continue ;; esac
    seen+=("$p")
    all+=("$p")
    if [ -L "$p" ] || [ -e "$p" ]; then
      rp="$(readlink -f "$p" 2>/dev/null || true)"
      if [ -n "$rp" ]; then
        case " ${seen[*]:-} " in *" $rp "*) ;; *) seen+=("$rp"); all+=("$rp") ;; esac
      fi
    fi
  done
  printf '%s\n' "${all[@]}"
}

BINARY="" APPIMAGE="" OUT_DIR="" LANG_ARG="en" SANDBOX_MODE=""
HIDE_LIBS=() EXPECT_LIBS=() HIDE_PIPEWIRE_STACK=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --binary)
      shift; BINARY="${1:-}" ;;
    --appimage)
      shift; APPIMAGE="${1:-}" ;;
    --out)
      shift; OUT_DIR="${1:-}" ;;
    --lang)
      shift; LANG_ARG="${1:-}" ;;
    --sandbox)
      shift; SANDBOX_MODE="${1:-}" ;;
    --hide-lib)
      shift; HIDE_LIBS+=("${1:-}") ;;
    --hide-pipewire-stack)
      HIDE_PIPEWIRE_STACK=1 ;;
    --expect-lib)
      shift; EXPECT_LIBS+=("${1:-}") ;;
    *)
      echo "catalog-proxy: unknown option '$1'" >&2
      usage
      ;;
  esac
  shift
done

if [ -n "$BINARY" ] && [ -n "$APPIMAGE" ]; then
  echo "catalog-proxy: --binary and --appimage are mutually exclusive" >&2
  usage
fi
if [ -z "$BINARY" ] && [ -z "$APPIMAGE" ]; then
  usage
fi
case "$LANG_ARG" in
  en | fr) : ;;
  *)
    echo "catalog-proxy: --lang must be en or fr, got '$LANG_ARG'" >&2
    usage
    ;;
esac

if [ -z "$SANDBOX_MODE" ]; then
  if [ -n "$APPIMAGE" ] && command -v bwrap >/dev/null 2>&1; then
    SANDBOX_MODE="bwrap"
  else
    SANDBOX_MODE="none"
  fi
fi
case "$SANDBOX_MODE" in
  bwrap | none) : ;;
  *)
    echo "catalog-proxy: --sandbox must be bwrap or none, got '$SANDBOX_MODE'" >&2
    usage
    ;;
esac

if { [ "${#HIDE_LIBS[@]}" -gt 0 ] || [ "$HIDE_PIPEWIRE_STACK" = 1 ]; } && [ "$SANDBOX_MODE" != "bwrap" ]; then
  echo "catalog-proxy: --hide-lib/--hide-pipewire-stack require --sandbox bwrap" >&2
  usage
fi

for e in "${EXPECT_LIBS[@]}"; do
  [ -z "$e" ] && continue
  case "$e" in
    *=appdir | *=fallback | *=host) : ;;
    *)
      echo "catalog-proxy: --expect-lib must be SONAME=appdir|fallback|host, got '$e'" >&2
      usage
      ;;
  esac
done

if [ -n "$APPIMAGE" ]; then
  [ -e "$APPIMAGE" ] || {
    echo "catalog-proxy: appimage not found: $APPIMAGE" >&2
    exit 2
  }
  APPIMAGE="$(realpath -m -- "$APPIMAGE")"
  chmod +x "$APPIMAGE" 2>/dev/null || true
  TARGET_FOR_SHA="$APPIMAGE"
else
  [ -e "$BINARY" ] || {
    echo "catalog-proxy: binary not found: $BINARY" >&2
    exit 2
  }
  BINARY="$(realpath -m -- "$BINARY")"
  TARGET_FOR_SHA="$BINARY"
fi

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

# --- extract the AppImage (no FUSE needed), or launch the bare binary ------
EXTRACT_DIR=""
LAUNCH_TARGET=""
if [ -n "$APPIMAGE" ]; then
  EXTRACT_DIR="$OUT_DIR/squashfs-root"
  rm -rf "$EXTRACT_DIR"
  if ! (cd "$OUT_DIR" && "$APPIMAGE" --appimage-extract >"$OUT_DIR/extract.log" 2>&1); then
    echo "catalog-proxy: --appimage-extract failed -- see $OUT_DIR/extract.log" >&2
    exit 3
  fi
  if [ ! -x "$EXTRACT_DIR/AppRun" ]; then
    echo "catalog-proxy: extracted AppDir has no executable AppRun" >&2
    exit 3
  fi
  # uruntime's --appimage-extract writes the real tree to a directory named
  # "AppDir" and leaves "squashfs-root" as a compatibility symlink to it --
  # canonicalize now so classify_lib_source's path comparison below matches
  # what /proc/<pid>/maps will actually show (the kernel resolves symlinks).
  EXTRACT_DIR="$(readlink -f "$EXTRACT_DIR")"
  LAUNCH_TARGET="$EXTRACT_DIR/AppRun"
else
  LAUNCH_TARGET="$BINARY"
fi

# --- resolve the bwrap lib-mask arguments (D-10 / Task 1) -------------------
SONAMES_TO_HIDE=()
for s in "${HIDE_LIBS[@]}"; do
  [ -n "$s" ] && SONAMES_TO_HIDE+=("$s")
done
MASKED_DIRS=()
if [ "$HIDE_PIPEWIRE_STACK" = 1 ]; then
  SONAMES_TO_HIDE+=("libpipewire-0.3.so.0")
  for d in /usr/lib/x86_64-linux-gnu/spa-0.2 /usr/lib/x86_64-linux-gnu/pipewire-0.3 /usr/share/pipewire; do
    [ -e "$d" ] && MASKED_DIRS+=("$d")
  done
fi

MASKED_FILES=()
for s in "${SONAMES_TO_HIDE[@]}"; do
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    case " ${MASKED_FILES[*]:-} " in *" $p "*) continue ;; esac
    MASKED_FILES+=("$p")
  done < <(resolve_host_lib_paths "$s")
done

MASK_ARGS=()
for p in "${MASKED_FILES[@]}"; do
  MASK_ARGS+=(--ro-bind /dev/null "$p")
done
for d in "${MASKED_DIRS[@]}"; do
  MASK_ARGS+=(--tmpfs "$d")
done

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

# Unique per-run marker so the real cleanmic process can be found by
# /proc/<pid>/environ instead of by name (several catalog-proxy runs, or an
# unrelated cleanmic, could exist on the same host at once).
APP_MARKER="catalog-proxy-$$-$(date +%s%N)"

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
  "CATALOG_PROXY_MARKER=$APP_MARKER"
)

LAUNCH_CMD=(env "${ENV_ARGS[@]}" dbus-run-session -- "$LAUNCH_TARGET")
if [ "$SANDBOX_MODE" = "bwrap" ]; then
  BWRAP_ARGS=(
    --ro-bind / /
    --dev /dev
    --proc /proc
    --tmpfs /tmp
    --ro-bind /tmp/.X11-unix /tmp/.X11-unix
    --bind "$OUT_DIR" "$OUT_DIR"
    --unshare-net
    --die-with-parent
  )
  BWRAP_ARGS+=("${MASK_ARGS[@]}")
  LAUNCH_CMD=(bwrap "${BWRAP_ARGS[@]}" -- "${LAUNCH_CMD[@]}")
fi

setsid sh -c "echo \$\$ > '$SESSION_PIDFILE'; exec \"\$@\"" -- "${LAUNCH_CMD[@]}" \
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

# --- find the real cleanmic pid (D-10 / Task 1): via its marked environ, --
# never by bare process name, since AppRun `exec`s into it (same pid) and
# dbus-run-session may have forked a child to reach it.
APP_PID=""
find_app_pid() {
  local marker="$1" pid_dir pid comm
  for pid_dir in /proc/[0-9]*; do
    pid="${pid_dir#/proc/}"
    comm="$(cat "$pid_dir/comm" 2>/dev/null || true)"
    [ "$comm" = "cleanmic" ] || continue
    if tr '\0' '\n' <"$pid_dir/environ" 2>/dev/null | grep -qxF "CATALOG_PROXY_MARKER=$marker"; then
      printf '%s' "$pid"
      return 0
    fi
  done
  return 1
}

# classify_lib_source SONAME MAPPED_PATH -- appdir | fallback | host, for the
# --expect-lib report below. The appdir branch only applies in --appimage
# mode (EXTRACT_DIR set) -- in --binary mode there is no AppDir, so every
# match there is host.
classify_lib_source() {
  local soname="$1" mapped="$2"
  case "$mapped" in
    */pipewire-fallback/"$soname")
      printf 'fallback'
      return
      ;;
  esac
  if [ -n "$EXTRACT_DIR" ]; then
    case "$mapped" in
      "$EXTRACT_DIR"/usr/lib/*)
        printf 'appdir'
        return
        ;;
    esac
  fi
  printf 'host'
}

EXPECT_RESULTS=()
EXPECT_FAILS=0

if [ "$WIN_FOUND" = 1 ]; then
  APP_PID="$(find_app_pid "$APP_MARKER" || true)"
  if [ "${#EXPECT_LIBS[@]}" -gt 0 ]; then
    if [ -z "$APP_PID" ]; then
      for e in "${EXPECT_LIBS[@]}"; do
        EXPECT_RESULTS+=("FAIL: $e (could not find the cleanmic process to inspect /proc/<pid>/maps)")
        EXPECT_FAILS=$((EXPECT_FAILS + 1))
      done
    else
      MAPS_CONTENT="$(cat "/proc/$APP_PID/maps" 2>/dev/null || true)"
      for e in "${EXPECT_LIBS[@]}"; do
        SONAME="${e%%=*}"
        WANT="${e#*=}"
        MAPPED_LINE="$(printf '%s\n' "$MAPS_CONTENT" | grep -F "$SONAME" | head -1 || true)"
        MAPPED_PATH="$(printf '%s' "$MAPPED_LINE" | awk '{print $NF}')"
        if [ -z "$MAPPED_PATH" ]; then
          EXPECT_RESULTS+=("FAIL: $SONAME expected=$WANT actual=<not mapped>")
          EXPECT_FAILS=$((EXPECT_FAILS + 1))
          continue
        fi
        GOT="$(classify_lib_source "$SONAME" "$MAPPED_PATH")"
        if [ "$GOT" = "$WANT" ]; then
          EXPECT_RESULTS+=("PASS: $SONAME expected=$WANT actual=$GOT ($MAPPED_PATH)")
        else
          EXPECT_RESULTS+=("FAIL: $SONAME expected=$WANT actual=$GOT ($MAPPED_PATH)")
          EXPECT_FAILS=$((EXPECT_FAILS + 1))
        fi
      done
    fi
  fi

  SHOT="$OUT_DIR/screenshot.png"
  if command -v import >/dev/null 2>&1; then
    DISPLAY="$DISP" import -window root "$SHOT" 2>>"$OUT_DIR/app.log" || true
  else
    DISPLAY="$DISP" magick import -window root "$SHOT" 2>>"$OUT_DIR/app.log" || true
  fi
  if ! pid_running "$APP_SID"; then
    VERDICT="FAIL"
    DETAIL="a window appeared, but the app was no longer alive right after the screenshot"
  elif [ "$EXPECT_FAILS" -gt 0 ]; then
    VERDICT="FAIL"
    DETAIL="a window titled CleanMic appeared within ${TOTAL_SECS}s, but ${EXPECT_FAILS} --expect-lib assertion(s) did not match"
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

GIT_HEAD="$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
GIT_DIRTY="clean"
if [ -n "$(git -C "$REPO_ROOT" status --porcelain 2>/dev/null || true)" ]; then
  GIT_DIRTY="dirty"
fi

{
  echo "# catalog-proxy report"
  echo
  if [ -n "$APPIMAGE" ]; then
    echo "- AppImage: $APPIMAGE"
    echo "- Extracted to: $EXTRACT_DIR"
  else
    echo "- Binary: $BINARY"
  fi
  echo "- SHA256: $(sha256sum "$TARGET_FOR_SHA" 2>/dev/null | cut -d' ' -f1)"
  echo "- Lang: $LANG_ARG"
  echo "- Display: $DISP (private Xvfb, 1280x1024x24)"
  echo "- Sandbox: $SANDBOX_MODE"
  if [ "$SANDBOX_MODE" = "bwrap" ]; then
    echo "- Masked lib files: ${MASKED_FILES[*]:-none}"
    echo "- Masked dirs (tmpfs): ${MASKED_DIRS[*]:-none}"
  fi
  echo "- Git HEAD: $GIT_HEAD ($GIT_DIRTY)"
  echo "- Grace period: 10s, poll attempts used: ${WAIT}/20"
  echo "- Window found: $([ "$WIN_FOUND" = 1 ] && echo yes || echo no)"
  echo "- App died before a window appeared: $([ "$DIED" = 1 ] && echo yes || echo no)"
  if [ "${#EXPECT_RESULTS[@]}" -gt 0 ]; then
    echo "- expect-lib results:"
    for r in "${EXPECT_RESULTS[@]}"; do
      echo "    - $r"
    done
  fi
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
