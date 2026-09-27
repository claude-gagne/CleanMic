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
# GTK4 + libadwaita (and their non-excluded transitive dependencies) are now
# BUNDLED by default (D-01, 15.4-03), via linuxdeploy + linuxdeploy-plugin-gtk.
# Set CLEANMIC_BUNDLE_GTK=0 to fall back to the pre-Plan-03 behavior (host
# GTK4/libadwaita, faster local iteration -- the resulting AppImage then
# requires the target system to already have GTK4 + libadwaita installed,
# same as every release before 15.4-03).
#
# The resulting AppImage assumes the target system has:
#   - D-Bus -- always host-provided (kept off the bundle by design, D-01)
# GTK4/libadwaita (and their non-excluded deps) are bundled by default; with
# CLEANMIC_BUNDLE_GTK=0 they fall back to host-provided instead.
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

# ── GTK4/libadwaita bundling pins (D-01, 15.4-03) ─────────────────────────────
# CLEANMIC_BUNDLE_GTK controls the new deploy step below: 1 (default) bundles
# GTK4 + libadwaita + their non-excluded transitive deps via linuxdeploy +
# linuxdeploy-plugin-gtk; 0 keeps the pre-15.4-03 host-GTK build.
CLEANMIC_BUNDLE_GTK="${CLEANMIC_BUNDLE_GTK:-1}"

# linuxdeploy: pinned to a real, dated release tag (not the moving
# "continuous" asset the rest of the AppImage ecosystem tends to use) so a
# rebuild months from now fetches the exact same bytes. Digest independently
# verified against a fresh download during 15.4-03 planning (matches GitHub's
# own reported asset digest).
LINUXDEPLOY_VERSION="1-alpha-20251107-1"
LINUXDEPLOY_FILENAME="linuxdeploy-x86_64.AppImage"
LINUXDEPLOY_URL="https://github.com/linuxdeploy/linuxdeploy/releases/download/${LINUXDEPLOY_VERSION}/${LINUXDEPLOY_FILENAME}"
LINUXDEPLOY_CACHE_DIR="$TOOLS_DIR/linuxdeploy/${LINUXDEPLOY_VERSION}"
LINUXDEPLOY_BIN="$LINUXDEPLOY_CACHE_DIR/$LINUXDEPLOY_FILENAME"

# linuxdeploy-plugin-gtk: no tagged releases upstream (raw script, consumed at
# `master`) -- pinned to a specific commit SHA instead of a version string, so
# the URL itself is reproducible.
LINUXDEPLOY_PLUGIN_GTK_COMMIT="7a3fbc31a9e5075073ff8790f26effbac5f84453"
LINUXDEPLOY_PLUGIN_GTK_URL="https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/${LINUXDEPLOY_PLUGIN_GTK_COMMIT}/linuxdeploy-plugin-gtk.sh"
LINUXDEPLOY_PLUGIN_GTK_CACHE_DIR="$TOOLS_DIR/linuxdeploy-plugin-gtk/${LINUXDEPLOY_PLUGIN_GTK_COMMIT}"
LINUXDEPLOY_PLUGIN_GTK_BIN="$LINUXDEPLOY_PLUGIN_GTK_CACHE_DIR/linuxdeploy-plugin-gtk.sh"

# ── Helpers ──────────────────────────────────────────────────────────────────
info()  { printf '\033[1;34m==> %s\033[0m\n' "$*"; }
warn()  { printf '\033[1;33m==> %s\033[0m\n' "$*"; }
error() { printf '\033[1;31m==> %s\033[0m\n' "$*" >&2; exit 1; }

