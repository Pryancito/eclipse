//! Drivers de almacenamiento para Eclipse OS
//! 
//! Basado en los drivers de almacenamiento de Redox OS

use crate::drivers::{
    device::{Device, DeviceInfo, DeviceType},
    manager::{Driver, DriverInfo, DriverResult, DriverError},
    MAX_DEVICES,
};

// Importar tipos necesarios para no_std
use alloc::vec::Vec;

// Información de dispositivo de almacenamiento
#[derive(Debug, Clone)]
pub struct StorageDeviceInfo {
    pub device_id: u32,
    pub name: [u8; 32],
    pub capacity: u64,
    pub block_size: u32,
    pub sector_size: u32,
    pub is_removable: bool,
    pub is_read_only: bool,
    pub interface_type: StorageInterface,
    pub model: [u8; 64],
    pub serial: [u8; 32],
    pub firmware: [u8; 16],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StorageInterface {
    ATA,
    SATA,
    NVMe,
    SCSI,
    USB,
    Unknown,
}

impl StorageInterface {
    pub fn as_str(&self) -> &'static str {
        match self {
            StorageInterface::ATA => "ATA",
            StorageInterface::SATA => "SATA",
            StorageInterface::NVMe => "NVMe",
            StorageInterface::SCSI => "SCSI",
            StorageInterface::USB => "USB",
            StorageInterface::Unknown => "Unknown",
        }
    }
}

// Driver de almacenamiento base
pub struct StorageDriver {
    pub info: DriverInfo,
    pub devices: [Option<StorageDeviceInfo>; MAX_DEVICES],
    pub device_count: u32,
}

impl StorageDriver {
    pub fn new() -> Self {
        let mut info = DriverInfo::new();
        info.set_name("storage");
        info.device_type = DeviceType::Storage;
        info.version = 1;

        Self {
            info,
            devices: [(); MAX_DEVICES].map(|_| None),
            device_count: 0,
        }
    }

    pub fn add_device(&mut self, device_info: StorageDeviceInfo) -> DriverResult<()> {
        if self.device_count >= MAX_DEVICES as u32 {
            return Err(DriverError::OutOfMemory);
        }

        for i in 0..MAX_DEVICES {
            if self.devices[i].is_none() {
                self.devices[i] = Some(device_info);
                self.device_count += 1;
                return Ok(());
            }
        }

        Err(DriverError::OutOfMemory)
    }

    pub fn remove_device(&mut self, device_id: u32) -> DriverResult<()> {
        for i in 0..MAX_DEVICES {
            if let Some(ref device) = self.devices[i] {
                if device.device_id == device_id {
                    self.devices[i] = None;
                    self.device_count -= 1;
                    return Ok(());
                }
            }
        }
        Err(DriverError::DeviceNotFound)
    }

    pub fn get_device(&self, device_id: u32) -> Option<&StorageDeviceInfo> {
        for i in 0..MAX_DEVICES {
            if let Some(ref device) = self.devices[i] {
                if device.device_id == device_id {
                    return Some(device);
                }
            }
        }
        None
    }

    pub fn list_devices(&self) -> Vec<u32> {
        let mut devices = Vec::new();
        for i in 0..MAX_DEVICES {
            if let Some(ref device) = self.devices[i] {
                devices.push(device.device_id);
            }
        }
        devices
    }
}

impl Driver for StorageDriver {
    fn get_info(&self) -> &DriverInfo {
        &self.info
    }

    fn initialize(&mut self) -> DriverResult<()> {
        // Inicialización simplificada
        self.info.is_loaded = true;
        Ok(())
    }

    fn cleanup(&mut self) -> DriverResult<()> {
        // Limpiar todos los dispositivos
        for i in 0..MAX_DEVICES {
            self.devices[i] = None;
        }
        self.device_count = 0;
        self.info.is_loaded = false;
        Ok(())
    }

    fn probe_device(&mut self, device_info: &DeviceInfo) -> bool {
        // Probar si es un dispositivo de almacenamiento
        device_info.device_type == DeviceType::Storage
    }

