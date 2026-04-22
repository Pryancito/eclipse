#!/usr/bin/env bash
set -e
set -u
# Construcción de componentes C/Meson del userland para Eclipse OS (musl estático).
#
# Requisitos típicos:
#   - Toolchain:  host-toolchains/bin/x86_64-linux-musl-gcc  (u otro prefijo vía env)
#   - Sysroot:    eclipse-os-build  con include/, lib/, lib/pkgconfig/ (wlroots, wayland, …)
#   - Host:       meson, ninja, pkg-config, wayland-scanner
#
# Uso:
#   ./build-userland.sh help
#   ./build-userland.sh wlroots    # instala en $ECLIPSE_SYSROOT/usr (recomendado antes de labwc)
#   ./build-userland.sh labwc      # genera userland/labwc_v08/bld/labwc
#   ./build-userland.sh labwc-install
#   ./build-userland.sh all
#
# Variables opcionales:
#   ECLIPSE_SYSROOT       destino de includes/libs/pkg-config (defecto: ../eclipse-os-build)
#   ECLIPSE_TOOLCHAIN_DIR defecto: ../host-toolchains
#   ECLIPSE_MESON_BUILDTYPE release o debug (defecto: release)
#   ECLIPSE_LABWC_CLEAN   si está a 1, borra userland/labwc_v08/bld antes de configurar
#   ECLIPSE_WLROOTS_CLEAN idem para wlroots_src/bld-eclipse
#   ECLIPSE_LIBINPUT_CLEAN idem para libinput_src/bld-eclipse (p. ej. tras cambiar --prefix)
#   ECLIPSE_XKBCOMMON_CLEAN idem para xkbcommon_src/bld-eclipse (--prefix=/usr incrustado en la .so)

set -euo pipefail

USERLAND_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_DIR="$(cd "${USERLAND_DIR}/.." && pwd)"

ECLIPSE_SYSROOT="${ECLIPSE_SYSROOT:-$BASE_DIR/eclipse-os-build}"
ECLIPSE_TOOLCHAIN_DIR="${ECLIPSE_TOOLCHAIN_DIR:-$BASE_DIR/host-toolchains}"
ECLIPSE_MESON_BUILDTYPE="${ECLIPSE_MESON_BUILDTYPE:-release}"
ECLIPSE_MESON_STATIC_LINK="${ECLIPSE_MESON_STATIC_LINK:-1}"
default_lib="shared"
if [[ "$ECLIPSE_MESON_STATIC_LINK" == "1" ]]; then
    default_lib="static"
fi

MUSL_PREFIX="${MUSL_PREFIX:-x86_64-linux-musl}"
MUSL_GCC="$ECLIPSE_TOOLCHAIN_DIR/bin/${MUSL_PREFIX}-gcc"
MUSL_GXX="$ECLIPSE_TOOLCHAIN_DIR/bin/${MUSL_PREFIX}-g++"
MUSL_AR="$ECLIPSE_TOOLCHAIN_DIR/bin/${MUSL_PREFIX}-gcc-ar"
MUSL_NM="$ECLIPSE_TOOLCHAIN_DIR/bin/${MUSL_PREFIX}-gcc-nm"
MUSL_RANLIB="$ECLIPSE_TOOLCHAIN_DIR/bin/${MUSL_PREFIX}-gcc-ranlib"
if [[ -x "$ECLIPSE_TOOLCHAIN_DIR/bin/${MUSL_PREFIX}-strip" ]]; then
	MUSL_STRIP="$ECLIPSE_TOOLCHAIN_DIR/bin/${MUSL_PREFIX}-strip"
else
	MUSL_STRIP="strip"
fi

# Compilador musl para Meson cross: preferir wrappers aislados (userland/host-tools) para que
# no se mezcle el PKG_CONFIG del sysroot con el del host; si no existen, usar $MUSL_GCC.
CROSS_MUSL_CC="$USERLAND_DIR/host-tools/bin-toolchain-isolated/${MUSL_PREFIX}-gcc"
CROSS_MUSL_CPP="$USERLAND_DIR/host-tools/bin-toolchain-isolated/${MUSL_PREFIX}-g++"
# Los wrappers aislados solo enlazan a binutils musl (-ar/-ranlib), no a *-gcc-ar / *-gcc-ranlib.
CROSS_MUSL_AR="$USERLAND_DIR/host-tools/bin-toolchain-isolated/${MUSL_PREFIX}-ar"
CROSS_MUSL_NM="$USERLAND_DIR/host-tools/bin-toolchain-isolated/${MUSL_PREFIX}-nm"
CROSS_MUSL_RANLIB="$USERLAND_DIR/host-tools/bin-toolchain-isolated/${MUSL_PREFIX}-ranlib"
CROSS_MUSL_STRIP="$USERLAND_DIR/host-tools/bin-toolchain-isolated/${MUSL_PREFIX}-strip"
if [[ ! -x "$CROSS_MUSL_CC" ]]; then
	CROSS_MUSL_CC="$MUSL_GCC"
	CROSS_MUSL_CPP="$MUSL_GXX"
	CROSS_MUSL_AR="$MUSL_AR"
	CROSS_MUSL_NM="$MUSL_NM"
	CROSS_MUSL_RANLIB="$MUSL_RANLIB"
	CROSS_MUSL_STRIP="$MUSL_STRIP"
fi

WAYLAND_SCANNER="$USERLAND_DIR/host-tools/bin/wayland-scanner"
# Asegurar que nuestras herramientas de host estén primero en el PATH
export PATH="$USERLAND_DIR/host-tools/bin:$PATH"
if [[ ! -x "$WAYLAND_SCANNER" ]]; then
	WAYLAND_SCANNER="${WAYLAND_SCANNER:-/usr/bin/wayland-scanner}"
fi

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

info() { echo -e "${BLUE}[userland]${NC} $*"; }
ok() { echo -e "${GREEN}[userland]${NC} $*"; }
warn() { echo -e "${YELLOW}[userland]${NC} $*"; }
err() { echo -e "${RED}[userland]${NC} $*" >&2; }

require_file() {
	local p="$1"
	local msg="${2:-}"
	if [[ ! -e "$p" ]]; then
		err "No existe: $p${msg:+ ($msg)}"
		exit 1
	fi
}

require_cmd() {
	if ! command -v "$1" >/dev/null 2>&1; then
		err "Falta la orden en PATH: $1"
		exit 1
	fi
}

