#!/usr/bin/env bash
#
# appimage-preflight.sh -- pre-flight host-library check for the CleanMic AppImage.
#
# Invoked by AppRun immediately before `exec`ing the real cleanmic binary.
# Detects any missing shared library the binary needs (e.g. libadwaita-1.so.0
# on non-GNOME hosts) and, instead of letting the dynamic linker crash with a
# cryptic error, prints a clear stderr message naming the missing lib(s) and
# the exact distro install command, best-effort mirrors that message via a GUI
# dialog for double-click users, then exits non-zero so AppRun does not exec.
#
# Usage: appimage-preflight.sh <path-to-cleanmic-binary>
#
# Exit codes:
#   0 - happy path: no missing library detected (also used when `ldd` is
#       unavailable, or the binary-path argument is missing/empty -- see
#       fail-open note below).
#   3 - a required shared library is genuinely missing. AppRun must abort
#       launch ONLY on this specific code. Any OTHER nonzero exit means the
#       helper itself failed to run to completion (crash, bad shebang, lost
#       +x, etc.) and is unrelated to a missing library -- callers must fail
#       OPEN on those (still exec the app) so a bug in this ~90-line helper
#       can never brick a healthy launch.
#
# Fail-open: if `ldd` is unavailable, the binary-path argument ($1) is
# missing/empty, or ldd reports nothing missing, this script exits 0
# immediately -- the happy path costs exactly one `ldd` invocation.
#
# Env overrides (testability only):
#   PREFLIGHT_OS_RELEASE  -- path to read instead of /etc/os-release
#   PREFLIGHT_NO_GUI      -- when set to a non-empty value, skip the GUI dialog
#                            cascade entirely (stderr message still prints)
#
# Threat model notes (see 10-CONTEXT.md / PLAN threat_model):
#   - os-release ID/ID_LIKE values only ever select a fixed literal `case`
#     branch below; they are never eval'd or interpolated into a displayed or
#     executed command.
#   - The message is passed to each GUI dialog tool as a single argument, never
#     via a re-splittable/eval'd shell string.

set -euo pipefail

BINARY="${1:-}"

if [ -z "$BINARY" ]; then
    echo "appimage-preflight.sh: missing required argument <path-to-cleanmic-binary>" >&2
    exit 0
fi

# ── Detection (D-01/D-02) ────────────────────────────────────────────────────
# Canonical form per 10-CONTEXT.md D-01. Fail-open if ldd itself is missing or
# errors -- `command -v` guard avoids `set -e` aborting the whole script.
MISSING=""
if command -v ldd >/dev/null 2>&1; then
    # Anchor on the "=> not found" shape of a genuine missing shared-library
    # dependency line. A bare 'not found' also matches glibc symbol-version
    # mismatch lines (e.g. "version `GLIBC_2.38' not found"), which have no
    # "=>" token and are NOT a missing library -- misclassifying them here
    # would print the binary's own path as a fake "missing library" and a
    # wrong install command (CR-01).
    MISSING="$(ldd "$BINARY" 2>/dev/null | LC_ALL=C grep '=> not found' | awk '{print $1}' || true)"
fi

if [ -z "$MISSING" ]; then
    # All libs resolve, or ldd unavailable/errored -- fail-open, launch proceeds.
    exit 0
fi

# ── Distro detection -> fixed install command (never eval os-release values) ─
OS_RELEASE="${PREFLIGHT_OS_RELEASE:-/etc/os-release}"

ID_VAL=""
ID_LIKE_VAL=""
if [ -f "$OS_RELEASE" ]; then
    # Parse only the ID= and ID_LIKE= lines; strip surrounding quotes. Values
    # are used purely as case-match tokens below, never eval'd or executed.
    ID_VAL="$(grep -E '^ID=' "$OS_RELEASE" 2>/dev/null | head -n1 | cut -d= -f2- | tr -d '"' || true)"
    ID_LIKE_VAL="$(grep -E '^ID_LIKE=' "$OS_RELEASE" 2>/dev/null | head -n1 | cut -d= -f2- | tr -d '"' || true)"
fi

DISTRO_TOKENS="$ID_VAL $ID_LIKE_VAL"

CMD_PACMAN="sudo pacman -S libadwaita"
CMD_APT="sudo apt install libadwaita-1-0"
CMD_DNF="sudo dnf install libadwaita"

INSTALL_CMD=""
case "$DISTRO_TOKENS" in
    *arch*)
        INSTALL_CMD="$CMD_PACMAN"
        ;;
    *debian*|*ubuntu*)
        INSTALL_CMD="$CMD_APT"
        ;;
    *fedora*)
        INSTALL_CMD="$CMD_DNF"
        ;;
    *)
        INSTALL_CMD="$CMD_PACMAN
    $CMD_APT
    $CMD_DNF"
        ;;
esac

# ── Message builder (D-03) ───────────────────────────────────────────────────
MISSING_LIST="$(printf '%s' "$MISSING" | tr '\n' ' ')"

MSG="CleanMic can't start: the following required librar$( [ "$(printf '%s\n' "$MISSING" | wc -l)" -gt 1 ] && echo 'ies are' || echo 'y is' ) missing:

    $MISSING_LIST

Install $( [ "$(printf '%s\n' "$MISSING" | wc -l)" -gt 1 ] && echo 'them' || echo 'it' ) with:

    $INSTALL_CMD
"

# ALWAYS print to stderr, regardless of GUI availability.
printf '%s\n' "$MSG" >&2

# ── GUI cascade (D-03 / D-03a) ───────────────────────────────────────────────
# kdialog -> zenity -> notify-send -> xmessage, first available wins. Message
# passed as a single argument to each tool. Skipped entirely under
# PREFLIGHT_NO_GUI (used by the automated regression test / CI).
if [ -z "${PREFLIGHT_NO_GUI:-}" ]; then
    if command -v kdialog >/dev/null 2>&1; then
        kdialog --error "$MSG" >/dev/null 2>&1 || true
    elif command -v zenity >/dev/null 2>&1; then
        zenity --error --text "$MSG" >/dev/null 2>&1 || true
    elif command -v notify-send >/dev/null 2>&1; then
        notify-send "CleanMic can't start" "$MSG" >/dev/null 2>&1 || true
    elif command -v xmessage >/dev/null 2>&1; then
        xmessage -center "$MSG" >/dev/null 2>&1 || true
    fi
fi

# Exit 3 = "a required library is genuinely missing" -- the ONE code that
# should ever block launch. See the exit-codes docstring at the top.
exit 3
