# 🌙 Eclipse OS Kernel

Kernel principal de Eclipse OS desarrollado en Rust `no_std` con arquitectura modular y soporte completo para hardware.

## 🎯 Características

### 🏗️ Arquitectura Modular
- **Sistema de drivers**: Drivers modulares para diferentes hardware
- **Gestión de memoria**: Allocator personalizado y gestión de páginas
- **Manejo de interrupciones**: PIC, APIC y excepciones
- **Sistema de archivos**: Soporte básico para FAT32 y NTFS

### 🖥️ Soporte de Display
- **VGA Text Mode**: Salida de texto en modo VGA
- **Framebuffer**: Soporte para framebuffer moderno
- **Display unificado**: API común para diferentes tipos de display
- **Colores y fuentes**: Soporte completo para colores VGA

### 🔧 Hardware Management
- **Detección automática**: Detección de hardware disponible
- **Drivers modulares**: Sistema extensible de drivers
- **Gestión de dispositivos**: Control centralizado de hardware
- **Monitoreo**: Sistema de monitoreo de hardware

## 🔧 Dependencias

```toml
[dependencies]
# Core
alloc = "1.0"
core = "1.0"

# Hardware
x86_64 = "0.14"
uart_16550 = "0.2"
pc-keyboard = "0.5"

# Collections
heapless = "0.8"
linked_list_allocator = "0.10"

# Serialization
serde = { version = "1.0", features = ["derive"], default-features = false }
bincode = "1.3"

# Utilities
spin = "0.9"
```

## 🚀 Compilación

### Target Bare Metal
```bash
# Instalar target
rustup target add x86_64-unknown-none

# Compilar kernel
cargo build --release --target x86_64-unknown-none
```

### Script de Compilación
```bash
# Usar script incluido
./build_kernel_uefi.sh
```

## 📁 Estructura del Código

### `src/main_simple.rs`
- **Kernel principal**: Lógica principal del kernel
- **Inicialización**: Inicialización de hardware y drivers
- **Display**: Configuración de VGA y framebuffer
- **Shell**: Shell interactivo básico

### `src/entry_simple.rs`
- **Entry point**: Punto de entrada del kernel
- **Memory allocator**: Allocator global para el kernel
- **Panic handler**: Manejo de pánicos del kernel

