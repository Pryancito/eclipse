#!/usr/bin/env bash
# Boot the kernel's in-kernel memory self-tests (`eclipse.ktest`, see
# zCore/src/ktest.rs) under QEMU and report what they printed.
#
# Usage:
#   scripts/qemu-ktest.sh ARCH [-s SMP] [-t SECONDS]
#
# ARCH is x86_64, riscv64 or aarch64. Build the kernel first:
#   make -C zCore build ARCH=$ARCH LINUX=1
# No root filesystem is needed: the tests run before the first process and
# the kernel powers the machine off when they are done.
#
# aarch64 boots through UEFI and rayboot: set AARCH64_FW to a directory with
# QEMU_EFI.fd and aarch64_uefi.efi (the tarball `cargo image --arch aarch64`
# downloads, unpacked; its default place is ignored/target/aarch64/firmware).
#
# Success: a "ktest: done pass=N fail=0" line. Failure: any FAIL line, a
# kernel stop, or no summary before the timeout.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARCH="${1:-}"
shift || true
SMP=2
TIMEOUT=240
while getopts "s:t:" opt; do
    case "$opt" in
        s) SMP="$OPTARG" ;;
        t) TIMEOUT="$OPTARG" ;;
        *) echo "usage: $0 ARCH [-s SMP] [-t SECONDS]" >&2; exit 2 ;;
    esac
done

CMDLINE="LOG=warn:eclipse.ktest"
WORK="$(mktemp -d)"
LOG="$WORK/serial.log"
trap 'rm -rf "$WORK"' EXIT

case "$ARCH" in
x86_64)
    ESP_IMG="$ROOT/target/x86_64/release/esp.img"
    [ -f "$ESP_IMG" ] || { echo "$0: no $ESP_IMG — run: make -C zCore build ARCH=x86_64 LINUX=1" >&2; exit 1; }
    cp "$ESP_IMG" "$WORK/esp.img"
    mcopy -o -i "$WORK/esp.img" ::/EFI/Boot/rboot.conf "$WORK/rboot.conf"
    sed -i "s#^cmdline=.*#cmdline=${CMDLINE}#" "$WORK/rboot.conf"
    mcopy -o -i "$WORK/esp.img" "$WORK/rboot.conf" ::/EFI/Boot/rboot.conf
    # rboot refuses to boot without an initramfs; the tests are done before
    # anything would read it.
    truncate -s 1M "$WORK/initramfs.img"
    mcopy -o -i "$WORK/esp.img" "$WORK/initramfs.img" ::/EFI/zCore/initramfs.img
    cmd=(qemu-system-x86_64 -smp "$SMP" -machine q35
        -cpu Haswell,+smap,-check,+fsgsbase -m 2G
        -drive format=raw,if=pflash,readonly=on,file="$ROOT/rboot/OVMF.fd"
        -drive format=raw,file="$WORK/esp.img" -nic none)
    ;;
riscv64)
    ELF="$ROOT/target/riscv64/release/zcore"
    [ -f "$ELF" ] || { echo "$0: no $ELF — run: make -C zCore build ARCH=riscv64 LINUX=1" >&2; exit 1; }
    OBJCOPY="$(find "$(rustc --print sysroot)" -name llvm-objcopy | head -1)"
    "${OBJCOPY:-rust-objcopy}" "$ELF" --strip-all -O binary "$WORK/zcore.bin"
    truncate -s 1M "$WORK/initrd.img"
    cmd=(qemu-system-riscv64 -smp "$SMP" -machine virt
        -cpu rv64,v=true,vlen=128,vext_spec=v1.0 -bios default -m 1G
        -kernel "$WORK/zcore.bin" -initrd "$WORK/initrd.img" -append "$CMDLINE")
    ;;
aarch64)
    ELF="$ROOT/target/aarch64/release/zcore"
    FW="${AARCH64_FW:-$ROOT/ignored/target/aarch64/firmware}"
    [ -f "$ELF" ] || { echo "$0: no $ELF — run: make -C zCore build ARCH=aarch64 LINUX=1" >&2; exit 1; }
    [ -f "$FW/QEMU_EFI.fd" ] || { echo "$0: no $FW/QEMU_EFI.fd — set AARCH64_FW" >&2; exit 1; }
    mkdir -p "$WORK/disk/EFI/Boot"
    cp "$FW/aarch64_uefi.efi" "$WORK/disk/EFI/Boot/bootaa64.efi"
    cp "$ELF" "$WORK/disk/os"
    # rayboot's own config, with the addresses QEMU's `virt` board uses.
    cat > "$WORK/disk/EFI/Boot/Boot.json" <<EOF
{
	"cmdline": "${CMDLINE}",
	"firmware_type": "QEMU",
	"uart_base": 150994944,
	"gic_base": 134217728,
	"offset": 18446462598732840960
}
EOF
    truncate -s 8M "$WORK/root.img"
    cmd=(qemu-system-aarch64 -smp "$SMP" -machine virt -cpu cortex-a72 -m 1G
        -bios "$FW/QEMU_EFI.fd" -hda "fat:rw:$WORK/disk"
        -drive file="$WORK/root.img",if=none,format=raw,id=x0
        -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0)
    ;;
*)
    echo "usage: $0 x86_64|riscv64|aarch64 [-s SMP] [-t SECONDS]" >&2
    exit 2
    ;;
esac

status=0
timeout "$TIMEOUT" "${cmd[@]}" -display none -no-reboot -serial file:"$LOG" -monitor none \
    </dev/null >"$WORK/qemu.out" 2>&1 || status=$?

grep -a '^ktest: ' "$LOG" | tr -d '\r' || true
if [ "$status" -eq 124 ]; then
    echo "$0: FAIL — QEMU still running after ${TIMEOUT}s (no power-off)" >&2
    tail -40 "$LOG" >&2
    exit 1
fi
if grep -aqE 'KERNEL STOP|^\[KERNEL BUG\]|panicked at' "$LOG"; then
    echo "$0: FAIL — the kernel stopped" >&2
    tail -60 "$LOG" >&2
    exit 1
fi
summary="$(grep -a '^ktest: done ' "$LOG" | tail -1 | tr -d '\r')"
if [ -z "$summary" ]; then
    echo "$0: FAIL — no summary line" >&2
    tail -60 "$LOG" >&2
    exit 1
fi
case "$summary" in
    *" fail=0 "*) echo "$0: $ARCH: ${summary#ktest: }" ;;
    *) echo "$0: FAIL — $summary" >&2; exit 1 ;;
esac
