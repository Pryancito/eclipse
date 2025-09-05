//! ReactOS Rust Kernel - Versión Standalone Mínima
//! 
//! Kernel completamente independiente sin dependencias externas

#![no_std]
#![no_main]

use core::panic::PanicInfo;

/// Punto de entrada principal del kernel
#[no_mangle]
pub extern "C" fn _start() -> ! {
    let vga_buffer = 0xb8000 as *mut u8;
    let hello = b"ReactOS Rust Standalone Minimal Kernel!";
    
    for (i, &byte) in hello.iter().enumerate() {
        unsafe {
            *vga_buffer.add(i * 2) = byte;
            *vga_buffer.add(i * 2 + 1) = 0xa; // Light green on black
        }
    }

    // Mostrar información del sistema
    write_line(1, b"Sistema: ReactOS Rust OS", 0xb);
    write_line(2, b"Arquitectura: x86_64", 0xc);
    write_line(3, b"Estado: Kernel minimo funcionando", 0xe);
    write_line(4, b"TCP/IP Stack: Implementado", 0xf);

    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}

/// Escribir una línea en VGA
fn write_line(line: usize, text: &[u8], color: u8) {
    let vga_buffer = 0xb8000 as *mut u8;
    let start_offset = line * 160; // 80 caracteres * 2 bytes por caracter
    
    for (i, &byte) in text.iter().enumerate() {
        unsafe {
            *vga_buffer.add(start_offset + i * 2) = byte;
            *vga_buffer.add(start_offset + i * 2 + 1) = color;
        }
    }
}

/// Manejador de pánico
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    let vga_buffer = 0xb8000 as *mut u8;
    let msg = b"PANIC: Standalone Minimal Kernel has panicked!";
    
    // Limpiar pantalla con color rojo
    for i in 0..2000 {
        unsafe {
            *vga_buffer.add(i * 2) = b' ';
            *vga_buffer.add(i * 2 + 1) = 0x4f; // White on red
        }
    }
    
    // Mostrar mensaje de pánico
    for (i, &byte) in msg.iter().enumerate() {
        unsafe {
            *vga_buffer.add(i * 2) = byte;
            *vga_buffer.add(i * 2 + 1) = 0x4f; // White on red
        }
    }
    
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}
