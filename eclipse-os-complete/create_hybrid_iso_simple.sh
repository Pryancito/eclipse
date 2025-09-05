#!/bin/bash

echo "🔧 Creando ISO híbrido booteable para Eclipse OS (UEFI + BIOS)"
echo "=============================================================="

# Crear directorio de trabajo para ISO híbrido
mkdir -p hybrid_iso/{boot/grub,EFI/BOOT}

# Copiar archivos necesarios
cp boot/eclipse_kernel hybrid_iso/boot/
cp efi/boot/bootx64.efi hybrid_iso/EFI/BOOT/

# Crear configuración GRUB para BIOS
cat > hybrid_iso/boot/grub/grub.cfg << 'GRUB_EOF'
set timeout=5
set default=0

menuentry "Eclipse OS - Live System" {
    multiboot /boot/eclipse_kernel
    boot
}

menuentry "Eclipse OS - Safe Mode" {
    multiboot /boot/eclipse_kernel safe_mode=1
    boot
}

menuentry "Eclipse OS - Debug Mode" {
    multiboot /boot/eclipse_kernel debug=1
    boot
}
GRUB_EOF

# Crear configuración GRUB para UEFI
cat > hybrid_iso/EFI/BOOT/grub.cfg << 'UEFI_EOF'
set timeout=5
set default=0

menuentry "Eclipse OS - UEFI Live System" {
    multiboot /boot/eclipse_kernel
    boot
}

menuentry "Eclipse OS - UEFI Safe Mode" {
    multiboot /boot/eclipse_kernel safe_mode=1
    boot
}

menuentry "Eclipse OS - UEFI Debug Mode" {
    multiboot /boot/eclipse_kernel debug=1
    boot
}
UEFI_EOF

# Crear archivo de información
cat > hybrid_iso/README.txt << 'INFO_EOF'
Eclipse OS - Sistema Operativo en Rust
=====================================

Versión: 1.0
Arquitectura: x86_64
Tipo: ISO Híbrido (UEFI + BIOS)

Características:
- Kernel microkernel en Rust
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

Para instalar en USB (UEFI/BIOS):
sudo dd if=eclipse-os-hybrid.iso of=/dev/sdX bs=4M status=progress

Para probar en QEMU:
qemu-system-x86_64 -cdrom eclipse-os-hybrid.iso -m 512M

Desarrollado con ❤️ en Rust
INFO_EOF

# Crear el ISO usando grub-mkrescue
echo "📀 Generando ISO híbrido con grub-mkrescue..."
grub-mkrescue -o eclipse-os-hybrid.iso hybrid_iso/

if [ $? -eq 0 ]; then
    echo "✅ ISO híbrido creado exitosamente: eclipse-os-hybrid.iso"
    echo "📊 Tamaño del archivo:"
    ls -lh eclipse-os-hybrid.iso
    echo ""
    echo "🔧 Aplicando isohybrid para compatibilidad UEFI/BIOS..."
    isohybrid eclipse-os-hybrid.iso
    echo ""
    echo "🚀 Para probar en QEMU:"
    echo "   qemu-system-x86_64 -cdrom eclipse-os-hybrid.iso -m 512M"
    echo ""
    echo "💾 Para copiar a USB (UEFI/BIOS):"
    echo "   sudo dd if=eclipse-os-hybrid.iso of=/dev/sdX bs=4M status=progress"
    echo ""
    echo "🔍 Verificar que es híbrido:"
    echo "   file eclipse-os-hybrid.iso"
else
    echo "❌ Error al crear el ISO híbrido"
    exit 1
fi
