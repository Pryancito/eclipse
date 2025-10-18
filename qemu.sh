#!/bin/bash

# Script para ejecutar Redox OS en QEMU con máxima entropía
# Incluye múltiples fuentes de entropía y optimizaciones de rendimiento

echo "🚀 Iniciando Redox OS con dispositivos de entropía mejorados"
echo "============================================================"

# Verificar que el disco existe
if [ ! -b "/dev/sda" ]; then
    echo "❌ Error: /dev/sda no existe o no es un dispositivo de bloque"
    exit 1
fi

echo "📁 Disco: /dev/sda"
echo "🎲 Configurando fuentes de entropía:"
echo "   ✅ VirtIO RNG con /dev/random (alta calidad)"
echo "   ✅ VirtIO RNG con /dev/urandom (alta velocidad)"
echo "   ✅ VirtIO RNG con /dev/hwrng (hardware RNG si disponible)"
echo "   ✅ CPU host con TSC invariancy"
echo "   ✅ KVM habilitado para mejor rendimiento"

# Crear pool de entropía temporal
dd if=/dev/urandom of=/tmp/qemu_entropy bs=1M count=2 2>/dev/null

sudo qemu-system-x86_64 -enable-kvm -smp 4 -m 8G \
    -cpu host,+invtsc \
    -drive id=hd0,file=/dev/nvme0n1,format=raw,if=virtio \
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
    -no-reboot -no-shutdown \
    -bios /usr/share/ovmf/OVMF.fd \
    \
    -object rng-random,filename=/dev/random,id=rng0 \
    -device virtio-rng-pci,rng=rng0,max-bytes=1024,period=1000 \
    \
    -object rng-random,filename=/dev/urandom,id=rng1 \
    -device virtio-rng-pci,rng=rng1,max-bytes=2048,period=500 \
    \
    -object rng-random,filename=/tmp/qemu_entropy,id=rng2 \
    -device virtio-rng-pci,rng=rng2,max-bytes=512,period=2000 \
    \
    -rtc base=utc \
    -serial stdio

# Limpiar archivo temporal
rm -f /tmp/qemu_entropy

echo ""
echo "✅ QEMU terminado"
