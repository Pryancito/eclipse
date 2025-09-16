use core::sync::atomic::{AtomicU32, Ordering};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::boxed::Box;
use alloc::format;

/// ID único para cada driver
pub type DriverId = u32;

/// Contador global para generar IDs únicos
static NEXT_DRIVER_ID: AtomicU32 = AtomicU32::new(1);

/// Estados posibles de un driver
#[derive(Debug, Clone, PartialEq)]
pub enum DriverState {
    Unloaded,
    Loading,
    Loaded,
    Initializing,
    Ready,
    Error(String),
    Unloading,
}

/// Información básica de un driver
#[derive(Debug, Clone)]
pub struct DriverInfo {
    pub id: DriverId,
    pub name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    pub state: DriverState,
    pub dependencies: Vec<String>,
    pub capabilities: Vec<DriverCapability>,
}

/// Capacidades que puede tener un driver
#[derive(Debug, Clone, PartialEq)]
pub enum DriverCapability {
    Graphics,
    Network,
    Storage,
    Audio,
    Input,
    Power,
    Security,
    Custom(String),
}

/// Mensaje IPC entre el kernel y los drivers
#[derive(Debug, Clone)]
pub enum DriverMessage {
    // Mensajes del kernel al driver
    Initialize,
    Shutdown,
    Suspend,
    Resume,
    GetStatus,
    GetCapabilities,
    ExecuteCommand { command: String, args: Vec<u8> },
    
    // Mensajes del driver al kernel
    StatusUpdate { status: DriverState },
    CapabilityUpdate { capabilities: Vec<DriverCapability> },
    Error { error: String },
    RequestResource { resource_type: String, resource_id: String },
    ReleaseResource { resource_type: String, resource_id: String },
    Custom { data: Vec<u8> },
}

/// Respuesta a un mensaje IPC
#[derive(Debug, Clone)]
pub enum DriverResponse {
    Success,
    SuccessWithData(Vec<u8>),
    Error(String),
    NotSupported,
    Busy,
}

/// Trait que deben implementar todos los drivers
pub trait Driver: Send + Sync {
    /// Inicializar el driver
    fn initialize(&mut self) -> Result<(), String>;
    
    /// Cerrar el driver
    fn shutdown(&mut self) -> Result<(), String>;
    
    /// Suspender el driver
    fn suspend(&mut self) -> Result<(), String>;
    
    /// Reanudar el driver
    fn resume(&mut self) -> Result<(), String>;
    
    /// Obtener información del driver
    fn get_info(&self) -> DriverInfo;
    
    /// Procesar mensaje IPC
    fn handle_message(&mut self, message: DriverMessage) -> DriverResponse;
    
    /// Obtener estado actual
    fn get_state(&self) -> DriverState;
    
    /// Verificar si el driver puede manejar un dispositivo específico
    fn can_handle_device(&self, vendor_id: u16, device_id: u16, class_code: u8) -> bool;
}

/// Manager de drivers con IPC
pub struct DriverManager {
    drivers: BTreeMap<DriverId, Box<dyn Driver>>,
    driver_info: BTreeMap<DriverId, DriverInfo>,
    message_queue: Vec<(DriverId, DriverMessage)>,
}

impl DriverManager {
    pub fn new() -> Self {
        Self {
            drivers: BTreeMap::new(),
            driver_info: BTreeMap::new(),
            message_queue: Vec::new(),
        }
    }
    
