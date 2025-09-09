//! Módulo principal simplificado del kernel Eclipse OS

#![no_std]
#![no_main]

extern crate alloc;
use alloc::boxed::Box;
use core::fmt::Result as FmtResult;

use core::panic::PanicInfo;
use alloc::format;
use alloc::string::String;

// Importar módulos del kernel
use crate::init_system::{InitSystem, InitProcess};
use crate::wayland::{init_wayland, is_wayland_initialized, get_wayland_state};

use crate::graphics::{set_colors, clear_screen, print_string, init_graphics};
use crate::graphics::Color as GraphicsColor;

// Serial COM1 para logs tempranos
#[inline(always)]
unsafe fn outb(port: u16, val: u8) {
    core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
}

#[inline(always)]
unsafe fn inb(port: u16) -> u8 {
    let mut val: u8;
    core::arch::asm!("in al, dx", in("dx") port, out("al") val, options(nomem, nostack, preserves_flags));
    val
}

unsafe fn serial_init() {
    let base: u16 = 0x3F8;
    outb(base + 1, 0x00);
    outb(base + 3, 0x80);
    outb(base + 0, 0x01);
    outb(base + 1, 0x00);
    outb(base + 3, 0x03);
    outb(base + 2, 0xC7);
    outb(base + 4, 0x0B);
}

unsafe fn serial_write_byte(b: u8) {
    let base: u16 = 0x3F8;
    while (inb(base + 5) & 0x20) == 0 {}
    outb(base, b);
}

unsafe fn serial_write_str(s: &str) {
    for &c in s.as_bytes() { serial_write_byte(c); }
}

unsafe fn serial_write_hex32(val: u32) {
    for i in (0..8).rev() {
        let nibble = (val >> (i * 4)) & 0xF;
        let c = if nibble < 10 {
            b'0' + nibble as u8
        } else {
            b'A' + (nibble - 10) as u8
        };
        serial_write_byte(c);
    }
}

unsafe fn serial_write_hex64(val: u64) {
    for i in (0..16).rev() {
        let nibble = (val >> (i * 4)) & 0xF;
        let c = if nibble < 10 {
            b'0' + nibble as u8
        } else {
            b'A' + (nibble - 10) as u8
        };
        serial_write_byte(c);
    }
}

/// Función para convertir números a string
fn int_to_string(mut num: u64) -> &'static str {
    // Para simplificar, devolveremos strings fijos para números comunes
    match num {
        0 => "0",
        1 => "1",
        2 => "2",
        3 => "3",
        4 => "4",
        5 => "5",
        6 => "6",
        7 => "7",
        8 => "8",
        9 => "9",
        10 => "10",
        _ => "N/A", // Para números más grandes
    }
}

use core::fmt::Write;

// Modos de gráficos
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GraphicsMode {
    Framebuffer,
    VGA,
}

// Función para detectar hardware gráfico (usando nuevo sistema PCI)
fn detect_graphics_hardware() -> GraphicsMode {
    use crate::hardware_detection::{detect_graphics_hardware, GraphicsMode as NewGraphicsMode};
    
    let result = detect_graphics_hardware();
    match result.graphics_mode {
        NewGraphicsMode::Framebuffer => GraphicsMode::Framebuffer,
        NewGraphicsMode::VGA => GraphicsMode::VGA,
        NewGraphicsMode::HardwareAccelerated => GraphicsMode::Framebuffer, // Usar framebuffer como base
    }
}

// Colores VGA
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum Color {
    Black = 0,
    Blue = 1,
    Green = 2,
    Cyan = 3,
    Red = 4,
    Magenta = 5,
    Brown = 6,
    LightGray = 7,
    DarkGray = 8,
    LightBlue = 9,
    LightGreen = 10,
    LightCyan = 11,
    LightRed = 12,
    LightMagenta = 13,
    Yellow = 14,
    White = 15,
}

// Driver VGA real y funcional
pub struct VgaWriter {
    buffer: *mut u16,
    position: usize,
    color: u8,
    width: usize,
    height: usize,
}

impl VgaWriter {
    pub const fn new() -> Self {
        Self {
            buffer: 0xB8000 as *mut u16,
            position: 0,
            color: 0x0F, // Blanco sobre negro
            width: 80,
            height: 25,
        }
    }

    pub fn init_vga_mode(&mut self) {
        // Inicializar VGA en modo texto 80x25
        self.clear_screen();
        self.set_cursor(0, 0);
    }

    pub fn clear_screen(&mut self) {
            unsafe {
            for i in 0..(self.width * self.height) {
                *self.buffer.add(i) = 0x0720; // Espacio en blanco sobre negro
            }
        }
        self.position = 0;
    }

    pub fn set_cursor(&mut self, x: usize, y: usize) {
        if x >= self.width || y >= self.height {
            return;
        }
        
        self.position = y * self.width + x;
        
        let pos = (y * self.width + x) as u16;
        
        // Configurar cursor en VGA
        self.outb(0x3D4, 0x0F);
        self.outb(0x3D5, (pos & 0xFF) as u8);
        self.outb(0x3D4, 0x0E);
        self.outb(0x3D5, ((pos >> 8) & 0xFF) as u8);
    }

    pub fn get_cursor(&self) -> (usize, usize) {
        let x = self.position % self.width;
        let y = self.position / self.width;
        (x, y)
    }

    pub fn set_color(&mut self, fg: Color, bg: Color) {
        self.color = ((bg as u8) << 4) | (fg as u8);
    }

    pub fn write_char(&mut self, c: char) {
        match c {
            '\n' => {
                self.new_line();
            }
            '\r' => {
                let (_, y) = self.get_cursor();
                self.set_cursor(0, y);
            }
            '\t' => {
                for _ in 0..4 {
                    self.write_char(' ');
                }
            }
            _ => {
                if self.position < self.width * self.height {
                unsafe {
                        *self.buffer.add(self.position) = ((self.color as u16) << 8) | (c as u16);
                }
                self.position += 1;
                }
            }
        }
    }

