#!/usr/bin/env bash
# Install minimal X bitmap fonts (fixed + cursor) into the Eclipse rootfs.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT=$PWD
ARCH=${ARCH:-x86_64}
ROOTFS=${ROOTFS:-rootfs/$ARCH}
APK=${APK:-tools/apk/apk-${ARCH}.static}
ALPINE_REPO=${ALPINE_REPO:-http://dl-cdn.alpinelinux.org/alpine/v3.17}
FONT_STAGING=tools/tinyx/font-staging-$ARCH
FONT_DIR="$ROOTFS/usr/share/fonts/X11/misc"

if [ -f "$FONT_DIR/fonts.dir" ]; then
    echo "X fonts already installed under $FONT_DIR"
    exit 0
fi

if [ ! -x "$APK" ]; then
    echo "warning: $APK missing; skipping X fonts" >&2
    exit 0
fi

echo "== fetching X bitmap fonts =="
rm -rf "$FONT_STAGING"
mkdir -p "$FONT_STAGING/etc/apk/keys"
cp prebuilt/alpine-apk-keys/*.pub "$FONT_STAGING/etc/apk/keys/" 2>/dev/null || true
printf '%s/main\n' "$ALPINE_REPO" > "$FONT_STAGING/etc/apk/repositories"
"$APK" --root "$FONT_STAGING" --arch "$ARCH" --initdb --usermode add --no-cache \
    font-misc-misc font-cursor-misc mkfontdir 2>/dev/null || \
"$APK" --root "$FONT_STAGING" --arch "$ARCH" --initdb --usermode add --no-cache \
    font-misc-misc font-cursor-misc

mkdir -p "$FONT_DIR"
if [ -d "$FONT_STAGING/usr/share/fonts/misc" ]; then
    cp -a "$FONT_STAGING/usr/share/fonts/misc/." "$FONT_DIR/"
elif [ -d "$FONT_STAGING/usr/share/fonts/X11/misc" ]; then
    cp -a "$FONT_STAGING/usr/share/fonts/X11/misc/." "$FONT_DIR/"
else
    echo "warning: font packages did not install expected paths" >&2
    exit 0
fi

# fonts.dir/fonts.alias from Alpine packages are enough for libXfont.
echo "X fonts installed: $FONT_DIR ($(find "$FONT_DIR" -type f | wc -l) files)"
