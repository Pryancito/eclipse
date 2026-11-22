#!/bin/bash
# Compila la Open POSIX Test Suite (LTP, testcases/open_posix_testsuite) para
# Eclipse OS x86_64 y la deja donde la imagen de QEMU la recoge.
#
# Que mide: los 1607 tests de `conformance/interfaces`, enlazados en estatico
# con el mismo musl con el que se construye la rootfs. Cada test sale a
# `<dir>/<N-M>.run-test`, junto con su `.args` si lo tiene, y `list.txt` los
# nombra todos. `opts-run` (de aqui al lado) es el que los lanza dentro.
#
# Donde: `rootfs/x86_64-minimal/libc-test/open-posix`. Es el unico arbol
# extra que llega a la imagen viva de x86_64 (`LIVE_TEST_SUITE` en
# xtask/src/linux/image.rs), asi que despues hace falta
#   cargo image --arch x86_64 --variant minimal
#   make -C zCore build ARCH=x86_64 LINUX=1 MODE=release VARIANT=minimal
# `run.py` lo hace todo junto.
#
# LTP no se copia al repositorio (es GPL-2.0): se baja aqui a un commit fijo,
# para que dos pasadas midan lo mismo. Subir LTP_REV es cambiar de vara de
# medir: hay que volver a medir la base y reescribir expected-x86_64.txt.
set -eu

LTP_REV=3b962a950b3f37153a45bf1240d07d73ab07676e
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
WORK=$ROOT/ignored/open-posix
SRC=$WORK/ltp/testcases/open_posix_testsuite
OUT=${OUT:-$ROOT/rootfs/x86_64-minimal/libc-test/open-posix}
MUSL=${MUSL:-$ROOT/ignored/target/x86_64/x86_64-linux-musl-cross}
CC=$MUSL/bin/x86_64-linux-musl-gcc
STRIP=$MUSL/bin/x86_64-linux-musl-strip
JOBS=${JOBS:-$(nproc)}

if [ ! -x "$CC" ]; then
	echo "no hay $CC: cargo image --arch x86_64 --variant minimal lo descarga" >&2
	exit 1
fi

mkdir -p "$WORK"
if [ ! -d "$WORK/ltp/.git" ]; then
	git clone -q --filter=blob:none --sparse --no-checkout \
		https://github.com/linux-test-project/ltp.git "$WORK/ltp"
	git -C "$WORK/ltp" sparse-checkout set testcases/open_posix_testsuite
fi
if ! git -C "$WORK/ltp" cat-file -e "$LTP_REV^{commit}" 2>/dev/null; then
	git -C "$WORK/ltp" fetch -q --depth 1 origin "$LTP_REV"
fi
git -C "$WORK/ltp" checkout -q "$LTP_REV"

rm -rf "$OUT"
mkdir -p "$OUT"
LIB=$WORK/lib
mkdir -p "$LIB"
"$CC" -O2 -I"$SRC/include" -c "$SRC/lib/common.c" -o "$LIB/common.o"
ar rcs "$LIB/libcommon.a" "$LIB/common.o"
"$CC" -O2 -static -s -o "$OUT/opts-run" "$ROOT/scripts/open-posix/opts-run.c"

# Lo que harian los Makefiles generados de la suite (generate-makefiles.sh):
# un binario por NUMERO-NUMERO.c con main(), con los CFLAGS y LDLIBS de su
# directorio y de los de encima.
build_one() {
	f=$1
	d=$(dirname "$f")
	n=$(basename "$f" .c)
	grep -q main "$f" || return 0
	extra=""
	p=$d
	while [ "$p" != . ]; do
		[ -f "$p/CFLAGS" ] && extra="$extra $(cat "$p/CFLAGS")"
		[ -f "$p/LDLIBS" ] && extra="$extra $(cat "$p/LDLIBS")"
		p=$(dirname "$p")
	done
	mkdir -p "$OUT/$d"
	# shellcheck disable=SC2086
	if "$CC" -O1 -w -static -pthread -I"$SRC/include" -I"$d" -o "$OUT/$d/$n.run-test" \
		"$f" -L"$LIB" -lcommon $extra -lrt -lm >"$WORK/build.log.$$" 2>&1; then
		"$STRIP" "$OUT/$d/$n.run-test"
	else
		echo "$d/$n" >>"$WORK/build-fail.txt"
	fi
	rm -f "$WORK/build.log.$$"
	[ -f "$d/$n.args" ] && cp "$d/$n.args" "$OUT/$d/"
	return 0
}
: >"$WORK/build-fail.txt"
cd "$SRC"
export CC STRIP OUT SRC LIB WORK
export -f build_one
find conformance/interfaces -regex '.*/[0-9]+-[0-9]+\.c' | sort |
	xargs -P "$JOBS" -I{} bash -c 'build_one "$1"' _ {}
cd "$OUT"
find . -name '*.run-test' | sed 's,^\./,,' | sort >list.txt
echo "$(wc -l <list.txt) tests en $OUT; sin compilar con musl: $(wc -l <"$WORK/build-fail.txt") ($WORK/build-fail.txt)"
