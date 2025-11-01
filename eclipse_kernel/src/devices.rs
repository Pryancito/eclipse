//! Sistema Básico de Dispositivos Virtuales para Eclipse OS
//!
//! Este módulo implementa un sistema completo de dispositivos virtuales que permite:
//! - Registro y gestión de dispositivos del sistema
//! - Framework para drivers de dispositivos
//! - Comunicación entre dispositivos y el kernel
//! - Abstracción de hardware para facilitar la portabilidad

#![no_std]
#![allow(unused_imports)]

extern crate alloc;
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::collections::{BTreeMap, VecDeque};
use core::any::Any;
use core::fmt;

// Logging simplificado para dispositivos

/// Tipos de dispositivos soportados
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceType {
    /// Dispositivo de almacenamiento (discos, SSD, etc.)
    Storage,
    /// Dispositivo de entrada (teclado, mouse, etc.)
    Input,
    /// Dispositivo de salida (pantalla, impresora, etc.)
    Output,
    /// Dispositivo de red (tarjetas de red, WiFi, etc.)
    Network,
    /// Dispositivo USB
    Usb,
    /// Dispositivo PCI
    Pci,
    /// Dispositivo genérico
    Generic,
}

/// Estados posibles de un dispositivo
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    /// Dispositivo no inicializado
    Uninitialized,
    /// Dispositivo inicializándose
    Initializing,
    /// Dispositivo listo para usar
    Ready,
    /// Dispositivo en uso
    Busy,
    /// Dispositivo suspendido
    Suspended,
    /// Dispositivo con error
    Error,
    /// Dispositivo desconectado/removido
    Removed,
}

/// Información básica de un dispositivo
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Identificador único del dispositivo
    pub id: DeviceId,
    /// Nombre del dispositivo
    pub name: String,
    /// Tipo de dispositivo
    pub device_type: DeviceType,
    /// Estado actual del dispositivo
    pub state: DeviceState,
    /// Versión del driver
    pub driver_version: String,
    /// Dirección base (para dispositivos mapeados en memoria)
    pub base_address: Option<u64>,
    /// IRQ asignada (si aplica)
    pub irq: Option<u8>,
    /// Vendor ID (para PCI/USB)
    pub vendor_id: Option<u16>,
    /// Device ID (para PCI/USB)
    pub device_id: Option<u16>,
}

impl DeviceInfo {
    /// Crea una nueva instancia de DeviceInfo
    pub fn new(id: DeviceId, name: &str, device_type: DeviceType) -> Self {
        Self {
            id,
            name: name.to_string(),
            device_type,
            state: DeviceState::Uninitialized,
            driver_version: "1.0.0".to_string(),
            base_address: None,
            irq: None,
            vendor_id: None,
            device_id: None,
        }
    }
}

/// Identificador único de dispositivo
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(pub u64);

impl DeviceId {
    /// Genera un nuevo ID único de dispositivo
    pub fn new() -> Self {
        static mut NEXT_ID: u64 = 1;
        unsafe {
            let id = NEXT_ID;
            NEXT_ID += 1;
            DeviceId(id)
        }
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DEV:{:08x}", self.0)
    }
}

/// Resultado de operaciones de dispositivo
pub type DeviceResult<T> = Result<T, DeviceError>;

/// Errores que pueden ocurrir en operaciones de dispositivo
#[derive(Debug, Clone)]
pub enum DeviceError {
    /// Dispositivo no encontrado
    DeviceNotFound,
    /// Dispositivo no inicializado
    NotInitialized,
    /// Dispositivo ocupado
    Busy,
    /// Error de I/O
    IoError(String),
    /// Error de configuración
    ConfigError(String),
    /// Operación no soportada
    Unsupported,
    /// Error de tiempo de espera
    Timeout,
    /// Error genérico
    Other(String),
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceError::DeviceNotFound => write!(f, "Dispositivo no encontrado"),
            DeviceError::NotInitialized => write!(f, "Dispositivo no inicializado"),
            DeviceError::Busy => write!(f, "Dispositivo ocupado"),
            DeviceError::IoError(msg) => write!(f, "Error de I/O: {}", msg),
            DeviceError::ConfigError(msg) => write!(f, "Error de configuración: {}", msg),
            DeviceError::Unsupported => write!(f, "Operación no soportada"),
            DeviceError::Timeout => write!(f, "Tiempo de espera agotado"),
            DeviceError::Other(msg) => write!(f, "Error: {}", msg),
        }
    }
}

/// Trait para operaciones básicas de dispositivo
pub trait Device: Send + Sync {
    /// Inicializa el dispositivo
    fn init(&mut self) -> DeviceResult<()>;

