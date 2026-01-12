//! Sistema de drivers para Eclipse OS
//!
//! Este módulo implementa un sistema de drivers básico que incluye:
//! - Gestión de dispositivos de hardware
//! - Drivers para dispositivos de almacenamiento
//! - Drivers de red
//! - Drivers de video
//! - Drivers de audio
//! - Drivers de entrada (teclado, mouse)

pub mod advanced;
pub mod acceleration_2d;
pub mod amd_graphics;
pub mod block;
pub mod binary_driver_manager;
pub mod bochs_vbe;
pub mod device;
pub mod direct_framebuffer;
// pub mod drm;
// pub mod drm_integration;
// pub mod drm_manager;
pub mod framebuffer;
pub mod framebuffer_manager;
pub mod framebuffer_updater;
pub mod gpu_control;
pub mod gpu_manager;
pub mod gui_integration;
pub mod hardware_framebuffer;
pub mod input;
pub mod input_system;
pub mod intel_graphics;
pub mod intel_raid;
pub mod intel_ahci_raid;
pub mod intel_raid_driver;
pub mod ipc;
pub mod keyboard;
pub mod stdin;
pub mod manager;
pub mod modular;
pub mod mouse;
pub mod network;
pub mod nvidia_cuda;
pub mod nvidia_example;
pub mod nvidia_graphics;
pub mod nvidia_integration;
pub mod nvidia_pci_driver;
pub mod nvme;
pub mod sata_ahci;
pub mod nvidia_rtx;
pub mod nvidia_smi;
pub mod nvidia_vulkan;
pub mod pci;
pub mod pci_driver;
pub mod pci_polished;
pub mod virtio_polished;
pub mod resolution_manager;
pub mod storage;
pub mod uefi_gop;
pub mod uefi_graphics;
pub mod usb;
pub mod usb_hid;
pub mod usb_hid_reader; // Lector seguro de datos HID desde XHCI
pub mod usb_hub;
pub mod usb_keyboard;
pub mod usb_keyboard_real;
pub mod usb_manager;
pub mod usb_mouse;
pub mod usb_mouse_real;
pub mod usb_xhci;
pub mod usb_xhci_improved;
pub mod usb_xhci_global;
pub mod usb_xhci_with_crate; // Acceso global seguro al XHCI
pub mod usb_xhci_transfer;
pub mod usb_xhci_enumerate;
// pub mod usb_xhci_interrupts; // ELIMINADO - causaba kernel panics por problemas de concurrencia
pub mod usb_xhci_port;
pub mod usb_xhci_context;
pub mod usb_xhci_enumerator;
pub mod usb_xhci_commands;
pub mod usb_xhci_control;
pub mod virtio_blk;
pub mod virtio_std;
pub mod ata_direct;
pub mod virtio_gpu;
pub mod vmware_svga;
pub mod storage_manager;
pub mod storage_device_wrapper;
pub mod power_management;
pub mod usb_diagnostic;
pub mod usb_events;
pub mod usb_hotplug;
pub mod usb_audio;
pub mod usb_video;
pub mod usb_network;
pub mod usb_user_api;
pub mod usb_power_management;
pub mod ahci;
pub mod virtio_net;

// Re-exportar componentes principales
pub use device::{DeviceError, DeviceState, DeviceType};
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
    input::init_input_drivers()?;

    Ok(())
}

// Obtener información del sistema de drivers
pub fn get_driver_system_info() -> DriverSystemInfo {
    DriverSystemInfo::new()
}
