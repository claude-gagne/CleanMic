#!/usr/bin/env bash
#
# build-appimage.sh -- Build a CleanMic AppImage for x86_64 Linux.
#
# Usage:  ./scripts/build-appimage.sh
#
# Prerequisites:
#   - Rust toolchain (cargo)
#   - Development headers for GTK4, libadwaita, PipeWire (build-time only)
#   - wget or curl (to download appimagetool if not cached)
#
# The resulting AppImage assumes the target system has:
#   - GTK4 + libadwaita (standard on Ubuntu 24.04 with GNOME) -- host-provided
#     until Plan 03 bundles them
#   - D-Bus -- always host-provided
# These libraries are NOT bundled in the AppImage.
#
# PipeWire's client library (libpipewire-0.3.so.0) ships as a FALLBACK ONLY
# (D-10, 15.4-02): a copy lives at usr/lib/pipewire-fallback/, and AppRun
# (scripts/appimage-apprun.sh) adds that dir to LD_LIBRARY_PATH ONLY when the
# host has no libpipewire-0.3.so.0 at all. A normal install always resolves
# its own system copy first -- there is never a client/daemon ABI mismatch.
# See THIRD-PARTY-LICENSES.md's "PipeWire client library" section.

set -euo pipefail

# Ensure cargo is on PATH (common for rustup installs)
if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck source=/dev/null
    . "$HOME/.cargo/env"
fi

# ── Paths ────────────────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILD_DIR="$PROJECT_ROOT/build"
APPDIR="$BUILD_DIR/AppDir"
TOOLS_DIR="$BUILD_DIR/tools"
BINARY="$PROJECT_ROOT/target/release/cleanmic"
APPIMAGETOOL="$TOOLS_DIR/appimagetool"
APPIMAGETOOL_URL="https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage"

# ── uruntime pin (Ubuntu 26.04 compat — see .planning/quick/260427-ebz-…/) ───
# uruntime is a libfuse3-compatible, statically-linked AppImage runtime stub.
# Default appimagetool runtime is libfuse2-only and breaks on default Ubuntu 26.04
# installs (libfuse2t64 lives in universe, not in the default desktop install set).
# We hand uruntime to appimagetool via --runtime-file in Step 8.
URUNTIME_VERSION="v0.5.7"
URUNTIME_FILENAME="uruntime-appimage-squashfs-x86_64"
URUNTIME_URL="https://github.com/VHSgunzo/uruntime/releases/download/${URUNTIME_VERSION}/${URUNTIME_FILENAME}"
URUNTIME_CACHE_DIR="$TOOLS_DIR/uruntime/${URUNTIME_VERSION}"
URUNTIME_BIN="$URUNTIME_CACHE_DIR/$URUNTIME_FILENAME"
URUNTIME_SHA_SIDECAR="$URUNTIME_BIN.sha256"

# ── Helpers ──────────────────────────────────────────────────────────────────
info()  { printf '\033[1;34m==> %s\033[0m\n' "$*"; }
warn()  { printf '\033[1;33m==> %s\033[0m\n' "$*"; }
error() { printf '\033[1;31m==> %s\033[0m\n' "$*" >&2; exit 1; }

# ── DPDFNet variant selector (Phase 15.1 Plan 07 — D-01/D-02/D-03/D-04) ─────
# DPDFNET_VARIANTS picks which DPDFNet model asset set (if any) this build
# bundles offline, independently of the other suppression engines:
#   none      -- (default) no `dpdfnet` cargo feature, no models/runtime
#                bundled. Identical to this script's pre-Phase-15.1 behavior
#                and filename, so existing invocations/CI are unaffected.
#   dpdfnet2  -- bundle only the DPDFNet-2 model + the one shared runtime.
#   dpdfnet8  -- bundle only the DPDFNet-8 model + the one shared runtime.
#   both      -- bundle both models + the one shared runtime.
# The throwaway `dpdfnet-experimental` cargo feature is NEVER included in any
# of these four package builds (measurement-only, never shipped).
DPDFNET_VARIANTS="${DPDFNET_VARIANTS:-none}"
BASE_FEATURES="gui,pipewire,tray,rnnoise,deepfilter,updater"

# dpdfnet_variant_features <variant> -- pure, prints the cargo --features
# value for the given variant.
dpdfnet_variant_features() {
    local variant="$1"
    case "$variant" in
        none) printf '%s' "$BASE_FEATURES" ;;
        dpdfnet2|dpdfnet8|both) printf '%s,dpdfnet' "$BASE_FEATURES" ;;
        *) return 1 ;;
    esac
}

