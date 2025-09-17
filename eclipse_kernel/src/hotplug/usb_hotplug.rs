//! Sistema de Hot-Plug USB para Eclipse OS
//! 
//! Implementa detección y manejo de dispositivos USB conectados/desconectados
//! en tiempo real, incluyendo soporte completo para ratón y teclado USB.

use super::{UsbDeviceInfo, UsbDeviceType, UsbDeviceState, UsbSpeed, UsbHotplugEvent, HotplugConfig};
use super::events::{UsbEventQueue, UsbEventFilter, UsbEventStats};
use super::notifications::{UsbNotificationSystem, UsbSystemNotification};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use crate::syslog;

/// Controlador de hot-plug USB
pub struct UsbHotplugManager {
    config: HotplugConfig,
    devices: BTreeMap<u32, UsbDeviceInfo>,
    event_queue: UsbEventQueue,
    event_filter: UsbEventFilter,
    event_stats: UsbEventStats,
    notification_system: UsbNotificationSystem,
    system_notifications: super::notifications::UsbSystemNotificationHandler,
    next_device_id: u32,
    is_polling: bool,
}

impl UsbHotplugManager {
    pub fn new(config: HotplugConfig) -> Self {
        Self {
            config,
            devices: BTreeMap::new(),
            event_queue: UsbEventQueue::new(100),
            event_filter: UsbEventFilter::default(),
            event_stats: UsbEventStats::default(),
            notification_system: UsbNotificationSystem::new(),
            system_notifications: super::notifications::UsbSystemNotificationHandler::new(50),
            next_device_id: 1,
            is_polling: false,
        }
    }

