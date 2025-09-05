#!/bin/bash
echo "🧪 Iniciando Eclipse OS Kernel en QEMU..."
echo "Presiona Ctrl+Alt+G para liberar el mouse de QEMU"
echo "Presiona Ctrl+Alt+Q para salir de QEMU"
echo ""

# Crear un disco virtual simple
echo "Creando disco virtual..."
dd if=/dev/zero of=eclipse_disk.img bs=1M count=10 2>/dev/null

# Ejecutar QEMU con el kernel como un binario ejecutable
echo "Iniciando QEMU..."
qemu-system-x86_64 \
    -machine q35 \
    -cpu qemu64 \
    -m 512M \
    -drive file=eclipse_disk.img,format=raw \
    -kernel eclipse_kernel \
    -netdev user,id=net0 \
    -device e1000,netdev=net0 \
    -vga std \
    -serial mon:stdio \
    -monitor none \
    -name "Eclipse OS Kernel" \
    -nographic \
    -no-reboot
