#!/bin/bash

echo "🚀 Iniciando Eclipse OS con imagen de disco mejorada..."
echo "Presiona Ctrl+Alt+G para liberar el mouse de QEMU"
echo "Presiona Ctrl+Alt+Q para salir de QEMU"
echo ""

# Verificar que la imagen existe
if [ ! -f "eclipse-os.img" ]; then
    echo "❌ Error: eclipse-os.img no encontrada"
    exit 1
fi

# Verificar el tamaño de la imagen
echo "📊 Información de la imagen:"
ls -lh eclipse-os.img
echo ""

# Verificar que el bootloader existe
if [ ! -f "efi/boot/bootx64.efi" ]; then
    echo "❌ Error: bootx64.efi no encontrado"
    exit 1
fi

echo "✅ Archivos verificados correctamente"
echo ""

# Ejecutar QEMU con más opciones de debug
echo "🖥️  Iniciando QEMU..."
qemu-system-x86_64 \
    -drive format=raw,file=eclipse-os.img \
    -m 512M \
    -serial mon:stdio \
    -monitor stdio \
    -vga std \
    -debugcon file:debug.log \
    -d guest_errors \
    -no-reboot \
    -no-shutdown

