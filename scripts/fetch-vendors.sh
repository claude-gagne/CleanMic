#!/usr/bin/env bash
#
# fetch-vendors.sh -- Download pre-built third-party binaries needed for the
#                     AppImage build, plus the pinned DPDFNet reference tree
#                     used to generate offline golden behavior contracts.
#
# Currently fetches:
#   vendor/libdeep_filter_ladspa.so  — DeepFilterNet3 LADSPA plugin
#     Source: https://github.com/Rikorose/DeepFilterNet
#     Model weights are embedded; no separate model files needed.
#     Code is dual MIT/Apache-2.0; the embedded model weights are documented
#     as an inference (not an explicit upstream grant) in
#     THIRD-PARTY-LICENSES.md at the repo root — see that file, not this
#     comment, for the authoritative license record.
#
#   vendor/dpdfnet-reference/         — pinned HushMic/DPDFNet renderer
#     lineage, DPDFNet-2/8 models, and ONNX Runtime (Phase 15.1, Plan 01,
#     D-01/D-02). Staged and hash-verified ONLY via the explicit
#     `--dpdfnet-reference` acquisition mode below; never fetched as a side
#     effect of a plain `fetch-vendors.sh` invocation. `--verify-dpdfnet-reference`
#     re-checks an already-staged tree with NO network access at all.
#
# Usage:
#   bash scripts/fetch-vendors.sh                       # existing DeepFilterNet fetch (default, unchanged)
#   bash scripts/fetch-vendors.sh --dpdfnet-reference   # network: stage vendor/dpdfnet-reference/
#   bash scripts/fetch-vendors.sh --verify-dpdfnet-reference  # offline-only re-verification
#
# Testing: VENDOR_DIR can be overridden to point at a sandbox directory (see
#          scripts/test-fetch-vendors.sh).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
VENDOR_DIR="${VENDOR_DIR:-$PROJECT_ROOT/vendor}"

DEEPFILTER_VERSION="0.5.6"
DEEPFILTER_URL="https://github.com/Rikorose/DeepFilterNet/releases/download/v${DEEPFILTER_VERSION}/libdeep_filter_ladspa-${DEEPFILTER_VERSION}-x86_64-unknown-linux-gnu.so"
DEEPFILTER_OUT="$VENDOR_DIR/libdeep_filter_ladspa.so"
# Trust-on-first-use pin: upstream publishes no checksum for this release
# asset (confirmed via the GitHub Releases API — see THIRD-PARTY-LICENSES.md
# Provenance note). This value was established by a direct download +
# sha256sum in this session and must be re-established on any version bump.
DEEPFILTER_SHA256="2ca3205c2911d389604a826a240e745597d50252b5cab81c8248252b335e2236"

info()  { printf '\033[1;34m==> %s\033[0m\n' "$*"; }
error() { printf '\033[1;31m==> ERROR: %s\033[0m\n' "$*" >&2; exit 1; }

verify_deepfilter_checksum() {
    local actual_sha
    actual_sha=$(sha256sum "$DEEPFILTER_OUT" | awk '{print $1}')
    if [ "$actual_sha" != "$DEEPFILTER_SHA256" ]; then
        error "SHA256 mismatch for $DEEPFILTER_OUT
    expected: $DEEPFILTER_SHA256
    actual:   $actual_sha
  The file may be corrupted or tampered with. Remediation:
    rm -f \"$DEEPFILTER_OUT\" && bash \"$0\""
    fi
}

