#!/bin/bash
# AppRun -- entry point for CleanMic AppImage
HERE="$(dirname "$(readlink -f "$0")")"

# APPDIR is set by the AppImage runtime before AppRun is called.
# Export it explicitly so child processes can find bundled libraries.
export APPDIR="${APPDIR:-$HERE}"

# Add bundled libraries to search path so dlopen() can find them.
export LD_LIBRARY_PATH="$HERE/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

# D-01/15.4-03: with GTK4 + libadwaita bundled (CLEANMIC_BUNDLE_GTK=1, the
# default in scripts/build-appimage.sh), linuxdeploy-plugin-gtk installs the
# bundled GSettings schemas, GObject-Introspection typelibs, and gdk-pixbuf
# loaders at these conventional paths under usr/. Each export below is
# guarded on that bundled path actually existing, so this script behaves
# exactly as it did before 15.4-03 when CLEANMIC_BUNDLE_GTK=0 (host-GTK
# build -- none of these paths exist, none of these exports fire).
#
# Deliberately NOT exported here: GDK_BACKEND, GTK_THEME. The plugin's own
# apprun-hooks/linuxdeploy-plugin-gtk.sh would force GDK_BACKEND=x11
# (breaking Wayland-native behavior) and a fixed GTK_THEME=Adwaita:<variant>
# (overriding libadwaita's own light/dark styling) -- scripts/build-appimage.sh
# deletes that hooks directory at build time specifically so it can never be
# sourced, and this script never reintroduces either variable.
if [ -d "$HERE/usr/share/glib-2.0/schemas" ]; then
    export GSETTINGS_SCHEMA_DIR="$HERE/usr/share/glib-2.0/schemas"
fi
if [ -d "$HERE/usr/lib/girepository-1.0" ]; then
    export GI_TYPELIB_PATH="$HERE/usr/lib/girepository-1.0"
fi
if [ -f "$HERE/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache" ]; then
    export GDK_PIXBUF_MODULE_FILE="$HERE/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"
fi

# D-10: libpipewire-0.3.so.0 ships as a FALLBACK ONLY, in
# usr/lib/pipewire-fallback/ -- never in usr/lib itself. The official
# AppImage excludelist singles PipeWire out (like libjack) because a
# bundled client's ABI must match whatever pipewire.service/wireplumber
# is actually running on the host; a real user's own system copy must
# always be the one that resolves. So the fallback dir is APPENDED (never
# prepended) to LD_LIBRARY_PATH, and only when `ldd`, run with the exact
# environment the real launch below will use, reports the soname
# unresolved. If `ldd` itself isn't on the host, the fallback dir is not
# added either -- the host's own copy (if any) wins by default in that
# case too, same as if this whole block did not exist.
if command -v ldd >/dev/null 2>&1; then
    if ldd "$HERE/usr/bin/cleanmic" 2>/dev/null | grep -q 'libpipewire-0\.3\.so\.0 => not found'; then
        export LD_LIBRARY_PATH="$LD_LIBRARY_PATH:$HERE/usr/lib/pipewire-fallback"
        export CLEANMIC_PIPEWIRE_FALLBACK=1
    fi
fi

# Constrain FFTW/OpenBLAS thread pools to prevent Khip from saturating all cores.
export OPENBLAS_NUM_THREADS=1
export OMP_NUM_THREADS=1
export FFTW_NUM_THREADS=1

export PATH="$HERE/usr/bin:$PATH"
export XDG_DATA_DIRS="$HERE/usr/share${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"

# Set up locale search path so gettext finds bundled .mo files
export TEXTDOMAIN=cleanmic
export TEXTDOMAINDIR="$HERE/usr/share/locale"

# Pre-flight: abort launch with a clear message if a required host library
# (e.g. libadwaita-1.so.0) is missing, instead of a cryptic linker crash.
# Inherits the LD_LIBRARY_PATH/env exported above so ldd sees exactly what
# the real launch would resolve. Fails open (exits 0 fast) when nothing is
# missing, so the happy path pays only this one extra invocation.
#
# Exit code 3 is the ONE sentinel meaning "a required library is genuinely
# missing" -- only that code aborts launch. Any OTHER nonzero exit means the
# helper itself failed to run (crash, bad shebang, lost +x, etc.), which is
# unrelated to a missing library; fail OPEN in that case so a bug in the
# ~90-line helper can never brick a healthy launch.
"$HERE/usr/bin/cleanmic-preflight" "$HERE/usr/bin/cleanmic"
PREFLIGHT_STATUS=$?
if [ "$PREFLIGHT_STATUS" -eq 3 ]; then
    exit 1
elif [ "$PREFLIGHT_STATUS" -ne 0 ]; then
    echo "cleanmic-preflight: unexpected exit status $PREFLIGHT_STATUS -- ignoring and launching anyway (fail-open)" >&2
fi

exec "$HERE/usr/bin/cleanmic" "$@"
