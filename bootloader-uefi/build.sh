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
    echo "Preparando ESP local (data/EFI/BOOT):"
    ESP_DIR="../data/EFI/BOOT"
    mkdir -p "$ESP_DIR"
    cp target/x86_64-unknown-uefi/release/eclipse-bootloader.efi "$ESP_DIR/BOOTX64.EFI"
    echo "   - Copiado BOOTX64.EFI a $ESP_DIR"
    echo "   - Si deseas un USB: cp -r ../data/EFI /mnt/usb/EFI"
    echo ""
    echo "Rutas donde el bootloader buscará el kernel:"
    echo "   - /eclipse_kernel, /vmlinuz-eclipse"
    echo "   - /boot/(eclipse_kernel|vmlinuz-eclipse)"
    echo "   - /EFI/BOOT/(eclipse_kernel|vmlinuz-eclipse)"
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
