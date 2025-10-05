//! Driver USB Mass Storage para Eclipse OS
//!
//! Implementa soporte completo para dispositivos USB de almacenamiento
//! incluyendo pendrives, discos duros externos y otros dispositivos USB MSC

use crate::drivers::{
    device::{Device, DeviceInfo, DeviceOperations, DeviceType},
    manager::{Driver, DriverError, DriverInfo, DriverResult},
    MAX_DEVICES,
};

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::format;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

// Constantes USB Mass Storage
const USB_MSC_MAX_DEVICES: u8 = 16;
const USB_MSC_BLOCK_SIZE: u32 = 512;
const USB_MSC_MAX_TRANSFER_SIZE: u32 = 65536;

// Códigos de comando SCSI
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScsiCommand {
    TestUnitReady = 0x00,
    RequestSense = 0x03,
    FormatUnit = 0x04,
    Read6 = 0x08,
    Write6 = 0x0A,
    ReadCapacity = 0x25,
    Read10 = 0x28,
    Write10 = 0x2A,
    Read12 = 0xA8,
    Write12 = 0xAA,
    Inquiry = 0x12,
    ModeSense = 0x1A,
    ModeSelect = 0x15,
    PreventAllowMediumRemoval = 0x1E,
    StartStopUnit = 0x1B,
    SynchronizeCache = 0x35,
    ReadToc = 0x43,
    ReadDiscInformation = 0x51,
    GetConfiguration = 0x46,
    GetEventStatusNotification = 0x4A,
}

// Tipos de dispositivo USB Mass Storage
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbMscDeviceType {
    Unknown,
    DirectAccess,        // Disco duro, SSD
    SequentialAccess,    // Unidad de cinta
    Printer,            // Impresora
    Processor,          // Procesador
    WriteOnce,          // Dispositivo de escritura única
    CDROM,              // CD-ROM, DVD-ROM
    Scanner,            // Escáner
    OpticalMemory,      // Dispositivo óptico
    MediumChanger,      // Cambiador de medios
    Communication,      // Dispositivo de comunicación
    Security,           // Dispositivo de seguridad
    WellKnown,          // Dispositivo conocido
    Other,              // Otro tipo
}

// Descriptor de dispositivo USB Mass Storage
#[derive(Debug, Clone)]
pub struct UsbMscDeviceInfo {
    pub device_id: u32,
    pub name: [u8; 64],
    pub vendor_id: u16,
    pub product_id: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub max_lun: u8,
    pub current_lun: u8,
    pub block_size: u32,
    pub total_blocks: u64,
    pub total_capacity: u64,  // En bytes
    pub is_removable: bool,
    pub is_write_protected: bool,
    pub is_ready: bool,
    pub device_type: UsbMscDeviceType,
    pub serial_number: [u8; 32],
    pub firmware_version: [u8; 16],
    pub is_initialized: bool,
}

// Controlador USB Mass Storage
pub struct UsbMscController {
    pub controller_id: u32,
    pub name: [u8; 32],
    pub is_enabled: bool,
    pub devices: Vec<UsbMscDeviceInfo>,
    pub max_devices: u8,
    pub transfer_buffer: [u8; USB_MSC_MAX_TRANSFER_SIZE as usize],
    pub is_busy: bool,
}

impl UsbMscController {
    pub fn new(controller_id: u32) -> Self {
        let mut name = [0u8; 32];
        let name_str = b"USB MSC Controller";
        let copy_len = core::cmp::min(name_str.len(), 31);
        name[..copy_len].copy_from_slice(&name_str[..copy_len]);
        
        Self {
            controller_id,
            name,
            is_enabled: false,
            devices: Vec::new(),
            max_devices: USB_MSC_MAX_DEVICES,
            transfer_buffer: [0u8; USB_MSC_MAX_TRANSFER_SIZE as usize],
            is_busy: false,
        }
    }
    
    /// Habilitar controlador
    pub fn enable(&mut self) -> DriverResult<()> {
        if self.is_enabled {
            return Ok(());
        }
        
        // TODO: Implementar inicialización real del controlador USB MSC
        // Por ahora simulamos la habilitación
        
        self.is_enabled = true;
        Ok(())
    }
    
