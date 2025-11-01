/// Driver USB Mass Storage (Bulk-Only Transport)
///
/// Este driver implementa el protocolo USB Mass Storage Class (MSC):
/// - USB Bulk-Only Transport (BOT) especificación 1.0
/// - SCSI Transparent Command Set
/// - Soporta pendrives, discos externos USB, etc.
///
/// Basado en USB Mass Storage Class Spec 1.0

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use alloc::string::String;
use alloc::format;

use crate::drivers::manager::DriverResult;
use crate::drivers::usb_xhci_control::*;

/// Constantes del protocolo Mass Storage
pub const MSC_SUBCLASS_RBC: u8 = 0x01;      // Reduced Block Commands
pub const MSC_SUBCLASS_MMC5: u8 = 0x02;     // CD/DVD
pub const MSC_SUBCLASS_QIC157: u8 = 0x03;   // Tape
pub const MSC_SUBCLASS_UFI: u8 = 0x04;      // Floppy
pub const MSC_SUBCLASS_SFF8070I: u8 = 0x05; // Removable
pub const MSC_SUBCLASS_SCSI: u8 = 0x06;     // SCSI Transparent

/// Protocolos Mass Storage
pub const MSC_PROTOCOL_CBI: u8 = 0x00;      // Control/Bulk/Interrupt
pub const MSC_PROTOCOL_CB: u8 = 0x01;       // Control/Bulk
pub const MSC_PROTOCOL_BOT: u8 = 0x50;      // Bulk-Only Transport

/// Command Block Wrapper (CBW) - 31 bytes
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct CommandBlockWrapper {
    pub signature: u32,          // 'USBC' = 0x43425355
    pub tag: u32,                // Único para cada CBW
    pub data_transfer_length: u32, // Bytes a transferir
    pub flags: u8,               // Bit 7: Dirección (0=OUT, 1=IN)
    pub lun: u8,                 // Logical Unit Number (0-15)
    pub command_length: u8,      // Longitud del comando SCSI (1-16)
    pub command: [u8; 16],       // Comando SCSI
}

impl CommandBlockWrapper {
    /// Signature para CBW
    pub const SIGNATURE: u32 = 0x43425355;  // 'USBC'
    
    /// Crea un nuevo CBW
    pub fn new(tag: u32, data_length: u32, direction_in: bool, lun: u8) -> Self {
        Self {
            signature: Self::SIGNATURE,
            tag,
            data_transfer_length: data_length,
            flags: if direction_in { 0x80 } else { 0x00 },
            lun: lun & 0x0F,
            command_length: 0,
            command: [0; 16],
        }
    }
    
    /// Configura el comando SCSI
    pub fn set_command(&mut self, command: &[u8]) {
        let len = command.len().min(16);
        self.command[..len].copy_from_slice(&command[..len]);
        self.command_length = len as u8;
    }
    
    /// Convierte a bytes
    pub fn to_bytes(&self) -> [u8; 31] {
        unsafe {
            core::mem::transmute_copy(self)
        }
    }
}

/// Command Status Wrapper (CSW) - 13 bytes
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct CommandStatusWrapper {
    pub signature: u32,          // 'USBS' = 0x53425355
    pub tag: u32,                // Debe coincidir con el CBW
    pub data_residue: u32,       // Diferencia entre datos esperados y transferidos
    pub status: u8,              // 0=Success, 1=Failed, 2=Phase Error
}

impl CommandStatusWrapper {
    /// Signature para CSW
    pub const SIGNATURE: u32 = 0x53425355;  // 'USBS'
    
    /// Status codes
    pub const STATUS_PASSED: u8 = 0x00;
    pub const STATUS_FAILED: u8 = 0x01;
    pub const STATUS_PHASE_ERROR: u8 = 0x02;
    
    /// Crea desde bytes
    pub fn from_bytes(bytes: &[u8; 13]) -> Self {
        unsafe {
            core::mem::transmute_copy(bytes)
        }
    }
    
