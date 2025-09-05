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
# Compilar como ELF para x86_64-unknown-none usando linker.ld
RUSTFLAGS="-Clink-arg=-Tlinker.ld" cargo build --release --target x86_64-unknown-none

if [ $? -eq 0 ]; then
    echo ""
    echo "✅ Kernel Eclipse compilado exitosamente!"
    echo ""
    echo "📁 Archivo generado:"
    echo "   - target/x86_64-unknown-none/release/eclipse_kernel"
    echo ""
    echo "🚀 Preparando copia a la partición ESP local de datos (data/EFI/BOOT):"
    ESP_DIR="../data/EFI/BOOT"
    mkdir -p "$ESP_DIR"
    KERNEL_OUT="target/x86_64-unknown-none/release/eclipse_kernel"
    # Copiar con varios nombres que el bootloader busca
    cp "$KERNEL_OUT" "$ESP_DIR/eclipse_kernel" 2>/dev/null || true
    cp "$KERNEL_OUT" "$ESP_DIR/vmlinuz-eclipse" 2>/dev/null || true
    cp "$KERNEL_OUT" "../bootloader-uefi/vmlinuz-eclipse" 2>/dev/null || true
    echo "   - Copiado a $ESP_DIR/eclipse_kernel y $ESP_DIR/vmlinuz-eclipse"
    echo "   - Copiado también a ../bootloader-uefi/vmlinuz-eclipse"
    echo "   3. Luego compila el bootloader: cd ../bootloader-uefi && ./build.sh"
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
