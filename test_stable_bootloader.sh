#!/bin/bash

# Script para probar el bootloader estable de Eclipse OS
# Este script crea una imagen de prueba y la ejecuta en QEMU

echo "🧪 Prueba del Bootloader Estable de Eclipse OS"
echo "============================================="
echo ""

# Verificar que los archivos necesarios existen
if [ ! -f "target_hardware/x86_64-unknown-none/release/eclipse_kernel" ]; then
    echo "❌ Error: Kernel no encontrado"
    echo "   Ejecuta: cargo build --release"
    exit 1
fi

if [ ! -f "bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi" ]; then
    echo "❌ Error: Bootloader no encontrado"
    echo "   Ejecuta: cd bootloader-uefi && ./build.sh"
    exit 1
fi

echo "🔧 Creando imagen de prueba..."
echo ""

# Crear imagen de prueba
dd if=/dev/zero of=eclipse-test-stable.img bs=1M count=32
LOOP_DEVICE=$(sudo losetup --find --show eclipse-test-stable.img)
echo "📁 Loop device: $LOOP_DEVICE"

# Crear particiones
sudo fdisk $LOOP_DEVICE <<EOF
o
n
p
1
2048
65535
t
c
w
EOF

# Formatear partición
sudo mkfs.fat -F 32 ${LOOP_DEVICE}p1

# Montar partición
mkdir -p /mnt/eclipse-test
sudo mount ${LOOP_DEVICE}p1 /mnt/eclipse-test

# Crear estructura EFI
sudo mkdir -p /mnt/eclipse-test/EFI/BOOT

# Copiar bootloader estable
sudo cp bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi /mnt/eclipse-test/EFI/BOOT/BOOTX64.EFI

# Copiar kernel
sudo cp target_hardware/x86_64-unknown-none/release/eclipse_kernel /mnt/eclipse-test/eclipse_kernel

# Crear archivos de configuración
sudo tee /mnt/eclipse-test/README.txt > /dev/null << 'EOF'
🌙 Eclipse OS - Prueba del Bootloader Estable
============================================

Versión: 1.0 (Estable)
Arquitectura: x86_64
Tipo: Imagen de prueba
Estado: Sin reseteo automático

Características:
- Bootloader UEFI estable
- Sin reseteo automático
- Bucle infinito para mantener el sistema activo
- Mensajes de estado periódicos

Esta es una imagen de prueba para verificar que el bootloader
estable funciona correctamente sin reseteos automáticos.
EOF

# Desmontar y limpiar
sudo umount /mnt/eclipse-test
sudo rmdir /mnt/eclipse-test
sudo losetup -d $LOOP_DEVICE

echo "✅ Imagen de prueba creada: eclipse-test-stable.img"
echo ""

# Mostrar información de la imagen
echo "📊 Información de la imagen:"
ls -lh eclipse-test-stable.img
echo ""

# Ejecutar en QEMU
echo "🚀 Ejecutando en QEMU..."
echo "   (El bootloader estable debería funcionar sin reseteos automáticos)"
echo "   (Presiona Ctrl+C para salir)"
echo ""

qemu-system-x86_64 \
    -bios /usr/share/qemu/OVMF.fd \
    -drive file=eclipse-test-stable.img,format=raw \
    -m 512M \
    -serial stdio

echo ""
echo "🧹 Limpiando archivos de prueba..."
rm -f eclipse-test-stable.img

echo "✅ Prueba completada"
echo ""
echo "💡 Si el bootloader funcionó correctamente:"
echo "  - Deberías haber visto mensajes de estado periódicos"
echo "  - No debería haberse reiniciado automáticamente"
echo "  - El sistema debería haber permanecido activo"
echo ""
echo "🔧 Si necesitas reinstalar en disco real:"
echo "  sudo ./reinstall_stable.sh"

