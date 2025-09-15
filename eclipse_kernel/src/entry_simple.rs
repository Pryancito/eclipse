//! Punto de entrada UEFI para Eclipse OS Kernel
//!
//! Este archivo proporciona un punto de entrada compatible con UEFI
//! que recibe información del framebuffer del bootloader UEFI.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

// Importar módulos necesarios
use crate::drivers::framebuffer::get_framebuffer;
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
    // Configurar SSE/MMX inmediatamente al inicio
    unsafe {
        // Asegurar que SSE esté habilitado
        core::arch::asm!(
            "mov rax, cr0",
            "and rax, ~(1 << 2)",        // CR0.EM = 0
            "or  rax,  (1 << 1)",        // CR0.MP = 1
            "mov cr0, rax",
            "mov rax, cr4",
            "or  rax,  (1 << 9)",        // CR4.OSFXSR = 1
            "or  rax,  (1 << 10)",       // CR4.OSXMMEXCPT = 1
            "mov cr4, rax"
        );
    }
    
    // Llamar a la función principal del kernel
    // Inicializar el framebuffer usando la información recibida
    // El puntero framebuffer_info nunca es Option, es un puntero crudo.
    // Hay que leerlo de forma segura antes de usarlo.
    unsafe {
        let info = core::ptr::read_volatile(framebuffer_info);
        
        // Debug: Verificar información del framebuffer recibida
        // Intentar escribir directamente al framebuffer para test
        if info.base_address != 0 {
            let fb_ptr = info.base_address as *mut u32;
            
            // Escribir patrón de test directamente
            for y in 0..info.height.min(50) {
                for x in 0..info.width.min(50) {
                    let offset = (y * info.width + x) as isize;
                    core::ptr::write_volatile(fb_ptr.add(offset as usize), 0x00FF0000); // Rojo
                }
            }
            
            // Dibujar rectángulo verde
            for y in 0..20 {
                for x in 0..40 {
                    let offset = (y * info.width + x) as isize;
                    core::ptr::write_volatile(fb_ptr.add(offset as usize), 0x0000FF00); // Verde
                }
            }
        }
        
        if let Some(fb) = crate::drivers::framebuffer::init_framebuffer(
            info.base_address,
            info.width,
            info.height,
            info.pixels_per_scan_line,
            info.pixel_format,
            info.red_mask | info.green_mask | info.blue_mask
        ).ok().and_then(|_| get_framebuffer()) {
            crate::main_simple::kernel_main(fb);
        } else {
            // Si falla la inicialización, intentar escribir directamente
            if info.base_address != 0 {
                let fb_ptr = info.base_address as *mut u32;
                
                // Escribir patrón de emergencia
                for y in 0..info.height.min(100) {
                    for x in 0..info.width.min(100) {
                        let offset = (y * info.width + x) as isize;
                        core::ptr::write_volatile(fb_ptr.add(offset as usize), 0x000000FF); // Azul
                    }
                }
            }
        }
    }
    
    // El kernel_main ya tiene su propio bucle, pero por si acaso:
    loop {
        for _ in 0..100000 {
            core::hint::spin_loop();
        }
    }
}

/// Obtener información del framebuffer si está disponible
pub fn get_framebuffer_info() -> Option<FramebufferInfo> {
    unsafe { FRAMEBUFFER_INFO }
}