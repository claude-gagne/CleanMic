#!/usr/bin/env bash
#
# test-appimage-preflight.sh -- regression test for scripts/appimage-preflight.sh
#
# Self-contained: PATH-shims a fake `ldd` (and fake `zenity`) plus fake
# os-release files inside a temp sandbox, so the missing-lib branch, distro
# mapping, and GUI cascade are all exercised WITHOUT needing an actually
# broken system and WITHOUT spawning any real dialog windows (D-04).
#
# Not wired into the Makefile `test` target on purpose -- CleanMic's project
# bar is `cargo build` + `cargo test`; this is a standalone shell check meant
# to be run directly:
#
#   bash scripts/test-appimage-preflight.sh
#
# Exits 0 if every case PASSes, non-zero (with a FAIL summary) otherwise.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
HELPER="$PROJECT_ROOT/scripts/appimage-preflight.sh"

SANDBOX="$(mktemp -d)"
trap 'rm -rf "$SANDBOX"' EXIT

FAIL_COUNT=0

pass() { printf 'PASS: %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1"; FAIL_COUNT=$((FAIL_COUNT + 1)); }

# Real /bin/true stands in for the cleanmic binary path arg -- the helper
# never actually inspects binary contents beyond passing it to `ldd`.
BINARY_ARG="/bin/true"

# ─────────────────────────────────────────────────────────────────────────
# Case 1: missing-lib branch, multi-lib parsing (D-02)
# ─────────────────────────────────────────────────────────────────────────
CASE1_DIR="$SANDBOX/case1"
mkdir -p "$CASE1_DIR"
cat > "$CASE1_DIR/ldd" << 'EOF'
#!/usr/bin/env bash
echo "	libc.so.6 => /lib/libc.so.6 (0x1234)"
echo "	libadwaita-1.so.0 => not found"
echo "	libbogusdep.so.9 => not found"
EOF
chmod +x "$CASE1_DIR/ldd"

set +e
CASE1_STDERR="$(PATH="$CASE1_DIR:$PATH" PREFLIGHT_NO_GUI=1 bash "$HELPER" "$BINARY_ARG" 2>&1 1>/dev/null)"
CASE1_EXIT=$?
set -e

if [ "$CASE1_EXIT" -eq 1 ]; then
    pass "case1: exit code is 1 on missing lib"
else
    fail "case1: expected exit 1, got $CASE1_EXIT"
fi

if printf '%s' "$CASE1_STDERR" | grep -q 'libadwaita-1.so.0'; then
    pass "case1: stderr names libadwaita-1.so.0"
else
    fail "case1: stderr missing libadwaita-1.so.0 (got: $CASE1_STDERR)"
fi

if printf '%s' "$CASE1_STDERR" | grep -q 'libbogusdep.so.9'; then
    pass "case1: stderr names second missing lib (D-02 multi-lib parsing)"
else
    fail "case1: stderr missing libbogusdep.so.9 (got: $CASE1_STDERR)"
fi

# ─────────────────────────────────────────────────────────────────────────
# Case 1b: CR-01 regression -- a glibc symbol-version mismatch line (ends in
# "not found" but has NO "=>" token) must NOT be misdetected as a missing
# library, even alongside a genuinely resolved line. Happy path expected.
# ─────────────────────────────────────────────────────────────────────────
CASE1B_DIR="$SANDBOX/case1b"
mkdir -p "$CASE1B_DIR"
cat > "$CASE1B_DIR/ldd" << 'EOF'
#!/usr/bin/env bash
echo "	libc.so.6 => /lib/libc.so.6 (0x1234)"
echo "./cleanmic: /lib/x86_64-linux-gnu/libc.so.6: version \`GLIBC_2.38' not found (required by ./cleanmic)"
EOF
chmod +x "$CASE1B_DIR/ldd"

set +e
CASE1B_STDERR="$(PATH="$CASE1B_DIR:$PATH" PREFLIGHT_NO_GUI=1 bash "$HELPER" "$BINARY_ARG" 2>&1 1>/dev/null)"
CASE1B_EXIT=$?
set -e

if [ "$CASE1B_EXIT" -eq 0 ]; then
    pass "case1b: GLIBC version-mismatch line (no '=>' token) treated as happy path (exit 0)"
else
    fail "case1b: expected exit 0 (fail-open on GLIBC mismatch), got $CASE1B_EXIT"
fi

if [ -z "$CASE1B_STDERR" ]; then
    pass "case1b: no false 'missing library' message printed for GLIBC mismatch"
else
    fail "case1b: expected empty stderr, got: $CASE1B_STDERR"
fi

# ─────────────────────────────────────────────────────────────────────────
# Case 2: happy path -- no missing libs
# ─────────────────────────────────────────────────────────────────────────
CASE2_DIR="$SANDBOX/case2"
mkdir -p "$CASE2_DIR"
cat > "$CASE2_DIR/ldd" << 'EOF'
#!/usr/bin/env bash
echo "	libc.so.6 => /lib/libc.so.6 (0x1234)"
echo "	libm.so.6 => /lib/libm.so.6 (0x5678)"
EOF
chmod +x "$CASE2_DIR/ldd"

set +e
CASE2_STDERR="$(PATH="$CASE2_DIR:$PATH" PREFLIGHT_NO_GUI=1 bash "$HELPER" "$BINARY_ARG" 2>&1 1>/dev/null)"
CASE2_EXIT=$?
set -e

if [ "$CASE2_EXIT" -eq 0 ]; then
    pass "case2: exit code is 0 on happy path"
else
    fail "case2: expected exit 0, got $CASE2_EXIT"
fi

if [ -z "$CASE2_STDERR" ]; then
    pass "case2: stderr is empty on happy path"
else
    fail "case2: expected empty stderr, got: $CASE2_STDERR"
fi

# ─────────────────────────────────────────────────────────────────────────
# Case 3: distro mapping via PREFLIGHT_OS_RELEASE (missing-lib fake ldd active)
# ─────────────────────────────────────────────────────────────────────────
CASE3_DIR="$SANDBOX/case3"
mkdir -p "$CASE3_DIR"
cat > "$CASE3_DIR/ldd" << 'EOF'
#!/usr/bin/env bash
echo "	libadwaita-1.so.0 => not found"
EOF
chmod +x "$CASE3_DIR/ldd"

run_distro_case() {
    local label="$1" os_release_content="$2" expect="$3"
    local os_file="$SANDBOX/os-release-$label"
    printf '%s\n' "$os_release_content" > "$os_file"

    set +e
    local out
    out="$(PATH="$CASE3_DIR:$PATH" PREFLIGHT_NO_GUI=1 PREFLIGHT_OS_RELEASE="$os_file" bash "$HELPER" "$BINARY_ARG" 2>&1 1>/dev/null)"
    set -e

    if printf '%s' "$out" | grep -q -- "$expect"; then
        pass "case3 ($label): stderr contains expected command(s)"
    else
        fail "case3 ($label): expected to find '$expect' in: $out"
    fi
}

run_distro_case "arch" 'ID=arch' 'pacman -S libadwaita'
run_distro_case "ubuntu" 'ID=ubuntu' 'apt install libadwaita-1-0'
run_distro_case "fedora" 'ID=fedora' 'dnf install libadwaita'

# Unknown distro -> all three commands listed.
set +e
CASE3_UNKNOWN_OS="$SANDBOX/os-release-weirdos"
printf 'ID=weirdos\n' > "$CASE3_UNKNOWN_OS"
CASE3_UNKNOWN_OUT="$(PATH="$CASE3_DIR:$PATH" PREFLIGHT_NO_GUI=1 PREFLIGHT_OS_RELEASE="$CASE3_UNKNOWN_OS" bash "$HELPER" "$BINARY_ARG" 2>&1 1>/dev/null)"
set -e
if printf '%s' "$CASE3_UNKNOWN_OUT" | grep -q 'pacman -S libadwaita' \
    && printf '%s' "$CASE3_UNKNOWN_OUT" | grep -q 'apt install libadwaita-1-0' \
    && printf '%s' "$CASE3_UNKNOWN_OUT" | grep -q 'dnf install libadwaita'; then
    pass "case3 (weirdos/unknown): stderr lists all three install commands"
else
    fail "case3 (weirdos/unknown): expected all three commands, got: $CASE3_UNKNOWN_OUT"
fi

# ID_LIKE=arch (EndeavourOS case) -- proves ID_LIKE is honored, not just ID.
run_distro_case "endeavouros" "ID=endeavouros
ID_LIKE=arch" 'pacman -S libadwaita'

# ─────────────────────────────────────────────────────────────────────────
# Case 4: GUI cascade selection -- fake zenity present, kdialog absent
# ─────────────────────────────────────────────────────────────────────────
CASE4_DIR="$SANDBOX/case4"
mkdir -p "$CASE4_DIR"
cat > "$CASE4_DIR/ldd" << 'EOF'
#!/usr/bin/env bash
echo "	libadwaita-1.so.0 => not found"
EOF
chmod +x "$CASE4_DIR/ldd"

MARKER_FILE="$SANDBOX/zenity-marker"
cat > "$CASE4_DIR/zenity" << EOF
#!/usr/bin/env bash
echo "called" > "$MARKER_FILE"
exit 0
EOF
chmod +x "$CASE4_DIR/zenity"

# Minimal controlled PATH: sandbox dir (fake ldd + fake zenity, no kdialog
# shim) first, then just enough of the real PATH for coreutils (grep/awk/cut/
# tr/wc) that the helper and this harness both need. `command -v` resolves in
# PATH order, so our fake zenity wins over any real one; kdialog is assumed
# absent on the standard CI/dev hosts this test targets (verified: not present
# on this host).
set +e
PATH="$CASE4_DIR:/usr/bin:/bin" PREFLIGHT_NO_GUI="" bash "$HELPER" "$BINARY_ARG" >/dev/null 2>&1
set -e

if [ -f "$MARKER_FILE" ]; then
    pass "case4: GUI cascade fell through to fake zenity (marker file written)"
else
    fail "case4: expected zenity marker file to be written, cascade did not invoke zenity"
fi

# ─────────────────────────────────────────────────────────────────────────
# Summary
# ─────────────────────────────────────────────────────────────────────────
if [ "$FAIL_COUNT" -eq 0 ]; then
    printf '\nAll cases PASSED.\n'
    exit 0
else
    printf '\n%d case(s) FAILED.\n' "$FAIL_COUNT"
    exit 1
fi
