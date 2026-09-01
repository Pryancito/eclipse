#!/usr/bin/env bash
# run-qemu-macvtap.sh — Lanza QEMU con Macvtap sobre la interfaz física.
# La VM obtiene su propia MAC/IP en el router, como si fuera un equipo más.
# Requiere sudo para crear el dispositivo macvtap.
set -e

IFACE="${IFACE:-eno1}"
MACVTAP="macvtap0"
VM_MAC="52:54:00:12:34:56"
LOG="${LOG:-warn}"
ACCEL="${ACCEL:-1}"
GRAPHIC="${GRAPHIC:-on}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# ─── 1. Build kernel via zCore Makefile (includes graphic feature) ──────────
echo "[+] Building kernel (GRAPHIC=$GRAPHIC, LOG=$LOG)..."
make -C "$SCRIPT_DIR" build \
    MODE=release LINUX=1 \
    LOG="$LOG" \
    GRAPHIC="$GRAPHIC" \
    ACCEL="$ACCEL"

# Update rboot.conf cmdline (initramfs path is fixed in rboot.conf)
ESP="$SCRIPT_DIR/../target/x86_64/release/esp"
sed -i "s#cmdline=.*#cmdline=LOG=$LOG#" \
    "$ESP/EFI/Boot/rboot.conf"

# ─── 2. Create macvtap interface ───────────────────────────────────────────
echo "[+] Creating macvtap interface $MACVTAP on $IFACE..."
sudo ip link delete "$MACVTAP" 2>/dev/null || true
sudo ip link add link "$IFACE" name "$MACVTAP" type macvtap mode bridge
sudo ip link set "$MACVTAP" address "$VM_MAC"
sudo ip link set "$MACVTAP" up

IFINDEX=$(cat "/sys/class/net/$MACVTAP/ifindex")
TAPDEV="/dev/tap${IFINDEX}"
echo "[+] $MACVTAP (ifindex=$IFINDEX) -> $TAPDEV"
sudo chmod 666 "$TAPDEV"

# ─── 3. Build QEMU command ─────────────────────────────────────────────────
QEMU_ARGS=(
    -smp 4
    -machine q35
    -m 1G
    -serial mon:stdio
    -serial "file:/tmp/serial.out"
    -drive "format=raw,if=pflash,readonly=on,file=$SCRIPT_DIR/../rboot/OVMF.fd"
    -drive "format=raw,file=fat:rw:$ESP"
    -nic none
    -device qemu-xhci,id=xhci
    -device usb-kbd,bus=xhci.0
    -device usb-tablet,bus=xhci.0
    -netdev "tap,id=net0,fd=3"
    -device "e1000e,netdev=net0,mac=$VM_MAC"
)

if [ "$ACCEL" = "1" ]; then
    QEMU_ARGS+=(-accel kvm -cpu "host,migratable=no,+invtsc")
else
    QEMU_ARGS+=(-cpu "Haswell,+smap,-check,-fsgsbase")
fi

if [ "$GRAPHIC" = "off" ]; then
    QEMU_ARGS+=(-display none)
else
    QEMU_ARGS+=(-vga virtio)
fi

# Guest HDA codec (PCI 04:03). Prefer pipewire/pa/alsa; fall back to silent.
AUDIO="${AUDIO:-on}"
QEMU_BIN="${QEMU:-qemu-system-x86_64}"
qemu_audio_help() { "$1" -audiodev help 2>/dev/null || true; }
pick_audio_backend() {
    local help="$1"
    echo "$help" | grep -qx pipewire && { echo pipewire; return; }
    echo "$help" | grep -qx pa && { echo pa; return; }
    echo "$help" | grep -qx alsa && { echo alsa; return; }
    echo none
}
AUDIO_HELP=$(qemu_audio_help "$QEMU_BIN")
AUDIODEV="${AUDIODEV:-}"
if [ "$AUDIO" = "off" ]; then
    AUDIODEV="${AUDIODEV:-none}"
else
    if [ -z "$AUDIODEV" ]; then
        AUDIODEV=$(pick_audio_backend "$AUDIO_HELP")
        if [ "$AUDIODEV" = "none" ] && [ -x /usr/bin/qemu-system-x86_64 ]; then
            DISTRO_HELP=$(qemu_audio_help /usr/bin/qemu-system-x86_64)
            DISTRO_DEV=$(pick_audio_backend "$DISTRO_HELP")
            if [ "$DISTRO_DEV" != "none" ]; then
                QEMU_BIN=/usr/bin/qemu-system-x86_64
                AUDIODEV="$DISTRO_DEV"
            fi
        fi
    fi
fi
echo "[+] QEMU audio backend=$AUDIODEV via $QEMU_BIN"
QEMU_ARGS+=(-audiodev "$AUDIODEV,id=snd0" -device intel-hda,id=snd -device hda-output,bus=snd.0,cad=0,audiodev=snd0)

# ─── 4. Launch QEMU ────────────────────────────────────────────────────────
echo "[+] Launching QEMU with macvtap (fd=3 -> $TAPDEV)..."
exec 3<>"$TAPDEV"
exec "$QEMU_BIN" "${QEMU_ARGS[@]}"

# Cleanup (only reached if exec fails, normally unreachable)
sudo ip link delete "$MACVTAP" 2>/dev/null || true
