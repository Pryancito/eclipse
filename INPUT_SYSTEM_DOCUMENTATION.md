# Sistema de Entrada de Teclado y Ratón - Eclipse OS

## Resumen

Eclipse OS implementa un sistema de entrada completo que soporta dispositivos de entrada PS/2 y USB. El sistema unifica los eventos de teclado y ratón en un framework centralizado que permite a las aplicaciones consumir eventos de entrada de manera consistente.

## Arquitectura

### Componentes Principales

```
┌─────────────────────────────────────────────────────────────┐
│                    Input System (InputSystem)               │
│  - Event buffer (VecDeque)                                  │
│  - Device management                                         │
│  - Event processing                                          │
└─────────────────────────────────────────────────────────────┘
                            ▲
                            │ (push_input_event)
        ┌───────────────────┴───────────────────┐
        │                                       │
┌───────┴──────┐                       ┌────────┴─────┐
│ PS/2 System  │                       │  USB System  │
│              │                       │              │
│ - Keyboard   │                       │ - Keyboard   │
│ - Mouse      │                       │ - Mouse      │
└──────────────┘                       └──────────────┘
       ▲                                       ▲
       │ (interrupts)                         │
┌──────┴──────┐                       ┌───────┴──────┐
│     PIC     │                       │  USB Manager │
│  IRQ 1,12   │                       │              │
└─────────────┘                       └──────────────┘
```

## Drivers de Teclado

### PS/2 Keyboard Driver

**Ubicación**: `eclipse_kernel/src/drivers/keyboard.rs`

El driver de teclado PS/2 implementa:
- Lectura directa del puerto de datos (0x60)
- Decodificación de Scancode Set 1
- Manejo de teclas modificadoras (Shift, Ctrl, Alt)
- Conversión de scancodes a caracteres

#### Ejemplo de Uso

```rust
use eclipse_kernel::drivers::keyboard::{BasicKeyboardDriver, KeyboardDriver};

let mut keyboard = BasicKeyboardDriver::new();

// Leer eventos de teclado
while let Some(event) = keyboard.read_key() {
    if let Some(c) = event.key.to_char(shift_pressed) {
        println!("Tecla presionada: {}", c);
    }
}
```

### Códigos de Tecla Soportados

El driver soporta:
- **Letras**: A-Z
- **Números**: 0-9
- **Teclas especiales**: Enter, Escape, Backspace, Tab, Space
- **Modificadores**: Shift, Ctrl, Alt, CapsLock
- **Funciones**: F1-F12
- **Navegación**: Flechas, Home, End, PageUp, PageDown, Insert, Delete

## Drivers de Ratón

### PS/2 Mouse Driver

**Ubicación**: `eclipse_kernel/src/drivers/mouse.rs`

El driver de ratón PS/2 implementa:
- Inicialización completa del controlador PS/2
- Decodificación de paquetes de 3 bytes (estándar)
- Soporte para rueda del ratón (paquetes de 4 bytes)
- Seguimiento de posición y estado de botones

#### Protocolo PS/2 del Ratón

**Paquete estándar (3 bytes)**:
```
Byte 0: [YOvf][XOvf][YSign][XSign][1][MBtn][RBtn][LBtn]
Byte 1: X Movement (8 bits)
Byte 2: Y Movement (8 bits)
```

**Paquete con rueda (4 bytes)**:
```
Byte 0-2: Como en el paquete estándar
Byte 3: [0][0][0][0][ZMov3][ZMov2][ZMov1][ZMov0]
```

#### Ejemplo de Uso

```rust
use eclipse_kernel::drivers::mouse::{PS2MouseDriver, MouseDriver};

let mut mouse = PS2MouseDriver::new();
mouse.initialize()?;

// Leer eventos del ratón
while let Some(event) = mouse.read_event() {
    match event.state {
        MouseState::Moved => {
            println!("Posición: ({}, {})", event.x, event.y);
        }
        MouseState::Pressed => {
            println!("Botón presionado: {:?}", event.button);
        }
        _ => {}
    }
}
```

## Sistema de Integración PS/2

**Ubicación**: `eclipse_kernel/src/drivers/ps2_integration.rs`

Este módulo proporciona la integración entre los drivers PS/2 y el InputSystem.

### Funciones Principales

#### `init_ps2_system()`
Inicializa el sistema PS/2 global, detecta dispositivos y configura interrupciones.

