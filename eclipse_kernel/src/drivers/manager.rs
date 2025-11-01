//! Gestor de drivers para Eclipse OS
//!
//! Basado en la arquitectura de drivers de Redox OS

use crate::drivers::{
    device::{Device, DeviceInfo, DeviceState, DeviceType},
    DRIVER_NAME_LEN, MAX_DEVICES, MAX_DRIVERS,
};

// Importar tipos necesarios para no_std
use alloc::boxed::Box;
use alloc::vec::Vec;

// Resultado de operaciones de drivers
pub type DriverResult<T> = Result<T, DriverError>;

// Errores del gestor de drivers
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DriverError {
    DeviceNotFound,
    DriverNotFound,
    DeviceAlreadyExists,
    DriverAlreadyExists,
    DeviceBusy,
    DeviceNotReady,
    InvalidParameter,
    OutOfMemory,
    IoError,
    Timeout,
    Unknown,
}

impl DriverError {
    pub fn as_str(&self) -> &'static str {
        match self {
            DriverError::DeviceNotFound => "Device not found",
            DriverError::DriverNotFound => "Driver not found",
            DriverError::DeviceAlreadyExists => "Device already exists",
            DriverError::DriverAlreadyExists => "Driver already exists",
            DriverError::DeviceBusy => "Device busy",
            DriverError::DeviceNotReady => "Device not ready",
            DriverError::InvalidParameter => "Invalid parameter",
            DriverError::OutOfMemory => "Out of memory",
            DriverError::IoError => "I/O error",
            DriverError::Timeout => "Timeout",
            DriverError::Unknown => "Unknown error",
        }
    }
}

// Información de driver
#[derive(Debug, Clone)]
pub struct DriverInfo {
    pub id: u32,
    pub name: [u8; DRIVER_NAME_LEN],
    pub device_type: DeviceType,
    pub version: u32,
    pub is_loaded: bool,
    pub device_count: u32,
}

impl DriverInfo {
    pub fn new() -> Self {
        Self {
            id: 0,
            name: [0; DRIVER_NAME_LEN],
            device_type: DeviceType::Unknown,
            version: 0,
            is_loaded: false,
            device_count: 0,
        }
    }

    pub fn set_name(&mut self, name: &str) {
        let name_bytes = name.as_bytes();
        let len = name_bytes.len().min(DRIVER_NAME_LEN - 1);

        for i in 0..DRIVER_NAME_LEN {
            if i < len {
                self.name[i] = name_bytes[i];
            } else {
                self.name[i] = 0;
            }
        }
    }
}

// Trait para drivers
pub trait Driver {
    fn get_info(&self) -> &DriverInfo;
    fn initialize(&mut self) -> DriverResult<()>;
    fn cleanup(&mut self) -> DriverResult<()>;
    fn probe_device(&mut self, device_info: &DeviceInfo) -> bool;
    fn attach_device(&mut self, device: &mut Device) -> DriverResult<()>;
    fn detach_device(&mut self, device_id: u32) -> DriverResult<()>;
    fn handle_interrupt(&mut self, device_id: u32) -> DriverResult<()>;
}

// Gestor de drivers
pub struct DriverManager {
    pub devices: [Option<Device>; MAX_DEVICES],
    pub drivers: [Option<Box<dyn Driver>>; MAX_DRIVERS],
    pub device_count: u32,
    pub driver_count: u32,
    pub next_device_id: u32,
    pub next_driver_id: u32,
}

impl DriverManager {
    pub fn new() -> Self {
        Self {
            devices: [(); MAX_DEVICES].map(|_| None),
            drivers: [(); MAX_DRIVERS].map(|_| None),
            device_count: 0,
            driver_count: 0,
            next_device_id: 1,
            next_driver_id: 1,
        }
    }

    /// Inicializar el gestor de drivers
    pub fn init(&mut self) -> DriverResult<()> {
        // Inicializar contadores
        self.next_device_id = 1;
        self.next_driver_id = 1;
        self.device_count = 0;
        self.driver_count = 0;

        // Limpiar arrays
        for i in 0..MAX_DEVICES {
            self.devices[i] = None;
        }
        for i in 0..MAX_DRIVERS {
            self.drivers[i] = None;
        }

        Ok(())
    }

    /// Registrar un nuevo driver
    pub fn register_driver(&mut self, mut driver: Box<dyn Driver>) -> DriverResult<u32> {
        if self.driver_count >= MAX_DRIVERS as u32 {
            return Err(DriverError::OutOfMemory);
        }

        let driver_id = self.next_driver_id;
        self.next_driver_id += 1;

        // Inicializar el driver
        driver.initialize()?;

        // Buscar slot libre
        for i in 0..MAX_DRIVERS {
            if self.drivers[i].is_none() {
                self.drivers[i] = Some(driver);
                self.driver_count += 1;
                return Ok(driver_id);
            }
        }

        Err(DriverError::OutOfMemory)
    }

    /// Desregistrar un driver
    pub fn unregister_driver(&mut self, driver_id: u32) -> DriverResult<()> {
        for i in 0..MAX_DRIVERS {
            if let Some(ref mut driver) = self.drivers[i] {
                if driver.get_info().id == driver_id {
                    driver.cleanup()?;
                    self.drivers[i] = None;
                    self.driver_count -= 1;
                    return Ok(());
                }
            }
        }
        Err(DriverError::DriverNotFound)
    }

