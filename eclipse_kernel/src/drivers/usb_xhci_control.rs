/// Control Transfers para XHCI
/// 
/// Este módulo implementa Control Transfers USB completos para XHCI.
/// Los Control Transfers son la base de la comunicación USB y se usan para:
/// - Leer descriptores de dispositivos
/// - Configurar dispositivos
/// - Enviar comandos específicos del dispositivo

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

use crate::drivers::manager::DriverResult;

/// Setup Packet para Control Transfers (USB Spec 9.3)
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct SetupPacket {
    /// Request Type (bmRequestType)
    /// Bits 0-4: Recipient (0=Device, 1=Interface, 2=Endpoint, 3=Other)
    /// Bits 5-6: Type (0=Standard, 1=Class, 2=Vendor)
    /// Bit 7: Direction (0=Host to Device, 1=Device to Host)
    pub request_type: u8,
    
    /// Request (bRequest) - comando específico
    pub request: u8,
    
    /// Value (wValue) - parámetro del request
    pub value: u16,
    
    /// Index (wIndex) - parámetro del request (ej: interface/endpoint)
    pub index: u16,
    
    /// Length (wLength) - longitud de los datos a transferir
    pub length: u16,
}

impl SetupPacket {
    /// Crea un nuevo Setup Packet
    pub fn new(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> Self {
        Self {
            request_type,
            request,
            value,
            index,
            length,
        }
    }
    
    /// GET_DESCRIPTOR request (request 6)
    pub fn get_descriptor(descriptor_type: u8, descriptor_index: u8, language_id: u16, length: u16) -> Self {
        Self::new(
            0x80,  // Device to Host, Standard, Device
            6,     // GET_DESCRIPTOR
            ((descriptor_type as u16) << 8) | (descriptor_index as u16),
            language_id,
            length,
        )
    }
    
    /// SET_ADDRESS request (request 5)
    pub fn set_address(address: u8) -> Self {
        Self::new(
            0x00,  // Host to Device, Standard, Device
            5,     // SET_ADDRESS
            address as u16,
            0,
            0,
        )
    }
    
    /// SET_CONFIGURATION request (request 9)
    pub fn set_configuration(config_value: u8) -> Self {
        Self::new(
            0x00,  // Host to Device, Standard, Device
            9,     // SET_CONFIGURATION
            config_value as u16,
            0,
            0,
        )
    }
    
    /// GET_STATUS request (request 0)
    pub fn get_status(recipient: u8, index: u16) -> Self {
        Self::new(
            0x80 | recipient,  // Device to Host, Standard, [recipient]
            0,                 // GET_STATUS
            0,
            index,
            2,  // Status es 2 bytes
        )
    }
    
    /// Convierte a array de bytes (para MMIO)
    pub fn to_bytes(&self) -> [u8; 8] {
        unsafe {
            core::mem::transmute_copy(self)
        }
    }
}

/// Control Transfer Context
pub struct ControlTransfer {
    setup: SetupPacket,
    data_buffer: Option<Vec<u8>>,
    status: TransferStatus,
}

/// Estado de un transfer
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransferStatus {
    Pending,
    InProgress,
    Completed,
    Failed(u32),  // Código de error
}

impl ControlTransfer {
    /// Crea un nuevo Control Transfer
    pub fn new(setup: SetupPacket) -> Self {
        let data_len = setup.length as usize;
        let data_buffer = if data_len > 0 {
            Some(vec![0u8; data_len])
        } else {
            None
        };
        
        Self {
            setup,
            data_buffer,
            status: TransferStatus::Pending,
        }
    }
    
    /// Crea Control Transfer para GET_DESCRIPTOR
    pub fn get_descriptor(descriptor_type: u8, descriptor_index: u8, length: u16) -> Self {
        let setup = SetupPacket::get_descriptor(descriptor_type, descriptor_index, 0, length);
        Self::new(setup)
    }
    
    /// Obtiene el setup packet
    pub fn setup_packet(&self) -> &SetupPacket {
        &self.setup
    }
    