    /// Verifica si el status es exitoso
    pub fn is_success(&self) -> bool {
        self.status == Self::STATUS_PASSED
    }
}

/// Comandos SCSI para Mass Storage
pub mod scsi {
    /// TEST UNIT READY (0x00)
    pub fn test_unit_ready() -> Vec<u8> {
        vec![0x00, 0, 0, 0, 0, 0]
    }
    
    /// REQUEST SENSE (0x03)
    pub fn request_sense(alloc_length: u8) -> Vec<u8> {
        vec![0x03, 0, 0, 0, alloc_length, 0]
    }
    
    /// INQUIRY (0x12) - obtiene información del dispositivo
    pub fn inquiry(alloc_length: u8) -> Vec<u8> {
        vec![0x12, 0, 0, 0, alloc_length, 0]
    }
    
    /// READ CAPACITY (10) (0x25) - obtiene capacidad del dispositivo
    pub fn read_capacity_10() -> Vec<u8> {
        vec![0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    }
    
    /// READ (10) (0x28) - lee sectores
    pub fn read_10(lba: u32, sectors: u16) -> Vec<u8> {
        vec![
            0x28,  // Opcode
            0,     // Flags
            (lba >> 24) as u8,
            (lba >> 16) as u8,
            (lba >> 8) as u8,
            lba as u8,
            0,     // Reserved
            (sectors >> 8) as u8,
            sectors as u8,
            0,     // Control
        ]
    }
    
    /// WRITE (10) (0x2A) - escribe sectores
    pub fn write_10(lba: u32, sectors: u16) -> Vec<u8> {
        vec![
            0x2A,  // Opcode
            0,     // Flags
            (lba >> 24) as u8,
            (lba >> 16) as u8,
            (lba >> 8) as u8,
            lba as u8,
            0,     // Reserved
            (sectors >> 8) as u8,
            sectors as u8,
            0,     // Control
        ]
    }
}

/// Información de dispositivo Mass Storage (desde INQUIRY)
#[derive(Debug, Clone)]
pub struct MassStorageInfo {
    pub device_type: u8,         // 0=Direct Access (disk), etc.
    pub removable: bool,
    pub vendor: String,          // 8 chars
    pub product: String,         // 16 chars
    pub revision: String,        // 4 chars
}

impl MassStorageInfo {
    /// Parsea desde respuesta INQUIRY (36 bytes mínimo)
    pub fn from_inquiry_response(data: &[u8]) -> Option<Self> {
        if data.len() < 36 {
            return None;
        }
        
        let device_type = data[0] & 0x1F;
        let removable = (data[1] & 0x80) != 0;
        
        let vendor = String::from_utf8_lossy(&data[8..16]).trim().to_string();
        let product = String::from_utf8_lossy(&data[16..32]).trim().to_string();
        let revision = String::from_utf8_lossy(&data[32..36]).trim().to_string();
        
        Some(Self {
            device_type,
            removable,
            vendor,
            product,
            revision,
        })
    }
    
    /// Obtiene el tipo de dispositivo como string
    pub fn device_type_string(&self) -> &'static str {
        match self.device_type {
            0x00 => "Direct Access (Disk)",
            0x01 => "Sequential Access (Tape)",
            0x05 => "CD-ROM",
            0x07 => "Optical Memory",
            0x0E => "Simplified Direct Access",
            _ => "Unknown",
        }
    }
}

/// Capacidad del dispositivo (desde READ CAPACITY)
#[derive(Debug, Clone, Copy)]
pub struct DeviceCapacity {
    pub last_lba: u32,           // Último sector válido
    pub block_size: u32,         // Tamaño de bloque en bytes
}

impl DeviceCapacity {
    /// Parsea desde respuesta READ CAPACITY (8 bytes)
    pub fn from_response(data: &[u8]) -> Option<Self> {
        if data.len() < 8 {
            return None;
        }
        
        let last_lba = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let block_size = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        
        Some(Self {
            last_lba,
            block_size,
        })
    }
    