    /// Deshabilitar controlador
    pub fn disable(&mut self) -> DriverResult<()> {
        if !self.is_enabled {
            return Ok(());
        }
        
        // Limpiar dispositivos
        self.devices.clear();
        self.is_enabled = false;
        Ok(())
    }
    
    /// Detectar dispositivos USB Mass Storage
    pub fn detect_devices(&mut self) -> DriverResult<u32> {
        if !self.is_enabled {
            return Err(DriverError::DeviceNotReady);
        }
        
        // Limpiar dispositivos existentes
        self.devices.clear();
        
        // TODO: Implementar detección real de dispositivos USB MSC
        // Por ahora simulamos algunos dispositivos
        
        // Simular pendrive USB
        let mut pendrive = UsbMscDeviceInfo {
            device_id: 1,
            name: [0u8; 64],
            vendor_id: 0x1234,
            product_id: 0x5678,
            device_class: 0x08,  // Mass Storage
            device_subclass: 0x06,  // SCSI
            device_protocol: 0x50,  // Bulk-Only Transport
            max_lun: 0,
            current_lun: 0,
            block_size: USB_MSC_BLOCK_SIZE,
            total_blocks: 2048000,  // 1GB
            total_capacity: 2048000 * USB_MSC_BLOCK_SIZE as u64,
            is_removable: true,
            is_write_protected: false,
            is_ready: true,
            device_type: UsbMscDeviceType::DirectAccess,
            serial_number: [0u8; 32],
            firmware_version: [0u8; 16],
            is_initialized: false,
        };
        
        // Configurar nombre del pendrive
        let name_str = b"USB Pendrive";
        let copy_len = core::cmp::min(name_str.len(), 63);
        pendrive.name[..copy_len].copy_from_slice(&name_str[..copy_len]);
        
        // Configurar número de serie
        let serial_str = b"1234567890";
        let copy_len = core::cmp::min(serial_str.len(), 31);
        pendrive.serial_number[..copy_len].copy_from_slice(&serial_str[..copy_len]);
        
        // Configurar versión de firmware
        let fw_str = b"1.00";
        let copy_len = core::cmp::min(fw_str.len(), 15);
        pendrive.firmware_version[..copy_len].copy_from_slice(&fw_str[..copy_len]);
        
        self.devices.push(pendrive);
        
        // Simular disco duro externo
        let mut hdd = UsbMscDeviceInfo {
            device_id: 2,
            name: [0u8; 64],
            vendor_id: 0xABCD,
            product_id: 0xEF01,
            device_class: 0x08,  // Mass Storage
            device_subclass: 0x06,  // SCSI
            device_protocol: 0x50,  // Bulk-Only Transport
            max_lun: 0,
            current_lun: 0,
            block_size: USB_MSC_BLOCK_SIZE,
            total_blocks: 10485760,  // 5GB
            total_capacity: 10485760 * USB_MSC_BLOCK_SIZE as u64,
            is_removable: true,
            is_write_protected: false,
            is_ready: true,
            device_type: UsbMscDeviceType::DirectAccess,
            serial_number: [0u8; 32],
            firmware_version: [0u8; 16],
            is_initialized: false,
        };
        
        // Configurar nombre del HDD
        let name_str = b"USB External HDD";
        let copy_len = core::cmp::min(name_str.len(), 63);
        hdd.name[..copy_len].copy_from_slice(&name_str[..copy_len]);
        
        // Configurar número de serie
        let serial_str = b"HDD123456789";
        let copy_len = core::cmp::min(serial_str.len(), 31);
        hdd.serial_number[..copy_len].copy_from_slice(&serial_str[..copy_len]);
        
        // Configurar versión de firmware
        let fw_str = b"2.10";
        let copy_len = core::cmp::min(fw_str.len(), 15);
        hdd.firmware_version[..copy_len].copy_from_slice(&fw_str[..copy_len]);
        
        self.devices.push(hdd);
        
        Ok(self.devices.len() as u32)
    }
    