    fn attach_device(&mut self, device: &mut Device) -> DriverResult<()> {
        // Crear información de dispositivo de almacenamiento
        let mut storage_info = StorageDeviceInfo {
            device_id: device.info.id,
            name: [0; 32],
            capacity: 0,
            block_size: 512,
            sector_size: 512,
            is_removable: false,
            is_read_only: false,
            interface_type: StorageInterface::Unknown,
            model: [0; 64],
            serial: [0; 32],
            firmware: [0; 16],
        };

        // Configurar nombre
        storage_info.name[..device.info.name.len()].copy_from_slice(&device.info.name);
        
        // Configurar capacidad simulada (1GB)
        storage_info.capacity = 1024 * 1024 * 1024;
        
        // Configurar interfaz basada en vendor/device ID
        if device.info.vendor_id == 0x8086 {
            storage_info.interface_type = StorageInterface::SATA;
        } else if device.info.vendor_id == 0x1042 {
            storage_info.interface_type = StorageInterface::ATA;
        } else {
            storage_info.interface_type = StorageInterface::Unknown;
        }

        // Agregar dispositivo
        self.add_device(storage_info)?;
        
        // Configurar driver ID en el dispositivo
        device.driver_id = Some(self.info.id);
        
        Ok(())
    }

    fn detach_device(&mut self, device_id: u32) -> DriverResult<()> {
        self.remove_device(device_id)
    }

    fn handle_interrupt(&mut self, _device_id: u32) -> DriverResult<()> {
        // Manejo de interrupciones simplificado
        Ok(())
    }
}

// Driver ATA (basado en ided de Redox)
pub struct AtaDriver {
    pub base: StorageDriver,
    pub channels: [AtaChannel; 2],
}

#[derive(Debug, Clone)]
pub struct AtaChannel {
    pub base_port: u16,
    pub control_port: u16,
    pub busmaster_port: u16,
    pub irq: u8,
    pub is_enabled: bool,
}

impl AtaChannel {
    pub fn new(base_port: u16, control_port: u16, busmaster_port: u16, irq: u8) -> Self {
        Self {
            base_port,
            control_port,
            busmaster_port,
            irq,
            is_enabled: false,
        }
    }

    pub fn enable(&mut self) {
        self.is_enabled = true;
    }

    pub fn disable(&mut self) {
        self.is_enabled = false;
    }
}

impl AtaDriver {
    pub fn new() -> Self {
        Self {
            base: StorageDriver::new(),
            channels: [
                AtaChannel::new(0x1F0, 0x3F6, 0, 14), // Primary
                AtaChannel::new(0x170, 0x376, 0, 15), // Secondary
            ],
        }
    }

    pub fn detect_devices(&mut self) -> DriverResult<()> {
        for i in 0..2 {
            self.channels[i].enable();
            
            // Detectar dispositivos en el canal
            for device in 0..2 {
                if let Ok(device_info) = self.identify_device(i, device) {
                    let mut storage_info = StorageDeviceInfo {
                        device_id: (i * 2 + device) as u32 + 1,
                        name: [0; 32],
                        capacity: device_info.capacity,
                        block_size: 512,
                        sector_size: 512,
                        is_removable: false,
                        is_read_only: false,
                        interface_type: StorageInterface::ATA,
                        model: device_info.model,
                        serial: device_info.serial,
                        firmware: device_info.firmware,
                    };
                    
                    // Configurar nombre (simplificado)
                    let name = b"ata0.0";
                    let len = name.len().min(31);
                    storage_info.name[..len].copy_from_slice(&name[..len]);
                    
                    self.base.add_device(storage_info)?;
                }
            }
        }
        Ok(())
    }

    fn identify_device(&self, channel: usize, device: usize) -> DriverResult<AtaDeviceInfo> {
        // Implementación simplificada de identificación ATA
        // En una implementación real, esto leería los registros ATA
        
        Ok(AtaDeviceInfo {
            capacity: 1024 * 1024 * 1024, // 1GB
            model: [0; 64],
            serial: [0; 32],
            firmware: [0; 16],
        })
    }
}