    /// Obtiene el buffer de datos (mutable)
    pub fn data_buffer_mut(&mut self) -> Option<&mut Vec<u8>> {
        self.data_buffer.as_mut()
    }
    
    /// Obtiene el buffer de datos (inmutable)
    pub fn data_buffer(&self) -> Option<&Vec<u8>> {
        self.data_buffer.as_ref()
    }
    
    /// Marca el transfer como completado
    pub fn complete(&mut self) {
        self.status = TransferStatus::Completed;
    }
    
    /// Marca el transfer como fallido
    pub fn fail(&mut self, error_code: u32) {
        self.status = TransferStatus::Failed(error_code);
    }
    
    /// Obtiene el estado actual
    pub fn status(&self) -> TransferStatus {
        self.status
    }
    
    /// Verifica si está completado
    pub fn is_completed(&self) -> bool {
        self.status == TransferStatus::Completed
    }
}

/// Setup TRB para XHCI (Control Transfer Setup Stage)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SetupTrb {
    /// Setup Data (8 bytes del Setup Packet)
    pub setup_data: u64,
    
    /// Status (TRB Transfer Length)
    pub status: u32,
    
    /// Control (flags y tipo de TRB)
    pub control: u32,
}

impl SetupTrb {
    /// Crea un Setup TRB a partir de un Setup Packet
    pub fn from_setup_packet(setup: &SetupPacket, cycle_bit: bool) -> Self {
        let setup_bytes = setup.to_bytes();
        let setup_data = u64::from_le_bytes(setup_bytes);
        
        // Control field:
        // Bits 0: Cycle bit
        // Bits 6-7: TRT (Transfer Type) = 3 (No Data Stage si length=0)
        // Bits 10-15: TRB Type = 2 (Setup Stage)
        let trt = if setup.length == 0 {
            0  // No Data Stage
        } else if setup.request_type & 0x80 != 0 {
            3  // IN Data Stage
        } else {
            2  // OUT Data Stage
        };
        
        let control = (cycle_bit as u32) |
                     (1u32 << 5) |           // IDT (Immediate Data)
                     (trt << 16) |           // TRT
                     (2u32 << 10);           // TRB Type = 2 (Setup)
        
        Self {
            setup_data,
            status: 8,  // Transfer Length = 8 bytes (tamaño del setup packet)
            control,
        }
    }
    
    /// Convierte a u128 para enqueue
    pub fn to_u128(&self) -> u128 {
        (self.setup_data as u128) |
        ((self.status as u128) << 64) |
        ((self.control as u128) << 96)
    }
}

/// Data Stage TRB para XHCI (Control Transfer Data Stage)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DataStageTrb {
    /// Data Buffer Pointer (dirección física del buffer)
    pub data_buffer: u64,
    
    /// Status
    pub status: u32,
    
    /// Control
    pub control: u32,
}

impl DataStageTrb {
    /// Crea un Data Stage TRB
    pub fn new(buffer_addr: u64, length: u16, direction_in: bool, cycle_bit: bool) -> Self {
        // Control field:
        // Bit 0: Cycle bit
        // Bit 16: DIR (0=OUT, 1=IN)
        // Bits 10-15: TRB Type = 3 (Data Stage)
        let control = (cycle_bit as u32) |
                     ((direction_in as u32) << 16) |
                     (3u32 << 10);  // TRB Type = 3
        
        Self {
            data_buffer: buffer_addr,
            status: length as u32,
            control,
        }
    }
    
    /// Convierte a u128
    pub fn to_u128(&self) -> u128 {
        (self.data_buffer as u128) |
        ((self.status as u128) << 64) |
        ((self.control as u128) << 96)
    }
}

/// Status Stage TRB para XHCI (Control Transfer Status Stage)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct StatusStageTrb {
    /// Reserved
    pub reserved: u64,
    
    /// Status
    pub status: u32,
    
    /// Control
    pub control: u32,
}

