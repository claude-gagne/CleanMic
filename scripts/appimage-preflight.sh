#!/usr/bin/env bash
#
# appimage-preflight.sh -- pre-flight host-library check for the CleanMic AppImage.
#
# Invoked by AppRun immediately before `exec`ing the real cleanmic binary.
# Detects any missing shared library the binary needs (e.g. libadwaita-1.so.0
# on non-GNOME hosts) and, instead of letting the dynamic linker crash with a
# cryptic error, prints a clear stderr message naming the missing lib(s) and
# a per-distro, per-library install command, best-effort mirrors that
# message via a GUI dialog for double-click users, then exits non-zero so
# AppRun does not exec. Separately (15.4-02/D-05), a too-old host glibc
# (CR-01: a symbol-version mismatch, NOT a missing library) gets its own
# distinct "needs a newer system" message with no install command.
#
# Usage: appimage-preflight.sh <path-to-cleanmic-binary>
#
# Exit codes:
#   0 - happy path: no missing library and no glibc-version mismatch
#       detected (also used when `ldd` is unavailable, or the binary-path
#       argument is missing/empty -- see fail-open note below).
#   3 - EITHER a required shared library is genuinely missing, OR the host
#       glibc is too old for this build. AppRun must abort launch ONLY on
#       this specific code. Any OTHER nonzero exit means the helper itself
#       failed to run to completion (crash, bad shebang, lost +x, etc.) and
#       is unrelated to either case -- callers must fail OPEN on those
#       (still exec the app) so a bug in this ~250-line helper can never
#       brick a healthy launch.
#
# Fail-open: if `ldd` is unavailable, the binary-path argument ($1) is
# missing/empty, or ldd reports nothing missing/mismatched, this script
# exits 0 immediately -- the happy path costs exactly one `ldd` invocation.
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
# Reconciliation with bundling (D-05, 15.4-02): the missing-library branch
# below now maps EACH missing soname to the correct per-distro package via a
# fixed table, instead of a single libadwaita-only literal command -- so as
# later plans bundle more of the GUI stack (Plan 03), the corresponding
# table rows simply become unreachable (their sonames never resolve to
# "not found" anymore) with no code change here. Today, still-host-provided
# libraries are: libadwaita-1.so.0, libgtk-4.so.1, libdbus-1.so.3 (always
# host-provided, D-01), the glib/gio/gobject sonames, and (only reachable if
# the bundled fallback file itself is somehow missing) libpipewire-0.3.so.0.
# A soname with no table entry gets named with generic guidance -- this
# script never invents a package name for a library it doesn't recognize.
#
# Threat model notes (see 10-CONTEXT.md / 15.4-02-PLAN.md threat_model):
#   - os-release ID/ID_LIKE values, and every soname parsed from `ldd`
#     output, only ever select a fixed literal table row below; nothing is
#     ever eval'd or executed.
#   - The message is passed to each GUI dialog tool as a single argument, never
#     via a re-splittable/eval'd shell string.
#   - Dynamic values (the missing-lib names, and the glibc version strings
#     parsed from `ldd`/`ldd --version`) are markup-escaped (`& < >`) before
#     being embedded into the zenity/kdialog markup forms, preserving the
#     T-10-02 no-injection mitigation.

set -euo pipefail

BINARY="${1:-}"

if [ -z "$BINARY" ]; then
    echo "appimage-preflight.sh: missing required argument <path-to-cleanmic-binary>" >&2
    exit 0
fi

# ── Detection (D-01/D-02, CR-01, D-05) ───────────────────────────────────────
# Fail-open if ldd itself is missing or errors -- `command -v` guard avoids
# `set -e` aborting the whole script.
LDD_OUTPUT=""
if command -v ldd >/dev/null 2>&1; then
    LDD_OUTPUT="$(ldd "$BINARY" 2>/dev/null || true)"
fi

# Anchor on the "=> not found" shape of a genuine missing shared-library
# dependency line. A bare 'not found' also matches glibc symbol-version
# mismatch lines (e.g. "version `GLIBC_2.38' not found"), which have no
# "=>" token and are NOT a missing library -- misclassifying them here
# would print the binary's own path as a fake "missing library" and a
# wrong install command (CR-01).
MISSING="$(printf '%s\n' "$LDD_OUTPUT" | LC_ALL=C grep '=> not found' | awk '{print $1}' || true)"

