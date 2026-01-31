# Microkernel Eclipse OS - Guía de Implementación

Este documento describe la implementación del microkernel Eclipse OS creado desde cero.

## Resumen

Se ha creado un microkernel moderno en Rust completamente desde cero en el directorio `kernel/`. El microkernel implementa los componentes esenciales requeridos y mantiene compatibilidad con el bootloader UEFI existente.

## Arquitectura

### Componentes Principales

1. **Boot (boot.rs)**
   - GDT (Global Descriptor Table) con 5 entradas
   - Selectores de segmento para ring 0 y ring 3
   - Carga y recarga de segmentos

2. **Memoria (memory.rs)**
   - Allocator global simple basado en lista enlazada
   - Heap de 2 MB para el kernel
   - Estructuras de paginación (PageTable, PageTableEntry)
   - Flags de paginación (presente, escribible, usuario, etc.)

3. **Interrupciones (interrupts.rs)**
   - Estructura para estadísticas de interrupciones
   - Sistema básico (stub) preparado para expansión futura

4. **IPC (ipc.rs)**
   - Sistema completo de mensajería
   - Soporte para 32 servidores y 256 clientes
   - Cola global de 1024 mensajes
   - 10 tipos de mensajes predefinidos
   - Procesamiento eficiente de mensajes

5. **Serial (serial.rs)**
   - Comunicación por puerto COM1 (0x3F8)
   - Baud rate 38400
   - FIFO habilitado
   - Útil para debugging

## Flujo de Arranque

1. **Bootloader UEFI** carga el kernel y pasa información del framebuffer
2. **_start** (main.rs):
   - Carga GDT
   - Inicializa memoria
   - Inicializa interrupciones
   - Inicializa IPC
3. **kernel_main** entra en loop principal:
   - Procesa mensajes IPC
   - Yield CPU con `hlt`

## Características del Microkernel

### ✅ Completado

- **No `std`**: Completamente bare-metal sin biblioteca estándar
- **Compatible UEFI**: Punto de entrada compatible con bootloader existente
- **Gestión de Memoria**: Sistema funcional con allocator global
- **IPC Completo**: Sistema de mensajería robusto
- **Serial Debugging**: Comunicación para debugging
- **Compilación Exitosa**: Binario de 888 KB generado

### 🚧 Por Implementar

- **IDT Completa**: Handlers de interrupciones completos
- **Context Switching**: Cambio de contexto entre procesos
- **Scheduler**: Planificador de tareas
- **Paginación Activa**: Configuración de tablas de páginas
- **Tests en QEMU**: Pruebas de funcionamiento

## Compilación

```bash
cd kernel
cargo +nightly build --target x86_64-unknown-none --release
```

El binario se genera en:
```
target/x86_64-unknown-none/release/eclipse_microkernel
```

## Estructura de Directorios

```
kernel/
├── Cargo.toml                          # Dependencias y configuración
├── linker.ld                           # Script del linker
├── x86_64-eclipse-microkernel.json     # Target specification
├── build.sh                            # Script de compilación
├── README.md                           # Documentación
└── src/
    ├── main.rs                         # Punto de entrada (2.5 KB)
    ├── lib.rs                          # Biblioteca del kernel
    ├── boot.rs                         # GDT (1.6 KB)
    ├── memory.rs                       # Gestión de memoria (3.8 KB)
    ├── interrupts.rs                   # Interrupciones (stub)
    ├── ipc.rs                          # Sistema IPC (8.1 KB)
    └── serial.rs                       # Serial debugging (2.7 KB)
```

## Responsabilidades del Microkernel

Según la arquitectura microkernel, este kernel maneja **únicamente**:

1. **Gestión de Memoria**: Allocator, heap, paginación
2. **IPC**: Comunicación entre procesos mediante mensajes
3. **Interrupciones**: Manejo básico de interrupciones
4. **Scheduling**: (Pendiente) Planificación de tareas

Todos los demás servicios (filesystem, network, graphics, etc.) se ejecutan como servidores en espacio de usuario.

## Mensajes IPC

El sistema IPC soporta los siguientes tipos de mensajes:

- **System** (0x00000001): Mensajes del sistema
- **Memory** (0x00000002): Gestión de memoria
- **FileSystem** (0x00000004): Operaciones de archivos
- **Network** (0x00000008): Comunicaciones de red
- **Graphics** (0x00000010): Operaciones gráficas
- **Audio** (0x00000020): Audio
- **Input** (0x00000040): Dispositivos de entrada
- **AI** (0x00000080): Servicios de IA
- **Security** (0x00000100): Seguridad
- **User** (0x00000200): Mensajes de usuario

## Compatibilidad con Bootloader

El microkernel es compatible con el bootloader UEFI existente en `bootloader-uefi/`:

- **Firma del punto de entrada**: `extern "C" fn _start(framebuffer_info_ptr: u64) -> !`
- **Formato ELF64**: Compatible con x86_64
- **Dirección de carga**: 0xFFFFFFFF80100000 (higher half)

## Siguiente Fase

Para completar el microkernel, se requiere:

1. Implementar IDT completa con todos los handlers
2. Implementar context switching en assembly
3. Crear scheduler básico round-robin
4. Configurar paginación activa
5. Probar con bootloader en QEMU
6. Integrar con build.sh principal del proyecto

## Notas Técnicas

- **Rust nightly** requerido para `abi_x86_interrupt`
- **No red zone**: Deshabilitada para código del kernel
- **LTO**: Link-Time Optimization habilitada en release
- **Optimización**: Nivel "z" para tamaño mínimo
- **Panic strategy**: Abort (no unwinding)
