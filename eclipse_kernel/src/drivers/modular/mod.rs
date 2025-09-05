//! Sistema de drivers modulares para Eclipse OS
//! 
//! Este módulo implementa un sistema de drivers que se pueden cargar
//! dinámicamente, incluyendo drivers avanzados como DRM, GPU, etc.

pub mod drm;
pub mod gpu;
pub mod audio;
pub mod network_advanced;
pub mod auto_register;
pub mod demo;
pub mod manager;
pub mod std_modules;

/// Trait para drivers modulares
pub trait ModularDriver {
    /// Nombre del driver
    fn name(&self) -> &'static str;
    
    /// Versión del driver
    fn version(&self) -> &'static str;
    
    /// Inicializar el driver
    fn init(&mut self) -> Result<(), DriverError>;
    
    /// Verificar si el driver está disponible
    fn is_available(&self) -> bool;
    
    /// Obtener información del driver
    fn get_info(&self) -> DriverInfo;
    
    /// Cerrar el driver
    fn close(&mut self);
}

/// Información del driver
#[derive(Debug, Clone)]
pub struct DriverInfo {
    pub name: heapless::String<32>,
    pub version: heapless::String<16>,
    pub vendor: heapless::String<32>,
    pub capabilities: heapless::Vec<Capability, 16>,
}

/// Capacidades del driver
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Capability {
    Graphics,
    Audio,
    Network,
    Storage,
    Input,
    PowerManagement,
    HardwareAcceleration,
}

/// Errores del driver
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DriverError {
    NotAvailable,
    InitializationFailed,
    HardwareError,
    PermissionDenied,
    NotSupported,
    InvalidParameter,
}

/// Gestor de drivers modulares
pub struct ModularDriverManager {
    drivers: heapless::Vec<&'static mut dyn ModularDriver, 8>,
}

impl ModularDriverManager {
    /// Crear nuevo gestor de drivers
    pub const fn new() -> Self {
        Self {
            drivers: heapless::Vec::new(),
        }
    }
    
    /// Registrar un driver
    pub fn register_driver(&mut self, driver: &'static mut dyn ModularDriver) -> Result<(), DriverError> {
        if self.drivers.len() >= self.drivers.capacity() {
            return Err(DriverError::NotSupported);
        }
        
        if let Err(_) = self.drivers.push(driver) {
            return Err(DriverError::NotSupported);
        }
        
        Ok(())
    }
    
    /// Inicializar todos los drivers
    pub fn init_all(&mut self) {
        for driver in self.drivers.iter_mut() {
            let _ = driver.init();
        }
    }
    
    /// Obtener driver por nombre
    pub fn get_driver(&mut self, name: &str) -> Option<&mut dyn ModularDriver> {
        for driver in self.drivers.iter_mut() {
            if driver.name() == name {
                return Some(*driver);
            }
        }
        None
    }
    
    /// Listar drivers disponibles
    pub fn list_drivers(&self) -> heapless::Vec<heapless::String<32>, 8> {
        let mut names = heapless::Vec::new();
        for driver in self.drivers.iter() {
            let mut name = heapless::String::<32>::new();
            let _ = name.push_str(driver.name());
            let _ = names.push(name);
        }
        names
    }
}

/// Instancia global del gestor de drivers modulares
static mut MODULAR_DRIVER_MANAGER: ModularDriverManager = ModularDriverManager::new();

/// Inicializar el sistema de drivers modulares
pub fn init_modular_drivers() {
    unsafe {
        // Registrar automáticamente todos los drivers disponibles
        let _ = auto_register::auto_register_all_drivers();
        
        // Inicializar todos los drivers registrados
        MODULAR_DRIVER_MANAGER.init_all();
    }
}

/// Registrar un driver modular
pub fn register_modular_driver(driver: &'static mut dyn ModularDriver) -> Result<(), DriverError> {
    unsafe {
        MODULAR_DRIVER_MANAGER.register_driver(driver)
    }
}

/// Obtener driver modular por nombre
pub fn get_modular_driver(name: &str) -> Option<&'static mut dyn ModularDriver> {
    unsafe {
        MODULAR_DRIVER_MANAGER.get_driver(name)
    }
}

/// Listar drivers modulares disponibles
pub fn list_modular_drivers() -> heapless::Vec<heapless::String<32>, 8> {
    unsafe {
        MODULAR_DRIVER_MANAGER.list_drivers()
    }
}
