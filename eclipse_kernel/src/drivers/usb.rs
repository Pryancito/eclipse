//! Driver USB para Eclipse OS
//! 
//! Basado en los drivers USB de Redox OS

use crate::drivers::{
    device::{Device, DeviceInfo, DeviceType, DeviceOperations},
    manager::{Driver, DriverInfo, DriverResult, DriverError},
    MAX_DEVICES,
};

// Importar tipos necesarios para no_std
use alloc::vec::Vec;

// Constantes USB
const USB_MAX_DEVICES: u8 = 127;
const USB_MAX_ENDPOINTS: u8 = 16;
const USB_MAX_INTERFACES: u8 = 8;
const USB_MAX_CONFIGURATIONS: u8 = 8;

// Tipos de descriptor USB
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbDescriptorType {
    Device = 0x01,
    Configuration = 0x02,
    String = 0x03,
    Interface = 0x04,
    Endpoint = 0x05,
    DeviceQualifier = 0x06,
    OtherSpeedConfiguration = 0x07,
    InterfacePower = 0x08,
    Otg = 0x09,
    Debug = 0x0A,
    InterfaceAssociation = 0x0B,
}

// Clases de dispositivo USB
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbDeviceClass {
    InterfaceSpecific = 0x00,
    Audio = 0x01,
    Communications = 0x02,
    HID = 0x03,
    Physical = 0x05,
    Image = 0x06,
    Printer = 0x07,
    MassStorage = 0x08,
    Hub = 0x09,
    Data = 0x0A,
    SmartCard = 0x0B,
    ContentSecurity = 0x0D,
    Video = 0x0E,
    PersonalHealthcare = 0x0F,
    AudioVideo = 0x10,
    Billboard = 0x11,
    UsbTypeCBridge = 0x12,
    Miscellaneous = 0xEF,
    VendorSpecific = 0xFF,
}

// Descriptor de dispositivo USB
#[derive(Debug, Clone)]
pub struct UsbDeviceDescriptor {
    pub length: u8,
    pub descriptor_type: UsbDescriptorType,
    pub usb_version: u16,
    pub device_class: UsbDeviceClass,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub max_packet_size: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub device_version: u16,
    pub manufacturer_string: u8,
    pub product_string: u8,
    pub serial_number_string: u8,
    pub num_configurations: u8,
}

impl UsbDeviceDescriptor {
    pub fn new() -> Self {
        Self {
            length: 18,
            descriptor_type: UsbDescriptorType::Device,
            usb_version: 0x0200,
            device_class: UsbDeviceClass::InterfaceSpecific,
            device_subclass: 0,
            device_protocol: 0,
            max_packet_size: 64,
            vendor_id: 0,
            product_id: 0,
            device_version: 0x0100,
            manufacturer_string: 0,
            product_string: 0,
            serial_number_string: 0,
            num_configurations: 1,
        }
    }
}

// Descriptor de configuración USB
#[derive(Debug, Clone)]
pub struct UsbConfigurationDescriptor {
    pub length: u8,
    pub descriptor_type: UsbDescriptorType,
    pub total_length: u16,
    pub num_interfaces: u8,
    pub configuration_value: u8,
    pub configuration_string: u8,
    pub attributes: u8,
    pub max_power: u8,
}

impl UsbConfigurationDescriptor {
    pub fn new() -> Self {
        Self {
            length: 9,
            descriptor_type: UsbDescriptorType::Configuration,
            total_length: 0,
            num_interfaces: 0,
            configuration_value: 1,
            configuration_string: 0,
            attributes: 0x80, // Bus powered
            max_power: 50,    // 100mA
        }
    }
}

// Descriptor de interfaz USB
#[derive(Debug, Clone)]
pub struct UsbInterfaceDescriptor {
    pub length: u8,
    pub descriptor_type: UsbDescriptorType,
    pub interface_number: u8,
    pub alternate_setting: u8,
    pub num_endpoints: u8,
    pub interface_class: UsbDeviceClass,
    pub interface_subclass: u8,
    pub interface_protocol: u8,
    pub interface_string: u8,
}

impl UsbInterfaceDescriptor {
    pub fn new() -> Self {
        Self {
            length: 9,
            descriptor_type: UsbDescriptorType::Interface,
            interface_number: 0,
            alternate_setting: 0,
            num_endpoints: 0,
            interface_class: UsbDeviceClass::InterfaceSpecific,
            interface_subclass: 0,
            interface_protocol: 0,
            interface_string: 0,
        }
    }
}