# tofu_fetch URL DEST LABEL -- generic trust-on-first-use cache+verify,
# mirroring the uruntime pin's own semantics (Step 7b below): first download
# computes and persists a sha256 sidecar; every later run verifies the cached
# file against that sidecar via `sha256sum -c` and hard-errors on mismatch
# (no auto-redownload -- could be tampering or cache corruption; let the
# operator decide, same as uruntime). Used by the GTK-bundling tool pins
# (D-01, 15.4-03) below; the uruntime block keeps its own inline copy of this
# logic unchanged to avoid touching already-verified working code.
tofu_fetch() {
    local url="$1" dest="$2" label="$3"
    local sidecar="$dest.sha256"
    mkdir -p "$(dirname "$dest")"
    if [ ! -f "$dest" ]; then
        info "  Cache miss -- downloading $label..."
        if command -v wget &>/dev/null; then
            wget -q -O "$dest.tmp" "$url"
        elif command -v curl &>/dev/null; then
            curl -fsSL -o "$dest.tmp" "$url"
        else
            error "Neither wget nor curl found. Cannot download $label."
        fi
        mv "$dest.tmp" "$dest"
        chmod +x "$dest"
        local actual_sha
        actual_sha=$(sha256sum "$dest" | awk '{print $1}')
        printf '%s  %s\n' "$actual_sha" "$(basename "$dest")" > "$sidecar"
        info "  Downloaded $label -> $dest (TOFU sidecar pinned: $actual_sha)"
    else
        if [ ! -f "$sidecar" ]; then
            warn "  Cache hit but sidecar missing for $label -- recomputing (defensive TOFU)."
            local actual_sha
            actual_sha=$(sha256sum "$dest" | awk '{print $1}')
            printf '%s  %s\n' "$actual_sha" "$(basename "$dest")" > "$sidecar"
        fi
        if ! ( cd "$(dirname "$dest")" && sha256sum -c "$(basename "$sidecar")" --status ); then
            error "$label SHA256 mismatch.
    cached file: $dest
    sidecar:     $sidecar
  Either the cache is corrupted/tampered, or the sidecar is stale.
  To re-establish TOFU: rm -rf \"$(dirname "$dest")\" && re-run this script."
        fi
        info "  Cache hit: $dest (SHA256 verified) -- $label"
    fi
}

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