```rust
use eclipse_kernel::drivers::ps2_integration;

// Inicializar sistema PS/2
ps2_integration::init_ps2_system()?;

// Verificar dispositivos detectados
let kb_enabled = ps2_integration::is_ps2_keyboard_enabled();
let mouse_enabled = ps2_integration::is_ps2_mouse_enabled();
```

#### `process_ps2_events()`
Procesa eventos de dispositivos PS/2 y los envía al InputSystem.

```rust
// Llamar periódicamente en el bucle principal
ps2_integration::process_ps2_events();
```

### Manejo de Interrupciones

El sistema configura manejadores de interrupciones para:
- **IRQ 1**: Teclado PS/2
- **IRQ 12**: Ratón PS/2

Cuando se recibe una interrupción:
1. El handler lee datos del puerto
2. Actualiza el buffer del driver
3. Notifica al InputSystem cuando hay eventos completos

## Input System (Sistema de Entrada Unificado)

**Ubicación**: `eclipse_kernel/src/drivers/input_system.rs`

El InputSystem centraliza todos los eventos de entrada del sistema operativo.

### Características

- **Buffer de eventos**: Cola FIFO de hasta 1000 eventos
- **Soporte multi-dispositivo**: PS/2 y USB
- **Estadísticas**: Contadores de eventos procesados
- **Thread-safe**: Acceso seguro mediante Mutex

### API Pública

#### Consumir Eventos

```rust
use eclipse_kernel::drivers::input_system;

// Obtener próximo evento
if let Some(event) = input_system::get_next_input_event() {
    match event.event_type {
        InputEventType::Keyboard(kb_event) => {
            // Procesar evento de teclado
        }
        InputEventType::Mouse(mouse_event) => {
            // Procesar evento de ratón
        }
        _ => {}
    }
}

// Verificar si hay eventos
if input_system::has_input_events() {
    let count = input_system::input_event_count();
    println!("Eventos pendientes: {}", count);
}
```

#### Obtener Eventos por Tipo

```rust
// Obtener solo eventos de teclado
let kb_events = input_system::get_keyboard_events(10);

// Obtener solo eventos de ratón
let mouse_events = input_system::get_mouse_events(10);
```

#### Estadísticas

```rust
if let Some(stats) = input_system::get_input_stats() {
    println!("Total eventos: {}", stats.total_events);
    println!("Eventos de teclado: {}", stats.keyboard_events);
    println!("Eventos de ratón: {}", stats.mouse_events);
    println!("Teclados activos: {}", stats.active_keyboards);
    println!("Ratones activos: {}", stats.active_mice);
}
```

## Inicialización en el Kernel

El sistema de entrada se inicializa durante el arranque del kernel:

```rust
// Fase 12: Sistema de entrada (teclado y ratón)
match input_system::init_input_system() {
    Ok(_) => println!("✓ Sistema de entrada iniciado"),
    Err(e) => println!("⚠ Error: {}", e),
}

// Fase 12.1: Dispositivos PS/2
match ps2_integration::init_ps2_system() {
    Ok(_) => {
        if ps2_integration::is_ps2_keyboard_enabled() {
            println!("✓ Teclado PS/2 detectado");
        }
        if ps2_integration::is_ps2_mouse_enabled() {
            println!("✓ Ratón PS/2 detectado");
        }
    }
    Err(e) => println!("⚠ Error PS/2: {}", e),
}
```

## Configuración del PIC

El sistema configura el Programmable Interrupt Controller (PIC 8259A) para habilitar las IRQs necesarias:

```rust
// IRQ 1 para teclado PS/2
pic.enable_irq(1)?;

// IRQ 12 para ratón PS/2 (en PIC secundario)
pic.enable_irq(12)?;
```

## Ejemplo Completo: Aplicación de Entrada

```rust
use eclipse_kernel::drivers::input_system::*;
use eclipse_kernel::drivers::ps2_integration;

fn input_demo() -> Result<(), &'static str> {
    // Inicializar sistemas
    init_input_system()?;
    ps2_integration::init_ps2_system()?;

    // Bucle de eventos
    loop {
        // Procesar eventos PS/2
        ps2_integration::process_ps2_events();

        // Consumir eventos del sistema
        while let Some(event) = get_next_input_event() {
            match event.event_type {
                InputEventType::Keyboard(kb_event) => {
                    if kb_event.pressed {
                        println!("Tecla presionada: {:?}", kb_event.key_code);
                    }
                }
                InputEventType::Mouse(mouse_event) => {
                    match mouse_event {
                        MouseEvent::Move { position, .. } => {
                            println!("Ratón en ({}, {})", position.x, position.y);
                        }
                        MouseEvent::ButtonPress { button, .. } => {
                            println!("Botón presionado: {:?}", button);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        // Pequeña espera para no saturar la CPU
        for _ in 0..10000 {
            core::hint::spin_loop();
        }
    }
}
```

