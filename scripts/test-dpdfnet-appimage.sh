#!/usr/bin/env bash
#
# test-dpdfnet-appimage.sh -- regression test + package-report tool for the
# DPDFNet packaging surface added to scripts/build-appimage.sh (Phase 15.1,
# Plan 07 -- D-01/D-02/D-03/D-04).
#
# Two independent modes:
#
#   --offline-contract
#       Self-contained, sandboxed, 100% offline (no network, no real
#       cargo/AppImage build). Proves the pure variant-selector functions
#       (dpdfnet_variant_features / dpdfnet_output_suffix /
#       dpdfnet_models_for_variant) and the real bundling function
#       (dpdfnet_bundle_assets) sourced from scripts/build-appimage.sh
#       (BUILD_APPIMAGE_SOURCE_ONLY=1) behave correctly for all four build
#       selectors, fail closed on missing assets, never cross-contaminate
#       variants, and that THIRD-PARTY-LICENSES.md carries the required
#       notices/pins. Mirrors scripts/test-appimage-preflight.sh's
#       fake-tool/sandbox style.
#
#   --report <15.1-PACKAGE-DELTA.json>
#       Real mode: reads a package_delta evidence record and, for each of
#       the four named builds (baseline, dpdfnet2_only, dpdfnet8_only,
#       both_variants), locates the actual built AppImage in build/,
#       verifies its real SHA-256 matches the record, extracts it
#       (--appimage-extract, no FUSE required), and confirms the extracted
#       AppDir contains exactly the expected runtime/model files (and NOT
#       the sibling variant's model), the license notice, and that every
#       bundled model/runtime re-hashes to the same pins
#       scripts/fetch-vendors.sh already enforces offline.
#
# Not wired into the Makefile `test` target on purpose -- CleanMic's project
# bar is `cargo build` + `cargo test`; this is a standalone shell check meant
# to be run directly, matching the existing scripts/test-*.sh convention.
#
# Exits 0 if every case PASSes, non-zero (with a FAIL summary) otherwise.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILD_SCRIPT="$SCRIPT_DIR/build-appimage.sh"
FETCH_SCRIPT="$SCRIPT_DIR/fetch-vendors.sh"
THIRD_PARTY_NOTICE="$PROJECT_ROOT/THIRD-PARTY-LICENSES.md"

# The exact pins scripts/fetch-vendors.sh enforces -- read live from that
# file rather than duplicated as constants here, so this test can never
# silently drift from the real pinned values.
DPDFNET2_MODEL_SHA256="$(grep -oP '^DPDFNET2_MODEL_SHA256="\K[0-9a-f]+' "$FETCH_SCRIPT")"
DPDFNET8_MODEL_SHA256="$(grep -oP '^DPDFNET8_MODEL_SHA256="\K[0-9a-f]+' "$FETCH_SCRIPT")"
ORT_RUNTIME_SHA256="$(grep -oP '^ORT_RUNTIME_SHA256="\K[0-9a-f]+' "$FETCH_SCRIPT")"

