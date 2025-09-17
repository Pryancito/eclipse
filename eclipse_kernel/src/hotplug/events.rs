//! Sistema de eventos para hot-plug USB
//! 
//! Maneja la cola de eventos y notificaciones del sistema USB.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

/// Tipo de dispositivo USB
#[derive(Debug, Clone, PartialEq)]
pub enum UsbDeviceType {
    Mouse,
    Keyboard,
    Storage,
    Audio,
    Network,
    Unknown,
}

/// Estado de un dispositivo USB
#[derive(Debug, Clone, PartialEq)]
pub enum UsbDeviceState {
    Connected,
    Disconnected,
    Initializing,
    Ready,
    Error,
}

/// Información de un dispositivo USB
#[derive(Debug, Clone)]
pub struct UsbDeviceInfo {
    pub device_id: u32,
    pub vendor_id: u16,
    pub product_id: u16,
    pub device_type: UsbDeviceType,
    pub state: UsbDeviceState,
    pub device_name: String,
    pub driver_loaded: bool,
    pub port_number: u8,
    pub speed: UsbSpeed,
}

/// Velocidad USB
#[derive(Debug, Clone, PartialEq)]
pub enum UsbSpeed {
    LowSpeed,    // 1.5 Mbps
    FullSpeed,   // 12 Mbps
    HighSpeed,   // 480 Mbps
    SuperSpeed,  // 5 Gbps
    SuperSpeedPlus, // 10 Gbps
}

/// Evento de hot-plug USB
#[derive(Debug, Clone)]
pub enum UsbHotplugEvent {
    DeviceConnected(UsbDeviceInfo),
    DeviceDisconnected(u32), // device_id
    DeviceReady(UsbDeviceInfo),
    DeviceError(u32, String), // device_id, error_message
}

/// Cola de eventos USB
pub struct UsbEventQueue {
    events: VecDeque<UsbHotplugEvent>,
    max_events: usize,
}

impl UsbEventQueue {
    pub fn new(max_events: usize) -> Self {
        Self {
            events: VecDeque::new(),
            max_events,
        }
    }

    /// Agregar evento a la cola
    pub fn push_event(&mut self, event: UsbHotplugEvent) {
        if self.events.len() >= self.max_events {
            self.events.pop_front(); // Remover evento más antiguo
        }
        self.events.push_back(event);
    }

    /// Obtener siguiente evento
    pub fn pop_event(&mut self) -> Option<UsbHotplugEvent> {
        self.events.pop_front()
    }

    /// Verificar si hay eventos pendientes
    pub fn has_events(&self) -> bool {
        !self.events.is_empty()
    }

    /// Obtener número de eventos pendientes
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// Limpiar todos los eventos
    pub fn clear(&mut self) {
        self.events.clear();
    }
}

/// Filtro de eventos USB
pub struct UsbEventFilter {
    pub filter_mouse: bool,
    pub filter_keyboard: bool,
    pub filter_storage: bool,
    pub filter_audio: bool,
    pub filter_network: bool,
}

impl Default for UsbEventFilter {
    fn default() -> Self {
        Self {
            filter_mouse: true,
            filter_keyboard: true,
            filter_storage: true,
            filter_audio: true,
            filter_network: true,
        }
    }
}

impl UsbEventFilter {
    /// Verificar si un evento debe ser procesado
    pub fn should_process(&self, event: &UsbHotplugEvent) -> bool {
        match event {
            UsbHotplugEvent::DeviceConnected(info) => self.filter_device_type(&info.device_type),
            UsbHotplugEvent::DeviceDisconnected(_) => true, // Siempre procesar desconexiones
            UsbHotplugEvent::DeviceReady(info) => self.filter_device_type(&info.device_type),
            UsbHotplugEvent::DeviceError(_, _) => true, // Siempre procesar errores
        }
    }

    fn filter_device_type(&self, device_type: &super::UsbDeviceType) -> bool {
        match device_type {
            super::UsbDeviceType::Mouse => self.filter_mouse,
            super::UsbDeviceType::Keyboard => self.filter_keyboard,
            super::UsbDeviceType::Storage => self.filter_storage,
            super::UsbDeviceType::Audio => self.filter_audio,
            super::UsbDeviceType::Network => self.filter_network,
            super::UsbDeviceType::Unknown => true, // Siempre procesar dispositivos desconocidos
        }
    }
}

/// Estadísticas de eventos USB
#[derive(Debug, Default)]
pub struct UsbEventStats {
    pub total_events: u64,
    pub device_connected_events: u64,
    pub device_disconnected_events: u64,
    pub device_ready_events: u64,
    pub device_error_events: u64,
    pub events_processed: u64,
    pub events_dropped: u64,
}

impl UsbEventStats {
    pub fn record_event(&mut self, event: &UsbHotplugEvent) {
        self.total_events += 1;
        match event {
            UsbHotplugEvent::DeviceConnected(_) => self.device_connected_events += 1,
            UsbHotplugEvent::DeviceDisconnected(_) => self.device_disconnected_events += 1,
            UsbHotplugEvent::DeviceReady(_) => self.device_ready_events += 1,
            UsbHotplugEvent::DeviceError(_, _) => self.device_error_events += 1,
        }
    }

    pub fn record_processed(&mut self) {
        self.events_processed += 1;
    }

    pub fn record_dropped(&mut self) {
        self.events_dropped += 1;
    }

    pub fn to_string(&self) -> String {
        alloc::format!(
            "Eventos USB - Total: {}, Conectados: {}, Desconectados: {}, Listos: {}, Errores: {}, Procesados: {}, Descartados: {}",
            self.total_events,
            self.device_connected_events,
            self.device_disconnected_events,
            self.device_ready_events,
            self.device_error_events,
            self.events_processed,
            self.events_dropped
        )
    }
}
