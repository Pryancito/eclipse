#![cfg_attr(not(test), no_std)]

extern crate alloc;
extern crate eclipse_syscall;

pub mod compositor;
pub mod render;
pub mod input;
pub mod ipc;
pub mod space;
pub mod backend;
pub mod state;

pub use state::SmithayState;
pub use compositor::{ShellWindow, WindowContent};
