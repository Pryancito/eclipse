//! Punto de entrada principal del kernel Eclipse OS

#![no_std]
#![no_main]

// use core::panic::PanicInfo;
use core::error::Error;
extern crate alloc;
use alloc::boxed::Box;

// Importar funciones necesarias
use eclipse_kernel::main_simple::kernel_main;
use eclipse_kernel::allocator;

// Salida serie COM1 para diagnóstico temprano
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

// Usamos el panic handler definido en lib.rs
// Punto de entrada principal del kernel (con parámetros del framebuffer)
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // ⚠️  KERNEL ULTRA-SIMPLE PARA DIAGNOSTICAR PAGE FAULT ⚠️
    // Solo las operaciones más básicas para identificar el problema

    unsafe {
        // 1. Inicializar el allocador PRIMERO (necesario para logging)
        #[cfg(feature = "alloc")]
        {
            allocator::init_allocator();
        }
        unsafe {
            core::arch::asm!(
                "call {kernel_call}",
                kernel_call = sym kernel_call,
            );
        }

        loop {
            for _ in 0..10000 {
                core::hint::spin_loop();
            }
        }
    }
}

unsafe fn kernel_call() -> Result<(), Box<dyn Error>> {
    kernel_main()?;
    Ok(())
}