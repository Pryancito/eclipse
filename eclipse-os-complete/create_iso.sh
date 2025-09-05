#!/bin/bash

echo "🔧 Creando imagen ISO booteable para Eclipse OS..."

# Crear directorio de trabajo
mkdir -p iso_build/{boot/grub,efi/boot}

# Copiar archivos necesarios
cp boot/eclipse_kernel iso_build/boot/
cp efi/boot/bootx64.efi iso_build/efi/boot/

# Crear configuración GRUB
cat > iso_build/boot/grub/grub.cfg << 'GRUB_EOF'
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

# Crear archivo de información del sistema
cat > iso_build/README.txt << 'INFO_EOF'
Eclipse OS - Sistema Operativo en Rust
=====================================

Versión: 1.0
Arquitectura: x86_64
Tipo: Live System

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

Para instalar en USB:
1. Usar dd: sudo dd if=eclipse-os.iso of=/dev/sdX bs=4M status=progress
2. O usar herramientas como Rufus, Etcher, etc.

Para probar en QEMU:
qemu-system-x86_64 -cdrom eclipse-os.iso -m 512M

Desarrollado con ❤️ en Rust
INFO_EOF

# Crear el ISO usando GRUB
echo "📀 Generando imagen ISO..."
grub-mkrescue -o eclipse-os-live.iso iso_build/

if [ $? -eq 0 ]; then
    echo "✅ ISO creado exitosamente: eclipse-os-live.iso"
    echo "📊 Tamaño del archivo:"
    ls -lh eclipse-os-live.iso
    echo ""
    echo "🚀 Para probar en QEMU:"
    echo "   qemu-system-x86_64 -cdrom eclipse-os-live.iso -m 512M"
    echo ""
    echo "💾 Para copiar a USB:"
    echo "   sudo dd if=eclipse-os-live.iso of=/dev/sdX bs=4M status=progress"
else
    echo "❌ Error al crear el ISO"
    exit 1
fi
