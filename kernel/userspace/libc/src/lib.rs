//! Eclipse OS Libc - Biblioteca estándar mínima para userspace
#![no_std]
#![feature(lang_items)]

pub mod syscall;
pub mod stdio;
pub mod stdlib;

pub use syscall::*;
pub use stdio::*;
pub use stdlib::*;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(1);
}

#[lang = "eh_personality"]
extern "C" fn eh_personality() {}
