//! Estructuras base para dispositivos en Eclipse OS
//! 
//! Basado en la arquitectura de drivers de Redox OS

use core::fmt;

// Tipos de dispositivos
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeviceType {
    Storage,
    Network,
    Video,
    Audio,
    Input,
    Usb,
    Pci,
    Unknown,
}

impl DeviceType {
    pub fn as_u32(&self) -> u32 {
        match self {
            DeviceType::Storage => 0x01,
            DeviceType::Network => 0x02,
            DeviceType::Video => 0x03,
            DeviceType::Audio => 0x04,
            DeviceType::Input => 0x05,
            DeviceType::Usb => 0x06,
            DeviceType::Pci => 0x07,
            DeviceType::Unknown => 0xFF,
        }
    }

    pub fn from_u32(value: u32) -> Self {
        match value {
            0x01 => DeviceType::Storage,
            0x02 => DeviceType::Network,
            0x03 => DeviceType::Video,
            0x04 => DeviceType::Audio,
            0x05 => DeviceType::Input,
            0x06 => DeviceType::Usb,
            0x07 => DeviceType::Pci,
            _ => DeviceType::Unknown,
        }
    }
}

// Estados de dispositivos
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeviceState {
    Unknown,
    Initializing,
    Ready,
    Busy,
    Error,
    Disabled,
}

impl DeviceState {
    pub fn as_u32(&self) -> u32 {
        match self {
            DeviceState::Unknown => 0x00,
            DeviceState::Initializing => 0x01,
            DeviceState::Ready => 0x02,
            DeviceState::Busy => 0x03,
            DeviceState::Error => 0x04,
            DeviceState::Disabled => 0x05,
        }
    }

    pub fn from_u32(value: u32) -> Self {
        match value {
            0x00 => DeviceState::Unknown,
            0x01 => DeviceState::Initializing,
            0x02 => DeviceState::Ready,
            0x03 => DeviceState::Busy,
            0x04 => DeviceState::Error,
            0x05 => DeviceState::Disabled,
            _ => DeviceState::Unknown,
        }
    }
}

// Errores de dispositivos
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeviceError {
    NotFound,
    NotSupported,
    Busy,
    Timeout,
    InvalidParameter,
    OutOfMemory,
    IoError,
    HardwareError,
    DriverError,
    Unknown,
}

impl DeviceError {
    pub fn as_str(&self) -> &'static str {
        match self {
            DeviceError::NotFound => "Device not found",
            DeviceError::NotSupported => "Device not supported",
            DeviceError::Busy => "Device busy",
            DeviceError::Timeout => "Operation timeout",
            DeviceError::InvalidParameter => "Invalid parameter",
            DeviceError::OutOfMemory => "Out of memory",
            DeviceError::IoError => "I/O error",
            DeviceError::HardwareError => "Hardware error",
            DeviceError::DriverError => "Driver error",
            DeviceError::Unknown => "Unknown error",
        }
    }
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// Información de dispositivo
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub id: u32,
    pub name: [u8; 32],
    pub device_type: DeviceType,
    pub state: DeviceState,
    pub vendor_id: u16,
    pub device_id: u16,
    pub subsystem_vendor_id: u16,
    pub subsystem_id: u16,
    pub class_code: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub revision_id: u8,
    pub irq_line: u8,
    pub irq_pin: u8,
    pub base_addresses: [u64; 6],
    pub capabilities: u32,
}

impl DeviceInfo {
    pub fn new() -> Self {
        Self {
            id: 0,
            name: [0; 32],
            device_type: DeviceType::Unknown,
            state: DeviceState::Unknown,
            vendor_id: 0,
            device_id: 0,
            subsystem_vendor_id: 0,
            subsystem_id: 0,
            class_code: 0,
            subclass: 0,
            prog_if: 0,
            revision_id: 0,
            irq_line: 0,
            irq_pin: 0,
            base_addresses: [0; 6],
            capabilities: 0,
        }
    }

    pub fn set_name(&mut self, name: &str) {
        let name_bytes = name.as_bytes();
        let len = name_bytes.len().min(31);
        
        for i in 0..32 {
            if i < len {
                self.name[i] = name_bytes[i];
            } else {
                self.name[i] = 0;
            }
        }
    }

    pub fn get_name(&self) -> &str {
        // Encontrar el final del string
        let mut len = 0;
        for i in 0..32 {
            if self.name[i] == 0 {
                len = i;
                break;
            }
        }
        
        // Convertir a string (simplificado)
        unsafe {
            core::str::from_utf8_unchecked(&self.name[0..len])
        }
    }
}

// Estructura base de dispositivo
pub struct Device {
    pub info: DeviceInfo,
    pub driver_id: Option<u32>,
    pub is_initialized: bool,
    pub is_enabled: bool,
    pub error_count: u32,
    pub last_error: Option<DeviceError>,
}

impl Device {
    pub fn new(info: DeviceInfo) -> Self {
        Self {
            info,
            driver_id: None,
            is_initialized: false,
            is_enabled: false,
            error_count: 0,
            last_error: None,
        }
    }

    pub fn initialize(&mut self) -> Result<(), DeviceError> {
        if self.is_initialized {
            return Ok(());
        }

        self.info.state = DeviceState::Initializing;
        
        // Implementación simplificada - siempre exitosa
        self.is_initialized = true;
        self.is_enabled = true;
        self.info.state = DeviceState::Ready;
        
        Ok(())
    }

    pub fn enable(&mut self) -> Result<(), DeviceError> {
        if !self.is_initialized {
            return Err(DeviceError::DriverError);
        }

        self.is_enabled = true;
        self.info.state = DeviceState::Ready;
        Ok(())
    }

    pub fn disable(&mut self) -> Result<(), DeviceError> {
        self.is_enabled = false;
        self.info.state = DeviceState::Disabled;
        Ok(())
    }

    pub fn set_error(&mut self, error: DeviceError) {
        self.error_count += 1;
        self.last_error = Some(error);
        self.info.state = DeviceState::Error;
    }

    pub fn clear_error(&mut self) {
        self.last_error = None;
        if self.is_enabled {
            self.info.state = DeviceState::Ready;
        }
    }

    pub fn is_ready(&self) -> bool {
        self.is_initialized && self.is_enabled && self.info.state == DeviceState::Ready
    }

    pub fn is_busy(&self) -> bool {
        self.info.state == DeviceState::Busy
    }

    pub fn has_error(&self) -> bool {
        self.last_error.is_some()
    }
}

// Trait para operaciones de dispositivo
pub trait DeviceOperations {
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, DeviceError>;
    fn write(&mut self, buffer: &[u8]) -> Result<usize, DeviceError>;
    fn ioctl(&mut self, command: u32, arg: u64) -> Result<u64, DeviceError>;
    fn poll(&mut self) -> Result<bool, DeviceError>;
    fn reset(&mut self) -> Result<(), DeviceError>;
}

// Implementación por defecto
impl DeviceOperations for Device {
    fn read(&mut self, _buffer: &mut [u8]) -> Result<usize, DeviceError> {
        Err(DeviceError::NotSupported)
    }

    fn write(&mut self, _buffer: &[u8]) -> Result<usize, DeviceError> {
        Err(DeviceError::NotSupported)
    }

    fn ioctl(&mut self, _command: u32, _arg: u64) -> Result<u64, DeviceError> {
        Err(DeviceError::NotSupported)
    }

    fn poll(&mut self) -> Result<bool, DeviceError> {
        Ok(false)
    }

    fn reset(&mut self) -> Result<(), DeviceError> {
        self.clear_error();
        self.info.state = DeviceState::Ready;
        Ok(())
    }
}
