#![no_std]

use core::ptr;
use alloc::vec::Vec;
use alloc::string::String;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::boxed::Box;

/// Protocolo HID (Human Interface Device) completo para Eclipse OS
/// Implementa descriptores HID, reportes, y gestión de dispositivos

/// Tipos de descriptores HID
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HidDescriptorType {
    Hid = 0x21,
    Report = 0x22,
    Physical = 0x23,
}

/// Tipos de reportes HID
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HidReportType {
    Input = 0x01,
    Output = 0x02,
    Feature = 0x03,
}

/// Tipos de datos HID
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HidDataType {
    Main = 0x00,
    Global = 0x01,
    Local = 0x02,
}

/// Páginas de uso HID
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HidUsagePage {
    GenericDesktop = 0x01,
    SimulationControls = 0x02,
    VrControls = 0x03,
    SportControls = 0x04,
    GameControls = 0x05,
    GenericDeviceControls = 0x06,
    Keyboard = 0x07,
    Led = 0x08,
    Button = 0x09,
    Ordinal = 0x0A,
    Telephony = 0x0B,
    Consumer = 0x0C,
    Digitizer = 0x0D,
    Haptics = 0x0E,
    PhysicalInputDevice = 0x0F,
    Unicode = 0x10,
    So = 0x11,
    AlphanumericDisplay = 0x14,
    MedicalInstrument = 0x40,
    Monitor = 0x80,
    MonitorEnumerated = 0x81,
    MonitorVirtual = 0x82,
    MonitorReserved = 0x83,
    PowerDevice = 0x84,
    BatterySystem = 0x85,
    PowerReserved = 0x86,
    PowerReserved2 = 0x87,
    PowerReserved3 = 0x88,
    PowerReserved4 = 0x89,
    PowerReserved5 = 0x8A,
    PowerReserved6 = 0x8B,
    PowerReserved7 = 0x8C,
    PowerReserved8 = 0x8D,
    PowerReserved9 = 0x8E,
    PowerReserved10 = 0x8F,
    PowerReserved11 = 0x90,
    PowerReserved12 = 0x91,
    PowerReserved13 = 0x92,
    PowerReserved14 = 0x93,
    PowerReserved15 = 0x94,
    PowerReserved16 = 0x95,
    PowerReserved17 = 0x96,
    PowerReserved18 = 0x97,
    PowerReserved19 = 0x98,
    PowerReserved20 = 0x99,
    PowerReserved21 = 0x9A,
    PowerReserved22 = 0x9B,
    PowerReserved23 = 0x9C,
    PowerReserved24 = 0x9D,
    PowerReserved25 = 0x9E,
    PowerReserved26 = 0x9F,
    PowerReserved27 = 0xA0,
    PowerReserved28 = 0xA1,
    PowerReserved29 = 0xA2,
    PowerReserved30 = 0xA3,
    PowerReserved31 = 0xA4,
    PowerReserved32 = 0xA5,
    PowerReserved33 = 0xA6,
    PowerReserved34 = 0xA7,
    PowerReserved35 = 0xA8,
    PowerReserved36 = 0xA9,
    PowerReserved37 = 0xAA,
    PowerReserved38 = 0xAB,
    PowerReserved39 = 0xAC,
    PowerReserved40 = 0xAD,
    PowerReserved41 = 0xAE,
    PowerReserved42 = 0xAF,
    PowerReserved43 = 0xB0,
    PowerReserved44 = 0xB1,
    PowerReserved45 = 0xB2,
    PowerReserved46 = 0xB3,
    PowerReserved47 = 0xB4,
    PowerReserved48 = 0xB5,
    PowerReserved49 = 0xB6,
    PowerReserved50 = 0xB7,
    PowerReserved51 = 0xB8,
    PowerReserved52 = 0xB9,
    PowerReserved53 = 0xBA,
    PowerReserved54 = 0xBB,
    PowerReserved55 = 0xBC,
    PowerReserved56 = 0xBD,
    PowerReserved57 = 0xBE,
    PowerReserved58 = 0xBF,
    PowerReserved59 = 0xC0,
    PowerReserved60 = 0xC1,
    PowerReserved61 = 0xC2,
    PowerReserved62 = 0xC3,
    PowerReserved63 = 0xC4,
    PowerReserved64 = 0xC5,
    PowerReserved65 = 0xC6,
    PowerReserved67 = 0xC7,
    PowerReserved68 = 0xC8,
    PowerReserved69 = 0xC9,
    PowerReserved70 = 0xCA,
    PowerReserved71 = 0xCB,
    PowerReserved72 = 0xCC,
    PowerReserved73 = 0xCD,
    PowerReserved74 = 0xCE,
    PowerReserved75 = 0xCF,
    PowerReserved76 = 0xD0,
    PowerReserved77 = 0xD1,
    PowerReserved78 = 0xD2,
    PowerReserved79 = 0xD3,
    PowerReserved80 = 0xD4,
    PowerReserved81 = 0xD5,
    PowerReserved82 = 0xD6,
    PowerReserved83 = 0xD7,
    PowerReserved84 = 0xD8,
    PowerReserved85 = 0xD9,
    PowerReserved86 = 0xDA,
    PowerReserved87 = 0xDB,
    PowerReserved88 = 0xDC,
    PowerReserved89 = 0xDD,
    PowerReserved90 = 0xDE,
    PowerReserved91 = 0xDF,
    PowerReserved92 = 0xE0,
    PowerReserved93 = 0xE1,
    PowerReserved94 = 0xE2,
    PowerReserved95 = 0xE3,
    PowerReserved96 = 0xE4,
    PowerReserved97 = 0xE5,
    PowerReserved98 = 0xE6,
    PowerReserved99 = 0xE7,
    PowerReserved100 = 0xE8,
    PowerReserved101 = 0xE9,
    PowerReserved102 = 0xEA,
    PowerReserved103 = 0xEB,
    PowerReserved104 = 0xEC,
    PowerReserved105 = 0xED,
    PowerReserved106 = 0xEE,
    PowerReserved107 = 0xEF,
    PowerReserved108 = 0xF0,
    PowerReserved109 = 0xF1,
    PowerReserved110 = 0xF2,
    PowerReserved111 = 0xF3,
    PowerReserved112 = 0xF4,
    PowerReserved113 = 0xF5,
    PowerReserved114 = 0xF6,
    PowerReserved115 = 0xF7,
    PowerReserved116 = 0xF8,
    PowerReserved117 = 0xF9,
    PowerReserved118 = 0xFA,
    PowerReserved119 = 0xFB,
    PowerReserved120 = 0xFC,
    PowerReserved121 = 0xFD,
    PowerReserved122 = 0xFE,
    PowerReserved123 = 0xFF,
}