    /// Inicializar dispositivo USB MSC
    pub fn initialize_device(&mut self, device_id: u32) -> DriverResult<()> {
        if !self.is_enabled {
            return Err(DriverError::DeviceNotReady);
        }
        
        // Buscar dispositivo
        let device_index = self.devices.iter()
            .position(|d| d.device_id == device_id)
            .ok_or(DriverError::DeviceNotFound)?;
        
        if self.devices[device_index].is_initialized {
            return Ok(());
        }
        
        // TODO: Implementar inicialización real del dispositivo
        // Por ahora simulamos la inicialización
        
        // Simular comando INQUIRY
        self.send_scsi_command(device_id, ScsiCommand::Inquiry, &[], &mut [])?;
        
        // Simular comando READ CAPACITY
        let mut capacity_data = [0u8; 8];
        self.send_scsi_command(device_id, ScsiCommand::ReadCapacity, &[], &mut capacity_data)?;
        
        // Simular comando TEST UNIT READY
        self.send_scsi_command(device_id, ScsiCommand::TestUnitReady, &[], &mut [])?;
        
        self.devices[device_index].is_initialized = true;
        self.devices[device_index].is_ready = true;
        
        Ok(())
    }
    
    /// Leer bloques del dispositivo
    pub fn read_blocks(&mut self, device_id: u32, lba: u64, block_count: u32, buffer: &mut [u8]) -> DriverResult<()> {
        if !self.is_enabled {
            return Err(DriverError::DeviceNotReady);
        }
        
        // Buscar dispositivo
        let device = self.devices.iter()
            .find(|d| d.device_id == device_id)
            .ok_or(DriverError::DeviceNotFound)?;
        
        if !device.is_initialized {
            return Err(DriverError::DeviceNotReady);
        }
        
        if !device.is_ready {
            return Err(DriverError::DeviceNotReady);
        }
        
        // Verificar límites
        if lba + block_count as u64 > device.total_blocks {
            return Err(DriverError::InvalidParameter);
        }
        
        // TODO: Implementar lectura real de bloques
        // Por ahora simulamos la lectura
        
        let bytes_to_read = block_count * device.block_size;
        if buffer.len() < bytes_to_read as usize {
            return Err(DriverError::InvalidParameter);
        }
        
        // Simular datos leídos
        for i in 0..bytes_to_read as usize {
            buffer[i] = ((lba as u8 + (i / device.block_size as usize) as u8) ^ 0xAA) as u8;
        }
        
        Ok(())
    }
    
    /// Escribir bloques al dispositivo
    pub fn write_blocks(&mut self, device_id: u32, lba: u64, block_count: u32, buffer: &[u8]) -> DriverResult<()> {
        if !self.is_enabled {
            return Err(DriverError::DeviceNotReady);
        }
        
        // Buscar dispositivo
        let device = self.devices.iter()
            .find(|d| d.device_id == device_id)
            .ok_or(DriverError::DeviceNotFound)?;
        
        if !device.is_initialized {
            return Err(DriverError::DeviceNotReady);
        }
        
        if !device.is_ready {
            return Err(DriverError::DeviceNotReady);
        }
        
        if device.is_write_protected {
            return Err(DriverError::InvalidParameter);
        }
        
        // Verificar límites
        if lba + block_count as u64 > device.total_blocks {
            return Err(DriverError::InvalidParameter);
        }
        
        // TODO: Implementar escritura real de bloques
        // Por ahora simulamos la escritura
        
        let bytes_to_write = block_count * device.block_size;
        if buffer.len() < bytes_to_write as usize {
            return Err(DriverError::InvalidParameter);
        }
        
        // Simular escritura
        // En una implementación real, enviaríamos los datos al dispositivo
        
        Ok(())
    }
    
