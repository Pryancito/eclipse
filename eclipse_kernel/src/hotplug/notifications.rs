//! Sistema de notificaciones para hot-plug USB
//!
//! Maneja notificaciones y callbacks para eventos USB.

use super::{UsbDeviceInfo, UsbDeviceType, UsbHotplugEvent};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// Tipo de callback para eventos USB
pub type UsbEventCallback = Box<dyn Fn(&UsbHotplugEvent) + Send + Sync>;

/// Callback específico para dispositivos conectados
pub type UsbDeviceConnectedCallback = Box<dyn Fn(&UsbDeviceInfo) + Send + Sync>;

/// Callback específico para dispositivos desconectados
pub type UsbDeviceDisconnectedCallback = Box<dyn Fn(u32) + Send + Sync>; // device_id

/// Callback específico para ratones USB
pub type UsbMouseCallback = Box<dyn Fn(&UsbDeviceInfo) + Send + Sync>;

/// Callback específico para teclados USB
pub type UsbKeyboardCallback = Box<dyn Fn(&UsbDeviceInfo) + Send + Sync>;

/// Sistema de notificaciones USB
pub struct UsbNotificationSystem {
    general_callbacks: Vec<UsbEventCallback>,
    device_connected_callbacks: Vec<UsbDeviceConnectedCallback>,
    device_disconnected_callbacks: Vec<UsbDeviceDisconnectedCallback>,
    mouse_callbacks: Vec<UsbMouseCallback>,
    keyboard_callbacks: Vec<UsbKeyboardCallback>,
}

impl UsbNotificationSystem {
    pub fn new() -> Self {
        Self {
            general_callbacks: Vec::new(),
            device_connected_callbacks: Vec::new(),
            device_disconnected_callbacks: Vec::new(),
            mouse_callbacks: Vec::new(),
            keyboard_callbacks: Vec::new(),
        }
    }

    /// Registrar callback general para eventos USB
    pub fn register_general_callback(&mut self, callback: UsbEventCallback) {
        self.general_callbacks.push(callback);
    }

    /// Registrar callback para dispositivos conectados
    pub fn register_device_connected_callback(&mut self, callback: UsbDeviceConnectedCallback) {
        self.device_connected_callbacks.push(callback);
    }

    /// Registrar callback para dispositivos desconectados
    pub fn register_device_disconnected_callback(
        &mut self,
        callback: UsbDeviceDisconnectedCallback,
    ) {
        self.device_disconnected_callbacks.push(callback);
    }

    /// Registrar callback para ratones USB
    pub fn register_mouse_callback(&mut self, callback: UsbMouseCallback) {
        self.mouse_callbacks.push(callback);
    }

    /// Registrar callback para teclados USB
    pub fn register_keyboard_callback(&mut self, callback: UsbKeyboardCallback) {
        self.keyboard_callbacks.push(callback);
    }

    /// Notificar evento a todos los callbacks registrados
    pub fn notify_event(&self, event: &UsbHotplugEvent) {
        // Notificar callbacks generales
        for callback in &self.general_callbacks {
            callback(event);
        }

        // Notificar callbacks específicos
        match event {
            UsbHotplugEvent::DeviceConnected(info) => {
                for callback in &self.device_connected_callbacks {
                    callback(info);
                }

                // Notificar callbacks específicos por tipo de dispositivo
                match info.device_type {
                    UsbDeviceType::Mouse => {
                        for callback in &self.mouse_callbacks {
                            callback(info);
                        }
                    }
                    UsbDeviceType::Keyboard => {
                        for callback in &self.keyboard_callbacks {
                            callback(info);
                        }
                    }
                    _ => {}
                }
            }
            UsbHotplugEvent::DeviceDisconnected(device_id) => {
                for callback in &self.device_disconnected_callbacks {
                    callback(*device_id);
                }
            }
            UsbHotplugEvent::DeviceReady(info) => {
                // Los dispositivos listos también pueden ser ratones o teclados
                match info.device_type {
                    UsbDeviceType::Mouse => {
                        for callback in &self.mouse_callbacks {
                            callback(info);
                        }
                    }
                    UsbDeviceType::Keyboard => {
                        for callback in &self.keyboard_callbacks {
                            callback(info);
                        }
                    }
                    _ => {}
                }
            }
            UsbHotplugEvent::DeviceError(_, _) => {
                // Los errores se manejan en callbacks generales
            }
        }
    }

    /// Limpiar todos los callbacks
    pub fn clear_callbacks(&mut self) {
        self.general_callbacks.clear();
        self.device_connected_callbacks.clear();
        self.device_disconnected_callbacks.clear();
        self.mouse_callbacks.clear();
        self.keyboard_callbacks.clear();
    }

    /// Obtener número de callbacks registrados
    pub fn callback_count(&self) -> usize {
        self.general_callbacks.len()
            + self.device_connected_callbacks.len()
            + self.device_disconnected_callbacks.len()
            + self.mouse_callbacks.len()
            + self.keyboard_callbacks.len()
    }
}

/// Notificación de estado del sistema USB
#[derive(Debug, Clone)]
pub enum UsbSystemNotification {
    SystemInitialized,
    SystemShutdown,
    PollingStarted,
    PollingStopped,
    Error(String),
    Warning(String),
    Info(String),
}

/// Sistema de notificaciones del sistema
pub struct UsbSystemNotificationHandler {
    notifications: Vec<UsbSystemNotification>,
    max_notifications: usize,
}

impl UsbSystemNotificationHandler {
    pub fn new(max_notifications: usize) -> Self {
        Self {
            notifications: Vec::new(),
            max_notifications,
        }
    }

    /// Agregar notificación del sistema
    pub fn add_notification(&mut self, notification: UsbSystemNotification) {
        if self.notifications.len() >= self.max_notifications {
            self.notifications.remove(0); // Remover notificación más antigua
        }
        self.notifications.push(notification);
    }

    /// Obtener todas las notificaciones
    pub fn get_notifications(&self) -> &[UsbSystemNotification] {
        &self.notifications
    }

    /// Limpiar notificaciones
    pub fn clear_notifications(&mut self) {
        self.notifications.clear();
    }

    /// Obtener número de notificaciones
    pub fn notification_count(&self) -> usize {
        self.notifications.len()
    }
}