/// Descriptor HID principal
#[derive(Debug, Clone)]
pub struct HidDescriptor {
    pub length: u8,
    pub descriptor_type: HidDescriptorType,
    pub hid_version: u16,
    pub country_code: u8,
    pub num_descriptors: u8,
    pub report_descriptor_type: u8,
    pub report_descriptor_length: u16,
}

impl HidDescriptor {
    pub fn new() -> Self {
        Self {
            length: 9,
            descriptor_type: HidDescriptorType::Hid,
            hid_version: 0x0111, // HID 1.11
            country_code: 0x00,  // No country code
            num_descriptors: 1,
            report_descriptor_type: HidDescriptorType::Report as u8,
            report_descriptor_length: 0,
        }
    }
    
    /// Convertir a bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = alloc::vec::Vec::new();
        bytes.push(self.length);
        bytes.push(self.descriptor_type as u8);
        bytes.push((self.hid_version & 0xFF) as u8);
        bytes.push(((self.hid_version >> 8) & 0xFF) as u8);
        bytes.push(self.country_code);
        bytes.push(self.num_descriptors);
        bytes.push(self.report_descriptor_type);
        bytes.push((self.report_descriptor_length & 0xFF) as u8);
        bytes.push(((self.report_descriptor_length >> 8) & 0xFF) as u8);
        bytes
    }
}

