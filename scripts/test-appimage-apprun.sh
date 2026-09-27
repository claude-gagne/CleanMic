#!/usr/bin/env bash
#
# test-appimage-apprun.sh -- regression test for scripts/appimage-apprun.sh
#
# Self-contained: builds a fake AppDir (fake usr/bin/cleanmic that dumps its
# own environment to a file, a fake usr/bin/cleanmic-preflight whose exit
# code is controlled per case, and a fully-restricted PATH with only the
# coreutils AppRun itself needs, plus a per-case fake/absent `ldd`) so the
# D-10 fallback decision and the pre-flight exit-code contract can be
# exercised WITHOUT a real AppImage, a real PipeWire install, or spawning
# any GUI.
#
# Not wired into the Makefile `test` target on purpose -- CleanMic's project
# bar is `cargo build` + `cargo test`; this is a standalone shell check,
# alongside test-appimage-preflight.sh, meant to be run directly:
#
#   bash scripts/test-appimage-apprun.sh
#
# Exits 0 if every case PASSes, non-zero (with a FAIL summary) otherwise.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
APPRUN_SRC="$PROJECT_ROOT/scripts/appimage-apprun.sh"

SANDBOX="$(mktemp -d)"
trap 'rm -rf "$SANDBOX"' EXIT

FAIL_COUNT=0
pass() { printf 'PASS: %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1"; FAIL_COUNT=$((FAIL_COUNT + 1)); }

# Fully controlled PATH (mirrors test-appimage-preflight.sh's WR-02 pattern):
# only symlinks to the exact coreutils AppRun needs, plus this sandbox's own
# fake ldd (or none at all, for case c) -- never the real system PATH, so
# the host's own real ldd/PipeWire install can never leak into a case.
BASE_BIN="$SANDBOX/base-bin"
mkdir -p "$BASE_BIN"
for tool in bash dirname readlink grep env cat mkdir chmod; do
    tool_path="$(type -P "$tool")"
    ln -s "$tool_path" "$BASE_BIN/$tool"
done
BASH_BIN="$(type -P bash)"

# make_appdir -- builds a fresh fake AppDir under $1, with AppRun copied
# from the real template, a fake cleanmic that dumps its env to $ENV_OUT,
# and a fake cleanmic-preflight whose exit code reads $FAKE_PREFLIGHT_EXIT
# at run time (not bake-time), so one fake binary serves every case.
make_appdir() {
    local dir="$1"
    rm -rf "$dir"
    mkdir -p "$dir/usr/bin" "$dir/usr/lib"
    cp "$APPRUN_SRC" "$dir/AppRun"
    chmod +x "$dir/AppRun"

    cat > "$dir/usr/bin/cleanmic" << 'EOF'
#!/usr/bin/env bash
env > "$FAKE_CLEANMIC_ENV_OUT"
exit 0
EOF
    chmod +x "$dir/usr/bin/cleanmic"

    cat > "$dir/usr/bin/cleanmic-preflight" << 'EOF'
#!/usr/bin/env bash
exit "${FAKE_PREFLIGHT_EXIT:-0}"
EOF
    chmod +x "$dir/usr/bin/cleanmic-preflight"
}

run_apprun() {
    # run_apprun APPDIR CASE_BIN ENV_OUT PREFLIGHT_EXIT [extra env "K=V" ...]
    local appdir="$1" case_bin="$2" env_out="$3" preflight_exit="$4"
    shift 4
    rm -f "$env_out"
    set +e
    PATH="$case_bin" FAKE_CLEANMIC_ENV_OUT="$env_out" FAKE_PREFLIGHT_EXIT="$preflight_exit" \
        env "$@" "$BASH_BIN" "$appdir/AppRun" >"$SANDBOX/apprun.stdout" 2>"$SANDBOX/apprun.stderr"
    APPRUN_EXIT=$?
    set -e
}

# ─────────────────────────────────────────────────────────────────────────
# Case (a): host resolves libpipewire -- no fallback dir added
# ─────────────────────────────────────────────────────────────────────────
APPDIR_A="$SANDBOX/appdir-a"
make_appdir "$APPDIR_A"
CASE_A_BIN="$SANDBOX/case-a-bin"
mkdir -p "$CASE_A_BIN"
for f in "$BASE_BIN"/*; do ln -s "$f" "$CASE_A_BIN/$(basename "$f")"; done
cat > "$CASE_A_BIN/ldd" << 'EOF'
#!/usr/bin/env bash
echo "	libpipewire-0.3.so.0 => /usr/lib/x86_64-linux-gnu/libpipewire-0.3.so.0 (0x1234)"
EOF
chmod +x "$CASE_A_BIN/ldd"

ENV_OUT_A="$SANDBOX/env-a.txt"
run_apprun "$APPDIR_A" "$CASE_A_BIN" "$ENV_OUT_A" 0

if [ -f "$ENV_OUT_A" ]; then
    pass "case a: cleanmic was exec'd (host resolves libpipewire)"
else
    fail "case a: cleanmic was never exec'd"
fi
if ! grep -q 'pipewire-fallback' "$ENV_OUT_A" 2>/dev/null; then
    pass "case a: LD_LIBRARY_PATH has no pipewire-fallback entry"
else
    fail "case a: unexpected pipewire-fallback in env: $(grep LD_LIBRARY_PATH "$ENV_OUT_A" 2>/dev/null)"
fi
if ! grep -q '^CLEANMIC_PIPEWIRE_FALLBACK=' "$ENV_OUT_A" 2>/dev/null; then
    pass "case a: CLEANMIC_PIPEWIRE_FALLBACK is not exported"
else
    fail "case a: CLEANMIC_PIPEWIRE_FALLBACK was unexpectedly exported"
fi
if ! grep -qE '^(GSETTINGS_SCHEMA_DIR|GI_TYPELIB_PATH|GDK_PIXBUF_MODULE_FILE)=' "$ENV_OUT_A" 2>/dev/null; then
    pass "case a: no bundled-GTK exports fire when none of the bundled paths exist (CLEANMIC_BUNDLE_GTK=0 shape)"
else
    fail "case a: unexpected bundled-GTK export in env: $(grep -E '^(GSETTINGS_SCHEMA_DIR|GI_TYPELIB_PATH|GDK_PIXBUF_MODULE_FILE)=' "$ENV_OUT_A" 2>/dev/null)"
fi

# ─────────────────────────────────────────────────────────────────────────
# Case (b): ldd reports libpipewire unresolved -- fallback appended LAST
# ─────────────────────────────────────────────────────────────────────────
APPDIR_B="$SANDBOX/appdir-b"
make_appdir "$APPDIR_B"
CASE_B_BIN="$SANDBOX/case-b-bin"
mkdir -p "$CASE_B_BIN"
for f in "$BASE_BIN"/*; do ln -s "$f" "$CASE_B_BIN/$(basename "$f")"; done
cat > "$CASE_B_BIN/ldd" << 'EOF'
#!/usr/bin/env bash
echo "	libpipewire-0.3.so.0 => not found"
EOF
chmod +x "$CASE_B_BIN/ldd"

ENV_OUT_B="$SANDBOX/env-b.txt"
run_apprun "$APPDIR_B" "$CASE_B_BIN" "$ENV_OUT_B" 0

LD_LIB_PATH_B="$(grep '^LD_LIBRARY_PATH=' "$ENV_OUT_B" 2>/dev/null | cut -d= -f2-)"
case "$LD_LIB_PATH_B" in
    *":$APPDIR_B/usr/lib/pipewire-fallback")
        pass "case b: pipewire-fallback dir appended LAST to LD_LIBRARY_PATH"
        ;;
    *)
        fail "case b: expected LD_LIBRARY_PATH to end with pipewire-fallback, got: $LD_LIB_PATH_B"
        ;;
esac
if grep -q '^CLEANMIC_PIPEWIRE_FALLBACK=1$' "$ENV_OUT_B" 2>/dev/null; then
    pass "case b: CLEANMIC_PIPEWIRE_FALLBACK=1 exported"
else
    fail "case b: expected CLEANMIC_PIPEWIRE_FALLBACK=1, got: $(grep CLEANMIC_PIPEWIRE_FALLBACK "$ENV_OUT_B" 2>/dev/null)"
fi

# ─────────────────────────────────────────────────────────────────────────
# Case (c): no ldd on PATH at all -- no fallback, app still execs
# ─────────────────────────────────────────────────────────────────────────
APPDIR_C="$SANDBOX/appdir-c"
make_appdir "$APPDIR_C"
CASE_C_BIN="$SANDBOX/case-c-bin"
mkdir -p "$CASE_C_BIN"
for f in "$BASE_BIN"/*; do
    b="$(basename "$f")"
    [ "$b" = "ldd" ] && continue
    ln -s "$f" "$CASE_C_BIN/$b"
done

ENV_OUT_C="$SANDBOX/env-c.txt"
run_apprun "$APPDIR_C" "$CASE_C_BIN" "$ENV_OUT_C" 0

if [ -f "$ENV_OUT_C" ]; then
    pass "case c: cleanmic still exec'd with no ldd on PATH"
else
    fail "case c: cleanmic was never exec'd with no ldd on PATH"
fi
if ! grep -q 'pipewire-fallback' "$ENV_OUT_C" 2>/dev/null; then
    pass "case c: no fallback dir added when ldd is unavailable"
else
    fail "case c: unexpected pipewire-fallback in env: $(grep LD_LIBRARY_PATH "$ENV_OUT_C" 2>/dev/null)"
fi

# ─────────────────────────────────────────────────────────────────────────
# Case (d): preflight exits 3 -- AppRun exits 1 WITHOUT exec'ing cleanmic
# ─────────────────────────────────────────────────────────────────────────
APPDIR_D="$SANDBOX/appdir-d"
make_appdir "$APPDIR_D"
CASE_D_BIN="$SANDBOX/case-d-bin"
mkdir -p "$CASE_D_BIN"
for f in "$BASE_BIN"/*; do ln -s "$f" "$CASE_D_BIN/$(basename "$f")"; done
cat > "$CASE_D_BIN/ldd" << 'EOF'
#!/usr/bin/env bash
echo "	libpipewire-0.3.so.0 => /usr/lib/x86_64-linux-gnu/libpipewire-0.3.so.0 (0x1234)"
EOF
chmod +x "$CASE_D_BIN/ldd"

ENV_OUT_D="$SANDBOX/env-d.txt"
run_apprun "$APPDIR_D" "$CASE_D_BIN" "$ENV_OUT_D" 3

if [ "$APPRUN_EXIT" -eq 1 ]; then
    pass "case d: AppRun exits 1 when preflight exits 3"
else
    fail "case d: expected AppRun exit 1, got $APPRUN_EXIT"
fi
if [ ! -f "$ENV_OUT_D" ]; then
    pass "case d: cleanmic was never exec'd (preflight abort)"
else
    fail "case d: cleanmic was unexpectedly exec'd despite preflight exit 3"
fi

# ─────────────────────────────────────────────────────────────────────────
# Case (e): preflight exits 7 (unexpected, non-3) -- fails open, execs anyway
# ─────────────────────────────────────────────────────────────────────────
APPDIR_E="$SANDBOX/appdir-e"
make_appdir "$APPDIR_E"
CASE_E_BIN="$SANDBOX/case-e-bin"
mkdir -p "$CASE_E_BIN"
for f in "$BASE_BIN"/*; do ln -s "$f" "$CASE_E_BIN/$(basename "$f")"; done
cat > "$CASE_E_BIN/ldd" << 'EOF'
#!/usr/bin/env bash
echo "	libpipewire-0.3.so.0 => /usr/lib/x86_64-linux-gnu/libpipewire-0.3.so.0 (0x1234)"
EOF
chmod +x "$CASE_E_BIN/ldd"

ENV_OUT_E="$SANDBOX/env-e.txt"
run_apprun "$APPDIR_E" "$CASE_E_BIN" "$ENV_OUT_E" 7

if [ -f "$ENV_OUT_E" ]; then
    pass "case e: cleanmic still exec'd (fail-open on unexpected preflight exit 7)"
else
    fail "case e: cleanmic was never exec'd despite fail-open contract"
fi

# ─────────────────────────────────────────────────────────────────────────
# Case (f): AppRun never exports/overrides GDK_BACKEND or GTK_THEME
# ─────────────────────────────────────────────────────────────────────────
APPDIR_F="$SANDBOX/appdir-f"
make_appdir "$APPDIR_F"
CASE_F_BIN="$SANDBOX/case-f-bin"
mkdir -p "$CASE_F_BIN"
for f in "$BASE_BIN"/*; do ln -s "$f" "$CASE_F_BIN/$(basename "$f")"; done
cat > "$CASE_F_BIN/ldd" << 'EOF'
#!/usr/bin/env bash
echo "	libpipewire-0.3.so.0 => /usr/lib/x86_64-linux-gnu/libpipewire-0.3.so.0 (0x1234)"
EOF
chmod +x "$CASE_F_BIN/ldd"

ENV_OUT_F1="$SANDBOX/env-f1.txt"
set +e
env -u GDK_BACKEND -u GTK_THEME \
    PATH="$CASE_F_BIN" FAKE_CLEANMIC_ENV_OUT="$ENV_OUT_F1" FAKE_PREFLIGHT_EXIT=0 \
    "$BASH_BIN" "$APPDIR_F/AppRun" >"$SANDBOX/apprun.stdout" 2>"$SANDBOX/apprun.stderr"
set -e

if ! grep -q '^GDK_BACKEND=' "$ENV_OUT_F1" 2>/dev/null && ! grep -q '^GTK_THEME=' "$ENV_OUT_F1" 2>/dev/null; then
    pass "case f: AppRun does not export GDK_BACKEND/GTK_THEME when unset by the caller"
else
    fail "case f: unexpected GDK_BACKEND/GTK_THEME in env: $(grep -E '^(GDK_BACKEND|GTK_THEME)=' "$ENV_OUT_F1" 2>/dev/null)"
fi

ENV_OUT_F2="$SANDBOX/env-f2.txt"
run_apprun "$APPDIR_F" "$CASE_F_BIN" "$ENV_OUT_F2" 0 "GDK_BACKEND=fakebackend" "GTK_THEME=faketheme"

if grep -q '^GDK_BACKEND=fakebackend$' "$ENV_OUT_F2" 2>/dev/null && grep -q '^GTK_THEME=faketheme$' "$ENV_OUT_F2" 2>/dev/null; then
    pass "case f: AppRun passes through a caller-set GDK_BACKEND/GTK_THEME unchanged"
else
    fail "case f: expected caller-set GDK_BACKEND/GTK_THEME to survive unchanged, got: $(grep -E '^(GDK_BACKEND|GTK_THEME)=' "$ENV_OUT_F2" 2>/dev/null)"
fi

# ─────────────────────────────────────────────────────────────────────────
# Case (g): bundled GTK paths present -- guarded exports fire with the
# expected values (D-01/15.4-03); GDK_BACKEND/GTK_THEME still untouched.
# ─────────────────────────────────────────────────────────────────────────
APPDIR_G="$SANDBOX/appdir-g"
make_appdir "$APPDIR_G"
CASE_G_BIN="$SANDBOX/case-g-bin"
mkdir -p "$CASE_G_BIN"
for f in "$BASE_BIN"/*; do ln -s "$f" "$CASE_G_BIN/$(basename "$f")"; done
cat > "$CASE_G_BIN/ldd" << 'EOF'
#!/usr/bin/env bash
echo "	libpipewire-0.3.so.0 => /usr/lib/x86_64-linux-gnu/libpipewire-0.3.so.0 (0x1234)"
EOF
chmod +x "$CASE_G_BIN/ldd"

mkdir -p "$APPDIR_G/usr/share/glib-2.0/schemas"
mkdir -p "$APPDIR_G/usr/lib/girepository-1.0"
mkdir -p "$APPDIR_G/usr/lib/gdk-pixbuf-2.0/2.10.0"
: > "$APPDIR_G/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"

ENV_OUT_G="$SANDBOX/env-g.txt"
run_apprun "$APPDIR_G" "$CASE_G_BIN" "$ENV_OUT_G" 0

if grep -q "^GSETTINGS_SCHEMA_DIR=$APPDIR_G/usr/share/glib-2.0/schemas\$" "$ENV_OUT_G" 2>/dev/null; then
    pass "case g: GSETTINGS_SCHEMA_DIR exported when the bundled schema dir exists"
else
    fail "case g: expected GSETTINGS_SCHEMA_DIR, got: $(grep GSETTINGS_SCHEMA_DIR "$ENV_OUT_G" 2>/dev/null)"
fi
if grep -q "^GI_TYPELIB_PATH=$APPDIR_G/usr/lib/girepository-1.0\$" "$ENV_OUT_G" 2>/dev/null; then
    pass "case g: GI_TYPELIB_PATH exported when the bundled typelib dir exists"
else
    fail "case g: expected GI_TYPELIB_PATH, got: $(grep GI_TYPELIB_PATH "$ENV_OUT_G" 2>/dev/null)"
fi
if grep -q "^GDK_PIXBUF_MODULE_FILE=$APPDIR_G/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache\$" "$ENV_OUT_G" 2>/dev/null; then
    pass "case g: GDK_PIXBUF_MODULE_FILE exported when the bundled loaders.cache exists"
else
    fail "case g: expected GDK_PIXBUF_MODULE_FILE, got: $(grep GDK_PIXBUF_MODULE_FILE "$ENV_OUT_G" 2>/dev/null)"
fi
if ! grep -qE '^(GDK_BACKEND|GTK_THEME)=' "$ENV_OUT_G" 2>/dev/null; then
    pass "case g: GDK_BACKEND/GTK_THEME still not forced when bundled GTK paths exist"
else
    fail "case g: unexpected GDK_BACKEND/GTK_THEME in env: $(grep -E '^(GDK_BACKEND|GTK_THEME)=' "$ENV_OUT_G" 2>/dev/null)"
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