# Meson/pkg-config suelen grabar en el ELF el RPATH del sysroot del host (p. ej. …/eclipse-os-build/usr/lib).
# En la imagen Eclipse no existe esa ruta: el enlazador dinámico debe buscar en /usr/lib y /lib.
eclipse_fix_labwc_rpath() {
	local bin="$1"
	if [[ "${ECLIPSE_LABWC_SKIP_RPATH_PATCH:-0}" == "1" ]]; then
		info "ECLIPSE_LABWC_SKIP_RPATH_PATCH=1: no se reescribe RPATH en $bin"
		return 0
	fi
	require_cmd patchelf
	local rp="${ECLIPSE_LABWC_RUNTIME_RPATH:-/usr/lib:/lib}"
	if [[ "$rp" == "remove" || "$rp" == "none" ]]; then
		patchelf --remove-rpath "$bin" 2>/dev/null || true
		ok "labwc: sin RPATH/RUNPATH (host: LD_LIBRARY_PATH hacia el sysroot musl; en imagen Eclipse suele bastar /usr/lib sin RUNPATH si ld-musl.path está bien)"
	else
		patchelf --set-rpath "$rp" "$bin" || true
		ok "labwc: RPATH de ejecución → $rp ($bin)"
	fi
}

# Los protocolos están como submódulo del repo xfwl4, no del árbol que se esté compilando (seatd, labwc, …).
ensure_xfwl4_xfce_wayland_protocols() {
	local xfwl4_root="$USERLAND_DIR/xfwl4"
	local proto="$xfwl4_root/resources/xfce-wayland-protocols/xfce-output-management-private-v1.xml"
	if [[ -f "$proto" ]]; then
		return 0
	fi
	require_cmd git
	info "Inicializando submódulo resources/xfce-wayland-protocols en xfwl4..."
	(cd "$xfwl4_root" && git submodule update --init --recursive resources/xfce-wayland-protocols)
}

check_toolchain() {
	require_file "$MUSL_GCC" "ajusta ECLIPSE_TOOLCHAIN_DIR o MUSL_PREFIX"
	require_file "$MUSL_GXX"
	require_file "$MUSL_AR"
}

check_sysroot() {
	if [[ ! -d "$ECLIPSE_SYSROOT/include" && ! -d "$ECLIPSE_SYSROOT/usr/include" ]]; then
		warn "Sysroot sin include/ visible: $ECLIPSE_SYSROOT (¿staging incompleto?)"
	fi
	mkdir -p "$ECLIPSE_SYSROOT/lib/pkgconfig" "$ECLIPSE_SYSROOT/usr/lib/pkgconfig" \
		"$ECLIPSE_SYSROOT/share/pkgconfig" "$ECLIPSE_SYSROOT/usr/share/pkgconfig" \
		"$ECLIPSE_SYSROOT/usr/bin" "$ECLIPSE_SYSROOT/usr/share/hwdata"
	ensure_compiler_libs
	ensure_musl_interpreter
}

ensure_compiler_libs() {
	check_toolchain
	local dest="$ECLIPSE_SYSROOT/usr/lib"
	mkdir -p "$dest"

	# Toolchain libs: libstdc++.so.6 y libgcc_s.so.1
	local tc_lib_dir="$ECLIPSE_TOOLCHAIN_DIR/lib"

	for lib in libstdc++.so.6 libgcc_s.so.1; do
		if [[ ! -f "$dest/$lib" ]]; then
			if [[ -f "$tc_lib_dir/$lib" ]]; then
				info "Copiando $lib al sysroot..."
				cp -a "$tc_lib_dir/$lib"* "$dest/"
			else
				warn "No se encontró $lib en $tc_lib_dir (necesario para Mesa/Gallium)"
			fi
		fi
	done
}

# El cargador ELF del kernel de Eclipse OS aún no sigue symlinks para el intérprete.
# Convertimos ld-musl-x86_64.so.1 en un hardlink (o copia) de libc.so.
ensure_musl_interpreter() {
	local libdir="$ECLIPSE_SYSROOT/lib"
	local ld_musl="$libdir/ld-musl-x86_64.so.1"
	local libc="$libdir/libc.so"

	if [[ -L "$ld_musl" ]]; then
		info "Convirtiendo symlink de intérprete musl en hardlink/copia..."
		rm -f "$ld_musl"
		ln "$libc" "$ld_musl" 2>/dev/null || cp "$libc" "$ld_musl"
	fi
}

# hwdata: wlroots backend DRM referencia pnp.ids vía pkg-config del sysroot.
patch_sysroot_extras() {
	local pnp="$ECLIPSE_SYSROOT/usr/share/hwdata/pnp.ids"
	if [[ ! -f "$pnp" ]] && [[ -f /usr/share/hwdata/pnp.ids ]]; then
		cp -a /usr/share/hwdata/pnp.ids "$pnp"
		info "Copiado pnp.ids -> $pnp (wlroots / DRM)"
	fi
}

# wlroots 0.18 es el subproyecto declarado en labwc_v08/subprojects/wlroots.wrap.
# Compilamos wayland nativo (solo host) una vez bajo userland/.eclipse-builddeps/.
# Varios .wrap de labwc redirigen a wlroots/subprojects/*.wrap; hace falta el árbol wlroots
# antes de que Meson resuelva pixman, libdrm, etc.
ensure_wlroots_subproject() {
	local root="$USERLAND_DIR/labwc_v08"
	local wr="$root/subprojects/wlroots"
	if [[ -f "$wr/meson.build" ]]; then
		return 0
	fi
	require_cmd git
	info "Clonando wlroots 0.18 (subproyecto labwc_v08, requerido por wrap-redirect)…"
	rm -rf "$wr"
	git clone --depth 1 --branch 0.18 https://gitlab.freedesktop.org/wlroots/wlroots.git "$wr"
}

# libgbm.a del sysroot a veces es un stub mínimo; wlroots 0.20 enlaza gbm_bo_get_fd_for_plane.
ensure_gbm_stub() {
	check_toolchain
	check_sysroot
	local stub_dir="$USERLAND_DIR/gbm_stub"
	local out_lib="$ECLIPSE_SYSROOT/lib/libgbm.a"
	require_file "$stub_dir/gbm.c"
	require_file "$stub_dir/gbm.h"
	mkdir -p "$(dirname "$out_lib")"
	local tmp
	tmp="$(mktemp -d)"
	# El musl-gcc del prefijo suele no traer libc en -isysroot; los headers están en ECLIPSE_SYSROOT.
	"$MUSL_GCC" -c -o "$tmp/gbm.o" \
		-isystem "$ECLIPSE_SYSROOT/include" \
		-isystem "$ECLIPSE_SYSROOT/usr/include" \
		-I"$stub_dir" "$stub_dir/gbm.c"
	"$MUSL_AR" rcs "$out_lib" "$tmp/gbm.o"
	rm -rf "$tmp"
}