/// Item de descriptor HID
#[derive(Debug, Clone)]
pub struct HidDescriptorItem {
    pub tag: u8,
    pub data: Vec<u8>,
}

impl HidDescriptorItem {
    pub fn new(tag: u8, data: Vec<u8>) -> Self {
        Self { tag, data }
    }
    
    /// Crear item de página de uso
    pub fn usage_page(usage_page: HidUsagePage) -> Self {
        Self::new(0x05, alloc::vec::Vec::from([usage_page as u8]))
    }
    
    /// Crear item de uso
    pub fn usage(usage: u16) -> Self {
        if usage <= 0xFF {
            Self::new(0x09, alloc::vec::Vec::from([usage as u8]))
        } else {
            Self::new(0x09, alloc::vec::Vec::from([(usage & 0xFF) as u8, ((usage >> 8) & 0xFF) as u8]))
        }
    }
    
    /// Crear item de colección
    pub fn collection(collection_type: u8) -> Self {
        Self::new(0xA1, alloc::vec::Vec::from([collection_type]))
    }
    
    /// Crear item de fin de colección
    pub fn end_collection() -> Self {
        Self::new(0xC0, alloc::vec::Vec::from([]))
    }
    
    /// Crear item de entrada
    pub fn input(input_bits: u32) -> Self {
        let mut data = Vec::new();
        if input_bits <= 0xFF {
            data.push(input_bits as u8);
        } else if input_bits <= 0xFFFF {
            data.push((input_bits & 0xFF) as u8);
            data.push(((input_bits >> 8) & 0xFF) as u8);
        } else {
            data.push((input_bits & 0xFF) as u8);
            data.push(((input_bits >> 8) & 0xFF) as u8);
            data.push(((input_bits >> 16) & 0xFF) as u8);
            data.push(((input_bits >> 24) & 0xFF) as u8);
        }
        Self::new(0x81, data)
    }
    
    /// Crear item de salida
    pub fn output(output_bits: u32) -> Self {
        let mut data = Vec::new();
        if output_bits <= 0xFF {
            data.push(output_bits as u8);
        } else if output_bits <= 0xFFFF {
            data.push((output_bits & 0xFF) as u8);
            data.push(((output_bits >> 8) & 0xFF) as u8);
        } else {
            data.push((output_bits & 0xFF) as u8);
            data.push(((output_bits >> 8) & 0xFF) as u8);
            data.push(((output_bits >> 16) & 0xFF) as u8);
            data.push(((output_bits >> 24) & 0xFF) as u8);
        }
        Self::new(0x91, data)
    }
    
    /// Crear item de característica
    pub fn feature(feature_bits: u32) -> Self {
        let mut data = Vec::new();
        if feature_bits <= 0xFF {
            data.push(feature_bits as u8);
        } else if feature_bits <= 0xFFFF {
            data.push((feature_bits & 0xFF) as u8);
            data.push(((feature_bits >> 8) & 0xFF) as u8);
        } else {
            data.push((feature_bits & 0xFF) as u8);
            data.push(((feature_bits >> 8) & 0xFF) as u8);
            data.push(((feature_bits >> 16) & 0xFF) as u8);
            data.push(((feature_bits >> 24) & 0xFF) as u8);
        }
        Self::new(0xB1, data)
    }
    
    /// Convertir a bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = alloc::vec::Vec::new();
        bytes.push(self.tag);
        bytes.extend_from_slice(&self.data);
        bytes
    }
}

/// Descriptor de reporte HID
#[derive(Debug, Clone)]
pub struct HidReportDescriptor {
    pub items: Vec<HidDescriptorItem>,
}