    /// Calcula el número total de bloques
    pub fn total_blocks(&self) -> u32 {
        self.last_lba + 1
    }
    
    /// Calcula el tamaño total en MB
    pub fn total_mb(&self) -> u64 {
        ((self.total_blocks() as u64) * (self.block_size as u64)) / (1024 * 1024)
    }
}

/// Dispositivo Mass Storage
pub struct MassStorageDevice {
    slot_id: u8,
    interface_number: u8,
    endpoint_in: u8,             // Bulk IN
    endpoint_out: u8,            // Bulk OUT
    max_packet_size: u16,
    lun: u8,                     // Logical Unit Number
    tag_counter: u32,            // Para generar tags únicos
    info: Option<MassStorageInfo>,
    capacity: Option<DeviceCapacity>,
}

impl MassStorageDevice {
    /// Crea un nuevo dispositivo Mass Storage
    pub fn new(slot_id: u8, interface_number: u8) -> Self {
        Self {
            slot_id,
            interface_number,
            endpoint_in: 0x81,   // Por defecto
            endpoint_out: 0x01,  // Por defecto
            max_packet_size: 512,
            lun: 0,
            tag_counter: 0,
            info: None,
            capacity: None,
        }
    }
    
    /// Configura endpoints
    pub fn set_endpoints(&mut self, ep_in: u8, ep_out: u8, max_packet: u16) {
        self.endpoint_in = ep_in;
        self.endpoint_out = ep_out;
        self.max_packet_size = max_packet;
    }
    
    /// Genera un tag único
    fn next_tag(&mut self) -> u32 {
        self.tag_counter = self.tag_counter.wrapping_add(1);
        self.tag_counter
    }
    
    /// Obtiene información del dispositivo
    pub fn info(&self) -> Option<&MassStorageInfo> {
        self.info.as_ref()
    }
    
    /// Obtiene capacidad del dispositivo
    pub fn capacity(&self) -> Option<&DeviceCapacity> {
        self.capacity.as_ref()
    }
    
    /// Slot ID
    pub fn slot_id(&self) -> u8 {
        self.slot_id
    }
}

/// Manager de dispositivos Mass Storage
pub struct MassStorageManager {
    devices: Vec<MassStorageDevice>,
}

impl MassStorageManager {
    /// Crea un nuevo manager
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
        }
    }
    
    /// Registra un dispositivo
    pub fn register_device(&mut self, device: MassStorageDevice) {
        crate::debug::serial_write_str(&format!(
            "USB_MSC: Registrando dispositivo Mass Storage (slot={})\n",
            device.slot_id
        ));
        
        self.devices.push(device);
    }
    
    /// Obtiene un dispositivo por índice
    pub fn get_device(&self, index: usize) -> Option<&MassStorageDevice> {
        self.devices.get(index)
    }
    
    /// Obtiene un dispositivo mutable por índice
    pub fn get_device_mut(&mut self, index: usize) -> Option<&mut MassStorageDevice> {
        self.devices.get_mut(index)
    }
    
    /// Número de dispositivos
    pub fn device_count(&self) -> usize {
        self.devices.len()
    }
}

/// Requests específicos para Mass Storage Class
pub mod msc_requests {
    use super::*;
    
    /// Bulk-Only Mass Storage Reset (0xFF)
    pub fn bulk_only_mass_storage_reset(interface: u8) -> SetupPacket {
        SetupPacket::new(
            0x21,  // Class, Interface
            0xFF,  // Bulk-Only Mass Storage Reset
            0,
            interface as u16,
            0,
        )
    }
    
    /// Get Max LUN (0xFE)
    pub fn get_max_lun(interface: u8) -> SetupPacket {
        SetupPacket::new(
            0xA1,  // Class, Interface, Device to Host
            0xFE,  // Get Max LUN
            0,
            interface as u16,
            1,     // 1 byte de respuesta
        )
    }
}