# seatd con -Dlibseat-builtin=enabled mete seatd/server.c en libseat.a y choca con labwc/src/server.c.
ensure_libseat_no_builtin() {
	local lib="$ECLIPSE_SYSROOT/lib/libseat.a"
	if [[ ! -f "$lib" ]]; then
		return 0
	fi
	if ! ar t "$lib" | grep -q '^seatd_server\.c\.o$'; then
		return 0
	fi
	require_cmd meson
	require_cmd ninja
	local root="$USERLAND_DIR/seatd_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld/eclipse-rebuild-labwc"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	info "Reconstruyendo libseat sin seatd embebido (evita server_init/server_finish duplicados con labwc)…"
	write_meson_cross "$cross"
	rm -rf "$bld"
	meson setup "$bld" "$root" --cross-file="$cross" \
		--prefix="$ECLIPSE_SYSROOT" \
		--libdir=lib \
		"-Dbuildtype=$ECLIPSE_MESON_BUILDTYPE" \
		-Ddefault_library=${default_lib:-shared} \
		-Dlibseat-builtin=disabled \
		-Dserver=enabled \
		-Dlibseat-logind=disabled \
		-Dman-pages=disabled
	ninja -C "$bld"
	DESTDIR="" ninja -C "$bld" install
	ok "libseat reinstalado (libseat-builtin=disabled)."
}

build_libdisplay_info() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/libdisplay-info_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo libdisplay-info..."
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared}
	ninja -C "$bld" install
}

build_libliftoff() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/labwc_v08/subprojects/libliftoff"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo libliftoff..."
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared}
	ninja -C "$bld" install
}

build_seatd() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/seatd_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo seatd..."
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Dlibseat-builtin=disabled -Dserver=enabled -Dman-pages=disabled
	ninja -C "$bld" install
}

ensure_native_wayland_scanner() {
	local tag="${ECLIPSE_WAYLAND_TAG:-1.25.0}"
	local prefix="$USERLAND_DIR/.eclipse-builddeps/wayland-host-$tag"
	local scanner="$prefix/bin/wayland-scanner"
	if [[ -x "$scanner" ]]; then
	export WAYLAND_SCANNER="$scanner"
	export PATH="$(dirname "$scanner"):$PATH"
	local stub_pc="$USERLAND_DIR/.eclipse-builddeps/pkgconfig-wayland-scanner-only"
	mkdir -p "$stub_pc"
	cat >"$stub_pc/wayland-scanner.pc" <<PC
prefix=/usr
exec_prefix=\${prefix}
bindir=\${exec_prefix}/bin
includedir=\${exec_prefix}/include
datarootdir=\${prefix}/share
pkgdatadir=\${datarootdir}/wayland
wayland_scanner=wayland-scanner
Name: Wayland Scanner
Description: Wayland scanner (Eclipse build stub)
Version: $tag
PC
	export ECLIPSE_WAYLAND_SCANNER_PC_DIR="$stub_pc"
	info "wayland-scanner $tag (cache): $scanner"
	return 0
	fi
	require_cmd git
	require_cmd meson
	require_cmd ninja
	local src="$USERLAND_DIR/.eclipse-builddeps/wayland-$tag-src"
	info "Compilando wayland-scanner $tag (nativo, una vez) → $prefix …"
	rm -rf "$src"
	mkdir -p "$(dirname "$src")"
	git clone --depth 1 --branch "$tag" https://gitlab.freedesktop.org/wayland/wayland.git "$src"
	# Compilación **nativa** glibc: no usar el musl exportado al final de este script.
	(
		unset CC CXX CPP AR NM RANLIB LD
		unset PKG_CONFIG_PATH PKG_CONFIG_LIBDIR PKG_CONFIG_SYSROOT_DIR
		meson setup "$src/build-native" "$src" \
			--buildtype=release \
			--prefix="$prefix" \
			-Ddocumentation=false \
			-Dtests=false \
			-Ddefault_library=${default_lib:-shared}
		ninja -C "$src/build-native"
		ninja -C "$src/build-native" install
	)
	export WAYLAND_SCANNER="$scanner"
	export PATH="$(dirname "$scanner"):$PATH"
	local stub_pc="$USERLAND_DIR/.eclipse-builddeps/pkgconfig-wayland-scanner-only"
	mkdir -p "$stub_pc"
	cat >"$stub_pc/wayland-scanner.pc" <<PC
prefix=/usr
exec_prefix=\${prefix}
bindir=\${exec_prefix}/bin
includedir=\${exec_prefix}/include
datarootdir=\${prefix}/share
pkgdatadir=\${datarootdir}/wayland
wayland_scanner=wayland-scanner
Name: Wayland Scanner
Description: Wayland scanner (Eclipse build stub)
Version: $tag
PC
	export ECLIPSE_WAYLAND_SCANNER_PC_DIR="$stub_pc"
	ok "wayland-scanner nativo instalado: $WAYLAND_SCANNER"
}

