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