# A glibc symbol-version mismatch line: ends in "not found" but has no "=>"
# token. Only consulted when MISSING is empty -- a genuinely missing
# library takes priority over a glibc mismatch (the more actionable
# failure wins if both somehow appear at once).
GLIBC_REQUIRED=""
if [ -z "$MISSING" ]; then
    GLIBC_LINES="$(printf '%s\n' "$LDD_OUTPUT" | LC_ALL=C grep -E "GLIBC_[0-9]+\.[0-9]+' not found" || true)"
    if [ -n "$GLIBC_LINES" ]; then
        GLIBC_REQUIRED="$(printf '%s\n' "$GLIBC_LINES" | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sed 's/GLIBC_//' | sort -V -u | tail -n1 || true)"
    fi
fi

if [ -z "$MISSING" ] && [ -z "$GLIBC_REQUIRED" ]; then
    # All libs resolve, or ldd unavailable/errored -- fail-open, launch proceeds.
    exit 0
fi

# ── Locale detection (D-03b) -- shared by both branches below ──────────────
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

# Markup-escape (T-10-02): escape a DYNAMIC token before it is ever embedded
# in the zenity/kdialog markup forms below. Order matters -- '&' must be
# escaped first, or the '&' introduced by escaping '<'/'>' would itself get
# re-escaped.
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

# ── GUI cascade dispatcher (D-03 / D-03a) -- shared by both branches ───────
# kdialog -> zenity -> notify-send -> xmessage, first available wins. Each
# tool receives the message as a single argument, never a re-splittable/
# eval'd shell string. The two modal dialogs (kdialog, zenity) get the
# richer markup form; notify-send/xmessage stay PLAIN. Skipped entirely
# under PREFLIGHT_NO_GUI.
show_gui() {
    local zen_text="$1" kd_msg="$2" plain_msg="$3"
    [ -n "${PREFLIGHT_NO_GUI:-}" ] && return 0
    if command -v kdialog >/dev/null 2>&1; then
        kdialog --title "CleanMic" --error "$kd_msg" >/dev/null 2>&1 || true
    elif command -v zenity >/dev/null 2>&1; then
        zenity --title="CleanMic" --width=440 --error --text "$zen_text" >/dev/null 2>&1 || true
    elif command -v notify-send >/dev/null 2>&1; then
        notify-send "CleanMic" "$plain_msg" >/dev/null 2>&1 || true
    elif command -v xmessage >/dev/null 2>&1; then
        xmessage -center "$plain_msg" >/dev/null 2>&1 || true
    fi
}

# ═══════════════════════════════════════════════════════════════════════
# Branch 1 (15.4-02/D-05): host glibc is too old for this build. NOT a
# missing library (CR-01) -- there is nothing to apt/dnf/pacman install for
# this, so no install command is ever shown here.
# ═══════════════════════════════════════════════════════════════════════
if [ -z "$MISSING" ]; then
    HOST_LDD_VERSION="$(ldd --version 2>/dev/null | head -n1 || true)"
    GLIBC_REQUIRED_ESCAPED="$(escape_markup "$GLIBC_REQUIRED")"
    HOST_LDD_VERSION_ESCAPED="$(escape_markup "$HOST_LDD_VERSION")"

    if [ "$IS_FRENCH" -eq 1 ]; then
        G_HEADING="CleanMic a besoin d'un système plus récent"
        G_BODY="Cette version de CleanMic nécessite glibc $GLIBC_REQUIRED ou une version plus récente. Votre système a :"
        G_BODY_ESC="Cette version de CleanMic nécessite glibc $GLIBC_REQUIRED_ESCAPED ou une version plus récente. Votre système a :"
    else
        G_HEADING="CleanMic needs a newer system"
        G_BODY="This build of CleanMic needs glibc $GLIBC_REQUIRED or newer. Your system has:"
        G_BODY_ESC="This build of CleanMic needs glibc $GLIBC_REQUIRED_ESCAPED or newer. Your system has:"
    fi

    G_MSG="$G_HEADING
$G_BODY

    $HOST_LDD_VERSION
"
    printf '%s\n' "$G_MSG" >&2

    G_ZEN_TEXT="<b>$G_HEADING</b>

$G_BODY_ESC

<tt>    $HOST_LDD_VERSION_ESCAPED</tt>"
    G_KD_MSG="<h3>$G_HEADING</h3><p>$G_BODY_ESC</p><p><tt>$HOST_LDD_VERSION_ESCAPED</tt></p>"

    show_gui "$G_ZEN_TEXT" "$G_KD_MSG" "$G_MSG"
    exit 3
fi

# ═══════════════════════════════════════════════════════════════════════
# Branch 2: one or more required shared libraries are genuinely missing
# (D-01/D-02/D-05).
# ═══════════════════════════════════════════════════════════════════════

# ── Distro detection -> fixed family token (never eval os-release values) ──
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

FAMILY="unknown"
case "$DISTRO_TOKENS" in
    *arch*)
        FAMILY="arch"
        ;;
    *debian*|*ubuntu*)
        FAMILY="debian"
        ;;
    *fedora*)
        FAMILY="fedora"
        ;;
