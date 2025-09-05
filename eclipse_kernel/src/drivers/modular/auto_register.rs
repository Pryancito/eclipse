//! Sistema de registro automático de drivers modulares
//! 
//! Permite registrar automáticamente todos los drivers modulares
//! disponibles en el sistema.

use super::{ModularDriver, DriverError, ModularDriverManager};

/// Registro automático de drivers
pub struct AutoDriverRegistrar {
    registered_count: usize,
}

impl AutoDriverRegistrar {
    /// Crear nuevo registrador automático
    pub const fn new() -> Self {
        Self {
            registered_count: 0,
        }
    }
    
    /// Registrar todos los drivers modulares disponibles
    pub fn register_all_drivers(&mut self, manager: &mut ModularDriverManager) -> Result<usize, DriverError> {
        let mut registered = 0;
        
        // Registrar driver DRM
        if let Ok(_) = self.register_drm_driver(manager) {
            registered += 1;
        }
        
        // Registrar driver GPU
        if let Ok(_) = self.register_gpu_driver(manager) {
            registered += 1;
        }
        
        // Registrar driver de audio
        if let Ok(_) = self.register_audio_driver(manager) {
            registered += 1;
        }
        
        // Registrar driver de red avanzado
        if let Ok(_) = self.register_network_driver(manager) {
            registered += 1;
        }
        
        self.registered_count = registered;
        Ok(registered)
    }
    
    /// Registrar driver DRM
    fn register_drm_driver(&self, manager: &mut ModularDriverManager) -> Result<(), DriverError> {
        use super::drm::get_drm_driver;
        let drm_driver = get_drm_driver();
        manager.register_driver(drm_driver)
    }
    
    /// Registrar driver GPU
    fn register_gpu_driver(&self, manager: &mut ModularDriverManager) -> Result<(), DriverError> {
        use super::gpu::get_gpu_driver;
        let gpu_driver = get_gpu_driver();
        manager.register_driver(gpu_driver)
    }
    
    /// Registrar driver de audio
    fn register_audio_driver(&self, manager: &mut ModularDriverManager) -> Result<(), DriverError> {
        use super::audio::get_audio_driver;
        let audio_driver = get_audio_driver();
        manager.register_driver(audio_driver)
    }
    
    /// Registrar driver de red
    fn register_network_driver(&self, manager: &mut ModularDriverManager) -> Result<(), DriverError> {
        use super::network_advanced::get_network_advanced_driver;
        let network_driver = get_network_advanced_driver();
        manager.register_driver(network_driver)
    }
    
    /// Obtener número de drivers registrados
    pub fn get_registered_count(&self) -> usize {
        self.registered_count
    }
}

/// Instancia global del registrador automático
static mut AUTO_DRIVER_REGISTRAR: AutoDriverRegistrar = AutoDriverRegistrar::new();

/// Registrar automáticamente todos los drivers modulares
pub fn auto_register_all_drivers() -> Result<usize, DriverError> {
    unsafe {
        use super::MODULAR_DRIVER_MANAGER;
        AUTO_DRIVER_REGISTRAR.register_all_drivers(&mut MODULAR_DRIVER_MANAGER)
    }
}

/// Obtener número de drivers registrados
pub fn get_registered_driver_count() -> usize {
    unsafe {
        AUTO_DRIVER_REGISTRAR.get_registered_count()
    }
}



