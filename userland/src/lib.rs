//! # Eclipse OS Userland en Rust

#![no_std]

pub mod drm_display;
pub mod framebuffer_display;
pub mod wayland_integration;

use anyhow::Result;

// Allocador global simple
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;

struct SimpleAllocator;

unsafe impl GlobalAlloc for SimpleAllocator {
    unsafe fn alloc(&self, _layout: Layout) -> *mut u8 {
        null_mut()
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // No-op
    }
}

#[global_allocator]
static ALLOCATOR: SimpleAllocator = SimpleAllocator;

pub fn initialize() -> Result<()> {
    log::info!("Userland inicializado");
    Ok(())
}

pub fn execute_command(command: &str) -> Result<()> {
    // TODO: Implementar ejecución de comandos
    log::info!("Ejecutando comando: {}", command);
    Ok(())
}

pub fn get_prompt() -> String {
    "eclipse$ ".to_string()
}