// Descriptor de endpoint USB
#[derive(Debug, Clone)]
pub struct UsbEndpointDescriptor {
    pub length: u8,
    pub descriptor_type: UsbDescriptorType,
    pub endpoint_address: u8,
    pub attributes: u8,
    pub max_packet_size: u16,
    pub interval: u8,
}

impl UsbEndpointDescriptor {
    pub fn new() -> Self {
        Self {
            length: 7,
            descriptor_type: UsbDescriptorType::Endpoint,
            endpoint_address: 0,
            attributes: 0,
            max_packet_size: 0,
            interval: 0,
        }
    }

    pub fn get_endpoint_number(&self) -> u8 {
        self.endpoint_address & 0x0F
    }

    pub fn is_in_endpoint(&self) -> bool {
        (self.endpoint_address & 0x80) != 0
    }

    pub fn is_out_endpoint(&self) -> bool {
        (self.endpoint_address & 0x80) == 0
    }

    pub fn get_transfer_type(&self) -> u8 {
        self.attributes & 0x03
    }
}

// Información de dispositivo USB
#[derive(Debug, Clone)]
pub struct UsbDeviceInfo {
    pub address: u8,
    pub speed: UsbSpeed,
    pub device_descriptor: UsbDeviceDescriptor,
    pub configurations: Vec<UsbConfigurationDescriptor>,
    pub interfaces: Vec<UsbInterfaceDescriptor>,
    pub endpoints: Vec<UsbEndpointDescriptor>,
    pub is_configured: bool,
    pub is_attached: bool,
    pub parent_hub: Option<u8>,
    pub port_number: u8,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbSpeed {
    LowSpeed = 0,
    FullSpeed = 1,
    HighSpeed = 2,
    SuperSpeed = 3,
}

impl UsbDeviceInfo {
    pub fn new(address: u8) -> Self {
        Self {
            address,
            speed: UsbSpeed::FullSpeed,
            device_descriptor: UsbDeviceDescriptor::new(),
            configurations: Vec::new(),
            interfaces: Vec::new(),
            endpoints: Vec::new(),
            is_configured: false,
            is_attached: false,
            parent_hub: None,
            port_number: 0,
        }
    }
}

// Controlador USB Host
#[derive(Debug, Clone)]
pub struct UsbHostController {
    pub controller_type: UsbControllerType,
    pub base_address: u64,
    pub irq: u8,
    pub is_enabled: bool,
    pub max_devices: u8,
    pub current_devices: u8,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbControllerType {
    OHCI,  // Open Host Controller Interface
    EHCI,  // Enhanced Host Controller Interface
    XHCI,  // eXtensible Host Controller Interface
    UHCI,  // Universal Host Controller Interface
}

impl UsbHostController {
    pub fn new(controller_type: UsbControllerType, base_address: u64, irq: u8) -> Self {
        Self {
            controller_type,
            base_address,
            irq,
            is_enabled: false,
            max_devices: USB_MAX_DEVICES,
            current_devices: 0,
        }
    }

    pub fn enable(&mut self) -> DriverResult<()> {
        self.is_enabled = true;
        Ok(())
    }

    pub fn disable(&mut self) {
        self.is_enabled = false;
    }

    pub fn reset(&mut self) -> DriverResult<()> {
        // Reset del controlador
        Ok(())
    }

    pub fn detect_devices(&mut self) -> DriverResult<()> {
        // Detectar dispositivos USB conectados
        Ok(())
    }
}

// Driver USB base
pub struct UsbDriver {
    pub info: DriverInfo,
    pub devices: [Option<UsbDeviceInfo>; MAX_DEVICES],
    pub device_count: u32,
    pub controllers: [Option<UsbHostController>; 4],
    pub controller_count: u32,
    pub is_initialized: bool,
}

impl UsbDriver {
    pub fn new() -> Self {
        let mut info = DriverInfo::new();
        info.set_name("usb");
        info.device_type = DeviceType::Usb;
        info.version = 1;

        Self {
            info,
            devices: [(); MAX_DEVICES].map(|_| None),
            device_count: 0,
            controllers: [(); 4].map(|_| None),
            controller_count: 0,
            is_initialized: false,
        }
    }

    /// Agregar controlador USB
    pub fn add_controller(&mut self, controller: UsbHostController) -> DriverResult<()> {
        if self.controller_count >= 4 {
            return Err(DriverError::OutOfMemory);
        }

        for i in 0..4 {
            if self.controllers[i].is_none() {
                self.controllers[i] = Some(controller);
                self.controller_count += 1;
                return Ok(());
            }
        }

        Err(DriverError::OutOfMemory)
    }

