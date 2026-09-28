#!/usr/bin/env bash
#
# build-gtk-stack.sh -- Build the parts of the GTK4/libadwaita stack Ubuntu
# 22.04 (jammy, glibc 2.35) is too old for, into a self-contained prefix.
#
# WHY THIS EXISTS (D-09, 15.4-03). The catalog's own AppImageHub test runner
# is a GitHub-hosted `ubuntu-22.04` box (glibc 2.35). CleanMic's Cargo.toml
# requires the `libadwaita` crate's `v1_5` feature, which needs the
# underlying C library at >= 1.5 -- which in turn needs GTK4 >= 4.13.4.
# Ubuntu 22.04 ships libadwaita 1.1 and GTK4 way behind that floor, so
# building the release AppImage on jammy (to get glibc 2.35 into the
# binary's own GLIBC_ symbol-version ceiling) requires compiling GTK4 +
# libadwaita -- and everything jammy is too old for underneath them --
# from source, into a prefix kept separate from the system package tree.
#
# VERSION FLOORS (from GTK 4.14.5's and libadwaita 1.5.0's own meson.build
# files, read during 15.4-03 planning/execution -- see
# .planning/phases/15.4-self-contained-appimage-appimagehub-listing/
# 15.4-RESEARCH.md and 15.4-03-PLAN.md's <interfaces> block):
#   meson              >= 0.63   (jammy ships 0.61 -- too old)
#   glib               >= 2.76   (jammy ships 2.72 -- too old)
#   wayland            >= 1.21   (jammy ships 1.20 -- too old)
#   wayland-protocols  >= 1.31   (jammy ships 1.25 -- too old)
#   GTK4               >= 4.13.4 (jammy ships nowhere close -- too old)
#   libadwaita         >= 1.5    (jammy ships 1.1 -- too old)
# Everything else GTK4/libadwaita need (pango, harfbuzz, cairo, gdk-pixbuf,
# graphene, epoxy, fribidi, appstream) is either new enough already on
# jammy [ASSUMED from jammy package versions at planning time; this build
# is the actual verification] or pulled in by libadwaita's own meson
# subproject wrap (appstream -- see the "appstream" note below).
#
# SUPPLY CHAIN (T-15.4-SC). Every tarball this script downloads is pinned
# by sha256, checked with `sha256sum -c` before extraction, hard-failing on
# any mismatch. Preferred source for each hash, in order:
#   - GNOME sources (glib, GTK4, libadwaita): download.gnome.org's own
#     published *.sha256sum file, cross-verified against a fresh download
#     during 15.4-03 planning.
#   - wayland / wayland-protocols: the exact sha256sum asset GitLab's
#     Releases API lists alongside the tarball, same cross-verification.
#   - meson: upstream (mesonbuild/meson's GitHub release) publishes no
#     digest for this asset (same situation as this repo's own
#     libdeep_filter_ladspa.so pin) -- the hash below was established by
#     direct download during 15.4-03 planning and is pinned trust-on-first-
#     use; a future version bump must re-establish and re-record a fresh
#     hash, never reuse this one.
# No pip/npm/cargo installs are added by this script -- meson itself runs
# as a plain checksum-pinned tarball via `python3 meson.py`, never
# `pip install meson`, so the whole build-tool supply chain stays
# tarball-and-checksum, matching this repo's existing linuxdeploy/
# linuxdeploy-plugin-gtk/uruntime pins in scripts/build-appimage.sh.
#
# ONE EXCEPTION -- appstream (git, not a checksum-pinned tarball).
# libadwaita 1.5.0's own meson.build declares `dependency('appstream',
# fallback: ['appstream', 'appstream_dep'])`, and its
# subprojects/appstream.wrap is a `[wrap-git]` entry pointing at
# `https://github.com/ximion/appstream.git` `revision = main` -- an
# upstream, un-pinned moving branch. That wrap only actually triggers if
# jammy's own libappstream-dev (0.15 at last check) is rejected by
# libadwaita's own version requirement; if jammy's copy satisfies it, no
# git clone happens at all. Either way, this is libadwaita 1.5.0's own
# upstream choice, not something this script can pin without patching
# libadwaita's own subprojects/appstream.wrap -- documented here so it is
# never mistaken for an oversight in this script's own pinning discipline.
#
# USAGE
#   scripts/ci/build-gtk-stack.sh --print-plan
#     Print every component this script would build -- name, version,
#     download URL, pinned sha256 -- and exit 0. Makes NO network access
#     and touches NOTHING on disk. Safe to run anywhere, offline.
#   scripts/ci/build-gtk-stack.sh [--prefix DIR]
#     Actually build. Default DIR: /opt/cleanmic-gtk. Requires network
#     access (to fetch the pinned tarballs) and the apt build-dependencies
#     appimage-build.yml's build-jammy job installs (build-essential,
#     python3, ninja-build, pkg-config, the jammy -dev packages the stack
#     needs, ...).
#
# IDEMPOTENT: before building a component, checks whether DIR's own
# pkg-config files already satisfy that component's version floor (via
# PKG_CONFIG_LIBDIR scoped to DIR alone, never the host's system
# pkg-config paths) -- a warm `actions/cache` hit on DIR (keyed on this
# script's own hash, per appimage-build.yml) skips every component that is
# already built, so a cache hit is a no-op re-run.
#
# EXIT CODES: 0 ok (including --print-plan). Non-zero: download failed,
# sha256 mismatch, or the underlying meson/ninja build failed -- in every
# case `set -euo pipefail` below stops at the first failing command.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

