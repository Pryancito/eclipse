//! Lunas — Entorno de Escritorio para Eclipse OS.
//!
//! Backend dual: Eclipse nativo (DRM + SideWind + IPC) / Linux host.
//! Wayland y XWayland integrados como backends de cliente.

#![cfg_attr(target_vendor = "eclipse", no_std)]

#[cfg(target_vendor = "eclipse")]
extern crate alloc;
// Hacer que las rutas `alloc::` funcionen en targets std también.
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
pub mod render;
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
