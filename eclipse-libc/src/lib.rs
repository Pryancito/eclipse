//! Eclipse Libc - POSIX C library for Eclipse OS
#![no_std]

extern crate eclipse_syscall;

pub mod types;
pub mod alloc;
pub mod c_str;
pub mod platform;
pub mod header {
    pub mod stdio;
    pub mod stdlib;
    pub mod string;
    pub mod pthread;
    pub mod unistd;
    pub mod time;
    pub mod errno;
    pub mod signal;
}

pub use types::*;

#[cfg(not(test))]
#[global_allocator]
static ALLOCATOR: alloc::Allocator = alloc::Allocator;

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