    /// Enviar comando SCSI
    fn send_scsi_command(&mut self, device_id: u32, command: ScsiCommand, data_in: &[u8], data_out: &mut [u8]) -> DriverResult<()> {
        if !self.is_enabled {
            return Err(DriverError::DeviceNotReady);
        }
        
        if self.is_busy {
            return Err(DriverError::DeviceBusy);
        }
        
        self.is_busy = true;
        
        // TODO: Implementar envío real de comandos SCSI
        // Por ahora simulamos el comando
        
        match command {
            ScsiCommand::Inquiry => {
                // Simular respuesta INQUIRY
                if data_out.len() >= 36 {
                    data_out[0] = 0x00; // Peripheral Device Type
                    data_out[1] = 0x00; // Removable
                    data_out[2] = 0x02; // Version
                    data_out[3] = 0x02; // Response Data Format
                    data_out[4] = 31;   // Additional Length
                    data_out[5] = 0x00; // Reserved
                    data_out[6] = 0x00; // Reserved
                    data_out[7] = 0x00; // Reserved
                    
                    // Vendor Identification
                    let vendor = b"Eclipse";
                    let copy_len = core::cmp::min(vendor.len(), 8);
                    data_out[8..8+copy_len].copy_from_slice(&vendor[..copy_len]);
                    
                    // Product Identification
                    let product = b"USB Storage";
                    let copy_len = core::cmp::min(product.len(), 16);
                    data_out[16..16+copy_len].copy_from_slice(&product[..copy_len]);
                    
                    // Product Revision Level
                    let revision = b"1.00";
                    let copy_len = core::cmp::min(revision.len(), 4);
                    data_out[32..32+copy_len].copy_from_slice(&revision[..copy_len]);
                }
            },
            ScsiCommand::ReadCapacity => {
                // Simular respuesta READ CAPACITY
                if data_out.len() >= 8 {
                    // LBA del último bloque
                    data_out[0] = 0x00;
                    data_out[1] = 0x1F;
                    data_out[2] = 0xFF;
                    data_out[3] = 0xFF;
                    
                    // Tamaño del bloque
                    data_out[4] = 0x00;
                    data_out[5] = 0x00;
                    data_out[6] = 0x02;
                    data_out[7] = 0x00; // 512 bytes
                }
            },
            ScsiCommand::TestUnitReady => {
                // Simular respuesta TEST UNIT READY
                // Comando exitoso si no hay error
            },
            _ => {
                // Otros comandos
            }
        }
        
        self.is_busy = false;
        Ok(())
    }
    
    /// Obtener dispositivos USB MSC
    pub fn get_devices(&self) -> &Vec<UsbMscDeviceInfo> {
        &self.devices
    }
    
    /// Obtener dispositivo por ID
    pub fn get_device(&self, device_id: u32) -> Option<&UsbMscDeviceInfo> {
        self.devices.iter().find(|d| d.device_id == device_id)
    }
    
    /// Verificar si el controlador está ocupado
    pub fn is_busy(&self) -> bool {
        self.is_busy
    }
}

/// Driver USB Mass Storage
pub struct UsbMscDriver {
    pub info: DriverInfo,
    pub controllers: Vec<UsbMscController>,
    pub devices: Vec<UsbMscDeviceInfo>,
    pub is_initialized: bool,
}

impl UsbMscDriver {
    pub fn new() -> Self {
        let mut info = DriverInfo::new();
        info.set_name("usb_mass_storage");
        info.device_type = DeviceType::Storage;
        info.version = 2;
        
        Self {
            info,
            controllers: Vec::new(),
            devices: Vec::new(),
            is_initialized: false,
        }
    }
    
    /// Agregar controlador USB MSC
    pub fn add_controller(&mut self, controller: UsbMscController) -> DriverResult<()> {
        if self.controllers.len() >= 4 {
            return Err(DriverError::OutOfMemory);
        }
        
        self.controllers.push(controller);
        Ok(())
    }
    
    /// Inicializar todos los controladores
    pub fn initialize_all_controllers(&mut self) -> DriverResult<()> {
        for controller in &mut self.controllers {
            controller.enable()?;
            controller.detect_devices()?;
            
            // Agregar dispositivos detectados
            for device in controller.devices.clone() {
                self.devices.push(device);
            }
        }
        
        self.is_initialized = true;
        Ok(())
    }
    