## Resolución de Problemas

### El teclado no responde

1. Verificar que la inicialización fue exitosa:
   ```rust
   if ps2_integration::is_ps2_keyboard_enabled() {
       println!("Teclado PS/2 habilitado");
   }
   ```

2. Verificar que las interrupciones están habilitadas:
   - IRQ 1 debe estar habilitada en el PIC
   - La IDT debe estar configurada con el handler de teclado

3. Comprobar el puerto de datos:
   ```rust
   // Leer directamente del puerto para debug
   let status = inb(0x64);
   if (status & 0x01) != 0 {
       let scancode = inb(0x60);
       println!("Scancode: 0x{:02X}", scancode);
   }
   ```

### El ratón no se mueve

1. Verificar inicialización del ratón:
   ```rust
   if ps2_integration::is_ps2_mouse_enabled() {
       println!("Ratón PS/2 habilitado");
   }
   ```

2. Comprobar que el puerto auxiliar está habilitado:
   - El comando 0xA8 debe enviarse al puerto 0x64

3. Verificar IRQ 12:
   - La IRQ 12 debe estar habilitada en el PIC secundario
   - El handler de ratón debe estar configurado en la IDT

### Eventos no llegan al InputSystem

1. Verificar que el sistema de entrada está inicializado:
   ```rust
   if let Some(stats) = input_system::get_input_stats() {
       println!("Sistema inicializado, eventos: {}", stats.total_events);
   }
   ```

2. Asegurarse de llamar a `process_ps2_events()` regularmente:
   ```rust
   // En el bucle principal del kernel
   ps2_integration::process_ps2_events();
   ```

3. Verificar que el buffer no está lleno:
   ```rust
   let stats = input_system::get_input_stats()?;
   println!("Uso del buffer: {}%", stats.buffer_usage);
   ```

## Referencias Técnicas

### Especificaciones PS/2

- **Teclado**: Scancode Set 1 (IBM XT/AT compatible)
- **Ratón**: Protocolo IntelliMouse (con soporte para rueda)
- **Puertos I/O**:
  - 0x60: Datos
  - 0x64: Estado/Comando

### Códigos de Comando del Controlador

- `0xA8`: Habilitar puerto auxiliar (ratón)
- `0xA7`: Deshabilitar puerto auxiliar
- `0xD4`: Escribir al dispositivo auxiliar

### Códigos de Comando del Ratón

- `0xFF`: Reset
- `0xF4`: Habilitar reporte de datos
- `0xF5`: Deshabilitar reporte de datos
- `0xF6`: Establecer valores predeterminados
- `0xF3`: Establecer tasa de muestreo
- `0xF2`: Obtener ID del dispositivo

### Respuestas del Ratón

- `0xFA`: ACK (Comando aceptado)
- `0xAA`: Self-test passed
- `0x00`: ID estándar (ratón sin rueda)
- `0x03`: ID con rueda (IntelliMouse)
- `0x04`: ID con 5 botones

## Mejoras Futuras

### Planeadas

- [ ] Soporte para teclados USB HID
- [ ] Soporte para ratones USB HID
- [ ] Gestos del touchpad
- [ ] Configuración de sensibilidad del ratón
- [ ] Repetición automática de teclas
- [ ] Mapeo de teclas configurable
- [ ] Soporte para múltiples layouts de teclado

### En Consideración

- [ ] Soporte para gamepads y joysticks
- [ ] API de entrada para aplicaciones userland
- [ ] Sistema de shortcuts globales
- [ ] Grabación y reproducción de eventos de entrada
- [ ] Filtros de eventos personalizables

## Contribuir

Para contribuir al sistema de entrada:

1. Revisar el código en `eclipse_kernel/src/drivers/`
2. Seguir las convenciones de código existentes
3. Agregar tests para nuevas funcionalidades
4. Actualizar esta documentación
5. Enviar un Pull Request

## Licencia

Este componente es parte de Eclipse OS y está licenciado bajo la Licencia MIT.
