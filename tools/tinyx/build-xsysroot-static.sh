#!/usr/bin/env bash
# Build static (.a) libraries for a fully static Xfbdev link.
#
# Alpine v3.17 ships libXfont.a but only .so for libfontenc, freetype, libpng,
# zlib, bzip2 and brotli.  This script cross-compiles those deps into $SYSROOT.
#
# Usage:
#   ARCH=x86_64 SYSROOT=tools/tinyx/sysroot-x86_64 tools/tinyx/build-xsysroot-static.sh
#   tools/tinyx/build-xsysroot-static.sh clean
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT=$PWD

ARCH=${ARCH:-x86_64}
SYSROOT=${SYSROOT:-tools/tinyx/sysroot-$ARCH}
STAMP="$SYSROOT/.static-libs-ready"
TOOLCHAIN=${TOOLCHAIN:-target/$ARCH/$ARCH-linux-musl-cross}
JOBS=${JOBS:-$(nproc 2>/dev/null || echo 4)}
HOST=$ARCH-linux-musl
CROSS=$ROOT/$TOOLCHAIN/bin/$HOST-
WORKDIR=tools/tinyx/deps-build-$ARCH
SRCDIR=tools/tinyx/deps-src

if [ "${1:-}" = "clean" ]; then
    echo "== cleaning static TinyX deps =="
    rm -rf "$WORKDIR" "$STAMP"
    rm -rf "$SYSROOT/usr/lib/shared-hidden" 2>/dev/null || true
    exit 0
fi

if [ ! -f "$SYSROOT/usr/lib/pkgconfig/xfont.pc" ]; then
    echo "error: run fetch-xsysroot.sh first (missing $SYSROOT/usr/lib/pkgconfig/xfont.pc)" >&2
    exit 1
fi
if [ ! -x "${CROSS}gcc" ]; then
    echo "error: musl cross compiler not found at ${CROSS}gcc" >&2
    exit 1
fi
if [ -f "$STAMP" ]; then
    echo "TinyX static libs already built: $SYSROOT"
    exit 0
fi

SYSROOT="$(cd "$SYSROOT" && pwd)"
mkdir -p "$WORKDIR" "$SRCDIR"

export CC="${CROSS}gcc"
export AR="${CROSS}ar"
export RANLIB="${CROSS}ranlib"
export STRIP="${CROSS}strip"
export PKG_CONFIG_SYSROOT_DIR="$SYSROOT"
export PKG_CONFIG_LIBDIR="$SYSROOT/usr/lib/pkgconfig:$SYSROOT/usr/share/pkgconfig"
export CPPFLAGS="-I$SYSROOT/usr/include ${CPPFLAGS:-}"
export CFLAGS="-O2 -fPIC ${CFLAGS:-}"
# Do not export LDFLAGS globally — musl sysroot paths break host tools (cmake, etc.).
DEP_LDFLAGS="-L$SYSROOT/usr/lib -L$SYSROOT/lib"

fetch() {
    local out="$1" url="$2"
    if [ ! -f "$SRCDIR/$out" ]; then
        echo "== downloading $out =="
        curl -fsSL -o "$SRCDIR/$out" "$url"
    fi
}

install_a() {
    local name="$1"
    shift
    local f
    for f in "$@"; do
        cp -f "$f" "$SYSROOT/usr/lib/$name"
        "${RANLIB}" "$SYSROOT/usr/lib/$name" 2>/dev/null || true
    done
}

echo "== building static zlib =="
fetch zlib-1.2.13.tar.gz https://github.com/madler/zlib/releases/download/v1.2.13/zlib-1.2.13.tar.gz
rm -rf "$WORKDIR/zlib-1.2.13"
tar -C "$WORKDIR" -xf "$SRCDIR/zlib-1.2.13.tar.gz"
( cd "$WORKDIR/zlib-1.2.13" && \
    CHOST="$HOST" ./configure --prefix=/usr --static && \
    make -j"$JOBS" libz.a && \
    make install DESTDIR="$SYSROOT" )
cp -f "$WORKDIR/zlib-1.2.13/libz.a" "$SYSROOT/lib/libz.a"

echo "== building static bzip2 =="
fetch bzip2-1.0.8.tar.gz https://sourceware.org/pub/bzip2/bzip2-1.0.8.tar.gz
rm -rf "$WORKDIR/bzip2-1.0.8"
tar -C "$WORKDIR" -xf "$SRCDIR/bzip2-1.0.8.tar.gz"
( cd "$WORKDIR/bzip2-1.0.8" && \
    make CC="$CC" AR="$AR" RANLIB="$RANLIB" CFLAGS="$CFLAGS" libbz2.a )
install_a libbz2.a "$WORKDIR/bzip2-1.0.8/libbz2.a"

