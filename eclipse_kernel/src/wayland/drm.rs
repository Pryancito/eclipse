//! DRM Wayland para Eclipse OS
//!
//! Implementa la integración con DRM (Direct Rendering Manager) para Wayland.

use super::protocol::*;
use alloc::string::String;
use alloc::vec::Vec;

/// Gestor DRM para Wayland
pub struct DrmManager {
    pub devices: Vec<DrmDevice>,
    pub current_device: Option<ObjectId>,
}

impl DrmManager {
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
            current_device: None,
        }
    }

    /// Inicializar DRM
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        // En un sistema real, aquí se inicializaría DRM
        // Por ahora, simulamos la inicialización
        Ok(())
    }

    /// Agregar dispositivo DRM
    pub fn add_device(&mut self, device: DrmDevice) -> ObjectId {
        let device_id = self.devices.len() as ObjectId + 1;
        self.devices.push(device);
        device_id
    }

    /// Obtener dispositivo actual
    pub fn get_current_device(&self) -> Option<&DrmDevice> {
        if let Some(id) = self.current_device {
            self.devices.get((id - 1) as usize)
        } else {
            None
        }
    }
}

/// Dispositivo DRM
pub struct DrmDevice {
    pub id: ObjectId,
    pub name: String,
    pub capabilities: DrmCapabilities,
    pub is_active: bool,
}

impl DrmDevice {
    pub fn new(name: String) -> Self {
        Self {
            id: 0,
            name,
            capabilities: DrmCapabilities::empty(),
            is_active: false,
        }
    }
}

/// Capacidades DRM
#[derive(Debug, Clone, Copy)]
pub struct DrmCapabilities {
    pub has_gem: bool,
    pub has_prime: bool,
    pub has_async_page_flip: bool,
}

impl DrmCapabilities {
    pub fn empty() -> Self {
        Self {
            has_gem: false,
            has_prime: false,
            has_async_page_flip: false,
        }
    }
}
