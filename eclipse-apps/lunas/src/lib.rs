//! Lunas — Desktop Environment for Eclipse OS.
//!
//! Modern desktop environment built on top of SideWind, providing:
//! - Window management with animations and tiling
//! - Input handling (keyboard, mouse)
//! - IPC communication with system services
//! - Desktop shell (taskbar, app launcher, wallpaper, notifications)
//! - DRM/KMS rendering pipeline

#![cfg_attr(target_vendor = "eclipse", no_std)]

#[cfg(target_vendor = "eclipse")]
extern crate alloc;
#[cfg(not(target_vendor = "eclipse"))]
extern crate alloc;
#[cfg(target_vendor = "eclipse")]
pub extern crate eclipse_std as std;
#[cfg(target_vendor = "eclipse")]
pub use libc;

pub mod backend;
pub mod compositor;
pub mod desktop;
pub mod display;
pub mod input;
pub mod ipc;
pub mod painter;
pub mod render;
pub mod state;
pub mod style_engine;
pub mod wayland;
pub mod widgets;

#[cfg(target_vendor = "eclipse")]
pub mod getrandom_shim {
    use eclipse_syscall::syscall3;
    use eclipse_syscall::SYS_GETRANDOM;

    #[no_mangle]
    pub unsafe extern "C" fn getrandom(buf: *mut u8, buflen: usize, flags: u32) -> isize {
        syscall3(SYS_GETRANDOM, buf as usize, buflen, flags as usize) as isize
    }
}
