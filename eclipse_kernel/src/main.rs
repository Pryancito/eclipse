//! Punto de entrada principal del binario del kernel Eclipse OS

#![no_std]
#![no_main]

extern crate alloc;
use eclipse_kernel::{
    drivers::framebuffer::{
        get_framebuffer, init_framebuffer, Color, FramebufferDriver, FramebufferInfo,
    },
    main_simple::kernel_main,
    debug::serial_write_str,
};
use core::panic::PanicInfo;

// --- Funciones de depuración serie movidas a debug.rs ---

/*
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    if let Some(mut fb) = get_framebuffer() {
        fb.clear_screen(Color::RED);
        fb.write_text_kernel("KERNEL PANIC", Color::WHITE);
        if let Some(location) = info.location() {
            let msg = alloc::format!(
                "Panic in {}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            );
            fb.write_text_kernel(&msg, Color::WHITE);
        }
    }
    loop {}
}
*/

/// Punto de entrada del kernel, llamado desde el bootloader.
#[no_mangle]
#[link_section = ".init"]
pub extern "C" fn _start(framebuffer_info_ptr: u64) -> ! {
    // Inicializar puerto serie para logs tempranos
    // (No se puede inicializar de verdad sin más código, pero podemos escribir directamente)
    serial_write_str("KERNEL: _start entry\n");

    // Configurar SSE/MMX de manera segura
    serial_write_str("KERNEL: Configuring SSE/MMX...\n");
    unsafe {
        core::arch::asm!(
            "mov rax, cr0",
            "and ax, 0xFFFB", // Clear EM bit
            "or ax, 0x2",     // Set MP bit
            "mov cr0, rax",
            "mov rax, cr4",
            "or ax, 0x600",   // Set OSFXSR and OSXMMEXCPT bits
            "mov cr4, rax",
            out("rax") _,
            options(nostack, preserves_flags)
        );
    }
    serial_write_str("KERNEL: SSE/MMX configured.\n");
    
    if framebuffer_info_ptr != 0 {
        serial_write_str("KERNEL: Framebuffer info found. Initializing...\n");
        unsafe {
            let fb_info = core::ptr::read_volatile(framebuffer_info_ptr as *const FramebufferInfo);
            match init_framebuffer(
                fb_info.base_address,
                fb_info.width,
                fb_info.height,
                fb_info.pixels_per_scan_line,
                fb_info.pixel_format,
                fb_info.red_mask | fb_info.green_mask | fb_info.blue_mask,
            ) {
                Ok(()) => {
                    serial_write_str("KERNEL: Framebuffer initialized successfully.\n");
                }
                Err(e) => {
                    serial_write_str(&alloc::format!("KERNEL: ERROR - Framebuffer initialization failed: {}\n", e));
                }
            }
        }
    } else {
        serial_write_str("KERNEL: WARNING - No framebuffer info received.\n");
    }

    serial_write_str("KERNEL: Calling kernel_main_wrapper...\n");
    kernel_main_wrapper();
}

/// Wrapper para llamar a kernel_main con el framebuffer.
fn kernel_main_wrapper() -> ! {
    serial_write_str("KERNEL: kernel_main_wrapper called.\n");
    
    if let Some(fb) = get_framebuffer() {
        serial_write_str("KERNEL: Framebuffer available, calling kernel_main.\n");
        kernel_main(fb);
    } else {
        serial_write_str("KERNEL: ERROR - No framebuffer available, cannot proceed.\n");
        // Crear un framebuffer de emergencia o continuar sin él
        // Por ahora, entramos en un bucle infinito
    }
    
    // Si kernel_main retorna (no debería), entramos en un bucle infinito.
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}
