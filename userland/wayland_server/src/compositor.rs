//! Compositor - Manages composition and rendering of surfaces

use crate::objects::Surface;
use crate::server::MAX_CLIENTS;
use heapless::Vec;

/// Maximum surfaces
pub const MAX_SURFACES: usize = 64;

/// Compositor manages all surfaces and rendering
pub struct Compositor {
    pub surfaces: Vec<Surface, MAX_SURFACES>,
    pub frame_count: u64,
}

impl Compositor {
    pub fn new() -> Self {
        Self {
            surfaces: Vec::new(),
            frame_count: 0,
        }
    }

    /// Add a new surface
    pub fn add_surface(&mut self, surface: Surface) -> Result<(), &'static str> {
        self.surfaces.push(surface).map_err(|_| "Too many surfaces")
    }

    /// Get surface by ID
    pub fn get_surface(&self, id: u32) -> Option<&Surface> {
        self.surfaces.iter().find(|s| s.id == id)
    }

    /// Get mutable surface by ID
    pub fn get_surface_mut(&mut self, id: u32) -> Option<&mut Surface> {
        self.surfaces.iter_mut().find(|s| s.id == id)
    }

    /// Remove surface
    pub fn remove_surface(&mut self, id: u32) -> bool {
        if let Some(pos) = self.surfaces.iter().position(|s| s.id == id) {
            self.surfaces.swap_remove(pos);
            true
        } else {
            false
        }
    }

    /// Render all surfaces
    pub fn render(&mut self) -> Result<(), &'static str> {
        // In a real implementation, this would:
        // 1. Composite all surfaces based on z-order
        // 2. Apply effects
        // 3. Write to framebuffer
        // 4. Handle damage regions

        self.frame_count += 1;
        Ok(())
    }

    /// Get frame count
    pub fn get_frame_count(&self) -> u64 {
        self.frame_count
    }
}