echo "== building static brotli =="
fetch brotli-1.0.9.tar.gz https://github.com/google/brotli/archive/v1.0.9.tar.gz
rm -rf "$WORKDIR/brotli-1.0.9"
tar -C "$WORKDIR" -xf "$SRCDIR/brotli-1.0.9.tar.gz"
cmake -S "$WORKDIR/brotli-1.0.9" -B "$WORKDIR/brotli-build" \
    -DCMAKE_SYSTEM_NAME=Linux \
    -DCMAKE_C_COMPILER="$CC" \
    -DCMAKE_AR="$AR" \
    -DCMAKE_RANLIB="$RANLIB" \
    -DCMAKE_INSTALL_PREFIX=/usr \
    -DCMAKE_INSTALL_LIBDIR=lib \
    -DBUILD_SHARED_LIBS=OFF \
    -DBROTLI_BUILD_TOOLS=OFF \
    -DBROTLI_DISABLE_TESTS=ON
env -u LD_LIBRARY_PATH -u LDFLAGS cmake --build "$WORKDIR/brotli-build" -j"$JOBS" \
    --target brotlicommon-static brotlidec-static
install_a libbrotlicommon.a "$WORKDIR/brotli-build/libbrotlicommon-static.a"
install_a libbrotlidec.a "$WORKDIR/brotli-build/libbrotlidec-static.a"

echo "== building static libpng =="
fetch libpng-1.6.44.tar.xz https://download.sourceforge.net/libpng/libpng-1.6.44.tar.xz
rm -rf "$WORKDIR/libpng-1.6.44"
tar -C "$WORKDIR" -xf "$SRCDIR/libpng-1.6.44.tar.xz"
( cd "$WORKDIR/libpng-1.6.44" && \
    CPPFLAGS="-I$SYSROOT/usr/include" \
    LDFLAGS="$DEP_LDFLAGS" \
    ./configure --host="$HOST" --prefix=/usr --disable-shared --enable-static && \
    make -j"$JOBS" LDFLAGS="$DEP_LDFLAGS" && \
    make install DESTDIR="$SYSROOT" )

echo "== building static freetype =="
fetch freetype-2.12.1.tar.xz https://download.sourceforge.net/freetype/freetype-2.12.1.tar.xz
rm -rf "$WORKDIR/freetype-2.12.1"
tar -C "$WORKDIR" -xf "$SRCDIR/freetype-2.12.1.tar.xz"
( cd "$WORKDIR/freetype-2.12.1" && \
    ./configure --host="$HOST" --prefix=/usr --disable-shared --enable-static \
        --with-zlib=yes --with-bzip2=yes --with-png=yes --with-brotli=yes \
        PKG_CONFIG="$ROOT/tools/tinyx/pkg-config-static.sh" \
        PKG_CONFIG_PATH="$PKG_CONFIG_LIBDIR" \
        CPPFLAGS="-I$SYSROOT/usr/include" \
        LDFLAGS="$DEP_LDFLAGS" && \
    make -j"$JOBS" LDFLAGS="$DEP_LDFLAGS" && \
    make install DESTDIR="$SYSROOT" )

echo "== building static libfontenc =="
fetch libfontenc-1.1.6.tar.xz https://www.x.org/pub/individual/lib/libfontenc-1.1.6.tar.xz
rm -rf "$WORKDIR/libfontenc-1.1.6"
tar -C "$WORKDIR" -xf "$SRCDIR/libfontenc-1.1.6.tar.xz"
( cd "$WORKDIR/libfontenc-1.1.6" && \
    autoreconf -fi && \
    PKG_CONFIG="$ROOT/tools/tinyx/pkg-config-static.sh" \
    PKG_CONFIG_PATH="$PKG_CONFIG_LIBDIR" \
    PKG_CONFIG_SYSROOT_DIR="$SYSROOT" \
    CPPFLAGS="-I$SYSROOT/usr/include" \
    LDFLAGS="$DEP_LDFLAGS" \
    ./configure --host="$HOST" --prefix=/usr --disable-shared --enable-static \
        ac_cv_func_malloc_0_nonnull=yes ac_cv_func_realloc_0_nonnull=yes \
        ZLIB_LIBS="-L$SYSROOT/usr/lib -L$SYSROOT/lib -lz" && \
    make -j"$JOBS" LDFLAGS="$DEP_LDFLAGS" && \
    make install DESTDIR="$SYSROOT" )

echo "== hiding shared libs (force static link) =="
HIDDEN="$SYSROOT/usr/lib/shared-hidden"
mkdir -p "$HIDDEN" "$SYSROOT/lib/shared-hidden"
shopt -s nullglob
for libdir in "$SYSROOT/usr/lib" "$SYSROOT/lib"; do
    hidden="$libdir/shared-hidden"
    for pat in libfontenc libfreetype libpng16 libpng libbz2 libbrotlidec libbrotlicommon libz libXfont; do
        for f in "$libdir"/${pat}.so*; do
            mv -f "$f" "$hidden/"
        done
    done
done
shopt -u nullglob

touch "$STAMP"
echo "static sysroot ready: $SYSROOT"
ls -la "$SYSROOT/usr/lib/"*.a "$SYSROOT/lib/libz.a" 2>/dev/null | awk '{print $NF}'
