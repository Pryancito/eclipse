//! Módulo Wayland para Eclipse OS
//! 
//! Este módulo implementa el protocolo Wayland para proporcionar
//! un sistema de ventanas moderno y eficiente.

pub mod protocol;
pub mod compositor;
pub mod display;
pub mod surface;
pub mod input;
pub mod output;
pub mod shell;
pub mod client;
pub mod server;
pub mod buffer;
pub mod shm;
pub mod drm;
pub mod egl;
pub mod display_driver;

use core::sync::atomic::{AtomicBool, Ordering};
use alloc::vec::Vec;
use alloc::string::String;

/// Estado global de Wayland
pub struct WaylandState {
    pub is_initialized: AtomicBool,
    pub display_fd: i32,
    pub compositor_running: AtomicBool,
}

impl WaylandState {
    pub fn new() -> Self {
        Self {
            is_initialized: AtomicBool::new(false),
            display_fd: -1,
            compositor_running: AtomicBool::new(false),
        }
    }
    
    pub fn initialize(&self) -> Result<(), &'static str> {
        if self.is_initialized.load(Ordering::Acquire) {
            return Ok(());
        }
        
        // Inicializar display de Wayland
        self.init_display()?;
        
        // Inicializar compositor
        self.init_compositor()?;
        
        self.is_initialized.store(true, Ordering::Release);
        Ok(())
    }
    
    fn init_display(&self) -> Result<(), &'static str> {
        // En un sistema real, aquí se inicializaría el display de Wayland
        // Por ahora, simulamos la inicialización
        Ok(())
    }
    
    fn init_compositor(&self) -> Result<(), &'static str> {
        // Inicializar compositor Wayland
        self.compositor_running.store(true, Ordering::Release);
        Ok(())
    }
}

/// Instancia global de Wayland
pub static mut WAYLAND_STATE: WaylandState = WaylandState {
    is_initialized: AtomicBool::new(false),
    display_fd: -1,
    compositor_running: AtomicBool::new(false),
};

/// Inicializar sistema Wayland
pub fn init_wayland() -> Result<(), &'static str> {
    unsafe {
        WAYLAND_STATE.initialize()
    }
}

/// Verificar si Wayland está inicializado
pub fn is_wayland_initialized() -> bool {
    unsafe {
        WAYLAND_STATE.is_initialized.load(Ordering::Acquire)
    }
}

/// Obtener estado de Wayland
pub fn get_wayland_state() -> &'static WaylandState {
    unsafe {
        &WAYLAND_STATE
    }
}