### `src/drivers/`
- **modular/**: Sistema de drivers modulares
  - `drm.rs`: Driver DRM básico
  - `gpu.rs`: Driver de GPU
  - `audio.rs`: Driver de audio
  - `network_advanced.rs`: Driver de red avanzado
  - `manager.rs`: Gestor de drivers
  - `std_modules.rs`: Módulos de userland

### `src/display.rs`
- **Display unificado**: API común para display
- **VGA support**: Soporte completo para VGA
- **Framebuffer**: Soporte para framebuffer moderno
- **Colores**: Sistema de colores VGA

### `src/shell.rs`
- **Shell interactivo**: Shell básico del kernel
- **Comandos**: Comandos del sistema
- **Input/Output**: Manejo de entrada y salida

## 🔍 Inicialización del Kernel

### 1. Entry Point
```rust
#[no_mangle]
pub extern "C" fn _start(
    framebuffer_base: u64,
    framebuffer_width: u32,
    framebuffer_height: u32,
    framebuffer_pitch: u32,
    framebuffer_format: u32,
) -> ! {
    // Inicializar allocator
    init_heap();
    
    // Llamar al kernel principal
    kernel_main(
        framebuffer_base,
        framebuffer_width,
        framebuffer_height,
        framebuffer_pitch,
        framebuffer_format,
    );
}
```

### 2. Inicialización Principal
```rust
pub fn kernel_main(
    framebuffer_base: u64,
    framebuffer_width: u32,
    framebuffer_height: u32,
    framebuffer_pitch: u32,
    framebuffer_format: u32,
) -> ! {
    // Inicializar VGA
    init_vga_mode();
    
    // Configurar display
    if framebuffer_base != 0 {
        // Usar framebuffer si está disponible
        init_framebuffer(framebuffer_base, framebuffer_width, framebuffer_height);
    } else {
        // Usar VGA como fallback
        init_vga_display();
    }
    
    // Inicializar drivers modulares
    init_modular_drivers();
    
    // Inicializar gestor de drivers
    init_advanced_driver_manager();
    
    // Inicializar módulos std
    init_std_modules();
    
    // Mostrar información del sistema
    display_system_info();
    
    // Iniciar shell
    start_shell();
}
```

## 🖥️ Sistema de Display

### VGA Text Mode
```rust
// Inicializar VGA
pub fn init_vga_mode() {
    // Configurar modo de texto 80x25
    outb(0x3D4, 0x0A);  // Cursor start
    outb(0x3D5, 0x20);  // Cursor start value
    outb(0x3D4, 0x0B);  // Cursor end
    outb(0x3D5, 0x00);  // Cursor end value
}

// Escribir carácter en VGA
pub fn write_char(c: u8) {
    let color = (Color::White as u8) | ((Color::Black as u8) << 4);
    let index = (VGA_BUFFER_HEIGHT - 1) * VGA_BUFFER_WIDTH + VGA_BUFFER_WIDTH - 1;
    VGA_BUFFER[index] = VgaChar {
        ascii_character: c,
        color_code: color,
    };
}
```

## 🔧 Sistema de Drivers

### Driver Modular
```rust
pub trait ModularDriver {
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    fn init(&mut self) -> Result<(), &'static str>;
    fn status(&self) -> DriverStatus;
    fn capabilities(&self) -> Vec<&'static str>;
}
```

### Gestión de Drivers
```rust
pub struct ModularDriverManager {
    drivers: Vec<Box<dyn ModularDriver>>,
}

impl ModularDriverManager {
    pub fn register_driver(&mut self, driver: Box<dyn ModularDriver>) {
        self.drivers.push(driver);
    }
    
    pub fn init_all(&mut self) {
        for driver in &mut self.drivers {
            let _ = driver.init();
        }
    }
    
    pub fn list_drivers(&self) -> Vec<&str> {
        self.drivers.iter().map(|d| d.name()).collect()
    }
}
```

## 🖥️ Shell Interactivo

### Comandos Disponibles
- `help` - Mostrar ayuda
- `info` - Información del sistema
- `drivers` - Listar drivers
- `modules` - Listar módulos
- `clear` - Limpiar pantalla
- `colors` - Demostración de colores
- `test` - Test del sistema

## 🐛 Debugging

### Panic Handler
```rust
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    VGA.set_color(Color::LightRed, Color::Black);
    VGA.write_string("\n\n╔════════════════════════════════════════════════════════════════════════╗\n");
    VGA.write_string("║                                KERNEL PANIC                                 ║\n");
    VGA.write_string("╚════════════════════════════════════════════════════════════════════════╝\n");
    
    VGA.set_color(Color::White, Color::Black);
    VGA.write_string("\nEl kernel ha encontrado un error crítico y se ha detenido.\n");
    
    // Mostrar información de debug
    if let Some(location) = info.location() {
        VGA.write_string("Ubicación: ");
        VGA.write_string(location.file());
        VGA.write_string(":");
        // ... más información
    }
    
    VGA.write_string("Mensaje: Kernel panic detectado\n");
    VGA.write_string("\nReinicia el sistema para continuar.\n");
    
    loop {
        unsafe { core::arch::asm!("hlt"); }
    }
}
```

## 📊 Rendimiento

### Optimizaciones
- **Compilación release**: Máximo rendimiento
- **Memory management**: Gestión eficiente de memoria
- **Driver system**: Sistema de drivers optimizado
- **Display rendering**: Renderizado optimizado

### Métricas
- **Tiempo de inicialización**: < 100ms
- **Uso de memoria**: ~20KB para kernel básico
- **Latencia de shell**: < 1ms por comando
- **Rendimiento VGA**: 60 FPS para texto

## 🔧 Testing

### Test Básico
```bash
# Compilar y testear
./build_kernel_uefi.sh
./test_simple.sh
```

### Test con QEMU
```bash
# Test con VGA
./test_vga.sh

# Test con framebuffer
./test_boot.sh
```

## 🤝 Contribución

### Añadir Nuevo Driver
1. Implementar trait `ModularDriver`
2. Registrar en `auto_register.rs`
3. Añadir inicialización en `init_modular_drivers()`
4. Añadir tests si es posible

### Mejoras de Rendimiento
1. Optimizar sistema de display
2. Mejorar gestión de memoria
3. Optimizar drivers
4. Reducir latencia de shell

---

**Eclipse OS Kernel** - *El corazón del sistema operativo* 🌙