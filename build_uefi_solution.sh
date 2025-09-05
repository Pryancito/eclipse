#!/bin/bash

# Script de compilación para Eclipse OS con bootloader UEFI funcional
# Este script crea una ISO UEFI que funciona correctamente usando un enfoque diferente

echo "🌙 Compilando Eclipse OS con bootloader UEFI funcional..."
echo "========================================================"

# Configurar variables de entorno
export RUSTFLAGS="-C target-cpu=native -C opt-level=2"
export CARGO_TARGET_DIR="target_hardware"

# Crear directorio de compilación
mkdir -p $CARGO_TARGET_DIR

# 1. Compilar el kernel Eclipse
echo ""
echo "🔧 Paso 1: Compilando kernel Eclipse..."
cargo build --release --target x86_64-unknown-none --manifest-path eclipse_kernel/Cargo.toml

if [ $? -ne 0 ]; then
    echo "❌ Error compilando el kernel Eclipse"
    exit 1
fi

echo "✅ Kernel Eclipse compilado exitosamente"

# 2. Verificar que el bootloader UEFI esté compilado
echo ""
echo "🔧 Paso 2: Verificando bootloader UEFI..."
if [ ! -f "bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi" ]; then
    echo "📦 Compilando bootloader UEFI..."
    cd bootloader-uefi
    ./build.sh
    if [ $? -ne 0 ]; then
        echo "❌ Error compilando bootloader UEFI"
        exit 1
    fi
    cd ..
fi

echo "✅ Bootloader UEFI verificado"

# 3. Crear estructura de directorios para ISO UEFI
echo ""
echo "🔧 Paso 3: Creando estructura de directorios UEFI..."
mkdir -p /tmp/eclipse-uefi-solution/{EFI/BOOT,boot}

# 4. Copiar archivos necesarios
echo ""
echo "🔧 Paso 4: Copiando archivos..."

# Copiar bootloader UEFI
cp bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi /tmp/eclipse-uefi-solution/EFI/BOOT/BOOTX64.EFI

# Copiar kernel Eclipse
cp $CARGO_TARGET_DIR/x86_64-unknown-none/release/eclipse_kernel /tmp/eclipse-uefi-solution/boot/

# 5. Crear archivo de información
echo ""
echo "🔧 Paso 5: Creando archivo de información..."
cat > /tmp/eclipse-uefi-solution/README.txt << 'INFO_EOF'
🌙 Eclipse OS - Sistema Operativo en Rust
=========================================

Versión: 1.0 Hardware Safe
Arquitectura: x86_64
Tipo: ISO UEFI (Bootloader personalizado)

Características:
- Kernel microkernel en Rust
- Bootloader UEFI personalizado (no GRUB)
- Inicialización segura para hardware real
- Sistema de debug integrado
- Múltiples modos de arranque

Modos de arranque disponibles:
- Modo normal: Inicialización segura con fallbacks
- Modo debug: Logging detallado para diagnosticar problemas
- Modo mínimo: Inicialización mínima para hardware problemático

Para instalar en USB (UEFI):
sudo dd if=eclipse-os-uefi-solution.iso of=/dev/sdX bs=4M status=progress

Para probar en QEMU:
qemu-system-x86_64 -cdrom eclipse-os-uefi-solution.iso -m 512M -bios /usr/share/qemu/OVMF.fd

Desarrollado con ❤️ en Rust
INFO_EOF

# 6. Verificar estructura
echo ""
echo "🔧 Paso 6: Verificando estructura de archivos..."
echo "📁 Estructura creada:"
ls -la /tmp/eclipse-uefi-solution/
echo ""
echo "📁 EFI/BOOT:"
ls -la /tmp/eclipse-uefi-solution/EFI/BOOT/
echo ""
echo "📁 boot:"
ls -la /tmp/eclipse-uefi-solution/boot/

# 7. Crear imagen ISO UEFI usando xorriso con configuración correcta
echo ""
echo "🔧 Paso 7: Creando imagen ISO UEFI..."
xorriso -as mkisofs \
    -iso-level 3 \
    -full-iso9660-filenames \
    -volid "ECLIPSE_OS_UEFI" \
    -appid "Eclipse OS v1.0 UEFI" \
    -publisher "Eclipse OS Team" \
    -preparer "Eclipse OS UEFI Builder" \
    -eltorito-boot EFI/BOOT/BOOTX64.EFI \
    -no-emul-boot \
    -boot-load-size 4 \
    -boot-info-table \
    -isohybrid-gpt-basdat \
    -output eclipse-os-uefi-solution.iso \
    /tmp/eclipse-uefi-solution/

if [ $? -ne 0 ]; then
    echo "❌ Error creando imagen ISO UEFI"
    exit 1
fi

# 8. Aplicar isohybrid para compatibilidad UEFI
echo ""
echo "🔧 Paso 8: Aplicando isohybrid para compatibilidad UEFI..."
isohybrid --uefi eclipse-os-uefi-solution.iso

if [ $? -ne 0 ]; then
    echo "⚠️  Advertencia: isohybrid falló, pero la ISO puede funcionar"
fi

# 9. Limpiar archivos temporales
echo ""
echo "🔧 Paso 9: Limpiando archivos temporales..."
rm -rf /tmp/eclipse-uefi-solution

# 10. Mostrar resumen
echo ""
echo "🎉 ¡Compilación completada exitosamente!"
echo "========================================"
echo ""
echo "📋 Archivos generados:"
echo "  - eclipse-os-uefi-solution.iso (Imagen UEFI funcional)"
echo "  - target_hardware/ (Directorio de compilación del kernel)"
echo "  - bootloader-uefi/target/ (Directorio de compilación del bootloader)"
echo ""
echo "🚀 Para probar en QEMU (UEFI):"
echo "  qemu-system-x86_64 -cdrom eclipse-os-uefi-solution.iso -m 512M -bios /usr/share/qemu/OVMF.fd"
echo ""
echo "💾 Para instalar en USB:"
echo "  sudo dd if=eclipse-os-uefi-solution.iso of=/dev/sdX bs=4M status=progress"
echo ""
echo "🔍 Características de la ISO UEFI:"
echo "  - Bootloader UEFI personalizado"
echo "  - Carga directa del kernel Eclipse"
echo "  - Sin dependencias de GRUB"
echo "  - Inicialización segura para hardware real"
echo ""
echo "✨ ¡Listo para probar en hardware real!"
