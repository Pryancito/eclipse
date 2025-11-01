//! Enumeración de dispositivos USB con XHCI
//!
//! Este módulo implementa la enumeración completa de dispositivos USB usando TRBs de comando

use alloc::vec::Vec;
use alloc::string::String;
use alloc::format;
use core::ptr::{read_volatile, write_volatile};
use crate::drivers::usb_xhci_transfer::*;

/// Descriptor de dispositivo USB estándar
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct UsbDeviceDescriptor {
    pub length: u8,
    pub descriptor_type: u8,
    pub usb_version: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub max_packet_size0: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub device_version: u16,
    pub manufacturer_index: u8,
    pub product_index: u8,
    pub serial_number_index: u8,
    pub num_configurations: u8,
}

impl UsbDeviceDescriptor {
    pub fn new() -> Self {
        Self {
            length: 18,
            descriptor_type: 1,
            usb_version: 0,
            device_class: 0,
            device_subclass: 0,
            device_protocol: 0,
            max_packet_size0: 0,
            vendor_id: 0,
            product_id: 0,
            device_version: 0,
            manufacturer_index: 0,
            product_index: 0,
            serial_number_index: 0,
            num_configurations: 0,
        }
    }

    /// Parsea el descriptor desde bytes
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 18 {
            return None;
        }

        Some(Self {
            length: bytes[0],
            descriptor_type: bytes[1],
            usb_version: u16::from_le_bytes([bytes[2], bytes[3]]),
            device_class: bytes[4],
            device_subclass: bytes[5],
            device_protocol: bytes[6],
            max_packet_size0: bytes[7],
            vendor_id: u16::from_le_bytes([bytes[8], bytes[9]]),
            product_id: u16::from_le_bytes([bytes[10], bytes[11]]),
            device_version: u16::from_le_bytes([bytes[12], bytes[13]]),
            manufacturer_index: bytes[14],
            product_index: bytes[15],
            serial_number_index: bytes[16],
            num_configurations: bytes[17],
        })
    }

    /// Describe la clase del dispositivo
    pub fn class_description(&self) -> &'static str {
        match self.device_class {
            0x00 => "Use class information in interfaces",
            0x01 => "Audio",
            0x02 => "Communications (CDC)",
            0x03 => "HID (Human Interface Device)",
            0x05 => "Physical",
            0x06 => "Image",
            0x07 => "Printer",
            0x08 => "Mass Storage",
            0x09 => "Hub",
            0x0A => "CDC-Data",
            0x0B => "Smart Card",
            0x0D => "Content Security",
            0x0E => "Video",
            0x0F => "Personal Healthcare",
            0x10 => "Audio/Video Devices",
            0x11 => "Billboard Device",
            0x12 => "USB Type-C Bridge",
            0xDC => "Diagnostic Device",
            0xE0 => "Wireless Controller",
            0xEF => "Miscellaneous",
            0xFE => "Application Specific",
            0xFF => "Vendor Specific",
            _ => "Unknown",
        }
    }
}

/// Descriptor de configuración USB
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct UsbConfigurationDescriptor {
    pub length: u8,
    pub descriptor_type: u8,
    pub total_length: u16,
    pub num_interfaces: u8,
    pub configuration_value: u8,
    pub configuration_index: u8,
    pub attributes: u8,
    pub max_power: u8,
}

impl UsbConfigurationDescriptor {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 9 {
            return None;
        }

        Some(Self {
            length: bytes[0],
            descriptor_type: bytes[1],
            total_length: u16::from_le_bytes([bytes[2], bytes[3]]),
            num_interfaces: bytes[4],
            configuration_value: bytes[5],
            configuration_index: bytes[6],
            attributes: bytes[7],
            max_power: bytes[8],
        })
    }

    /// Indica si el dispositivo es auto-alimentado
    pub fn is_self_powered(&self) -> bool {
        (self.attributes & 0x40) != 0
    }

    /// Indica si soporta remote wakeup
    pub fn supports_remote_wakeup(&self) -> bool {
        (self.attributes & 0x20) != 0
    }

    /// Obtiene el consumo máximo de energía en mA
    pub fn max_power_ma(&self) -> u16 {
        (self.max_power as u16) * 2
    }
}

