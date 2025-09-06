//! Gestor de DRM para Eclipse OS
//!
//! Este módulo gestiona la integración del sistema DRM con el kernel,
//! proporcionando una interfaz unificada para control de pantalla.

use crate::drivers::drm::{DrmDriver, DrmDriverState, DrmOperation, DrmDriverStats, create_drm_driver};
use crate::drivers::framebuffer::{FramebufferDriver, FramebufferInfo, Color};
use crate::desktop_ai::{Point, Rect};
use alloc::vec::Vec;
use alloc::string::{String, ToString};
use alloc::format;

/// Gestor de drivers DRM
#[derive(Debug, Clone)]
pub struct DrmManager {
    drm_drivers: [Option<DrmDriver>; 4],
    drm_count: usize,
    primary_drm: Option<usize>,
    is_initialized: bool,
}

impl DrmManager {
    /// Crear una nueva instancia del gestor DRM
    pub fn new() -> Self {
        Self {
            drm_drivers: [(); 4].map(|_| None),
            drm_count: 0,
            primary_drm: None,
            is_initialized: false,
        }
    }

    /// Inicializar el gestor DRM
    pub fn initialize(&mut self, framebuffer_info: Option<FramebufferInfo>) -> Result<(), &'static str> {
        if self.is_initialized {
            return Ok(());
        }

        // Crear driver DRM principal
        let mut drm_driver = create_drm_driver();
        drm_driver.initialize(framebuffer_info)?;

        // Agregar a la lista de drivers
        if self.drm_count < self.drm_drivers.len() {
            self.drm_drivers[self.drm_count] = Some(drm_driver);
            self.primary_drm = Some(self.drm_count);
            self.drm_count += 1;
        }

        self.is_initialized = true;
        Ok(())
    }

    /// Obtener driver DRM primario
    pub fn get_primary_drm(&mut self) -> Option<&mut DrmDriver> {
        if let Some(primary_index) = self.primary_drm {
            self.drm_drivers.get_mut(primary_index)?.as_mut()
        } else {
            None
        }
    }

    /// Obtener driver DRM por índice
    pub fn get_drm_driver(&mut self, index: usize) -> Option<&mut DrmDriver> {
        self.drm_drivers.get_mut(index)?.as_mut()
    }

    /// Obtener todos los drivers DRM
    pub fn get_drm_drivers(&mut self) -> &mut [Option<DrmDriver>] {
        &mut self.drm_drivers[..self.drm_count]
    }

    /// Obtener número de drivers DRM
    pub fn get_drm_count(&self) -> usize {
        self.drm_count
    }

    /// Verificar si hay drivers DRM listos
    pub fn has_ready_drivers(&self) -> bool {
        self.drm_drivers.iter()
            .filter_map(|d| d.as_ref())
            .any(|d| d.is_ready())
    }

    /// Obtener framebuffer primario
    pub fn get_primary_framebuffer(&mut self) -> Option<&mut FramebufferDriver> {
        if let Some(primary_index) = self.primary_drm {
            if let Some(drm) = self.drm_drivers.get_mut(primary_index) {
                drm.as_mut()?.get_framebuffer()
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Ejecutar operación en el driver primario
    pub fn execute_operation(&mut self, operation: DrmOperation) -> Result<(), &'static str> {
        if let Some(ref mut drm) = self.get_primary_drm() {
            drm.execute_operation(operation)
        } else {
            Err("No hay driver DRM primario disponible")
        }
    }

    /// Limpiar pantalla con color
    pub fn clear_screen(&mut self, color: Color) -> Result<(), &'static str> {
        self.execute_operation(DrmOperation::ClearScreen { color })
    }

    /// Dibujar pixel
    pub fn draw_pixel(&mut self, point: Point, color: Color) -> Result<(), &'static str> {
        self.execute_operation(DrmOperation::DrawPixel { point, color })
    }

    /// Dibujar rectángulo
    pub fn draw_rect(&mut self, rect: Rect, color: Color) -> Result<(), &'static str> {
        self.execute_operation(DrmOperation::DrawRect { rect, color })
    }

    /// Cambiar modo de pantalla
    pub fn set_mode(&mut self, width: u32, height: u32, refresh_rate: u32) -> Result<(), &'static str> {
        self.execute_operation(DrmOperation::SetMode { width, height, refresh_rate })
    }

    /// Habilitar VSync
    pub fn enable_vsync(&mut self) -> Result<(), &'static str> {
        self.execute_operation(DrmOperation::EnableVsync)
    }

    /// Deshabilitar VSync
    pub fn disable_vsync(&mut self) -> Result<(), &'static str> {
        self.execute_operation(DrmOperation::DisableVsync)
    }

    /// Cambiar buffer (doble buffer)
    pub fn flip_buffer(&mut self) -> Result<(), &'static str> {
        self.execute_operation(DrmOperation::FlipBuffer)
    }

    /// Obtener información de todos los drivers DRM
    pub fn get_drm_info(&self) -> Vec<String> {
        let mut info = Vec::new();

        for (i, driver) in self.drm_drivers.iter().enumerate() {
            if let Some(driver) = driver {
                let state_str = match driver.state {
                    DrmDriverState::Ready => "Listo",
                    DrmDriverState::Initializing => "Inicializando",
                    DrmDriverState::Error => "Error",
                    DrmDriverState::Suspended => "Suspendido",
                    DrmDriverState::Uninitialized => "No inicializado",
                };

                let drm_info = driver.get_info();
                let stats = driver.get_stats();
                
                info.push(format!(
                    "DRM {}: {} - {}x{} - {} - Estado: {}",
                    i + 1,
                    drm_info.device_path,
                    drm_info.width,
                    drm_info.height,
                    if stats.supports_hardware_acceleration { "HW Accel" } else { "SW" },
                    state_str
                ));
            }
        }

        if info.is_empty() {
            info.push("No se cargaron drivers DRM".to_string());
        }
        info
    }

    /// Obtener estadísticas del gestor DRM
    pub fn get_drm_stats(&self) -> DrmManagerStats {
        let total_drivers = self.drm_count;
        let ready_drivers = self.drm_drivers.iter()
            .filter_map(|d| d.as_ref())
            .filter(|d| d.is_ready())
            .count();
        let has_primary = self.primary_drm.is_some();
        let is_initialized = self.is_initialized;

        DrmManagerStats {
            total_drivers,
            ready_drivers,
            has_primary,
            is_initialized,
        }
    }

    /// Verificar si está inicializado
    pub fn is_initialized(&self) -> bool {
        self.is_initialized
    }
}

/// Estadísticas del gestor DRM
#[derive(Debug, Clone)]
pub struct DrmManagerStats {
    pub total_drivers: usize,
    pub ready_drivers: usize,
    pub has_primary: bool,
    pub is_initialized: bool,
}

/// Función de conveniencia para crear gestor DRM
pub fn create_drm_manager() -> DrmManager {
    DrmManager::new()
}

/// Función de conveniencia para inicializar DRM con framebuffer
pub fn initialize_drm_manager(framebuffer_info: Option<FramebufferInfo>) -> Result<DrmManager, &'static str> {
    let mut manager = DrmManager::new();
    manager.initialize(framebuffer_info)?;
    Ok(manager)
}
