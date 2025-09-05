#![no_std]
#![no_main]

// Usar la versión mejorada del kernel
mod main_improved;
mod shell;
mod display;

// Re-exportar la función _start del kernel mejorado
#[no_mangle]
pub extern "C" fn _start(
    framebuffer_base: u64,
    framebuffer_width: u32,
    framebuffer_height: u32,
    framebuffer_pixels_per_scan_line: u32,
    framebuffer_pixel_format: u32,
) -> ! {
    main_improved::kernel_main(
        framebuffer_base,
        framebuffer_width,
        framebuffer_height,
        framebuffer_pixels_per_scan_line,
        framebuffer_pixel_format,
    )
}