#!/bin/bash

echo "🚀 Eclipse OS - Probador de ISO Híbrido"
echo "======================================="
echo ""

echo "Archivos disponibles:"
ls -lh *.iso *.qcow2 2>/dev/null || echo "No se encontraron imágenes"
echo ""

echo "Opciones de prueba:"
echo "1) ISO Híbrido en QEMU (UEFI + BIOS)"
echo "2) ISO Híbrido en QEMU (solo UEFI)"
echo "3) ISO Híbrido en QEMU (solo BIOS)"
echo "4) Kernel directo en QEMU"
echo "5) Ver información detallada"
echo "6) Salir"
echo ""

read -p "Selecciona una opción [1-6]: " choice

case $choice in
    1)
        echo ""
        echo "🚀 Iniciando ISO Híbrido en QEMU (UEFI + BIOS)..."
        if [ -f "eclipse-os-hybrid.iso" ]; then
            qemu-system-x86_64 -cdrom eclipse-os-hybrid.iso -m 512M -no-reboot
        else
            echo "❌ eclipse-os-hybrid.iso no encontrado"
        fi
        ;;
    2)
        echo ""
        echo "🚀 Iniciando ISO Híbrido en QEMU (solo UEFI)..."
        if [ -f "eclipse-os-hybrid.iso" ]; then
            qemu-system-x86_64 -cdrom eclipse-os-hybrid.iso -m 512M -no-reboot -bios /usr/share/qemu/OVMF.fd
        else
            echo "❌ eclipse-os-hybrid.iso no encontrado"
        fi
        ;;
    3)
        echo ""
        echo "🚀 Iniciando ISO Híbrido en QEMU (solo BIOS)..."
        if [ -f "eclipse-os-hybrid.iso" ]; then
            qemu-system-x86_64 -cdrom eclipse-os-hybrid.iso -m 512M -no-reboot -machine pc
        else
            echo "❌ eclipse-os-hybrid.iso no encontrado"
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
        echo "ISO Híbrido:"
        if [ -f "eclipse-os-hybrid.iso" ]; then
            ls -lh eclipse-os-hybrid.iso
            echo "Tipo: $(file eclipse-os-hybrid.iso)"
            echo ""
            echo "Contenido del ISO:"
            isoinfo -l -i eclipse-os-hybrid.iso 2>/dev/null || echo "isoinfo no disponible"
        else
            echo "❌ No encontrado"
        fi
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
        ;;
    6)
        echo "👋 ¡Hasta luego!"
        exit 0
        ;;
    *)
        echo "❌ Opción inválida"
        exit 1
        ;;
esac
