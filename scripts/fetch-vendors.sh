#!/usr/bin/env bash
#
# fetch-vendors.sh -- Download pre-built third-party binaries needed for the
#                     AppImage build.
#
# Currently fetches:
#   vendor/libdeep_filter_ladspa.so  — DeepFilterNet3 LADSPA plugin (MIT/Apache-2.0)
#     Source: https://github.com/Rikorose/DeepFilterNet
#     Model weights are embedded; no separate model files needed.
#
# Usage: bash scripts/fetch-vendors.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
VENDOR_DIR="$PROJECT_ROOT/vendor"

DEEPFILTER_VERSION="0.5.6"
DEEPFILTER_URL="https://github.com/Rikorose/DeepFilterNet/releases/download/v${DEEPFILTER_VERSION}/libdeep_filter_ladspa-${DEEPFILTER_VERSION}-x86_64-unknown-linux-gnu.so"
DEEPFILTER_OUT="$VENDOR_DIR/libdeep_filter_ladspa.so"

info()  { printf '\033[1;34m==> %s\033[0m\n' "$*"; }
error() { printf '\033[1;31m==> ERROR: %s\033[0m\n' "$*" >&2; exit 1; }

mkdir -p "$VENDOR_DIR"

# ── DeepFilterNet LADSPA plugin ───────────────────────────────────────────────
if [ -f "$DEEPFILTER_OUT" ]; then
    info "vendor/libdeep_filter_ladspa.so already present — skipping download."
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
fi

info "All vendor binaries ready."