# dpdfnet_output_suffix <variant> -- pure, prints the AppImage output
# filename suffix for the given variant. "none" is empty so the default
# invocation keeps the exact pre-Phase-15.1 filename.
dpdfnet_output_suffix() {
    local variant="$1"
    case "$variant" in
        none) printf '' ;;
        dpdfnet2) printf -- '-dpdfnet2' ;;
        dpdfnet8) printf -- '-dpdfnet8' ;;
        both) printf -- '-dpdfnet-both' ;;
        *) return 1 ;;
    esac
}

# dpdfnet_models_for_variant <variant> -- pure, prints the newline-separated
# bundled model filename(s) for the given variant (nothing for "none").
# Filenames match src/engine/dpdfnet.rs's DpdfnetVariant::model_filename().
dpdfnet_models_for_variant() {
    local variant="$1"
    case "$variant" in
        none) return 0 ;;
        dpdfnet2) printf '%s\n' "dpdfnet2_48khz_hr.onnx" ;;
        dpdfnet8) printf '%s\n' "dpdfnet8_48khz_hr.onnx" ;;
        both) printf '%s\n%s\n' "dpdfnet2_48khz_hr.onnx" "dpdfnet8_48khz_hr.onnx" ;;
        *) return 1 ;;
    esac
}

# dpdfnet_bundle_assets <variant> <ref_dir> <appdir> -- copies the ONE shared
# ONNX Runtime + the requested model(s) from an already offline-verified
# DPDFNet reference tree into an AppDir. Fails closed (nonzero exit via
# `error`) on any missing source file -- never partially bundles, never
# searches/downloads at build OR runtime (T-15.1-11/T-15.1-12). Never
# packages the golden probe, the `enhance` renderer, or checked-out
# HushMic/DPDFNet source -- only the shared runtime + requested model(s).
dpdfnet_bundle_assets() {
    local variant="$1" ref_dir="$2" appdir="$3"
    [ "$variant" = "none" ] && return 0

    local runtime_src="$ref_dir/lib/libonnxruntime.so"
    if [ ! -f "$runtime_src" ]; then
        error "DPDFNet runtime not found at $runtime_src (run: bash scripts/fetch-vendors.sh --dpdfnet-reference)"
    fi
    mkdir -p "$appdir/usr/lib"
    cp "$runtime_src" "$appdir/usr/lib/libonnxruntime.so"
    info "  Bundled shared runtime: libonnxruntime.so"

    mkdir -p "$appdir/usr/share/cleanmic/models"
    local model_file model_src
    while IFS= read -r model_file; do
        [ -n "$model_file" ] || continue
        model_src="$ref_dir/models/$model_file"
        if [ ! -f "$model_src" ]; then
            error "DPDFNet model not found at $model_src (run: bash scripts/fetch-vendors.sh --dpdfnet-reference)"
        fi
        cp "$model_src" "$appdir/usr/share/cleanmic/models/$model_file"
        info "  Bundled model: $model_file"
    done <<EOF
$(dpdfnet_models_for_variant "$variant")
EOF
}

case "$DPDFNET_VARIANTS" in
    none|dpdfnet2|dpdfnet8|both) ;;
    *) error "DPDFNET_VARIANTS must be one of: none, dpdfnet2, dpdfnet8, both (got: $DPDFNET_VARIANTS)" ;;
esac

# ── Test-only early exit ─────────────────────────────────────────────────────
# Lets scripts/test-dpdfnet-appimage.sh `source` this file to reuse the pure
# functions above (feature/suffix/model-set computation, plus the sandboxable
# dpdfnet_bundle_assets) without running a real cargo build/AppImage
# packaging pass. Never set by a normal build invocation.
if [ "${BUILD_APPIMAGE_SOURCE_ONLY:-0}" = "1" ]; then
    return 0 2>/dev/null || exit 0
fi

CARGO_FEATURES="$(dpdfnet_variant_features "$DPDFNET_VARIANTS")"
OUTPUT="$BUILD_DIR/CleanMic-x86_64$(dpdfnet_output_suffix "$DPDFNET_VARIANTS").AppImage"
DPDFNET_REF_DIR="${DPDFNET_REF_DIR:-$PROJECT_ROOT/vendor/dpdfnet-reference}"