fetch_deepfilter() {
    mkdir -p "$VENDOR_DIR"

    # ── DeepFilterNet LADSPA plugin ───────────────────────────────────────
    if [ -f "$DEEPFILTER_OUT" ]; then
        info "vendor/libdeep_filter_ladspa.so already present — verifying checksum..."
        verify_deepfilter_checksum
        info "Checksum verified."
    else
        info "Downloading DeepFilterNet v${DEEPFILTER_VERSION} LADSPA plugin (~50 MB)..."
        DEEPFILTER_TMP="$DEEPFILTER_OUT.tmp"
        trap 'rm -f "$DEEPFILTER_TMP"' EXIT
        if command -v curl &>/dev/null; then
            curl -fL -o "$DEEPFILTER_TMP" "$DEEPFILTER_URL"
        elif command -v wget &>/dev/null; then
            wget -O "$DEEPFILTER_TMP" "$DEEPFILTER_URL"
        else
            error "Neither curl nor wget found. Cannot download."
        fi
        # Download completed successfully -- move into place before verifying, so
        # an interrupted/killed download (Ctrl-C, network drop, disk full,
        # OOM-kill) never leaves a partial file at the real vendor path. Mirrors
        # the uruntime temp-then-rename pattern in build-appimage.sh.
        mv "$DEEPFILTER_TMP" "$DEEPFILTER_OUT"
        trap - EXIT
        chmod 755 "$DEEPFILTER_OUT"
        info "Saved to $DEEPFILTER_OUT ($(du -h "$DEEPFILTER_OUT" | cut -f1))"
        info "Verifying checksum..."
        verify_deepfilter_checksum
        info "Checksum verified."
    fi

    info "All vendor binaries ready."
}

# ═══════════════════════════════════════════════════════════════════════════
# DPDFNet reference tree (Phase 15.1, Plan 01 — D-01/D-02)
#
# `vendor/dpdfnet-reference/` stages, in one atomically-published tree:
#   bin/enhance                    — pinned HushMic renderer (50 dB patch)
#   bin/dpdfnet-golden-probe       — deterministic golden-vector probe (new;
#                                     added by scripts/patches/dpdfnet-golden-probe.patch)
#   lib/libonnxruntime.so          — pinned ONNX Runtime 1.27.0
#   models/dpdfnet{2,8}_48khz_hr.onnx — pinned DPDFNet-2/8 models
#   source/hushmic-source-commit.txt  — pinned HushMic source-commit marker
#   MANIFEST.json                  — strict manifest: identifiers, revisions,
#                                     paths, sizes (implicit via sha256sum),
#                                     and SHA-256 values for every artifact.
#
# Only `--dpdfnet-reference` may touch the network (git clone + setup-assets.sh
# downloads). `--verify-dpdfnet-reference` is 100% offline and never invokes
# git/curl/wget.
# ═══════════════════════════════════════════════════════════════════════════

DPDFNET_REF_DIR="${DPDFNET_REF_DIR:-$VENDOR_DIR/dpdfnet-reference}"

HUSHMIC_REPO_URL="https://github.com/Fovty/hushmic.git"
HUSHMIC_COMMIT="5f7d3180d07e5636d694ca6d4e2a1d1dfe3d42b7"
HUSHMIC_SOURCE_COMMIT_FILE_SHA256="ab22118c97d680cb62ec24dfd3ccfeeec30a5d4dfba6878f9ee71ad2a10eded1"

DPDFNET_REPO_URL="https://github.com/ceva-ip/DPDFNet.git"
DPDFNET_COMMIT="fadf269abc8207743cc7ff05e8cace6154008c04"
DPDFNET_HF_MODEL_CARD_REVISION="dd6818d00f50c836fed43a6243ebe49116de5964"

DPDFNET2_MODEL_SHA256="7f0575a5cec0ba4ffd8f8bd657e06d007e4ccdd955d76faab922b9d3291dc14b"
DPDFNET8_MODEL_SHA256="7b3afbb260a08fe9af3d16e3bda992971be1e7e951d1dee7c2d235f5c43f5631"
ORT_RUNTIME_SHA256="4061866361d9a8d2872f5f419c5515ce35a830a0c5c77ce1723320ac0dbabfc7"
# The exact source-built `enhance` renderer already validated in Phase 15
# (see 15-LICENSING-EVIDENCE.md / the dpdfnet8+dpdfnet2 adapter descriptors).
RENDERER_SHA256="744459f2e227dfb9ff2fa0906fb56368feeeb6e9503c328052426ff9e587ce91"

