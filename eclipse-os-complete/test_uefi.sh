#!/bin/bash
echo "🔧 Iniciando Eclipse OS v0.4.0 en modo UEFI..."
echo "Presiona Ctrl+Alt+G para liberar el mouse de QEMU"
echo "Presiona Ctrl+Alt+Q para salir de QEMU"
echo ""

# Verificar que QEMU esté disponible
if ! command -v qemu-system-x86_64 &> /dev/null; then
    echo "❌ Error: QEMU no está instalado"
    echo "   Instala QEMU para poder probar el sistema"
    exit 1
fi

# Ejecutar QEMU en modo UEFI
qemu-system-x86_64 \
    -machine q35 \
    -cpu qemu64 \
    -m 1G \
    -drive file=eclipse-os.img,format=raw \
    -bios /usr/share/qemu/OVMF.fd \
    -netdev user,id=net0 \
    -device e1000,netdev=net0 \
    -vga std \
    -name "Eclipse OS v0.4.0 UEFI" \
    -no-reboot
