//! Wayland Server for Eclipse OS
//!
//! A complete Wayland compositor implementation in Rust for userland

#![no_std]

extern crate alloc;

pub mod protocol;
pub mod server;
pub mod objects;
pub mod socket;
pub mod compositor;

pub use protocol::*;
pub use server::*;
pub use objects::*;
pub use socket::*;
pub use compositor::*;