write_meson_cross() {
	local out="$1"
	mkdir -p "$(dirname "$out")"
	# -static global rompe subproyectos que enlazan mocks .so (p. ej. libliftoff tests).
	# Para binario totalmente estático, prueba ECLIPSE_MESON_STATIC_LINK=1 y desactiva tests
	# en los wraps, o enlaza solo el binario final con LDFLAGS=-static.
	local _link="['-L$ECLIPSE_SYSROOT/usr/lib', '-L$ECLIPSE_SYSROOT/lib', '-Wl,-rpath-link,$ECLIPSE_SYSROOT/usr/lib', '-Wl,-rpath-link,$ECLIPSE_SYSROOT/lib']"
	if [[ "${ECLIPSE_MESON_STATIC_LINK:-0}" == "1" ]]; then
		_link="['-static', '-Wl,--allow-multiple-definition', '-L$ECLIPSE_SYSROOT/usr/lib', '-L$ECLIPSE_SYSROOT/lib', '-Wl,-rpath-link,$ECLIPSE_SYSROOT/usr/lib', '-Wl,-rpath-link,$ECLIPSE_SYSROOT/lib']"
	fi
	# shellcheck disable=SC2016
	cat >"$out" <<EOF
[binaries]
c = '$CROSS_MUSL_CC'
cpp = '$CROSS_MUSL_CPP'
ar = '$CROSS_MUSL_AR'
nm = '$CROSS_MUSL_NM'
ranlib = '$CROSS_MUSL_RANLIB'
strip = '$CROSS_MUSL_STRIP'
pkg-config = 'pkg-config'
wayland-scanner = '$USERLAND_DIR/host-tools/bin-isolated/wayland-scanner'
glib-genmarshal = '$USERLAND_DIR/host-tools/bin-isolated/glib-genmarshal'
glib-mkenums = '$USERLAND_DIR/host-tools/bin-isolated/glib-mkenums'
gdbus-codegen = '$USERLAND_DIR/host-tools/bin-isolated/gdbus-codegen'
glib-compile-resources = '$USERLAND_DIR/host-tools/bin-isolated/glib-compile-resources'
glib-compile-schemas = '$USERLAND_DIR/host-tools/bin-isolated/glib-compile-schemas'
xdt-gen-visibility = '$USERLAND_DIR/host-tools/bin/xdt-gen-visibility'
rust = ['rustc', '--target', 'x86_64-unknown-linux-musl']
cargo = 'cargo'

[build_binaries]
c = '/usr/bin/cc'
cpp = '/usr/bin/c++'
ar = '/usr/bin/ar'
nm = '/usr/bin/nm'
pkg-config = 'pkg-config'
wayland-scanner = '$USERLAND_DIR/host-tools/bin-isolated/wayland-scanner'
glib-genmarshal = '$USERLAND_DIR/host-tools/bin-isolated/glib-genmarshal'
glib-mkenums = '$USERLAND_DIR/host-tools/bin-isolated/glib-mkenums'
gdbus-codegen = '$USERLAND_DIR/host-tools/bin-isolated/gdbus-codegen'
glib-compile-resources = '$USERLAND_DIR/host-tools/bin-isolated/glib-compile-resources'
glib-compile-schemas = '$USERLAND_DIR/host-tools/bin-isolated/glib-compile-schemas'
xdt-gen-visibility = '$USERLAND_DIR/host-tools/bin/xdt-gen-visibility'

[built-in options]
c_args = ['-idirafter', '$ECLIPSE_SYSROOT/usr/include', '-idirafter', '$ECLIPSE_SYSROOT/include', '-D_REDIR_TIME64=0', '-Wno-error=undef']
cpp_args = ['-idirafter', '$ECLIPSE_SYSROOT/usr/include', '-idirafter', '$ECLIPSE_SYSROOT/include', '-D_REDIR_TIME64=0', '-Wno-error=undef']
c_link_args = $_link
cpp_link_args = $_link

[host_machine]
system = 'linux'
cpu_family = 'x86_64'
cpu = 'x86_64'
endian = 'little'
EOF
	info "Escrito: $out"
}

export_musl_cross_env() {
	export CC="$MUSL_GCC"
	export CXX="$MUSL_GXX"
	export AR="$MUSL_AR"
	export NM="$MUSL_NM"
	export RANLIB="$MUSL_RANLIB"
	local scan_pc="${ECLIPSE_WAYLAND_SCANNER_PC_DIR:-}"
	local pc_pre=
	if [[ -n "$scan_pc" && -d "$scan_pc" ]]; then
		pc_pre="$scan_pc:"
		# Dependencias de *build machine* (p. ej. wayland-scanner) ignoran PKG_CONFIG_PATH en cross.
		export PKG_CONFIG_PATH_FOR_BUILD="$scan_pc${PKG_CONFIG_PATH_FOR_BUILD:+:$PKG_CONFIG_PATH_FOR_BUILD}"
	fi
	export PKG_CONFIG_PATH="${pc_pre}$ECLIPSE_SYSROOT/usr/lib/pkgconfig:$ECLIPSE_SYSROOT/lib/pkgconfig:$ECLIPSE_SYSROOT/usr/share/pkgconfig:$ECLIPSE_SYSROOT/share/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
	export PKG_CONFIG_LIBDIR="${PKG_CONFIG_LIBDIR:-$ECLIPSE_SYSROOT/usr/lib/pkgconfig:$ECLIPSE_SYSROOT/lib/pkgconfig}"
	export PKG_CONFIG_SYSROOT_DIR="$ECLIPSE_SYSROOT"
	local sys_inc="-idirafter $ECLIPSE_SYSROOT/usr/include -idirafter $ECLIPSE_SYSROOT/include"
	export CFLAGS="$sys_inc -fPIC ${CFLAGS:-}"
	export CXXFLAGS="$sys_inc -fPIC ${CXXFLAGS:-}"
	export CPPFLAGS="$sys_inc ${CPPFLAGS:-}"
	export LDFLAGS="-L$ECLIPSE_SYSROOT/usr/lib -L$ECLIPSE_SYSROOT/lib -Wl,-rpath-link,$ECLIPSE_SYSROOT/usr/lib -Wl,-rpath-link,$ECLIPSE_SYSROOT/lib ${LDFLAGS:-}"
}

build_wlroots() {
	check_toolchain
	check_sysroot
	patch_sysroot_extras
	ensure_native_wayland_scanner
	ensure_gbm_stub
	export_musl_cross_env
	require_cmd meson
	require_cmd ninja

	local root="$USERLAND_DIR/wlroots_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"

	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols

	write_meson_cross "$cross"

	if [[ "${ECLIPSE_WLROOTS_CLEAN:-0}" == "1" ]]; then
		rm -rf "$bld"
	fi

	info "Configurando wlroots → $bld"
	# backends/renderers/session=auto tolera sysroots parciales; fuerza drm+libinput con
	# ECLIPSE_WLROOTS_BACKENDS=drm,libinput si ya tienes todo en el sysroot.
	meson setup "$bld" "$root" --cross-file="$cross" \
		--prefix="$ECLIPSE_SYSROOT/usr" \
		"--buildtype=$ECLIPSE_MESON_BUILDTYPE" \
		-Ddefault_library=${default_lib:-shared} \
		-Dexamples=false \
		-Dxwayland=disabled \
		-Dbackends="${ECLIPSE_WLROOTS_BACKENDS:-drm,libinput}" \
		-Drenderers="${ECLIPSE_WLROOTS_RENDERERS:-auto}" \
		-Dsession="${ECLIPSE_WLROOTS_SESSION:-enabled}"

	info "Compilando wlroots…"
	ninja -C "$bld"

	info "Instalando wlroots en $ECLIPSE_SYSROOT/usr …"
	DESTDIR="" ninja -C "$bld" install
	ok "wlroots instalado (pkg-config: wlroots-0.18 en el sysroot)."
}

build_xkeyboard_config() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	require_cmd meson
	require_cmd ninja

	local root="$USERLAND_DIR/xkeyboard-config_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"

	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols

	write_meson_cross "$cross"

	if [[ "${ECLIPSE_XKB_CLEAN:-0}" == "1" ]]; then
		rm -rf "$bld"
	fi

	info "Configurando xkeyboard-config → $bld"
	# prefix=/usr + DESTDIR: mismos datos bajo el sysroot sin rutas absolutas del host en metadatos.
	meson setup "$bld" "$root" --cross-file="$cross" \
		--prefix=/usr \
		-Ddatadir=share \
		"-Dbuildtype=$ECLIPSE_MESON_BUILDTYPE"

	info "Instalando xkeyboard-config…"
	DESTDIR="$ECLIPSE_SYSROOT" ninja -C "$bld" install

	ok "xkeyboard-config instalado en $ECLIPSE_SYSROOT/usr/share/X11/xkb"
}

