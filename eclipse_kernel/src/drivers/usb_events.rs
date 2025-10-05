//! Sistema de eventos USB para hot-plug
//! 
//! Define los tipos de eventos USB y estructuras para manejar
//! la conexión y desconexión de dispositivos en tiempo real.

use crate::debug::serial_write_str;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

/// Tipos de eventos USB
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbEventType {
    DeviceConnected,
    DeviceDisconnected,
    DeviceError,
    PortStatusChanged,
    PowerStateChanged,
}

/// Información de un dispositivo USB
#[derive(Debug, Clone)]
pub struct UsbDeviceInfo {
    pub device_id: u32,
    pub vendor_id: u16,
    pub product_id: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub port_number: u8,
    pub controller_type: UsbControllerType,
    pub speed: UsbDeviceSpeed,
    pub power_state: UsbPowerState,
    pub connection_time: u64,
}

/// Tipo de controlador USB
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbControllerType {
    XHCI,  // USB 3.0+
    EHCI,  // USB 2.0
    OHCI,  // USB 1.1
    UHCI,  // USB 1.1
}

/// Velocidad del dispositivo USB
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbDeviceSpeed {
    Unknown,
    Low,        // USB 1.1 - 1.5 Mbps
    Full,       // USB 1.1 - 12 Mbps
    High,       // USB 2.0 - 480 Mbps
    Super,      // USB 3.0 - 5 Gbps
    SuperPlus,  // USB 3.1 - 10 Gbps
}

/// Estado de energía del dispositivo
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbPowerState {
    On,
    Suspend,
    Off,
    Error,
}

/// Error USB
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbError {
    DeviceNotFound,
    DeviceNotReady,
    PowerFailure,
    CommunicationError,
    InvalidConfiguration,
    UnsupportedDevice,
}

/// Evento USB
#[derive(Debug, Clone)]
pub struct UsbEvent {
    pub event_type: UsbEventType,
    pub device_info: Option<UsbDeviceInfo>,
    pub error: Option<UsbError>,
    pub timestamp: u64,
    pub port_number: u8,
    pub controller_type: UsbControllerType,
}

impl UsbEvent {
    /// Crear evento de dispositivo conectado
    pub fn device_connected(device_info: UsbDeviceInfo, timestamp: u64) -> Self {
        Self {
            event_type: UsbEventType::DeviceConnected,
            device_info: Some(device_info.clone()),
            error: None,
            timestamp,
            port_number: device_info.port_number,
            controller_type: device_info.controller_type,
        }
    }

    /// Crear evento de dispositivo desconectado
    pub fn device_disconnected(device_info: UsbDeviceInfo, timestamp: u64) -> Self {
        let controller_type = device_info.controller_type;
        let port_number = device_info.port_number;
        Self {
            event_type: UsbEventType::DeviceDisconnected,
            device_info: Some(device_info),
            error: None,
            timestamp,
            port_number,
            controller_type,
        }
    }

    /// Crear evento de error
    pub fn device_error(port_number: u8, controller_type: UsbControllerType, error: UsbError, timestamp: u64) -> Self {
        Self {
            event_type: UsbEventType::DeviceError,
            device_info: None,
            error: Some(error),
            timestamp,
            port_number,
            controller_type,
        }
    }

    /// Crear evento de cambio de estado de puerto
    pub fn port_status_changed(port_number: u8, controller_type: UsbControllerType, timestamp: u64) -> Self {
        Self {
            event_type: UsbEventType::PortStatusChanged,
            device_info: None,
            error: None,
            timestamp,
            port_number,
            controller_type,
        }
    }
}

