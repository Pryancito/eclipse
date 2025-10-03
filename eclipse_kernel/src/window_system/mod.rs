//! Sistema de Ventanas Eclipse OS
//!
//! Implementa un sistema de ventanas similar a X11/Wayland para Eclipse OS
//! con gestión completa de ventanas, eventos y composición.

pub mod client_api;
pub mod compositor;
pub mod event_system;
pub mod geometry;
pub mod protocol;
pub mod window;
pub mod window_manager;

use alloc::collections::BTreeMap;
use alloc::string::String;
use core::sync::atomic::{AtomicU32, Ordering};

/// ID único para ventanas
pub type WindowId = u32;

/// ID único para clientes
pub type ClientId = u32;

/// Estado global del sistema de ventanas
pub struct WindowSystem {
    /// Gestor de ventanas
    pub window_manager: window_manager::WindowManager,
    /// Sistema de eventos
    pub event_system: event_system::EventSystem,
    /// API para clientes
    pub client_api: client_api::ClientAPI,
    /// Compositor de ventanas
    pub compositor: compositor::WindowCompositor,
    /// Próximo ID de ventana disponible
    next_window_id: AtomicU32,
    /// Próximo ID de cliente disponible
    next_client_id: AtomicU32,
}

impl WindowSystem {
    /// Crear nuevo sistema de ventanas
    pub fn new() -> Result<Self, &'static str> {
        let window_manager = window_manager::WindowManager::new()?;
        let event_system = event_system::EventSystem::new()?;
        let client_api = client_api::ClientAPI::new()?;
        let compositor = compositor::WindowCompositor::new()?;

        Ok(Self {
            window_manager,
            event_system,
            client_api,
            compositor,
            next_window_id: AtomicU32::new(1),
            next_client_id: AtomicU32::new(1),
        })
    }

    /// Generar nuevo ID de ventana
    pub fn generate_window_id(&self) -> WindowId {
        self.next_window_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Generar nuevo ID de cliente
    pub fn generate_client_id(&self) -> ClientId {
        self.next_client_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Inicializar el sistema de ventanas
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        self.window_manager.initialize()?;
        self.event_system.initialize()?;
        self.client_api.initialize()?;
        self.compositor.initialize()?;
        Ok(())
    }

    /// Procesar un frame de renderizado
    pub fn render_frame(&mut self) -> Result<(), &'static str> {
        self.compositor.compose_frame()?;
        Ok(())
    }

    /// Manejar eventos del sistema
    pub fn handle_events(&mut self) -> Result<(), &'static str> {
        self.event_system.process_events()?;
        Ok(())
    }

    /// Obtener estadísticas del sistema
    pub fn get_stats(&self) -> WindowSystemStats {
        WindowSystemStats {
            window_count: self.window_manager.get_window_count(),
            client_count: self.client_api.get_client_count(),
            frame_rate: self.compositor.get_frame_rate(),
            event_queue_size: self.event_system.get_queue_size(),
        }
    }
}

/// Estadísticas del sistema de ventanas
#[derive(Debug, Clone)]
pub struct WindowSystemStats {
    pub window_count: u32,
    pub client_count: u32,
    pub frame_rate: f32,
    pub event_queue_size: usize,
}

/// Instancia global del sistema de ventanas
static mut WINDOW_SYSTEM: Option<WindowSystem> = None;

/// Inicializar el sistema de ventanas global
pub fn init_window_system() -> Result<(), &'static str> {
    unsafe {
        if WINDOW_SYSTEM.is_some() {
            return Err("Sistema de ventanas ya inicializado");
        }

        let mut system = WindowSystem::new()?;
        system.initialize()?;
        WINDOW_SYSTEM = Some(system);
    }
    Ok(())
}

/// Obtener referencia al sistema de ventanas
pub fn get_window_system() -> Result<&'static mut WindowSystem, &'static str> {
    unsafe {
        WINDOW_SYSTEM
            .as_mut()
            .ok_or("Sistema de ventanas no inicializado")
    }
}

/// Verificar si el sistema de ventanas está inicializado
pub fn is_window_system_initialized() -> bool {
    unsafe { WINDOW_SYSTEM.is_some() }
}

/// Renderizar un frame del sistema de ventanas
pub fn render_window_system_frame() -> Result<(), &'static str> {
    let system = get_window_system()?;
    system.render_frame()
}

/// Procesar eventos del sistema de ventanas
pub fn process_window_system_events() -> Result<(), &'static str> {
    let system = get_window_system()?;
    system.handle_events()
}

/// Obtener el gestor de ventanas global
pub fn get_window_manager() -> Result<&'static mut window_manager::WindowManager, &'static str> {
    let system = get_window_system()?;
    Ok(&mut system.window_manager)
}

/// Obtener la API de clientes global
pub fn get_client_api() -> Result<&'static mut client_api::ClientAPI, &'static str> {
    let system = get_window_system()?;
    Ok(&mut system.client_api)
}

/// Obtener el compositor global
pub fn get_compositor() -> Result<&'static mut compositor::WindowCompositor, &'static str> {
    let system = get_window_system()?;
    Ok(&mut system.compositor)
}