    /// Registrar un nuevo dispositivo
    pub fn register_device(&mut self, mut device_info: DeviceInfo) -> DriverResult<u32> {
        if self.device_count >= MAX_DEVICES as u32 {
            return Err(DriverError::OutOfMemory);
        }

        let device_id = self.next_device_id;
        self.next_device_id += 1;

        device_info.id = device_id;
        let mut device = Device::new(device_info);

        // Buscar driver compatible
        for i in 0..MAX_DRIVERS {
            if let Some(ref mut driver) = self.drivers[i] {
                if driver.probe_device(&device.info) {
                    driver.attach_device(&mut device)?;
                    break;
                }
            }
        }

        // Buscar slot libre
        for i in 0..MAX_DEVICES {
            if self.devices[i].is_none() {
                self.devices[i] = Some(device);
                self.device_count += 1;
                return Ok(device_id);
            }
        }

        Err(DriverError::OutOfMemory)
    }

    /// Desregistrar un dispositivo
    pub fn unregister_device(&mut self, device_id: u32) -> DriverResult<()> {
        for i in 0..MAX_DEVICES {
            if let Some(ref mut device) = self.devices[i] {
                if device.info.id == device_id {
                    // Detach del driver
                    if let Some(driver_id) = device.driver_id {
                        for j in 0..MAX_DRIVERS {
                            if let Some(ref mut driver) = self.drivers[j] {
                                if driver.get_info().id == driver_id {
                                    let _ = driver.detach_device(device_id);
                                    break;
                                }
                            }
                        }
                    }

                    self.devices[i] = None;
                    self.device_count -= 1;
                    return Ok(());
                }
            }
        }
        Err(DriverError::DeviceNotFound)
    }

    /// Obtener dispositivo por ID
    pub fn get_device(&self, device_id: u32) -> Option<&Device> {
        for i in 0..MAX_DEVICES {
            if let Some(ref device) = self.devices[i] {
                if device.info.id == device_id {
                    return Some(device);
                }
            }
        }
        None
    }

    /// Obtener dispositivo mutable por ID (simplificado)
    pub fn get_device_mut(&mut self, _device_id: u32) -> Option<&mut Device> {
        // Implementación simplificada - no hace nada por ahora
        None
    }

    /// Obtener driver por ID
    pub fn get_driver(&self, driver_id: u32) -> Option<&dyn Driver> {
        for i in 0..MAX_DRIVERS {
            if let Some(ref driver) = self.drivers[i] {
                if driver.get_info().id == driver_id {
                    return Some(driver.as_ref());
                }
            }
        }
        None
    }

    /// Obtener driver mutable por ID (simplificado)
    pub fn get_driver_mut(&mut self, _driver_id: u32) -> Option<&mut dyn Driver> {
        // Implementación simplificada - no hace nada por ahora
        None
    }

    /// Listar dispositivos por tipo
    pub fn list_devices_by_type(&self, device_type: DeviceType) -> Vec<u32> {
        let mut devices = Vec::new();
        for i in 0..MAX_DEVICES {
            if let Some(ref device) = self.devices[i] {
                if device.info.device_type == device_type {
                    devices.push(device.info.id);
                }
            }
        }
        devices
    }

    /// Listar todos los dispositivos
    pub fn list_all_devices(&self) -> Vec<u32> {
        let mut devices = Vec::new();
        for i in 0..MAX_DEVICES {
            if let Some(ref device) = self.devices[i] {
                devices.push(device.info.id);
            }
        }
        devices
    }

    /// Obtener estadísticas
    pub fn get_stats(&self) -> DriverStats {
        let mut stats = DriverStats::new();

        stats.total_devices = self.device_count;
        stats.total_drivers = self.driver_count;

        for i in 0..MAX_DEVICES {
            if let Some(ref device) = self.devices[i] {
                match device.info.device_type {
                    DeviceType::Storage => stats.storage_devices += 1,
                    DeviceType::Network => stats.network_devices += 1,
                    DeviceType::Video => stats.video_devices += 1,
                    DeviceType::Audio => stats.audio_devices += 1,
                    DeviceType::Input => stats.input_devices += 1,
                    _ => stats.other_devices += 1,
                }

                if device.is_ready() {
                    stats.ready_devices += 1;
                } else if device.info.state == DeviceState::Error {
                    stats.error_devices += 1;
                }
            }
        }

        stats
    }
}

// Estadísticas del gestor de drivers
#[derive(Debug, Clone, Copy)]
pub struct DriverStats {
    pub total_devices: u32,
    pub total_drivers: u32,
    pub ready_devices: u32,
    pub error_devices: u32,
    pub storage_devices: u32,
    pub network_devices: u32,
    pub video_devices: u32,
    pub audio_devices: u32,
    pub input_devices: u32,
    pub other_devices: u32,
}

impl DriverStats {
    pub fn new() -> Self {
        Self {
            total_devices: 0,
            total_drivers: 0,
            ready_devices: 0,
            error_devices: 0,
            storage_devices: 0,
            network_devices: 0,
            video_devices: 0,
            audio_devices: 0,
            input_devices: 0,
            other_devices: 0,
        }
    }
}

// Instancia global del gestor de drivers
static mut DRIVER_MANAGER: Option<DriverManager> = None;

/// Inicializar el gestor de drivers
pub fn init_driver_manager() -> DriverResult<()> {
    unsafe {
        DRIVER_MANAGER = Some(DriverManager::new());
        if let Some(ref mut manager) = DRIVER_MANAGER {
            manager.init()?;
        }
    }
    Ok(())
}

/// Obtener instancia del gestor de drivers
pub fn get_driver_manager() -> Option<&'static mut DriverManager> {
    unsafe { DRIVER_MANAGER.as_mut() }
}
