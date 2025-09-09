//! Punto de entrada UEFI para Eclipse OS Kernel
//!
//! Este archivo proporciona un punto de entrada compatible con UEFI
//! que recibe información del framebuffer del bootloader UEFI.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

// Importar módulos necesarios
use crate::main_simple::kernel_main;

// Estructura para información del framebuffer (debe coincidir con el bootloader)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub base_address: u64,
    pub width: u32,
    pub height: u32,
    pub pixels_per_scan_line: u32,
    pub pixel_format: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub reserved_mask: u32,
}

// Variable global para almacenar información del framebuffer
static mut FRAMEBUFFER_INFO: Option<FramebufferInfo> = None;

/// Punto de entrada principal del kernel compatible con UEFI
/// Esta función es llamada por el bootloader UEFI con información del framebuffer
#[no_mangle]
pub extern "C" fn uefi_entry(framebuffer_info: *const FramebufferInfo) -> ! {
    // Guardar información del framebuffer si está disponible
    if !framebuffer_info.is_null() {
        unsafe {
            FRAMEBUFFER_INFO = Some(*framebuffer_info);
        }
    }

    // Inicializar serial para debugging
    unsafe {
        serial_init();
        serial_write_str("KERNEL: UEFI entry point reached\r\n");
    }

    // Llamar a la función principal del kernel
    match crate::main_simple::kernel_main() {
        Ok(_) => {
            unsafe {
                serial_write_str("KERNEL: kernel_main() returned Ok, entering infinite loop\r\n");
            }
            loop {
                unsafe { core::arch::asm!("hlt"); }
            }
        }
        Err(e) => {
            unsafe {
                serial_write_str("KERNEL: kernel_main() returned error: ");
                serial_write_str(e);
                serial_write_str("\r\n");
            }
            loop {
                unsafe { core::arch::asm!("hlt"); }
            }
        }
    }
}

/// Obtener información del framebuffer si está disponible
pub fn get_framebuffer_info() -> Option<FramebufferInfo> {
    unsafe { FRAMEBUFFER_INFO }
}

// Funciones de serial para debugging temprano
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
    for &c in s.as_bytes() {
        serial_write_byte(c);
    }
}
