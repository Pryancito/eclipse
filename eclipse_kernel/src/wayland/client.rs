//! Cliente Wayland para Eclipse OS
//! 
//! Implementa la gestión de clientes Wayland.

use super::protocol::*;
use alloc::vec::Vec;

/// Cliente Wayland
pub struct WaylandClient {
    pub id: ObjectId,
    pub display_fd: i32,
    pub is_connected: bool,
    pub surfaces: Vec<ObjectId>,
}

impl WaylandClient {
    pub fn new(id: ObjectId, display_fd: i32) -> Self {
        Self {
            id,
            display_fd,
            is_connected: true,
            surfaces: Vec::new(),
        }
    }
    
    /// Desconectar cliente
    pub fn disconnect(&mut self) {
        self.is_connected = false;
    }
    
    /// Agregar superficie
    pub fn add_surface(&mut self, surface_id: ObjectId) {
        self.surfaces.push(surface_id);
    }
    
    /// Remover superficie
    pub fn remove_surface(&mut self, surface_id: ObjectId) {
        self.surfaces.retain(|&id| id != surface_id);
    }
}
