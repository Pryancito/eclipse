#![cfg_attr(target_vendor = "eclipse", no_std)]

#[cfg(target_vendor = "eclipse")]
extern crate alloc;
#[cfg(target_vendor = "eclipse")]
pub extern crate eclipse_std as std;
#[cfg(target_vendor = "eclipse")]
pub use libc;

pub mod backend;
pub mod compositor;
pub mod input;
pub mod ipc;
pub mod render;
pub mod display;
#[cfg(not(target_vendor = "eclipse"))]
pub mod smithay_wayland;
pub mod state;

#[cfg(target_vendor = "eclipse")]
pub mod getrandom_shim {
    use eclipse_syscall::syscall3;
    use eclipse_syscall::SYS_GETRANDOM;

    #[no_mangle]
    pub unsafe extern "C" fn getrandom(buf: *mut u8, buflen: usize, flags: u32) -> isize {
        syscall3(SYS_GETRANDOM, buf as usize, buflen, flags as usize) as isize
    }
}