FAIL_COUNT=0
pass() { printf 'PASS: %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1"; FAIL_COUNT=$((FAIL_COUNT + 1)); }

# ═══════════════════════════════════════════════════════════════════════════
# --offline-contract
# ═══════════════════════════════════════════════════════════════════════════
run_offline_contract() {
    SANDBOX="$(mktemp -d)"
    trap 'rm -rf "$SANDBOX"' EXIT

    # Source only the pure/bundling functions from build-appimage.sh, without
    # running a real cargo build or AppImage packaging pass.
    # shellcheck source=/dev/null
    BUILD_APPIMAGE_SOURCE_ONLY=1 DPDFNET_VARIANTS=none source "$BUILD_SCRIPT"

    # ── Case 1: dpdfnet_variant_features -- correct feature set per variant ──
    [ "$(dpdfnet_variant_features none)" = "gui,pipewire,tray,rnnoise,deepfilter,updater" ] \
        && pass "case1: none -> base features only (no dpdfnet, no dpdfnet-experimental)" \
        || fail "case1: none features wrong: $(dpdfnet_variant_features none)"

    for variant in dpdfnet2 dpdfnet8 both; do
        features="$(dpdfnet_variant_features "$variant")"
        if grep -q ',dpdfnet$' <<< "$features" && ! grep -q 'dpdfnet-experimental' <<< "$features"; then
            pass "case1: $variant -> base features + dpdfnet (never dpdfnet-experimental)"
        else
            fail "case1: $variant features wrong: $features"
        fi
    done

    set +e
    dpdfnet_variant_features bogus >/dev/null 2>&1
    BOGUS_FEATURES_EXIT=$?
    set -e
    [ "$BOGUS_FEATURES_EXIT" -ne 0 ] \
        && pass "case1: unknown variant rejected by dpdfnet_variant_features" \
        || fail "case1: unknown variant should have failed"

    # ── Case 2: dpdfnet_output_suffix -- distinct per variant, none preserves default ──
    [ -z "$(dpdfnet_output_suffix none)" ] \
        && pass "case2: none -> empty suffix (preserves default CleanMic-x86_64.AppImage name)" \
        || fail "case2: none suffix should be empty, got: $(dpdfnet_output_suffix none)"
    [ "$(dpdfnet_output_suffix dpdfnet2)" = "-dpdfnet2" ] \
        && pass "case2: dpdfnet2 -> -dpdfnet2 suffix" \
        || fail "case2: dpdfnet2 suffix wrong: $(dpdfnet_output_suffix dpdfnet2)"
    [ "$(dpdfnet_output_suffix dpdfnet8)" = "-dpdfnet8" ] \
        && pass "case2: dpdfnet8 -> -dpdfnet8 suffix" \
        || fail "case2: dpdfnet8 suffix wrong: $(dpdfnet_output_suffix dpdfnet8)"
    [ "$(dpdfnet_output_suffix both)" = "-dpdfnet-both" ] \
        && pass "case2: both -> -dpdfnet-both suffix" \
        || fail "case2: both suffix wrong: $(dpdfnet_output_suffix both)"

    # ── Case 3: dpdfnet_models_for_variant -- correct, non-cross-contaminating sets ──
    [ -z "$(dpdfnet_models_for_variant none)" ] \
        && pass "case3: none -> no models" \
        || fail "case3: none should list no models"

    M2="$(dpdfnet_models_for_variant dpdfnet2)"
    if grep -qF 'dpdfnet2_48khz_hr.onnx' <<< "$M2" && ! grep -qF 'dpdfnet8_48khz_hr.onnx' <<< "$M2"; then
        pass "case3: dpdfnet2 -> only the dpdfnet2 model (never dpdfnet8)"
    else
        fail "case3: dpdfnet2 model set wrong: $M2"
    fi

    M8="$(dpdfnet_models_for_variant dpdfnet8)"
    if grep -qF 'dpdfnet8_48khz_hr.onnx' <<< "$M8" && ! grep -qF 'dpdfnet2_48khz_hr.onnx' <<< "$M8"; then
        pass "case3: dpdfnet8 -> only the dpdfnet8 model (never dpdfnet2)"
    else
        fail "case3: dpdfnet8 model set wrong: $M8"
    fi

    MBOTH="$(dpdfnet_models_for_variant both)"
    if grep -qF 'dpdfnet2_48khz_hr.onnx' <<< "$MBOTH" && grep -qF 'dpdfnet8_48khz_hr.onnx' <<< "$MBOTH"; then
        pass "case3: both -> both models"
    else
        fail "case3: both model set wrong: $MBOTH"
    fi

    # ── Fake reference tree (small placeholder bytes, NOT the real multi-MB assets) ──
    FAKE_REF="$SANDBOX/fake-ref"
    mkdir -p "$FAKE_REF/lib" "$FAKE_REF/models"
    printf 'fake-onnxruntime-bytes' > "$FAKE_REF/lib/libonnxruntime.so"
    printf 'fake-dpdfnet2-bytes' > "$FAKE_REF/models/dpdfnet2_48khz_hr.onnx"
    printf 'fake-dpdfnet8-bytes' > "$FAKE_REF/models/dpdfnet8_48khz_hr.onnx"

    # ── Case 4: dpdfnet_bundle_assets(dpdfnet2) -- only dpdfnet2 model + runtime ──
    APPDIR4="$SANDBOX/appdir4"
    mkdir -p "$APPDIR4"
    dpdfnet_bundle_assets dpdfnet2 "$FAKE_REF" "$APPDIR4"
    if [ -f "$APPDIR4/usr/lib/libonnxruntime.so" ] \
        && [ -f "$APPDIR4/usr/share/cleanmic/models/dpdfnet2_48khz_hr.onnx" ] \
        && [ ! -f "$APPDIR4/usr/share/cleanmic/models/dpdfnet8_48khz_hr.onnx" ]; then
        pass "case4: dpdfnet2 bundle -- runtime + dpdfnet2 model only, no dpdfnet8 model"
    else
        fail "case4: dpdfnet2 bundle produced wrong file set"
    fi

    # ── Case 5: dpdfnet_bundle_assets(dpdfnet8) -- only dpdfnet8 model + runtime ──
    APPDIR5="$SANDBOX/appdir5"
    mkdir -p "$APPDIR5"
    dpdfnet_bundle_assets dpdfnet8 "$FAKE_REF" "$APPDIR5"
    if [ -f "$APPDIR5/usr/lib/libonnxruntime.so" ] \
        && [ -f "$APPDIR5/usr/share/cleanmic/models/dpdfnet8_48khz_hr.onnx" ] \
        && [ ! -f "$APPDIR5/usr/share/cleanmic/models/dpdfnet2_48khz_hr.onnx" ]; then
        pass "case5: dpdfnet8 bundle -- runtime + dpdfnet8 model only, no dpdfnet2 model"
    else
        fail "case5: dpdfnet8 bundle produced wrong file set"
    fi

    # ── Case 6: dpdfnet_bundle_assets(both) -- both models + one shared runtime ──
    APPDIR6="$SANDBOX/appdir6"
    mkdir -p "$APPDIR6"
    dpdfnet_bundle_assets both "$FAKE_REF" "$APPDIR6"
    if [ -f "$APPDIR6/usr/lib/libonnxruntime.so" ] \
        && [ -f "$APPDIR6/usr/share/cleanmic/models/dpdfnet2_48khz_hr.onnx" ] \
        && [ -f "$APPDIR6/usr/share/cleanmic/models/dpdfnet8_48khz_hr.onnx" ]; then
        pass "case6: both bundle -- both models + one shared runtime"
    else
        fail "case6: both bundle produced wrong file set"
    fi

    # ── Case 7: dpdfnet_bundle_assets(none) -- nothing bundled ──
    APPDIR7="$SANDBOX/appdir7"
    mkdir -p "$APPDIR7/usr/lib"
    dpdfnet_bundle_assets none "$FAKE_REF" "$APPDIR7"
    if [ ! -f "$APPDIR7/usr/lib/libonnxruntime.so" ] && [ ! -d "$APPDIR7/usr/share/cleanmic/models" ]; then
        pass "case7: none bundle -- nothing bundled (baseline stays clean)"
    else
        fail "case7: none bundle should not have touched the AppDir"
    fi

    # ── Case 8 (fail-closed): missing runtime aborts before any model is copied ──
    BROKEN_REF_NO_RUNTIME="$SANDBOX/broken-ref-no-runtime"
    mkdir -p "$BROKEN_REF_NO_RUNTIME/models"
    printf 'fake-dpdfnet2-bytes' > "$BROKEN_REF_NO_RUNTIME/models/dpdfnet2_48khz_hr.onnx"
    APPDIR8="$SANDBOX/appdir8"
    mkdir -p "$APPDIR8"
    set +e
    CASE8_OUT="$(dpdfnet_bundle_assets dpdfnet2 "$BROKEN_REF_NO_RUNTIME" "$APPDIR8" 2>&1)"
    CASE8_EXIT=$?
    set -e
    if [ "$CASE8_EXIT" -ne 0 ] && [ ! -f "$APPDIR8/usr/lib/libonnxruntime.so" ] && [ ! -d "$APPDIR8/usr/share/cleanmic/models" ]; then
        pass "case8: missing runtime fails closed, no partial bundle left behind"
    else
        fail "case8: expected nonzero exit + no partial state, got exit=$CASE8_EXIT out=$CASE8_OUT"
    fi

    # ── Case 9 (fail-closed): missing model aborts, sibling model never copied ──
    BROKEN_REF_NO_MODEL="$SANDBOX/broken-ref-no-model"
    mkdir -p "$BROKEN_REF_NO_MODEL/lib" "$BROKEN_REF_NO_MODEL/models"
    printf 'fake-onnxruntime-bytes' > "$BROKEN_REF_NO_MODEL/lib/libonnxruntime.so"
    printf 'fake-dpdfnet2-bytes' > "$BROKEN_REF_NO_MODEL/models/dpdfnet2_48khz_hr.onnx"
    # dpdfnet8 model deliberately absent.
    APPDIR9="$SANDBOX/appdir9"
    mkdir -p "$APPDIR9"
    set +e
    CASE9_OUT="$(dpdfnet_bundle_assets both "$BROKEN_REF_NO_MODEL" "$APPDIR9" 2>&1)"
    CASE9_EXIT=$?
    set -e
    if [ "$CASE9_EXIT" -ne 0 ] && [ ! -f "$APPDIR9/usr/share/cleanmic/models/dpdfnet8_48khz_hr.onnx" ]; then
        pass "case9: missing dpdfnet8 model fails closed for 'both' variant"
    else
        fail "case9: expected nonzero exit + no dpdfnet8 model, got exit=$CASE9_EXIT out=$CASE9_OUT"
    fi

    # ── Case 10: unknown DPDFNET_VARIANTS value rejected at source time ──
    set +e
    CASE10_OUT="$(DPDFNET_VARIANTS=bogus BUILD_APPIMAGE_SOURCE_ONLY=1 bash -c "source '$BUILD_SCRIPT'" 2>&1)"
    CASE10_EXIT=$?
    set -e
    if [ "$CASE10_EXIT" -ne 0 ] && grep -qi 'DPDFNET_VARIANTS must be one of' <<< "$CASE10_OUT"; then
        pass "case10: unknown DPDFNET_VARIANTS value rejected"
    else
        fail "case10: expected rejection, got exit=$CASE10_EXIT out=$CASE10_OUT"
    fi

    # ── Case 11: no network download / no system-runtime path for the ONNX Runtime ──
    if grep -q 'DPDFNET_REF_DIR' "$BUILD_SCRIPT" \
        && ! grep -Eq '(curl|wget)[^\n]*onnxruntime' "$BUILD_SCRIPT"; then
        pass "case11: ONNX Runtime is bundled only from the pinned vendor reference tree (no download, no system path)"
    else
        fail "case11: build-appimage.sh should source the runtime only from DPDFNET_REF_DIR, never curl/wget it"
    fi

    # ── Case 12: THIRD-PARTY-LICENSES.md carries the required notices/pins ──
    if [ ! -f "$THIRD_PARTY_NOTICE" ]; then
        fail "case12: THIRD-PARTY-LICENSES.md not found"
    else
        NOTICE_TEXT="$(cat "$THIRD_PARTY_NOTICE")"
        if grep -qF "$DPDFNET2_MODEL_SHA256" <<< "$NOTICE_TEXT" \
            && grep -qF "$DPDFNET8_MODEL_SHA256" <<< "$NOTICE_TEXT" \
            && grep -qF "$ORT_RUNTIME_SHA256" <<< "$NOTICE_TEXT" \
            && grep -qi 'ceva-ip/DPDFNet' <<< "$NOTICE_TEXT" \
            && grep -qi 'onnxruntime' <<< "$NOTICE_TEXT"; then
            pass "case12: THIRD-PARTY-LICENSES.md carries the DPDFNet2/8 + ONNX Runtime pins and provenance"
        else
            fail "case12: THIRD-PARTY-LICENSES.md is missing one or more required pins/provenance markers"
        fi
    fi

    printf '\n'
    if [ "$FAIL_COUNT" -eq 0 ]; then
        printf 'All --offline-contract cases PASSED.\n'
        return 0
    fi
    printf '%d --offline-contract case(s) FAILED.\n' "$FAIL_COUNT"
    return 1
}

# ═══════════════════════════════════════════════════════════════════════════
# --report <package-delta.json>
# ═══════════════════════════════════════════════════════════════════════════
run_report() {
    local package_json="$1"
    if [ ! -f "$package_json" ]; then
        fail "report: package_delta file not found: $package_json"
        printf '1 report case(s) FAILED.\n'
        return 1
    fi

    # Deliberately NOT `local` -- the EXIT trap fires after this function
    # returns (at global script scope), and a `local` variable would already
    # be out of scope by then, making `rm -rf "$extract_root"` fail with
    # "unbound variable" under `set -u` (same class of bug Plan 01 already
    # fixed in fetch-vendors.sh's `stage` variable).
    extract_root="$(mktemp -d)"
    trap 'rm -rf "$extract_root"' EXIT

    # Delegate all JSON parsing to python3 -- prints one "label|artifact_path"
    # line per build so bash never hand-parses JSON.
    local build_lines
    build_lines="$(python3 - "$package_json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as fh:
    data = json.load(fh)

labels = ("baseline", "dpdfnet2_only", "dpdfnet8_only", "both_variants")
for label in labels:
    build = data["builds"][label]
    print(f"{label}|{build['sha256']}|{build['compressed_bytes']}")
PY
)"

    local -A SUFFIX_FOR_LABEL=(
        [baseline]=""
        [dpdfnet2_only]="-dpdfnet2"
        [dpdfnet8_only]="-dpdfnet8"
        [both_variants]="-dpdfnet-both"
    )
    local -A EXPECT_MODELS=(
        [baseline]=""
        [dpdfnet2_only]="dpdfnet2_48khz_hr.onnx"
        [dpdfnet8_only]="dpdfnet8_48khz_hr.onnx"
        [both_variants]="dpdfnet2_48khz_hr.onnx dpdfnet8_48khz_hr.onnx"
    )
    local -A FORBID_MODELS=(
        [baseline]="dpdfnet2_48khz_hr.onnx dpdfnet8_48khz_hr.onnx"
        [dpdfnet2_only]="dpdfnet8_48khz_hr.onnx"
        [dpdfnet8_only]="dpdfnet2_48khz_hr.onnx"
        [both_variants]=""
    )

    while IFS='|' read -r label recorded_sha256 recorded_bytes; do
        [ -n "$label" ] || continue
        local appimage_path="$PROJECT_ROOT/build/CleanMic-x86_64${SUFFIX_FOR_LABEL[$label]}.AppImage"

        if [ ! -f "$appimage_path" ]; then
            fail "report($label): AppImage not found at $appimage_path"
            continue
        fi

        local actual_sha256 actual_bytes
        actual_sha256="$(sha256sum "$appimage_path" | awk '{print $1}')"
        actual_bytes="$(stat -c %s "$appimage_path")"

        if [ "$actual_sha256" = "$recorded_sha256" ]; then
            pass "report($label): SHA-256 matches recorded evidence"
        else
            fail "report($label): SHA-256 mismatch (recorded $recorded_sha256, actual $actual_sha256)"
        fi

        if [ "$actual_bytes" = "$recorded_bytes" ]; then
            pass "report($label): compressed_bytes matches recorded evidence ($actual_bytes)"
        else
            fail "report($label): compressed_bytes mismatch (recorded $recorded_bytes, actual $actual_bytes)"
        fi

        local extract_dir="$extract_root/$label"
        mkdir -p "$extract_dir"
        chmod +x "$appimage_path"
        if ( cd "$extract_dir" && "$appimage_path" --appimage-extract >/dev/null 2>&1 ); then
            pass "report($label): AppImage extracted offline"
        else
            fail "report($label): --appimage-extract failed"
            continue
        fi
        local appdir="$extract_dir/squashfs-root"

        if [ -f "$appdir/usr/share/doc/cleanmic/THIRD-PARTY-LICENSES.md" ]; then
            pass "report($label): THIRD-PARTY-LICENSES.md notice present in extracted AppDir"
        else
            fail "report($label): THIRD-PARTY-LICENSES.md missing from extracted AppDir"
        fi

        if [ "$label" = "baseline" ]; then
            if [ ! -f "$appdir/usr/lib/libonnxruntime.so" ]; then
                pass "report($label): no ONNX Runtime bundled (true baseline)"
            else
                fail "report($label): baseline must not bundle libonnxruntime.so"
            fi
        else
            if [ -f "$appdir/usr/lib/libonnxruntime.so" ]; then
                local runtime_hash
                runtime_hash="$(sha256sum "$appdir/usr/lib/libonnxruntime.so" | awk '{print $1}')"
                if [ "$runtime_hash" = "$ORT_RUNTIME_SHA256" ]; then
                    pass "report($label): bundled ONNX Runtime re-hashes to the pinned value"
                else
                    fail "report($label): bundled ONNX Runtime hash mismatch (expected $ORT_RUNTIME_SHA256, got $runtime_hash)"
                fi
            else
                fail "report($label): expected libonnxruntime.so, not found"
            fi
        fi

        for model in ${EXPECT_MODELS[$label]}; do
            local model_path="$appdir/usr/share/cleanmic/models/$model"
            if [ -f "$model_path" ]; then
                local model_hash expected_hash
                model_hash="$(sha256sum "$model_path" | awk '{print $1}')"
                case "$model" in
                    dpdfnet2_48khz_hr.onnx) expected_hash="$DPDFNET2_MODEL_SHA256" ;;
                    dpdfnet8_48khz_hr.onnx) expected_hash="$DPDFNET8_MODEL_SHA256" ;;
                esac
                if [ "$model_hash" = "$expected_hash" ]; then
                    pass "report($label): $model present and re-hashes to the pinned value"
                else
                    fail "report($label): $model hash mismatch (expected $expected_hash, got $model_hash)"
                fi
            else
                fail "report($label): expected model $model not found"
            fi
        done

        for model in ${FORBID_MODELS[$label]}; do
            if [ ! -f "$appdir/usr/share/cleanmic/models/$model" ]; then
                pass "report($label): sibling model $model correctly absent"
            else
                fail "report($label): sibling model $model must NOT be bundled"
            fi
        done
    done <<< "$build_lines"

    printf '\n'
    if [ "$FAIL_COUNT" -eq 0 ]; then
        printf 'All --report cases PASSED.\n'
        return 0
    fi
    printf '%d --report case(s) FAILED.\n' "$FAIL_COUNT"
    return 1
}

# ── Mode dispatch ─────────────────────────────────────────────────────────
case "${1:-}" in
    --offline-contract)
        run_offline_contract
        ;;
    --report)
        [ -n "${2:-}" ] || { echo "usage: $0 --report <package-delta.json>" >&2; exit 2; }
        run_report "$2"
        ;;
    *)
        echo "usage: $0 --offline-contract | --report <package-delta.json>" >&2
        exit 2
        ;;
esac