    /// Inicializar dispositivo específico
    pub fn initialize_device(&mut self, device_id: u32) -> DriverResult<()> {
        for controller in &mut self.controllers {
            if controller.is_enabled {
                if let Ok(_) = controller.initialize_device(device_id) {
                    return Ok(());
                }
            }
        }
        Err(DriverError::DeviceNotFound)
    }
    
    /// Leer bloques de dispositivo
    pub fn read_device_blocks(&mut self, device_id: u32, lba: u64, block_count: u32, buffer: &mut [u8]) -> DriverResult<()> {
        for controller in &mut self.controllers {
            if controller.is_enabled {
                if let Ok(_) = controller.read_blocks(device_id, lba, block_count, buffer) {
                    return Ok(());
                }
            }
        }
        Err(DriverError::DeviceNotFound)
    }
    
    /// Escribir bloques a dispositivo
    pub fn write_device_blocks(&mut self, device_id: u32, lba: u64, block_count: u32, buffer: &[u8]) -> DriverResult<()> {
        for controller in &mut self.controllers {
            if controller.is_enabled {
                if let Ok(_) = controller.write_blocks(device_id, lba, block_count, buffer) {
                    return Ok(());
                }
            }
        }
        Err(DriverError::DeviceNotFound)
    }
    
    /// Obtener dispositivos USB MSC
    pub fn get_devices(&self) -> &Vec<UsbMscDeviceInfo> {
        &self.devices
    }
    
    /// Obtener dispositivo por ID
    pub fn get_device(&self, device_id: u32) -> Option<&UsbMscDeviceInfo> {
        self.devices.iter().find(|d| d.device_id == device_id)
    }
    
    /// Obtener dispositivos por tipo
    pub fn get_devices_by_type(&self, device_type: UsbMscDeviceType) -> Vec<&UsbMscDeviceInfo> {
        self.devices.iter()
            .filter(|device| device.device_type == device_type)
            .collect()
    }
    
    /// Obtener dispositivos removibles
    pub fn get_removable_devices(&self) -> Vec<&UsbMscDeviceInfo> {
        self.devices.iter()
            .filter(|device| device.is_removable)
            .collect()
    }
    
    /// Verificar si hay dispositivos conectados
    pub fn has_devices(&self) -> bool {
        !self.devices.is_empty()
    }
    
    /// Obtener estadísticas
    pub fn get_stats(&self) -> String {
        let mut stats = String::new();
        stats.push_str("USB Mass Storage Driver Stats:\n");
        stats.push_str(&format!("Controllers: {}\n", self.controllers.len()));
        stats.push_str(&format!("Devices: {}\n", self.devices.len()));
        
        for device in &self.devices {
            stats.push_str(&format!("  Device {}: {} ({})\n", 
                device.device_id,
                core::str::from_utf8(&device.name[..device.name.iter().position(|&x| x == 0).unwrap_or(device.name.len())]).unwrap_or("Unknown"),
                if device.is_removable { "Removable" } else { "Fixed" }
            ));
        }
        
        stats
    }
}

impl Driver for UsbMscDriver {
    fn get_info(&self) -> &DriverInfo {
        &self.info
    }
    
    fn initialize(&mut self) -> DriverResult<()> {
        if self.is_initialized {
            return Ok(());
        }
        
        self.initialize_all_controllers()?;
        Ok(())
    }
    
    fn cleanup(&mut self) -> DriverResult<()> {
        for controller in &mut self.controllers {
            let _ = controller.disable();
        }
        
        self.controllers.clear();
        self.devices.clear();
        self.is_initialized = false;
        
        Ok(())
    }
    
    fn probe_device(&mut self, device_info: &DeviceInfo) -> bool {
        device_info.device_type == DeviceType::Storage
    }
    
    fn attach_device(&mut self, device: &mut Device) -> DriverResult<()> {
        // TODO: Implementar attach de dispositivo
        Ok(())
    }
    
    fn detach_device(&mut self, device_id: u32) -> DriverResult<()> {
        // TODO: Implementar detach de dispositivo
        Ok(())
    }
    
    fn handle_interrupt(&mut self, _irq: u32) -> DriverResult<()> {
        // TODO: Implementar manejo de interrupciones
        Ok(())
    }
}