# ── Step 6b: Bundle GTK4 + libadwaita via linuxdeploy (D-01, 15.4-03) ───────
if [ "$CLEANMIC_BUNDLE_GTK" = "1" ]; then
    info "Bundling GTK4 + libadwaita (CLEANMIC_BUNDLE_GTK=1, D-01)..."

    info "Verifying linuxdeploy + linuxdeploy-plugin-gtk cache..."
    tofu_fetch "$LINUXDEPLOY_URL" "$LINUXDEPLOY_BIN" "linuxdeploy $LINUXDEPLOY_VERSION"
    tofu_fetch "$LINUXDEPLOY_PLUGIN_GTK_URL" "$LINUXDEPLOY_PLUGIN_GTK_BIN" "linuxdeploy-plugin-gtk @ ${LINUXDEPLOY_PLUGIN_GTK_COMMIT:0:12}"

    info "Running linuxdeploy --plugin gtk (this can take a minute)..."
    # --custom-apprun points at the SOURCE template, not the AppDir copy just
    # made above: linuxdeploy's own custom-AppRun deploy path removes
    # whatever is already at AppDir/AppRun before copying the given path IN,
    # so pointing it at the already-in-place copy would delete its own
    # source out from under itself.
    # No --desktop-file is passed (we install our own desktop file in Step 4
    # above) -- this is deliberate: linuxdeploy only wraps AppRun to source
    # apprun-hooks/ when it also deployed a desktop file of its own, and that
    # wrapper would force GDK_BACKEND=x11 and a fixed GTK_THEME (violating
    # this plan's must_haves). Skipping -d keeps our own AppRun authoritative
    # and apprun-hooks/ inert (deleted below regardless, belt and suspenders).
    (
        export APPIMAGE_EXTRACT_AND_RUN=1
        export DEPLOY_GTK_VERSION=4
        export PATH="$LINUXDEPLOY_PLUGIN_GTK_CACHE_DIR:$PATH"
        # linuxdeploy-plugin-gtk's own "more libraries" pass (pango/gio/gobject/
        # librsvg's transitive deps) re-invokes linuxdeploy IN-PROCESS via
        # `env LINUXDEPLOY_PLUGIN_MODE=1 linuxdeploy --appdir=... --library=...`
        # -- a second AppDir/exclude-pattern instance that never sees our
        # --exclude-library CLI flags below [VERIFIED empirically during
        # 15.4-03: libdbus-1.so.3 was bundled anyway even with the CLI flag,
        # traced to this second pass re-discovering it via libgtk-4/
        # libadwaita's own gio/GDBus dependency]. LINUXDEPLOY_EXCLUDED_LIBRARIES
        # is read directly from the environment by every linuxdeploy process's
        # AppDir constructor, so exporting it here (not just passing
        # --exclude-library) reaches that second pass too.
        export LINUXDEPLOY_EXCLUDED_LIBRARIES="libpipewire-0.3.so*;libdbus-1.so*"
        "$LINUXDEPLOY_BIN" \
            --appdir "$APPDIR" \
            --executable "$APPDIR/usr/bin/cleanmic" \
            --custom-apprun "$APPRUN_SRC" \
            --plugin gtk \
            --exclude-library 'libpipewire-0.3.so*' \
            --exclude-library 'libdbus-1.so*'
    ) || error "linuxdeploy GTK bundling failed (set CLEANMIC_BUNDLE_GTK=0 to fall back to the host-GTK build)"

    # linuxdeploy auto-discovers our desktop file under usr/share/applications
    # (even though we never pass -d/--desktop-file) and, having found one,
    # runs its AppDir-root setup -- which, because --plugin gtk already wrote
    # apprun-hooks/linuxdeploy-plugin-gtk.sh by this point, WRAPS AppRun: our
    # own script gets renamed to AppRun.wrapped, and a new autogenerated
    # AppRun is written that sources every apprun-hooks/*.sh (forcing
    # GDK_BACKEND=x11 and a fixed GTK_THEME) before exec'ing AppRun.wrapped
    # [VERIFIED empirically during 15.4-03 planning/execution -- confirmed by
    # inspecting the post-deploy AppDir]. That directly violates this plan's
    # must_haves (neither GDK_BACKEND nor GTK_THEME may be forced) and would
    # break launch entirely once apprun-hooks/ is deleted below (the
    # autogenerated wrapper would then `source` a glob that matches nothing).
    # Fix: unconditionally re-assert our own AppRun as the real AppRun,
    # regardless of whatever linuxdeploy did to it.
    cp "$APPRUN_SRC" "$APPDIR/AppRun"
    chmod +x "$APPDIR/AppRun"
    rm -f "$APPDIR/AppRun.wrapped"

    # apprun-hooks/ is never sourced by our own AppRun (scripts/appimage-apprun.sh
    # carries its own guarded exports for the bundled GSettings schema dir, GI
    # typelibs, and gdk-pixbuf loaders.cache instead -- see that script).
    # Delete it so a future change can never accidentally start sourcing it.
    rm -rf "$APPDIR/apprun-hooks"

    # Post-deploy assertion (D-01): no excludelist soname bundled directly
    # under usr/lib (the one documented exemption is the D-10
    # usr/lib/pipewire-fallback/ dir), and no glibc library got pulled in
    # either -- glibc's own family is on the same excludelist. Fetches the
    # same excludelist catalog-proxy.sh's own lint uses (and shares its cache
    # path), but checks only this one condition inline rather than reusing
    # catalog-proxy's full --appdir --lint-only: that lint also expects a
    # packaged AppImage's .DirIcon (created by appimagetool in Step 8, which
    # has not run yet at this point in the build), and would always FAIL here
    # for a reason unrelated to GTK bundling.
    info "Asserting the bundle respects the AppImage excludelist..."
    EXCLUDELIST_CACHE_FOR_BUILD="$BUILD_DIR/tools/excludelist"
    mkdir -p "$(dirname "$EXCLUDELIST_CACHE_FOR_BUILD")"
    if [ ! -f "$EXCLUDELIST_CACHE_FOR_BUILD" ]; then
        EXCLUDELIST_URL="https://raw.githubusercontent.com/AppImage/AppImages/master/excludelist"
        if command -v curl &>/dev/null; then
            curl -fsSL --max-time 10 -o "$EXCLUDELIST_CACHE_FOR_BUILD.tmp" "$EXCLUDELIST_URL" \
                && mv "$EXCLUDELIST_CACHE_FOR_BUILD.tmp" "$EXCLUDELIST_CACHE_FOR_BUILD"
        elif command -v wget &>/dev/null; then
            wget -q -O "$EXCLUDELIST_CACHE_FOR_BUILD.tmp" "$EXCLUDELIST_URL" \
                && mv "$EXCLUDELIST_CACHE_FOR_BUILD.tmp" "$EXCLUDELIST_CACHE_FOR_BUILD"
        fi
    fi
    if [ -f "$EXCLUDELIST_CACHE_FOR_BUILD" ]; then
        EXCLUDELIST_VIOLATIONS=""
        while IFS= read -r soname; do
            [ -n "$soname" ] || continue
            while IFS= read -r hit; do
                [ -n "$hit" ] || continue
                case "$hit" in
                    "$APPDIR"/usr/lib/pipewire-fallback/*) ;; # documented D-10 exemption
                    *) EXCLUDELIST_VIOLATIONS="${EXCLUDELIST_VIOLATIONS}
  - $hit" ;;
                esac
            done < <(find "$APPDIR/usr/lib" -type f -name "$soname" 2>/dev/null)
        done < <(grep -v '^#' "$EXCLUDELIST_CACHE_FOR_BUILD" | grep -v '^[[:space:]]*$' | awk '{print $1}')
        if [ -n "$EXCLUDELIST_VIOLATIONS" ]; then
            error "Excludelist violation(s) bundled directly under usr/lib (outside the documented D-10 pipewire-fallback exemption):${EXCLUDELIST_VIOLATIONS}"
        fi
        info "  No excludelist soname bundled outside the documented D-10 fallback exemption."
    else
        warn "  Could not fetch the AppImage excludelist (offline?) -- skipping this assertion this run. catalog-proxy.sh's own lint (run separately) still checks this against the packed AppImage."
    fi

    # BUNDLED-LIBRARIES.txt (D-01): every .so under usr/lib, its resolved
    # source package + version (via dpkg -S / dpkg-query when the build host
    # knows the file), so THIRD-PARTY-LICENSES.md's "Bundled GTK stack"
    # section can point here for exact, regenerated-every-build provenance
    # instead of a hand-maintained list that drifts from what's shipped.
    info "Recording bundled library provenance (BUNDLED-LIBRARIES.txt)..."
    BUNDLED_LIBS_OUT="$APPDIR/usr/share/doc/cleanmic/BUNDLED-LIBRARIES.txt"
    mkdir -p "$(dirname "$BUNDLED_LIBS_OUT")"
    # set +e for this block only: dpkg -S legitimately exits nonzero for any
    # file it can't match to a package (common -- see the basename-glob note
    # below), and under `set -e` that would abort the whole build the first
    # time it happened, not just skip that one line.
    set +e
    {
        echo "# Bundled libraries under usr/lib -- generated by scripts/build-appimage.sh"
        echo "# CLEANMIC_BUNDLE_GTK=1 (D-01, 15.4-03). Regenerated on every build; do not hand-edit."
        echo "# file | soname (best-effort) | source package | package version"
        echo
        while IFS= read -r -d '' f; do
            rel="${f#"$APPDIR"/}"
            soname="$(objdump -p "$f" 2>/dev/null | awk '/SONAME/{print $2; exit}')"
            [ -n "$soname" ] || soname="$(basename "$f")"
            # Match by basename glob, not by the copied-into-AppDir path:
            # once linuxdeploy has copied a file into build/AppDir/usr/lib,
            # its own path no longer resolves to anything dpkg's database
            # knows about (dpkg -S on the AppDir copy's path always misses).
            pkg_name=""
            if command -v dpkg >/dev/null 2>&1; then
                pkg_name="$(dpkg -S "*/$(basename "$f")" 2>/dev/null | head -1 | cut -d: -f1)"
            fi
            if [ -n "$pkg_name" ] && command -v dpkg-query >/dev/null 2>&1; then
                pkg_ver="$(dpkg-query -W -f='${Version}' "$pkg_name" 2>/dev/null)"
                printf '%s | %s | %s | %s\n' "$rel" "$soname" "$pkg_name" "${pkg_ver:-unknown}"
            else
                printf '%s | %s | built from source: unknown NAME/VERSION (not a distro package on the build host)\n' "$rel" "$soname"
            fi
        done < <(find "$APPDIR/usr/lib" -type f \( -name '*.so' -o -name '*.so.*' \) -print0 2>/dev/null | sort -z)
    } > "$BUNDLED_LIBS_OUT"
    BUNDLED_LIBS_COUNT="$(grep -c ' | ' "$BUNDLED_LIBS_OUT")"
    set -e
    info "  Wrote $BUNDLED_LIBS_OUT ($BUNDLED_LIBS_COUNT entries)"
else
    info "CLEANMIC_BUNDLE_GTK=0 -- keeping GTK4/libadwaita host-provided (legacy behavior, pre-15.4-03)"
fi

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
