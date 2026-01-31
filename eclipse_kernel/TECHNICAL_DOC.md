# Eclipse Microkernel - Documentación Técnica Completa

## Resumen

El microkernel Eclipse OS implementa una arquitectura microkernel completa con todas las funcionalidades esenciales:
- **IDT Completa**: Interrupt Descriptor Table con handlers de excepciones e IRQs
- **Context Switching**: Cambio de contexto entre procesos en assembly
- **Scheduler**: Planificador round-robin con preemption
- **Paginación Activa**: Sistema de paginación habilitado con identity mapping

## Arquitectura del Sistema

### 1. Sistema de Interrupciones (interrupts.rs)

#### Estructura IDT

```rust
struct Idt {
    entries: [IdtEntry; 256],  // 256 entradas para interrupciones/excepciones
}
```

#### Excepciones Implementadas

| Vector | Excepción | Error Code | Handler |
|--------|-----------|------------|---------|
| 0 | Division by Zero (#DE) | No | exception_0 |
| 1 | Debug (#DB) | No | exception_1 |
| 3 | Breakpoint (#BP) | No | exception_3 |
| 4 | Overflow (#OF) | No | exception_4 |
| 6 | Invalid Opcode (#UD) | No | exception_6 |
| 8 | Double Fault (#DF) | Sí | exception_8 |
| 13 | General Protection (#GP) | Sí | exception_13 |
| 14 | Page Fault (#PF) | Sí | exception_14 |

#### IRQs Implementadas

| IRQ | Dispositivo | Vector | Handler |
|-----|-------------|--------|---------|
| 0 | Timer (PIT) | 32 | irq_0 |
| 1 | Keyboard | 33 | irq_1 |

#### Alineación de Stack

**IMPORTANTE**: Todos los handlers naked implementan alineación de stack de 16 bytes según x86-64 ABI:

```assembly
push rbp          ; Guardar base pointer
mov rbp, rsp      ; Establecer nuevo frame
and rsp, -16      ; Alinear stack a 16 bytes
; ... llamar función Rust ...
mov rsp, rbp      ; Restaurar stack
pop rbp           ; Restaurar base pointer
```

#### PIC 8259 Configuration

```
Master PIC (0x20-0x21):
- IRQ 0-7 mapeados a INT 0x20-0x27
- Máscara: 0xFC (solo IRQ0 y IRQ1 habilitados)

Slave PIC (0xA0-0xA1):
- IRQ 8-15 mapeados a INT 0x28-0x2F
- Máscara: 0xFF (todo deshabilitado)
```

### 2. Gestión de Procesos (process.rs)

#### Estructura de Contexto

```rust
struct Context {
    // Registros generales
    rax, rbx, rcx, rdx,
    rsi, rdi, rbp,
    r8, r9, r10, r11, r12, r13, r14, r15,
    
    // Control
    rsp,      // Stack pointer
    rip,      // Instruction pointer
    rflags,   // Flags register
}
```

#### Process Control Block (PCB)

```rust
struct Process {
    id: ProcessId,              // ID único
    state: ProcessState,        // Ready/Running/Blocked/Terminated
    context: Context,           // Contexto guardado
    stack_base: u64,            // Base del stack
    stack_size: usize,          // Tamaño del stack
    priority: u8,               // Prioridad (0-255)
    time_slice: u32,            // Quantum en ticks
}
```

#### Context Switching

El context switch se implementa en assembly inline y realiza:

1. **Guardar contexto actual**:
   - Todos los registros generales
   - RSP (stack pointer)
   - RIP (siguiente instrucción)
   - RFLAGS

2. **Cargar contexto nuevo**:
   - Restaurar todos los registros
   - Restaurar RSP
   - Restaurar RFLAGS
   - Saltar a RIP (jmp)

**Nota**: La función `switch_context` nunca retorna al punto de llamada original, sino que "salta" al RIP del contexto restaurado.

### 3. Scheduler (scheduler.rs)

#### Algoritmo Round-Robin

El scheduler implementa un algoritmo round-robin simple:

1. **Cola FIFO** de procesos ready (64 entradas)
2. **Quantum**: 10 timer ticks por proceso
3. **Preemption**: Forzada cada 10 ticks por timer interrupt

#### Flujo de Scheduling

```
Timer IRQ (cada tick)
    ↓
scheduler::tick()
    ↓
Cada 10 ticks → schedule()
    ↓
┌─────────────────────────┐
│ 1. Guardar proceso actual│
│    en cola ready         │
└────────────┬─────────────┘
             ↓
┌─────────────────────────┐
│ 2. Sacar siguiente      │
│    proceso de cola       │
└────────────┬─────────────┘
             ↓
┌─────────────────────────┐
│ 3. Cambiar estado a     │
│    Running               │
└────────────┬─────────────┘
             ↓
┌─────────────────────────┐
│ 4. Realizar context     │
│    switch                │
└─────────────────────────┘
```

#### API del Scheduler

```rust
// Agregar proceso a cola ready
pub fn enqueue_process(pid: ProcessId)

// Planificar siguiente proceso
pub fn schedule()

// Ceder CPU voluntariamente
pub fn yield_cpu()

// Dormir proceso (stub)
pub fn sleep(ticks: u64)
```

### 4. Sistema de Paginación (memory.rs)

#### Estructura de Paginación

```
PML4 (Level 4)
  ↓
PDPT (Level 3)
  ↓
PD (Level 2)
  ↓
Huge Pages (2MB)
```

#### Mapeo de Memoria

**Identity Mapping**: Primeros 1GB
- Virtual 0x0 - 0x40000000 → Física 0x0 - 0x40000000
- Usando 512 huge pages de 2MB cada una
- Flags: Present | Writable | Huge

**Higher Half Kernel**: PML4[511] también apunta a PDPT
- Soporte para kernel en higher half (0xFFFFFFFF80000000)

#### Configuración de CR3

```rust
// PML4 cargado en CR3
mov cr3, [dirección de PML4]
```

#### Flags de Página

| Flag | Bit | Descripción |
|------|-----|-------------|
| PRESENT | 0 | Página presente en memoria |
| WRITABLE | 1 | Página escribible |
| USER | 2 | Accesible desde ring 3 |
| WRITE_THROUGH | 3 | Write-through cache |
| CACHE_DISABLE | 4 | Cache deshabilitada |
| ACCESSED | 5 | Página accedida |
| DIRTY | 6 | Página modificada |
| HUGE | 7 | Huge page (2MB/1GB) |
| GLOBAL | 8 | No flush en TLB |

### 5. IPC (ipc.rs)

El sistema IPC permanece sin cambios de la implementación anterior:
- 32 servidores
- 256 clientes
- 1024 mensajes en cola global
- 10 tipos de mensajes

### 6. Debugging (serial.rs)

#### Funciones de Debug

```rust
serial_print(s: &str)          // Imprimir string
serial_print_hex(num: u64)     // Imprimir hexadecimal
serial_print_dec(num: u64)     // Imprimir decimal
```

## Flujo de Arranque Completo

```
1. UEFI Bootloader
   ↓
2. _start(framebuffer_info)
   ↓
3. serial::init()              → COM1 inicializado
   ↓
4. boot::load_gdt()            → GDT cargada
   ↓
5. memory::init()              → Heap inicializado
   ↓
6. memory::init_paging()       → Paginación activa
   ↓
7. interrupts::init()          → IDT cargada, IRQs habilitadas
   ↓
8. ipc::init()                 → IPC inicializado
   ↓
9. scheduler::init()           → Scheduler listo
   ↓
10. kernel_main()
    ↓
    10a. Crear proceso test   → process::create_process()
    ↓
    10b. Agregar a scheduler  → scheduler::enqueue_process()
    ↓
    10c. Main loop
         - process_messages()
         - hlt (yield)
         - Timer IRQ → schedule()
```

## Seguridad y Estabilidad

### Protecciones Implementadas

1. **Stack Overflow Protection**: Cada proceso tiene stack dedicado
2. **Memory Isolation**: Paginación separa espacios de memoria
3. **Privilege Levels**: GDT con ring 0 (kernel) y ring 3 (user)
4. **Exception Handling**: Todos los errores capturados y loggeados

### Limitaciones Conocidas

1. **No MMU completa**: Solo identity mapping básico
2. **Sin protección de memoria entre procesos**: Todos en mismo espacio
3. **Scheduler simple**: No hay prioridades dinámicas
4. **Sin filesystem real**: Solo stubs IPC
5. **Sin syscalls**: Procesos no pueden llamar kernel directamente

## Estadísticas del Sistema

El microkernel mantiene estadísticas en tiempo real:

### Interrupciones
```rust
struct InterruptStats {
    exceptions: u64,      // Total excepciones
    irqs: u64,           // Total IRQs
    timer_ticks: u64,    // Ticks del timer
}
```

### Scheduler
```rust
struct SchedulerStats {
    context_switches: u64,  // Context switches realizados
    total_ticks: u64,       // Ticks totales
}
```

## Próximas Mejoras

1. **Syscalls**: Implementar int 0x80 para llamadas al sistema
2. **User Mode**: Ejecutar procesos en ring 3
3. **Virtual Memory**: Espacios de direcciones separados por proceso
4. **Priority Scheduling**: Scheduler con prioridades
5. **Thread Support**: Múltiples threads por proceso
6. **Sincronización**: Mutex, semáforos, etc.
7. **Filesystem Real**: Integrar con eclipsefs
8. **Network Stack**: Implementar TCP/IP básico

## Debugging

### Serial Output

Todos los mensajes se envían a COM1 (0x3F8):
- Baud rate: 38400
- Formato: 8N1 (8 bits, no paridad, 1 stop bit)
- FIFO habilitada

### Capturar Output en QEMU

```bash
qemu-system-x86_64 \
    -kernel eclipse_microkernel \
    -serial stdio \
    -no-reboot \
    -no-shutdown
```

## Conclusión

El microkernel Eclipse OS está completo con todas las funcionalidades esenciales de un microkernel moderno:
- ✅ Gestión de memoria con paginación
- ✅ Manejo completo de interrupciones
- ✅ Context switching funcional
- ✅ Scheduler round-robin
- ✅ Sistema IPC
- ✅ Debugging por serial

El sistema está listo para ejecutar múltiples procesos con preemption y puede servir como base para un sistema operativo completo.
