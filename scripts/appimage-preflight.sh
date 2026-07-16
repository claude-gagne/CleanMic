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
#   PREFLIGHT_LANG        -- when set, overrides ${LC_ALL:-${LC_MESSAGES:-$LANG}}
#                            for locale detection (testability only)
#
# Localization (D-03b): the message is localized to French when the detected
# locale begins with "fr" (case-insensitive), else English. This is plain
# shell string selection keyed on $LANG -- not gettext/.po, since this helper
# runs from AppRun before any Rust/gettext locale binding exists.
#
# Threat model notes (see 10-CONTEXT.md / PLAN threat_model):
#   - os-release ID/ID_LIKE values only ever select a fixed literal `case`
#     branch below; they are never eval'd or interpolated into a displayed or
#     executed command.
#   - The message is passed to each GUI dialog tool as a single argument, never
#     via a re-splittable/eval'd shell string.
#   - Dynamic values (the missing-lib names parsed from `ldd` output) are
#     markup-escaped (`& < >`) before being embedded into the zenity/kdialog
#     markup forms, preserving the T-10-02 no-injection mitigation even though
#     lib names are already constrained by the linker.

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
# Honor the test override first; otherwise prefer /etc/os-release, falling
# back to /usr/lib/os-release per os-release(5) for minimal/container images
# that ship only the latter without the usual symlink (IN-03).
if [ -n "${PREFLIGHT_OS_RELEASE:-}" ]; then
    OS_RELEASE="$PREFLIGHT_OS_RELEASE"
elif [ -f /etc/os-release ]; then
    OS_RELEASE=/etc/os-release
elif [ -f /usr/lib/os-release ]; then
    OS_RELEASE=/usr/lib/os-release
else
    OS_RELEASE=/etc/os-release
fi

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
        # Unindented, newline-joined -- the message builder below applies
        # its own indentation uniformly to every rendering form.
        INSTALL_CMD="$CMD_PACMAN
$CMD_APT
$CMD_DNF"
        ;;
esac

# ── Locale detection (D-03b) ─────────────────────────────────────────────────
# PREFLIGHT_LANG overrides for testability; otherwise the standard glibc
# locale-precedence chain. French iff the resolved locale begins with "fr"
# (case-insensitive -- covers fr_FR.UTF-8, FR, fr-CA, etc.).
LOCALE_VAL="${PREFLIGHT_LANG:-${LC_ALL:-${LC_MESSAGES:-${LANG:-}}}}"
IS_FRENCH=0
case "$LOCALE_VAL" in
    [Ff][Rr]*)
        IS_FRENCH=1
        ;;
esac

# ── Message builder (D-03 / D-03b) ───────────────────────────────────────────
LIB_COUNT="$(printf '%s\n' "$MISSING" | wc -l)"
IS_PLURAL=0
if [ "$LIB_COUNT" -gt 1 ]; then
    IS_PLURAL=1
fi

if [ "$IS_FRENCH" -eq 1 ]; then
    HEADING="CleanMic ne peut pas démarrer"
    if [ "$IS_PLURAL" -eq 1 ]; then
        BODY="Les bibliothèques système requises suivantes sont manquantes :"
        ACTION="Installez-les, puis relancez CleanMic :"
    else
        BODY="Une bibliothèque système requise est manquante :"
        ACTION="Installez-la, puis relancez CleanMic :"
    fi
else
    HEADING="CleanMic can't start"
    if [ "$IS_PLURAL" -eq 1 ]; then
        BODY="The following required system libraries are missing:"
        ACTION="Install them, then launch CleanMic again:"
    else
        BODY="A required system library is missing:"
        ACTION="Install it, then launch CleanMic again:"
    fi
fi