impl StatusStageTrb {
    /// Crea un Status Stage TRB
    pub fn new(direction_in: bool, cycle_bit: bool) -> Self {
        // Control field:
        // Bit 0: Cycle bit
        // Bit 5: IOC (Interrupt on Completion)
        // Bit 16: DIR (inverso al data stage)
        // Bits 10-15: TRB Type = 4 (Status Stage)
        let control = (cycle_bit as u32) |
                     (1u32 << 5) |                      // IOC
                     ((!direction_in as u32) << 16) |   // DIR invertido
                     (4u32 << 10);                      // TRB Type = 4
        
        Self {
            reserved: 0,
            status: 0,
            control,
        }
    }
    
    /// Convierte a u128
    pub fn to_u128(&self) -> u128 {
        (self.reserved as u128) |
        ((self.status as u128) << 64) |
        ((self.control as u128) << 96)
    }
}

/// Ejecutor de Control Transfers
pub struct ControlTransferExecutor;

impl ControlTransferExecutor {
    /// Prepara TRBs para un Control Transfer completo
    pub fn prepare_trbs(transfer: &ControlTransfer, cycle_bit: bool) -> Vec<u128> {
        let mut trbs = Vec::new();
        
        // 1. Setup Stage TRB
        let setup_trb = SetupTrb::from_setup_packet(transfer.setup_packet(), cycle_bit);
        trbs.push(setup_trb.to_u128());
        
        // 2. Data Stage TRB (si hay datos)
        if let Some(buffer) = transfer.data_buffer() {
            if buffer.len() > 0 {
                let buffer_addr = buffer.as_ptr() as u64;
                let direction_in = transfer.setup_packet().request_type & 0x80 != 0;
                let data_trb = DataStageTrb::new(
                    buffer_addr,
                    buffer.len() as u16,
                    direction_in,
                    cycle_bit
                );
                trbs.push(data_trb.to_u128());
            }
        }
        
        // 3. Status Stage TRB
        let direction_in = transfer.setup_packet().request_type & 0x80 != 0;
        let status_trb = StatusStageTrb::new(direction_in, cycle_bit);
        trbs.push(status_trb.to_u128());
        
        trbs
    }
}

/// Constantes para Control Transfers
pub mod constants {
    // Request Types (bmRequestType)
    pub const HOST_TO_DEVICE: u8 = 0x00;
    pub const DEVICE_TO_HOST: u8 = 0x80;
    pub const STANDARD_REQUEST: u8 = 0x00;
    pub const CLASS_REQUEST: u8 = 0x20;
    pub const VENDOR_REQUEST: u8 = 0x40;
    
    // Recipients
    pub const DEVICE: u8 = 0x00;
    pub const INTERFACE: u8 = 0x01;
    pub const ENDPOINT: u8 = 0x02;
    pub const OTHER: u8 = 0x03;
    
    // Standard Requests (bRequest)
    pub const GET_STATUS: u8 = 0;
    pub const CLEAR_FEATURE: u8 = 1;
    pub const SET_FEATURE: u8 = 3;
    pub const SET_ADDRESS: u8 = 5;
    pub const GET_DESCRIPTOR: u8 = 6;
    pub const SET_DESCRIPTOR: u8 = 7;
    pub const GET_CONFIGURATION: u8 = 8;
    pub const SET_CONFIGURATION: u8 = 9;
    pub const GET_INTERFACE: u8 = 10;
    pub const SET_INTERFACE: u8 = 11;
    pub const SYNCH_FRAME: u8 = 12;
    
    // Descriptor Types
    pub const DEVICE_DESCRIPTOR: u8 = 1;
    pub const CONFIGURATION_DESCRIPTOR: u8 = 2;
    pub const STRING_DESCRIPTOR: u8 = 3;
    pub const INTERFACE_DESCRIPTOR: u8 = 4;
    pub const ENDPOINT_DESCRIPTOR: u8 = 5;
    pub const DEVICE_QUALIFIER: u8 = 6;
    pub const OTHER_SPEED_CONFIGURATION: u8 = 7;
    pub const INTERFACE_POWER: u8 = 8;
    pub const HID_DESCRIPTOR: u8 = 0x21;
    pub const HID_REPORT_DESCRIPTOR: u8 = 0x22;
}

