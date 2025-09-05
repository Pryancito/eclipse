//! Eclipse Kernel - Versión Simple

#![no_std]
#![no_main]

use core::panic::PanicInfo;

struct SimpleAllocator;

unsafe impl core::alloc::GlobalAlloc for SimpleAllocator {
    unsafe fn alloc(&self, _layout: core::alloc::Layout) -> *mut u8 {
        core::ptr::null_mut()
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: core::alloc::Layout) {}
}

#[global_allocator]
static ALLOCATOR: SimpleAllocator = SimpleAllocator;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop { unsafe { core::arch::asm!("hlt"); } }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    unsafe {
        serial_init();
        serial_write_str(b"KERNEL STARTED\r\n");
        vga_write_str_at("KERNEL STARTED", 0, 0, 0x0F);
        vga_write_str_at("VIDEO VGA OK", 1, 0, 0x0A);
    }
    loop { unsafe { core::arch::asm!("hlt"); } }
}

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

// COM1 base 0x3F8
unsafe fn serial_init() {
    let base: u16 = 0x3F8;
    outb(base + 1, 0x00); // Disable all interrupts
    outb(base + 3, 0x80); // Enable DLAB
    outb(base + 0, 0x01); // Divisor low (115200 baud)
    outb(base + 1, 0x00); // Divisor high
    outb(base + 3, 0x03); // 8 bits, no parity, one stop, clear DLAB
    outb(base + 2, 0xC7); // Enable FIFO, clear, 14-byte threshold
    outb(base + 4, 0x0B); // IRQs enabled, RTS/DSR set
}

unsafe fn serial_write_byte(b: u8) {
    let base: u16 = 0x3F8;
    // Wait for Transmitter Holding Register Empty (LSR bit 5)
    while (inb(base + 5) & 0x20) == 0 {}
    outb(base, b);
}

unsafe fn serial_write_str(s: &[u8]) {
    for &c in s { serial_write_byte(c); }
}

// VGA texto modo 80x25 en 0xB8000, atributo alto byte
unsafe fn vga_write_str_at(s: &str, row: usize, col: usize, attr: u8) {
    let base = 0xB8000 as *mut u16;
    let mut off = row.saturating_mul(80).saturating_add(col);
    for b in s.bytes() {
        let val: u16 = ((attr as u16) << 8) | (b as u16);
        core::ptr::write_volatile(base.add(off), val);
        off = off.saturating_add(1);
    }
}

#[no_mangle]
pub extern "C" fn memcpy(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    unsafe {
        for i in 0..n {
            *dest.add(i) = *src.add(i);
        }
        dest
    }
}

#[no_mangle]
pub extern "C" fn memset(s: *mut u8, c: i32, n: usize) -> *mut u8 {
    unsafe {
        for i in 0..n {
            *s.add(i) = c as u8;
        }
        s
    }
}

#[no_mangle]
pub extern "C" fn memcmp(s1: *const u8, s2: *const u8, n: usize) -> i32 {
    unsafe {
        for i in 0..n {
            let a = *s1.add(i);
            let b = *s2.add(i);
            if a != b { return (a as i32) - (b as i32); }
        }
        0
    }
}

#[no_mangle]
pub extern "C" fn rust_eh_personality() -> i32 { 0 }