# Markup-escape (T-10-02): escape the DYNAMIC missing-lib token(s) before they
# are ever embedded in the zenity/kdialog markup forms below. Order matters --
# '&' must be escaped first, or the '&' introduced by escaping '<'/'>' would
# itself get re-escaped.
escape_markup() {
    # NOTE: an unescaped '&' in a bash ${var//pat/repl} replacement means
    # "insert the matched text" -- so the replacement strings below MUST
    # backslash-escape their literal '&', or "&lt;"/"&gt;" would expand to
    # "<lt;"/">gt;" instead of the intended HTML entities.
    local s="$1"
    s="${s//&/\&amp;}"
    s="${s//</\&lt;}"
    s="${s//>/\&gt;}"
    printf '%s' "$s"
}

# Joins a newline-separated block of text with the given separator (used to
# build kdialog's <br>-separated HTML lines without relying on literal
# newlines being preserved inside a <p> element).
join_lines() {
    local sep="$1" text="$2" result="" first=1 line
    while IFS= read -r line; do
        if [ "$first" -eq 1 ]; then
            result="$line"
            first=0
        else
            result="$result$sep$line"
        fi
    done <<< "$text"
    printf '%s' "$result"
}

MISSING_ESCAPED="$(escape_markup "$MISSING")"

# ── PLAIN form -- stderr, notify-send body, xmessage ─────────────────────────
# No markup; each lib/command line indented 4 spaces; blank-line grouping
# between heading (+body), libs, action, and command.
MISSING_INDENTED="$(printf '%s\n' "$MISSING" | sed 's/^/    /')"
INSTALL_CMD_INDENTED="$(printf '%s\n' "$INSTALL_CMD" | sed 's/^/    /')"

MSG="$HEADING
$BODY

$MISSING_INDENTED

$ACTION

$INSTALL_CMD_INDENTED
"

# ALWAYS print to stderr, regardless of GUI availability.
printf '%s\n' "$MSG" >&2

# ── ZENITY form -- Pango markup ──────────────────────────────────────────────
ZEN_MISSING_INDENTED="$(printf '%s\n' "$MISSING_ESCAPED" | sed 's/^/    /')"
ZEN_CMD_INDENTED="$(printf '%s\n' "$INSTALL_CMD" | sed 's/^/    /')"

ZEN_TEXT="<b>$HEADING</b>

$BODY

<tt>$ZEN_MISSING_INDENTED</tt>

$ACTION

<tt>$ZEN_CMD_INDENTED</tt>"

# ── KDIALOG form -- Qt HTML subset ───────────────────────────────────────────
KD_MISSING_JOINED="$(join_lines '<br>' "$MISSING_ESCAPED")"
KD_CMD_JOINED="$(join_lines '<br>' "$INSTALL_CMD")"

KD_MSG="<h3>$HEADING</h3><p>$BODY</p><p><tt>$KD_MISSING_JOINED</tt></p><p>$ACTION</p><p><tt>$KD_CMD_JOINED</tt></p>"

# ── GUI cascade (D-03 / D-03a / D-03b) ───────────────────────────────────────
# kdialog -> zenity -> notify-send -> xmessage, first available wins. Message
# passed as a single argument to each tool -- never a re-splittable/eval'd
# shell string. The two modal dialogs (kdialog, zenity) are the PRIMARY
# channel and get the richer markup forms; notify-send and xmessage remain
# last-resort supplements and stay PLAIN. Skipped entirely under
# PREFLIGHT_NO_GUI (used by the automated regression test / CI).
if [ -z "${PREFLIGHT_NO_GUI:-}" ]; then
    if command -v kdialog >/dev/null 2>&1; then
        kdialog --title "CleanMic" --error "$KD_MSG" >/dev/null 2>&1 || true
    elif command -v zenity >/dev/null 2>&1; then
        zenity --title="CleanMic" --width=440 --error --text "$ZEN_TEXT" >/dev/null 2>&1 || true
    elif command -v notify-send >/dev/null 2>&1; then
        notify-send "CleanMic" "$MSG" >/dev/null 2>&1 || true
    elif command -v xmessage >/dev/null 2>&1; then
        xmessage -center "$MSG" >/dev/null 2>&1 || true
    fi
fi

# Exit 3 = "a required library is genuinely missing" -- the ONE code that
# should ever block launch. See the exit-codes docstring at the top.
exit 3
