#!/bin/bash

# Script de compilación para el kernel Eclipse compatible con UEFI
# El bootloader UEFI cargará este kernel como un binario ELF

echo "=========================================="
echo "    ECLIPSE KERNEL - UEFI COMPATIBLE"
echo "=========================================="
echo ""

# Verificar que estamos en el directorio correcto
if [ ! -f "Cargo.toml" ]; then
    echo "❌ Error: No se encontró Cargo.toml"
    echo "   Asegúrate de estar en el directorio eclipse_kernel/"
    exit 1
fi

# Compilar el kernel Eclipse (binario ELF)
echo "🔨 Compilando kernel Eclipse (compatible con UEFI)..."
cargo build --release

if [ $? -eq 0 ]; then
    echo ""
    echo "✅ Kernel Eclipse compilado exitosamente!"
    echo ""
    echo "📁 Archivo generado:"
    echo "   - target/x86_64-unknown-linux-gnu/release/eclipse_kernel"
    echo ""
    echo "🚀 Para usar con el bootloader UEFI:"
    echo "   1. El bootloader UEFI cargará este kernel como binario ELF"
    echo "   2. Copia el kernel: cp target/x86_64-unknown-linux-gnu/release/eclipse_kernel ../bootloader-uefi/vmlinuz-eclipse"
    echo "   3. Compila el bootloader: cd ../bootloader-uefi && ./build.sh"
    echo ""
    echo "💡 El kernel Eclipse incluye:"
    echo "   - Sistema de mensajes de boot"
    echo "   - Framework de testing integrado"
    echo "   - Compatibilidad con carga UEFI"
    echo "   - Entorno no_std optimizado"
    echo ""
    echo "📋 Flujo de arranque:"
    echo "   1. UEFI → Bootloader UEFI (BOOTX64.EFI)"
    echo "   2. Bootloader UEFI → Carga kernel Eclipse (eclipse_kernel)"
    echo "   3. Kernel Eclipse → Sistema operativo Eclipse"
else
    echo ""
    echo "❌ Error en la compilación del kernel Eclipse"
    echo "   Revisa los errores mostrados arriba."
    exit 1
fi

echo ""
echo "=========================================="
echo "    KERNEL UEFI COMPATIBLE COMPLETADO"
echo "=========================================="
