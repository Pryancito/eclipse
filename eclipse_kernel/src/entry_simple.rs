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
use crate::drivers::framebuffer::FramebufferInfo; // Importar desde drivers/framebuffer.rs

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
    
    // Inicializar allocador global antes de cualquier uso de alloc
    #[cfg(feature = "alloc")]
    {
        crate::allocator::init_allocator();
    }
    
    // Llamar a la función principal del kernel
    // Inicializar el framebuffer usando la información recibida
    unsafe {
        let info = core::ptr::read_volatile(framebuffer_info);
        
        // Almacenar la información del framebuffer globalmente
        FRAMEBUFFER_INFO = Some(info);
        
        // Inicializar el framebuffer correctamente
        if let Ok(_) = crate::drivers::framebuffer::init_framebuffer(
            info.base_address,
            info.width,
            info.height,
            info.pixels_per_scan_line,
            info.pixel_format,
            info.red_mask | info.green_mask | info.blue_mask
        ) {
            // Si la inicialización fue exitosa, usar la API del framebuffer
            if let Some(mut fb) = get_framebuffer() {
                // Limpiar pantalla con negro
                fb.clear_screen(crate::drivers::framebuffer::Color::BLACK);
                
                // Llamar a la función principal del kernel
                crate::main_simple::kernel_main(&mut fb);
            } else {
                // Fallback: escribir directamente al framebuffer
                if info.base_address != 0 {
                    let fb_ptr = info.base_address as *mut u32;
                    
                    // Limpiar pantalla con negro
                    for y in 0..info.height {
                        for x in 0..info.width {
                            let offset = (y * info.pixels_per_scan_line + x) as isize;
                            core::ptr::write_volatile(fb_ptr.add(offset as usize), 0x00000000); // Negro
                        }
                    }
                    
                    // Dibujar un mensaje simple
                    for y in 100..120 {
                        for x in 100..300 {
                            if y < info.height && x < info.width {
                                let offset = (y * info.pixels_per_scan_line + x) as isize;
                                core::ptr::write_volatile(fb_ptr.add(offset as usize), 0x00FFFFFF); // Blanco
                            }
                        }
                    }
                }
            }
        } else {
            // Si falla la inicialización, escribir directamente al framebuffer
            if info.base_address != 0 {
                let fb_ptr = info.base_address as *mut u32;
                
                // Limpiar pantalla con azul oscuro
                for y in 0..info.height {
                    for x in 0..info.width {
                        let offset = (y * info.pixels_per_scan_line + x) as isize;
                        core::ptr::write_volatile(fb_ptr.add(offset as usize), 0x00000080); // Azul oscuro
                    }
                }
                
                // Dibujar mensaje de error
                for y in 200..220 {
                    for x in 200..400 {
                        if y < info.height && x < info.width {
                            let offset = (y * info.pixels_per_scan_line + x) as isize;
                            core::ptr::write_volatile(fb_ptr.add(offset as usize), 0x00FF0000); // Rojo
                        }
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