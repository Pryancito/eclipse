#!/usr/bin/env bash
# Eclipse OS en el navegador: construye los ficheros de eclipse-os.dev/live/emu/.
#
#   scripts/live-web/build.sh [DESTINO]        (por defecto ../Pryancito.github.io/live/emu)
#
# Hace cuatro cosas y deja el resultado en DESTINO:
#   1. el núcleo riscv64 (zcore.bin) y un rootfs mínimo recortado para la web
#      (initrd.img: la variante minimal sin las apps gráficas lunar*, apk ni los
#      probes), ambos comprimidos con gzip;
#   2. qemu-system-riscv64 compilado a WebAssembly con qemu-wasm
#      (github.com/ktock/qemu-wasm, QEMU 8.2 con un backend TCG que genera wasm),
#      dentro del contenedor de Emscripten de scripts/live-web/Dockerfile;
#   3. el firmware OpenSBI que trae ese mismo QEMU;
#   4. manifest.json, que lee live/live.js: qué descargar y con qué argumentos
#      arrancar QEMU.
#
# Necesita: docker, la toolchain de Rust del repo, cargo-binutils (rust-objcopy),
# curl y git. Tarda unos 30 min la primera vez (las dependencias de Emscripten);
# luego solo se recompila lo que cambia. Ver README.md junto a este script.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
DEST=${1:-$ROOT/../Pryancito.github.io/live/emu}
WORK=${LIVE_WEB_WORK:-$ROOT/ignored/live-web}
mkdir -p "$DEST" "$WORK"
DEST=$(cd "$DEST" && pwd)

QEMU_WASM_REPO=https://github.com/ktock/qemu-wasm
QEMU_WASM_REV=0ef7b4e2814b231705d8371dd7997f5b72e70baf
IMAGE=eclipse-live-web-emsdk
CMDLINE="LOG=off:TERM=xterm-256color:console.shell=true:virtcon.disable=true:desktop=none:init.quiet"

say() { printf '\n== %s\n' "$*"; }

# fetch URL FILE SHA256
fetch() {
  if [ ! -f "$2" ] || ! echo "$3  $2" | sha256sum -c --status; then
    curl -fsSL -o "$2.tmp" "$1"
    echo "$3  $2.tmp" | sha256sum -c --status || { echo "sha256 de $1 no cuadra" >&2; exit 1; }
    mv "$2.tmp" "$2"
  fi
}

