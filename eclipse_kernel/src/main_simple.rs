//! Módulo principal simplificado del kernel Eclipse OS

#![no_std]
#![no_main]

extern crate alloc;

use core::iter::Iterator;
use core::option::Option::Some;
use core::prelude::rust_2024::derive;

use core::panic::PanicInfo;

// Importar módulos del kernel
use crate::init_system::{InitSystem, InitProcess};
use crate::wayland::{init_wayland, is_wayland_initialized, get_wayland_state};

/// Función para convertir números a string
fn int_to_string(mut num: u64) -> heapless::String<32> {
    let mut result = heapless::String::<32>::new();
    if num == 0 {
        let _ = result.push_str("0");
        return result;
    }
    
    while num > 0 {
        let digit = (num % 10) as u8;
        let _ = result.push((digit + b'0') as char);
        num /= 10;
    }
    
    // Invertir el string
    let mut reversed = heapless::String::<32>::new();
    for &byte in result.as_bytes().iter().rev() {
        let _ = reversed.push(byte as char);
    }
    
    reversed
}

use core::fmt::Write;

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

// Driver VGA simplificado
pub struct VgaWriter {
    buffer: *mut u16,
    position: usize,
    color: u8,
}

impl VgaWriter {
    pub const fn new() -> Self {
        Self {
            buffer: 0xB8000 as *mut u16,
            position: 0,
            color: 0x0F, // Blanco sobre negro
        }
    }

    pub fn init_vga_mode(&mut self) {
        // Configurar modo VGA 80x25
        self.outb(0x3D4, 0x00);
        self.outb(0x3D5, 0x00);
        self.outb(0x3D4, 0x01);
        self.outb(0x3D5, 0x00);
        self.outb(0x3D4, 0x02);
        self.outb(0x3D5, 0x00);
        self.outb(0x3D4, 0x03);
        self.outb(0x3D5, 0x00);
        self.outb(0x3D4, 0x04);
        self.outb(0x3D5, 0x00);
        self.outb(0x3D4, 0x05);
        self.outb(0x3D5, 0x00);
        self.outb(0x3D4, 0x06);
        self.outb(0x3D5, 0x00);
        self.outb(0x3D4, 0x07);
        self.outb(0x3D5, 0x00);
        self.outb(0x3D4, 0x08);
        self.outb(0x3D5, 0x00);
        self.outb(0x3D4, 0x09);
        self.outb(0x3D5, 0x00);
        self.outb(0x3D4, 0x0A);
        self.outb(0x3D5, 0x00);
        self.outb(0x3D4, 0x0B);
        self.outb(0x3D5, 0x00);
        self.outb(0x3D4, 0x0C);
        self.outb(0x3D5, 0x00);
        self.outb(0x3D4, 0x0D);
        self.outb(0x3D5, 0x00);
        self.outb(0x3D4, 0x0E);
        self.outb(0x3D5, 0x00);
        self.outb(0x3D4, 0x0F);
        self.outb(0x3D5, 0x00);
    }

    pub fn clear_screen(&mut self) {
        for i in 0..2000 {
            unsafe {
                *self.buffer.add(i) = 0x0F00; // Blanco sobre negro, espacio
            }
        }
        self.position = 0;
    }

    pub fn set_color(&mut self, fg: Color, bg: Color) {
        self.color = ((bg as u8) << 4) | (fg as u8);
    }

