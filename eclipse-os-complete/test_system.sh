#!/bin/bash
echo "🧪 Iniciando Eclipse OS completo en QEMU..."
echo "Presiona Ctrl+Alt+G para liberar el mouse de QEMU"
echo "Presiona Ctrl+Alt+Q para salir de QEMU"
echo ""

# Ejecutar QEMU con el sistema completo
qemu-system-x86_64 \
    -machine q35 \
    -cpu qemu64 \
    -m 1G \
    -drive file=eclipse-os.img,format=raw \
    -netdev user,id=net0 \
    -device e1000,netdev=net0 \
    -vga std \
    -serial mon:stdio \
    -monitor none \
    -name "Eclipse OS Complete" \
    -nographic \
    -no-reboot