# ── Step 0: Offline re-verify the pinned DPDFNet reference assets ──────────
# Only for DPDF-bearing builds (D-01/D-02/D-03). Never touches the network --
# fails closed on missing/tampered/symlinked/hash-drifted assets before any
# cargo build or AppDir work begins.
if [ "$DPDFNET_VARIANTS" != "none" ]; then
    info "DPDFNET_VARIANTS=$DPDFNET_VARIANTS -- verifying pinned DPDFNet reference assets offline..."
    bash "$SCRIPT_DIR/fetch-vendors.sh" --verify-dpdfnet-reference
fi

# ── Step 1: Build release binary ────────────────────────────────────────────
info "Building release binary (features: $CARGO_FEATURES)..."
(cd "$PROJECT_ROOT" && cargo build --release --features "$CARGO_FEATURES")

if [ ! -f "$BINARY" ]; then
    error "Release binary not found at $BINARY"
fi

info "Binary size: $(du -h "$BINARY" | cut -f1)"

# ── Step 2: Create AppDir structure ─────────────────────────────────────────
info "Creating AppDir structure..."

rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin"
mkdir -p "$APPDIR/usr/share/applications"
mkdir -p "$APPDIR/usr/share/icons/hicolor/scalable/apps"
mkdir -p "$APPDIR/usr/share/icons/hicolor/symbolic/apps"
mkdir -p "$APPDIR/usr/lib"

# ── Step 3: Copy binary ─────────────────────────────────────────────────────
info "Installing binary..."
cp "$BINARY" "$APPDIR/usr/bin/cleanmic"
strip "$APPDIR/usr/bin/cleanmic" 2>/dev/null || warn "strip not available, binary not stripped"

# ── Step 3-pw: Bundle libpipewire-0.3.so.0 as a host-absent fallback (D-10) ─
# Real users keep loading their own system PipeWire client library -- this
# copy is NEVER placed in usr/lib itself (which AppRun's LD_LIBRARY_PATH
# always resolves first); it lives only in usr/lib/pipewire-fallback/, which
# AppRun (scripts/appimage-apprun.sh) appends to LD_LIBRARY_PATH ONLY when
# ldd reports libpipewire-0.3.so.0 unresolved on the host. This respects the
# AppImage excludelist's intent (client/daemon ABI-skew risk) while still
# fixing the catalog's actual failure: a host with no libpipewire at all.
info "Bundling libpipewire-0.3.so.0 fallback (host-absent only, D-10)..."
PIPEWIRE_SONAME="libpipewire-0.3.so.0"
PIPEWIRE_LDD_LINE="$(ldd "$BINARY" 2>/dev/null | grep "$PIPEWIRE_SONAME" || true)"
PIPEWIRE_RESOLVED="$(printf '%s' "$PIPEWIRE_LDD_LINE" | awk '{print $3}')"
if [ -z "$PIPEWIRE_RESOLVED" ] || [ ! -e "$PIPEWIRE_RESOLVED" ]; then
    error "Could not resolve $PIPEWIRE_SONAME on the build host via ldd -- cannot bundle the D-10 fallback copy. (ldd output: ${PIPEWIRE_LDD_LINE:-<empty>})"
fi
PIPEWIRE_REAL="$(readlink -f "$PIPEWIRE_RESOLVED")"
if [ ! -f "$PIPEWIRE_REAL" ]; then
    error "Resolved $PIPEWIRE_SONAME target does not exist: $PIPEWIRE_REAL"
fi
mkdir -p "$APPDIR/usr/lib/pipewire-fallback"
cp "$PIPEWIRE_REAL" "$APPDIR/usr/lib/pipewire-fallback/$PIPEWIRE_SONAME"
# The fallback copy must depend on nothing beyond glibc/the loader -- if the
# build host's own PipeWire client needed anything else, bundling it here
# would just move the "cannot open shared object file" failure one level
# down, onto a host that also lacks whatever that extra dependency is.
PIPEWIRE_FALLBACK_DEPS="$(ldd "$APPDIR/usr/lib/pipewire-fallback/$PIPEWIRE_SONAME" 2>/dev/null | grep -Ev 'linux-vdso\.so|libc\.so|ld-linux' || true)"
if [ -n "$PIPEWIRE_FALLBACK_DEPS" ]; then
    error "The resolved $PIPEWIRE_SONAME ($PIPEWIRE_REAL) needs more than glibc/the loader -- refusing to bundle it as a D-10 fallback:
