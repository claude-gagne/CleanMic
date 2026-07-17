#!/usr/bin/env bash
#
# fetch-vendors.sh -- Download pre-built third-party binaries needed for the
#                     AppImage build.
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
# Usage: bash scripts/fetch-vendors.sh
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

mkdir -p "$VENDOR_DIR"

# ── DeepFilterNet LADSPA plugin ───────────────────────────────────────────────
if [ -f "$DEEPFILTER_OUT" ]; then
    info "vendor/libdeep_filter_ladspa.so already present — verifying checksum..."
    verify_deepfilter_checksum
    info "Checksum verified."
else
    info "Downloading DeepFilterNet v${DEEPFILTER_VERSION} LADSPA plugin (~50 MB)..."
    if command -v curl &>/dev/null; then
        curl -L -o "$DEEPFILTER_OUT" "$DEEPFILTER_URL"
    elif command -v wget &>/dev/null; then
        wget -O "$DEEPFILTER_OUT" "$DEEPFILTER_URL"
    else
        error "Neither curl nor wget found. Cannot download."
    fi
    chmod 755 "$DEEPFILTER_OUT"
    info "Saved to $DEEPFILTER_OUT ($(du -h "$DEEPFILTER_OUT" | cut -f1))"
    info "Verifying checksum..."
    verify_deepfilter_checksum
    info "Checksum verified."
fi

info "All vendor binaries ready."
