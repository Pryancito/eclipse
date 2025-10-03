//! Módulo Wayland para Eclipse OS
//!
//! Este módulo implementa el protocolo Wayland para proporcionar
//! un sistema de ventanas moderno y eficiente.

pub mod advanced_protocols;
pub mod apps;
pub mod buffer;
pub mod client;
pub mod client_api;
pub mod compositor;
pub mod display;
pub mod display_driver;
pub mod drm;
pub mod egl;
pub mod example;
pub mod input;
pub mod output;
pub mod protocol;
pub mod rendering;
pub mod server;
pub mod shell;
pub mod shm;
pub mod surface;

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

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
    unsafe { WAYLAND_STATE.initialize() }
}

/// Verificar si Wayland está inicializado
pub fn is_wayland_initialized() -> bool {
    unsafe { WAYLAND_STATE.is_initialized.load(Ordering::Acquire) }
}

/// Obtener estado de Wayland
pub fn get_wayland_state() -> &'static WaylandState {
    unsafe { &WAYLAND_STATE }
}

/// Sistema de protocolos Wayland avanzados global
use advanced_protocols::AdvancedWaylandProtocols;

/// Instancia global del sistema avanzado
pub static mut ADVANCED_WAYLAND: Option<AdvancedWaylandProtocols> = None;

/// Inicializar sistema Wayland avanzado
pub fn init_advanced_wayland() -> Result<(), String> {
    unsafe {
        ADVANCED_WAYLAND = Some(AdvancedWaylandProtocols::new());
        if let Some(ref mut system) = ADVANCED_WAYLAND {
            system.initialize()?;
        }
    }
    Ok(())
}

/// Obtener sistema Wayland avanzado
pub fn get_advanced_wayland() -> Option<&'static mut AdvancedWaylandProtocols> {
    unsafe { ADVANCED_WAYLAND.as_mut() }
}

/// Verificar si el sistema avanzado está inicializado
pub fn is_advanced_wayland_initialized() -> bool {
    unsafe { ADVANCED_WAYLAND.is_some() }
}
