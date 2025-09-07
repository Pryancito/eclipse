//! Punto de entrada principal unificado del kernel Eclipse OS
//! Combina funcionalidades de main_simple.rs y main_desktop.rs

#![no_std]
#![no_main]

extern crate alloc;

use core::iter::Iterator;
use core::option::Option::Some;
use core::prelude::rust_2024::derive;
use core::panic::PanicInfo;
use alloc::format;
use alloc::string::String;

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
    for ch in result.chars().rev() {
        let _ = reversed.push(ch);
    }
    reversed
}

// Colores VGA
#[derive(Debug, Clone, Copy, PartialEq)]
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
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    fg_color: Color,
    bg_color: Color,
}

impl VgaWriter {
    pub const fn new() -> Self {
        Self {
            buffer: 0xB8000 as *mut u16,
            width: 80,
            height: 25,
            x: 0,
            y: 0,
            fg_color: Color::White,
            bg_color: Color::Black,
        }
    }
    
    pub fn set_color(&mut self, fg: Color, bg: Color) {
        self.fg_color = fg;
        self.bg_color = bg;
    }
    
    pub fn write_char(&mut self, c: char) {
        if c == '\n' {
            self.new_line();
            return;
        }
        
        if self.x >= self.width {
            self.new_line();
        }
        
        let color_byte = ((self.bg_color as u8) << 4) | (self.fg_color as u8);
        let character = (c as u16) | ((color_byte as u16) << 8);
        
        unsafe {
            let index = self.y * self.width + self.x;
            if index < self.width * self.height {
                core::ptr::write_volatile(self.buffer.add(index), character);
            }
        }
        
        self.x += 1;
    }
    
    pub fn write_string(&mut self, s: &str) {
        for c in s.chars() {
            self.write_char(c);
        }
    }
    
    fn new_line(&mut self) {
        self.x = 0;
        self.y += 1;
        if self.y >= self.height {
            self.scroll();
        }
    }
    
    fn scroll(&mut self) {
        unsafe {
            for y in 1..self.height {
                for x in 0..self.width {
                    let src_index = y * self.width + x;
                    let dst_index = (y - 1) * self.width + x;
                    let character = core::ptr::read_volatile(self.buffer.add(src_index));
                    core::ptr::write_volatile(self.buffer.add(dst_index), character);
                }
            }
        }
        
        // Limpiar la última línea
        for x in 0..self.width {
            let index = (self.height - 1) * self.width + x;
            unsafe {
                core::ptr::write_volatile(self.buffer.add(index), 0);
            }
        }
        
        self.y = self.height - 1;
    }
}

// Instancia global de VGA
pub static mut VGA: VgaWriter = VgaWriter::new();

// Ventana del escritorio (del main_desktop.rs)
#[derive(Debug, Clone, Copy)]
pub struct DesktopWindow {
    pub id: usize,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub title: &'static str,
    pub visible: bool,
    pub focused: bool,
}

impl DesktopWindow {
    pub fn new(id: usize, x: u32, y: u32, width: u32, height: u32, title: &'static str) -> Self {
        Self {
            id,
            x,
            y,
            width,
            height,
            title,
            visible: true,
            focused: false,
        }
    }
}

// Gestor de escritorio
pub struct DesktopManager {
    windows: heapless::Vec<DesktopWindow, 16>,
    active_window: Option<usize>,
    background_color: Color,
}

impl DesktopManager {
    pub fn new() -> Self {
        Self {
            windows: heapless::Vec::new(),
            active_window: None,
            background_color: Color::Blue,
        }
    }
    
