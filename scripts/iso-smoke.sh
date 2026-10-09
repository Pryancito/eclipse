#!/bin/sh
# Arranca una ISO de `dist/` en QEMU y comprueba que llega a PID 1.
#
# Por que existe: `make release` construia la ISO que se distribuye y nadie la
# arrancaba, ni aqui ni en CI. Los tres fallos que dejaron la ISO de arm64
# inservible --- las herramientas de `tools/` enlazadas con el `ld` del host
# (por eso la imagen salio con busybox de PID 1, con un `warning:` por toda
# senal), `/etc/apk/arch` sin escribir y las claves de firma sin ir por arco ---
# no los caza compilar: los caza arrancar.
#
# `cargo image` ya verifica la rootfs (xtask/src/linux/verify.rs) antes de
# empaquetarla, y eso cubre lo que se puede mirar sin encender nada. Esto es la
# otra mitad: que la ISO terminada arranque y que PID 1 sea eclipse-init.
#
# El patron por defecto es la PRIMERA linea que escribe eclipse-init, y esta
# elegido a proposito: lo que fallo en arm64 fue que busybox tomo el relevo en
# silencio, y busybox no escribe esa linea. Un patron mas generico (un prompt,
# un «ok») lo habria dado por bueno.
#
#   scripts/iso-smoke.sh [-a ARCH] [-v VARIANT] [-i ISO] [-t TIMEOUT]
#                        [-m MEM] [-o OUT] [-p PATRON]
#
#   -a ARCH      arquitectura (por defecto x86_64; es la unica con ISO de UEFI)
#   -v VARIANT   variante (por defecto minimal)
#   -i ISO       la ISO, si no es dist/eclipse-<variante>-<arco>.iso
#   -t TIMEOUT   segundos de espera del patron (por defecto 420)
#   -m MEM       RAM del invitado (por defecto 4G; ver abajo)
#   -o OUT       donde va el log de la consola (por defecto build/iso-smoke.log)
#   -p PATRON    lo que hay que ver en la consola
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

ARCH=x86_64
VARIANT=minimal
ISO=""
TIMEOUT=420
MEM=4G
OUT=""
# La primera linea de `tools/eclipse-init/src/main.rs` (`log("starting")`).
PATRON='[eclipse-init]'

while getopts "a:v:i:t:m:o:p:" opt; do
    case "$opt" in
        a) ARCH=$OPTARG ;;
        v) VARIANT=$OPTARG ;;
        i) ISO=$OPTARG ;;
        t) TIMEOUT=$OPTARG ;;
        m) MEM=$OPTARG ;;
        o) OUT=$OPTARG ;;
        p) PATRON=$OPTARG ;;
        *) echo "uso: $0 [-a ARCH] [-v VARIANT] [-i ISO] [-t TIMEOUT] [-m MEM] [-o OUT] [-p PATRON]" >&2; exit 2 ;;
    esac
done

[ "$ARCH" = x86_64 ] || {
    echo "$0: de momento solo x86_64 saca una ISO de UEFI que arranque (ver 'make iso')" >&2
    exit 2
}

[ -n "$ISO" ] || ISO="$ROOT/dist/eclipse-$VARIANT-$ARCH.iso"
[ -f "$ISO" ] || {
    echo "$0: no hay ISO en $ISO; construyela con 'make release ARCHS=$ARCH VARIANTS=$VARIANT'" >&2
    exit 1
}

OVMF="$ROOT/rboot/OVMF.fd"
[ -f "$OVMF" ] || { echo "$0: falta el firmware UEFI en $OVMF" >&2; exit 1; }

[ -n "$OUT" ] || OUT="$ROOT/build/iso-smoke.log"
mkdir -p "$(dirname -- "$OUT")"
: > "$OUT"

command -v qemu-system-x86_64 >/dev/null || {
    echo "$0: falta qemu-system-x86_64 (paquete: qemu-system-x86)" >&2
    exit 1
}

echo "=== humo de la ISO: $(basename -- "$ISO") ($VARIANT, $ARCH), hasta $TIMEOUT s ==="
echo "=== esperando en la consola: $PATRON ==="

# Sin `-enable-kvm` a proposito: en un runner de CI no hay /dev/kvm, y con la
# escotilla puesta QEMU sale con 1 sin explicar casi nada. Arranca igual, mas
# despacio, y de ahi el timeout generoso.
#
# `-m 4G` tampoco es capricho: con 2G `rboot` muere en «failed to map physical
# memory: OutOfFrames» ANTES de la primera linea de kernel, y en la consola eso
# se lee como una ISO rota.
qemu-system-x86_64 \
    -smp 2 \
    -machine q35 \
    -cpu Haswell,+smap,-check,-fsgsbase,+invtsc \
    -m "$MEM" \
    -serial mon:stdio \
    -drive format=raw,if=pflash,readonly=on,file="$OVMF" \
    -drive file="$ISO",media=cdrom \
    -nic none \
    -display none \
    -no-reboot \
    < /dev/null > "$OUT" 2>&1 &
QEMU_PID=$!

limpia() { kill "$QEMU_PID" 2>/dev/null || true; }
trap limpia EXIT INT TERM

# Un sistema operativo que arranca bien no termina: en cuanto se ve el patron,
# el trabajo esta hecho y se apaga QEMU. Esperar a que salga por su cuenta seria
# esperar para siempre.
deadline=$(( $(date +%s) + TIMEOUT ))
while :; do
    if grep -qF -- "$PATRON" "$OUT" 2>/dev/null; then
        echo "=== OK: la ISO arranco y PID 1 hablo ==="
        grep -F -- "$PATRON" "$OUT" | head -3
        exit 0
    fi
    if ! kill -0 "$QEMU_PID" 2>/dev/null; then
        # Una ultima mirada: QEMU puede haber escrito el patron y haberse ido
        # entre el grep de arriba y este.
        if grep -qF -- "$PATRON" "$OUT" 2>/dev/null; then
            echo "=== OK: la ISO arranco y PID 1 hablo ==="
            exit 0
        fi
        echo "$0: QEMU se fue sin que apareciera «$PATRON»" >&2
        echo "--- ultimas 40 lineas de la consola ---" >&2
        tail -40 "$OUT" >&2
        exit 1
    fi
    if [ "$(date +%s)" -ge "$deadline" ]; then
        echo "$0: $TIMEOUT s sin ver «$PATRON» en la consola" >&2
        echo "--- ultimas 40 lineas de la consola ---" >&2
        tail -40 "$OUT" >&2
        exit 1
    fi
    sleep 2
done
