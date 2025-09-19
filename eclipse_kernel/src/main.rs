//! Punto de entrada principal del kernel Eclipse OS

#![no_std]
#![no_main]

use eclipse_kernel::main_simple::kernel_main;
use eclipse_kernel::drivers::framebuffer::{
    FramebufferInfo, FramebufferDriver, Color,
    get_framebuffer, init_framebuffer
};

// Punto de entrada principal del kernel (con parámetros del framebuffer)
#[no_mangle]
pub extern "C" fn _start(framebuffer_info_ptr: *const FramebufferInfo) -> ! {
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

        // Obtener el framebuffer mutable
        if let Some(fb) = get_framebuffer() {
            kernel_main(fb);
        }

        // El kernel nunca debería llegar aquí, pero por seguridad
        loop {
            core::hint::spin_loop();
        }
    }
}