build_labwc() {
	check_toolchain
	check_sysroot
	patch_sysroot_extras
	ensure_native_wayland_scanner
	ensure_gbm_stub
	export_musl_cross_env
	ensure_libseat_no_builtin
	require_cmd meson
	require_cmd ninja
	require_file "$WAYLAND_SCANNER" "falló la construcción nativa de wayland-scanner"

	local root="$USERLAND_DIR/labwc_v08"
	local cross="$root/meson.cross"
	local bld="$root/bld"

	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	ensure_wlroots_subproject

	# labwc enlaza wayland-server, Mesa (EGL/gbm), udev, cairo/pango, etc. desde el
	# sysroot como .so; -static en el cross file hace que ld rechace esas bibliotecas.
	write_meson_cross "$cross"

	if [[ "${ECLIPSE_LABWC_CLEAN:-0}" == "1" ]]; then
		rm -rf "$bld"
	fi

	info "Configurando labwc → $bld"
	local default_lib="shared"
	if [[ "${ECLIPSE_MESON_STATIC_LINK:-0}" == "1" ]]; then
		default_lib="static"
	fi
	meson setup "$bld" "$root" --cross-file="$cross" \
		--prefix="$ECLIPSE_SYSROOT/usr" \
		"--buildtype=$ECLIPSE_MESON_BUILDTYPE" \
		--default-library=static \
		--force-fallback-for=wlroots,pixman,libxkbcommon,wayland,libdisplay-info,libsfdo,libffi \
		-Dxwayland=disabled \
		-Dnls=disabled \
		-Dman-pages=disabled \
		-Dwlroots:werror=false \
		-Dwlroots:backends="${ECLIPSE_WLROOTS_BACKENDS:-drm,libinput}" \
		-Dwlroots:session=enabled

	info "Compilando labwc (objetivo 'labwc' solamente, sin tests de subproyectos)…"
	ninja -C "$bld" labwc

	eclipse_fix_labwc_rpath "$bld/labwc"

	ok "Binario: $bld/labwc"
}

install_labwc_bin() {
	local bld="$USERLAND_DIR/labwc_v08/bld"
	require_file "$bld/labwc"
	eclipse_fix_labwc_rpath "$bld/labwc"
	cp -f "$bld/labwc" "$ECLIPSE_SYSROOT/usr/bin/labwc"

	# Instalar configuración y temas por defecto
	mkdir -p "$ECLIPSE_SYSROOT/usr/share/labwc" "$ECLIPSE_SYSROOT/usr/share/themes/labwc/openbox-3"
	cp -f "$USERLAND_DIR/labwc_v08/docs/rc.xml" "$ECLIPSE_SYSROOT/usr/share/labwc/rc.xml"
	cp -f "$USERLAND_DIR/labwc_v08/docs/menu.xml" "$ECLIPSE_SYSROOT/usr/share/labwc/menu.xml"
	cp -f "$USERLAND_DIR/labwc_v08/docs/themerc" "$ECLIPSE_SYSROOT/usr/share/themes/labwc/openbox-3/themerc"

	ok "Instalado: $ECLIPSE_SYSROOT/usr/bin/labwc (y assets en usr/share/labwc)"
}

install_xfwl4_bin() {
	local bld="$USERLAND_DIR/xfwl4/bld"
	require_file "$bld/xfwl4"
	cp -f "$bld/xfwl4" "$ECLIPSE_SYSROOT/usr/bin/xfwl4"
	ok "Instalado: $ECLIPSE_SYSROOT/usr/bin/xfwl4"
}

build_zlib() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/zlib_src"
	info "Construyendo zlib..."
	(
		cd "$root"
		git checkout . && git clean -fdx
		# Deshabilitar símbolos versionados (causan caos con musl si el linker los detecta)
		sed -i 's/--version-script,${SRCDIR}zlib.map//g' configure
		# Forzar -fPIC para que la lib estática pueda ser usada en objetos compartidos
		CHOST="$MUSL_PREFIX" CC="$MUSL_GCC" CFLAGS="$CFLAGS -fPIC" ./configure --prefix="$ECLIPSE_SYSROOT/usr"
		make -j$(nproc)
		make install
	)
}

build_expat() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/expat_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	# expat a veces no tiene meson.build, pero si lo tiene lo usamos, si no CMake
	if [[ -f "$root/meson.build" ]]; then
		write_meson_cross "$cross"
		meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
			-Ddefault_library=${default_lib:-shared} -Dtests=false
		ninja -C "$bld" install
	else
		local shared="ON"
		if [[ "$default_lib" == "static" ]]; then
			shared="OFF"
		fi
		info "expat no tiene meson.build, usando cmake (shared=$shared)..."
		mkdir -p "$bld"
		(
			cd "$bld"
			cmake "$root" -DCMAKE_INSTALL_PREFIX="$ECLIPSE_SYSROOT/usr" \
				-DCMAKE_C_COMPILER="$MUSL_GCC" -DCMAKE_CXX_COMPILER="$MUSL_GXX" \
				-DEXPAT_BUILD_TESTS=OFF -DEXPAT_BUILD_EXAMPLES=OFF \
				-DEXPAT_BUILD_TOOLS=OFF \
				-DBUILD_SHARED_LIBS="$shared"
			make -j$(nproc) install
		)
	fi
}

build_libffi() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/libffi_src"
	info "Construyendo libffi..."
	(
		cd "$root"
		[[ -f configure ]] || ./autogen.sh
		./configure --host="$MUSL_PREFIX" --prefix="$ECLIPSE_SYSROOT/usr" --enable-static --disable-shared
		make -j$(nproc)
		make install
	)
}

build_pixman() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/pixman_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo pixman..."
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Dtests=disabled
	ninja -C "$bld" install
}

build_libudev_zero() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/libudev-zero_src"
	info "Construyendo libudev-zero..."
	(
		cd "$root"
		# libudev-zero suele tener un Makefile simple
		local p="$ECLIPSE_SYSROOT/usr"
		local target="install"
		if [[ "$default_lib" == "static" ]]; then
			target="install-static"
		fi
		make CC="$MUSL_GCC" AR="$MUSL_AR" PREFIX="$p" LIBDIR="$p/lib" INCLUDEDIR="$p/include" PKGCONFIGDIR="$p/lib/pkgconfig" -j$(nproc)
		make CC="$MUSL_GCC" AR="$MUSL_AR" PREFIX="$p" LIBDIR="$p/lib" INCLUDEDIR="$p/include" PKGCONFIGDIR="$p/lib/pkgconfig" "$target"
	)
}