impl HidReportDescriptor {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }
    
    /// Crear descriptor para teclado
    pub fn create_keyboard_descriptor() -> Self {
        let mut descriptor = Self::new();
        
        // Usage Page (Generic Desktop)
        descriptor.items.push(HidDescriptorItem::usage_page(HidUsagePage::GenericDesktop));
        
        // Usage (Keyboard)
        descriptor.items.push(HidDescriptorItem::usage(0x06));
        
        // Collection (Application)
        descriptor.items.push(HidDescriptorItem::collection(0x01));
        
        // Usage Page (Keyboard)
        descriptor.items.push(HidDescriptorItem::usage_page(HidUsagePage::Keyboard));
        
        // Usage Minimum (0x00)
        descriptor.items.push(HidDescriptorItem::usage(0x00));
        
        // Usage Maximum (0xE7)
        descriptor.items.push(HidDescriptorItem::usage(0xE7));
        
        // Logical Minimum (0)
        descriptor.items.push(HidDescriptorItem::new(0x15, alloc::vec::Vec::from([0x00])));
        
        // Logical Maximum (1)
        descriptor.items.push(HidDescriptorItem::new(0x25, alloc::vec::Vec::from([0x01])));
        
        // Report Count (8)
        descriptor.items.push(HidDescriptorItem::new(0x95, alloc::vec::Vec::from([0x08])));
        
        // Report Size (1)
        descriptor.items.push(HidDescriptorItem::new(0x75, alloc::vec::Vec::from([0x01])));
        
        // Input (Data, Variable, Absolute) -- Modifier byte
        descriptor.items.push(HidDescriptorItem::input(0x02));
        
        // Report Count (1)
        descriptor.items.push(HidDescriptorItem::new(0x95, alloc::vec::Vec::from([0x01])));
        
        // Report Size (8)
        descriptor.items.push(HidDescriptorItem::new(0x75, alloc::vec::Vec::from([0x08])));
        
        // Input (Constant) -- Reserved byte
        descriptor.items.push(HidDescriptorItem::input(0x01));
        
        // Report Count (5)
        descriptor.items.push(HidDescriptorItem::new(0x95, alloc::vec::Vec::from([0x05])));
        
        // Report Size (1)
        descriptor.items.push(HidDescriptorItem::new(0x75, alloc::vec::Vec::from([0x01])));
        
        // Usage Page (LEDs)
        descriptor.items.push(HidDescriptorItem::usage_page(HidUsagePage::Led));
        
        // Usage Minimum (1)
        descriptor.items.push(HidDescriptorItem::usage(0x01));
        
        // Usage Maximum (5)
        descriptor.items.push(HidDescriptorItem::usage(0x05));
        
        // Output (Data, Variable, Absolute) -- LED report
        descriptor.items.push(HidDescriptorItem::output(0x02));
        
        // Report Count (1)
        descriptor.items.push(HidDescriptorItem::new(0x95, alloc::vec::Vec::from([0x01])));
        
        // Report Size (3)
        descriptor.items.push(HidDescriptorItem::new(0x75, alloc::vec::Vec::from([0x03])));
        
        // Output (Constant) -- LED report padding
        descriptor.items.push(HidDescriptorItem::output(0x01));
        
        // Report Count (6)
        descriptor.items.push(HidDescriptorItem::new(0x95, alloc::vec::Vec::from([0x06])));
        
        // Report Size (8)
        descriptor.items.push(HidDescriptorItem::new(0x75, alloc::vec::Vec::from([0x08])));
        
        // Logical Minimum (0)
        descriptor.items.push(HidDescriptorItem::new(0x15, alloc::vec::Vec::from([0x00])));
        
        // Logical Maximum (101)
        descriptor.items.push(HidDescriptorItem::new(0x25, alloc::vec::Vec::from([0x65])));
        
        // Usage Page (Keyboard)
        descriptor.items.push(HidDescriptorItem::usage_page(HidUsagePage::Keyboard));
        
        // Usage Minimum (0)
        descriptor.items.push(HidDescriptorItem::usage(0x00));
        
        // Usage Maximum (101)
        descriptor.items.push(HidDescriptorItem::usage(0x65));
        
        // Input (Data, Array) -- Key array
        descriptor.items.push(HidDescriptorItem::input(0x00));
        
        // End Collection
        descriptor.items.push(HidDescriptorItem::end_collection());
        
        descriptor
    }
    
    /// Crear descriptor para mouse
    pub fn create_mouse_descriptor() -> Self {
        let mut descriptor = Self::new();
        
        // Usage Page (Generic Desktop)
        descriptor.items.push(HidDescriptorItem::usage_page(HidUsagePage::GenericDesktop));
        
        // Usage (Mouse)
        descriptor.items.push(HidDescriptorItem::usage(0x02));
        
        // Collection (Application)
        descriptor.items.push(HidDescriptorItem::collection(0x01));
        
        // Usage (Pointer)
        descriptor.items.push(HidDescriptorItem::usage(0x01));
        
        // Collection (Physical)
        descriptor.items.push(HidDescriptorItem::collection(0x00));
        
        // Usage Page (Button)
        descriptor.items.push(HidDescriptorItem::usage_page(HidUsagePage::Button));
        
        // Usage Minimum (1)
        descriptor.items.push(HidDescriptorItem::usage(0x01));
        
        // Usage Maximum (3)
        descriptor.items.push(HidDescriptorItem::usage(0x03));
        
        // Logical Minimum (0)
        descriptor.items.push(HidDescriptorItem::new(0x15, alloc::vec::Vec::from([0x00])));
        
        // Logical Maximum (1)
        descriptor.items.push(HidDescriptorItem::new(0x25, alloc::vec::Vec::from([0x01])));
        
        // Report Count (3)
        descriptor.items.push(HidDescriptorItem::new(0x95, alloc::vec::Vec::from([0x03])));
        
        // Report Size (1)
        descriptor.items.push(HidDescriptorItem::new(0x75, alloc::vec::Vec::from([0x01])));
        
        // Input (Data, Variable, Absolute) -- Buttons
        descriptor.items.push(HidDescriptorItem::input(0x02));
        
        // Report Count (1)
        descriptor.items.push(HidDescriptorItem::new(0x95, alloc::vec::Vec::from([0x01])));
        
        // Report Size (5)
        descriptor.items.push(HidDescriptorItem::new(0x75, alloc::vec::Vec::from([0x05])));
        
        // Input (Constant) -- Padding
        descriptor.items.push(HidDescriptorItem::input(0x01));
        
        // Usage Page (Generic Desktop)
        descriptor.items.push(HidDescriptorItem::usage_page(HidUsagePage::GenericDesktop));
        
        // Usage (X)
        descriptor.items.push(HidDescriptorItem::usage(0x30));
        
        // Usage (Y)
        descriptor.items.push(HidDescriptorItem::usage(0x31));
        
        // Usage (Wheel)
        descriptor.items.push(HidDescriptorItem::usage(0x38));
        
        // Logical Minimum (-127)
        descriptor.items.push(HidDescriptorItem::new(0x15, alloc::vec::Vec::from([0x81])));
        
        // Logical Maximum (127)
        descriptor.items.push(HidDescriptorItem::new(0x25, alloc::vec::Vec::from([0x7F])));
        
        // Report Size (8)
        descriptor.items.push(HidDescriptorItem::new(0x75, alloc::vec::Vec::from([0x08])));
        
        // Report Count (3)
        descriptor.items.push(HidDescriptorItem::new(0x95, alloc::vec::Vec::from([0x03])));
        
        // Input (Data, Variable, Relative) -- X, Y, Wheel
        descriptor.items.push(HidDescriptorItem::input(0x06));
        
        // End Collection
        descriptor.items.push(HidDescriptorItem::end_collection());
        
        // End Collection
        descriptor.items.push(HidDescriptorItem::end_collection());
        
        descriptor
    }
    
    /// Convertir a bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = alloc::vec::Vec::new();
        for item in &self.items {
            bytes.extend_from_slice(&item.to_bytes());
        }
        bytes
    }
}