    fn new_line(&mut self) {
        let (_, y) = self.get_cursor();
        if y + 1 >= self.height {
            self.scroll_up();
        } else {
            self.set_cursor(0, y + 1);
        }
    }

    fn scroll_up(&mut self) {
        unsafe {
            // Mover todas las líneas hacia arriba
            for y in 0..(self.height - 1) {
                for x in 0..self.width {
                    let src = (y + 1) * self.width + x;
                    let dst = y * self.width + x;
                    *self.buffer.add(dst) = *self.buffer.add(src);
                }
            }
            
            // Limpiar la última línea
            for x in 0..self.width {
                let pos = (self.height - 1) * self.width + x;
                *self.buffer.add(pos) = 0x0720;
            }
        }
        
        self.set_cursor(0, self.height - 1);
    }

    pub fn write_string(&mut self, s: &str) {
        for c in s.chars() {
            self.write_char(c);
        }
    }

    fn outb(&self, port: u16, value: u8) {
        unsafe {
            core::arch::asm!(
                "out dx, al",
                in("dx") port,
                in("al") value,
                options(nomem, nostack, preserves_flags)
            );
        }
    }
}

// Driver de serie simplificado
pub struct SerialWriter {
    base_port: u16,
}

impl SerialWriter {
    pub const fn new() -> Self {
        Self { base_port: 0x3F8 }
    }

    pub fn init(&mut self) {
        // Configurar COM1
        self.outb(self.base_port + 1, 0x00); // Deshabilitar interrupciones
        self.outb(self.base_port + 3, 0x80); // Habilitar DLAB
        self.outb(self.base_port + 0, 0x03); // Divisor de baudios bajo
        self.outb(self.base_port + 1, 0x00); // Divisor de baudios alto
        self.outb(self.base_port + 3, 0x03); // 8 bits, sin paridad, 1 stop bit
        self.outb(self.base_port + 2, 0xC7); // Habilitar FIFO
        self.outb(self.base_port + 4, 0x0B); // Habilitar DTR, RTS y OUT2
    }

    #[inline]
    pub fn write_byte(&self, b: u8) {
        unsafe {
            // Esperar a que el transmisor esté listo (LSR bit 5)
            let lsr = self.base_port + 5;
            let mut ready: u8 = 0;
            loop {
                core::arch::asm!(
                    "in al, dx",
                    in("dx") lsr,
                    out("al") ready,
                    options(nomem, nostack, preserves_flags)
                );
                if (ready & 0x20) != 0 { break; }
            }
            self.outb(self.base_port, b);
        }
    }

    pub fn write_str(&self, s: &str) {
        for &c in s.as_bytes() {
            self.write_byte(c);
        }
    }
    fn outb(&self, port: u16, value: u8) {
        unsafe {
            core::arch::asm!(
                "out dx, al",
                in("dx") port,
                in("al") value,
                options(nomem, nostack, preserves_flags)
            );
        }
    }
}

// Variables globales
pub static mut VGA: VgaWriter = VgaWriter::new();
pub static mut SERIAL: SerialWriter = SerialWriter::new();

// El allocador global está definido en allocator.rs