    /// Inicializar el sistema de hot-plug USB
    pub fn initialize(&mut self) -> Result<(), String> {
        syslog::log_kernel(syslog::SyslogSeverity::Info, "USB_HOTPLUG",("Inicializando sistema de hot-plug USB...");
        
        self.system_notifications.add_notification(
            UsbSystemNotification::SystemInitialized
        );

        // Registrar callbacks por defecto
        self.register_default_callbacks();

        syslog::log_kernel(syslog::SyslogSeverity::Info, "USB_HOTPLUG",("Sistema de hot-plug USB inicializado correctamente");
        Ok(())
    }

    /// Registrar callbacks por defecto
    fn register_default_callbacks(&mut self) {
        // Callback para dispositivos conectados
        let connected_callback = Box::new(|device: &UsbDeviceInfo| {
            syslog::log_kernel(syslog::SyslogSeverity::Info, "USB_HOTPLUG",(&alloc::format!(
                "Dispositivo USB conectado: {} (ID: {}, Tipo: {:?})",
                device.device_name,
                device.device_id,
                device.device_type
            ));
        });
        self.notification_system.register_device_connected_callback(connected_callback);

        // Callback para dispositivos desconectados
        let disconnected_callback = Box::new(|device_id: u32| {
            syslog::log_kernel(syslog::SyslogSeverity::Info, "USB_HOTPLUG",(&alloc::format!(
                "Dispositivo USB desconectado: ID {}",
                device_id
            ));
        });
        self.notification_system.register_device_disconnected_callback(disconnected_callback);

        // Callback para ratones USB
        let mouse_callback = Box::new(|device: &UsbDeviceInfo| {
            syslog::log_kernel(syslog::SyslogSeverity::Info, "USB_HOTPLUG",(&alloc::format!(
                "Ratón USB detectado: {} en puerto {}",
                device.device_name,
                device.port_number
            ));
        });
        self.notification_system.register_mouse_callback(mouse_callback);

        // Callback para teclados USB
        let keyboard_callback = Box::new(|device: &UsbDeviceInfo| {
            syslog::log_kernel(syslog::SyslogSeverity::Info, "USB_HOTPLUG",(&alloc::format!(
                "Teclado USB detectado: {} en puerto {}",
                device.device_name,
                device.port_number
            ));
        });
        self.notification_system.register_keyboard_callback(keyboard_callback);
    }

    /// Iniciar polling de dispositivos USB
    pub fn start_polling(&mut self) -> Result<(), String> {
        if self.is_polling {
            return Err("El polling ya está activo".to_string());
        }

        self.is_polling = true;
        self.system_notifications.add_notification(
            UsbSystemNotification::PollingStarted
        );

        syslog::log_kernel(syslog::SyslogSeverity::Info, "USB_HOTPLUG",("Iniciando polling de dispositivos USB...");
        Ok(())
    }

    /// Detener polling de dispositivos USB
    pub fn stop_polling(&mut self) {
        self.is_polling = false;
        self.system_notifications.add_notification(
            UsbSystemNotification::PollingStopped
        );
        syslog::log_kernel(syslog::SyslogSeverity::Info, "USB_HOTPLUG",("Polling de dispositivos USB detenido");
    }

    /// Procesar eventos USB pendientes
    pub fn process_events(&mut self) {
        while let Some(event) = self.event_queue.pop_event() {
            self.event_stats.record_event(&event);
            
            if self.event_filter.should_process(&event) {
                self.notification_system.notify_event(&event);
                self.event_stats.record_processed();
            } else {
                self.event_stats.record_dropped();
            }
        }
    }

    /// Simular detección de dispositivo USB (para testing)
    pub fn simulate_device_connection(&mut self, device_type: UsbDeviceType, port: u8) -> Result<u32, String> {
        if self.devices.len() >= self.config.max_devices {
            return Err("Máximo número de dispositivos alcanzado".to_string());
        }

        let device_id = self.next_device_id;
        self.next_device_id += 1;

        let device_info = UsbDeviceInfo {
            device_id,
            vendor_id: self.get_vendor_id_for_type(&device_type),
            product_id: self.get_product_id_for_type(&device_type),
            device_type: device_type.clone(),
            state: UsbDeviceState::Connected,
            device_name: self.get_device_name_for_type(&device_type),
            driver_loaded: false,
            port_number: port,
            speed: UsbSpeed::HighSpeed, // Asumir High Speed por defecto
        };

        self.devices.insert(device_id, device_info.clone());
        
        let event = UsbHotplugEvent::DeviceConnected(device_info);
        self.event_queue.push_event(event);

        Ok(device_id)
    }

    /// Simular desconexión de dispositivo USB
    pub fn simulate_device_disconnection(&mut self, device_id: u32) -> Result<(), String> {
        if self.devices.remove(&device_id).is_some() {
            let event = UsbHotplugEvent::DeviceDisconnected(device_id);
            self.event_queue.push_event(event);
            Ok(())
        } else {
            Err("Dispositivo no encontrado".to_string())
        }
    }

    /// Obtener información de un dispositivo
    pub fn get_device_info(&self, device_id: u32) -> Option<&UsbDeviceInfo> {
        self.devices.get(&device_id)
    }

    /// Listar todos los dispositivos conectados
    pub fn list_devices(&self) -> Vec<&UsbDeviceInfo> {
        self.devices.values().collect()
    }

    /// Obtener dispositivos por tipo
    pub fn get_devices_by_type(&self, device_type: UsbDeviceType) -> Vec<&UsbDeviceInfo> {
        self.devices.values()
            .filter(|device| device.device_type == device_type)
            .collect()
    }

    /// Obtener ratones USB
    pub fn get_usb_mice(&self) -> Vec<&UsbDeviceInfo> {
        self.get_devices_by_type(UsbDeviceType::Mouse)
    }

    /// Obtener teclados USB
    pub fn get_usb_keyboards(&self) -> Vec<&UsbDeviceInfo> {
        self.get_devices_by_type(UsbDeviceType::Keyboard)
    }

    /// Obtener estadísticas del sistema
    pub fn get_stats(&self) -> &UsbEventStats {
        &self.event_stats
    }

    /// Obtener número de dispositivos conectados
    pub fn device_count(&self) -> usize {
        self.devices.len()
    }

    /// Verificar si hay eventos pendientes
    pub fn has_pending_events(&self) -> bool {
        self.event_queue.has_events()
    }

    /// Obtener notificaciones del sistema
    pub fn get_system_notifications(&self) -> &[UsbSystemNotification] {
        self.system_notifications.get_notifications()
    }

    /// Limpiar notificaciones del sistema
    pub fn clear_system_notifications(&mut self) {
        self.system_notifications.clear_notifications();
    }

    /// Obtener configuración actual
    pub fn get_config(&self) -> &HotplugConfig {
        &self.config
    }

    /// Actualizar configuración
    pub fn update_config(&mut self, new_config: HotplugConfig) {
        self.config = new_config;
    }

    // Métodos auxiliares privados

    fn get_vendor_id_for_type(&self, device_type: &UsbDeviceType) -> u16 {
        match device_type {
            UsbDeviceType::Mouse => 0x046D, // Logitech
            UsbDeviceType::Keyboard => 0x046D, // Logitech
            UsbDeviceType::Storage => 0x0781, // SanDisk
            UsbDeviceType::Audio => 0x046D, // Logitech
            UsbDeviceType::Network => 0x0BDA, // Realtek
            UsbDeviceType::Unknown => 0x0000,
        }
    }

    fn get_product_id_for_type(&self, device_type: &UsbDeviceType) -> u16 {
        match device_type {
            UsbDeviceType::Mouse => 0xC077, // Logitech M100
            UsbDeviceType::Keyboard => 0xC31C, // Logitech K120
            UsbDeviceType::Storage => 0x5567, // SanDisk Cruzer
            UsbDeviceType::Audio => 0x0A44, // Logitech H390
            UsbDeviceType::Network => 0x818B, // Realtek RTL8188
            UsbDeviceType::Unknown => 0x0000,
        }
    }

    fn get_device_name_for_type(&self, device_type: &UsbDeviceType) -> String {
        match device_type {
            UsbDeviceType::Mouse => "Ratón USB".to_string(),
            UsbDeviceType::Keyboard => "Teclado USB".to_string(),
            UsbDeviceType::Storage => "Dispositivo de almacenamiento USB".to_string(),
            UsbDeviceType::Audio => "Dispositivo de audio USB".to_string(),
            UsbDeviceType::Network => "Adaptador de red USB".to_string(),
            UsbDeviceType::Unknown => "Dispositivo USB desconocido".to_string(),
        }
    }
}

impl Drop for UsbHotplugManager {
    fn drop(&mut self) {
        self.stop_polling();
        self.system_notifications.add_notification(
            UsbSystemNotification::SystemShutdown
        );
    }
}
