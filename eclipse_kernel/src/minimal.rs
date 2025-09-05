//! ReactOS Rust Kernel - Versión Mínima
//! 
//! Kernel mínimo para verificar que la compilación funciona

#![no_std]
#![no_main]

use core::panic::PanicInfo;

/// Punto de entrada principal del kernel
#[no_mangle]
pub extern "C" fn _start() -> ! {
    let vga_buffer = 0xb8000 as *mut u8;
    let hello = b"Hello from ReactOS Rust Minimal Kernel!";
    
    for (i, &byte) in hello.iter().enumerate() {
        unsafe {
            *vga_buffer.add(i * 2) = byte;
            *vga_buffer.add(i * 2 + 1) = 0xf; // White on black
        }
    }

    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}

/// Manejador de pánico
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    let vga_buffer = 0xb8000 as *mut u8;
    let msg = b"PANIC: Minimal Kernel has panicked!";
    
    for (i, &byte) in msg.iter().enumerate() {
        unsafe {
            *vga_buffer.add(160 + i * 2) = byte; // Second line
            *vga_buffer.add(160 + i * 2 + 1) = 0x4; // Red
        }
    }
    
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}