info()  { printf '\033[1;34m==> %s\033[0m\n' "$*"; }
warn()  { printf '\033[1;33m==> %s\033[0m\n' "$*"; }
error() { printf '\033[1;31m==> %s\033[0m\n' "$*" >&2; exit 1; }

# ── meson (build tool, not a pkg-config'd library) ──────────────────────────
MESON_VERSION="1.4.0"
MESON_URL="https://github.com/mesonbuild/meson/releases/download/${MESON_VERSION}/meson-${MESON_VERSION}.tar.gz"
MESON_SHA256="8fd6630c25c27f1489a8a0392b311a60481a3c161aa699b330e25935b750138d"
MESON_SRCDIR_NAME="meson-${MESON_VERSION}"

# ── The five components this script builds, in dependency order ───────────
# Fields (pipe-separated -- none of the values contain a literal '|'):
#   name | version | pkgconfig-name | url | sha256 | meson setup args
#
# `pkgconfig-name` is what `pkg-config --atleast-version=VERSION NAME`
# checks for the idempotency skip. `meson setup args` is word-split (no
# quoting needed -- none of these values contain spaces requiring quotes).
COMPONENTS="
glib|2.78.4|glib-2.0|https://download.gnome.org/sources/glib/2.78/glib-2.78.4.tar.xz|24b8e0672dca120cc32d394bccb85844e732e04fe75d18bb0573b2dbc7548f63|-Dtests=false
wayland|1.23.0|wayland-server|https://gitlab.freedesktop.org/wayland/wayland/uploads/99dba2bcc10368401c96cacd89875a1b/wayland-1.23.0.tar.xz|05b3e1574d3e67626b5974f862f36b5b427c7ceeb965cb36a4e6c2d342e45ab2|-Ddocumentation=false -Dtests=false -Ddtd_validation=false
wayland-protocols|1.34|wayland-protocols|https://gitlab.freedesktop.org/wayland/wayland-protocols/uploads/cc1a736492a7073d794b60caf4eadd73/wayland-protocols-1.34.tar.xz|c59b27cacd85f60baf4ee5f80df5c0d15760ead6a2432b00ab7e2e0574dcafeb|-Dtests=false
gtk4|4.14.5|gtk4|https://download.gnome.org/sources/gtk/4.14/gtk-4.14.5.tar.xz|5547f2b9f006b133993e070b87c17804e051efda3913feaca1108fa2be41e24d|-Dx11-backend=true -Dwayland-backend=true -Dbroadway-backend=false -Dwin32-backend=false -Dmacos-backend=false -Dmedia-gstreamer=disabled -Dprint-cups=disabled -Dprint-cpdb=disabled -Dvulkan=disabled -Dintrospection=disabled -Ddocumentation=false -Dman-pages=false -Dbuild-demos=false -Dbuild-examples=false -Dbuild-tests=false -Dbuild-testsuite=false
libadwaita|1.5.0|libadwaita-1|https://download.gnome.org/sources/libadwaita/1.5/libadwaita-1.5.0.tar.xz|fd92287df9bb95c963654fb6e70d3e082e2bcb37b147e0e3c905567167993783|-Dintrospection=disabled -Dvapi=false -Dgtk_doc=false -Dtests=false -Dexamples=false
"

