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
# Summary
# ─────────────────────────────────────────────────────────────────────────
if [ "$FAIL_COUNT" -eq 0 ]; then
    printf '\nAll cases PASSED (or SKIPPED).\n'
    exit 0
else
    printf '\n%d case(s) FAILED.\n' "$FAIL_COUNT"
    exit 1
fi
