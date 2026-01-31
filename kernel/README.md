# Eclipse Microkernel

Microkernel moderno escrito desde cero en Rust para Eclipse OS.

## Características

- **Arquitectura x86_64**: Soporte completo para procesadores de 64 bits
- **Microkernel puro**: Solo funcionalidades esenciales en el kernel
- **Gestión de memoria**: Paginación y heap allocator
- **Interrupciones**: IDT completa con handlers de excepciones e IRQs
- **IPC**: Sistema de mensajería entre procesos
- **Compatible con UEFI**: Carga directa desde bootloader UEFI

## Estructura

```
kernel/
├── src/
│   ├── main.rs          # Punto de entrada
│   ├── lib.rs           # Biblioteca del kernel
│   ├── boot.rs          # GDT y configuración inicial
│   ├── memory.rs        # Gestión de memoria
│   ├── interrupts.rs    # IDT y handlers
│   ├── ipc.rs           # Sistema IPC
│   └── serial.rs        # Serial para debugging
├── Cargo.toml           # Configuración del proyecto
├── linker.ld            # Linker script
└── build.sh             # Script de compilación
```

## Compilación

```bash
cd kernel
./build.sh
```

## Responsabilidades del Microkernel

El microkernel Eclipse maneja únicamente:
- **Gestión de Memoria**: Paginación, heap, asignación de memoria
- **IPC**: Sistema de mensajería entre procesos
- **Interrupciones**: Manejo de interrupciones del hardware
- **Scheduling Básico**: (Pendiente de implementación)

Todos los demás servicios se ejecutan como servidores en espacio de usuario.

## Compatibilidad

Este microkernel es compatible con el bootloader UEFI existente en `bootloader-uefi/`.
El bootloader pasa la información del framebuffer al kernel en el parámetro del punto de entrada.
