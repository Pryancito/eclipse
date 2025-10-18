#!/bin/bash

# Script para ejecutar Redox OS desde harddrive.img en QEMU
# Con soporte completo de entropía y hardware virtualizado

echo "🚀 Iniciando Redox OS desde harddrive.img"
echo "============================================"

DISK_PATH="/home/moebius/redox/build/x86_64/desktop/harddrive.img"

if [ ! -f "$DISK_PATH" ]; then
    echo "❌ Error: $DISK_PATH no existe"
    exit 1
fi

echo "📁 Disco: $DISK_PATH"
echo "💾 Tamaño: $(du -h $DISK_PATH | cut -f1)"
echo ""
echo "🎲 Configurando fuentes de entropía:"
echo "   ✅ VirtIO RNG con /dev/urandom (alta velocidad)"
echo "   ✅ CPU host con TSC invariancy"
echo "   ✅ KVM habilitado para mejor rendimiento"
echo ""
echo "⌨️  Controles:"
echo "   Ctrl+Alt+G     - Liberar mouse/teclado"
echo "   Ctrl+Alt+F     - Pantalla completa"
echo "   Ctrl+A, X      - Salir de QEMU"
echo ""

# Crear pool de entropía temporal
dd if=/dev/urandom of=/tmp/qemu_entropy bs=1M count=2 2>/dev/null

qemu-system-x86_64 \
    -enable-kvm \
    -smp 4 \
    -m 2G \
    -cpu host,+invtsc \
    -drive file="$DISK_PATH",format=raw,if=virtio \
    \
    -device qemu-xhci,id=xhci \
    -device usb-kbd,bus=xhci.0 \
    -device usb-mouse,bus=xhci.0 \
    \
    -device intel-hda \
    -device hda-output \
    \
    -netdev user,id=net0,hostfwd=tcp::2222-:22 \
    -device virtio-net-pci,netdev=net0 \
    \
    -object rng-random,filename=/dev/urandom,id=rng0 \
    -device virtio-rng-pci,rng=rng0,max-bytes=2048,period=500 \
    \
    -object rng-random,filename=/tmp/qemu_entropy,id=rng1 \
    -device virtio-rng-pci,rng=rng1,max-bytes=512,period=2000 \
    \
    -rtc base=utc \
    -serial stdio \
    -no-reboot

# Limpiar archivo temporal
rm -f /tmp/qemu_entropy

echo ""
echo "✅ QEMU terminado"