esac

# ── Fixed soname -> package table, one row per (family, soname) (D-05) ─────
# Values only ever select a literal package-name string below; nothing here
# is eval'd or executed. A soname with no entry for a family falls through
# to the `return 1` default -- callers must never invent a package name.
package_for_soname() {
    local family="$1" soname="$2"
    case "$family:$soname" in
        arch:libadwaita-1.so.0) printf 'libadwaita' ;;
        arch:libgtk-4.so.1) printf 'gtk4' ;;
        arch:libdbus-1.so.3) printf 'dbus' ;;
        arch:libpipewire-0.3.so.0) printf 'pipewire' ;;
        arch:libglib-2.0.so.0|arch:libgio-2.0.so.0|arch:libgobject-2.0.so.0)
            printf 'glib2'
            ;;
        debian:libadwaita-1.so.0) printf 'libadwaita-1-0' ;;
        debian:libgtk-4.so.1) printf 'libgtk-4-1' ;;
        debian:libdbus-1.so.3) printf 'libdbus-1-3' ;;
        debian:libpipewire-0.3.so.0) printf 'libpipewire-0.3-0t64' ;;
        debian:libglib-2.0.so.0|debian:libgio-2.0.so.0|debian:libgobject-2.0.so.0)
            printf 'libglib2.0-0t64'
            ;;
        fedora:libadwaita-1.so.0) printf 'libadwaita' ;;
        fedora:libgtk-4.so.1) printf 'gtk4' ;;
        fedora:libdbus-1.so.3) printf 'dbus-libs' ;;
        fedora:libpipewire-0.3.so.0) printf 'pipewire-libs' ;;
        fedora:libglib-2.0.so.0|fedora:libgio-2.0.so.0|fedora:libgobject-2.0.so.0)
            printf 'glib2'
            ;;
        *)
            return 1
            ;;
    esac
}

# is_unmapped_anywhere SONAME -- true iff no family's table has an entry
# for this soname (used only for the FAMILY=unknown case, so the "never
# invent a package name" guarantee holds even when the host distro itself
# couldn't be identified).
is_unmapped_anywhere() {
    local lib="$1"
    package_for_soname arch "$lib" >/dev/null 2>&1 && return 1
    package_for_soname debian "$lib" >/dev/null 2>&1 && return 1
    package_for_soname fedora "$lib" >/dev/null 2>&1 && return 1
    return 0
}

# build_family_command FAMILY PREFIX -- prints "PREFIX pkg1 pkg2 ..." for
# every MISSING soname this family's table maps, deduped, insertion order
# preserved; prints nothing if none of MISSING maps for this family.
build_family_command() {
    local family="$1" prefix="$2" pkgs="" pkg lib
    while IFS= read -r lib; do
        [ -n "$lib" ] || continue
        if pkg="$(package_for_soname "$family" "$lib")"; then
            case " $pkgs " in
                *" $pkg "*) ;;
                *) pkgs="$pkgs $pkg" ;;
            esac
        fi
    done <<EOF
$MISSING
EOF
    pkgs="${pkgs# }"
    [ -n "$pkgs" ] && printf '%s %s' "$prefix" "$pkgs"
    return 0
}

ARCH_CMD="$(build_family_command arch 'sudo pacman -S')"
APT_CMD="$(build_family_command debian 'sudo apt install')"
DNF_CMD="$(build_family_command fedora 'sudo dnf install')"

case "$FAMILY" in
    arch) INSTALL_CMD="$ARCH_CMD" ;;
    debian) INSTALL_CMD="$APT_CMD" ;;
    fedora) INSTALL_CMD="$DNF_CMD" ;;
    *)
        INSTALL_CMD=""
        for c in "$ARCH_CMD" "$APT_CMD" "$DNF_CMD"; do
            [ -n "$c" ] || continue
            if [ -z "$INSTALL_CMD" ]; then
                INSTALL_CMD="$c"
            else
                INSTALL_CMD="$INSTALL_CMD
$c"
            fi
        done
        ;;
esac

# Libraries this script has no package name for, for THIS family selection
# (or for any family, when the distro itself is unknown) -- named with
# generic guidance below, never with an invented command (D-05).
UNMAPPED_LIBS=""
while IFS= read -r lib; do
    [ -n "$lib" ] || continue
    unmapped=0
    if [ "$FAMILY" = "unknown" ]; then
        is_unmapped_anywhere "$lib" && unmapped=1
    else
        package_for_soname "$FAMILY" "$lib" >/dev/null 2>&1 || unmapped=1
    fi
    if [ "$unmapped" -eq 1 ]; then
        if [ -z "$UNMAPPED_LIBS" ]; then
            UNMAPPED_LIBS="$lib"
        else
            UNMAPPED_LIBS="$UNMAPPED_LIBS
