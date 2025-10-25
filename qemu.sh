#!/bin/bash
# Eclipse OS - QEMU (SIN GPU Passthrough - usa VirtIO)
# Para GPU passthrough real, usa hardware directo con la ISO

set -e

DISK="build/x86_64/eclipse/harddrive.img"

echo "╔═══════════════════════════════════════════════════════════╗"
echo "║              Eclipse OS - QEMU (VirtIO GPU)              ║"
echo "╚═══════════════════════════════════════════════════════════╝"
echo ""
echo "ℹ️  Nota: GPU passthrough con NVIDIA RTX 2060 requiere"
echo "   arranque directo en hardware. Este script usa VirtIO."
echo ""
echo "   Para usar la GPU real: Arranca desde USB/disco físico"
echo ""

# Verificar disco
if [ ! -f "$DISK" ]; then
    echo "❌ Error: $DISK no encontrado"
    exit 1
fi

echo "✅ Disco: $(du -h $DISK | cut -f1)"
echo ""
echo "Iniciando VM en 2 segundos..."
sleep 2

# Lanzar QEMU con VirtIO (sin GPU passthrough)
sudo qemu-system-x86_64 \
    -name "Eclipse OS" \
    -enable-kvm \
    -cpu host \
    -smp 8 \
    -m 8G \
    -machine q35,accel=kvm \
    -bios /usr/share/ovmf/OVMF.fd \
    -drive file="$DISK",format=raw,if=virtio \
    -vga virtio \
    -device virtio-net-pci,netdev=net0 \
    -netdev user,id=net0,hostfwd=tcp::5555-:22 \
    -device virtio-rng-pci,rng=rng0 \
    -object rng-random,filename=/dev/urandom,id=rng0 \
    -device qemu-xhci \
    -device usb-kbd \
    -device usb-tablet \
    -serial mon:stdio

echo ""
echo "✅ VM terminada"
echo ""
echo "💡 Para usar tu NVIDIA RTX 2060 SUPER:"
echo "   1. Graba la ISO en USB: sudo dd if=build/x86_64/eclipse/redox-live.iso of=/dev/sdX bs=4M"
echo "   2. Arranca desde el USB en tu PC con las 2 GPUs"
echo "   3. Los drivers NVIDIA se cargarán automáticamente"
