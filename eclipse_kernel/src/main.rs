//! Punto de entrada principal del kernel Eclipse OS

#![no_std]
#![no_main]

// use core::panic::PanicInfo;
use core::error::Error;
extern crate alloc;
use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;

// Importar funciones necesarias
use eclipse_kernel::main_simple::kernel_main;
use eclipse_kernel::allocator;
use eclipse_kernel::drivers::framebuffer::{init_framebuffer, FramebufferInfo};

// Estructuras para paginación x86-64
#[repr(C, align(4096))]
#[derive(Debug, Clone, Copy)]
pub struct PageTable {
    entries: [u64; 512],
}

impl PageTable {
    pub const fn new() -> Self {
        Self {
            entries: [0; 512],
        }
    }

    pub fn set_entry(&mut self, index: usize, entry: u64) {
        if index < 512 {
            self.entries[index] = entry;
        }
    }

    pub fn get_entry(&self, index: usize) -> u64 {
        if index < 512 {
            self.entries[index]
        } else {
            0
        }
    }
}

// Bits de las entradas de tabla de páginas
const PAGE_PRESENT: u64 = 1 << 0;           // Presente en memoria
const PAGE_WRITABLE: u64 = 1 << 1;          // Permiso de escritura
const PAGE_USER: u64 = 1 << 2;              // Acceso desde modo usuario
const PAGE_HUGE: u64 = 1 << 7;              // Página grande (2MB/1GB)
const PAGE_NO_EXECUTE: u64 = 1 << 63;       // No ejecutar

// Salida serie COM1 para diagnóstico temprano
// Salida serie COM1 para diagnóstico temprano
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

unsafe fn serial_write_hex32(val: u32) {
    for i in (0..8).rev() {
        let nibble = (val >> (i * 4)) & 0xF;
        let c = if nibble < 10 {
            b'0' + nibble as u8
        } else {
            b'A' + (nibble - 10) as u8
        };
        serial_write_byte(c);
    }
}

unsafe fn serial_write_hex64(val: u64) {
    for i in (0..16).rev() {
        let nibble = (val >> (i * 4)) & 0xF;
        let c = if nibble < 10 {
            b'0' + nibble as u8
        } else {
            b'A' + (nibble - 10) as u8
        };
        serial_write_byte(c);
    }
}

// Usamos el panic handler definido en lib.rs
// Punto de entrada principal del kernel (con parámetros del framebuffer)
#[no_mangle]
pub extern "C" fn _start(framebuffer_info_ptr: *const FramebufferInfo) -> ! {
    unsafe {
        // Leer la información del framebuffer de manera segura
        let fb_info = core::ptr::read_volatile(framebuffer_info_ptr);
        // Inicializar el framebuffer usando la nueva API
        init_framebuffer(
            fb_info.base_address,
            fb_info.width,
            fb_info.height,
            fb_info.pixels_per_scan_line,
            fb_info.pixel_format,
            fb_info.red_mask | fb_info.green_mask | fb_info.blue_mask
        );

        // Llamar directamente a la función principal del kernel
        kernel_main();

        loop {
            core::hint::spin_loop();
        }
    }
}