/// Información del dispositivo HID
#[derive(Debug, Clone)]
pub struct HidDeviceInfo {
    pub vendor_id: u16,
    pub product_id: u16,
    pub version: u16,
    pub manufacturer: String,
    pub product: String,
    pub serial_number: String,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub max_packet_size: u8,
    pub country_code: u8,
    pub num_descriptors: u8,
    pub report_descriptor_length: u16,
}

/// Driver HID genérico
#[derive(Debug)]
pub struct HidDriver {
    pub info: HidDeviceInfo,
    pub descriptor: HidDescriptor,
    pub report_descriptor: HidReportDescriptor,
    pub device_address: u8,
    pub endpoint_address: u8,
    pub initialized: bool,
    pub error_count: u32,
}

impl HidDriver {
    /// Crear nuevo driver HID
    pub fn new(info: HidDeviceInfo, device_address: u8, endpoint_address: u8) -> Self {
        Self {
            info,
            descriptor: HidDescriptor::new(),
            report_descriptor: HidReportDescriptor::new(),
            device_address,
            endpoint_address,
            initialized: false,
            error_count: 0,
        }
    }
    
    /// Inicializar el driver HID
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        // Configurar descriptor HID
        self.descriptor.report_descriptor_length = self.info.report_descriptor_length;
        
