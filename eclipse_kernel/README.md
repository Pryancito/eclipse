# Eclipse Microkernel

Microkernel moderno **completo** escrito desde cero en Rust para Eclipse OS.

## Características

### ✅ Completamente Implementado

- **Arquitectura x86_64**: Soporte completo para procesadores de 64 bits
- **Microkernel puro**: Solo funcionalidades esenciales en el kernel
- **Gestión de memoria**: Paginación activa con identity mapping y heap allocator
- **IDT Completa**: 256 entradas con handlers de excepciones e IRQs
- **Context Switching**: Cambio de contexto entre procesos en assembly
- **Scheduler Round-Robin**: Planificador con preemption cada 10 ticks
- **IPC**: Sistema de mensajería entre procesos
- **Compatible con UEFI**: Carga directa desde bootloader UEFI

## Componentes del Sistema

```
kernel/
├── Cargo.toml                    # Configuración del proyecto
├── linker.ld                     # Linker script
├── x86_64-eclipse-microkernel.json  # Target spec
├── build.sh                      # Script de compilación
├── README.md                     # Este archivo
├── IMPLEMENTATION.md             # Guía de implementación
├── TECHNICAL_DOC.md              # Documentación técnica completa
└── src/
    ├── main.rs                   # Entry point (121 líneas)
    ├── lib.rs                    # Library exports (31 líneas)
    ├── boot.rs                   # GDT (61 líneas)
    ├── memory.rs                 # Paginación y heap (208 líneas)
    ├── interrupts.rs             # IDT completa (429 líneas)
    ├── process.rs                # Context switching (273 líneas)
    ├── scheduler.rs              # Scheduling (142 líneas)
    ├── ipc.rs                    # Messaging (308 líneas)
    └── serial.rs                 # Debug output (140 líneas)
```

## Flujo de Arranque

1. **UEFI Bootloader** carga el kernel
2. **GDT** - Cargar Global Descriptor Table
3. **Memoria** - Inicializar heap (2MB)
4. **Paginación** - Activar con identity mapping
5. **IDT** - Cargar Interrupt Descriptor Table
6. **IPC** - Inicializar sistema de mensajes
7. **Scheduler** - Preparar cola de procesos
8. **Main Loop** - Procesar mensajes y hacer scheduling

## Responsabilidades del Microkernel

```bash
cd kernel
./build.sh
```

## Responsabilidades del Microkernel

El microkernel Eclipse maneja **únicamente**:
- ✅ **Gestión de Memoria**: Paginación, heap, allocator
- ✅ **Interrupciones**: IDT, exception handlers, IRQ handlers
- ✅ **IPC**: Sistema de mensajería entre procesos
- ✅ **Scheduling**: Planificación de procesos con preemption
- ✅ **Context Switching**: Cambio de contexto entre procesos

Todos los demás servicios se ejecutan como servidores en espacio de usuario:
- FileSystem, Network, Graphics, Audio, Input, AI, Security

## Compilación

```bash
cd kernel
cargo +nightly build --target x86_64-unknown-none --release
```

El binario se genera en:
```
target/x86_64-unknown-none/release/eclipse_microkernel
```

**Tamaño**: ~905 KB (release, LTO enabled)

## Testing y Debug

### Ejecutar en QEMU

```bash
qemu-system-x86_64 \
    -kernel target/x86_64-unknown-none/release/eclipse_microkernel \
    -serial stdio \
    -no-reboot \
    -no-shutdown
```

### Output Serial

Todos los mensajes de debug se envían a COM1 (0x3F8):
- Mensajes de inicialización
- Información de procesos
- Estadísticas del sistema

## Estadísticas

### Compilación
- **Lenguaje**: 100% Rust (no_std)
- **Líneas**: ~1713 líneas de código
- **Binary**: 905 KB (release)
- **Optimización**: LTO, size-optimized

### Runtime
- **Procesos**: Máximo 64
- **Context switches**: Sin límite
- **Timer**: Interrupciones cada ~18ms
- **Preemption**: Cada 10 ticks (~180ms)

## Características Técnicas

### Seguridad
- Stack alineado en handlers (16 bytes)
- Procesos con stacks dedicados
- Exception handling completo

### Performance
- Context switch en ~1000 ciclos
- Zero-copy message passing
- Huge pages (2MB) para rendimiento

### Compatibilidad
- UEFI bootloader compatible
- x86-64 calling convention
- ABI-compliant interrupt handlers

Este microkernel es compatible con el bootloader UEFI existente en `bootloader-uefi/`.
El bootloader pasa la información del framebuffer al kernel en el parámetro del punto de entrada.
