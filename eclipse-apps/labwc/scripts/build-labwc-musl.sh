#!/usr/bin/env bash
# Compila labwc con musl-gcc (cross file musl-target.txt) para Eclipse OS / entornos linux-musl.
# Requisitos: musl-tools, meson, ninja, cmake, pkg-config, python3, hwdata (hwdata.pc), build-essential.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
PREFIX="$ROOT/host-tools/prefix"
SCANNER="$PREFIX/bin/wayland-scanner"

if [[ ! -x "$SCANNER" ]]; then
	echo "Construyendo wayland-scanner 1.25 en $PREFIX (el del sistema suele ser < 1.25)..."
	rm -rf "$ROOT/build-wayland-host"
	meson setup "$ROOT/build-wayland-host" "$ROOT/subprojects/wayland-1.25.0" \
		-Dscanner=true -Dlibraries=false -Dtests=false -Ddocumentation=false \
		--prefix="$PREFIX" --buildtype=release
	ninja -C "$ROOT/build-wayland-host"
	ninja -C "$ROOT/build-wayland-host" install
fi

rm -rf build
# prefix=/ + libdir=lib + bindir=usr/bin → `meson install --destdir=.../eclipse-os-build`
# genera eclipse-os-build/lib/*.so y eclipse-os-build/usr/bin/labwc (rutas tipo rootfs Eclipse).
meson setup build \
	--prefix=/ \
	--libdir=lib \
	--bindir=usr/bin \
	-Ddefault_library=static \
	-Dsvg=disabled -Dnls=disabled -Dxwayland=disabled -Dman-pages=disabled \
	-Dwlroots:examples=false -Dwlroots:renderers=gles2 -Dwlroots:backends=drm,libinput \
	-Dlibinput:tests=false -Dlibinput:documentation=false -Dlibinput:libwacom=false \
	-Dlibinput:debug-gui=false -Dlibinput:mtdev=false \
	-Dlibevdev:tests=disabled -Dlibevdev:documentation=disabled \
	-Dpixman:tests=disabled -Dpixman:demos=disabled \
	-Dcairo:tests=disabled \
	-Dfreetype2:harfbuzz=disabled -Dharfbuzz:freetype=disabled -Dharfbuzz:tests=disabled \
	-Dharfbuzz:docs=disabled -Dharfbuzz:icu=disabled -Dfontconfig:tests=disabled \
	-Dglib:tests=false -Dglib:gtk_doc=false -Dglib:man=false -Dglib:selinux=disabled \
	-Dglib:libmount=disabled -Dglib:sysprof=disabled \
	-Dpango:documentation=false -Dpango:introspection=disabled \
	-Dpango:build-testsuite=false -Dpango:build-examples=false -Dpango:xft=disabled \
	-Dpango:libthai=disabled \
	-Dgvdb:tests=false \
	-Dwrap_mode=forcefallback \
	-Dwayland:scanner=false \
	--cross-file=musl-target.txt \
	--native-file=native.txt

ninja -C build
echo "Binario: $ROOT/build/labwc"

# Instalación opcional al staging Eclipse (ruta absoluta). Ej. desde la raíz del repo:
#   ECLIPSE_MESON_DESTDIR="$(pwd)/eclipse-os-build" ./eclipse-apps/labwc/scripts/build-labwc-musl.sh
if [[ -n "${ECLIPSE_MESON_DESTDIR:-}" ]]; then
	echo "meson install → DESTDIR=$ECLIPSE_MESON_DESTDIR (lib/ + usr/bin/)"
	meson install -C build --destdir="$ECLIPSE_MESON_DESTDIR" --no-rebuild
fi