# ── --print-plan: pure, offline, no side effects ────────────────────────────
print_plan() {
    printf 'meson | %s | %s | %s\n' "$MESON_VERSION" "$MESON_URL" "$MESON_SHA256"
    local line name version pcname url sha256 mesonargs
    while IFS='|' read -r name version pcname url sha256 mesonargs; do
        [ -n "$name" ] || continue
        printf '%s | %s | %s | %s\n' "$name" "$version" "$url" "$sha256"
    done <<EOF
$COMPONENTS
EOF
}

# ── Argument parsing ─────────────────────────────────────────────────────────
PREFIX="/opt/cleanmic-gtk"
PRINT_PLAN=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --print-plan)
            PRINT_PLAN=1 ;;
        --prefix)
            shift
            PREFIX="${1:-}" ;;
        -h | --help)
            echo "usage: $0 [--print-plan] [--prefix DIR]"
            exit 0 ;;
        *)
            echo "unknown option: $1" >&2
            exit 2 ;;
    esac
    shift
done

if [ "$PRINT_PLAN" = 1 ]; then
    print_plan
    exit 0
fi

if [ -z "$PREFIX" ]; then
    error "--prefix requires a directory argument"
fi

# ── Real build from here on: needs network + a build toolchain ─────────────
for tool in curl sha256sum tar python3 ninja pkg-config; do
    command -v "$tool" >/dev/null 2>&1 || error "required tool not found: $tool (see appimage-build.yml's apt install step)"
done

WORK_DIR="$PREFIX/_work"
DL_DIR="$WORK_DIR/dl"
SRC_DIR="$WORK_DIR/src"
BUILD_DIR="$WORK_DIR/build"
mkdir -p "$PREFIX" "$DL_DIR" "$SRC_DIR" "$BUILD_DIR"

# fetch_and_verify URL SHA256 DEST -- download (if not already cached) then
# hard-verify. Never auto-redownloads on a mismatch (could be tampering or
# a stale cache) -- the operator must remove DEST and re-run.
fetch_and_verify() {
    local url="$1" sha256="$2" dest="$3"
    if [ ! -f "$dest" ]; then
        info "  Downloading $(basename "$dest")..."
        curl -fsSL -o "$dest.tmp" "$url"
        mv "$dest.tmp" "$dest"
    fi
    if ! printf '%s  %s\n' "$sha256" "$dest" | sha256sum -c --status -; then
        error "sha256 mismatch for $dest (expected $sha256).
  Either the download is corrupted/tampered, or this script's pin is stale.
  To retry cleanly: rm -f \"$dest\" && re-run this script."
    fi
    info "  Verified $(basename "$dest") (sha256 OK)"
}