    /// Agregar dispositivo USB
    pub fn add_device(&mut self, device_info: UsbDeviceInfo) -> DriverResult<()> {
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

    /// Obtener dispositivo USB por dirección
    pub fn get_device(&self, address: u8) -> Option<&UsbDeviceInfo> {
        for i in 0..MAX_DEVICES {
            if let Some(ref device) = self.devices[i] {
                if device.address == address {
                    return Some(device);
                }
            }
        }
        None
    }

    /// Listar dispositivos por clase
    pub fn list_devices_by_class(&self, device_class: UsbDeviceClass) -> Vec<u8> {
        let mut devices = Vec::new();
        for i in 0..MAX_DEVICES {
            if let Some(ref device) = self.devices[i] {
                if device.device_descriptor.device_class == device_class {
                    devices.push(device.address);
                }
            }
        }
        devices
    }

    /// Obtener estadísticas USB
    pub fn get_usb_stats(&self) -> UsbStats {
        let mut stats = UsbStats::new();
        
        for i in 0..MAX_DEVICES {
            if let Some(ref device) = self.devices[i] {
                stats.total_devices += 1;
                
                if device.is_attached {
                    stats.attached_devices += 1;
                }
                
                if device.is_configured {
                    stats.configured_devices += 1;
                }
                
                match device.device_descriptor.device_class {
                    UsbDeviceClass::HID => stats.hid_devices += 1,
                    UsbDeviceClass::MassStorage => stats.storage_devices += 1,
                    UsbDeviceClass::Hub => stats.hub_devices += 1,
                    UsbDeviceClass::Audio => stats.audio_devices += 1,
                    UsbDeviceClass::Video => stats.video_devices += 1,
                    _ => stats.other_devices += 1,
                }
            }
        }
        
        stats.controller_count = self.controller_count;
        stats
    }

    /// Inicializar controladores USB
    pub fn initialize_controllers(&mut self) -> DriverResult<()> {
        for i in 0..4 {
            if let Some(ref mut controller) = self.controllers[i] {
                controller.enable()?;
                controller.reset()?;
                controller.detect_devices()?;
            }
        }
        Ok(())
    }
}

impl Driver for UsbDriver {
    fn get_info(&self) -> &DriverInfo {
        &self.info
    }

    fn initialize(&mut self) -> DriverResult<()> {
        if self.is_initialized {
            return Ok(());
        }

        self.info.is_loaded = true;
        self.initialize_controllers()?;
        self.is_initialized = true;
        
        Ok(())
    }

    fn cleanup(&mut self) -> DriverResult<()> {
        for i in 0..MAX_DEVICES {
            self.devices[i] = None;
        }
        self.device_count = 0;
        
        for i in 0..4 {
            if let Some(ref mut controller) = self.controllers[i] {
                controller.disable();
            }
        }
        self.controller_count = 0;
        
        self.info.is_loaded = false;
        self.is_initialized = false;
        Ok(())
    }

    fn probe_device(&mut self, device_info: &DeviceInfo) -> bool {
        device_info.device_type == DeviceType::Usb
    }

    fn attach_device(&mut self, device: &mut Device) -> DriverResult<()> {
        device.driver_id = Some(self.info.id);
        Ok(())
    }

    fn detach_device(&mut self, _device_id: u32) -> DriverResult<()> {
        Ok(())
    }

    fn handle_interrupt(&mut self, _device_id: u32) -> DriverResult<()> {
        Ok(())
    }
}

// Estadísticas USB
#[derive(Debug, Clone, Copy)]
pub struct UsbStats {
    pub total_devices: u32,
    pub attached_devices: u32,
    pub configured_devices: u32,
    pub controller_count: u32,
    pub hid_devices: u32,
    pub storage_devices: u32,
    pub hub_devices: u32,
    pub audio_devices: u32,
    pub video_devices: u32,
    pub other_devices: u32,
}

impl UsbStats {
    pub fn new() -> Self {
        Self {
            total_devices: 0,
            attached_devices: 0,
            configured_devices: 0,
            controller_count: 0,
            hid_devices: 0,
            storage_devices: 0,
            hub_devices: 0,
            audio_devices: 0,
            video_devices: 0,
            other_devices: 0,
        }
    }
}

// Funciones de inicialización
pub fn init_usb_drivers() -> DriverResult<()> {
    // Inicializar drivers USB
    Ok(())
}