    /// Apaga el dispositivo
    fn shutdown(&mut self) -> DeviceResult<()>;

    /// Suspende el dispositivo
    fn suspend(&mut self) -> DeviceResult<()> {
        Ok(()) // Implementación por defecto
    }

    /// Reanuda el dispositivo
    fn resume(&mut self) -> DeviceResult<()> {
        Ok(()) // Implementación por defecto
    }

    /// Obtiene información del dispositivo
    fn get_info(&self) -> &DeviceInfo;

    /// Obtiene información del dispositivo (mutable)
    fn get_info_mut(&mut self) -> &mut DeviceInfo;

    /// Maneja una interrupción (si el dispositivo la genera)
    fn handle_interrupt(&mut self) -> DeviceResult<()> {
        Ok(()) // Implementación por defecto
    }

    /// Operación específica del dispositivo (para downcasting)
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// Trait para dispositivos de almacenamiento
pub trait StorageDevice: Device {
    /// Lee datos del dispositivo
    fn read(&mut self, offset: u64, buffer: &mut [u8]) -> DeviceResult<usize>;

    /// Escribe datos al dispositivo
    fn write(&mut self, offset: u64, data: &[u8]) -> DeviceResult<usize>;

    /// Obtiene el tamaño del dispositivo en bytes
    fn size(&self) -> u64;

    /// Sincroniza los buffers (flush)
    fn flush(&mut self) -> DeviceResult<()> {
        Ok(())
    }
}

/// Trait para dispositivos de entrada
pub trait InputDevice: Device {
    /// Lee datos de entrada
    fn read_input(&mut self, buffer: &mut [u8]) -> DeviceResult<usize>;

    /// Verifica si hay datos disponibles
    fn has_data(&self) -> bool;
}

/// Trait para dispositivos de salida
pub trait OutputDevice: Device {
    /// Escribe datos de salida
    fn write_output(&mut self, data: &[u8]) -> DeviceResult<usize>;
}

/// Trait para dispositivos de red
pub trait NetworkDevice: Device {
    /// Envía un paquete de red
    fn send_packet(&mut self, data: &[u8]) -> DeviceResult<usize>;

    /// Recibe un paquete de red
    fn receive_packet(&mut self, buffer: &mut [u8]) -> DeviceResult<usize>;

    /// Obtiene la dirección MAC
    fn get_mac_address(&self) -> [u8; 6];

    /// Obtiene la dirección IP (si está configurada)
    fn get_ip_address(&self) -> Option<[u8; 4]>;
}

/// Estructura principal del administrador de dispositivos
pub struct DeviceManager {
    /// Mapa de dispositivos registrados
    devices: BTreeMap<DeviceId, Box<dyn Device>>,
    /// Cola de dispositivos pendientes de inicialización
    pending_init: VecDeque<DeviceId>,
    /// Próximo ID a asignar
    next_id: u64,
}

impl DeviceManager {
    /// Crea un nuevo administrador de dispositivos
    pub fn new() -> Self {
        Self {
            devices: BTreeMap::new(),
            pending_init: VecDeque::new(),
            next_id: 1,
        }
    }

    /// Registra un nuevo dispositivo
    pub fn register_device(&mut self, mut device: Box<dyn Device>) -> DeviceResult<DeviceId> {
        let device_id = DeviceId(self.next_id);
        self.next_id += 1;

        // Actualizar el ID en la información del dispositivo
        device.get_info_mut().id = device_id;

        // Agregar a la lista de dispositivos
        self.devices.insert(device_id, device);

        // Agregar a la cola de inicialización
        self.pending_init.push_back(device_id);

        // Logging simplificado para dispositivos
        Ok(device_id)
    }

    /// Desregistra un dispositivo
    pub fn unregister_device(&mut self, device_id: DeviceId) -> DeviceResult<Box<dyn Device>> {
        if let Some(device) = self.devices.remove(&device_id) {
            // Remover de la cola de inicialización si está ahí
            self.pending_init.retain(|&id| id != device_id);

            // Logging disabled
            Ok(device)
        } else {
            Err(DeviceError::DeviceNotFound)
        }
    }

    /// Inicializa dispositivos pendientes
    pub fn initialize_pending_devices(&mut self) -> DeviceResult<()> {
        while let Some(device_id) = self.pending_init.front().cloned() {
            self.pending_init.pop_front();

            if let Some(device) = self.devices.get_mut(&device_id) {
                match device.init() {
                    Ok(()) => {
                        // Logging disabled
                    }
                    Err(e) => {
                        // Logging disabled
                        // Marcar como error pero continuar con otros dispositivos
                        device.get_info_mut().state = DeviceState::Error;
                    }
                }
            }
        }
        Ok(())
    }

