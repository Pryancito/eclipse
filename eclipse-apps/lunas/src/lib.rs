//! Lunas — Desktop Environment for Eclipse OS.
//!
//! Modern desktop environment built on top of SideWind, providing:
//! - Window management with animations and tiling
//! - Input handling (keyboard, mouse)
//! - IPC communication with system services
//! - Desktop shell (taskbar, app launcher, wallpaper, notifications)
//! - DRM/KMS rendering pipeline


extern crate alloc;
pub use libc;

pub mod backend;
pub mod compositor;
pub mod config;
pub mod desktop;
pub mod display;
pub mod input;
pub mod ipc;
pub mod menu;
pub mod painter;
pub mod render;
pub mod state;
pub mod assets;
pub mod style_engine;
pub mod protocol;
pub mod switcher;
pub mod widgets;
pub mod wayland_socket;
pub mod window_rules;
pub mod xwayland;

pub mod getrandom_shim {
    use eclipse_syscall::syscall3;
    use eclipse_syscall::SYS_GETRANDOM;

    #[no_mangle]
    pub unsafe extern "C" fn getrandom(buf: *mut u8, buflen: usize, flags: u32) -> isize {
        syscall3(SYS_GETRANDOM, buf as usize, buflen, flags as usize) as isize
    }
}
