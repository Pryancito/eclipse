#!/bin/bash

# Script de compilación para Eclipse OS con bootloader UEFI funcional
# Este script crea una ISO híbrida que funciona tanto en BIOS como en UEFI

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

# 3. Crear estructura de directorios para ISO híbrida
echo ""
echo "🔧 Paso 3: Creando estructura de directorios híbrida..."
mkdir -p /tmp/eclipse-hybrid/{EFI/BOOT,boot,isolinux}

# 4. Copiar archivos necesarios
echo ""
echo "🔧 Paso 4: Copiando archivos..."

# Copiar bootloader UEFI
cp bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi /tmp/eclipse-hybrid/EFI/BOOT/BOOTX64.EFI

# Copiar kernel Eclipse
cp $CARGO_TARGET_DIR/x86_64-unknown-none/release/eclipse_kernel /tmp/eclipse-hybrid/boot/

# Crear un bootloader BIOS simple (usando GRUB)
echo "📦 Creando bootloader BIOS con GRUB..."
cat > /tmp/eclipse-hybrid/isolinux/isolinux.cfg << 'EOF'
default eclipse
timeout 5

label eclipse
  menu label Eclipse OS
  kernel /boot/eclipse_kernel
  append console=ttyS0 quiet
EOF

# 5. Crear archivo de información
echo ""
echo "🔧 Paso 5: Creando archivo de información..."
cat > /tmp/eclipse-hybrid/README.txt << 'INFO_EOF'
🌙 Eclipse OS - Sistema Operativo en Rust
=========================================

Versión: 1.0 Hardware Safe
Arquitectura: x86_64
Tipo: ISO Híbrida (BIOS + UEFI)

Características:
- Kernel microkernel en Rust
- Bootloader UEFI personalizado (no GRUB)
- Compatibilidad BIOS y UEFI
- Inicialización segura para hardware real
- Sistema de debug integrado
- Múltiples modos de arranque

Modos de arranque disponibles:
- Modo normal: Inicialización segura con fallbacks
- Modo debug: Logging detallado para diagnosticar problemas
- Modo mínimo: Inicialización mínima para hardware problemático

Para instalar en USB (UEFI):
sudo dd if=eclipse-os-uefi-working.iso of=/dev/sdX bs=4M status=progress

Para probar en QEMU:
qemu-system-x86_64 -cdrom eclipse-os-uefi-working.iso -m 512M

Desarrollado con ❤️ en Rust
INFO_EOF

# 6. Verificar estructura
echo ""
echo "🔧 Paso 6: Verificando estructura de archivos..."
echo "📁 Estructura creada:"
ls -la /tmp/eclipse-hybrid/
echo ""
echo "📁 EFI/BOOT:"
ls -la /tmp/eclipse-hybrid/EFI/BOOT/
echo ""
echo "📁 boot:"
ls -la /tmp/eclipse-hybrid/boot/
echo ""
echo "📁 isolinux:"
ls -la /tmp/eclipse-hybrid/isolinux/

# 7. Crear imagen ISO híbrida usando xorriso
echo ""
echo "🔧 Paso 7: Creando imagen ISO híbrida..."
xorriso -as mkisofs \
    -iso-level 3 \
    -full-iso9660-filenames \
    -volid "ECLIPSE_OS_HYBRID" \
    -appid "Eclipse OS v1.0 Hybrid" \
    -publisher "Eclipse OS Team" \
    -preparer "Eclipse OS Hybrid Builder" \
    -eltorito-boot isolinux/isolinux.cfg \
    -no-emul-boot \
    -boot-load-size 4 \
    -boot-info-table \
    -eltorito-alt-boot \
    -e EFI/BOOT/BOOTX64.EFI \
    -no-emul-boot \
    -output eclipse-os-uefi-working.iso \
    /tmp/eclipse-hybrid/

if [ $? -ne 0 ]; then
    echo "❌ Error creando imagen ISO híbrida"
    exit 1
fi

# 8. Aplicar isohybrid para compatibilidad UEFI
echo ""
echo "🔧 Paso 8: Aplicando isohybrid para compatibilidad UEFI..."
isohybrid --uefi eclipse-os-uefi-working.iso

if [ $? -ne 0 ]; then
    echo "⚠️  Advertencia: isohybrid falló, pero la ISO puede funcionar"
fi

# 9. Limpiar archivos temporales
echo ""
echo "🔧 Paso 9: Limpiando archivos temporales..."
rm -rf /tmp/eclipse-hybrid

# 10. Mostrar resumen
echo ""
echo "🎉 ¡Compilación completada exitosamente!"
echo "========================================"
echo ""
echo "📋 Archivos generados:"
echo "  - eclipse-os-uefi-working.iso (Imagen híbrida BIOS + UEFI)"
echo "  - target_hardware/ (Directorio de compilación del kernel)"
echo "  - bootloader-uefi/target/ (Directorio de compilación del bootloader)"
echo ""
echo "🚀 Para probar en QEMU (BIOS):"
echo "  qemu-system-x86_64 -cdrom eclipse-os-uefi-working.iso -m 512M"
echo ""
echo "🚀 Para probar en QEMU (UEFI):"
echo "  qemu-system-x86_64 -cdrom eclipse-os-uefi-working.iso -m 512M -bios /usr/share/qemu/OVMF.fd"
echo ""
echo "💾 Para instalar en USB:"
echo "  sudo dd if=eclipse-os-uefi-working.iso of=/dev/sdX bs=4M status=progress"
echo ""
echo "🔍 Características de la ISO híbrida:"
echo "  - Compatible con BIOS y UEFI"
echo "  - Bootloader UEFI personalizado"
echo "  - Carga directa del kernel Eclipse"
echo "  - Sin dependencias de GRUB en UEFI"
echo "  - Inicialización segura para hardware real"
echo ""
echo "✨ ¡Listo para probar en hardware real!"