    /// Obtiene un dispositivo por ID
    pub fn get_device(&self, device_id: DeviceId) -> Option<&Box<dyn Device>> {
        self.devices.get(&device_id)
    }

    /// Obtiene un dispositivo por ID (mutable)
    pub fn get_device_mut(&mut self, device_id: DeviceId) -> Option<&mut Box<dyn Device>> {
        self.devices.get_mut(&device_id)
    }

    /// Obtiene un dispositivo como un tipo específico
    pub fn get_device_as<T: 'static>(&self, device_id: DeviceId) -> Option<&T> {
        self.devices.get(&device_id)?
            .as_any().downcast_ref::<T>()
    }

    /// Obtiene un dispositivo como un tipo específico (mutable)
    pub fn get_device_as_mut<T: 'static>(&mut self, device_id: DeviceId) -> Option<&mut T> {
        self.devices.get_mut(&device_id)?
            .as_any_mut().downcast_mut::<T>()
    }

    /// Lista todos los dispositivos
    pub fn list_devices(&self) -> Vec<DeviceInfo> {
        self.devices.values()
            .map(|device| device.get_info().clone())
            .collect()
    }

    /// Lista dispositivos por tipo
    pub fn list_devices_by_type(&self, device_type: DeviceType) -> Vec<DeviceInfo> {
        self.devices.values()
            .filter(|device| device.get_info().device_type == device_type)
            .map(|device| device.get_info().clone())
            .collect()
    }

    /// Maneja una interrupción para un dispositivo específico
    pub fn handle_device_interrupt(&mut self, device_id: DeviceId) -> DeviceResult<()> {
        if let Some(device) = self.devices.get_mut(&device_id) {
            device.handle_interrupt()
        } else {
            Err(DeviceError::DeviceNotFound)
        }
    }

    /// Obtiene estadísticas del administrador de dispositivos
    pub fn get_stats(&self) -> DeviceManagerStats {
        let total_devices = self.devices.len();
        let ready_devices = self.devices.values()
            .filter(|d| d.get_info().state == DeviceState::Ready)
            .count();
        let error_devices = self.devices.values()
            .filter(|d| d.get_info().state == DeviceState::Error)
            .count();

        DeviceManagerStats {
            total_devices,
            ready_devices,
            error_devices,
            pending_init: self.pending_init.len(),
        }
    }
}

/// Estadísticas del administrador de dispositivos
#[derive(Debug, Clone)]
pub struct DeviceManagerStats {
    /// Número total de dispositivos
    pub total_devices: usize,
    /// Número de dispositivos listos
    pub ready_devices: usize,
    /// Número de dispositivos con error
    pub error_devices: usize,
    /// Número de dispositivos pendientes de inicialización
    pub pending_init: usize,
}

/// Instancia global del administrador de dispositivos
static mut DEVICE_MANAGER: Option<DeviceManager> = None;

/// Inicializa el sistema de dispositivos virtuales
pub fn init_device_system() -> DeviceResult<()> {
    unsafe {
        DEVICE_MANAGER = Some(DeviceManager::new());
    }
    // Logging disabled
    Ok(())
}

/// Obtiene una referencia al administrador de dispositivos
pub fn get_device_manager() -> Option<&'static mut DeviceManager> {
    unsafe {
        DEVICE_MANAGER.as_mut()
    }
}

/// Registra un dispositivo en el sistema global
pub fn register_device_global(device: Box<dyn Device>) -> DeviceResult<DeviceId> {
    if let Some(manager) = get_device_manager() {
        manager.register_device(device)
    } else {
        Err(DeviceError::Other("Administrador de dispositivos no inicializado".to_string()))
    }
}

/// Inicializa dispositivos pendientes globalmente
pub fn initialize_pending_devices_global() -> DeviceResult<()> {
    if let Some(manager) = get_device_manager() {
        manager.initialize_pending_devices()
    } else {
        Err(DeviceError::Other("Administrador de dispositivos no inicializado".to_string()))
    }
}

// Macros para facilitar el uso del sistema de logging
#[macro_export]
macro_rules! device_debug {
    ($($arg:tt)*) => ($crate::logging::get_logger().debug("DEVICE", &alloc::format!($($arg)*)))
}

#[macro_export]
macro_rules! device_info {
    ($($arg:tt)*) => ($crate::logging::get_logger().info("DEVICE", &alloc::format!($($arg)*)))
}

#[macro_export]
macro_rules! device_warn {
    ($($arg:tt)*) => ($crate::logging::get_logger().warn("DEVICE", &alloc::format!($($arg)*)))
}

#[macro_export]
macro_rules! device_error {
    ($($arg:tt)*) => ($crate::logging::get_logger().error("DEVICE", &alloc::format!($($arg)*)))
}