    /// Registrar un nuevo driver
    pub fn register_driver(&mut self, mut driver: Box<dyn Driver>) -> Result<DriverId, String> {
        let id = NEXT_DRIVER_ID.fetch_add(1, Ordering::SeqCst);
        let info = driver.get_info();
        
        // Verificar dependencias
        for dep in &info.dependencies {
            if !self.has_driver_by_name(dep) {
                return Err(format!("Dependencia no encontrada: {}", dep));
            }
        }
        
        // Inicializar el driver
        match driver.initialize() {
            Ok(_) => {
                let mut info = driver.get_info();
                info.id = id;
                info.state = DriverState::Ready;
                
                self.drivers.insert(id, driver);
                self.driver_info.insert(id, info.clone());
                
                // Driver registrado: {} (ID: {})
                Ok(id)
            }
            Err(e) => {
                let mut info = driver.get_info();
                info.id = id;
                info.state = DriverState::Error(e.clone());
                self.driver_info.insert(id, info);
                Err(e)
            }
        }
    }
    
    /// Desregistrar un driver
    pub fn unregister_driver(&mut self, id: DriverId) -> Result<(), String> {
        if let Some(mut driver) = self.drivers.remove(&id) {
            driver.shutdown()?;
            self.driver_info.remove(&id);
            // Driver desregistrado: ID {}
            Ok(())
        } else {
            Err(format!("Driver no encontrado: ID {}", id))
        }
    }
    
    /// Enviar mensaje a un driver
    pub fn send_message(&mut self, id: DriverId, message: DriverMessage) -> Result<DriverResponse, String> {
        if let Some(driver) = self.drivers.get_mut(&id) {
            Ok(driver.handle_message(message))
        } else {
            Err(format!("Driver no encontrado: ID {}", id))
        }
    }
    
    /// Encolar mensaje para procesamiento posterior
    pub fn queue_message(&mut self, id: DriverId, message: DriverMessage) {
        self.message_queue.push((id, message));
    }
    
    /// Procesar cola de mensajes
    pub fn process_message_queue(&mut self) {
        let messages = core::mem::take(&mut self.message_queue);
        for (id, message) in messages {
            if let Err(e) = self.send_message(id, message) {
                // Error procesando mensaje para driver
            }
        }
    }
    
    /// Obtener información de un driver
    pub fn get_driver_info(&self, id: DriverId) -> Option<&DriverInfo> {
        self.driver_info.get(&id)
    }
    
    /// Listar todos los drivers
    pub fn list_drivers(&self) -> Vec<&DriverInfo> {
        self.driver_info.values().collect()
    }
    
    /// Buscar driver por nombre
    pub fn find_driver_by_name(&self, name: &str) -> Option<DriverId> {
        self.driver_info.iter()
            .find(|(_, info)| info.name == name)
            .map(|(id, _)| *id)
    }
    
    /// Verificar si existe un driver por nombre
    pub fn has_driver_by_name(&self, name: &str) -> bool {
        self.driver_info.values().any(|info| info.name == name)
    }
    
    /// Obtener drivers por capacidad
    pub fn get_drivers_by_capability(&self, capability: &DriverCapability) -> Vec<DriverId> {
        self.driver_info.iter()
            .filter(|(_, info)| info.capabilities.contains(capability))
            .map(|(id, _)| *id)
            .collect()
    }
    
    /// Obtener driver que puede manejar un dispositivo específico
    pub fn get_driver_for_device(&self, vendor_id: u16, device_id: u16, class_code: u8) -> Option<DriverId> {
        self.drivers.iter()
            .find(|(_, driver)| driver.can_handle_device(vendor_id, device_id, class_code))
            .map(|(id, _)| *id)
    }
    
    /// Suspender todos los drivers
    pub fn suspend_all(&mut self) -> Result<(), String> {
        for (id, driver) in self.drivers.iter_mut() {
            if let Err(e) = driver.suspend() {
                return Err(format!("Error suspendiendo driver {}: {}", id, e));
            }
        }
        Ok(())
    }
    
    /// Reanudar todos los drivers
    pub fn resume_all(&mut self) -> Result<(), String> {
        for (id, driver) in self.drivers.iter_mut() {
            if let Err(e) = driver.resume() {
                return Err(format!("Error reanudando driver {}: {}", id, e));
            }
        }
        Ok(())
    }
}

impl Default for DriverManager {
    fn default() -> Self {
        Self::new()
    }
}