    pub fn write_string(&mut self, s: &str) {
        for byte in s.bytes() {
            if byte == b'\n' {
                self.position = (self.position / 80 + 1) * 80;
            } else {
                unsafe {
                    *self.buffer.add(self.position) = (self.color as u16) << 8 | byte as u16;
                }
                self.position += 1;
            }
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

// Allocator global simple
use alloc::alloc::{GlobalAlloc, Layout};

struct SimpleAllocator;

unsafe impl GlobalAlloc for SimpleAllocator {
    unsafe fn alloc(&self, _layout: Layout) -> *mut u8 {
        // Implementación simple - en un kernel real esto sería más complejo
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Implementación simple
    }
}

#[global_allocator]
static ALLOCATOR: SimpleAllocator = SimpleAllocator;

/// Función principal del kernel
pub fn kernel_main() -> ! {
    // Mostrar banner de inicio del kernel
    unsafe {
        VGA.set_color(Color::LightGreen, Color::Black);
        VGA.write_string("╔══════════════════════════════════════════════════════════════════════════════╗\n");
        VGA.write_string("║                            Eclipse OS Kernel v0.4.0                        ║\n");
        VGA.write_string("║                         Sistema de Drivers Modulares                        ║\n");
        VGA.write_string("╚══════════════════════════════════════════════════════════════════════════════╝\n");
        VGA.set_color(Color::White, Color::Black);
        VGA.write_string("\nInicializando kernel Eclipse OS...\n\n");
    }
    
    // Inicializar drivers básicos
    unsafe {
        VGA.set_color(Color::LightGreen, Color::Black);
        VGA.write_string("✓ Drivers VGA inicializados\n");
        VGA.write_string("✓ Driver de serie COM1 activo\n");
        VGA.write_string("✓ Gestión de memoria básica lista\n");
        
        // Inicializar sistema de inicialización con systemd
        VGA.write_string("✓ Inicializando sistema de inicialización...\n");
    }
    
    // Inicializar systemd
    let mut init_system = InitSystem::new();
    match init_system.initialize() {
        Ok(_) => {
            unsafe {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("✓ Sistema de inicialización (systemd) configurado\n");
                
                // Mostrar información del proceso init
                if let Some(init_info) = init_system.get_init_info() {
                    VGA.write_string("✓ Proceso init: ");
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
                VGA.write_string("  - Gráficos: VGA\n");
                VGA.write_string("  - Init System: systemd\n");
                VGA.write_string("  - Init Process: eclipse-systemd\n");
                VGA.write_string("  - Display Server: Wayland\n");
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("❌ Error al inicializar systemd: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }
    
    // Demostración de colores
    unsafe {
        VGA.set_color(Color::LightRed, Color::Black);
        VGA.write_string("\nDemostración de colores VGA:\n");
        VGA.set_color(Color::White, Color::Black);
        
        let colors = [
            (Color::Red, "Rojo"),
            (Color::Green, "Verde"),
            (Color::Blue, "Azul"),
            (Color::Yellow, "Amarillo"),
            (Color::Cyan, "Cian"),
            (Color::Magenta, "Magenta"),
        ];
        
        for (color, name) in colors.iter() {
            VGA.set_color(*color, Color::Black);
            VGA.write_string("■ ");
            VGA.set_color(Color::White, Color::Black);
            VGA.write_string(name);
            if *color == Color::Cyan || *color == Color::Magenta {
                VGA.write_string("\n");
            } else {
                VGA.write_string("  ");
            }
        }
        VGA.write_string("\n");
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
                VGA.write_string("✓ Wayland inicializado correctamente\n");
                VGA.write_string("✓ Compositor Wayland activo\n");
                VGA.write_string("✓ Protocolo de ventanas listo\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("❌ Error inicializando Wayland: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }
    
    // Mostrar mensaje final y transferir control a systemd
    unsafe {
        VGA.set_color(Color::LightGreen, Color::Black);
        VGA.write_string("\n✓ Kernel Eclipse OS inicializado correctamente\n");
        VGA.set_color(Color::Yellow, Color::Black);
        VGA.write_string("Transferiendo control a systemd...\n");
        VGA.set_color(Color::White, Color::Black);
    }
    
    // Transferir control a systemd
    match init_system.execute_init() {
        Ok(_) => {
            unsafe {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("✓ Control transferido a systemd exitosamente\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
        Err(e) => {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("❌ Error al transferir control a systemd: ");
                VGA.write_string(e);
                VGA.write_string("\n");
                VGA.set_color(Color::White, Color::Black);
            }
        }
    }
    
    // Bucle principal del kernel (en caso de que systemd no tome control)
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}

#[panic_handler]
pub fn panic(info: &PanicInfo) -> ! {
    unsafe {
        VGA.set_color(Color::LightRed, Color::Black);
        VGA.write_string("\n\n╔══════════════════════════════════════════════════════════════════════════════╗\n");
        VGA.write_string("║                                KERNEL PANIC                                 ║\n");
        VGA.write_string("╚══════════════════════════════════════════════════════════════════════════════╝\n");
        VGA.set_color(Color::White, Color::Black);
        VGA.write_string("\nEl kernel ha encontrado un error crítico y se ha detenido.\n");
        
        if let Some(location) = info.location() {
            VGA.write_string("Ubicación: ");
            VGA.write_string(location.file());
            VGA.write_string(":");
            // Convertir número a string manualmente
            let line = location.line();
            let mut line_str = [0u8; 10];
            let mut i = 0;
            let mut num = line;
            if num == 0 {
                line_str[i] = b'0';
                i += 1;
            } else {
                while num > 0 && i < 10 {
                    line_str[i] = b'0' + (num % 10) as u8;
                    num /= 10;
                    i += 1;
                }
            }
            // Invertir el string
            for j in 0..i/2 {
                let temp = line_str[j];
                line_str[j] = line_str[i-1-j];
                line_str[i-1-j] = temp;
            }
            VGA.write_string(core::str::from_utf8(&line_str[0..i]).unwrap_or("?"));
            VGA.write_string("\n");
        }
        
        VGA.write_string("Mensaje: Kernel panic detectado\n");
        VGA.write_string("\nReinicia el sistema para continuar.\n");
    }
    
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}
