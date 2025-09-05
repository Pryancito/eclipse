#!/bin/bash

echo "🚀 Eclipse OS - Opciones de Prueba"
echo "=================================="
echo ""

echo "Archivos disponibles:"
ls -lh *.iso *.qcow2 2>/dev/null || echo "No se encontraron imágenes"
echo ""

echo "Opciones de prueba disponibles:"
echo "1) ISO Live en QEMU (bootloader completo)"
echo "2) Kernel directo en QEMU (modo simplificado)"
echo "3) Ver información del sistema"
echo "4) Salir"
echo ""

read -p "Selecciona una opción [1-4]: " choice

case $choice in
    1)
        echo ""
        echo "🚀 Iniciando ISO Live en QEMU..."
        if [ -f "eclipse-os-live.iso" ]; then
            qemu-system-x86_64 -cdrom eclipse-os-live.iso -m 512M -no-reboot
        else
            echo "❌ eclipse-os-live.iso no encontrado. Ejecutar build_images.sh primero"
        fi
        ;;
    2)
        echo ""
        echo "🚀 Iniciando kernel directo en QEMU..."
        if [ -f "boot/eclipse_kernel" ]; then
            ./test_simple_qemu.sh
        else
            echo "❌ eclipse_kernel no encontrado. Ejecutar build_complete.sh primero"
        fi
        ;;
    3)
        echo ""
        echo "📊 Información del sistema Eclipse OS:"
        echo "====================================="
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
        echo "Imágenes:"
        ls -lh *.iso *.qcow2 2>/dev/null || echo "No se encontraron imágenes"
        echo ""
        echo "Scripts disponibles:"
        ls -lh *.sh 2>/dev/null || echo "No se encontraron scripts"
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