# ── meson: fetch once, run straight out of its own extracted tree ─────────
MESON_TARBALL="$DL_DIR/meson-${MESON_VERSION}.tar.gz"
fetch_and_verify "$MESON_URL" "$MESON_SHA256" "$MESON_TARBALL"
if [ ! -d "$SRC_DIR/$MESON_SRCDIR_NAME" ]; then
    tar -xf "$MESON_TARBALL" -C "$SRC_DIR"
fi
MESON=(python3 "$SRC_DIR/$MESON_SRCDIR_NAME/meson.py")
info "meson ${MESON_VERSION} ready: ${MESON[*]}"

# Scope the idempotency check to THIS prefix alone -- PKG_CONFIG_LIBDIR
# (unlike PKG_CONFIG_PATH) REPLACES the default search dirs entirely, so a
# too-old system copy (e.g. jammy's own glib.pc at 2.72) can never satisfy
# it by accident.
PREFIX_PKGCONFIG_LIBDIR="$PREFIX/lib/x86_64-linux-gnu/pkgconfig:$PREFIX/lib/pkgconfig:$PREFIX/lib64/pkgconfig:$PREFIX/share/pkgconfig"

component_already_satisfied() {
    local pcname="$1" version="$2"
    PKG_CONFIG_LIBDIR="$PREFIX_PKGCONFIG_LIBDIR" PKG_CONFIG_PATH="" \
        pkg-config --atleast-version="$version" "$pcname" 2>/dev/null
}

# The real build: our own prefix's pkg-config files take priority (so GTK4
# picks up the glib/wayland we just built here, not jammy's too-old system
# copies), the host's system pkg-config paths stay reachable after that for
# everything this script does NOT build (pango, harfbuzz, cairo, gdk-pixbuf,
# graphene, epoxy, fribidi -- new enough on jammy already per this plan's
# own version-floor research).
export PKG_CONFIG_PATH="$PREFIX_PKGCONFIG_LIBDIR${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
export LD_LIBRARY_PATH="$PREFIX/lib/x86_64-linux-gnu:$PREFIX/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export PATH="$PREFIX/bin:$PATH"

build_component() {
    local name="$1" version="$2" pcname="$3" url="$4" sha256="$5"
    shift 5
    local meson_args=("$@")

    if component_already_satisfied "$pcname" "$version"; then
        info "$name >= $version already satisfied in $PREFIX -- skipping (idempotent)"
        return 0
    fi

    info "Building $name $version..."
    local tarball="$DL_DIR/$(basename "$url")"
    fetch_and_verify "$url" "$sha256" "$tarball"

    local srcdirname
    srcdirname="$(basename "$url")"
    srcdirname="${srcdirname%.tar.xz}"
    srcdirname="${srcdirname%.tar.gz}"
    local extracted="$SRC_DIR/$srcdirname"
    if [ ! -d "$extracted" ]; then
        tar -xf "$tarball" -C "$SRC_DIR"
    fi

    local builddir="$BUILD_DIR/$srcdirname-build"
    rm -rf "$builddir"
    "${MESON[@]}" setup "$builddir" "$extracted" --prefix="$PREFIX" --buildtype=release "${meson_args[@]}"
    "${MESON[@]}" compile -C "$builddir"
    "${MESON[@]}" install -C "$builddir"
    info "  $name $version installed into $PREFIX"
}

while IFS='|' read -r name version pcname url sha256 mesonargs; do
    [ -n "$name" ] || continue
    # shellcheck disable=SC2086
    build_component "$name" "$version" "$pcname" "$url" "$sha256" $mesonargs
done <<EOF
$COMPONENTS
EOF

info "GTK4/libadwaita stack ready in $PREFIX"
info "Point the AppImage build at it: PKG_CONFIG_PATH=\"$PREFIX_PKGCONFIG_LIBDIR:\$PKG_CONFIG_PATH\" LD_LIBRARY_PATH=\"$PREFIX/lib/x86_64-linux-gnu:$PREFIX/lib:\$LD_LIBRARY_PATH\""