$PIPEWIRE_FALLBACK_DEPS"
fi
info "  Bundled fallback: usr/lib/pipewire-fallback/$PIPEWIRE_SONAME (from $PIPEWIRE_REAL)"

# ── Step 3a: Bundle the pre-flight dependency-check helper ──────────────────
# AppRun invokes this immediately before exec to catch a missing host library
# (e.g. libadwaita-1.so.0) with a clear message instead of a cryptic linker
# crash. Required artifact, unlike the optional DeepFilter .so below.
info "Bundling pre-flight helper..."
PREFLIGHT_SRC="$SCRIPT_DIR/appimage-preflight.sh"
if [ ! -f "$PREFLIGHT_SRC" ]; then
    error "Required file not found: $PREFLIGHT_SRC"
fi
cp "$PREFLIGHT_SRC" "$APPDIR/usr/bin/cleanmic-preflight"
chmod +x "$APPDIR/usr/bin/cleanmic-preflight"

# ── Step 3b: Bundle DeepFilterNet LADSPA plugin ─────────────────────────────
# libdeep_filter_ladspa.so has the DeepFilterNet3 model embedded — no extra
# model files needed. It only depends on standard system libs (libc, libm).
DEEPFILTER_SO="$PROJECT_ROOT/vendor/libdeep_filter_ladspa.so"
if [ -f "$DEEPFILTER_SO" ]; then
    info "Bundling libdeep_filter_ladspa.so..."
    cp "$DEEPFILTER_SO" "$APPDIR/usr/lib/libdeep_filter_ladspa.so"
else
    warn "vendor/libdeep_filter_ladspa.so not found — DeepFilterNet will be unavailable in the AppImage."
    warn "Run: curl -L -o vendor/libdeep_filter_ladspa.so <url>"
fi

# ── Step 3c: Ship the third-party license notice ────────────────────────────
# MIT/Apache-2.0 require the license/copyright notice to accompany binary
# redistribution, not just live in the git repo (D-02). This is REQUIRED
# (unlike the optional .so above) — the notice must legally accompany the
# bundled binary, so a missing source fails the build rather than warning.
info "Bundling third-party license notice..."
THIRD_PARTY_NOTICE="$PROJECT_ROOT/THIRD-PARTY-LICENSES.md"
if [ ! -f "$THIRD_PARTY_NOTICE" ]; then
    error "Required file not found: $THIRD_PARTY_NOTICE (must accompany the bundled DeepFilterNet binary)"
fi
mkdir -p "$APPDIR/usr/share/doc/cleanmic"
cp "$THIRD_PARTY_NOTICE" "$APPDIR/usr/share/doc/cleanmic/THIRD-PARTY-LICENSES.md"

# ── Step 3d: Bundle the shared ONNX Runtime + requested DPDFNet model(s) ───
if [ "$DPDFNET_VARIANTS" != "none" ]; then
    info "Bundling DPDFNet shared runtime + model(s) (DPDFNET_VARIANTS=$DPDFNET_VARIANTS)..."
    dpdfnet_bundle_assets "$DPDFNET_VARIANTS" "$DPDFNET_REF_DIR" "$APPDIR"
fi

# ── Step 4: Copy desktop file and icons ──────────────────────────────────────
info "Installing desktop file and icons..."

cp "$PROJECT_ROOT/assets/com.cleanmic.CleanMic.desktop" \
   "$APPDIR/usr/share/applications/"

# AppImage requires the desktop file and icon at the AppDir root as well
cp "$PROJECT_ROOT/assets/com.cleanmic.CleanMic.desktop" "$APPDIR/"

cp "$PROJECT_ROOT/assets/icons/com.cleanmic.CleanMic.svg" \
   "$APPDIR/usr/share/icons/hicolor/scalable/apps/"
cp "$PROJECT_ROOT/assets/icons/com.cleanmic.CleanMic.svg" "$APPDIR/"

# Symbolic/tray icons
cp "$PROJECT_ROOT/assets/icons/cleanmic-active.svg" \
   "$APPDIR/usr/share/icons/hicolor/symbolic/apps/cleanmic-active-symbolic.svg"
cp "$PROJECT_ROOT/assets/icons/cleanmic-disabled.svg" \
   "$APPDIR/usr/share/icons/hicolor/symbolic/apps/cleanmic-disabled-symbolic.svg"

