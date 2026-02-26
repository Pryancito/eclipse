#![cfg_attr(not(target_env = "gnu"), no_std)]
#![cfg_attr(not(target_env = "gnu"), feature(lang_items))]

pub mod syscall;
pub mod stdio;
pub mod stdlib;

pub use syscall::*;
pub use stdio::*;
pub use stdlib::*;

#[cfg(all(not(test), not(target_env = "gnu")))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(1);
}

#[cfg(all(not(test), not(target_env = "gnu")))]
#[lang = "eh_personality"]
extern "C" fn eh_personality() {}
