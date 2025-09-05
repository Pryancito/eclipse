//! Sistema de drivers para Eclipse OS
//! 
//! Este módulo implementa un sistema de drivers básico que incluye:
//! - Gestión de dispositivos de hardware
//! - Drivers para dispositivos de almacenamiento
//! - Drivers de red
//! - Drivers de video
//! - Drivers de audio
//! - Drivers de entrada (teclado, mouse)

pub mod device;
pub mod storage;
pub mod network;
pub mod video;
pub mod input;
pub mod pci;
pub mod usb;
pub mod manager;
pub mod modular;

// Re-exportar componentes principales
pub use device::{DeviceType, DeviceState, DeviceError};
pub use manager::DriverResult;

// Constantes del sistema de drivers
pub const MAX_DEVICES: usize = 256;
pub const MAX_DRIVERS: usize = 64;
pub const DEVICE_NAME_LEN: usize = 32;
pub const DRIVER_NAME_LEN: usize = 32;

// Tipos de dispositivos soportados
pub const DEVICE_TYPE_STORAGE: u32 = 0x01;
pub const DEVICE_TYPE_NETWORK: u32 = 0x02;
pub const DEVICE_TYPE_VIDEO: u32 = 0x03;
pub const DEVICE_TYPE_AUDIO: u32 = 0x04;
pub const DEVICE_TYPE_INPUT: u32 = 0x05;
pub const DEVICE_TYPE_USB: u32 = 0x06;
pub const DEVICE_TYPE_PCI: u32 = 0x07;
pub const DEVICE_TYPE_UNKNOWN: u32 = 0xFF;

// Estados de dispositivos
pub const DEVICE_STATE_UNKNOWN: u32 = 0x00;
pub const DEVICE_STATE_INITIALIZING: u32 = 0x01;
pub const DEVICE_STATE_READY: u32 = 0x02;
pub const DEVICE_STATE_BUSY: u32 = 0x03;
pub const DEVICE_STATE_ERROR: u32 = 0x04;
pub const DEVICE_STATE_DISABLED: u32 = 0x05;

// Información del sistema de drivers
#[derive(Debug, Clone, Copy)]
pub struct DriverSystemInfo {
    pub total_devices: u32,
    pub active_devices: u32,
    pub total_drivers: u32,
    pub loaded_drivers: u32,
    pub storage_devices: u32,
    pub network_devices: u32,
    pub video_devices: u32,
    pub audio_devices: u32,
    pub input_devices: u32,
}

impl DriverSystemInfo {
    pub fn new() -> Self {
        Self {
            total_devices: 0,
            active_devices: 0,
            total_drivers: 0,
            loaded_drivers: 0,
            storage_devices: 0,
            network_devices: 0,
            video_devices: 0,
            audio_devices: 0,
            input_devices: 0,
        }
    }
}

// Inicialización del sistema de drivers
pub fn init_driver_system() -> DriverResult<()> {
    // Inicializar gestor de drivers
    manager::init_driver_manager()?;
    
    // Inicializar drivers básicos
    storage::init_storage_drivers()?;
    network::init_network_drivers()?;
    video::init_video_drivers()?;
    input::init_input_drivers()?;
    
    Ok(())
}

// Obtener información del sistema de drivers
pub fn get_driver_system_info() -> DriverSystemInfo {
    DriverSystemInfo::new()
}