# Tray icons (ksni looks up by icon_name "cleanmic-active" without -symbolic suffix)
cp "$PROJECT_ROOT/assets/icons/cleanmic-active.svg" \
   "$APPDIR/usr/share/icons/hicolor/scalable/apps/cleanmic-active.svg"
cp "$PROJECT_ROOT/assets/icons/cleanmic-disabled.svg" \
   "$APPDIR/usr/share/icons/hicolor/scalable/apps/cleanmic-disabled.svg"

# PNG app icon (D-03): appdir-lint's own check-appstream prefers a PNG
# .DirIcon over SVG-only (SVG-only is a WARNING, not fatal) -- installed to
# the standard hicolor PNG location AND at the AppDir root next to the SVG
# so appimagetool's own Icon=-driven .DirIcon lookup (Step 8) has a PNG to
# find, alongside the existing SVG.
mkdir -p "$APPDIR/usr/share/icons/hicolor/256x256/apps"
cp "$PROJECT_ROOT/assets/icons/com.cleanmic.CleanMic.png" \
   "$APPDIR/usr/share/icons/hicolor/256x256/apps/com.cleanmic.CleanMic.png"
cp "$PROJECT_ROOT/assets/icons/com.cleanmic.CleanMic.png" "$APPDIR/com.cleanmic.CleanMic.png"

# ── Step 4b: AppStream metainfo (D-03) ──────────────────────────────────────
info "Bundling AppStream metainfo..."
METAINFO_SRC="$PROJECT_ROOT/assets/com.cleanmic.CleanMic.metainfo.xml"
if [ ! -f "$METAINFO_SRC" ]; then
    error "Required file not found: $METAINFO_SRC"
fi
mkdir -p "$APPDIR/usr/share/metainfo"
cp "$METAINFO_SRC" "$APPDIR/usr/share/metainfo/com.cleanmic.CleanMic.metainfo.xml"

