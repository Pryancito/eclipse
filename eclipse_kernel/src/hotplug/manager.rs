//! Manager principal del sistema de Hot-Plug
//! 
//! Coordina el sistema de hot-plug USB y proporciona una interfaz unificada.

use super::{HotplugConfig, UsbDeviceInfo, UsbDeviceType, UsbHotplugEvent};
use super::usb_hotplug::UsbHotplugManager;
use super::events::UsbEventStats;
use super::notifications::UsbSystemNotification;
use alloc::string::String;
use alloc::vec::Vec;
use crate::syslog;

/// Manager principal del sistema de hot-plug
pub struct HotplugManager {
    usb_manager: UsbHotplugManager,
    is_initialized: bool,
}

impl HotplugManager {
    pub fn new(config: HotplugConfig) -> Self {
        Self {
            usb_manager: UsbHotplugManager::new(config),
            is_initialized: false,
        }
    }

    /// Inicializar el sistema de hot-plug
    pub fn initialize(&mut self) -> Result<(), String> {
        if self.is_initialized {
            return Err("El sistema de hot-plug ya está inicializado".to_string());
        }

        syslog::log_kernel(syslog::SyslogSeverity::Info, "HOTPLUG",_kernel(syslog::SyslogSeverity::Info, "HOTPLUG", "Inicializando sistema de hot-plug...");

        // Inicializar USB
        self.usb_manager.initialize()?;

        self.is_initialized = true;
        syslog::log_kernel(syslog::SyslogSeverity::Info, "HOTPLUG",_kernel(syslog::SyslogSeverity::Info, "HOTPLUG", "Sistema de hot-plug inicializado correctamente");
        Ok(())
    }

    /// Iniciar el sistema de hot-plug
    pub fn start(&mut self) -> Result<(), String> {
        if !self.is_initialized {
            return Err("El sistema de hot-plug no está inicializado".to_string());
        }

        syslog::log_kernel(syslog::SyslogSeverity::Info, "HOTPLUG",("Iniciando sistema de hot-plug...");

        // Iniciar polling USB
        self.usb_manager.start_polling()?;

        syslog::log_kernel(syslog::SyslogSeverity::Info, "HOTPLUG",("Sistema de hot-plug iniciado correctamente");
        Ok(())
    }

    /// Detener el sistema de hot-plug
    pub fn stop(&mut self) {
        syslog::log_kernel(syslog::SyslogSeverity::Info, "HOTPLUG",("Deteniendo sistema de hot-plug...");
        
        self.usb_manager.stop_polling();
        
        syslog::log_kernel(syslog::SyslogSeverity::Info, "HOTPLUG",("Sistema de hot-plug detenido");
    }

    /// Procesar eventos pendientes
    pub fn process_events(&mut self) {
        if self.is_initialized {
            self.usb_manager.process_events();
        }
    }

    /// Simular conexión de dispositivo USB
    pub fn simulate_usb_device_connection(&mut self, device_type: UsbDeviceType, port: u8) -> Result<u32, String> {
        if !self.is_initialized {
            return Err("El sistema de hot-plug no está inicializado".to_string());
        }

        self.usb_manager.simulate_device_connection(device_type, port)
    }

    /// Simular desconexión de dispositivo USB
    pub fn simulate_usb_device_disconnection(&mut self, device_id: u32) -> Result<(), String> {
        if !self.is_initialized {
            return Err("El sistema de hot-plug no está inicializado".to_string());
        }

        self.usb_manager.simulate_device_disconnection(device_id)
    }

    /// Obtener información de dispositivo USB
    pub fn get_usb_device_info(&self, device_id: u32) -> Option<&UsbDeviceInfo> {
        self.usb_manager.get_device_info(device_id)
    }

    /// Listar todos los dispositivos USB
    pub fn list_usb_devices(&self) -> Vec<&UsbDeviceInfo> {
        self.usb_manager.list_devices()
    }

    /// Obtener ratones USB
    pub fn get_usb_mice(&self) -> Vec<&UsbDeviceInfo> {
        self.usb_manager.get_usb_mice()
    }

    /// Obtener teclados USB
    pub fn get_usb_keyboards(&self) -> Vec<&UsbDeviceInfo> {
        self.usb_manager.get_usb_keyboards()
    }

    /// Obtener estadísticas del sistema
    pub fn get_stats(&self) -> &UsbEventStats {
        self.usb_manager.get_stats()
    }

    /// Obtener número de dispositivos USB conectados
    pub fn usb_device_count(&self) -> usize {
        self.usb_manager.device_count()
    }

    /// Verificar si hay eventos pendientes
    pub fn has_pending_events(&self) -> bool {
        self.usb_manager.has_pending_events()
    }

    /// Obtener notificaciones del sistema
    pub fn get_system_notifications(&self) -> &[UsbSystemNotification] {
        self.usb_manager.get_system_notifications()
    }

    /// Limpiar notificaciones del sistema
    pub fn clear_system_notifications(&mut self) {
        self.usb_manager.clear_system_notifications();
    }

    /// Obtener configuración actual
    pub fn get_config(&self) -> &HotplugConfig {
        self.usb_manager.get_config()
    }

    /// Actualizar configuración
    pub fn update_config(&mut self, new_config: HotplugConfig) {
        self.usb_manager.update_config(new_config);
    }

    /// Verificar si el sistema está inicializado
    pub fn is_initialized(&self) -> bool {
        self.is_initialized
    }

    /// Obtener estado del sistema
    pub fn get_system_status(&self) -> String {
        if !self.is_initialized {
            return "No inicializado".to_string();
        }

        let device_count = self.usb_device_count();
        let event_count = if self.has_pending_events() { "Sí" } else { "No" };
        let stats = self.get_stats();

        alloc::format!(
            "Hot-plug USB - Dispositivos: {}, Eventos pendientes: {}, Eventos procesados: {}",
            device_count,
            event_count,
            stats.events_processed
        )
    }

    /// Realizar demostración del sistema
    pub fn run_demo(&mut self) -> Result<(), String> {
        if !self.is_initialized {
            return Err("El sistema de hot-plug no está inicializado".to_string());
        }

        syslog::log_kernel(syslog::SyslogSeverity::Info, "HOTPLUG",("Iniciando demostración del sistema de hot-plug USB...");

        // Simular conexión de ratón
        let mouse_id = self.simulate_usb_device_connection(UsbDeviceType::Mouse, 1)?;
        syslog::log_kernel(syslog::SyslogSeverity::Info, "HOTPLUG",(&alloc::format!("Ratón USB conectado con ID: {}", mouse_id));

        // Procesar eventos
        self.process_events();

        // Simular conexión de teclado
        let keyboard_id = self.simulate_usb_device_connection(UsbDeviceType::Keyboard, 2)?;
        syslog::log_kernel(syslog::SyslogSeverity::Info, "HOTPLUG",(&alloc::format!("Teclado USB conectado con ID: {}", keyboard_id));

        // Procesar eventos
        self.process_events();

        // Simular conexión de dispositivo de almacenamiento
        let storage_id = self.simulate_usb_device_connection(UsbDeviceType::Storage, 3)?;
        syslog::log_kernel(syslog::SyslogSeverity::Info, "HOTPLUG",(&alloc::format!("Dispositivo de almacenamiento USB conectado con ID: {}", storage_id));

        // Procesar eventos
        self.process_events();

        // Mostrar estadísticas
        let stats = self.get_stats();
        syslog::log_kernel(syslog::SyslogSeverity::Info, "HOTPLUG",(&stats.to_string());

        // Simular desconexión de ratón
        self.simulate_usb_device_disconnection(mouse_id)?;
        syslog::log_kernel(syslog::SyslogSeverity::Info, "HOTPLUG",("Ratón USB desconectado");

        // Procesar eventos
        self.process_events();

        syslog::log_kernel(syslog::SyslogSeverity::Info, "HOTPLUG",("Demostración del sistema de hot-plug USB completada");
        Ok(())
    }
}

impl Drop for HotplugManager {
    fn drop(&mut self) {
        if self.is_initialized {
            self.stop();
        }
    }
}
