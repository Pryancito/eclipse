#!/bin/bash

echo "🚀 Eclipse OS - Probador Universal"
echo "================================="
echo ""

echo "Archivos disponibles:"
ls -lh *.iso *.img *.qcow2 2>/dev/null || echo "No se encontraron imágenes"
echo ""

echo "Opciones de prueba:"
echo "1) ISO Híbrido en QEMU (GRUB)"
echo "2) ISO UEFI en QEMU (nuestro bootloader)"
echo "3) Imagen de disco UEFI en QEMU (recomendado)"
echo "4) Kernel directo en QEMU"
echo "5) Ver información detallada"
echo "6) Crear todas las imágenes"
echo "7) Salir"
echo ""

read -p "Selecciona una opción [1-7]: " choice

case $choice in
    1)
        echo ""
        echo "🚀 Iniciando ISO Híbrido en QEMU..."
        if [ -f "eclipse-os-hybrid.iso" ]; then
            qemu-system-x86_64 -cdrom eclipse-os-hybrid.iso -m 512M -no-reboot
        else
            echo "❌ eclipse-os-hybrid.iso no encontrado"
        fi
        ;;
    2)
        echo ""
        echo "🚀 Iniciando ISO UEFI en QEMU..."
        if [ -f "eclipse-os-uefi.iso" ]; then
            qemu-system-x86_64 -cdrom eclipse-os-uefi.iso -m 512M -no-reboot
        else
            echo "❌ eclipse-os-uefi.iso no encontrado"
        fi
        ;;
    3)
        echo ""
        echo "🚀 Iniciando imagen de disco UEFI en QEMU..."
        if [ -f "eclipse-os-uefi-final.img" ]; then
            ./test_uefi_final.sh
        else
            echo "❌ eclipse-os-uefi-final.img no encontrado"
        fi
        ;;
    4)
        echo ""
        echo "🚀 Iniciando kernel directo en QEMU..."
        if [ -f "boot/eclipse_kernel" ]; then
            ./test_simple_qemu.sh
        else
            echo "❌ eclipse_kernel no encontrado"
        fi
        ;;
    5)
        echo ""
        echo "📊 Información detallada del sistema:"
        echo "===================================="
        echo ""
        echo "Imágenes disponibles:"
        for img in *.iso *.img *.qcow2; do
            if [ -f "$img" ]; then
                echo "  $img: $(ls -lh "$img" | awk '{print $5}')"
            fi
        done
        echo ""
        echo "Kernel:"
        if [ -f "boot/eclipse_kernel" ]; then
            ls -lh boot/eclipse_kernel
            echo "Tamaño: $(stat -c%s boot/eclipse_kernel) bytes"
        else
            echo "❌ No encontrado"
        fi
        echo ""
        echo "Bootloader UEFI:"
        if [ -f "efi/boot/bootx64.efi" ]; then
            ls -lh efi/boot/bootx64.efi
            echo "Tamaño: $(stat -c%s efi/boot/bootx64.efi) bytes"
        else
            echo "❌ No encontrado"
        fi
        echo ""
        echo "Scripts disponibles:"
        ls -lh *.sh 2>/dev/null || echo "No se encontraron scripts"
        ;;
    6)
        echo ""
        echo "🔧 Creando todas las imágenes..."
        echo ""
        echo "📀 Creando ISO híbrido..."
        ./create_hybrid_iso_simple.sh
        echo ""
        echo "📀 Creando ISO UEFI..."
        ./create_uefi_iso.sh
        echo ""
        echo "💾 Creando imagen de disco UEFI..."
        ./create_uefi_disk_final.sh
        echo ""
        echo "✅ Todas las imágenes creadas"
        ;;
    7)
        echo "👋 ¡Hasta luego!"
        exit 0
        ;;
    *)
        echo "❌ Opción inválida"
        exit 1
        ;;
esac