        // Crear descriptor de reporte basado en el tipo de dispositivo
        self.create_report_descriptor()?;
        
        // Configurar endpoint
        self.configure_endpoint()?;
        
        self.initialized = true;
        Ok(())
    }
    
    /// Crear descriptor de reporte basado en el tipo de dispositivo
    fn create_report_descriptor(&mut self) -> Result<(), &'static str> {
        match self.info.device_class {
            0x03 => { // HID Class
                match self.info.device_subclass {
                    0x01 => { // Boot Interface Subclass
                        match self.info.device_protocol {
                            0x01 => { // Keyboard
                                self.report_descriptor = HidReportDescriptor::create_keyboard_descriptor();
                            }
                            0x02 => { // Mouse
                                self.report_descriptor = HidReportDescriptor::create_mouse_descriptor();
                            }
                            _ => {
                                return Err("Protocolo HID no soportado");
                            }
                        }
                    }
                    _ => {
                        return Err("Subclase HID no soportada");
                    }
                }
            }
            _ => {
                return Err("Clase de dispositivo no es HID");
            }
        }
        Ok(())
    }
    
    /// Configurar endpoint
    fn configure_endpoint(&mut self) -> Result<(), &'static str> {
        // En una implementación real, aquí se configuraría el endpoint USB
        // Por ahora simulamos la configuración
        Ok(())
    }
    
    /// Obtener descriptor HID
    pub fn get_descriptor(&self) -> &HidDescriptor {
        &self.descriptor
    }
    
    /// Obtener descriptor de reporte
    pub fn get_report_descriptor(&self) -> &HidReportDescriptor {
        &self.report_descriptor
    }
    
    /// Obtener información del dispositivo
    pub fn get_info(&self) -> &HidDeviceInfo {
        &self.info
    }
    
    /// Verificar si está inicializado
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
    
    /// Obtener longitud del descriptor de reporte
    pub fn get_report_descriptor_length(&self) -> u16 {
        self.report_descriptor.to_bytes().len() as u16
    }
}

/// Función de conveniencia para crear un driver HID
pub fn create_hid_driver(info: HidDeviceInfo, device_address: u8, endpoint_address: u8) -> HidDriver {
    HidDriver::new(info, device_address, endpoint_address)
}
