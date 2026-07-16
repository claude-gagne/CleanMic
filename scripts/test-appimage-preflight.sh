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

if [ "$CASE1_EXIT" -eq 3 ]; then
    pass "case1: exit code is 3 on missing lib"
else
    fail "case1: expected exit 3, got $CASE1_EXIT"
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

# Fully controlled PATH (WR-02): rather than falling back to the real
# /usr/bin:/bin (whose contents depend on the host -- a KDE dev box would
# have a real kdialog there, causing a spurious FAIL), build a minimal bin
# directory containing ONLY symlinks to the specific coreutils the helper
# needs (grep/awk/cut/tr/wc/head), plus this sandbox's fake ldd/zenity. No
# real system bin directory is ever on PATH, so kdialog (or any other real
# GUI tool) can NEVER be resolved here, regardless of what's installed on the
# host running this test.
CASE4_BIN="$SANDBOX/case4-bin"
mkdir -p "$CASE4_BIN"
# `bash` itself must be resolvable via PATH too: the fake ldd/zenity scripts
# use a `#!/usr/bin/env bash` shebang, and `env` looks up `bash` on PATH when
# the OS execs them -- without a real bin directory on PATH, that lookup
# would otherwise fail and silently break the fakes. `sed` is needed by the
# helper's message-indentation logic (D-03b).
for tool in grep awk cut tr wc head sed bash; do
    # `type -P` resolves the on-disk binary even if a shell function or
    # alias shadows the name in this shell (command -v would return just
    # the bare name in that case, producing a broken/relative symlink).
    tool_path="$(type -P "$tool")"
    ln -s "$tool_path" "$CASE4_BIN/$tool"
done

# Resolve bash itself BEFORE restricting PATH, then invoke by absolute path
# so the restricted PATH only governs what the helper script can see.
BASH_BIN="$(type -P bash)"

set +e
PATH="$CASE4_DIR:$CASE4_BIN" PREFLIGHT_NO_GUI="" "$BASH_BIN" "$HELPER" "$BINARY_ARG" >/dev/null 2>&1
set -e

if [ -f "$MARKER_FILE" ]; then
    pass "case4: GUI cascade fell through to fake zenity (marker file written)"
else
    fail "case4: expected zenity marker file to be written, cascade did not invoke zenity"
fi

# ─────────────────────────────────────────────────────────────────────────
# Case 5: localization (D-03b) -- French vs. English plain message
# ─────────────────────────────────────────────────────────────────────────
CASE5_DIR="$SANDBOX/case5"
mkdir -p "$CASE5_DIR"
cat > "$CASE5_DIR/ldd" << 'EOF'
#!/usr/bin/env bash
echo "	libadwaita-1.so.0 => not found"
EOF
chmod +x "$CASE5_DIR/ldd"

set +e
CASE5_FR_STDERR="$(PATH="$CASE5_DIR:$PATH" PREFLIGHT_NO_GUI=1 PREFLIGHT_LANG=fr_FR.UTF-8 bash "$HELPER" "$BINARY_ARG" 2>&1 1>/dev/null)"
set -e

if printf '%s' "$CASE5_FR_STDERR" | grep -q 'CleanMic ne peut pas démarrer'; then
    pass "case5 (fr): stderr shows French heading"
else
    fail "case5 (fr): expected French heading, got: $CASE5_FR_STDERR"
fi

if printf '%s' "$CASE5_FR_STDERR" | grep -q 'Installez-la'; then
    pass "case5 (fr): stderr shows French singular install phrasing"
else
    fail "case5 (fr): expected 'Installez-la', got: $CASE5_FR_STDERR"
fi

set +e
CASE5_EN_STDERR="$(PATH="$CASE5_DIR:$PATH" PREFLIGHT_NO_GUI=1 PREFLIGHT_LANG=en_US.UTF-8 bash "$HELPER" "$BINARY_ARG" 2>&1 1>/dev/null)"
set -e

if printf '%s' "$CASE5_EN_STDERR" | grep -q "CleanMic can't start"; then
    pass "case5 (en): stderr shows English heading"
else
    fail "case5 (en): expected English heading, got: $CASE5_EN_STDERR"
fi

if printf '%s' "$CASE5_EN_STDERR" | grep -q 'Install it'; then
    pass "case5 (en): stderr shows English singular install phrasing"
