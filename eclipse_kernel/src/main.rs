//! Punto de entrada principal del kernel Eclipse OS

#![no_std]
#![no_main]

// use core::panic::PanicInfo;
use eclipse_kernel::main_simple::kernel_main;
use eclipse_kernel::uefi_framebuffer::init_framebuffer_from_bootloader;

// Serial COM1 para logs tempranos
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
    for &c in s.as_bytes() { serial_write_byte(c); }
}

// Estructura para recibir información del framebuffer del bootloader UEFI
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

/// Punto de entrada principal del kernel (con parámetros del framebuffer)
#[no_mangle]
pub extern "C" fn _start(framebuffer_info: *const FramebufferInfo) -> ! {
    // Serial temprano
    unsafe {
        serial_init();
        serial_write_str("KERNEL: _start\r\n");
    }
    // DEBUG: Escribir inmediatamente a VGA para confirmar que llegamos aquí
    unsafe {
        use eclipse_kernel::main_simple::{VGA, Color};
        VGA.init_vga_mode();
        VGA.set_color(Color::Red, Color::Black);
        VGA.write_string("DEBUG: _start() llamado!\n");
        VGA.set_color(Color::White, Color::Black);
    }
    
    // Si tenemos información del framebuffer, usarla
    if !framebuffer_info.is_null() {
        unsafe {
            let fb_info = &*framebuffer_info;
            
            // Inicializar framebuffer con parámetros del bootloader UEFI
            if let Err(e) = init_framebuffer_from_bootloader(
                fb_info.base_address,
                fb_info.width,
                fb_info.height,
                fb_info.pixels_per_scan_line,
                fb_info.pixel_format,
                fb_info.red_mask | fb_info.green_mask | fb_info.blue_mask
            ) {
                // Si falla la inicialización del framebuffer, continuar sin framebuffer
                // El kernel_main() manejará el fallback a VGA
            }
        }
    }
    
    // Continuar con la inicialización del kernel
    kernel_main()
}

/// Punto de entrada UEFI con parámetros del framebuffer
#[no_mangle]
pub extern "C" fn uefi_entry_with_framebuffer(
    base_address: u64,
    width: u64,
    height: u64,
    pixels_per_scan_line: u64
) -> ! {
    // Inicializar framebuffer con parámetros del bootloader UEFI
    if let Err(_e) = init_framebuffer_from_bootloader(
        base_address,
        width as u32,
        height as u32,
        pixels_per_scan_line as u32,
        0, // pixel_format (RGB888)
        0  // pixel_bitmask
    ) {
        // Si falla la inicialización del framebuffer, usar VGA como fallback
        kernel_main();
    }
    
    // Continuar con la inicialización del kernel
    kernel_main();
}
