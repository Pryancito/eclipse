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
    let mut cycle_count = 0;
    loop {
        cycle_count += 1;
        if cycle_count % 1000000 == 0 {
            // Mostrar progreso
        }
        unsafe { core::arch::asm!("hlt"); }
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