build_libevdev() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/libevdev_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo libevdev..."
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Dtests=disabled
	ninja -C "$bld" install
}

build_mtdev() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/mtdev_src"
	info "Construyendo mtdev..."
	(
		cd "$root"
		[[ -f configure ]] || ./autogen.sh
		./configure --host="$MUSL_PREFIX" --prefix="$ECLIPSE_SYSROOT/usr" --enable-static --disable-shared
		make -j$(nproc)
		make install
	)
}

build_libinput() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/libinput_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	if [[ "${ECLIPSE_LIBINPUT_CLEAN:-0}" == "1" ]]; then
		rm -rf "$bld"
	fi
	info "Construyendo libinput..."
	# libinput necesita libevdev, mtdev y libudev (usamos libudev-zero)
	# --prefix=/usr (no $ECLIPSE_SYSROOT/usr): los quirks/datadir se compilan como /usr/share/…;
	# la instalación al sysroot va con DESTDIR (mismo patrón que libjpeg-turbo con CMAKE_INSTALL_PREFIX=/usr).
	meson setup "$bld" "$root" --cross-file="$cross" --prefix=/usr \
		-Ddefault_library=${default_lib:-shared} -Ddebug-gui=false -Dtests=false -Ddocumentation=false \
		-Dlibwacom=false
	DESTDIR="$ECLIPSE_SYSROOT" ninja -C "$bld" install
}

build_xkbcommon() {
	check_toolchain
	check_sysroot
	ensure_native_wayland_scanner
	export_musl_cross_env
	local root="$USERLAND_DIR/xkbcommon_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	if [[ "${ECLIPSE_XKBCOMMON_CLEAN:-0}" == "1" ]]; then
		rm -rf "$bld"
	fi
	info "Construyendo xkbcommon..."
	# --prefix=/usr + DESTDIR: instala bajo el sysroot con rutas lógicas /usr/…
	# -Dxkb-config-root=…: si no se fija, Meson usa pkg-config `xkb_base` de xkeyboard-config,
	#   que suele ser la ruta absoluta del host (…/eclipse-os-build/usr/…) → [XKB-632] en la imagen.
	meson setup "$bld" "$root" --cross-file="$cross" --prefix=/usr \
		-Ddefault_library=${default_lib:-shared} -Denable-x11=false -Denable-docs=false -Denable-wayland=true \
		-Dxkb-config-root=/usr/share/X11/xkb \
		-Dxkb-config-extra-path=/etc/xkb \
		-Dx-locale-root=/usr/share/X11/locale
	DESTDIR="$ECLIPSE_SYSROOT" ninja -C "$bld" install
}

build_pcre2() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/pcre2_src"
	info "Construyendo pcre2..."
	(
		cd "$root"
		[[ -f configure ]] || ./autogen.sh
		make clean || true
		local shared="--enable-shared"
		if [[ "$default_lib" == "static" ]]; then
			shared="--disable-shared"
		fi
		./configure --host="$MUSL_PREFIX" --prefix="$ECLIPSE_SYSROOT/usr" \
			"$shared" --enable-static --disable-shared --enable-pcre2-8 \
			--enable-pcre2-16 --enable-pcre2-32 --disable-stack-for-recursion
		make -j$(nproc)
		make install
	)
}

build_libjpeg_turbo() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/libjpeg-turbo_src"
	local bld="$root/bld-eclipse"
	mkdir -p "$bld"
	info "Construyendo libjpeg-turbo..."
	(
		cd "$bld"
		local shared="ON"
		local static="OFF"
		if [[ "$default_lib" == "static" ]]; then
			shared="OFF"
			static="ON"
		fi
		cmake "$root" \
			-DCMAKE_TOOLCHAIN_FILE="$USERLAND_DIR/eclipse-toolchain.cmake" \
			-DCMAKE_INSTALL_PREFIX="/usr" \
			-DCMAKE_INSTALL_LIBDIR="/usr/lib" \
			-DENABLE_SHARED="$shared" -DENABLE_STATIC="$static" \
			-DCMAKE_BUILD_TYPE=Release
		make -j$(nproc)
		make DESTDIR="$ECLIPSE_SYSROOT" install
	)
}

build_libpng() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/libpng_src"
	local bld="$root/bld-eclipse"
	mkdir -p "$bld"
	info "Construyendo libpng..."
	(
		cd "$bld"
		local shared="ON"
		local static="OFF"
		if [[ "$default_lib" == "static" ]]; then
			shared="OFF"
			static="ON"
		fi
		cmake "$root" \
			-DCMAKE_TOOLCHAIN_FILE="$USERLAND_DIR/eclipse-toolchain.cmake" \
			-DCMAKE_INSTALL_PREFIX="/usr" \
			-DENABLE_SHARED="$shared" -DENABLE_STATIC="$static" \
			-DPNG_TESTS=OFF \
			-DCMAKE_BUILD_TYPE=Release
		make -j$(nproc)
		make DESTDIR="$ECLIPSE_SYSROOT" install
	)
}

build_glib() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/glib_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo glib..."
	# glib necesita zlib, libffi, pcre2 (usamos el del sistema o pcre2_src si lo tenemos)
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Dselinux=disabled -Dxattr=false -Dlibmount=disabled -Dtests=false
	ninja -C "$bld" install
}

build_fribidi() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/fribidi_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo fribidi..."
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Ddocs=false -Dtests=false
	ninja -C "$bld" install
}

build_freetype() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/freetype_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo freetype..."
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Dzlib=enabled -Dpng=disabled -Dharfbuzz=disabled -Dbzip2=disabled -Dbrotli=disabled
	ninja -C "$bld" install
}

build_fontconfig() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/fontconfig_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo fontconfig..."
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Ddoc=disabled -Dtests=disabled
	ninja -C "$bld" install
}

build_harfbuzz() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/harfbuzz_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo harfbuzz..."
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Dglib=enabled -Dfreetype=enabled -Dtests=disabled -Ddocs=disabled -Dintrospection=disabled
	ninja -C "$bld" install
}

build_cairo() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/cairo_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo cairo..."
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Dfontconfig=enabled -Dfreetype=enabled -Dglib=enabled -Dzlib=enabled \
		-Dtests=disabled -Dxcb=disabled -Dxlib=disabled
	ninja -C "$bld" install
}

build_pango() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/pango_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo pango..."
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Dintrospection=disabled -Ddocumentation=false -Dbuild-testsuite=false -Dbuild-examples=false
	ninja -C "$bld" install
}

build_dbus() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/dbus_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo dbus..."
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Dsystemd=disabled -Dx11_autolaunch=disabled \
		-Dapparmor=disabled -Dselinux=disabled -Dlibaudit=disabled -Dxml_docs=disabled \
		-Ddoxygen_docs=disabled -Dducktype_docs=disabled -Dqt_help=disabled -Dmodular_tests=disabled \
		-Dmessage_bus=true -Dtools=true
	ninja -C "$bld" install
}