/// Descriptor de interfaz USB
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct UsbInterfaceDescriptor {
    pub length: u8,
    pub descriptor_type: u8,
    pub interface_number: u8,
    pub alternate_setting: u8,
    pub num_endpoints: u8,
    pub interface_class: u8,
    pub interface_subclass: u8,
    pub interface_protocol: u8,
    pub interface_index: u8,
}

impl UsbInterfaceDescriptor {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 9 {
            return None;
        }

        Some(Self {
            length: bytes[0],
            descriptor_type: bytes[1],
            interface_number: bytes[2],
            alternate_setting: bytes[3],
            num_endpoints: bytes[4],
            interface_class: bytes[5],
            interface_subclass: bytes[6],
            interface_protocol: bytes[7],
            interface_index: bytes[8],
        })
    }

    /// Describe la clase de la interfaz
    pub fn class_description(&self) -> &'static str {
        match self.interface_class {
            0x01 => "Audio",
            0x02 => "CDC-Control",
            0x03 => "HID",
            0x05 => "Physical",
            0x06 => "Image",
            0x07 => "Printer",
            0x08 => "Mass Storage",
            0x09 => "Hub",
            0x0A => "CDC-Data",
            0x0B => "Smart Card",
            0x0D => "Content Security",
            0x0E => "Video",
            0x0F => "Personal Healthcare",
            0xDC => "Diagnostic",
            0xE0 => "Wireless Controller",
            0xEF => "Miscellaneous",
            0xFE => "Application Specific",
            0xFF => "Vendor Specific",
            _ => "Unknown",
        }
    }
}

/// Descriptor de endpoint USB
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct UsbEndpointDescriptor {
    pub length: u8,
    pub descriptor_type: u8,
    pub endpoint_address: u8,
    pub attributes: u8,
    pub max_packet_size: u16,
    pub interval: u8,
}

impl UsbEndpointDescriptor {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 7 {
            return None;
        }

        Some(Self {
            length: bytes[0],
            descriptor_type: bytes[1],
            endpoint_address: bytes[2],
            attributes: bytes[3],
            max_packet_size: u16::from_le_bytes([bytes[4], bytes[5]]),
            interval: bytes[6],
        })
    }

    /// Obtiene el número de endpoint
    pub fn endpoint_number(&self) -> u8 {
        self.endpoint_address & 0x0F
    }

    /// Indica si es un endpoint de entrada (IN)
    pub fn is_in(&self) -> bool {
        (self.endpoint_address & 0x80) != 0
    }

    /// Obtiene el tipo de transferencia
    pub fn transfer_type(&self) -> EndpointTransferType {
        match self.attributes & 0x03 {
            0 => EndpointTransferType::Control,
            1 => EndpointTransferType::Isochronous,
            2 => EndpointTransferType::Bulk,
            3 => EndpointTransferType::Interrupt,
            _ => EndpointTransferType::Control,
        }
    }
}

/// Tipo de transferencia del endpoint
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointTransferType {
    Control,
    Isochronous,
    Bulk,
    Interrupt,
}

/// Solicitudes estándar de USB
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum UsbStandardRequest {
    GetStatus = 0,
    ClearFeature = 1,
    SetFeature = 3,
    SetAddress = 5,
    GetDescriptor = 6,
    SetDescriptor = 7,
    GetConfiguration = 8,
    SetConfiguration = 9,
    GetInterface = 10,
    SetInterface = 11,
    SynchFrame = 12,
}

/// Tipos de descriptores USB
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum UsbDescriptorType {
    Device = 1,
    Configuration = 2,
    String = 3,
    Interface = 4,
    Endpoint = 5,
    DeviceQualifier = 6,
    OtherSpeedConfiguration = 7,
    InterfacePower = 8,
    OTG = 9,
    Debug = 10,
    InterfaceAssociation = 11,
}

/// Comandos XHCI para enumeración
pub struct XhciEnumerator {
    mmio_base: u64,
    device_context_base_array_addr: u64,
}

