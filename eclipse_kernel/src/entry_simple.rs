//! Punto de entrada simple para Eclipse OS Kernel
//! 
//! Este archivo proporciona un punto de entrada básico para el kernel
//! que muestra "Eclipse OS" centrado en pantalla negra.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

// panic_handler definido en lib.rs

/// Función principal del kernel
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Inicializar VGA y mostrar "Eclipse OS" centrado
    init_vga_and_display();
    
    // Bucle infinito del kernel
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}

/// Inicializar VGA y mostrar "Eclipse OS" centrado
fn init_vga_and_display() {
    unsafe {
        let vga_buffer = 0xb8000 as *mut u16;
        
        // Limpiar toda la pantalla con fondo negro
        for i in 0..2000 {
            *vga_buffer.add(i) = 0x0000; // Negro sobre negro (fondo negro)
        }
        
        // Mostrar "Eclipse OS" centrado
        display_centered_text("Eclipse OS");
    }
}

/// Mostrar texto centrado en la pantalla
fn display_centered_text(text: &str) {
    unsafe {
        let vga_buffer = 0xb8000 as *mut u16;
        
        // Calcular posición central
        // Pantalla: 80 columnas x 25 filas
        let text_len = text.len();
        let start_col = (80 - text_len) / 2; // Centrar horizontalmente
        let start_row = 12; // Centrar verticalmente (fila 12 de 25)
        let start_pos = start_row * 80 + start_col;
        
        // Escribir texto centrado
        for (i, byte) in text.bytes().enumerate() {
            if start_pos + i < 2000 {
                *vga_buffer.add(start_pos + i) = 0x0F00 | byte as u16; // Blanco sobre negro
            }
        }
    }
}
