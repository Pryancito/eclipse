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
