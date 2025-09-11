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
        // Logging removido temporalmente para evitar breakpoint
    }

    // Llamar a la función principal del kernel
    match crate::main_simple::kernel_main() {
        Ok(_) => {
            loop {

                for _ in 0..100000 {
                    core::hint::spin_loop();
                }
            }
        }
        Err(e) => {
            loop {
                for _ in 0..100000 {
                    core::hint::spin_loop();
                }
            }
        }
    }
}

/// Obtener información del framebuffer si está disponible
pub fn get_framebuffer_info() -> Option<FramebufferInfo> {
    unsafe { FRAMEBUFFER_INFO }
}