//! Shell Wayland para Eclipse OS
//!
//! Implementa el shell de ventanas para Wayland.

use super::protocol::*;
use super::surface::*;
use alloc::string::String;
use alloc::vec::Vec;

/// Shell de ventanas Wayland
pub struct WaylandShell {
    pub surfaces: Vec<WaylandSurface>,
    pub next_surface_id: ObjectId,
}

impl WaylandShell {
    pub fn new() -> Self {
        Self {
            surfaces: Vec::new(),
            next_surface_id: 1,
        }
    }

    /// Crear nueva ventana
    pub fn create_window(
        &mut self,
        title: String,
        width: u32,
        height: u32,
    ) -> Result<ObjectId, &'static str> {
        let surface_id = self.next_surface_id;
        self.next_surface_id += 1;

        let mut surface = WaylandSurface::new(surface_id, 0); // 0 = server
        surface.width = width;
        surface.height = height;

        self.surfaces.push(surface);
        Ok(surface_id)
    }

    /// Destruir ventana
    pub fn destroy_window(&mut self, surface_id: ObjectId) -> Result<(), &'static str> {
        if let Some(pos) = self.surfaces.iter().position(|s| s.id == surface_id) {
            self.surfaces.remove(pos);
            Ok(())
        } else {
            Err("Window not found")
        }
    }

    /// Obtener ventana por ID
    pub fn get_window(&self, surface_id: ObjectId) -> Option<&WaylandSurface> {
        self.surfaces.iter().find(|s| s.id == surface_id)
    }

    /// Obtener ventana por ID (mutable)
    pub fn get_window_mut(&mut self, surface_id: ObjectId) -> Option<&mut WaylandSurface> {
        self.surfaces.iter_mut().find(|s| s.id == surface_id)
    }

    /// Obtener todas las ventanas
    pub fn get_all_windows(&self) -> &[WaylandSurface] {
        &self.surfaces
    }
}