# ---------------------------------------------------------------- 1. Eclipse
say "Eclipse: rootfs riscv64 minimal y núcleo"
cd "$ROOT"
ECLIPSE_XORG=off cargo rootfs --arch riscv64 --variant minimal
rm -rf "$WORK/rootfs"
cp -a rootfs/riscv64-minimal "$WORK/rootfs"
(
  cd "$WORK/rootfs"
  # Sin pantalla no hay apps gráficas; sin red, apk no sirve.
  rm -f bin/lunar* bin/apk bin/wavplay bin/*-probe bin/ecl-compute bin/ecl-vkcompute bin/eclipse-*bench
  # mountd muere en riscv64 (fifo_create: ENOSYS) y se relanzaría sin parar;
  # boot-sound no tiene tarjeta de sonido a la que hablar.
  rm -f etc/eclipse/services/mountd.service etc/eclipse/services/boot-sound.service
)
REPACK_ROOTFS="$WORK/rootfs" REPACK_IMAGE="$WORK/initrd.img" \
  cargo test -q -p xtask --release -- --ignored dbg_repack_initramfs
make -C zCore build ARCH=riscv64 LINUX=1 MODE=release
gzip -9c target/riscv64/release/zcore.bin > "$DEST/zcore.bin.gz"
gzip -9c "$WORK/initrd.img" > "$DEST/initrd.img.gz"

# ------------------------------------------------------ 2. QEMU para la web
say "qemu-wasm: fuentes"
Q="$WORK/qemu-wasm"
if [ ! -d "$Q/.git" ]; then
  git init -q "$Q"
  git -C "$Q" remote add origin "$QEMU_WASM_REPO"
fi
git -C "$Q" fetch -q --depth 1 origin "$QEMU_WASM_REV"
git -C "$Q" checkout -q --force "$QEMU_WASM_REV"
# Los subprojects de meson se clonan aquí: el contenedor monta las fuentes
# de solo lectura y no puede hacerlo él.
for w in dtc keycodemapdb berkeley-softfloat-3 berkeley-testfloat-3; do
  url=$(sed -n 's/^url *= *//p' "$Q/subprojects/$w.wrap")
  rev=$(sed -n 's/^revision *= *//p' "$Q/subprojects/$w.wrap")
  [ -d "$Q/subprojects/$w/.git" ] || git clone -q "$url" "$Q/subprojects/$w"
  git -C "$Q/subprojects/$w" checkout -q "$rev"
  [ -d "$Q/subprojects/packagefiles/$w" ] && cp -r "$Q/subprojects/packagefiles/$w/." "$Q/subprojects/$w/"
done

say "qemu-wasm: dependencias de Emscripten (imagen $IMAGE)"
CTX="$WORK/docker"
mkdir -p "$CTX"
U=https://archive.ubuntu.com/ubuntu/pool/main
fetch https://github.com/madler/zlib/releases/download/v1.3.2/zlib-1.3.2.tar.xz "$CTX/zlib.tar.xz" d7a0654783a4da529d1bb793b7ad9c3318020af77667bcae35f95d0e42a792f3
fetch $U/libf/libffi/libffi_3.5.2.orig.tar.gz "$CTX/libffi.tar.gz" dd19253d3007f366319a51d248a40c9e5fcace4498cbea990b566291844e4e30
fetch $U/p/pixman/pixman_0.42.2.orig.tar.gz "$CTX/pixman.tar.gz" ea1480efada2fd948bc75366f7c349e1c96d3297d09a3fe62626e38e234a625e
fetch $U/g/glib2.0/glib2.0_2.84.1.orig.tar.xz "$CTX/glib-orig.tar.xz" 2b4bc2ec49611a5fc35f86aca855f2ed0196e69e53092bab6bb73396bf30789a
fetch https://github.com/PCRE2Project/pcre2/releases/download/pcre2-10.44/pcre2-10.44.tar.bz2 "$CTX/pcre2-10.44.tar.bz2" d34f02e113cf7193a1ebf2770d3ac527088d485d4e047ed10e5d217c6ef5de96
fetch https://github.com/mesonbuild/wrapdb/releases/download/pcre2_10.44-2/pcre2_10.44-2_patch.zip "$CTX/pcre2_10.44-2_patch.zip" 4336d422ee9043847e5e10dbbbd01940d4c9e5027f31ccdc33a7898a1ca94009
if [ ! -f "$CTX/glib.tar.xz" ]; then
  # glib con su subproject pcre2 ya descargado (meson no tiene red en el build).
  rm -rf "$CTX/glib-2.84.1"
  tar xJf "$CTX/glib-orig.tar.xz" -C "$CTX"
  mkdir -p "$CTX/glib-2.84.1/subprojects/packagecache"
  cp "$CTX/pcre2-10.44.tar.bz2" "$CTX/pcre2_10.44-2_patch.zip" "$CTX/glib-2.84.1/subprojects/packagecache/"
  tar cJf "$CTX/glib.tar.xz" -C "$CTX" glib-2.84.1
  rm -rf "$CTX/glib-2.84.1"
fi
# LIVE_WEB_DOCKERFILE: otro Dockerfile (por ejemplo, uno que añada el
# certificado de un proxy); LIVE_WEB_DOCKER_ARGS: argumentos extra de build.
cp "${LIVE_WEB_DOCKERFILE:-$ROOT/scripts/live-web/Dockerfile}" "$CTX/Dockerfile"
# shellcheck disable=SC2086
docker build ${LIVE_WEB_DOCKER_ARGS:-} -t "$IMAGE" "$CTX"

say "qemu-wasm: compilar qemu-system-riscv64"
EXTRA_CFLAGS="-O3 -g -Wno-error=unused-command-line-argument -matomics -mbulk-memory -DNDEBUG -DG_DISABLE_ASSERT -D_GNU_SOURCE -sASYNCIFY=1 -pthread -sPROXY_TO_PTHREAD=1 -sFORCE_FILESYSTEM -sALLOW_TABLE_GROWTH -sTOTAL_MEMORY=2300MB -sWASM_BIGINT -sMALLOC=mimalloc --js-library=/build/node_modules/xterm-pty/emscripten-pty.js -sEXPORT_ES6=1 -sASYNCIFY_IMPORTS=ffi_call_js"
OUT="$WORK/qemu-out"
mkdir -p "$OUT"
docker run --rm -v "$Q:/qemu:ro" -v "$OUT:/out" "$IMAGE" bash -euc "
  emconfigure /qemu/configure --static --target-list=riscv64-softmmu --cpu=wasm32 --cross-prefix= \
    --without-default-features --enable-system --with-coroutine=fiber \
    --extra-cflags='$EXTRA_CFLAGS' --extra-cxxflags='$EXTRA_CFLAGS' \
    --extra-ldflags='-sEXPORTED_RUNTIME_METHODS=getTempRet0,setTempRet0,addFunction,removeFunction,TTY,FS'
  emmake make -j\$(nproc) qemu-system-riscv64
  # Sin DWARF: de 41 MB a 12 MB (3 MB con gzip).
  /emsdk/upstream/bin/wasm-opt --all-features --strip-debug --strip-producers \
    qemu-system-riscv64.wasm -o /out/qemu-system-riscv64.wasm
  cp qemu-system-riscv64 /out/qemu-system-riscv64.js
  cp qemu-system-riscv64.worker.js /out/
"
cp "$OUT/qemu-system-riscv64.wasm" "$OUT/qemu-system-riscv64.js" "$OUT/qemu-system-riscv64.worker.js" "$DEST/"

# --------------------------------------------------------------- 3. OpenSBI
cp "$Q/pc-bios/opensbi-riscv64-generic-fw_dynamic.bin" "$DEST/opensbi.bin"

# -------------------------------------------------------------- 4. manifest
say "manifest.json"
VERSION=$(sed -n 's/^VERSION_ID="\{0,1\}\([^"]*\)"\{0,1\}$/\1/p' "$WORK/rootfs/etc/os-release")
python3 - "$DEST" "$VERSION" "$(git -C "$ROOT" rev-parse --short=12 HEAD)" "$CMDLINE" <<'EOF'
import json, os, sys
dest, version, commit, cmdline = sys.argv[1:]
size = lambda f: os.path.getsize(os.path.join(dest, f))
files = [("opensbi.bin", "/pack/opensbi.bin", "OpenSBI"),
         ("zcore.bin.gz", "/pack/zcore.bin", "kernel"),
         ("initrd.img.gz", "/pack/initrd.img", "rootfs")]
manifest = {
    "version": f"{version}-riscv64-minimal",
    "commit": commit,
    "emulator": "emu/qemu-system-riscv64.js",
    "emulatorSize": size("qemu-system-riscv64.wasm") + size("qemu-system-riscv64.js"),
    "files": [{"url": "emu/" + f, "path": p, "label": l, "size": size(f)} for f, p, l in files],
    "arguments": ["-nographic", "-machine", "virt", "-cpu", "rv64", "-m", "512M", "-smp", "1",
                  "-accel", "tcg,tb-size=500", "-no-reboot", "-nic", "none",
                  "-bios", "/pack/opensbi.bin", "-kernel", "/pack/zcore.bin",
                  "-initrd", "/pack/initrd.img", "-append", cmdline],
}
with open(os.path.join(dest, "manifest.json"), "w") as f:
    json.dump(manifest, f, indent=2, ensure_ascii=False)
    f.write("\n")
EOF
ls -la "$DEST"
say "Listo: $DEST"
