//! Punto de entrada principal del kernel Eclipse OS

#![no_std]
#![no_main]

extern crate alloc;

// Importar funciones necesarias
use eclipse_kernel::main_simple::kernel_main;
use eclipse_kernel::drivers::framebuffer::{
    init_framebuffer, FramebufferInfo
};

// Usamos el panic handler definido en lib.rs
// Punto de entrada principal del kernel (con parámetros del framebuffer)
#[no_mangle]
pub extern "C" fn _start(framebuffer_info_ptr: *const FramebufferInfo) -> ! {
    unsafe {
        // Leer la información del framebuffer de manera segura
        let fb_info = core::ptr::read_volatile(framebuffer_info_ptr);
        // Inicializar el framebuffer usando la nueva API
        let _ = init_framebuffer(
            fb_info.base_address,
            fb_info.width,
            fb_info.height,
            fb_info.pixels_per_scan_line,
            fb_info.pixel_format,
            fb_info.red_mask | fb_info.green_mask | fb_info.blue_mask
        );

        // Llamar directamente a la función principal del kernel
        kernel_main();
        
        // El kernel nunca debería llegar aquí, pero por seguridad
        loop {
            core::hint::spin_loop();
        }
    }
}