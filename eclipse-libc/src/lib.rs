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
pub use header::stdio::*;
pub use header::stdlib::*;
pub use header::string::*;
pub use header::pthread::*;
pub use header::unistd::*;
pub use header::time::*;
pub use header::errno::*;
pub use header::signal::*;

#[cfg(all(not(test), feature = "allocator"))]
#[global_allocator]
static ALLOCATOR: alloc::Allocator = alloc::Allocator;

#[cfg(all(not(test), feature = "panic_handler"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe {
        eclipse_syscall::call::exit(1);
    }
}