build_libxml2() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/libxml2_src"
	info "Construyendo libxml2 (Autotools)..."
	(
		cd "$root"
		git checkout . && git clean -fdx
		# Forzar desactivación de símbolos versionados (causan caos en musl)
		sed -i 's/USE_VERSION_SCRIPT_TRUE=/USE_VERSION_SCRIPT_TRUE="# "/g' configure
		sed -i 's/USE_VERSION_SCRIPT_FALSE="# "/USE_VERSION_SCRIPT_FALSE=/g' configure
		local shared="--enable-shared"
		if [[ "$default_lib" == "static" ]]; then
			shared="--disable-shared"
		fi
		./configure --host="$MUSL_PREFIX" --prefix="$ECLIPSE_SYSROOT/usr" \
			"$shared" --enable-static --disable-shared --with-python=no --with-icu=no \
			--with-zlib="$ECLIPSE_SYSROOT/usr" --with-lzma=no
		make -j$(nproc)
		make install
		# Corregir .pc para que xkbcommon y otros encuentren <libxml/parser.h>
		# Nos aseguramos de no duplicar /libxml2
		sed -i 's|Cflags: -I${includedir}.*|Cflags: -I${includedir}/libxml2|' "$ECLIPSE_SYSROOT/usr/lib/pkgconfig/libxml-2.0.pc"
	)
}

build_shared_mime_info() {
	check_toolchain
	check_sysroot
	build_libxml2
	export_musl_cross_env
	local root="$USERLAND_DIR/shared-mime-info_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo shared-mime-info..."
	# shared-mime-info necesita itstool o similar para traducciones, intentamos con lo mínimo.
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Dupdate-mimedb=true -Dbuild-translations=false
	ninja -C "$bld" install
}

build_wayland() {
	check_toolchain
	check_sysroot
	ensure_native_wayland_scanner
	export_musl_cross_env
	local root="$USERLAND_DIR/wayland_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo wayland (libwayland-client/server)..."
	# No construimos el scanner para el target (ahorra dependencias y evita conflictos en host)
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Ddocumentation=false -Dtests=false -Dscanner=false
	ninja -C "$bld" install

	# Eliminar wayland-scanner.pc del sysroot para que otros componentes (Mesa, protocols)
	# usen obligatoriamente el stub de host-tools y no se confundan con rutas de target.
	rm -f "$ECLIPSE_SYSROOT/usr/lib/pkgconfig/wayland-scanner.pc"
}

build_wayland_protocols() {
	check_toolchain
	check_sysroot
	ensure_native_wayland_scanner
	export_musl_cross_env
	# Limpiamos scanner previo del target para evitar que se use en el host
	rm -f "$ECLIPSE_SYSROOT/usr/bin/wayland-scanner"
	local root="$USERLAND_DIR/wayland-protocols_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Instalando wayland-protocols..."
	export PKG_CONFIG_PATH="$USERLAND_DIR/host-tools/lib/x86_64-linux-gnu/pkgconfig:$PKG_CONFIG_PATH"
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Dtests=false
	ninja -C "$bld" install
}

build_libdrm() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/libdrm_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo libdrm..."
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Dudev=false -Dintel=disabled -Dradeon=disabled \
		-Damdgpu=disabled -Dnouveau=enabled -Dvmwgfx=disabled -Dman-pages=disabled -Dtests=false
	ninja -C "$bld" install
}

build_mesa() {
	check_toolchain
	check_sysroot
	ensure_native_wayland_scanner
	export_musl_cross_env
	local root="$USERLAND_DIR/mesa_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo Mesa (swrast + nouveau for EGL/GLES)..."
	# Mesa es pesado; usamos softpipe (software) y nouveau (si hay HW compatible)
	# Forzamos dri-drivers-path para que EGL sepa dónde buscar en el target.
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Dplatforms=wayland -Dgallium-drivers=softpipe,nouveau \
		-Dvulkan-drivers='' -Dopengl=true -Dgles1=disabled -Dgles2=enabled -Degl=enabled \
		-Dgbm=enabled -Dshared-glapi=enabled -Dllvm=disabled -Dtools='' -Dbuild-tests=false \
		-Dglx=disabled -Ddri-drivers-path=lib/dri -Dgallium-xa=disabled
	ninja -C "$bld" install
}

build_libepoxy() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/libepoxy_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo libepoxy..."
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Dtests=false -Dglx=no -Dx11=false
	ninja -C "$bld" install
}

build_gdk_pixbuf() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/gdk-pixbuf_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo gdk-pixbuf..."
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Dbuiltin_loaders=all -Dman=false -Dintrospection=disabled -Dtests=false -Dglycin=disabled
	ninja -C "$bld" install
}

build_at_spi2_core() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/at-spi2-core_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
	write_meson_cross "$cross"
	info "Construyendo at-spi2-core..."
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Dintrospection=disabled -Ddocs=false -Duse_systemd=false -Dx11=disabled -Ddefault_bus=dbus-daemon
	ninja -C "$bld" install
}

build_gtk3() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/gtk3_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo GTK 3.24..."
	# Desactivamos X11 para Eclipse OS (solo Wayland)
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Dwayland_backend=true -Dx11_backend=false \
		-Dintrospection=false -Ddemos=false -Dtests=false -Dexamples=false
	ninja -C "$bld" install
}

build_libxfce4util() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/libxfce4util_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo libxfce4util..."
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Dintrospection=false
	ninja -C "$bld" install
}

build_xfconf() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/xfconf_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo xfconf..."
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Dintrospection=false -Dtests=false
	ninja -C "$bld" install
}

build_libxfce4ui() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/libxfce4ui_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"
	info "Construyendo libxfce4ui..."
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Dintrospection=false -Dgtk-doc=false \
		-Dx11=disabled -Dwayland=enabled
	ninja -C "$bld" install
}

build_libxfce4windowing() {
	check_toolchain
	check_sysroot
	export_musl_cross_env
	local root="$USERLAND_DIR/libxfce4windowing_src"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols

	# Asegurar submodulo wlr-protocols
	if [[ ! -f "$root/protocols/wlr-protocols/unstable/wlr-foreign-toplevel-management-unstable-v1.xml" ]]; then
		info "Inicializando submodulo protocols/wlr-protocols en libxfce4windowing..."
		(cd "$root" && git submodule update --init --recursive protocols/wlr-protocols)
	fi

	write_meson_cross "$cross"
	info "Construyendo libxfce4windowing..."
	meson setup "$bld" "$root" --cross-file="$cross" --prefix="$ECLIPSE_SYSROOT/usr" \
		-Ddefault_library=${default_lib:-shared} -Dintrospection=false -Dtests=false \
		-Dx11=disabled -Dwayland=enabled
	ninja -C "$bld" install
}

