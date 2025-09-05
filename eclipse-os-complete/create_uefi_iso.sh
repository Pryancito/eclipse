#!/bin/bash

echo "🔧 Creando ISO UEFI con nuestro bootloader personalizado"
echo "======================================================="

# Crear directorio de trabajo para ISO UEFI
mkdir -p uefi_iso/{EFI/BOOT,boot}

# Copiar nuestro bootloader UEFI
cp efi/boot/bootx64.efi uefi_iso/EFI/BOOT/

# Copiar nuestro kernel
cp boot/eclipse_kernel uefi_iso/boot/

# Crear archivo de información
cat > uefi_iso/README.txt << 'INFO_EOF'
Eclipse OS - Sistema Operativo en Rust
=====================================

Versión: 1.0
Arquitectura: x86_64
Tipo: ISO UEFI (Bootloader personalizado)

Características:
- Kernel microkernel en Rust
- Bootloader UEFI personalizado
- Sistema de memoria avanzado
- Gestión de procesos multitarea
- Sistema de archivos virtual
- Drivers de hardware
- Stack de red completo
- Sistema de seguridad robusto
- Interfaz gráfica con soporte NVIDIA
- Sistema de AI avanzado
- Aplicaciones de usuario integradas

Aplicaciones incluidas:
- Shell interactivo
- Calculadora científica
- Gestor de archivos
- Información del sistema
- Editor de texto
- Gestor de tareas

Para instalar en USB (UEFI):
sudo dd if=eclipse-os-uefi.iso of=/dev/sdX bs=4M status=progress

Para probar en QEMU:
qemu-system-x86_64 -cdrom eclipse-os-uefi.iso -m 512M

Desarrollado con ❤️ en Rust
INFO_EOF

# Crear el ISO UEFI usando xorriso
echo "📀 Generando ISO UEFI..."
xorriso -as mkisofs \
    -iso-level 3 \
    -full-iso9660-filenames \
    -volid "ECLIPSE_OS_UEFI" \
    -appid "Eclipse OS v1.0 UEFI" \
    -publisher "Eclipse OS Team" \
    -preparer "Eclipse OS Builder" \
    -output eclipse-os-uefi.iso \
    uefi_iso/

if [ $? -eq 0 ]; then
    echo "✅ ISO UEFI creado exitosamente: eclipse-os-uefi.iso"
    echo "📊 Tamaño del archivo:"
    ls -lh eclipse-os-uefi.iso
    echo ""
    echo "🔧 Aplicando isohybrid para compatibilidad UEFI..."
    isohybrid --uefi eclipse-os-uefi.iso
    echo ""
    echo "🚀 Para probar en QEMU:"
    echo "   qemu-system-x86_64 -cdrom eclipse-os-uefi.iso -m 512M"
    echo ""
    echo "💾 Para copiar a USB (UEFI):"
    echo "   sudo dd if=eclipse-os-uefi.iso of=/dev/sdX bs=4M status=progress"
    echo ""
    echo "🔍 Verificar que es híbrido UEFI:"
    echo "   file eclipse-os-uefi.iso"
else
    echo "❌ Error al crear el ISO UEFI"
    exit 1
fi