impl XhciEnumerator {
    pub fn new(mmio_base: u64) -> Self {
        Self {
            mmio_base,
            device_context_base_array_addr: 0,
        }
    }

    /// Envía un comando Enable Slot
    pub fn enable_slot_command(&self, ring: &mut TransferRing) -> Result<(), &'static str> {
        crate::debug::serial_write_str("XHCI_ENUM: Enviando comando Enable Slot\n");
        
        // Crear TRB de Enable Slot
        let mut trb = Trb::new();
        trb.set_trb_type(TrbType::EnableSlot);
        trb.set_cycle_bit(true);
        
        ring.enqueue(trb)?;
        
        Ok(())
    }

    /// Envía un comando Address Device
    pub fn address_device_command(
        &self,
        ring: &mut TransferRing,
        slot_id: u8,
        input_context_addr: u64,
    ) -> Result<(), &'static str> {
        crate::debug::serial_write_str(&format!(
            "XHCI_ENUM: Enviando comando Address Device para slot {}\n",
            slot_id
        ));
        
        // Crear TRB de Address Device
        let mut trb = Trb::with_values(
            input_context_addr,
            0,
            0,
        );
        
        trb.set_trb_type(TrbType::AddressDevice);
        trb.set_cycle_bit(true);
        
        // El slot ID va en los bits 24-31 del campo control
        let mut control = trb.control;
        control |= (slot_id as u32) << 24;
        trb.control = control;
        
        ring.enqueue(trb)?;
        
        Ok(())
    }

    /// Envía un comando Configure Endpoint
    pub fn configure_endpoint_command(
        &self,
        ring: &mut TransferRing,
        slot_id: u8,
        input_context_addr: u64,
    ) -> Result<(), &'static str> {
        crate::debug::serial_write_str(&format!(
            "XHCI_ENUM: Enviando comando Configure Endpoint para slot {}\n",
            slot_id
        ));
        
        // Crear TRB de Configure Endpoint
        let mut trb = Trb::with_values(
            input_context_addr,
            0,
            0,
        );
        
        trb.set_trb_type(TrbType::ConfigureEndpoint);
        trb.set_cycle_bit(true);
        
        let mut control = trb.control;
        control |= (slot_id as u32) << 24;
        trb.control = control;
        
        ring.enqueue(trb)?;
        
        Ok(())
    }

    /// Obtiene el descriptor de dispositivo
    pub fn get_device_descriptor(
        &self,
        ring: &mut TransferRing,
        buffer_addr: u64,
    ) -> Result<(), &'static str> {
        crate::debug::serial_write_str("XHCI_ENUM: Solicitando descriptor de dispositivo\n");
        
        // Construir la transferencia de control GET_DESCRIPTOR
        ControlTransferBuilder::new(true)
            .setup(
                0x80,  // bmRequestType: Device-to-host, Standard, Device
                UsbStandardRequest::GetDescriptor as u8,
                ((UsbDescriptorType::Device as u16) << 8) | 0, // wValue: Descriptor type + index
                0,     // wIndex
                18,    // wLength: tamaño del descriptor de dispositivo
            )
            .data(buffer_addr, 18, true) // IN transfer
            .status(false, true) // OUT status stage con interrupción
            .build_into(ring)?;
        
        Ok(())
    }

    /// Obtiene el descriptor de configuración
    pub fn get_configuration_descriptor(
        &self,
        ring: &mut TransferRing,
        buffer_addr: u64,
        config_index: u8,
        length: u16,
    ) -> Result<(), &'static str> {
        crate::debug::serial_write_str(&format!(
            "XHCI_ENUM: Solicitando descriptor de configuración {}\n",
            config_index
        ));
        
        ControlTransferBuilder::new(true)
            .setup(
                0x80,  // bmRequestType
                UsbStandardRequest::GetDescriptor as u8,
                ((UsbDescriptorType::Configuration as u16) << 8) | (config_index as u16),
                0,
                length,
            )
            .data(buffer_addr, length as u32, true)
            .status(false, true)
            .build_into(ring)?;
        
        Ok(())
    }

    /// Establece la configuración del dispositivo
    pub fn set_configuration(
        &self,
        ring: &mut TransferRing,
        config_value: u8,
    ) -> Result<(), &'static str> {
        crate::debug::serial_write_str(&format!(
            "XHCI_ENUM: Estableciendo configuración {}\n",
            config_value
        ));
        
        ControlTransferBuilder::new(true)
            .setup(
                0x00,  // bmRequestType: Host-to-device, Standard, Device
                UsbStandardRequest::SetConfiguration as u8,
                config_value as u16,
                0,
                0,  // No data stage
            )
            .status(true, true) // IN status stage
            .build_into(ring)?;
        
        Ok(())
    }

    /// Obtiene un descriptor de string
    pub fn get_string_descriptor(
        &self,
        ring: &mut TransferRing,
        buffer_addr: u64,
        string_index: u8,
        language_id: u16,
        max_length: u16,
    ) -> Result<(), &'static str> {
        crate::debug::serial_write_str(&format!(
            "XHCI_ENUM: Solicitando string descriptor {}\n",
            string_index
        ));
        
        ControlTransferBuilder::new(true)
            .setup(
                0x80,
                UsbStandardRequest::GetDescriptor as u8,
                ((UsbDescriptorType::String as u16) << 8) | (string_index as u16),
                language_id,
                max_length,
            )
            .data(buffer_addr, max_length as u32, true)
            .status(false, true)
            .build_into(ring)?;
        
        Ok(())
    }

    /// Obtiene el estado del dispositivo
    pub fn get_device_status(
        &self,
        ring: &mut TransferRing,
        buffer_addr: u64,
    ) -> Result<(), &'static str> {
        crate::debug::serial_write_str("XHCI_ENUM: Solicitando estado del dispositivo\n");
        
        ControlTransferBuilder::new(true)
            .setup(
                0x80,  // Device-to-host, Standard, Device
                UsbStandardRequest::GetStatus as u8,
                0,
                0,
                2,  // El estado son 2 bytes
            )
            .data(buffer_addr, 2, true)
            .status(false, true)
            .build_into(ring)?;
        
        Ok(())
    }
}

