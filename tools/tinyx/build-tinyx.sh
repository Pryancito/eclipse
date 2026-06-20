#!/usr/bin/env bash
#
# Cross-compile the TinyX framebuffer X server (Xfbdev) for Eclipse OS (zCore).
#
# TinyX is the kdrive-based minimal X server resurrected by Tiny Core Linux
# (https://github.com/tinycorelinux/tinyx).  Unlike full Xorg — which links
# dozens of .so files and needs udev/DRM/glamor — Xfbdev is a single static
# binary that draws straight to /dev/fb0 and reads /dev/input/mice + the VT,
# i.e. exactly the kernel surface that `tools/x11-bench` already validates and
# `docs/README-xorg.md` describes.  That makes it the lightweight X server for
# Eclipse OS.
#
# The TinyX source itself is vendored under tools/tinyx/src.  Its handful of
# external X *development* libraries (protocol headers, xtrans, libXfont v1,
# libfontenc) are NOT vendored — point this script at a sysroot that provides
# their pkg-config files via $XSYSROOT (see tools/tinyx/README.md).
#
# Usage:
#   tools/tinyx/build-tinyx.sh                 # build + stage into the rootfs
#   XSYSROOT=/path/to/x-sysroot ARCH=x86_64 tools/tinyx/build-tinyx.sh
#   tools/tinyx/build-tinyx.sh clean
#
# Env knobs (all optional except XSYSROOT):
#   ARCH        Target arch                (default: x86_64)
#   TOOLCHAIN   musl cross toolchain dir   (default: target/$ARCH/$ARCH-linux-musl-cross)
#   XSYSROOT    Sysroot with the X dev libs' pkg-config files (REQUIRED)
#   ROOTFS      Where to stage Xfbdev      (default: rootfs/$ARCH)
#   JOBS        Parallel make jobs         (default: nproc)
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT=$PWD
SRC=tools/tinyx/src

ARCH=${ARCH:-x86_64}
JOBS=${JOBS:-$(nproc 2>/dev/null || echo 4)}
TOOLCHAIN=${TOOLCHAIN:-target/$ARCH/$ARCH-linux-musl-cross}
ROOTFS=${ROOTFS:-rootfs/$ARCH}
HOST=$ARCH-linux-musl
CROSS=$ROOT/$TOOLCHAIN/bin/$HOST-
BUILD=tools/tinyx/build-$ARCH

if [ "${1:-}" = "clean" ]; then
    echo "== cleaning =="
    rm -rf "$BUILD"
    # Drop generated autotools files from the vendored tree (scoped to $SRC so
    # we never touch other untracked files in the repo).
    git -C "$ROOT" clean -xfdq -- "$SRC" 2>/dev/null || true
    exit 0
fi

# --- sanity checks --------------------------------------------------------
if [ ! -x "${CROSS}gcc" ]; then
    echo "error: musl cross compiler not found at ${CROSS}gcc" >&2
    echo "       run 'cargo rootfs --arch $ARCH' first, or set TOOLCHAIN=." >&2
    exit 1
fi
if [ -z "${XSYSROOT:-}" ]; then
    cat >&2 <<'EOF'
error: XSYSROOT is not set.

Xfbdev needs the X protocol headers + a few dev libraries to compile.  Provide
a sysroot whose lib/pkgconfig (and share/aclocal) contain, for the target arch:

  xorgproto  xtrans  libXfont (version 1.x)  libfontenc

On a machine with an Alpine cross-sysroot you can populate it with:
  apk add --root "$XSYSROOT" --arch $ARCH \
      xorgproto-dev xtrans libxfont-dev libfontenc-dev util-macros

…or build libXfont v1 from https://www.x.org/archive/individual/lib/ .
Then re-run:  XSYSROOT=/path/to/sysroot tools/tinyx/build-tinyx.sh
EOF
    exit 1
fi

PKGCFG_DIR="$XSYSROOT/usr/lib/pkgconfig:$XSYSROOT/usr/share/pkgconfig:$XSYSROOT/lib/pkgconfig:$XSYSROOT/usr/lib/$HOST/pkgconfig"

export CC="${CROSS}gcc"
export AR="${CROSS}ar"
export RANLIB="${CROSS}ranlib"
export STRIP="${CROSS}strip"
export PKG_CONFIG_SYSROOT_DIR="$XSYSROOT"
export PKG_CONFIG_LIBDIR="$PKGCFG_DIR"
export PKG_CONFIG_PATH="$PKGCFG_DIR"
export ACLOCAL_PATH="$XSYSROOT/usr/share/aclocal:$XSYSROOT/share/aclocal"
export CPPFLAGS="-I$XSYSROOT/usr/include ${CPPFLAGS:-}"
export LDFLAGS="-L$XSYSROOT/usr/lib ${LDFLAGS:-}"

# --- configure (once) -----------------------------------------------------
echo "== generating configure (autoreconf) =="
( cd "$SRC" && [ -x configure ] || NOCONFIGURE=1 ./autogen.sh )

echo "== configuring (Xfbdev only, kdrive, no Xvesa/xdmcp) =="
rm -rf "$BUILD" && mkdir -p "$BUILD"
( cd "$BUILD" && "$ROOT/$SRC/configure" \
    --host="$HOST" \
    --prefix=/usr \
    --enable-static --disable-shared \
    --enable-kdrive \
    --enable-xfbdev \
    --disable-xvesa \
    --disable-xdmcp \
    --disable-xdm-auth-1 \
    --disable-dependency-tracking )

echo "== building Xfbdev (-j$JOBS) =="
make -C "$BUILD" -j"$JOBS"

BIN="$BUILD/kdrive/fbdev/Xfbdev"
[ -f "$BIN" ] || { echo "error: $BIN was not produced" >&2; exit 1; }
"$STRIP" "$BIN" || true

# --- stage into the rootfs ------------------------------------------------
echo "== staging into $ROOTFS/usr/bin/Xfbdev =="
mkdir -p "$ROOTFS/usr/bin"
cp -f "$BIN" "$ROOTFS/usr/bin/Xfbdev"
mkdir -p "$ROOTFS/etc/X11"
cp -f tools/tinyx/eclipse/xinitrc "$ROOTFS/etc/X11/xinitrc.tinyx" 2>/dev/null || true

echo
echo "Xfbdev built:  $BIN"
file "$BIN" 2>/dev/null || true
echo "Staged at:     $ROOTFS/usr/bin/Xfbdev"
echo "Run on Eclipse OS, e.g.:"
echo "    Xfbdev :0 -screen 1024x768 -mouse /dev/input/mice vt1 &"
echo "See tools/tinyx/README.md for fonts, input and launch details."
