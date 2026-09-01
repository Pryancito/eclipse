#!/usr/bin/env bash
# Smoke-test eclipse.tlbhammer under QEMU SMP.
#
# Usage:
#   scripts/qemu-tlbhammer-smoke.sh [-s SMP] [-t SECONDS] [-c EXTRA_KOPTS] [-S]
#
# -S  soak mode: after first "alive", keep running until TIMEOUT (POST-fix 10+ min).
#
# Success: log contains "tlbhammer: alive" and no "DIAG: shootdown starvation".
# Failure: panic / shootdown starvation / QEMU exit before first alive line.
#
# Requires: make -C zCore build MODE=release LINUX=1 (copies zcore.elf into esp/)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ESP_DIR="$ROOT/target/x86_64/release/esp"
OVMF="$ROOT/rboot/OVMF.fd"

SMP=6
TIMEOUT=180
SOAK=0
EXTRA="eclipse.tlbhammer=${SMP} DEADLOCKSPINS=50000000"

while getopts "s:t:c:S" opt; do
    case "$opt" in
        s) SMP="$OPTARG"; EXTRA="eclipse.tlbhammer=${SMP} DEADLOCKSPINS=50000000" ;;
        t) TIMEOUT="$OPTARG" ;;
        c) EXTRA="${EXTRA} ${OPTARG}" ;;
        S) SOAK=1 ;;
        *) echo "usage: $0 [-s SMP] [-t SECONDS] [-c EXTRA_KOPTS] [-S]" >&2; exit 2 ;;
    esac
done

[ -d "$ESP_DIR/EFI" ] || {
    echo "$0: no ESP — run: make -C zCore build MODE=release LINUX=1" >&2
    exit 1
}
KERNEL_ELF="$ROOT/target/x86_64/release/zcore"
ESP_KERNEL="$ESP_DIR/EFI/zCore/zcore.elf"
if [ -f "$KERNEL_ELF" ] && [ -f "$ESP_KERNEL" ] && [ "$KERNEL_ELF" -nt "$ESP_KERNEL" ]; then
    echo "$0: ESP kernel stale (run: make -C zCore build MODE=release LINUX=1)" >&2
    exit 1
fi

WORK="$(mktemp -d)"
LOG="$WORK/console.log"
ESP_IMG="$WORK/esp.img"
trap 'rm -rf "$WORK"' EXIT

esp_mb=$(($(du -sm "$ESP_DIR/EFI" | cut -f1) + 128))
dd if=/dev/zero of="$ESP_IMG" bs=1M count="$esp_mb" status=none
mkfs.vfat -F 32 "$ESP_IMG" >/dev/null
mmd -i "$ESP_IMG" ::/EFI ::/EFI/Boot ::/EFI/zCore
mcopy -i "$ESP_IMG" -s "$ESP_DIR/EFI" ::/

conf="$WORK/rboot.conf"
cp "$ESP_DIR/EFI/Boot/rboot.conf" "$conf"
sed -i "s#^cmdline=.*#& ${EXTRA}#" "$conf"
mcopy -o -i "$ESP_IMG" "$conf" ::/EFI/Boot/rboot.conf
echo "$0: cmdline extras: $EXTRA" >&2

qemu-system-x86_64 \
    -smp "$SMP" \
    -machine q35 \
    -cpu Haswell,+smap,-check,-fsgsbase,+invtsc \
    -m 4G \
    -serial file:"$LOG" \
    -drive format=raw,if=pflash,readonly=on,file="$OVMF" \
    -drive format=raw,file="$ESP_IMG" \
    -device qemu-xhci,id=xhci -device usb-kbd,bus=xhci.0 \
    -nic none \
    -display none \
    -no-reboot &
QEMU_PID=$!

deadline=$(( $(date +%s) + TIMEOUT ))
alive=0
alive_lines=0
while [ "$(date +%s)" -lt "$deadline" ]; do
    if grep -qE "tlbhammer: alive|TLB hammer ON" "$LOG" 2>/dev/null; then
        alive=1
        alive_lines=$(grep -cE "tlbhammer: alive|TLB hammer ON" "$LOG" 2>/dev/null || true)
        if [ "$SOAK" -eq 0 ]; then
            break
        fi
    fi
    if grep -q "DIAG: shootdown starvation" "$LOG" 2>/dev/null; then
        echo "$0: FAIL — shootdown starvation detected" >&2
        kill "$QEMU_PID" 2>/dev/null || true
        wait "$QEMU_PID" 2>/dev/null || true
        tail -80 "$LOG" >&2
        exit 1
    fi
    if grep -qE "KERNEL STOP|^\[KERNEL BUG\]|DIAG: shootdown starvation" "$LOG" 2>/dev/null; then
        echo "$0: FAIL — kernel panic/stop" >&2
        kill "$QEMU_PID" 2>/dev/null || true
        wait "$QEMU_PID" 2>/dev/null || true
        tail -80 "$LOG" >&2
        exit 1
    fi
    if ! kill -0 "$QEMU_PID" 2>/dev/null; then
        echo "$0: FAIL — QEMU exited early" >&2
        tail -80 "$LOG" >&2
        exit 1
    fi
    sleep 2
done

kill "$QEMU_PID" 2>/dev/null || true
wait "$QEMU_PID" 2>/dev/null || true

if [ "$alive" -eq 0 ]; then
    echo "$0: FAIL — no 'tlbhammer: alive' within ${TIMEOUT}s" >&2
    tail -80 "$LOG" >&2
    exit 1
fi

if grep -q "DIAG: shootdown starvation" "$LOG"; then
    echo "$0: FAIL — shootdown starvation in log" >&2
    exit 1
fi

if [ "$SOAK" -eq 1 ]; then
    echo "$0: OK — soak ${TIMEOUT}s complete, alive_lines=${alive_lines} (SMP=$SMP)" >&2
else
    echo "$0: OK — hammer alive within ${TIMEOUT}s (SMP=$SMP)" >&2
fi
grep "tlbhammer:" "$LOG" | tail -5 >&2
exit 0
