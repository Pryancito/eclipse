//! Punto de entrada simple para Eclipse OS Kernel
//! 
//! Este archivo proporciona un punto de entrada básico para el kernel
//! que muestra "Eclipse OS" centrado en pantalla negra.

#![no_std]
#![no_main]

use core::panic::PanicInfo;
use eclipse_kernel::main_simple::kernel_main;
use eclipse_kernel::drivers::framebuffer::{
    FramebufferInfo, FramebufferDriver, Color,
    get_framebuffer, init_framebuffer
};
use eclipse_kernel::syscalls::handler::init_syscall_system;
use eclipse_kernel::interrupts::manager::initialize_interrupt_system;
// panic_handler definido en lib.rs

// Nota PVH para QEMU
#[link_section = ".note"]
#[no_mangle]
pub static PVH_NOTE: [u8; 24] = [
    // Elf64_Nhdr
    4, 0, 0, 0,           // n_namesz = 4
    8, 0, 0, 0,           // n_descsz = 8  
    9, 0, 0, 0,           // n_type = NT_PVH
    // name = "Xen\0"
    0x58, 0x65, 0x6e, 0x00,
    // desc = entry point
    0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x200000
];

/// Función principal del kernel (llamada desde start.asm)
#[no_mangle]
pub extern "C" fn multiboot2_entry(framebuffer_info_ptr: *const FramebufferInfo) -> ! {
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

        // Inicializar sistema de syscalls
        let _syscall_handler = init_syscall_system();

        // Inicializar sistema de interrupciones
        let kernel_code_selector = 0x08; // Selector de código del kernel
        if let Err(e) = initialize_interrupt_system(kernel_code_selector) {
            panic!("Error al inicializar sistema de interrupciones: {}", e);
        }

        // Obtener el framebuffer mutable
        if let Some(fb) = get_framebuffer() {
            kernel_main(fb);
        } else {
            panic!("No se pudo obtener el framebuffer");
        }

        // El kernel nunca debería llegar aquí, pero por seguridad
        loop {
            core::hint::spin_loop();
        }
    }
}