/// Información completa de un dispositivo USB enumerado
pub struct EnumeratedDevice {
    pub slot_id: u8,
    pub port_number: u8,
    pub device_descriptor: UsbDeviceDescriptor,
    pub config_descriptor: Option<UsbConfigurationDescriptor>,
    pub interfaces: Vec<UsbInterfaceDescriptor>,
    pub endpoints: Vec<UsbEndpointDescriptor>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial_number: Option<String>,
}

impl EnumeratedDevice {
    pub fn new(slot_id: u8, port_number: u8) -> Self {
        Self {
            slot_id,
            port_number,
            device_descriptor: UsbDeviceDescriptor::new(),
            config_descriptor: None,
            interfaces: Vec::new(),
            endpoints: Vec::new(),
            manufacturer: None,
            product: None,
            serial_number: None,
        }
    }

    /// Genera un resumen del dispositivo
    pub fn summary(&self) -> String {
        let mut summary = String::new();
        
        summary.push_str(&format!("Device @ Slot {}, Port {}\n", self.slot_id, self.port_number));
        
        // Evitar referencias a campos packed usando copias
        let vendor_id = self.device_descriptor.vendor_id;
        let product_id = self.device_descriptor.product_id;
        summary.push_str(&format!(
            "  VID:PID = {:04X}:{:04X}\n",
            vendor_id,
            product_id
        ));
        let device_class = self.device_descriptor.device_class;
        summary.push_str(&format!(
            "  Class: {} (0x{:02X})\n",
            self.device_descriptor.class_description(),
            device_class
        ));
        
        if let Some(ref mfg) = self.manufacturer {
            summary.push_str(&format!("  Manufacturer: {}\n", mfg));
        }
        
        if let Some(ref prod) = self.product {
            summary.push_str(&format!("  Product: {}\n", prod));
        }
        
        if let Some(ref serial) = self.serial_number {
            summary.push_str(&format!("  Serial: {}\n", serial));
        }
        
        summary.push_str(&format!("  Interfaces: {}\n", self.interfaces.len()));
        summary.push_str(&format!("  Endpoints: {}\n", self.endpoints.len()));
        
        summary
    }
}