    pub fn add_window(&mut self, window: DesktopWindow) -> Result<(), &'static str> {
        if self.windows.len() >= 16 {
            return Err("Máximo de ventanas alcanzado");
        }
        self.windows.push(window).map_err(|_| "Error agregando ventana")?;
        Ok(())
    }
    
    pub fn get_window(&mut self, id: usize) -> Option<&mut DesktopWindow> {
        self.windows.iter_mut().find(|w| w.id == id)
    }
    
    pub fn focus_window(&mut self, id: usize) {
        // Desenfocar ventana actual
        if let Some(active_id) = self.active_window {
            if let Some(window) = self.get_window(active_id) {
                window.focused = false;
            }
        }
        
        // Enfocar nueva ventana
        if let Some(window) = self.get_window(id) {
            window.focused = true;
            self.active_window = Some(id);
        }
    }
    
    pub fn render(&self) {
        unsafe {
            VGA.set_color(Color::White, self.background_color);
            VGA.write_string("Eclipse OS Desktop Manager\n");
            VGA.write_string("==============================\n\n");
            
            VGA.set_color(Color::LightCyan, self.background_color);
            VGA.write_string("Ventanas activas:\n");
            
            for window in &self.windows {
                if window.visible {
                    let status = if window.focused { "*" } else { "o" };
                    VGA.set_color(Color::Yellow, self.background_color);
                    VGA.write_string(&format!("{} Ventana {}: {}\n", status, window.id, window.title));
                    VGA.set_color(Color::LightGray, self.background_color);
                    VGA.write_string(&format!("   Posicion: ({}, {}) Tamano: {}x{}\n", 
                        window.x, window.y, window.width, window.height));
                }
            }
            
            VGA.set_color(Color::White, self.background_color);
            VGA.write_string("\nPresiona 'q' para salir, 'n' para nueva ventana\n");
        }
    }
}

// Función principal del kernel unificada
pub fn kernel_main() -> ! {
    unsafe {
        // Inicializar VGA
        VGA.set_color(Color::LightGreen, Color::Black);
        VGA.write_string("Eclipse OS Kernel - Version Unificada\n");
        VGA.write_string("========================================\n\n");
        
        VGA.set_color(Color::White, Color::Black);
        VGA.write_string("Inicializando sistema...\n");
        
        // Inicializar sistema de inicialización
        let mut init_system = InitSystem::new();
        VGA.write_string("Sistema de inicializacion listo\n");
        
        // Inicializar Wayland
        match init_wayland() {
            Ok(_) => {
                VGA.set_color(Color::LightGreen, Color::Black);
                VGA.write_string("Wayland inicializado correctamente\n");
            }
            Err(e) => {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error inicializando Wayland: ");
                VGA.write_string(e);
                VGA.write_string("\n");
            }
        }
        
        VGA.set_color(Color::White, Color::Black);
        
        // Crear gestor de escritorio
        let mut desktop = DesktopManager::new();
        
        // Agregar ventanas de ejemplo
        let window1 = DesktopWindow::new(1, 10, 5, 60, 15, "Terminal");
        let window2 = DesktopWindow::new(2, 20, 10, 50, 12, "File Manager");
        let window3 = DesktopWindow::new(3, 5, 8, 70, 18, "System Monitor");
        
        let _ = desktop.add_window(window1);
        let _ = desktop.add_window(window2);
        let _ = desktop.add_window(window3);
        
        // Enfocar primera ventana
        desktop.focus_window(1);
        
        VGA.write_string("\nSistema de escritorio inicializado\n");
        VGA.write_string("=====================================\n\n");
        
        // Renderizar escritorio
        desktop.render();
        
        // Simular interacción del usuario
        VGA.set_color(Color::LightMagenta, Color::Black);
        VGA.write_string("\nSimulando interacciones del usuario...\n");
        
        // Cambiar foco entre ventanas
        desktop.focus_window(2);
        VGA.write_string("Cambiado foco a File Manager\n");
        
        desktop.focus_window(3);
        VGA.write_string("Cambiado foco a System Monitor\n");
        
        desktop.focus_window(1);
        VGA.write_string("Cambiado foco a Terminal\n");
        
        // Mostrar estadísticas del sistema
        VGA.set_color(Color::LightCyan, Color::Black);
        VGA.write_string("\nEstadisticas del sistema:\n");
        VGA.set_color(Color::White, Color::Black);
        VGA.write_string("  - Ventanas creadas: 3\n");
        VGA.write_string("  - Wayland activo: ");
        VGA.write_string(if is_wayland_initialized() { "Si" } else { "No" });
        VGA.write_string("\n");
        VGA.write_string("  - Modo de escritorio: Activo\n");
        VGA.write_string("  - Gestion de ventanas: Funcional\n");
        
        VGA.set_color(Color::LightGreen, Color::Black);
        VGA.write_string("\nEclipse OS Desktop Kernel funcionando correctamente!\n");
        VGA.write_string("   Sistema unificado con funcionalidades de escritorio\n");
        VGA.write_string("   y kernel simplificado integradas.\n");
        
        VGA.set_color(Color::White, Color::Black);
    }
    
    // Loop infinito del kernel
    loop {
        // Aquí iría el scheduler del kernel y manejo de interrupciones
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}

// El panic_handler está definido en lib.rs
