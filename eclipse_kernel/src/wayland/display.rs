//! Display Wayland para Eclipse OS
//!
//! Implementa la gestión del display principal de Wayland.

use super::compositor::*;
use super::input::*;
use super::output::*;
use super::protocol::*;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

/// Display Wayland principal
pub struct WaylandDisplay {
    pub is_running: AtomicBool,
    pub display_fd: i32,
    pub compositor: WaylandCompositor,
    pub output_manager: OutputManager,
    pub input_manager: InputManager,
    pub clients: Vec<WaylandClient>,
}

impl WaylandDisplay {
    pub fn new() -> Self {
        Self {
            is_running: AtomicBool::new(false),
            display_fd: -1,
            compositor: WaylandCompositor::new(),
            output_manager: OutputManager::new(),
            input_manager: InputManager::new(),
            clients: Vec::new(),
        }
    }

    /// Inicializar display
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        // Inicializar compositor
        self.compositor.initialize()?;

        // Crear output principal
        let main_output = WaylandOutput::new(1920, 1080, 60);
        self.output_manager.add_output(main_output);

        // Crear dispositivos de entrada
        let keyboard = WaylandInputDevice::new(InputDeviceType::Keyboard);
        let mouse = WaylandInputDevice::new(InputDeviceType::Mouse);
        let touch = WaylandInputDevice::new(InputDeviceType::Touch);

        self.input_manager.add_device(keyboard);
        self.input_manager.add_device(mouse);
        self.input_manager.add_device(touch);

        self.is_running.store(true, Ordering::Release);
        Ok(())
    }

    /// Agregar cliente
    pub fn add_client(&mut self, display_fd: i32) -> ObjectId {
        let client_id = self.compositor.server.add_client(display_fd);
        self.clients.push(WaylandClient::new(client_id, display_fd));
        client_id
    }

    /// Remover cliente
    pub fn remove_client(&mut self, client_id: ObjectId) {
        self.compositor.server.remove_client(client_id);
        self.clients.retain(|c| c.id != client_id);
    }

    /// Procesar eventos
    pub fn process_events(&mut self) -> Result<(), &'static str> {
        if !self.is_running.load(Ordering::Acquire) {
            return Err("Display not running");
        }

        // Procesar eventos de entrada
        // En un sistema real, aquí se leerían los eventos del display

        // Renderizar frame
        self.compositor.render_frame()?;

        Ok(())
    }

    /// Obtener estadísticas del display
    pub fn get_stats(&self) -> DisplayStats {
        DisplayStats {
            is_running: self.is_running.load(Ordering::Acquire),
            client_count: self.clients.len(),
            surface_count: self.compositor.surfaces.len(),
            output_count: self.output_manager.outputs.len(),
            input_device_count: self.input_manager.devices.len(),
        }
    }
}

/// Estadísticas del display
#[derive(Debug, Clone)]
pub struct DisplayStats {
    pub is_running: bool,
    pub client_count: usize,
    pub surface_count: usize,
    pub output_count: usize,
    pub input_device_count: usize,
}