build_xfwl4() {
	check_toolchain
	check_sysroot
	
	# Foundational libs
	build_zlib
	build_expat
	build_libffi
	build_pixman
	build_libudev_zero
	build_libevdev
	build_mtdev
	build_libxml2
	build_libinput
	build_xkbcommon
	build_libdisplay_info
	build_libliftoff
	build_seatd

	# Compilar toda el stack gráfico base primero
	build_wayland
	build_wayland_protocols
	build_libdrm
	build_mesa

	# Glib y dependencias UI base
	build_pcre2
	build_glib
	build_fribidi
	build_freetype
	build_fontconfig
	build_harfbuzz
	build_cairo
	build_pango

	# Dependencias de gdk-pixbuf
	build_shared_mime_info

	# DBus para at-spi2 y xfconf
	build_dbus

	# Luego las dependencias de GTK
	build_libepoxy
	build_libjpeg_turbo
	build_libpng
	build_gdk_pixbuf
	build_at_spi2_core
	build_gtk3
	build_libxfce4util
	build_xfconf
	build_libxfce4ui
	build_libxfce4windowing

	export_musl_cross_env
	require_cmd cargo
	require_cmd meson
	require_cmd ninja

	local root="$USERLAND_DIR/xfwl4"
	local cross="$root/meson.cross-eclipse"
	local bld="$root/bld-eclipse"
	require_file "$root/meson.build"
 
	ensure_xfwl4_xfce_wayland_protocols
	write_meson_cross "$cross"

	info "Configurando xfwl4 con Meson/Cargo..."
	# xfwl4 llama a cargo por debajo. Meson nos ayuda con pkg-config.
	meson setup "$bld" "$root" --cross-file="$cross" \
		--prefix="$ECLIPSE_SYSROOT/usr" \
		"-Dbuildtype=$ECLIPSE_MESON_BUILDTYPE" \
		"-Declipse_sysroot=$ECLIPSE_SYSROOT" \
		-Dbackends=udev,winit -Dxwayland=false

	info "Compilando xfwl4..."
	ninja -C "$bld"

	info "Instalando xfwl4..."
	ninja -C "$bld" install
	ok "xfwl4 instalado en $ECLIPSE_SYSROOT/usr/bin/xfwl4"
}

print_help() {
	cat <<EOF
Construcción userland (Eclipse OS / musl).

  $0 wlroots         — compila e instala wlroots_src en ECLIPSE_SYSROOT/usr
  $0 xkb-data        — compila e instala xkeyboard-config en el sysroot
  $0 labwc           — compila labwc (salida: userland/labwc_v08/bld/labwc)
  $0 labwc-install   — copia labwc a ECLIPSE_SYSROOT/usr/bin/
  $0 xfwl4           — compila e instala xfwl4 y TODAS sus dependencias (GTK3, Xfce, etc.)
  $0 all             — wlroots, xkb-data, labwc y xfwl4

Variables:
  ECLIPSE_SYSROOT=$ECLIPSE_SYSROOT
  ECLIPSE_TOOLCHAIN_DIR=$ECLIPSE_TOOLCHAIN_DIR
  ECLIPSE_LABWC_CLEAN=1 / ECLIPSE_WLROOTS_CLEAN=1 / ECLIPSE_LIBINPUT_CLEAN=1 / ECLIPSE_XKBCOMMON_CLEAN=1  — borrar bld antes de meson setup
  ECLIPSE_LABWC_RUNTIME_RPATH=/usr/lib:/lib  — RUNPATH en la imagen Eclipse (musl en /usr/lib).
    Valores remove|none — quita RUNPATH (host glibc: no ejecutes con /usr/lib del sistema; usa QEMU/chroot o LD_LIBRARY_PATH solo al sysroot musl).
  ECLIPSE_LABWC_SKIP_RPATH_PATCH=1  — no modificar RPATH (solo depuración en el host)
  ECLIPSE_WLROOTS_BACKENDS / _RENDERERS / _SESSION — opciones Meson (-D…) para wlroots
  ECLIPSE_MESON_STATIC_LINK=1 — añade -static al cross Meson (labwc lo omite: sysroot con .so wayland/Mesa/GLib).
  ECLIPSE_WAYLAND_TAG=1.25.0 — tag Git de wayland para scanner + subproyecto (coincide con labwc_v08/subprojects/wayland.wrap)

Notas (labwc): si libseat.a se construyó con libseat-builtin=enabled, el script lo
recompila con builtin=disabled para evitar símbolos duplicados con src/server.c.
Regenera libgbm.a desde gbm_stub (incl. gbm_bo_get_fd_for_plane) con el musl del sysroot.

EOF
}

main() {
	local cmd="${1:-help}"
	case "$cmd" in
	help | -h | --help)
		print_help
		;;
	wlroots)
		build_wlroots
		;;
	labwc)
		build_labwc
		;;
	install)
		check_sysroot
		install_labwc_bin
		install_xfwl4_bin
		;;
	mesa)
		build_mesa
		;;
	libdrm)
		build_libdrm
		;;
	display-info)
		build_libdisplay_info
		;;
	liftoff)
		build_libliftoff
		;;
	seatd)
		build_seatd
		;;
	all)
		build_wlroots
		build_xkeyboard_config
		build_labwc
		build_xfwl4
		;;
	xkb-data)
		build_xkeyboard_config
		build_xkbcommon
		;;
	libffi)
		build_libffi
		;;
	mtdev)
		build_mtdev
		;;
	pcre2)
		build_pcre2
		;;
	libxml2)
		build_libxml2
		;;
	zlib)
		build_zlib
		;;
	expat)
		build_expat
		;;
	udev)
		build_libudev_zero
		;;
	evdev)
		build_libevdev
		;;
	wayland)
		build_wayland
		;;
	input)
		build_libinput
		;;
	pixman)
		build_pixman
		;;
	libffi)
		build_libffi
		;;
	harfbuzz)
		build_harfbuzz
		;;
	glib)
		build_glib
		;;
	fribidi)
		build_fribidi
		;;
	freetype)
		build_freetype
		;;
	fontconfig)
		build_fontconfig
		;;
	harfbuzz)
		build_harfbuzz
		;;
	cairo)
		build_cairo
		;;
	pango)
		build_pango
		;;
	libepoxy)
		build_libepoxy
		;;
	xfwl4)
		build_xfwl4
		;;
	*)
		err "Objetivo desconocido: $cmd"
		print_help
		exit 1
		;;
	esac
}

main "$@"