$lib"
        fi
    fi
done <<EOF
$MISSING
EOF

# ── Message builder (D-03 / D-03b / D-05) ────────────────────────────────
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
    GENERIC_HEADING="Aucune commande d'installation précise n'est disponible pour :"
    GENERIC_ACTION="Cherchez dans le gestionnaire de paquets de votre distribution une bibliothèque fournissant chacune d'elles, puis relancez CleanMic."
else
    HEADING="CleanMic can't start"
    if [ "$IS_PLURAL" -eq 1 ]; then
        BODY="The following required system libraries are missing:"
        ACTION="Install them, then launch CleanMic again:"
    else
        BODY="A required system library is missing:"
        ACTION="Install it, then launch CleanMic again:"
    fi
    GENERIC_HEADING="No specific install command is available for:"
    GENERIC_ACTION="Search your distribution's package manager for a library providing each of these, then launch CleanMic again."
fi

MISSING_ESCAPED="$(escape_markup "$MISSING")"

# ── PLAIN form -- stderr, notify-send body, xmessage ─────────────────────────
# No markup; each lib/command line indented 4 spaces; blank-line grouping
# between heading (+body), libs, action/command, and generic-guidance block.
MISSING_INDENTED="$(printf '%s\n' "$MISSING" | sed 's/^/    /')"

MSG="$HEADING
$BODY

$MISSING_INDENTED
"
if [ -n "$INSTALL_CMD" ]; then
    INSTALL_CMD_INDENTED="$(printf '%s\n' "$INSTALL_CMD" | sed 's/^/    /')"
    MSG="$MSG
$ACTION

$INSTALL_CMD_INDENTED
"
fi
if [ -n "$UNMAPPED_LIBS" ]; then
    UNMAPPED_INDENTED="$(printf '%s\n' "$UNMAPPED_LIBS" | sed 's/^/    /')"
    MSG="$MSG
$GENERIC_HEADING

$UNMAPPED_INDENTED

$GENERIC_ACTION
"
fi

# ALWAYS print to stderr, regardless of GUI availability.
printf '%s\n' "$MSG" >&2

# ── ZENITY form -- Pango markup ──────────────────────────────────────────────
ZEN_MISSING_INDENTED="$(printf '%s\n' "$MISSING_ESCAPED" | sed 's/^/    /')"

ZEN_TEXT="<b>$HEADING</b>

$BODY

<tt>$ZEN_MISSING_INDENTED</tt>"
if [ -n "$INSTALL_CMD" ]; then
    ZEN_CMD_INDENTED="$(printf '%s\n' "$INSTALL_CMD" | sed 's/^/    /')"
    ZEN_TEXT="$ZEN_TEXT

$ACTION

<tt>$ZEN_CMD_INDENTED</tt>"
fi
if [ -n "$UNMAPPED_LIBS" ]; then
    UNMAPPED_ESCAPED="$(escape_markup "$UNMAPPED_LIBS")"
    ZEN_UNMAPPED_INDENTED="$(printf '%s\n' "$UNMAPPED_ESCAPED" | sed 's/^/    /')"
    ZEN_TEXT="$ZEN_TEXT

$GENERIC_HEADING

<tt>$ZEN_UNMAPPED_INDENTED</tt>

$GENERIC_ACTION"
fi

# ── KDIALOG form -- Qt HTML subset ───────────────────────────────────────────
KD_MISSING_JOINED="$(join_lines '<br>' "$MISSING_ESCAPED")"

KD_MSG="<h3>$HEADING</h3><p>$BODY</p><p><tt>$KD_MISSING_JOINED</tt></p>"
if [ -n "$INSTALL_CMD" ]; then
    KD_CMD_JOINED="$(join_lines '<br>' "$INSTALL_CMD")"
    KD_MSG="$KD_MSG<p>$ACTION</p><p><tt>$KD_CMD_JOINED</tt></p>"
fi
if [ -n "$UNMAPPED_LIBS" ]; then
    UNMAPPED_ESCAPED="$(escape_markup "$UNMAPPED_LIBS")"
    KD_UNMAPPED_JOINED="$(join_lines '<br>' "$UNMAPPED_ESCAPED")"
    KD_MSG="$KD_MSG<p>$GENERIC_HEADING</p><p><tt>$KD_UNMAPPED_JOINED</tt></p><p>$GENERIC_ACTION</p>"
fi

show_gui "$ZEN_TEXT" "$KD_MSG" "$MSG"

# Exit 3 = "a required library is genuinely missing" -- the ONE code that
# should ever block launch (shared with the glibc-too-old branch above).
# See the exit-codes docstring at the top.
exit 3
