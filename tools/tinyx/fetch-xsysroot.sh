#!/usr/bin/env bash
# Populate tools/tinyx/sysroot-$ARCH with X development headers/libs for cross-build.
#
# Uses Alpine v3.17 packages (libXfont 1.x) via the repo's static apk binary.
# libxfont 2.x on Alpine edge is incompatible with TinyX.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT=$PWD
ARCH=${ARCH:-x86_64}
SYSROOT=${SYSROOT:-tools/tinyx/sysroot-$ARCH}
APK=${APK:-tools/apk/apk-${ARCH}.static}
# Alpine 3.17 still ships libXfont 1.x (required by TinyX kdrive).
ALPINE_REPO=${ALPINE_REPO:-http://dl-cdn.alpinelinux.org/alpine/v3.17}

if [ ! -x "$APK" ]; then
    echo "error: $APK not found; run 'cargo rootfs --arch $ARCH' first" >&2
    exit 1
fi

if [ -f "$SYSROOT/usr/lib/pkgconfig/xfont.pc" ]; then
    echo "TinyX sysroot already populated: $SYSROOT"
    exit 0
fi

echo "== fetching TinyX X dev sysroot into $SYSROOT =="
rm -rf "$SYSROOT"
mkdir -p "$SYSROOT/etc/apk/keys"
cp prebuilt/alpine-apk-keys/*.pub "$SYSROOT/etc/apk/keys/" 2>/dev/null || true
printf '%s/main\n%s/community\n' "$ALPINE_REPO" "$ALPINE_REPO" > "$SYSROOT/etc/apk/repositories"

"$APK" --root "$SYSROOT" --arch "$ARCH" --initdb --usermode add --no-cache \
    libxfont-dev libfontenc-dev xorgproto xtrans util-macros \
    libxext-dev libxtst-dev zlib-dev

echo "sysroot ready: $SYSROOT"
