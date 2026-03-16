#![cfg_attr(not(target_os = "linux"), no_std)]

#[cfg(not(target_os = "linux"))]
extern crate alloc;
#[cfg(not(target_os = "linux"))]
pub extern crate eclipse_std as std;
#[cfg(not(target_os = "linux"))]
pub use libc;

pub mod backend;
pub mod compositor;
pub mod input;
pub mod ipc;
pub mod render;
#[cfg(target_os = "linux")]
pub mod smithay_wayland;
pub mod state;

#[cfg(not(target_os = "linux"))]
pub mod getrandom_shim {
    use eclipse_syscall::syscall3;
    use eclipse_syscall::SYS_GETRANDOM;

    #[no_mangle]
    pub unsafe extern "C" fn getrandom(buf: *mut u8, buflen: usize, flags: u32) -> isize {
        syscall3(SYS_GETRANDOM, buf as usize, buflen, flags as usize) as isize
    }
}