/// Contador de eventos (thread-safe)
static EVENT_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Obtener el siguiente ID de evento
pub fn get_next_event_id() -> u32 {
    EVENT_COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// Obtener timestamp actual (simulado)
pub fn get_current_timestamp() -> u64 {
    // En un sistema real, esto vendría de un timer del sistema
    // Por ahora, simulamos con un contador
    EVENT_COUNTER.load(Ordering::SeqCst) as u64
}

/// Función helper para crear información de dispositivo USB
impl UsbDeviceInfo {
    pub fn new(
        device_id: u32,
        vendor_id: u16,
        product_id: u16,
        device_class: u8,
        device_subclass: u8,
        device_protocol: u8,
        port_number: u8,
        controller_type: UsbControllerType,
        speed: UsbDeviceSpeed,
    ) -> Self {
        Self {
            device_id,
            vendor_id,
            product_id,
            device_class,
            device_subclass,
            device_protocol,
            port_number,
            controller_type,
            speed,
            power_state: UsbPowerState::On,
            connection_time: get_current_timestamp(),
        }
    }

    /// Obtener nombre del fabricante
    pub fn get_vendor_name(&self) -> &'static str {
        match self.vendor_id {
            0x8086 => "Intel",
            0x10DE => "NVIDIA",
            0x1002 => "AMD",
            0x1AF4 => "VirtIO",
            0x15AD => "VMware",
            0x1234 => "QEMU/Bochs",
            0x046D => "Logitech",
            0x045E => "Microsoft",
            0x05AC => "Apple",
            0x0BDA => "Realtek",
            0x04CA => "Lite-On",
            _ => "Unknown",
        }
    }

    /// Obtener nombre de la clase de dispositivo
    pub fn get_class_name(&self) -> &'static str {
        match self.device_class {
            0x00 => "Interface Specific",
            0x01 => "Audio",
            0x02 => "Communications",
            0x03 => "HID",
            0x05 => "Physical",
            0x06 => "Image",
            0x07 => "Printer",
            0x08 => "Mass Storage",
            0x09 => "Hub",
            0x0A => "CDC Data",
            0x0B => "Smart Card",
            0x0D => "Content Security",
            0x0E => "Video",
            0x0F => "Personal Healthcare",
            0x10 => "Audio/Video",
            0x11 => "Billboard",
            0x12 => "USB Type-C Bridge",
            0x3C => "I3C",
            0x3D => "Camera Control",
            0xE0 => "Wireless Controller",
            0xEF => "Miscellaneous",
            0xFE => "Application Specific",
            0xFF => "Vendor Specific",
            _ => "Unknown",
        }
    }

    /// Verificar si el dispositivo es HID
    pub fn is_hid_device(&self) -> bool {
        self.device_class == 0x03
    }

    /// Verificar si el dispositivo es de almacenamiento masivo
    pub fn is_mass_storage_device(&self) -> bool {
        self.device_class == 0x08
    }

    /// Verificar si el dispositivo es un hub
    pub fn is_hub_device(&self) -> bool {
        self.device_class == 0x09
    }

    /// Obtener velocidad como string
    pub fn get_speed_string(&self) -> &'static str {
        match self.speed {
            UsbDeviceSpeed::Unknown => "Unknown",
            UsbDeviceSpeed::Low => "Low Speed (1.5 Mbps)",
            UsbDeviceSpeed::Full => "Full Speed (12 Mbps)",
            UsbDeviceSpeed::High => "High Speed (480 Mbps)",
            UsbDeviceSpeed::Super => "Super Speed (5 Gbps)",
            UsbDeviceSpeed::SuperPlus => "Super Speed+ (10 Gbps)",
        }
    }
}

/// Función helper para logging de eventos
pub fn log_usb_event(event: &UsbEvent) {
    let event_id = get_next_event_id();
    let timestamp = event.timestamp;
    
    match event.event_type {
        UsbEventType::DeviceConnected => {
            if let Some(ref device) = event.device_info {
                serial_write_str(&alloc::format!(
                    "USB_HOTPLUG: [{}] Device connected - VID:{:04X} ({}) PID:{:04X} Class:{} Port:{} Speed:{} Controller:{:?}\n",
                    event_id,
                    device.vendor_id,
                    device.get_vendor_name(),
                    device.product_id,
                    device.get_class_name(),
                    device.port_number,
                    device.get_speed_string(),
                    device.controller_type
                ));
            }
        }
        UsbEventType::DeviceDisconnected => {
            if let Some(ref device) = event.device_info {
                serial_write_str(&alloc::format!(
                    "USB_HOTPLUG: [{}] Device disconnected - VID:{:04X} ({}) PID:{:04X} Class:{} Port:{} Controller:{:?}\n",
                    event_id,
                    device.vendor_id,
                    device.get_vendor_name(),
                    device.product_id,
                    device.get_class_name(),
                    device.port_number,
                    device.controller_type
                ));
            }
        }
        UsbEventType::DeviceError => {
            if let Some(ref error) = event.error {
                serial_write_str(&alloc::format!(
                    "USB_HOTPLUG: [{}] Device error - Port:{} Controller:{:?} Error:{:?}\n",
                    event_id,
                    event.port_number,
                    event.controller_type,
                    error
                ));
            }
        }
        UsbEventType::PortStatusChanged => {
            serial_write_str(&alloc::format!(
                "USB_HOTPLUG: [{}] Port status changed - Port:{} Controller:{:?}\n",
                event_id,
                event.port_number,
                event.controller_type
            ));
        }
        UsbEventType::PowerStateChanged => {
            serial_write_str(&alloc::format!(
                "USB_HOTPLUG: [{}] Power state changed - Port:{} Controller:{:?}\n",
                event_id,
                event.port_number,
                event.controller_type
            ));
        }
    }
}

/// Función helper para crear dispositivos USB simulados (para testing)
pub fn create_simulated_device(
    device_id: u32,
    vendor_id: u16,
    product_id: u16,
    device_class: u8,
    port_number: u8,
    controller_type: UsbControllerType,
    speed: UsbDeviceSpeed,
) -> UsbDeviceInfo {
    UsbDeviceInfo::new(
        device_id,
        vendor_id,
        product_id,
        device_class,
        0, // subclass
        0, // protocol
        port_number,
        controller_type,
        speed,
    )
}
