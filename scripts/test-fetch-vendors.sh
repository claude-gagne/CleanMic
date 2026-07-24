#!/usr/bin/env bash
#
# test-fetch-vendors.sh -- regression test for scripts/fetch-vendors.sh's
# SHA256 integrity gate (D-03).
#
# Self-contained: uses VENDOR_DIR overrides to point fetch-vendors.sh at a
# mktemp sandbox, so it never touches the real vendor/ directory.
#
# Not wired into the Makefile `test` target on purpose -- CleanMic's project
# bar is `cargo build` + `cargo test`; this is a standalone shell check meant
# to be run directly:
#
#   bash scripts/test-fetch-vendors.sh
#
# Exits 0 if every non-skipped case PASSes, non-zero (with a FAIL summary)
# otherwise.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
FETCH_SCRIPT="$SCRIPT_DIR/fetch-vendors.sh"

SANDBOX="$(mktemp -d)"
trap 'rm -rf "$SANDBOX"' EXIT

FAIL_COUNT=0

pass() { printf 'PASS: %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1"; FAIL_COUNT=$((FAIL_COUNT + 1)); }
skip() { printf 'SKIP: %s\n' "$1"; }

# ─────────────────────────────────────────────────────────────────────────
# Case 1 (required, D-03 gate): a tampered .so must make fetch-vendors.sh
# exit non-zero with a SHA256-mismatch message.
# ─────────────────────────────────────────────────────────────────────────
CASE1_VENDOR="$SANDBOX/case1-vendor"
mkdir -p "$CASE1_VENDOR"
# Deliberately wrong bytes -- will never hash to the pinned DEEPFILTER_SHA256.
printf 'tampered bytes, not the real DeepFilterNet plugin\n' > "$CASE1_VENDOR/libdeep_filter_ladspa.so"

set +e
CASE1_OUTPUT="$(VENDOR_DIR="$CASE1_VENDOR" bash "$FETCH_SCRIPT" 2>&1)"
CASE1_EXIT=$?
set -e

if [ "$CASE1_EXIT" -ne 0 ]; then
    pass "case1: tampered .so makes fetch-vendors.sh exit non-zero"
else
    fail "case1: expected non-zero exit for tampered .so, got 0"
fi

if printf '%s' "$CASE1_OUTPUT" | grep -qi 'SHA256 mismatch'; then
    pass "case1: output names a SHA256 mismatch"
else
    fail "case1: expected a SHA256-mismatch message, got: $CASE1_OUTPUT"
fi

# ─────────────────────────────────────────────────────────────────────────
# Case 2 (best-effort positive path): if the real vendored .so exists on
# disk, copying it into a sandbox VENDOR_DIR must verify cleanly (exit 0).
# Skipped (not failed) if the real file is absent -- we never download
# ~50 MB just to run this test.
# ─────────────────────────────────────────────────────────────────────────
REAL_SO="$PROJECT_ROOT/vendor/libdeep_filter_ladspa.so"
if [ -f "$REAL_SO" ]; then
    CASE2_VENDOR="$SANDBOX/case2-vendor"
    mkdir -p "$CASE2_VENDOR"
    cp "$REAL_SO" "$CASE2_VENDOR/libdeep_filter_ladspa.so"

    set +e
    CASE2_OUTPUT="$(VENDOR_DIR="$CASE2_VENDOR" bash "$FETCH_SCRIPT" 2>&1)"
    CASE2_EXIT=$?
    set -e

    if [ "$CASE2_EXIT" -eq 0 ]; then
        pass "case2: real vendored .so verifies cleanly (exit 0)"
    else
        fail "case2: expected exit 0 for the real vendored .so, got $CASE2_EXIT (output: $CASE2_OUTPUT)"
    fi
else
    skip "case2: vendor/libdeep_filter_ladspa.so not present locally -- not downloading ~50 MB for this test"
fi

# ─────────────────────────────────────────────────────────────────────────
# Case 3 (WR-01/WR-02 regression): a download that fails partway through
# (simulated network drop) must NOT leave a partial file at the final vendor
# path, and must NOT leave a leftover .tmp file behind either. Exercises the
# temp-then-rename + trap cleanup logic without any real network access, by
# shimming `curl` in PATH with a fake that writes partial garbage then exits
# non-zero (mirroring what a dropped connection looks like to the script).
# ─────────────────────────────────────────────────────────────────────────
CASE3_VENDOR="$SANDBOX/case3-vendor"
CASE3_BIN="$SANDBOX/case3-bin"
mkdir -p "$CASE3_VENDOR" "$CASE3_BIN"

cat > "$CASE3_BIN/curl" <<'FAKE_CURL'
#!/usr/bin/env bash
# Fake curl: simulate a connection drop partway through -- write partial
# bytes to the -o target, then exit non-zero, WITHOUT ever touching the
# final vendor path directly (the real curl only ever sees the .tmp path).
for ((i = 1; i <= $#; i++)); do
    if [ "${!i}" = "-o" ]; then
        j=$((i + 1))
        printf 'partial garbage, connection dropped\n' > "${!j}"
        break
    fi
done
exit 1
FAKE_CURL
chmod +x "$CASE3_BIN/curl"

set +e
CASE3_OUTPUT="$(VENDOR_DIR="$CASE3_VENDOR" PATH="$CASE3_BIN:$PATH" bash "$FETCH_SCRIPT" 2>&1)"
CASE3_EXIT=$?
set -e

if [ "$CASE3_EXIT" -ne 0 ]; then
    pass "case3: dropped connection makes fetch-vendors.sh exit non-zero"
else
    fail "case3: expected non-zero exit for a dropped connection, got 0"
fi

if [ ! -f "$CASE3_VENDOR/libdeep_filter_ladspa.so" ]; then
    pass "case3: no partial file left at the final vendor path"
else
    fail "case3: expected no file at final vendor path after a dropped connection, but found one"
fi

if [ ! -f "$CASE3_VENDOR/libdeep_filter_ladspa.so.tmp" ]; then
    pass "case3: no leftover .tmp file left behind"
else
    fail "case3: expected the .tmp file to be cleaned up on failure, but it is still present"
fi

# ─────────────────────────────────────────────────────────────────────────
# Case 4 (Phase 15.1 Plan 01, D-01/D-02): --verify-dpdfnet-reference must
# fail cleanly (nonzero, no crash) when nothing has been staged yet.
# ─────────────────────────────────────────────────────────────────────────
CASE4_VENDOR="$SANDBOX/case4-vendor"
mkdir -p "$CASE4_VENDOR"

set +e
CASE4_OUTPUT="$(VENDOR_DIR="$CASE4_VENDOR" bash "$FETCH_SCRIPT" --verify-dpdfnet-reference 2>&1)"
CASE4_EXIT=$?
set -e

if [ "$CASE4_EXIT" -ne 0 ]; then
    pass "case4: --verify-dpdfnet-reference fails cleanly when not staged"
else
    fail "case4: expected non-zero exit when vendor/dpdfnet-reference is absent, got 0"
fi
if printf '%s' "$CASE4_OUTPUT" | grep -qi 'not staged'; then
    pass "case4: output names the tree as not staged"
else
    fail "case4: expected a 'not staged' message, got: $CASE4_OUTPUT"
fi

# ─────────────────────────────────────────────────────────────────────────
# Setup for cases 5-8: build one real, hash-verified dpdfnet-reference tree
# in a sandbox by running the actual acquisition mode once. This DOES use
# the network (git clone + setup-assets.sh downloads + a real `cargo build`)
# -- unlike the DeepFilterNet cases above, there is no pre-built artifact to
# reuse, and the whole point of this suite is to exercise the real staging
# mechanism end-to-end at least once. Skipped (not failed) if network/cargo
# access is unavailable in this environment.
# ─────────────────────────────────────────────────────────────────────────
DPDFNET_STAGE_VENDOR="$SANDBOX/dpdfnet-stage-vendor"
mkdir -p "$DPDFNET_STAGE_VENDOR"
DPDFNET_STAGE_OK=0
if command -v git &>/dev/null && command -v cargo &>/dev/null; then
    set +e
    VENDOR_DIR="$DPDFNET_STAGE_VENDOR" bash "$FETCH_SCRIPT" --dpdfnet-reference >"$SANDBOX/dpdfnet-stage.log" 2>&1
    DPDFNET_STAGE_EXIT=$?
    set -e
    if [ "$DPDFNET_STAGE_EXIT" -eq 0 ] && [ -f "$DPDFNET_STAGE_VENDOR/dpdfnet-reference/MANIFEST.json" ]; then
        DPDFNET_STAGE_OK=1
        pass "setup: real --dpdfnet-reference acquisition succeeded in a sandbox"
    else
        skip "cases 5-9: real --dpdfnet-reference acquisition failed or network unavailable (see $SANDBOX/dpdfnet-stage.log)"
    fi
else
    skip "cases 5-9: git or cargo not available in this environment"
fi

if [ "$DPDFNET_STAGE_OK" -eq 1 ]; then
    # ─────────────────────────────────────────────────────────────────────
    # Case 5: --verify-dpdfnet-reference is 100% offline -- it must succeed
    # even when git/curl/wget are shimmed in PATH to immediately fail, proving
    # verify mode never invokes any of them.
    # ─────────────────────────────────────────────────────────────────────
    CASE5_BIN="$SANDBOX/case5-bin"
    mkdir -p "$CASE5_BIN"
    for tool in git curl wget; do
        cat >"$CASE5_BIN/$tool" <<FAKE_TOOL
#!/usr/bin/env bash
echo "FORBIDDEN: $tool invoked during offline verify" >&2
exit 99
FAKE_TOOL
        chmod +x "$CASE5_BIN/$tool"
    done

    set +e
    CASE5_OUTPUT="$(VENDOR_DIR="$DPDFNET_STAGE_VENDOR" PATH="$CASE5_BIN:$PATH" bash "$FETCH_SCRIPT" --verify-dpdfnet-reference 2>&1)"
    CASE5_EXIT=$?
    set -e

    if [ "$CASE5_EXIT" -eq 0 ]; then
        pass "case5: --verify-dpdfnet-reference succeeds with git/curl/wget shimmed to fail (proves it never calls them)"
    else
        fail "case5: expected exit 0 with git/curl/wget shimmed to fail, got $CASE5_EXIT (output: $CASE5_OUTPUT)"
    fi
    if printf '%s' "$CASE5_OUTPUT" | grep -qi 'FORBIDDEN'; then
        fail "case5: verify mode invoked a shimmed network tool -- offline contract violated"
    else
        pass "case5: no shimmed git/curl/wget was invoked"
    fi

    # ─────────────────────────────────────────────────────────────────────
    # Case 6 (D-01/D-02 integrity gate): a tampered staged model must make
    # --verify-dpdfnet-reference exit non-zero with a SHA256-mismatch message,
    # and must never leave a corrupted tree passing as valid.
    # ─────────────────────────────────────────────────────────────────────
    CASE6_VENDOR="$SANDBOX/case6-vendor"
    cp -r "$DPDFNET_STAGE_VENDOR" "$CASE6_VENDOR"
    printf 'tampered bytes, not the real DPDFNet-2 model\n' >"$CASE6_VENDOR/dpdfnet-reference/models/dpdfnet2_48khz_hr.onnx"

    set +e
    CASE6_OUTPUT="$(VENDOR_DIR="$CASE6_VENDOR" bash "$FETCH_SCRIPT" --verify-dpdfnet-reference 2>&1)"
    CASE6_EXIT=$?
    set -e

    if [ "$CASE6_EXIT" -ne 0 ]; then
        pass "case6: tampered staged model makes --verify-dpdfnet-reference exit non-zero"
    else
        fail "case6: expected non-zero exit for a tampered model, got 0"
    fi
    if printf '%s' "$CASE6_OUTPUT" | grep -qi 'SHA256 mismatch'; then
        pass "case6: output names a SHA256 mismatch"
    else
        fail "case6: expected a SHA256-mismatch message, got: $CASE6_OUTPUT"
    fi

    # ─────────────────────────────────────────────────────────────────────
    # Case 7: swapping a staged artifact for a symlink must be rejected, even
    # if the symlink target's bytes would otherwise hash-match.
    # ─────────────────────────────────────────────────────────────────────
    CASE7_VENDOR="$SANDBOX/case7-vendor"
    cp -r "$DPDFNET_STAGE_VENDOR" "$CASE7_VENDOR"
    REAL_RUNTIME="$CASE7_VENDOR/dpdfnet-reference/lib/libonnxruntime.so"
    mv "$REAL_RUNTIME" "$REAL_RUNTIME.real"
    ln -s "$REAL_RUNTIME.real" "$REAL_RUNTIME"

    set +e
    CASE7_OUTPUT="$(VENDOR_DIR="$CASE7_VENDOR" bash "$FETCH_SCRIPT" --verify-dpdfnet-reference 2>&1)"
    CASE7_EXIT=$?
    set -e

    if [ "$CASE7_EXIT" -ne 0 ]; then
        pass "case7: a symlinked staged artifact makes --verify-dpdfnet-reference exit non-zero"
    else
        fail "case7: expected non-zero exit for a symlinked artifact, got 0"
    fi
    if printf '%s' "$CASE7_OUTPUT" | grep -qi 'symlink'; then
        pass "case7: output names the symlink rejection"
    else
        fail "case7: expected a symlink-rejection message, got: $CASE7_OUTPUT"
    fi

    # ─────────────────────────────────────────────────────────────────────
    # Case 8 (WR-01/WR-02-style regression): an interrupted git clone (network
    # drop mid-acquisition) must leave NEITHER a final vendor/dpdfnet-reference
    # tree NOR any temporary staging residue.
    # ─────────────────────────────────────────────────────────────────────
    CASE8_VENDOR="$SANDBOX/case8-vendor"
    CASE8_BIN="$SANDBOX/case8-bin"
    mkdir -p "$CASE8_VENDOR" "$CASE8_BIN"
    cat >"$CASE8_BIN/git" <<'FAKE_GIT'
#!/usr/bin/env bash
if [ "$1" = "clone" ]; then
    dest="${@: -1}"
    mkdir -p "$dest"
    printf 'partial clone data, connection dropped\n' >"$dest/PARTIAL"
    exit 1
fi
exec /usr/bin/git "$@"
FAKE_GIT
    chmod +x "$CASE8_BIN/git"

    set +e
    VENDOR_DIR="$CASE8_VENDOR" PATH="$CASE8_BIN:$PATH" bash "$FETCH_SCRIPT" --dpdfnet-reference >/dev/null 2>&1
    CASE8_EXIT=$?
    set -e

    if [ "$CASE8_EXIT" -ne 0 ]; then
        pass "case8: an interrupted git clone makes --dpdfnet-reference exit non-zero"
    else
        fail "case8: expected non-zero exit for an interrupted git clone, got 0"
    fi
    if [ ! -d "$CASE8_VENDOR/dpdfnet-reference" ]; then
        pass "case8: no final vendor/dpdfnet-reference tree left behind"
    else
        fail "case8: expected no final tree after an interrupted clone, but found one"
    fi
    if ! find "$CASE8_VENDOR" -maxdepth 1 -name '.dpdfnet-reference.stage.*' | grep -q .; then
        pass "case8: no leftover staging temp directory"
    else
        fail "case8: expected the staging temp directory to be cleaned up, but it is still present"
    fi

    # ─────────────────────────────────────────────────────────────────────
    # Case 9: an interrupted (killed/failed) build must leave neither a final
    # tree nor staging residue, matching case 8's contract for the build step.
    # ─────────────────────────────────────────────────────────────────────
    CASE9_VENDOR="$SANDBOX/case9-vendor"
    CASE9_BIN="$SANDBOX/case9-bin"
    mkdir -p "$CASE9_VENDOR" "$CASE9_BIN"
    cat >"$CASE9_BIN/cargo" <<'FAKE_CARGO'
#!/usr/bin/env bash
echo "simulated build failure (killed/disk full)" >&2
exit 137
FAKE_CARGO
    chmod +x "$CASE9_BIN/cargo"

    set +e
    PATH="$CASE9_BIN:$PATH" VENDOR_DIR="$CASE9_VENDOR" bash "$FETCH_SCRIPT" --dpdfnet-reference >/dev/null 2>&1
    CASE9_EXIT=$?
    set -e

    if [ "$CASE9_EXIT" -ne 0 ]; then
        pass "case9: an interrupted build makes --dpdfnet-reference exit non-zero"
    else
        fail "case9: expected non-zero exit for an interrupted build, got 0"
    fi
    if [ ! -d "$CASE9_VENDOR/dpdfnet-reference" ]; then
        pass "case9: no final vendor/dpdfnet-reference tree left behind after a build failure"
    else
        fail "case9: expected no final tree after an interrupted build, but found one"
    fi
    if ! find "$CASE9_VENDOR" -maxdepth 1 -name '.dpdfnet-reference.stage.*' | grep -q .; then
        pass "case9: no leftover staging temp directory after a build failure"
    else
        fail "case9: expected the staging temp directory to be cleaned up, but it is still present"
    fi
fi

# ─────────────────────────────────────────────────────────────────────────
# Summary
# ─────────────────────────────────────────────────────────────────────────
if [ "$FAIL_COUNT" -eq 0 ]; then
    printf '\nAll cases PASSED (or SKIPPED).\n'
    exit 0
else
    printf '\n%d case(s) FAILED.\n' "$FAIL_COUNT"
    exit 1
fi
