//! Sistema de Hot-Plug para Eclipse OS
//!
//! Implementa detección y manejo de dispositivos USB conectados/desconectados
//! en tiempo real, incluyendo soporte completo para ratón y teclado USB.

pub mod events;
pub mod manager;
pub mod notifications;
pub mod usb_hotplug;

// Re-exportar tipos principales
pub use events::{UsbDeviceInfo, UsbDeviceState, UsbDeviceType, UsbHotplugEvent, UsbSpeed};
pub use manager::HotplugManager;

use alloc::string::String;
use spin::Mutex;

/// Configuración del sistema de hot-plug
#[derive(Debug, Clone)]
pub struct HotplugConfig {
    pub enable_usb_hotplug: bool,
    pub enable_mouse_support: bool,
    pub enable_keyboard_support: bool,
    pub enable_storage_support: bool,
    pub poll_interval_ms: u64,
    pub max_devices: usize,
}

impl Default for HotplugConfig {
    fn default() -> Self {
        Self {
            enable_usb_hotplug: true,
            enable_mouse_support: true,
            enable_keyboard_support: true,
            enable_storage_support: true,
            poll_interval_ms: 100, // 100ms
            max_devices: 32,
        }
    }
}

/// Gestor de hotplug global
static HOTPLUG_MANAGER: Mutex<Option<HotplugManager>> = Mutex::new(None);

/// Inicializar el sistema de hotplug
pub fn init_hotplug_manager(config: HotplugConfig) -> Result<(), String> {
    let mut manager_guard = HOTPLUG_MANAGER.lock();
    
    // Crear el gestor con el config
    let manager = HotplugManager::new(config);
    
    *manager_guard = Some(manager);
    
    Ok(())
}

/// Obtener referencia al gestor de hotplug
pub fn get_hotplug_manager() -> Option<&'static Mutex<Option<HotplugManager>>> {
    Some(&HOTPLUG_MANAGER)
}
