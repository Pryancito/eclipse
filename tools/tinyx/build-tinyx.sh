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
#   XSYSROOT    Sysroot with X dev libs (default: tools/tinyx/sysroot-$ARCH)
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
XSYSROOT=${XSYSROOT:-tools/tinyx/sysroot-$ARCH}
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
if [ ! -f "$XSYSROOT/usr/lib/pkgconfig/xfont.pc" ]; then
    echo "== populating TinyX sysroot =="
    ARCH="$ARCH" SYSROOT="$XSYSROOT" "$ROOT/tools/tinyx/fetch-xsysroot.sh"
fi
XSYSROOT="$(cd "$XSYSROOT" && pwd)"

echo "== building static X dependencies into sysroot =="
ARCH="$ARCH" SYSROOT="$XSYSROOT" TOOLCHAIN="$TOOLCHAIN" JOBS="$JOBS" \
    "$ROOT/tools/tinyx/build-xsysroot-static.sh"

PKGCFG_DIR="$XSYSROOT/usr/lib/pkgconfig:$XSYSROOT/usr/share/pkgconfig:$XSYSROOT/lib/pkgconfig:$XSYSROOT/usr/lib/$HOST/pkgconfig"

export CC="${CROSS}gcc"
export AR="${CROSS}ar"
export RANLIB="${CROSS}ranlib"
export STRIP="${CROSS}strip"
export PKG_CONFIG_SYSROOT_DIR="$XSYSROOT"
export PKG_CONFIG_LIBDIR="$PKGCFG_DIR"
export PKG_CONFIG_PATH="$PKGCFG_DIR"
export PKG_CONFIG="$ROOT/tools/tinyx/pkg-config-static.sh"
export ACLOCAL_PATH="$XSYSROOT/usr/share/aclocal:$XSYSROOT/share/aclocal"
export CPPFLAGS="-I$XSYSROOT/usr/include ${CPPFLAGS:-}"
export LDFLAGS="-static -L$XSYSROOT/usr/lib -L$XSYSROOT/lib ${LDFLAGS:-}"

# --- configure (once) -----------------------------------------------------
echo "== generating configure (autoreconf) =="
( cd "$SRC" && [ -x configure ] || NOCONFIGURE=1 ./autogen.sh )

echo "== configuring (Xfbdev only, kdrive, no Xvesa/xdmcp) =="
NEED_CONFIGURE=1
if [ -f "$BUILD/Makefile" ] && [ -f "$BUILD/kdrive/fbdev/Xfbdev" ]; then
    if file "$BUILD/kdrive/fbdev/Xfbdev" 2>/dev/null | grep -q 'statically linked'; then
        NEED_CONFIGURE=0
    fi
fi
if [ "$NEED_CONFIGURE" = 1 ]; then
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
        --disable-install-setuid \
        --disable-dependency-tracking )
fi

echo "== building Xfbdev (-j$JOBS) =="
make -C "$BUILD" -j"$JOBS"

BIN="$BUILD/kdrive/fbdev/Xfbdev"
[ -f "$BIN" ] || { echo "error: $BIN was not produced" >&2; exit 1; }

# libtool drops -static on the final link; relink with gcc directly.
echo "== relinking Xfbdev fully static =="
FBDEV="$BUILD/kdrive/fbdev"
"${CROSS}gcc" -static -no-pie \
    -L"$XSYSROOT/usr/lib" -L"$XSYSROOT/lib" \
    "$FBDEV/fbinit.o" "$FBDEV/libfbdev.a" \
    "$BUILD/dix/.libs/libdix.a" \
    "$BUILD/kdrive/src/libkdrive.a" \
    "$BUILD/kdrive/linux/liblinux.a" \
    "$BUILD/fb/.libs/libfb.a" \
    "$BUILD/mi/.libs/libmi.a" \
    "$BUILD/xfixes/.libs/libxfixes.a" \
    "$BUILD/Xext/.libs/libXext.a" \
    "$BUILD/dbe/.libs/libdbe.a" \
    "$BUILD/render/.libs/librender.a" \
    "$BUILD/randr/.libs/librandr.a" \
    "$BUILD/damageext/.libs/libdamageext.a" \
    "$BUILD/miext/damage/.libs/libdamage.a" \
    "$BUILD/miext/shadow/.libs/libshadow.a" \
    "$BUILD/os/.libs/libos.a" \
    "$BUILD/kdrive/src/libkdrivestubs.a" \
    -lXfont \
    "$XSYSROOT/usr/lib/libfreetype.a" \
    "$XSYSROOT/usr/lib/libbz2.a" \
    "$XSYSROOT/usr/lib/libpng16.a" \
    "$XSYSROOT/usr/lib/libbrotlidec.a" \
    "$XSYSROOT/usr/lib/libbrotlicommon.a" \
    "$XSYSROOT/usr/lib/libfontenc.a" \
    -lz -lm \
    -o "$BIN"

if ! file "$BIN" 2>/dev/null | grep -q 'statically linked'; then
    echo "error: Xfbdev relink did not produce a static binary" >&2
    file "$BIN" >&2 || true
    exit 1
fi
if "${CROSS}readelf" -d "$BIN" 2>/dev/null | grep -q NEEDED; then
    echo "error: Xfbdev is not fully static (dynamic NEEDED entries remain):" >&2
    "${CROSS}readelf" -d "$BIN" 2>/dev/null | grep NEEDED >&2 || true
    exit 1
fi
"$STRIP" "$BIN" || true

# --- stage into the rootfs ------------------------------------------------
echo "== staging into $ROOTFS/usr/bin/Xfbdev =="
mkdir -p "$ROOTFS/usr/bin" "$ROOTFS/etc/X11" "$ROOTFS/usr/share/fonts/X11/misc"
cp -f "$BIN" "$ROOTFS/usr/bin/Xfbdev"
cp -f tools/tinyx/eclipse/xinitrc "$ROOTFS/etc/X11/xinitrc.tinyx" 2>/dev/null || true
cp -f tools/tinyx/eclipse/startx "$ROOTFS/usr/bin/startx" 2>/dev/null || true
chmod +x "$ROOTFS/usr/bin/startx" 2>/dev/null || true

# Bitmap fonts required at runtime (fixed, cursor).
# Drop X runtime .so left over from older dynamic TinyX builds.
shopt -s nullglob
for f in "$ROOTFS/lib"/libXfont.so* "$ROOTFS/lib"/libfontenc.so* \
         "$ROOTFS/lib"/libfreetype.so* "$ROOTFS/lib"/libpng16.so* \
         "$ROOTFS/lib"/libbz2.so* "$ROOTFS/lib"/libbrotli*.so*
do
    rm -f "$f"
done
shopt -u nullglob

# Bitmap fonts required at runtime (fixed, cursor).
if [ -x "$ROOT/tools/tinyx/fetch-xfonts.sh" ]; then
    ARCH="$ARCH" ROOTFS="$ROOTFS" "$ROOT/tools/tinyx/fetch-xfonts.sh"
fi

echo
echo "Xfbdev built:  $BIN"
file "$BIN" 2>/dev/null || true
echo "Staged at:     $ROOTFS/usr/bin/Xfbdev"
echo "Run on Eclipse OS, e.g.:"
echo "    Xfbdev :0 -screen 1024x768 -mouse /dev/input/mice vt1 &"
echo "See tools/tinyx/README.md for fonts, input and launch details."
