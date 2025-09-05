#![no_std]
#![no_main]

use core::panic::PanicInfo;

// Salida serie COM1 para diagnóstico
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
    for &c in s.as_bytes() { 
        serial_write_byte(c); 
    }
}

// VGA text mode output
unsafe fn vga_write_str_at(s: &str, x: usize, y: usize) {
    let vga_buffer = 0xB8000 as *mut u16;
    let mut pos = y * 80 + x;
    
    for &c in s.as_bytes() {
        if pos < 80 * 25 {
            let color = 0x0F; // White on black
            *vga_buffer.add(pos) = (c as u16) | (color << 8);
            pos += 1;
        }
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Inicializar serie
    unsafe { 
        serial_init(); 
        serial_write_str("KERNEL STARTED\r\n");
    }
    
    // Escribir en VGA
    unsafe {
        vga_write_str_at("KERNEL STARTED", 0, 0);
        vga_write_str_at("VIDEO VGA OK", 0, 1);
    }
    
    // Bucle infinito
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    unsafe {
        serial_write_str("KERNEL PANIC\r\n");
        vga_write_str_at("KERNEL PANIC", 0, 2);
    }
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}