else
    fail "case5 (en): expected 'Install it', got: $CASE5_EN_STDERR"
fi

# ─────────────────────────────────────────────────────────────────────────
# Case 6: zenity argv/markup capture -- title, width, markup tags
# ─────────────────────────────────────────────────────────────────────────
CASE6_DIR="$SANDBOX/case6"
mkdir -p "$CASE6_DIR"
cat > "$CASE6_DIR/ldd" << 'EOF'
#!/usr/bin/env bash
echo "	libadwaita-1.so.0 => not found"
EOF
chmod +x "$CASE6_DIR/ldd"

CASE6_ARGV_FILE="$SANDBOX/zenity-argv"
cat > "$CASE6_DIR/zenity" << EOF
#!/usr/bin/env bash
printf '%s\n' "\$@" > "$CASE6_ARGV_FILE"
exit 0
EOF
chmod +x "$CASE6_DIR/zenity"

CASE6_BIN="$SANDBOX/case6-bin"
mkdir -p "$CASE6_BIN"
for tool in grep awk cut tr wc head sed bash; do
    tool_path="$(type -P "$tool")"
    ln -s "$tool_path" "$CASE6_BIN/$tool"
done

set +e
PATH="$CASE6_DIR:$CASE6_BIN" PREFLIGHT_NO_GUI="" "$BASH_BIN" "$HELPER" "$BINARY_ARG" >/dev/null 2>&1
set -e

if [ -f "$CASE6_ARGV_FILE" ] && grep -q -- '--title=CleanMic' "$CASE6_ARGV_FILE"; then
    pass "case6: zenity invoked with --title=CleanMic"
else
    fail "case6: expected --title=CleanMic in zenity argv, got: $(cat "$CASE6_ARGV_FILE" 2>/dev/null)"
fi

CASE6_ARGV_JOINED="$(cat "$CASE6_ARGV_FILE" 2>/dev/null || true)"
if printf '%s' "$CASE6_ARGV_JOINED" | grep -q '<b>' && printf '%s' "$CASE6_ARGV_JOINED" | grep -q '<tt>'; then
    pass "case6: zenity --text contains <b> and <tt> markup"
else
    fail "case6: expected <b> and <tt> in zenity argv, got: $CASE6_ARGV_JOINED"
fi

# ─────────────────────────────────────────────────────────────────────────
# Case 7: kdialog path -- HTML subset, title, no zenity/others present
# ─────────────────────────────────────────────────────────────────────────
CASE7_DIR="$SANDBOX/case7"
mkdir -p "$CASE7_DIR"
cat > "$CASE7_DIR/ldd" << 'EOF'
#!/usr/bin/env bash
echo "	libadwaita-1.so.0 => not found"
EOF
chmod +x "$CASE7_DIR/ldd"

CASE7_ARGV_FILE="$SANDBOX/kdialog-argv"
cat > "$CASE7_DIR/kdialog" << EOF
#!/usr/bin/env bash
printf '%s\n' "\$@" > "$CASE7_ARGV_FILE"
exit 0
EOF
chmod +x "$CASE7_DIR/kdialog"

CASE7_BIN="$SANDBOX/case7-bin"
mkdir -p "$CASE7_BIN"
for tool in grep awk cut tr wc head sed bash; do
    tool_path="$(type -P "$tool")"
    ln -s "$tool_path" "$CASE7_BIN/$tool"
done

set +e
PATH="$CASE7_DIR:$CASE7_BIN" PREFLIGHT_NO_GUI="" "$BASH_BIN" "$HELPER" "$BINARY_ARG" >/dev/null 2>&1
set -e

CASE7_ARGV_JOINED="$(cat "$CASE7_ARGV_FILE" 2>/dev/null || true)"
if printf '%s' "$CASE7_ARGV_JOINED" | grep -q 'CleanMic'; then
    pass "case7: kdialog invoked with CleanMic title"
else
    fail "case7: expected CleanMic title in kdialog argv, got: $CASE7_ARGV_JOINED"
fi

if printf '%s' "$CASE7_ARGV_JOINED" | grep -q '<h3>' && printf '%s' "$CASE7_ARGV_JOINED" | grep -q '<tt>'; then
    pass "case7: kdialog message contains <h3> and <tt> markup"
