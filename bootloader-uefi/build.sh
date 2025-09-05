#!/bin/bash

# Script de compilación para el bootloader UEFI simplificado de Eclipse OS
# Basado en el enfoque de Redox OS

echo "=========================================="
echo "    ECLIPSE BOOTLOADER UEFI - SIMPLIFICADO"
echo "    (Enfoque Redox OS)"
echo "=========================================="
echo ""

# Verificar que estamos en el directorio correcto
if [ ! -f "Cargo.toml" ]; then
    echo "❌ Error: No se encontró Cargo.toml"
    echo "   Asegúrate de estar en el directorio bootloader-uefi/"
    exit 1
fi

# Instalar dependencias si es necesario
echo "📦 Verificando dependencias..."
cargo check --target x86_64-unknown-uefi

if [ $? -ne 0 ]; then
    echo "🔧 Instalando dependencias UEFI..."
    rustup target add x86_64-unknown-uefi
    cargo install cargo-xbuild
fi

# Compilar el bootloader simplificado
echo ""
echo "🔨 Compilando bootloader UEFI simplificado..."
cargo build --release --target x86_64-unknown-uefi

if [ $? -eq 0 ]; then
    echo ""
    echo "✅ Bootloader UEFI simplificado compilado exitosamente!"
    echo ""
    echo "📁 Archivos generados:"
    echo "   - target/x86_64-unknown-uefi/release/eclipse-bootloader.efi"
    echo ""
    echo "🚀 Para instalar en un USB UEFI:"
    echo "   1. Monta tu USB en /mnt/usb"
    echo "   2. Crea la estructura: mkdir -p /mnt/usb/EFI/BOOT"
    echo "   3. Copia el bootloader: cp target/x86_64-unknown-uefi/release/eclipse-bootloader.efi /mnt/usb/EFI/BOOT/BOOTX64.EFI"
    echo "   4. Copia el kernel: cp ../eclipse_kernel/target/x86_64-unknown-linux-gnu/release/eclipse_kernel /mnt/usb/vmlinuz-eclipse"
    echo ""
    echo "💡 El bootloader simplificado buscará el kernel en:"
    echo "   - /vmlinuz-eclipse"
    echo "   - /boot/vmlinuz-eclipse"
    echo "   - /EFI/BOOT/vmlinuz-eclipse"
    echo "   - /eclipse_kernel"
else
    echo ""
    echo "❌ Error en la compilación del bootloader UEFI"
    echo "   Revisa los errores mostrados arriba."
    exit 1
fi

echo ""
echo "=========================================="
echo "    BUILD COMPLETADO"
echo "=========================================="
