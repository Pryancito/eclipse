#![no_std]
#![no_main]

// Usar el crate de librería para acceder a `main_simple`
extern crate eclipse_kernel;

// Allocator global mínimo (stub) para satisfacer `alloc` si algún módulo lo requiere
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;

struct SimpleAllocator;

unsafe impl GlobalAlloc for SimpleAllocator {
    unsafe fn alloc(&self, _layout: Layout) -> *mut u8 { null_mut() }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static ALLOCATOR: SimpleAllocator = SimpleAllocator;

// Punto de entrada del kernel simplificado
#[repr(C)]
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

#[no_mangle]
pub extern "C" fn _start(fb_ptr: *const FramebufferInfo) -> ! {
    // Salida serie temprana (COM1) para confirmar control
    unsafe {
        unsafe fn outb(port: u16, value: u8) {
            core::arch::asm!(
                "out dx, al",
                in("dx") port,
                in("al") value,
                options(nomem, nostack, preserves_flags)
            );
        }
        unsafe fn inb(port: u16) -> u8 {
            let mut val: u8;
            core::arch::asm!(
                "in al, dx",
                in("dx") port,
                out("al") val,
                options(nomem, nostack, preserves_flags)
            );
            val
        }
        fn serial_init() {
            let base: u16 = 0x3F8; // COM1
            unsafe {
                outb(base + 1, 0x00);
                outb(base + 3, 0x80);
                outb(base + 0, 0x01);
                outb(base + 1, 0x00);
                outb(base + 3, 0x03);
                outb(base + 2, 0xC7);
                outb(base + 4, 0x0B);
            }
        }
        fn serial_write_byte(b: u8) {
            let base: u16 = 0x3F8;
            loop {
                let lsr = unsafe { inb(base + 5) };
                if (lsr & 0x20) != 0 { break; }
            }
            unsafe { outb(base, b); }
        }
        fn serial_write_str(s: &str) { for &c in s.as_bytes() { serial_write_byte(c); } }

        serial_init();
        serial_write_str("KRN: inicio\r\n");
    }
    // Si recibimos framebuffer GOP válido, pintar para confirmar ejecución
    unsafe {
        if !fb_ptr.is_null() {
            let fb = &*fb_ptr;
            if fb.base_address != 0 && fb.width > 0 && fb.height > 0 {
                let width = fb.width as usize;
                let height = fb.height as usize;
                let stride = fb.pixels_per_scan_line as usize;
                let base = fb.base_address as *mut u8;
                // Color verde
                let (r, g, b) = (0u32, 180u32, 0u32);
                // Precalcular desplazamientos a partir de máscaras
                let r_shift = fb.red_mask.trailing_zeros();
                let g_shift = fb.green_mask.trailing_zeros();
                let b_shift = fb.blue_mask.trailing_zeros();
                for y in 0..height {
                    for x in 0..width {
                        let off = ((y * stride) + x) * 4;
                        let p = base.add(off);
                        match fb.pixel_format {
                            0 => { // PixelFormat::Rgb
                                core::ptr::write_volatile(p.add(0), r as u8);
                                core::ptr::write_volatile(p.add(1), g as u8);
                                core::ptr::write_volatile(p.add(2), b as u8);
                                core::ptr::write_volatile(p.add(3), 0);
                            }
                            1 => { // PixelFormat::Bgr
                                core::ptr::write_volatile(p.add(0), b as u8);
                                core::ptr::write_volatile(p.add(1), g as u8);
                                core::ptr::write_volatile(p.add(2), r as u8);
                                core::ptr::write_volatile(p.add(3), 0);
                            }
                            2 => { // PixelFormat::Bitmask
                                let mut pixel: u32 = 0;
                                pixel |= ((r & 0xFF) << r_shift) & fb.red_mask;
                                pixel |= ((g & 0xFF) << g_shift) & fb.green_mask;
                                pixel |= ((b & 0xFF) << b_shift) & fb.blue_mask;
                                core::ptr::write_volatile(p as *mut u32, pixel);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    eclipse_kernel::main_simple::kernel_main();
}