GOLDEN_PROBE_PATCH="$SCRIPT_DIR/patches/dpdfnet-golden-probe.patch"
HUSHMIC_50DB_PATCH="$PROJECT_ROOT/.planning/spikes/001-pinned-dpdfnet-renderer/hushmic-enhance-50db.patch"
# Byte-identical to $RENDERER_SHA256; reused because a fresh `cargo build` of
# this crate is NOT bit-for-bit reproducible across checkout paths (confirmed
# empirically — absolute build paths leak into the compiled binary's debug
# info), matching Phase 15's already-recorded "dpdfnet2 renderer reuse
# confirmed with no rebuild needed" precedent (STATE.md).
RENDERER_TRUSTED_COPY="${RENDERER_TRUSTED_COPY:-$PROJECT_ROOT/.planning/model-eval/external/dpdfnet8/enhance}"

dpdfnet_info() { info "[dpdfnet-reference] $*"; }

dpdfnet_pick_python() {
    if command -v python3.11 &>/dev/null; then
        echo python3.11
        return
    fi
    if command -v python3 &>/dev/null; then
        echo python3
        return
    fi
    error "python3 (>=3.11) not found; required by HushMic's scripts/setup-assets.sh"
}

# dpdfnet_require_pinned_file <path> <expected_sha256> <label>
# Fails closed on: missing file, symlink, or a sha256 mismatch.
dpdfnet_require_pinned_file() {
    local path="$1" expected="$2" label="$3"
    if [ -L "$path" ]; then
        error "$label: refusing a symlink at $path"
    fi
    if [ ! -f "$path" ]; then
        error "$label: missing regular file at $path"
    fi
    local actual
    actual=$(sha256sum "$path" | awk '{print $1}')
    if [ "$actual" != "$expected" ]; then
        error "$label: SHA256 mismatch
    path:     $path
    expected: $expected
    actual:   $actual"
    fi
}

# dpdfnet_verify_offline [root]
# 100% offline (no git/curl/wget). Re-verifies an already-staged tree against
# the SAME fixed pins used at acquisition time; never trusts anything read
# from the manifest alone (every pin below is also cross-checked as literally
# present in MANIFEST.json, so a manifest that silently dropped or renamed a
# field cannot pass).
dpdfnet_verify_offline() {
    local root="${1:-$DPDFNET_REF_DIR}"

    if [ -L "$root" ]; then
        error "vendor/dpdfnet-reference: refusing a symlinked root"
    fi
    if [ ! -d "$root" ]; then
        error "vendor/dpdfnet-reference: not staged at $root (run: bash $0 --dpdfnet-reference)"
    fi

    local manifest="$root/MANIFEST.json"
    if [ -L "$manifest" ]; then
        error "MANIFEST.json: refusing a symlink"
    fi
    if [ ! -f "$manifest" ]; then
        error "vendor/dpdfnet-reference/MANIFEST.json missing"
    fi

    dpdfnet_require_pinned_file "$root/models/dpdfnet2_48khz_hr.onnx" "$DPDFNET2_MODEL_SHA256" "dpdfnet2 model"
    dpdfnet_require_pinned_file "$root/models/dpdfnet8_48khz_hr.onnx" "$DPDFNET8_MODEL_SHA256" "dpdfnet8 model"
    dpdfnet_require_pinned_file "$root/lib/libonnxruntime.so" "$ORT_RUNTIME_SHA256" "ONNX Runtime library"
    dpdfnet_require_pinned_file "$root/bin/enhance" "$RENDERER_SHA256" "pinned renderer"
    dpdfnet_require_pinned_file "$root/source/hushmic-source-commit.txt" "$HUSHMIC_SOURCE_COMMIT_FILE_SHA256" "HushMic source-commit file"

    # The golden probe is a brand-new artifact type with no prior historical
    # pin (this acquisition establishes it for the first time); verify it is
    # present, non-symlink, executable, and that its on-disk bytes match the
    # hash the manifest itself claims for it (tamper/staleness detection),
    # rather than a fixed constant.
    local probe="$root/bin/dpdfnet-golden-probe"
    if [ -L "$probe" ]; then
        error "dpdfnet-golden-probe: refusing a symlink"
    fi
    if [ ! -f "$probe" ]; then
        error "dpdfnet-golden-probe: missing regular file"
    fi
    if [ ! -x "$probe" ]; then
        error "dpdfnet-golden-probe: not executable"
    fi
    local probe_sha
    probe_sha=$(sha256sum "$probe" | awk '{print $1}')
    if ! grep -qF "\"sha256\": \"$probe_sha\"" "$manifest"; then
        error "dpdfnet-golden-probe: on-disk hash $probe_sha is not recorded in MANIFEST.json (tampered or stale manifest)"
    fi

    local pin
    for pin in "$DPDFNET2_MODEL_SHA256" "$DPDFNET8_MODEL_SHA256" "$ORT_RUNTIME_SHA256" "$RENDERER_SHA256" "$HUSHMIC_SOURCE_COMMIT_FILE_SHA256"; do
        grep -qF "$pin" "$manifest" || error "MANIFEST.json missing expected pin $pin"
    done
    local commit
    for commit in "$HUSHMIC_COMMIT" "$DPDFNET_COMMIT"; do
        grep -qF "$commit" "$manifest" || error "MANIFEST.json missing expected source commit $commit"
    done

    info "vendor/dpdfnet-reference: offline verification passed ($root)."
}