#[derive(Debug, Clone)]
pub struct AtaDeviceInfo {
    pub capacity: u64,
    pub model: [u8; 64],
    pub serial: [u8; 32],
    pub firmware: [u8; 16],
}

impl Driver for AtaDriver {
    fn get_info(&self) -> &DriverInfo {
        self.base.get_info()
    }

    fn initialize(&mut self) -> DriverResult<()> {
        self.base.initialize()?;
        self.detect_devices()?;
        Ok(())
    }

    fn cleanup(&mut self) -> DriverResult<()> {
        for channel in &mut self.channels {
            channel.disable();
        }
        self.base.cleanup()
    }

    fn probe_device(&mut self, device_info: &DeviceInfo) -> bool {
        // Probar si es un dispositivo ATA
        device_info.device_type == DeviceType::Storage &&
        (device_info.class_code == 0x01 && device_info.subclass == 0x01) // Mass storage, ATA
    }

    fn attach_device(&mut self, device: &mut Device) -> DriverResult<()> {
        self.base.attach_device(device)
    }

    fn detach_device(&mut self, device_id: u32) -> DriverResult<()> {
        self.base.detach_device(device_id)
    }

    fn handle_interrupt(&mut self, device_id: u32) -> DriverResult<()> {
        // Manejo de interrupciones ATA
        self.base.handle_interrupt(device_id)
    }
}

// Driver NVMe (basado en nvmed de Redox)
pub struct NvmeDriver {
    pub base: StorageDriver,
    pub controllers: [NvmeController; 4],
}

#[derive(Debug, Clone)]
pub struct NvmeController {
    pub base_address: u64,
    pub is_enabled: bool,
    pub queue_count: u32,
    pub max_queues: u32,
}

impl NvmeController {
    pub fn new(base_address: u64) -> Self {
        Self {
            base_address,
            is_enabled: false,
            queue_count: 0,
            max_queues: 64,
        }
    }

    pub fn enable(&mut self) {
        self.is_enabled = true;
    }

    pub fn disable(&mut self) {
        self.is_enabled = false;
    }
}

impl NvmeDriver {
    pub fn new() -> Self {
        Self {
            base: StorageDriver::new(),
            controllers: [
                NvmeController::new(0),
                NvmeController::new(0),
                NvmeController::new(0),
                NvmeController::new(0),
            ],
        }
    }
}

impl Driver for NvmeDriver {
    fn get_info(&self) -> &DriverInfo {
        self.base.get_info()
    }

    fn initialize(&mut self) -> DriverResult<()> {
        self.base.initialize()
    }

    fn cleanup(&mut self) -> DriverResult<()> {
        for controller in &mut self.controllers {
            controller.disable();
        }
        self.base.cleanup()
    }

    fn probe_device(&mut self, device_info: &DeviceInfo) -> bool {
        // Probar si es un dispositivo NVMe
        device_info.device_type == DeviceType::Storage &&
        device_info.class_code == 0x01 && device_info.subclass == 0x08 // Mass storage, NVMe
    }

    fn attach_device(&mut self, device: &mut Device) -> DriverResult<()> {
        self.base.attach_device(device)
    }

    fn detach_device(&mut self, device_id: u32) -> DriverResult<()> {
        self.base.detach_device(device_id)
    }

    fn handle_interrupt(&mut self, device_id: u32) -> DriverResult<()> {
        self.base.handle_interrupt(device_id)
    }
}

// Funciones de inicialización
pub fn init_storage_drivers() -> DriverResult<()> {
    // Inicializar drivers de almacenamiento
    // En una implementación real, esto registraría los drivers con el gestor
    
    Ok(())
}

/// Inicializar gestor de almacenamiento (compatible con main.rs)
pub fn init_storage_manager() {
    // Inicializar gestor de almacenamiento
    // En una implementación real, esto configuraría el gestor global
}

/// Obtener estadísticas de almacenamiento (compatible con main.rs)
pub fn get_storage_statistics() -> (usize, usize, usize) {
    // Estadísticas simplificadas
    let total_storage = 1; // Un dispositivo de almacenamiento
    let ready_storage = 1; // Listo
    let error_storage = 0; // Sin errores
    (total_storage, ready_storage, error_storage)
}