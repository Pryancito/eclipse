pub use libc;

pub mod backend;
pub mod compositor;
pub mod input;
pub mod ipc;
pub mod render;
pub mod display;
#[cfg(all(not(target_os = "eclipse"), feature = "wayland"))]
pub mod smithay_wayland;
pub mod state;
pub mod painter;
pub mod style_engine;
pub mod stylus;
pub mod protocol;
pub mod xwayland;
pub mod wayland_socket;

#[cfg(target_os = "eclipse")]
pub mod getrandom_shim {
    use eclipse_syscall::syscall3;
    use eclipse_syscall::SYS_GETRANDOM;

    #[no_mangle]
    pub unsafe extern "C" fn getrandom(buf: *mut u8, buflen: usize, flags: u32) -> isize {
        syscall3(SYS_GETRANDOM, buf as usize, buflen, flags as usize) as isize
    }
}
