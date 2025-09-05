#!/bin/bash

echo "🚀 Eclipse OS - Generador de Imágenes"
echo "====================================="
echo ""

# Verificar dependencias
echo "🔍 Verificando dependencias..."
command -v grub-mkrescue >/dev/null 2>&1 || { echo "❌ grub-mkrescue no encontrado. Instalar: sudo apt install grub-pc-bin"; exit 1; }
command -v qemu-img >/dev/null 2>&1 || { echo "❌ qemu-img no encontrado. Instalar: sudo apt install qemu-utils"; exit 1; }
[ -x /sbin/parted ] || { echo "❌ parted no encontrado. Instalar: sudo apt install parted"; exit 1; }
[ -x /usr/sbin/mkfs.fat ] || { echo "❌ mkfs.fat no encontrado. Instalar: sudo apt install dosfstools"; exit 1; }

echo "✅ Todas las dependencias están disponibles"
echo ""

# Verificar que los archivos necesarios existen
echo "🔍 Verificando archivos del sistema..."
if [ ! -f "boot/eclipse_kernel" ]; then
    echo "❌ eclipse_kernel no encontrado. Ejecutar build_complete.sh primero"
    exit 1
fi

if [ ! -f "efi/boot/bootx64.efi" ]; then
    echo "❌ bootx64.efi no encontrado. Ejecutar build_complete.sh primero"
    exit 1
fi

echo "✅ Archivos del sistema encontrados"
echo ""

# Menú de opciones
echo "Selecciona qué imagen crear:"
echo "1) ISO Live (para USB y hardware real)"
echo "2) Imagen QEMU (para emulación)"
echo "3) Ambas imágenes"
echo "4) Salir"
echo ""
read -p "Opción [1-4]: " choice

case $choice in
    1)
        echo ""
        echo "🔧 Creando imagen ISO Live..."
        ./create_iso.sh
        ;;
    2)
        echo ""
        echo "🔧 Creando imagen QEMU..."
        ./create_qemu_image.sh
        ;;
    3)
        echo ""
        echo "🔧 Creando ambas imágenes..."
        echo ""
        echo "📀 Generando ISO Live..."
        ./create_iso.sh
        echo ""
        echo "💾 Generando imagen QEMU..."
        ./create_qemu_image.sh
        ;;
    4)
        echo "👋 ¡Hasta luego!"
        exit 0
        ;;
    *)
        echo "❌ Opción inválida"
        exit 1
        ;;
esac

echo ""
echo "🎉 ¡Proceso completado!"
echo ""
echo "📋 Resumen de archivos creados:"
ls -lh *.iso *.qcow2 2>/dev/null || echo "No se encontraron imágenes"
echo ""
echo "🚀 Comandos de prueba:"
echo "   ISO Live: qemu-system-x86_64 -cdrom eclipse-os-live.iso -m 512M"
echo "   QEMU:     ./test_qemu.sh"
echo ""
echo "💾 Para copiar ISO a USB:"
echo "   sudo dd if=eclipse-os-live.iso of=/dev/sdX bs=4M status=progress"