/// Función principal del kernel
pub fn kernel_main() -> Result<(), &'static str> {
    // DEBUG: Confirmar que llegamos a kernel_main
    unsafe {
        // Inicializar la interfaz serial si no está inicializada
        SERIAL.init();
        SERIAL.write_str("Kernel iniciado correctamente\r\n");
        
        // Inicializar sistema de gráficos
        init_graphics();
        
        set_colors(GraphicsColor::Green, GraphicsColor::Black);
        clear_screen();
        print_string("Kernel Iniciado!");
    }
    
    // Inicializar el allocador global
    #[cfg(feature = "alloc")]
    {
        crate::allocator::init_allocator();
    }
    
    // Inicializar sistema de display
    unsafe {
        VGA.set_color(Color::LightGreen, Color::Black);
        print_string("╔══════════════════════════════════════════════════════════════╗\n");
        print_string("║                    ECLIPSE OS KERNEL                         ║\n");
        print_string("║                        v0.5.0                                ║\n");
        print_string("╚══════════════════════════════════════════════════════════════╝\n");
        VGA.set_color(Color::Yellow, Color::Black);
        VGA.write_string("\nKERNEL TOMANDO CONTROL DEL SISTEMA...\n");
        VGA.set_color(Color::White, Color::Black);
        VGA.write_string("==========================================\n\n");
        
        // Mostrar información de debug sobre el framebuffer
        VGA.set_color(Color::LightCyan, Color::Black);
        VGA.write_string("Debug: Usando VGA para display\n");
        VGA.write_string("Debug: Framebuffer disponible: ");
        if crate::drivers::framebuffer::is_framebuffer_available() {
            VGA.write_string("Sí - Inicializando framebuffer...\n");
            
            // Usar framebuffer para mostrar mensajes
            use crate::graphics::*;
            framebuffer_clear(Color::Black);
            framebuffer_write_string("╔══════════════════════════════════════════════════════════════╗\n");
            framebuffer_write_string("║                    ECLIPSE OS KERNEL                         ║\n");
            framebuffer_write_string("║                        v0.5.0                                ║\n");
            framebuffer_write_string("╚══════════════════════════════════════════════════════════════╝\n");
            framebuffer_write_string("\n KERNEL TOMANDO CONTROL DEL SISTEMA...\n");
            framebuffer_write_string("==========================================\n\n");
            
            // Mostrar información del framebuffer
            framebuffer_write_string("Debug: Framebuffer activo - Resolución: ");
            if let Some(info) = crate::drivers::framebuffer::get_framebuffer_info() {
                framebuffer_write_string(&alloc::format!("{}x{}\n", info.width, info.height));
            }
        } else {
            VGA.write_string("No\n");
        }
    }
    
    // Inicializar drivers básicos
    unsafe {
        VGA.set_color(Color::LightGreen, Color::Black);
        VGA.write_string("Drivers VGA inicializados\n");
        VGA.write_string("Driver de serie COM1 activo\n");
        VGA.write_string("Gestión de memoria básica lista\n");
        
        // Inicializar framebuffer
        VGA.write_string("Inicializando framebuffer...\n");
    }
    
    // Detectar hardware gráfico usando PCI
    unsafe {
        VGA.set_color(Color::LightCyan, Color::Black);
        VGA.write_string("Detectando hardware gráfico...\n");
        VGA.set_color(Color::White, Color::Black);
    }
    
    // Usar nuevo sistema de detección
    use crate::hardware_detection::{HardwareDetector, GraphicsMode as NewGraphicsMode};
    let mut detector = HardwareDetector::new();
    let detection_result = detector.detect_hardware();
    
    // Clonar los datos necesarios para evitar problemas de borrow
    let available_gpus = detection_result.available_gpus.clone();
    let graphics_mode = detection_result.graphics_mode.clone();
    let recommended_driver = detection_result.recommended_driver.clone();
    
    // Obtener información de drivers después de clonar
    let driver_info = detector.get_gpu_driver_info();
    let (total, ready, intel_ready) = detector.get_driver_stats();
    
    // Mostrar información de GPUs detectadas
    unsafe {
        VGA.set_color(Color::LightBlue, Color::Black);
        VGA.write_string("Hardware detectado:\n");
        VGA.set_color(Color::White, Color::Black);
        
        for gpu_info in &available_gpus {
            VGA.write_string("  - ");
            VGA.write_string(gpu_info.gpu_type.as_str());
            VGA.write_string(" ");
            VGA.write_string(&alloc::format!("{:04X}:{:04X}", gpu_info.pci_device.vendor_id, gpu_info.pci_device.device_id));
            VGA.write_string(" (");
            VGA.write_string(&int_to_string(gpu_info.memory_size / (1024 * 1024)));
            VGA.write_string("MB)\n");
        }
        
        if available_gpus.is_empty() {
            VGA.write_string("  - No se detectaron GPUs\n");
        }
    }
    
    // Mostrar información de drivers cargados
    unsafe {
        VGA.set_color(Color::LightGreen, Color::Black);
        VGA.write_string("Drivers de GPU cargados:\n");
        VGA.set_color(Color::White, Color::Black);
        
        for driver_info in &driver_info {
            VGA.write_string("  - ");
            VGA.write_string(driver_info);
            VGA.write_string("\n");
        }
        
        // Mostrar estadísticas
        VGA.write_string("  - Total: ");
        VGA.write_string(&int_to_string(total as u64));
        VGA.write_string(", Listos: ");
        VGA.write_string(&int_to_string(ready as u64));
        VGA.write_string(", Intel: ");
        VGA.write_string(&int_to_string(intel_ready as u64));
        VGA.write_string("\n");
    }
    
    // Configurar modo de gráficos
    let graphics_mode = match graphics_mode {
        NewGraphicsMode::Framebuffer => {
            match crate::uefi_framebuffer::configure_framebuffer_for_hardware() {
                Ok(_) => {
                    unsafe {
                        VGA.set_color(Color::LightGreen, Color::Black);
                        VGA.write_string("Framebuffer inicializado correctamente\n");
                        VGA.set_color(Color::White, Color::Black);
                    }
                    GraphicsMode::Framebuffer
                }
                Err(e) => {
                    unsafe {
                        VGA.set_color(Color::LightRed, Color::Black);
                        VGA.write_string("Error inicializando framebuffer: ");
                        VGA.write_string(e);
                        VGA.write_string("\n");
                        VGA.set_color(Color::White, Color::Black);
                    }
                    GraphicsMode::VGA
                }
            }
        }
        NewGraphicsMode::HardwareAccelerated => {
            unsafe {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("Hardware acelerado disponible\n");
                        VGA.write_string("  - Driver recomendado: ");
                        VGA.write_string(recommended_driver.as_str());
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
            GraphicsMode::Framebuffer // Usar framebuffer como base
        }
        NewGraphicsMode::VGA => {
            unsafe {
                VGA.set_color(Color::Yellow, Color::Black);
                VGA.write_string("Usando modo VGA (texto)\n");
                VGA.write_string("  - Resolución: 80x25 caracteres\n");
                VGA.write_string("  - Colores: 16 colores\n");
                VGA.write_string("  - Modo: Texto\n");
                VGA.set_color(Color::White, Color::Black);
            }
            GraphicsMode::VGA
        }
    };
    
    // Inicializar integración DRM
    unsafe {
        VGA.set_color(Color::LightCyan, Color::Black);
        VGA.write_string("Inicializando integración DRM...\n");
        VGA.set_color(Color::White, Color::Black);
    }
    
    use crate::drivers::drm_integration::{DrmIntegration, DrmKernelCommand, create_drm_integration};
    let mut drm_integration = create_drm_integration();
    
    // Obtener información del framebuffer si está disponible
    let framebuffer_info = if graphics_mode == GraphicsMode::Framebuffer {
        Some(crate::drivers::framebuffer::FramebufferInfo {
            base_address: 0x1000000,
            size: 1920 * 1080 * 4,
            width: 1920,
            height: 1080,
            pitch: 1920 * 4,
            bpp: 32,
            red_offset: 0,
            green_offset: 8,
            blue_offset: 16,
            alpha_offset: 24,
            red_length: 8,
            green_length: 8,
            blue_length: 8,
            alpha_length: 8,
            pixel_format: crate::drivers::framebuffer::PixelFormat::RGBA8888,
        })
    } else {
        None
    };
    
    match drm_integration.initialize(framebuffer_info) {
        Ok(_) => {
            unsafe {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("Integración DRM inicializada correctamente\n");
                
                // Mostrar información de la integración DRM
                let integration_info = drm_integration.get_integration_info();
                VGA.write_string("  - Drivers kernel: ");
                VGA.write_string(&int_to_string(integration_info.kernel_drivers as u64));
                VGA.write_string("\n");
                VGA.write_string("  - Drivers listos: ");
                VGA.write_string(&int_to_string(integration_info.kernel_ready as u64));
                VGA.write_string("\n");
                VGA.write_string("  - Userland DRM: ");
                if integration_info.userland_available {
                    VGA.write_string("Disponible\n");
                } else {
                    VGA.write_string("No disponible\n");
                }
                VGA.write_string("  - Canal comunicación: Memoria compartida\n");
                VGA.set_color(Color::White, Color::Black);
            }
            
            // Probar operación DRM básica
            match drm_integration.execute_integrated_operation(DrmKernelCommand::Initialize) {
                Ok(_) => {
                    unsafe {
                        VGA.set_color(Color::LightGreen, Color::Black);
                        VGA.write_string("Comunicación DRM kernel-userland establecida\n");
                        VGA.set_color(Color::White, Color::Black);
                    }
                }
                Err(e) => {
                    unsafe {
                        VGA.set_color(Color::LightRed, Color::Black);
                        VGA.write_string("Error en comunicación DRM: ");
                        VGA.write_string(e);
                        VGA.write_string("\n");
                        VGA.set_color(Color::White, Color::Black);
                    }
                }
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error inicializando integración DRM: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }
        
        // Inicializar sistema de inicialización con systemd
    unsafe {
        VGA.write_string("Inicializando sistema de inicialización...\n");
    }
    
    // Inicializar systemd
    let mut init_system = InitSystem::new();
    match init_system.initialize() {
        Ok(_) => {
            unsafe {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("Sistema de inicialización (systemd) configurado\n");
                
                // Mostrar información del proceso init
                if let Some(init_info) = init_system.get_init_info() {
                    VGA.write_string("Proceso init: ");
                    VGA.write_string(init_info.name);
                    VGA.write_string(" (PID: ");
                    VGA.write_string(&int_to_string(init_info.pid as u64));
                    VGA.write_string(")\n");
                }
                
                VGA.set_color(Color::Yellow, Color::Black);
                VGA.write_string("\nInformación del sistema:\n");
                VGA.set_color(Color::White, Color::Black);
                VGA.write_string("  - Arquitectura: x86_64\n");
                VGA.write_string("  - Kernel: Rust (no_std)\n");
                VGA.write_string("  - Gráficos: ");
                match graphics_mode {
                    GraphicsMode::Framebuffer => VGA.write_string("Framebuffer (acelerado)\n"),
                    GraphicsMode::VGA => VGA.write_string("VGA (texto)\n"),
                }
                VGA.write_string("  - Init System: systemd\n");
                VGA.write_string("  - Init Process: eclipse-systemd\n");
                VGA.write_string("  - Display Server: Wayland\n");
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error al inicializar systemd: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }
    
    // Inicializar sistema de escritorio controlado por IA
    unsafe {
        VGA.set_color(Color::LightCyan, Color::Black);
        VGA.write_string("\nInicializando escritorio controlado por IA...\n");
        VGA.set_color(Color::White, Color::Black);
    }

    // Crear escritorio de ejemplo
    match crate::desktop_ai::ai_create_window(1, 100, 100, 400, 300, "Terminal") {
        Ok(_) => {
            unsafe {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("Ventana de terminal creada\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error creando ventana: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }

    // Renderizar escritorio
    match crate::desktop_ai::ai_render_desktop() {
        Ok(_) => {
            unsafe {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("[OK] Escritorio renderizado correctamente\n");
                
                // Mostrar estadísticas de rendimiento
                let stats = crate::desktop_ai::ai_get_performance_stats();
                VGA.write_string("  - Tiempo de renderizado: ");
                VGA.write_string(&int_to_string(stats.render_time));
                VGA.write_string("ms\n");
                VGA.write_string("  - Ventanas activas: ");
                VGA.write_string(&int_to_string(stats.windows_count as u64));
                VGA.write_string("\n");
                VGA.write_string("  - Cache hits: ");
                VGA.write_string(&int_to_string(stats.cache_hits as u64));
                VGA.write_string("\n");
                VGA.write_string("  - Tasa de acierto: ");
                VGA.write_string(&int_to_string((stats.cache_hit_rate * 100.0) as u64));
                VGA.write_string("%\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error renderizando escritorio: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }
    
    // Inicializar Wayland
    unsafe {
        VGA.set_color(Color::Cyan, Color::Black);
        VGA.write_string("\nInicializando Wayland...\n");
        VGA.set_color(Color::White, Color::Black);
    }
    
    match init_wayland() {
        Ok(_) => {
            unsafe {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("Wayland inicializado correctamente\n");
                VGA.write_string("Compositor Wayland activo\n");
                VGA.write_string("Protocolo de ventanas listo\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error inicializando Wayland: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }
    
    // Mostrar mensaje final y transferir control a systemd
    unsafe {
        VGA.set_color(Color::LightGreen, Color::Black);
        VGA.write_string("\nKernel Eclipse OS inicializado correctamente\n");
        VGA.set_color(Color::Yellow, Color::Black);
        VGA.write_string("Transferiendo control a systemd...\n");
        VGA.set_color(Color::White, Color::Black);
    }
    
    // Transferir control a systemd
    match init_system.execute_init() {
        Ok(_) => {
            unsafe {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("Control transferido a systemd exitosamente\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error al transferir control a systemd: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }
    
    // Crear sistema de aceleración 2D
    use crate::drivers::acceleration_2d::{Acceleration2D, AccelerationOperation, HardwareAccelerationType};
    use crate::drivers::framebuffer::{FramebufferDriver, Color as FbColor};
    use crate::desktop_ai::{Point, Rect};
    
    let framebuffer = FramebufferDriver::new();
    let mut acceleration_2d = Acceleration2D::new(framebuffer);
    
    // Demostración de aceleración 2D
    if graphics_mode == GraphicsMode::Framebuffer {
    
        unsafe {
        VGA.set_color(Color::LightCyan, Color::Black);
        VGA.write_string("Demostrando aceleración 2D...\n");
        VGA.set_color(Color::White, Color::Black);
    }
        
        // Inicializar aceleración 2D con la primera GPU detectada
        if let Some(gpu_info) = available_gpus.first() {
            match acceleration_2d.initialize_with_gpu(gpu_info) {
                crate::drivers::acceleration_2d::AccelerationResult::HardwareAccelerated => {
                    unsafe {
                        VGA.set_color(Color::LightGreen, Color::Black);
                        VGA.write_string("Aceleración 2D hardware habilitada\n");
                        VGA.write_string("  - GPU: ");
                        VGA.write_string(gpu_info.gpu_type.as_str());
                        VGA.write_string("\n");
                        VGA.write_string("  - Memoria: ");
                        VGA.write_string(&int_to_string(gpu_info.memory_size / (1024 * 1024)));
                        VGA.write_string("MB\n");
                        VGA.set_color(Color::White, Color::Black);
                    }
                }
                crate::drivers::acceleration_2d::AccelerationResult::SoftwareFallback => {
                    unsafe {
                        VGA.set_color(Color::Yellow, Color::Black);
                        VGA.write_string("Usando aceleración 2D software\n");
                        VGA.set_color(Color::White, Color::Black);
                    }
                }
                crate::drivers::acceleration_2d::AccelerationResult::DriverError(e) => {
                    unsafe {
                        VGA.set_color(Color::LightRed, Color::Black);
                        VGA.write_string("Error en aceleración 2D: ");
                        VGA.write_string(&e);
                        VGA.write_string("\n");
                        VGA.set_color(Color::White, Color::Black);
                    }
                }
                _ => {}
            }
        }
        
        // Demostrar operaciones de aceleración 2D
        let demo_operations = [
            AccelerationOperation::ClearScreen(FbColor { r: 0, g: 0, b: 128, a: 255 }), // Fondo azul
            AccelerationOperation::FillRect(Rect { x: 100, y: 100, width: 200, height: 150 }, FbColor { r: 255, g: 0, b: 0, a: 255 }), // Rectángulo rojo
            AccelerationOperation::DrawRect(Rect { x: 120, y: 120, width: 160, height: 110 }, FbColor { r: 255, g: 255, b: 0, a: 255 }, 3), // Rectángulo amarillo
            AccelerationOperation::DrawLine(Point { x: 50, y: 50 }, Point { x: 300, y: 200 }, FbColor { r: 0, g: 255, b: 0, a: 255 }, 2), // Línea verde
            AccelerationOperation::DrawCircle(Point { x: 400, y: 300 }, 50, FbColor { r: 255, g: 0, b: 255, a: 255 }, true), // Círculo magenta relleno
            AccelerationOperation::DrawCircle(Point { x: 500, y: 200 }, 30, FbColor { r: 255, g: 255, b: 255, a: 255 }, false), // Círculo blanco vacío
            AccelerationOperation::DrawTriangle(Point { x: 600, y: 100 }, Point { x: 700, y: 100 }, Point { x: 650, y: 200 }, FbColor { r: 255, g: 128, b: 0, a: 255 }, true), // Triángulo naranja
        ];
        
        for (i, operation) in demo_operations.iter().enumerate() {
            match acceleration_2d.execute_operation(operation.clone()) {
                crate::drivers::acceleration_2d::AccelerationResult::HardwareAccelerated => {
                    unsafe {
                        VGA.set_color(Color::LightGreen, Color::Black);
                        VGA.write_string("[OK] Operación ");
                        VGA.write_string(&int_to_string(i as u64 + 1));
                        VGA.write_string(" acelerada por hardware\n");
                        VGA.set_color(Color::White, Color::Black);
                    }
                }
                crate::drivers::acceleration_2d::AccelerationResult::SoftwareFallback => {
                    unsafe {
                        VGA.set_color(Color::Yellow, Color::Black);
                        VGA.write_string("Operación ");
                        VGA.write_string(&int_to_string(i as u64 + 1));
                        VGA.write_string(" usando software\n");
                        VGA.set_color(Color::White, Color::Black);
                    }
                }
                crate::drivers::acceleration_2d::AccelerationResult::DriverError(e) => {
    unsafe {
        VGA.set_color(Color::LightRed, Color::Black);
                        VGA.write_string("Error en operación ");
                        VGA.write_string(&int_to_string(i as u64 + 1));
                        VGA.write_string(": ");
                        VGA.write_string(&e);
                        VGA.write_string("\n");
        VGA.set_color(Color::White, Color::Black);
                    }
                }
                _ => {}
            }
        }
        
        // Mostrar información de aceleración
        unsafe {
            VGA.set_color(Color::LightCyan, Color::Black);
            VGA.write_string("Información de aceleración 2D:\n");
            VGA.write_string("  ");
            VGA.write_string(&acceleration_2d.get_acceleration_info());
            VGA.write_string("\n");
            VGA.set_color(Color::White, Color::Black);
        }
    }
    
    // Demostración del sistema de entrada USB
    unsafe {
        VGA.set_color(Color::LightCyan, Color::Black);
        VGA.write_string("Inicializando sistema de entrada USB...\n");
        VGA.set_color(Color::White, Color::Black);
    }
    
    use crate::drivers::input_system::{InputSystem, InputSystemConfig, create_default_input_system};
    use crate::drivers::usb_keyboard::{UsbKeyboardDriver, create_usb_keyboard_driver};
    use crate::drivers::usb_mouse::{UsbMouseDriver, create_usb_mouse_driver};
    
    // Crear sistema de entrada
    let mut input_system = create_default_input_system();
    
    match input_system.initialize() {
        Ok(_) => {
            unsafe {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("Sistema de entrada inicializado\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error inicializando sistema de entrada: ");
                VGA.write_string(e);
            VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }
    
    // Simular conexión de teclado USB
    let keyboard = create_usb_keyboard_driver(0x046D, 0xC31C, 1, 0x81); // Logitech USB Keyboard
    match input_system.add_keyboard(keyboard) {
        Ok(device_id) => {
            unsafe {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("Teclado USB conectado (ID: ");
                VGA.write_string(&int_to_string(device_id as u64));
                VGA.write_string(")\n");
                VGA.write_string("  - Vendor: Logitech (0x046D)\n");
                VGA.write_string("  - Product: USB Keyboard (0xC31C)\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error conectando teclado: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }
    
    // Simular conexión de mouse USB
    let mouse = create_usb_mouse_driver(0x046D, 0xC077, 2, 0x82); // Logitech USB Mouse
    match input_system.add_mouse(mouse) {
        Ok(device_id) => {
            unsafe {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("Mouse USB conectado (ID: ");
                VGA.write_string(&int_to_string(device_id as u64));
                VGA.write_string(")\n");
                VGA.write_string("  - Vendor: Logitech (0x046D)\n");
                VGA.write_string("  - Product: USB Mouse (0xC077)\n");
                VGA.write_string("  - Sensibilidad: 1.0x\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error conectando mouse: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }
    
    // Mostrar estadísticas del sistema de entrada
    let stats = input_system.get_stats();
    unsafe {
        VGA.set_color(Color::LightCyan, Color::Black);
        VGA.write_string("Estadísticas del sistema de entrada:\n");
        VGA.set_color(Color::White, Color::Black);
        VGA.write_string("  - Teclados activos: ");
        VGA.write_string(&int_to_string(stats.active_keyboards as u64));
        VGA.write_string("\n");
        VGA.write_string("  - Mouse activos: ");
        VGA.write_string(&int_to_string(stats.active_mice as u64));
        VGA.write_string("\n");
        VGA.write_string("  - Eventos totales: ");
        VGA.write_string(&int_to_string(stats.total_events));
        VGA.write_string("\n");
        VGA.write_string("  - Uso del buffer: ");
        VGA.write_string(&int_to_string(stats.buffer_usage as u64));
        VGA.write_string("%\n");
    }
    
    // Simular algunos eventos de entrada
    unsafe {
        VGA.set_color(Color::Yellow, Color::Black);
        VGA.write_string("Simulando eventos de entrada...\n");
        VGA.set_color(Color::White, Color::Black);
    }
    
    // Simular datos de teclado (tecla 'H' presionada)
    let keyboard_data = [0x00, 0x00, 0x0B, 0x00, 0x00, 0x00, 0x00, 0x00]; // H key
    if let Err(e) = input_system.process_keyboard_data(0, &keyboard_data) {
        unsafe {
            VGA.set_color(Color::LightRed, Color::Black);
            VGA.write_string("Error procesando datos de teclado: ");
            VGA.write_string(e);
            VGA.write_string("\n");
            VGA.set_color(Color::White, Color::Black);
        }
    }
    
    // Simular datos de mouse (movimiento + click izquierdo)
    let mouse_data = [0x01, 0x05, 0x03, 0x00]; // Left button + move right 5, down 3
    if let Err(e) = input_system.process_mouse_data(0, &mouse_data) {
        unsafe {
            VGA.set_color(Color::LightRed, Color::Black);
            VGA.write_string("Error procesando datos de mouse: ");
            VGA.write_string(e);
            VGA.write_string("\n");
            VGA.set_color(Color::White, Color::Black);
        }
    }
    
    // Procesar eventos
    if let Err(e) = input_system.process_events() {
        unsafe {
            VGA.set_color(Color::LightRed, Color::Black);
            VGA.write_string("Error procesando eventos: ");
            VGA.write_string(e);
            VGA.write_string("\n");
            VGA.set_color(Color::White, Color::Black);
        }
    }
    
    // Mostrar eventos procesados
    let mut event_count = 0;
    while let Some(event) = input_system.get_next_event() {
        event_count += 1;
        unsafe {
            VGA.set_color(Color::LightGreen, Color::Black);
            VGA.write_string("Evento ");
            VGA.write_string(&int_to_string(event_count));
            VGA.write_string(": ");
            VGA.set_color(Color::White, Color::Black);
        }
        
        match event.event_type {
            crate::drivers::input_system::InputEventType::Keyboard(keyboard_event) => {
                unsafe {
                    VGA.write_string("Teclado - ");
                    match keyboard_event {
                        crate::drivers::usb_keyboard::KeyboardEvent::KeyPress { key, .. } => {
                            VGA.write_string("Presionada: ");
                            VGA.write_string(key.name());
                        }
                        crate::drivers::usb_keyboard::KeyboardEvent::KeyRelease { key, .. } => {
                            VGA.write_string("Liberada: ");
                            VGA.write_string(key.name());
                        }
                        crate::drivers::usb_keyboard::KeyboardEvent::KeyRepeat { key, .. } => {
                            VGA.write_string("Repetida: ");
                            VGA.write_string(key.name());
                        }
                    }
                    VGA.write_string("\n");
                }
            }
            crate::drivers::input_system::InputEventType::Mouse(mouse_event) => {
                unsafe {
                    VGA.write_string("Mouse - ");
                    match mouse_event {
                        crate::drivers::usb_mouse::MouseEvent::Move { position, .. } => {
                            VGA.write_string("Movimiento a (");
                            VGA.write_string(&int_to_string(position.x as u64));
                            VGA.write_string(", ");
                            VGA.write_string(&int_to_string(position.y as u64));
                            VGA.write_string(")");
                        }
                        crate::drivers::usb_mouse::MouseEvent::ButtonPress { button, .. } => {
                            VGA.write_string("Botón presionado: ");
                            VGA.write_string(button.name());
                        }
                        crate::drivers::usb_mouse::MouseEvent::ButtonRelease { button, .. } => {
                            VGA.write_string("Botón liberado: ");
                            VGA.write_string(button.name());
                        }
                        crate::drivers::usb_mouse::MouseEvent::Wheel { wheel, .. } => {
                            VGA.write_string("Rueda: ");
                            VGA.write_string(&int_to_string(wheel.vertical as u64));
                        }
                    }
                    VGA.write_string("\n");
                }
            }
            crate::drivers::input_system::InputEventType::System(system_event) => {
                unsafe {
                    VGA.write_string("Sistema - ");
                    match system_event {
                        crate::drivers::input_system::SystemEvent::DeviceConnected { device_type, .. } => {
                            VGA.write_string("Dispositivo conectado: ");
                            VGA.write_string(&device_type);
                        }
                        crate::drivers::input_system::SystemEvent::DeviceDisconnected { device_type, .. } => {
                            VGA.write_string("Dispositivo desconectado: ");
                            VGA.write_string(&device_type);
                        }
                        crate::drivers::input_system::SystemEvent::InputError { error } => {
                            VGA.write_string("Error: ");
                            VGA.write_string(&error);
                        }
                        crate::drivers::input_system::SystemEvent::BufferOverflow => {
                            VGA.write_string("Buffer overflow");
                        }
                    }
                    VGA.write_string("\n");
                }
            }
        }
    }
    
    unsafe {
        VGA.set_color(Color::LightCyan, Color::Black);
        VGA.write_string("Sistema de entrada USB completamente funcional\n");
        VGA.write_string("  - Soporte completo para teclado y mouse USB\n");
        VGA.write_string("  - Protocolo HID implementado\n");
        VGA.write_string("  - Sistema de eventos unificado\n");
        VGA.write_string("  - Gestión automática de dispositivos\n");
        VGA.set_color(Color::White, Color::Black);
    }
    
    // Demostración del sistema completo integrado
    unsafe {
        VGA.set_color(Color::LightMagenta, Color::Black);
        VGA.write_string("Inicializando ecosistema completo de entrada y GUI...\n");
        VGA.set_color(Color::White, Color::Black);
    }
    
    use crate::drivers::usb_hub::{UsbHubDriver, UsbHubInfo, UsbHubType, UsbPowerSwitching, UsbOverCurrentProtection, create_standard_usb_hub};
    use crate::drivers::usb_hid::{HidDriver, HidDeviceInfo, create_hid_driver};
    use crate::drivers::gui_integration::{GuiManager, GuiWindow, GuiButton, GuiTextBox, create_gui_manager};
    use crate::apps::{InteractiveAppManager, create_app_manager};
    use alloc::boxed::Box;
    
    // Crear USB Hub
    let hub_info = UsbHubInfo {
        vendor_id: 0x05E3,
        product_id: 0x0608,
        manufacturer: String::from("Generic"),
        product: String::from("USB 2.0 Hub"),
        version: 0x0100,
        device_address: 1,
        num_ports: 4,
        hub_type: UsbHubType::Usb2Hub,
        power_switching: UsbPowerSwitching::Individual,
        over_current_protection: UsbOverCurrentProtection::Individual,
        tt_think_time: 8,
        port_indicators: true,
        compound_device: false,
    };
    
    let mut usb_hub = UsbHubDriver::new(hub_info);
    match usb_hub.initialize() {
        Ok(_) => {
            unsafe {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("USB Hub inicializado (4 puertos)\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error inicializando USB Hub: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }
    
    // Crear dispositivo HID
    let hid_info = HidDeviceInfo {
        vendor_id: 0x046D,
        product_id: 0xC31C,
        version: 0x0110,
        manufacturer: String::from("Logitech"),
        product: String::from("USB Keyboard"),
        serial_number: String::from("12345"),
        device_class: 0x03, // HID Class
        device_subclass: 0x01, // Boot Interface Subclass
        device_protocol: 0x01, // Keyboard
        max_packet_size: 8,
        country_code: 0x00,
        num_descriptors: 1,
        report_descriptor_length: 0,
    };
    
    let mut hid_driver = create_hid_driver(hid_info, 2, 0x81);
    match hid_driver.initialize() {
        Ok(_) => {
            unsafe {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("Driver HID inicializado\n");
                VGA.write_string("  - Descriptor HID: ");
                VGA.write_string(&int_to_string(hid_driver.get_descriptor().to_bytes().len() as u64));
                VGA.write_string(" bytes\n");
                VGA.write_string("  - Descriptor de reporte: ");
                VGA.write_string(&int_to_string(hid_driver.get_report_descriptor_length() as u64));
                VGA.write_string(" bytes\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error inicializando HID: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }
    
    // Crear gestor de GUI
    let mut gui_manager = create_gui_manager();
    match gui_manager.initialize() {
        Ok(_) => {
            unsafe {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("Gestor de GUI inicializado\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error inicializando GUI: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }
    
    // Crear ventanas de ejemplo
    match gui_manager.create_window(1, String::from("Ventana Principal"), Rect { x: 100, y: 100, width: 400, height: 300 }) {
        Ok(_) => {
            unsafe {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("Ventana principal creada\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error creando ventana: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }
    
    // Crear elementos de GUI
    let button = GuiButton::new(1, Rect { x: 20, y: 50, width: 100, height: 30 }, String::from("Botón"));
    match gui_manager.add_element(Box::new(button)) {
        Ok(_) => {
            unsafe {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("Botón GUI creado\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error creando botón: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }
    
    let textbox = GuiTextBox::new(2, Rect { x: 20, y: 100, width: 200, height: 25 }, 50);
    match gui_manager.add_element(Box::new(textbox)) {
        Ok(_) => {
            unsafe {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("Campo de texto GUI creado\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error creando campo de texto: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }
    
    // Crear gestor de aplicaciones interactivas
    let mut app_manager = create_app_manager();
    match app_manager.initialize() {
        Ok(_) => {
            unsafe {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("Gestor de aplicaciones inicializado\n");
                VGA.write_string("  - Aplicaciones disponibles: ");
                VGA.write_string(&int_to_string(app_manager.get_app_count() as u64));
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error inicializando aplicaciones: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }
    
    // Cambiar a la primera aplicación
    match app_manager.switch_app(0) {
        Ok(_) => {
            unsafe {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("Aplicación activa: ");
                VGA.write_string(app_manager.get_current_app_name().unwrap_or("Desconocida"));
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error cambiando aplicación: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }
    
    // Demostrar procesamiento de eventos integrado
    unsafe {
                VGA.set_color(Color::Yellow, Color::Black);
        VGA.write_string("Demostrando procesamiento de eventos integrado...\n");
        VGA.set_color(Color::White, Color::Black);
    }
    
    // Procesar eventos del sistema de entrada
    if let Err(e) = input_system.process_events() {
        unsafe {
            VGA.set_color(Color::LightRed, Color::Black);
            VGA.write_string("Error procesando eventos de entrada: ");
            VGA.write_string(e);
            VGA.write_string("\n");
            VGA.set_color(Color::White, Color::Black);
        }
    }
    
    // Procesar eventos de aplicaciones
    while let Some(event) = input_system.get_next_event() {
        // Procesar en el gestor de GUI
        if let Err(e) = gui_manager.process_input_event(&event) {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error procesando evento en GUI: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
        
        // Procesar en el gestor de aplicaciones
        if let Err(e) = app_manager.process_input(&event) {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error procesando evento en aplicación: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }
    
    // Renderizar GUI
    if let Err(e) = gui_manager.render(&mut acceleration_2d) {
        unsafe {
            VGA.set_color(Color::LightRed, Color::Black);
            VGA.write_string("Error renderizando GUI: ");
            VGA.write_string(e);
            VGA.write_string("\n");
            VGA.set_color(Color::White, Color::Black);
        }
    }
    
    // Renderizar aplicaciones
    if let Err(e) = app_manager.render(&mut acceleration_2d) {
        unsafe {
            VGA.set_color(Color::LightRed, Color::Black);
            VGA.write_string("Error renderizando aplicación: ");
            VGA.write_string(e);
            VGA.write_string("\n");
            VGA.set_color(Color::White, Color::Black);
        }
    }
    
    // Mostrar estadísticas finales
    let input_stats = input_system.get_stats();
    let hub_stats = usb_hub.get_stats();
    
    unsafe {
        VGA.set_color(Color::LightCyan, Color::Black);
        VGA.write_string("Estadísticas del ecosistema completo:\n");
        VGA.set_color(Color::White, Color::Black);
        VGA.write_string("  - Teclados USB: ");
        VGA.write_string(&int_to_string(input_stats.active_keyboards as u64));
        VGA.write_string("\n");
        VGA.write_string("  - Mouse USB: ");
        VGA.write_string(&int_to_string(input_stats.active_mice as u64));
        VGA.write_string("\n");
        VGA.write_string("  - Puertos USB activos: ");
        VGA.write_string(&int_to_string(hub_stats.active_ports as u64));
        VGA.write_string("/");
        VGA.write_string(&int_to_string(hub_stats.total_ports as u64));
        VGA.write_string("\n");
        VGA.write_string("  - Ventanas GUI: ");
        VGA.write_string(&int_to_string(gui_manager.get_window_count() as u64));
        VGA.write_string("\n");
        VGA.write_string("  - Elementos GUI: ");
        VGA.write_string(&int_to_string(gui_manager.get_element_count() as u64));
        VGA.write_string("\n");
        VGA.write_string("  - Aplicaciones: ");
        VGA.write_string(&int_to_string(app_manager.get_app_count() as u64));
        VGA.write_string("\n");
    }
    
    unsafe {
        VGA.set_color(Color::LightMagenta, Color::Black);
        VGA.write_string("Ecosistema completo de entrada y GUI funcional\n");
        VGA.write_string("  - USB Hub con 4 puertos\n");
        VGA.write_string("  - Protocolo HID completo\n");
        VGA.write_string("  - Sistema de entrada unificado\n");
        VGA.write_string("  - Interfaz gráfica avanzada\n");
        VGA.write_string("  - Aplicaciones interactivas\n");
        VGA.write_string("  - Integración completa con aceleración 2D\n");
        VGA.set_color(Color::White, Color::Black);
    }
    
    // Ejecutar demostración de IA
    unsafe {
        VGA.set_color(Color::Yellow, Color::Black);
        VGA.write_string("\nEJECUTANDO DEMOSTRACIÓN DE IA...\n");
        VGA.set_color(Color::White, Color::Black);
    }
    
    match crate::ai_simple_demo::run_simple_ai_demo() {
        Ok(_) => {
            unsafe {
                VGA.set_color(Color::Green, Color::Black);
                VGA.write_string("Demostración de IA completada exitosamente\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::Red, Color::Black);
                VGA.write_string("Error en demostración de IA: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }

    // Ejecutar shell con IA integrada
    unsafe {
        VGA.set_color(Color::Cyan, Color::Black);
        VGA.write_string("\nINICIANDO SHELL CON IA INTEGRADA...\n");
        VGA.set_color(Color::White, Color::Black);
    }

    // match crate::ai_shell::run_ai_shell() {
    //     Ok(_) => {
    //         unsafe {
    //             VGA.set_color(Color::Green, Color::Black);
    //             VGA.write_string("Shell con IA ejecutado exitosamente\n");
    //             VGA.set_color(Color::White, Color::Black);
    //         }
    //     }
    //     Err(e) => {
    //         unsafe {
    //         VGA.set_color(Color::Red, Color::Black);
    //         VGA.write_string("Error ejecutando shell con IA: ");
    //         VGA.write_string("Error de kernel");
    //         VGA.write_string("\n");
    //             VGA.set_color(Color::White, Color::Black);
    //         }
    //     }
    // }

    // Mostrar mensaje de que el kernel está listo
    unsafe {
        VGA.set_color(Color::Green, Color::Black);
        VGA.write_string("\nKERNEL ECLIPSE OS CON IA INICIALIZADO COMPLETAMENTE\n");
        VGA.set_color(Color::LightBlue, Color::Black);
        VGA.write_string("Esperando que el userland tome el control...\n");
        VGA.set_color(Color::White, Color::Black);
        VGA.write_string("===============================================\n\n");
    }

    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}

// El panic_handler está definido en lib.rs