else
    fail "case7: expected <h3> and <tt> in kdialog argv, got: $CASE7_ARGV_JOINED"
fi

# ─────────────────────────────────────────────────────────────────────────
# Case 8: notify-send path -- kdialog/zenity absent, body stays PLAIN
# ─────────────────────────────────────────────────────────────────────────
CASE8_DIR="$SANDBOX/case8"
mkdir -p "$CASE8_DIR"
cat > "$CASE8_DIR/ldd" << 'EOF'
#!/usr/bin/env bash
echo "	libadwaita-1.so.0 => not found"
EOF
chmod +x "$CASE8_DIR/ldd"

CASE8_ARGV_FILE="$SANDBOX/notify-send-argv"
cat > "$CASE8_DIR/notify-send" << EOF
#!/usr/bin/env bash
printf '%s\n' "\$@" > "$CASE8_ARGV_FILE"
exit 0
EOF
chmod +x "$CASE8_DIR/notify-send"

CASE8_BIN="$SANDBOX/case8-bin"
mkdir -p "$CASE8_BIN"
for tool in grep awk cut tr wc head sed bash; do
    tool_path="$(type -P "$tool")"
    ln -s "$tool_path" "$CASE8_BIN/$tool"
done

set +e
PATH="$CASE8_DIR:$CASE8_BIN" PREFLIGHT_NO_GUI="" "$BASH_BIN" "$HELPER" "$BINARY_ARG" >/dev/null 2>&1
set -e

CASE8_ARGV_JOINED="$(cat "$CASE8_ARGV_FILE" 2>/dev/null || true)"
if [ -n "$CASE8_ARGV_JOINED" ] \
    && ! printf '%s' "$CASE8_ARGV_JOINED" | grep -q '<b>' \
    && ! printf '%s' "$CASE8_ARGV_JOINED" | grep -q '<tt>' \
    && ! printf '%s' "$CASE8_ARGV_JOINED" | grep -q '<h3>'; then
    pass "case8: notify-send body is plain (no <b>/<tt>/<h3> markup)"
else
    fail "case8: expected plain notify-send body with no markup, got: $CASE8_ARGV_JOINED"
fi

# ─────────────────────────────────────────────────────────────────────────
# Case 9: markup-escape (T-10-02) -- dynamic missing-lib token with '<'/'&'
# ─────────────────────────────────────────────────────────────────────────
CASE9_DIR="$SANDBOX/case9"
mkdir -p "$CASE9_DIR"
cat > "$CASE9_DIR/ldd" << 'EOF'
#!/usr/bin/env bash
echo "	lib&bad<name>.so.0 => not found"
EOF
chmod +x "$CASE9_DIR/ldd"

CASE9_ARGV_FILE="$SANDBOX/case9-zenity-argv"
cat > "$CASE9_DIR/zenity" << EOF
#!/usr/bin/env bash
printf '%s\n' "\$@" > "$CASE9_ARGV_FILE"
exit 0
EOF
chmod +x "$CASE9_DIR/zenity"

CASE9_BIN="$SANDBOX/case9-bin"
mkdir -p "$CASE9_BIN"
for tool in grep awk cut tr wc head sed bash; do
    tool_path="$(type -P "$tool")"
    ln -s "$tool_path" "$CASE9_BIN/$tool"
done

set +e
PATH="$CASE9_DIR:$CASE9_BIN" PREFLIGHT_NO_GUI="" "$BASH_BIN" "$HELPER" "$BINARY_ARG" >/dev/null 2>&1
set -e

CASE9_ARGV_JOINED="$(cat "$CASE9_ARGV_FILE" 2>/dev/null || true)"
if printf '%s' "$CASE9_ARGV_JOINED" | grep -q 'lib&amp;bad&lt;name&gt;.so.0'; then
    pass "case9: zenity --text contains escaped dynamic lib token (T-10-02)"
else
    fail "case9: expected escaped 'lib&amp;bad&lt;name&gt;.so.0' in zenity argv, got: $CASE9_ARGV_JOINED"
fi

if printf '%s' "$CASE9_ARGV_JOINED" | grep -q 'lib&bad<name>.so.0'; then
    fail "case9: raw unescaped lib token 'lib&bad<name>.so.0' leaked into zenity argv"
else
    pass "case9: no raw unescaped lib token leaked into zenity argv"
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