# dpdfnet_acquire: the ONLY path allowed network access for the DPDFNet
# reference tree. Stages everything in a sibling temporary directory and
# atomically renames it into place ONLY after every source revision and
# artifact hash passes -- an interrupted, mismatched, or partially built
# source/model/runtime/probe leaves NO final tree (and no temp residue,
# via the EXIT trap) and exits nonzero.
dpdfnet_acquire() {
    mkdir -p "$VENDOR_DIR"

    if [ -d "$DPDFNET_REF_DIR" ]; then
        info "vendor/dpdfnet-reference already staged; re-verifying offline before reuse..."
        if dpdfnet_verify_offline "$DPDFNET_REF_DIR"; then
            info "Already staged and verified; nothing to do. Remove $DPDFNET_REF_DIR to force a clean re-acquisition."
            return 0
        fi
    fi

    command -v git &>/dev/null || error "git is required for --dpdfnet-reference"
    command -v cargo &>/dev/null || error "cargo is required for --dpdfnet-reference"
    [ -f "$GOLDEN_PROBE_PATCH" ] || error "missing $GOLDEN_PROBE_PATCH"
    [ -f "$HUSHMIC_50DB_PATCH" ] || error "missing $HUSHMIC_50DB_PATCH (Phase 15 spike patch)"

    local python_bin
    python_bin=$(dpdfnet_pick_python)

    # Deliberately NOT `local`: the EXIT trap below still needs to read this
    # after `set -e` unwinds out of this function's call frame (a `local`
    # variable goes out of scope the instant the function returns/aborts,
    # which would make the trap itself fail with "unbound variable" under
    # `set -u` and skip cleanup entirely).
    stage=$(mktemp -d "${VENDOR_DIR}/.dpdfnet-reference.stage.XXXXXX")
    # Any failure below (set -e) leaves ONLY this temp tree, cleaned up here --
    # never a partial vendor/dpdfnet-reference.
    trap 'rm -rf "$stage"' EXIT

    dpdfnet_info "cloning HushMic @ $HUSHMIC_COMMIT ..."
    git clone --quiet "$HUSHMIC_REPO_URL" "$stage/hushmic"
    (cd "$stage/hushmic" && git checkout --quiet "$HUSHMIC_COMMIT")
    local hushmic_head
    hushmic_head=$(cd "$stage/hushmic" && git rev-parse HEAD)
    [ "$hushmic_head" = "$HUSHMIC_COMMIT" ] || error "HushMic checkout resolved to $hushmic_head, expected $HUSHMIC_COMMIT"

    dpdfnet_info "cloning DPDFNet @ $DPDFNET_COMMIT (source/licensing provenance) ..."
    git clone --quiet "$DPDFNET_REPO_URL" "$stage/dpdfnet"
    (cd "$stage/dpdfnet" && git checkout --quiet "$DPDFNET_COMMIT")
    local dpdfnet_head
    dpdfnet_head=$(cd "$stage/dpdfnet" && git rev-parse HEAD)
    [ "$dpdfnet_head" = "$DPDFNET_COMMIT" ] || error "DPDFNet checkout resolved to $dpdfnet_head, expected $DPDFNET_COMMIT"

    printf '%s' "$HUSHMIC_COMMIT" >"$stage/hushmic-source-commit.txt"
    dpdfnet_require_pinned_file "$stage/hushmic-source-commit.txt" "$HUSHMIC_SOURCE_COMMIT_FILE_SHA256" "generated HushMic source-commit file"

    dpdfnet_info "applying the 50 dB attenuation patch + golden-probe patch ..."
    (cd "$stage/hushmic" && git apply "$HUSHMIC_50DB_PATCH")
    (cd "$stage/hushmic" && git apply "$GOLDEN_PROBE_PATCH")

    dpdfnet_info "fetching the pinned models + ONNX Runtime via setup-assets.sh ..."
    (cd "$stage/hushmic" && PYTHON="$python_bin" bash scripts/setup-assets.sh)
    dpdfnet_require_pinned_file "$stage/hushmic/assets/models/dpdfnet2_48khz_hr.onnx" "$DPDFNET2_MODEL_SHA256" "dpdfnet2 model (freshly fetched)"
    dpdfnet_require_pinned_file "$stage/hushmic/assets/models/dpdfnet8_48khz_hr.onnx" "$DPDFNET8_MODEL_SHA256" "dpdfnet8 model (freshly fetched)"
    dpdfnet_require_pinned_file "$stage/hushmic/assets/lib/libonnxruntime.so.1.27.0" "$ORT_RUNTIME_SHA256" "ONNX Runtime (freshly fetched)"

    dpdfnet_info "building the renderer + golden probe (Rust 1.93.1, locked Cargo graph) ..."
    (cd "$stage/hushmic" && cargo build --release --locked -p dpdfnet-ladspa --example enhance --example golden_probe)
    local built_probe="$stage/hushmic/target/release/examples/golden_probe"
    local built_enhance="$stage/hushmic/target/release/examples/enhance"
    [ -x "$built_probe" ] || error "golden_probe example did not build"
    [ -x "$built_enhance" ] || error "enhance example did not build"

    local renderer_src built_enhance_sha
    built_enhance_sha=$(sha256sum "$built_enhance" | awk '{print $1}')
    if [ "$built_enhance_sha" = "$RENDERER_SHA256" ]; then
        renderer_src="$built_enhance"
        dpdfnet_info "freshly built renderer matches the pinned hash exactly."
    else
        dpdfnet_info "freshly built renderer does not bit-for-bit match the pinned hash (expected: this crate's build embeds the checkout path); reusing the already hash-verified Phase 15 renderer copy."
        renderer_src="$RENDERER_TRUSTED_COPY"
        [ -f "$renderer_src" ] || error "no trusted renderer copy at $renderer_src and the fresh build did not match the pin -- cannot proceed"
    fi
    dpdfnet_require_pinned_file "$renderer_src" "$RENDERER_SHA256" "renderer (final selection)"

    mkdir -p "$stage/out/bin" "$stage/out/lib" "$stage/out/models" "$stage/out/source"
    install -m 755 "$renderer_src" "$stage/out/bin/enhance"
    install -m 755 "$built_probe" "$stage/out/bin/dpdfnet-golden-probe"
    install -m 644 "$stage/hushmic/assets/lib/libonnxruntime.so.1.27.0" "$stage/out/lib/libonnxruntime.so"
    install -m 644 "$stage/hushmic/assets/models/dpdfnet2_48khz_hr.onnx" "$stage/out/models/dpdfnet2_48khz_hr.onnx"
    install -m 644 "$stage/hushmic/assets/models/dpdfnet8_48khz_hr.onnx" "$stage/out/models/dpdfnet8_48khz_hr.onnx"
    install -m 644 "$stage/hushmic-source-commit.txt" "$stage/out/source/hushmic-source-commit.txt"

    local probe_sha now
    probe_sha=$(sha256sum "$stage/out/bin/dpdfnet-golden-probe" | awk '{print $1}')
    now=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

    cat >"$stage/out/MANIFEST.json" <<MANIFEST_EOF
{
  "schema_version": 1,
  "generated_at": "$now",
  "sources": {
    "hushmic": {
      "repo": "$HUSHMIC_REPO_URL",
      "commit": "$HUSHMIC_COMMIT",
      "commit_file": {"path": "source/hushmic-source-commit.txt", "sha256": "$HUSHMIC_SOURCE_COMMIT_FILE_SHA256"},
      "license_ref": "MIT OR Apache-2.0 reported by pinned source"
    },
    "dpdfnet": {
      "repo": "$DPDFNET_REPO_URL",
      "commit": "$DPDFNET_COMMIT",
      "license_ref": "Apache-2.0 (see .planning/phases/15-full-model-evaluation-conditional-dpdfnet-integration/15-LICENSING-EVIDENCE.md)",
      "hf_model_card_revision": "$DPDFNET_HF_MODEL_CARD_REVISION"
    }
  },
  "artifacts": {
    "renderer": {"path": "bin/enhance", "sha256": "$RENDERER_SHA256", "version": "source-build-2026-07-18-rust-1.93.1", "provenance": "reused byte-identical Phase 15 validated build (fresh rebuilds of this crate embed the checkout path and are not bit-for-bit reproducible)"},
    "golden_probe": {"path": "bin/dpdfnet-golden-probe", "sha256": "$probe_sha", "version": "dpdfnet-golden-probe-rust-1.93.1", "provenance": "built this acquisition from HushMic $HUSHMIC_COMMIT + scripts/patches/dpdfnet-golden-probe.patch, Rust 1.93.1, cargo build --locked"},
    "runtime": {"path": "lib/libonnxruntime.so", "sha256": "$ORT_RUNTIME_SHA256", "version": "ONNX Runtime 1.27.0"},
    "dpdfnet2_model": {"path": "models/dpdfnet2_48khz_hr.onnx", "sha256": "$DPDFNET2_MODEL_SHA256", "version": "DPDFNet-2 v0.5.1"},
    "dpdfnet8_model": {"path": "models/dpdfnet8_48khz_hr.onnx", "sha256": "$DPDFNET8_MODEL_SHA256", "version": "DPDFNet-8 v0.5.1"}
  }
}
MANIFEST_EOF

    # Self-verify the freshly staged tree BEFORE it is ever renamed into place.
    dpdfnet_verify_offline "$stage/out"

    mv "$stage/out" "$DPDFNET_REF_DIR"
    trap - EXIT
    rm -rf "$stage"
    info "vendor/dpdfnet-reference staged and verified at $DPDFNET_REF_DIR"
}

# ── Mode dispatch ─────────────────────────────────────────────────────────
case "${1:-}" in
    --dpdfnet-reference)
        dpdfnet_acquire
        ;;
    --verify-dpdfnet-reference)
        dpdfnet_verify_offline
        ;;
    "")
        fetch_deepfilter
        ;;
    *)
        error "unrecognized argument: $1 (expected --dpdfnet-reference, --verify-dpdfnet-reference, or no argument)"
        ;;
esac
