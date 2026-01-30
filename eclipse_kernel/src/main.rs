//! Punto de entrada principal del binario del kernel Eclipse OS

#![no_std]
#![no_main]

extern crate alloc;
use eclipse_kernel::{
    drivers::framebuffer::{
        get_framebuffer, init_framebuffer, FramebufferInfo,
    },
    main_simple::kernel_main,
    debug::serial_write_str,
};

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

/// GDT mínima para el kernel
#[repr(C, align(16))]
struct GdtTable {
    entries: [u64; 5],
}

static mut KERNEL_GDT: GdtTable = GdtTable {
    entries: [
        0x0000000000000000, // Null descriptor
        0x00AF9A000000FFFF, // Code segment (64-bit)
        0x00CF92000000FFFF, // Data segment
        0x0000000000000000, // Reservado
        0x0000000000000000, // Reservado
    ],
};

#[repr(C, packed)]
struct GdtPointer {
    limit: u16,
    base: u64,
}

/// Punto de entrada del kernel, llamado desde el bootloader.
#[no_mangle]
#[link_section = ".init"]
pub extern "C" fn _start(framebuffer_info_ptr: u64) -> ! {
    // CRÍTICO: Cargar GDT nuevo PRIMERO antes de hacer CUALQUIER otra cosa
    // La GDT de UEFI no está mapeada en nuestras page tables
    unsafe {
        let gdt_ptr = GdtPointer {
            limit: (core::mem::size_of::<GdtTable>() - 1) as u16,
            base: &raw const KERNEL_GDT as *const _ as u64,
        };
        
        core::arch::asm!(
            "lgdt [{}]",
            in(reg) &gdt_ptr,
            options(nostack, preserves_flags)
        );
        
        // Recargar segmentos de código y datos
        core::arch::asm!(
            "push 0x08",            // Code segment selector
            "lea rax, [rip + 2f]",
            "push rax",
            "retfq",                // Far return para recargar CS
            "2:",
            "mov ax, 0x10",         // Data segment selector
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov gs, ax",
            "mov ss, ax",
            out("rax") _,
            options(nostack)
        );
    }
    
    // Ahora SÍ podemos usar serial_write_str de forma segura
    serial_write_str("KERNEL: _start entry (GDT loaded)\n");
    
    if framebuffer_info_ptr != 0 {
        serial_write_str("KERNEL: Framebuffer info found.\n");
        unsafe {
            let fb_info = core::ptr::read_volatile(framebuffer_info_ptr as *const FramebufferInfo);
            serial_write_str("KERNEL: Calling init_framebuffer...\n");
            match init_framebuffer(
                fb_info.base_address,
                fb_info.width,
                fb_info.height,
                fb_info.pixels_per_scan_line,
                fb_info.pixel_format,
                fb_info.red_mask | fb_info.green_mask | fb_info.blue_mask,
            ) {
                Ok(()) => {
                    serial_write_str("KERNEL: Framebuffer initialized OK.\n");
                }
                Err(_e) => {
                    serial_write_str("KERNEL: ERROR - Framebuffer init failed.\n");
                }
            }
        }
    } else {
        serial_write_str("KERNEL: WARNING - No framebuffer info.\n");
    }

    // NOTE: Syscall and interrupt initialization moved to kernel_main in main_simple.rs
    // because they require heap allocation which is initialized there
    
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