# ── Step 5: Bundle locale files ──────────────────────────────────────────────
info "Bundling locale files..."
for podir in "$PROJECT_ROOT"/locale/*/LC_MESSAGES; do
    lang=$(basename "$(dirname "$podir")")
    mofile="$podir/cleanmic.mo"
    if [ -f "$mofile" ]; then
        mkdir -p "$APPDIR/usr/share/locale/$lang/LC_MESSAGES"
        cp "$mofile" "$APPDIR/usr/share/locale/$lang/LC_MESSAGES/cleanmic.mo"
        info "  Bundled locale: $lang"
    else
        warn "  No .mo file for $lang (run 'make mo' first)"
    fi
done

# ── Step 6: Create AppRun entry point ────────────────────────────────────────
# AppRun now lives in its own template (scripts/appimage-apprun.sh) instead
# of a heredoc here, so scripts/test-appimage-apprun.sh can exercise it
# directly (D-10's fallback decision, the pre-flight exit-code contract).
info "Creating AppRun..."

APPRUN_SRC="$SCRIPT_DIR/appimage-apprun.sh"
if [ ! -f "$APPRUN_SRC" ]; then
    error "Required file not found: $APPRUN_SRC"
fi
cp "$APPRUN_SRC" "$APPDIR/AppRun"
chmod +x "$APPDIR/AppRun"

# ── Step 7: Download appimagetool if needed ──────────────────────────────────
if [ ! -x "$APPIMAGETOOL" ]; then
    info "Downloading appimagetool..."
    mkdir -p "$TOOLS_DIR"
    if command -v wget &>/dev/null; then
        wget -q -O "$APPIMAGETOOL" "$APPIMAGETOOL_URL"
    elif command -v curl &>/dev/null; then
        curl -fsSL -o "$APPIMAGETOOL" "$APPIMAGETOOL_URL"
    else
        error "Neither wget nor curl found. Cannot download appimagetool."
    fi
    chmod +x "$APPIMAGETOOL"
fi

# ── Step 7b: Download / cache / verify uruntime ──────────────────────────────
# Two phases: download-if-missing, then SHA256 verify-or-establish (TOFU).
#
# TOFU semantics (per plan constraints — different from sibling "mining" project):
#   - First download: compute hash, persist sidecar (sha256sum-compatible format).
#   - Subsequent runs: verify cached binary against sidecar via `sha256sum -c`.
#   - Mismatch: ERROR with clear message; do NOT auto-redownload (could be
#     tampering OR cache corruption — let the operator decide).
#   - Missing sidecar but binary present (e.g. partial older cache, or a clean
#     checkout where someone hand-copied the binary): defensively recompute +
#     persist the sidecar; don't crash.
info "Verifying uruntime cache..."

mkdir -p "$URUNTIME_CACHE_DIR"

if [ ! -f "$URUNTIME_BIN" ]; then
    info "  Cache miss — downloading $URUNTIME_FILENAME ($URUNTIME_VERSION)..."
    if command -v wget &>/dev/null; then
        wget -q --show-progress -O "$URUNTIME_BIN.tmp" "$URUNTIME_URL"
    elif command -v curl &>/dev/null; then
        curl -fsSL -o "$URUNTIME_BIN.tmp" "$URUNTIME_URL"
    else
        error "Neither wget nor curl found. Cannot download uruntime."
    fi
    mv "$URUNTIME_BIN.tmp" "$URUNTIME_BIN"
    # uruntime release asset ships with mode 0644; needs +x for some toolchains
    # to read it as an executable header. (Sibling project's spike caught this.)
    chmod +x "$URUNTIME_BIN"

    # First-download TOFU: compute and persist the sidecar.
    actual_sha=$(sha256sum "$URUNTIME_BIN" | awk '{print $1}')
    printf '%s  %s\n' "$actual_sha" "$URUNTIME_FILENAME" > "$URUNTIME_SHA_SIDECAR"
    info "  Downloaded $(du -h "$URUNTIME_BIN" | cut -f1) -> $URUNTIME_BIN"
    info "  TOFU sidecar pinned: $actual_sha"
else
    # Cache hit — sidecar must exist (TOFU). If it doesn't, recompute defensively.
    if [ ! -f "$URUNTIME_SHA_SIDECAR" ]; then
        warn "  Cache hit but sidecar missing — recomputing (defensive TOFU)."
        actual_sha=$(sha256sum "$URUNTIME_BIN" | awk '{print $1}')
        printf '%s  %s\n' "$actual_sha" "$URUNTIME_FILENAME" > "$URUNTIME_SHA_SIDECAR"
        info "  Sidecar recomputed: $actual_sha"
    fi

    # Verify cached binary against sidecar — fail hard on mismatch.
    # Run sha256sum -c from the cache dir so it reads the bare filename in the sidecar.
    if ! ( cd "$URUNTIME_CACHE_DIR" && sha256sum -c "$(basename "$URUNTIME_SHA_SIDECAR")" --status ); then
        echo ""
        error "uruntime SHA256 mismatch.
    cached file:  $URUNTIME_BIN
    sidecar:      $URUNTIME_SHA_SIDECAR
  Either the cache is corrupted/tampered, or the sidecar is stale.
  To re-establish TOFU: rm -rf \"$URUNTIME_CACHE_DIR\" && re-run this script."
    fi
    info "  Cache hit: $URUNTIME_BIN (SHA256 verified)"
fi

info "Using uruntime $URUNTIME_VERSION as AppImage runtime (libfuse3-compatible)"

# ── Step 8: Build AppImage ───────────────────────────────────────────────────
info "Building AppImage..."

# appimagetool requires FUSE to run as an AppImage itself.
# If FUSE is not available, try extracting and running directly.
# --runtime-file hands appimagetool the uruntime stub (libfuse3-compatible) so
# the resulting AppImage launches on default Ubuntu 26.04 (no libfuse2t64 needed).
if "$APPIMAGETOOL" --version &>/dev/null 2>&1; then
    ARCH=x86_64 "$APPIMAGETOOL" --runtime-file "$URUNTIME_BIN" "$APPDIR" "$OUTPUT"
else
    warn "appimagetool cannot run directly (FUSE may be missing)."
    warn "Trying --appimage-extract-and-run workaround..."
    ARCH=x86_64 "$APPIMAGETOOL" --appimage-extract-and-run \
        --runtime-file "$URUNTIME_BIN" "$APPDIR" "$OUTPUT"
fi

info "AppImage created: $OUTPUT"
info "Size: $(du -h "$OUTPUT" | cut -f1)"
info "Runtime: uruntime $URUNTIME_VERSION (libfuse3 — works on Ubuntu 24.04 + 26.04)"
info ""
info "To run:  chmod +x $OUTPUT && ./$OUTPUT"
