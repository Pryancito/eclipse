//! COSMIC Desktop Environment Client
//!
//! Complete desktop environment implementation for Eclipse OS

#![no_std]

extern crate alloc;

pub mod wayland_client;
pub mod panel;
pub mod launcher;
pub mod window_manager;
pub mod mem;

pub use wayland_client::*;
pub use panel::*;
pub use launcher::*;
pub use window_manager::*;
