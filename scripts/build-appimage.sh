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
#   - GTK4 + libadwaita (standard on Ubuntu 24.04 with GNOME)
#   - PipeWire (standard on Ubuntu 22.04+)
#   - D-Bus
# These libraries are NOT bundled in the AppImage.

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
OUTPUT="$BUILD_DIR/CleanMic-x86_64.AppImage"
APPIMAGETOOL="$TOOLS_DIR/appimagetool"
APPIMAGETOOL_URL="https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage"

# ── Helpers ──────────────────────────────────────────────────────────────────
info()  { printf '\033[1;34m==> %s\033[0m\n' "$*"; }
warn()  { printf '\033[1;33m==> %s\033[0m\n' "$*"; }
error() { printf '\033[1;31m==> %s\033[0m\n' "$*" >&2; exit 1; }

# ── Step 1: Build release binary ────────────────────────────────────────────
info "Building release binary..."
(cd "$PROJECT_ROOT" && cargo build --release --all-features)

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
info "Creating AppRun..."

cat > "$APPDIR/AppRun" << 'APPRUN_EOF'
#!/bin/bash
# AppRun -- entry point for CleanMic AppImage
HERE="$(dirname "$(readlink -f "$0")")"

# APPDIR is set by the AppImage runtime before AppRun is called.
# Export it explicitly so child processes can find bundled libraries.
export APPDIR="${APPDIR:-$HERE}"

# Add bundled libraries to search path so dlopen() can find them.
export LD_LIBRARY_PATH="$HERE/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

# Constrain FFTW/OpenBLAS thread pools to prevent Khip from saturating all cores.
export OPENBLAS_NUM_THREADS=1
export OMP_NUM_THREADS=1
export FFTW_NUM_THREADS=1

export PATH="$HERE/usr/bin:$PATH"
export XDG_DATA_DIRS="$HERE/usr/share${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"

# Set up locale search path so gettext finds bundled .mo files
export TEXTDOMAIN=cleanmic
export TEXTDOMAINDIR="$HERE/usr/share/locale"

exec "$HERE/usr/bin/cleanmic" "$@"
APPRUN_EOF

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

# ── Step 8: Build AppImage ───────────────────────────────────────────────────
info "Building AppImage..."

# appimagetool requires FUSE to run as an AppImage itself.
# If FUSE is not available, try extracting and running directly.
if "$APPIMAGETOOL" --version &>/dev/null 2>&1; then
    ARCH=x86_64 "$APPIMAGETOOL" "$APPDIR" "$OUTPUT"
else
    warn "appimagetool cannot run directly (FUSE may be missing)."
    warn "Trying --appimage-extract-and-run workaround..."
    ARCH=x86_64 "$APPIMAGETOOL" --appimage-extract-and-run "$APPDIR" "$OUTPUT"
fi

info "AppImage created: $OUTPUT"
info "Size: $(du -h "$OUTPUT" | cut -f1)"
info ""
info "To run:  chmod +x $OUTPUT && ./$OUTPUT"
