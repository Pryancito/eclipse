//! Manager de hot-plug USB para Eclipse OS
//! 
//! Implementa detección automática de dispositivos USB conectados/desconectados
//! y gestión de eventos en tiempo real.

use crate::debug::serial_write_str;
use crate::drivers::pci::{PciManager, PciDevice};
use crate::drivers::usb_events::{
    UsbEvent, UsbEventType, UsbDeviceInfo, UsbControllerType, 
    UsbDeviceSpeed, UsbError, log_usb_event, get_current_timestamp
};
use crate::drivers::usb_xhci::XhciController;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Manager de hot-plug USB
pub struct UsbHotPlugManager {
    /// Lista de dispositivos USB conectados
    connected_devices: BTreeMap<u32, UsbDeviceInfo>,
    /// Cola de eventos USB
    event_queue: Vec<UsbEvent>,
    /// Controladores USB activos
    controllers: Vec<UsbControllerInfo>,
    /// Estado de monitoreo
    monitoring_enabled: AtomicBool,
    /// Contador de dispositivos
    device_counter: AtomicU32,
    /// Último timestamp de verificación
    last_check_time: AtomicU32,
}

/// Información de un controlador USB
#[derive(Debug, Clone)]
pub struct UsbControllerInfo {
    pub controller_type: UsbControllerType,
    pub pci_device: PciDevice,
    pub port_count: u8,
    pub is_active: bool,
    pub last_port_check: u64,
}

/// Configuración del manager de hot-plug
#[derive(Debug, Clone)]
pub struct UsbHotPlugConfig {
    pub check_interval_ms: u32,
    pub max_events_in_queue: usize,
    pub enable_logging: bool,
    pub auto_power_management: bool,
}

impl Default for UsbHotPlugConfig {
    fn default() -> Self {
        Self {
            check_interval_ms: 1000, // Verificar cada segundo
            max_events_in_queue: 256,
            enable_logging: true,
            auto_power_management: true,
        }
    }
}

impl UsbHotPlugManager {
    /// Crear nuevo manager de hot-plug
    pub fn new() -> Self {
        serial_write_str("USB_HOTPLUG: Inicializando manager de hot-plug USB\n");
        
        Self {
            connected_devices: BTreeMap::new(),
            event_queue: Vec::new(),
            controllers: Vec::new(),
            monitoring_enabled: AtomicBool::new(false),
            device_counter: AtomicU32::new(0),
            last_check_time: AtomicU32::new(0),
        }
    }

    /// Inicializar el manager con configuración
    pub fn initialize(&mut self, config: UsbHotPlugConfig) -> Result<(), UsbError> {
        serial_write_str("USB_HOTPLUG: Inicializando sistema de hot-plug...\n");
        
        // Escanear controladores USB
        self.scan_usb_controllers()?;
        
        // Inicializar estado inicial de dispositivos
        self.initialize_device_state()?;
        
        // Habilitar monitoreo
        self.monitoring_enabled.store(true, Ordering::SeqCst);
        
        serial_write_str(&alloc::format!(
            "USB_HOTPLUG: Sistema inicializado - {} controladores encontrados\n",
            self.controllers.len()
        ));
        
        Ok(())
    }

    /// Escanear controladores USB disponibles
    fn scan_usb_controllers(&mut self) -> Result<(), UsbError> {
        let mut pci_manager = PciManager::new();
        pci_manager.scan_all_buses();
        
        // Buscar controladores xHCI (USB 3.0+)
        let xhci_devices = pci_manager.find_devices_by_class_subclass(0x0C, 0x03);
        for pci_device in xhci_devices {
            self.controllers.push(UsbControllerInfo {
                controller_type: UsbControllerType::XHCI,
                pci_device,
                port_count: 8, // Estimación por defecto
                is_active: true,
                last_port_check: get_current_timestamp(),
            });
        }
        
        // Buscar controladores EHCI (USB 2.0)
        let ehci_devices = pci_manager.find_devices_by_class_subclass(0x0C, 0x20);
        for pci_device in ehci_devices {
            self.controllers.push(UsbControllerInfo {
                controller_type: UsbControllerType::EHCI,
                pci_device,
                port_count: 4, // Estimación por defecto
                is_active: true,
                last_port_check: get_current_timestamp(),
            });
        }
        
        // Buscar controladores OHCI (USB 1.1)
        let ohci_devices = pci_manager.find_devices_by_class_subclass(0x0C, 0x10);
        for pci_device in ohci_devices {
            self.controllers.push(UsbControllerInfo {
                controller_type: UsbControllerType::OHCI,
                pci_device,
                port_count: 2, // Estimación por defecto
                is_active: true,
                last_port_check: get_current_timestamp(),
            });
        }
        
        // Buscar controladores UHCI (USB 1.1)
        let uhci_devices = pci_manager.find_devices_by_class_subclass(0x0C, 0x00);
        for pci_device in uhci_devices {
            self.controllers.push(UsbControllerInfo {
                controller_type: UsbControllerType::UHCI,
                pci_device,
                port_count: 2, // Estimación por defecto
                is_active: true,
                last_port_check: get_current_timestamp(),
            });
        }
        
        Ok(())
    }

    /// Inicializar estado inicial de dispositivos
    fn initialize_device_state(&mut self) -> Result<(), UsbError> {
        serial_write_str("USB_HOTPLUG: Inicializando estado de dispositivos...\n");
        
        // Simular algunos dispositivos conectados inicialmente
        // En un sistema real, esto vendría del hardware
        
        // Simular teclado USB
        let keyboard = UsbDeviceInfo::new(
            self.get_next_device_id(),
            0x046D, // Logitech
            0xC31C, // Teclado
            0x03,   // HID
            0x01,   // Boot Interface
            0x01,   // Keyboard
            1,      // Puerto 1
            UsbControllerType::EHCI,
            UsbDeviceSpeed::High,
        );
        self.connected_devices.insert(keyboard.device_id, keyboard.clone());
        self.add_event(UsbEvent::device_connected(keyboard, get_current_timestamp()));
        
        // Simular mouse USB
        let mouse = UsbDeviceInfo::new(
            self.get_next_device_id(),
            0x046D, // Logitech
            0xC077, // Mouse
            0x03,   // HID
            0x01,   // Boot Interface
            0x02,   // Mouse
            2,      // Puerto 2
            UsbControllerType::EHCI,
            UsbDeviceSpeed::High,
        );
        self.connected_devices.insert(mouse.device_id, mouse.clone());
        self.add_event(UsbEvent::device_connected(mouse, get_current_timestamp()));
        
        // Simular USB hub
        let hub = UsbDeviceInfo::new(
            self.get_next_device_id(),
            0x8086, // Intel
            0x1E2D, // USB Hub
            0x09,   // Hub
            0x00,   // No subclass
            0x00,   // No protocol
            3,      // Puerto 3
            UsbControllerType::XHCI,
            UsbDeviceSpeed::Super,
        );
        self.connected_devices.insert(hub.device_id, hub.clone());
        self.add_event(UsbEvent::device_connected(hub, get_current_timestamp()));
        
        serial_write_str(&alloc::format!(
            "USB_HOTPLUG: Estado inicial - {} dispositivos detectados\n",
            self.connected_devices.len()
        ));
        
        Ok(())
    }

    /// Obtener siguiente ID de dispositivo
    fn get_next_device_id(&self) -> u32 {
        self.device_counter.fetch_add(1, Ordering::SeqCst)
    }

    /// Agregar evento a la cola
    fn add_event(&mut self, event: UsbEvent) {
        if self.event_queue.len() >= 256 { // Limitar tamaño de cola
            self.event_queue.remove(0); // Remover evento más antiguo
        }
        
        self.event_queue.push(event);
        
        if let Some(last_event) = self.event_queue.last() {
            log_usb_event(last_event);
        }
    }

    /// Procesar eventos de hot-plug
    pub fn process_hotplug_events(&mut self) -> Result<(), UsbError> {
        if !self.monitoring_enabled.load(Ordering::SeqCst) {
            return Ok(());
        }
        
        let current_time = get_current_timestamp();
        let last_check = self.last_check_time.load(Ordering::SeqCst) as u64;
        
        // Verificar si es tiempo de hacer una verificación
        if current_time - last_check < 1000 { // 1 segundo
            return Ok(());
        }
        
        self.last_check_time.store(current_time as u32, Ordering::SeqCst);
        
        // Simular detección de nuevos dispositivos
        self.simulate_device_detection()?;
        
        // Procesar eventos pendientes
        self.process_pending_events()?;
        
        Ok(())
    }

    /// Simular detección de dispositivos (para testing)
    fn simulate_device_detection(&mut self) -> Result<(), UsbError> {
        // En un sistema real, esto leería los registros de estado de los puertos
        // Por ahora, simulamos algunos eventos aleatorios
        
        let device_count = self.connected_devices.len();
        
        // Simular conexión de dispositivo ocasionalmente
        if device_count < 5 && (get_current_timestamp() % 10) == 0 {
            let new_device = self.simulate_new_device_connection()?;
            self.connected_devices.insert(new_device.device_id, new_device.clone());
            self.add_event(UsbEvent::device_connected(new_device, get_current_timestamp()));
        }
        
        // Simular desconexión de dispositivo ocasionalmente
        if device_count > 2 && (get_current_timestamp() % 15) == 0 {
            if let Some((device_id, device)) = self.connected_devices.iter().next() {
                let device_id = *device_id;
                let device = device.clone();
                self.connected_devices.remove(&device_id);
                self.add_event(UsbEvent::device_disconnected(device, get_current_timestamp()));
            }
        }
        
        Ok(())
    }

    /// Simular conexión de nuevo dispositivo
    fn simulate_new_device_connection(&self) -> Result<UsbDeviceInfo, UsbError> {
        let device_id = self.get_next_device_id();
        
        // Simular diferentes tipos de dispositivos
        let device_type = (device_id % 4) as u8;
        
        match device_type {
            0 => Ok(UsbDeviceInfo::new(
                device_id,
                0x058F, // Alcor Micro
                0x6387, // USB Mass Storage
                0x08,   // Mass Storage
                0x06,   // SCSI
                0x50,   // Bulk-Only
                4,      // Puerto 4
                UsbControllerType::XHCI,
                UsbDeviceSpeed::Super,
            )),
            1 => Ok(UsbDeviceInfo::new(
                device_id,
                0x045E, // Microsoft
                0x00CB, // Xbox Controller
                0x03,   // HID
                0x00,   // No subclass
                0x00,   // No protocol
                5,      // Puerto 5
                UsbControllerType::EHCI,
                UsbDeviceSpeed::High,
            )),
            2 => Ok(UsbDeviceInfo::new(
                device_id,
                0x0BDA, // Realtek
                0x8176, // USB WiFi
                0x02,   // Communications
                0x06,   // Ethernet Networking
                0x00,   // No protocol
                6,      // Puerto 6
                UsbControllerType::XHCI,
                UsbDeviceSpeed::High,
            )),
            _ => Ok(UsbDeviceInfo::new(
                device_id,
                0x1BCF, // Sunplus Innovation
                0x2B8A, // USB Camera
                0x0E,   // Video
                0x01,   // Video Control
                0x00,   // No protocol
                7,      // Puerto 7
                UsbControllerType::XHCI,
                UsbDeviceSpeed::High,
            )),
        }
    }

    /// Procesar eventos pendientes
    fn process_pending_events(&mut self) -> Result<(), UsbError> {
        while let Some(event) = self.event_queue.pop() {
            self.handle_event(event)?;
        }
        Ok(())
    }

    /// Manejar evento USB
    fn handle_event(&mut self, event: UsbEvent) -> Result<(), UsbError> {
        match event.event_type {
            UsbEventType::DeviceConnected => {
                if let Some(ref device) = event.device_info {
                    serial_write_str(&alloc::format!(
                        "USB_HOTPLUG: Procesando conexión - {} {} en puerto {}\n",
                        device.get_vendor_name(),
                        device.get_class_name(),
                        device.port_number
                    ));
                    
                    // Aquí se podría inicializar el driver específico del dispositivo
                    self.initialize_device_driver(device)?;
                }
            }
            UsbEventType::DeviceDisconnected => {
                if let Some(ref device) = event.device_info {
                    serial_write_str(&alloc::format!(
                        "USB_HOTPLUG: Procesando desconexión - {} {} del puerto {}\n",
                        device.get_vendor_name(),
                        device.get_class_name(),
                        device.port_number
                    ));
                    
                    // Aquí se podría limpiar el driver del dispositivo
                    self.cleanup_device_driver(device)?;
                }
            }
            UsbEventType::DeviceError => {
                if let Some(ref error) = event.error {
                    serial_write_str(&alloc::format!(
                        "USB_HOTPLUG: Error en puerto {} - {:?}\n",
                        event.port_number,
                        error
                    ));
                }
            }
            UsbEventType::PortStatusChanged => {
                serial_write_str(&alloc::format!(
                    "USB_HOTPLUG: Estado del puerto {} cambiado\n",
                    event.port_number
                ));
            }
            UsbEventType::PowerStateChanged => {
                serial_write_str(&alloc::format!(
                    "USB_HOTPLUG: Estado de energía del puerto {} cambiado\n",
                    event.port_number
                ));
            }
        }
        
        Ok(())
    }

    /// Inicializar driver del dispositivo
    fn initialize_device_driver(&self, device: &UsbDeviceInfo) -> Result<(), UsbError> {
        match device.device_class {
            0x03 => { // HID
                serial_write_str("USB_HOTPLUG: Inicializando driver HID\n");
                // Aquí se inicializaría el driver HID específico
            }
            0x08 => { // Mass Storage
                serial_write_str("USB_HOTPLUG: Inicializando driver Mass Storage\n");
                // Aquí se inicializaría el driver de almacenamiento masivo
            }
            0x09 => { // Hub
                serial_write_str("USB_HOTPLUG: Inicializando driver Hub\n");
                // Aquí se inicializaría el driver de hub
            }
            _ => {
                serial_write_str(&alloc::format!(
                    "USB_HOTPLUG: Clase de dispositivo {} no soportada aún\n",
                    device.device_class
                ));
            }
        }
        
        Ok(())
    }

    /// Limpiar driver del dispositivo
    fn cleanup_device_driver(&self, device: &UsbDeviceInfo) -> Result<(), UsbError> {
        serial_write_str(&alloc::format!(
            "USB_HOTPLUG: Limpiando driver para {} {}\n",
            device.get_vendor_name(),
            device.get_class_name()
        ));
        
        // Aquí se limpiaría el driver específico del dispositivo
        Ok(())
    }

    /// Obtener lista de dispositivos conectados
    pub fn get_connected_devices(&self) -> Vec<UsbDeviceInfo> {
        self.connected_devices.values().cloned().collect()
    }

    /// Obtener número de dispositivos conectados
    pub fn get_device_count(&self) -> usize {
        self.connected_devices.len()
    }

    /// Obtener estadísticas del sistema
    pub fn get_system_stats(&self) -> UsbHotPlugStats {
        UsbHotPlugStats {
            total_devices: self.connected_devices.len(),
            total_controllers: self.controllers.len(),
            active_controllers: self.controllers.iter().filter(|c| c.is_active).count(),
            events_processed: self.event_queue.len(),
            monitoring_enabled: self.monitoring_enabled.load(Ordering::SeqCst),
        }
    }

    /// Detener monitoreo
    pub fn stop_monitoring(&self) {
        self.monitoring_enabled.store(false, Ordering::SeqCst);
        serial_write_str("USB_HOTPLUG: Monitoreo detenido\n");
    }

    /// Reiniciar monitoreo
    pub fn start_monitoring(&self) {
        self.monitoring_enabled.store(true, Ordering::SeqCst);
        serial_write_str("USB_HOTPLUG: Monitoreo iniciado\n");
    }
}

/// Estadísticas del sistema de hot-plug
#[derive(Debug, Clone)]
pub struct UsbHotPlugStats {
    pub total_devices: usize,
    pub total_controllers: usize,
    pub active_controllers: usize,
    pub events_processed: usize,
    pub monitoring_enabled: bool,
}

/// Función principal de hot-plug USB
pub fn usb_hotplug_main() {
    serial_write_str("USB_HOTPLUG: Iniciando sistema de hot-plug USB\n");
    
    let mut hotplug_manager = UsbHotPlugManager::new();
    let config = UsbHotPlugConfig::default();
    
    if let Err(e) = hotplug_manager.initialize(config) {
        serial_write_str(&alloc::format!(
            "USB_HOTPLUG: Error al inicializar: {:?}\n",
            e
        ));
        return;
    }
    
    // Procesar eventos iniciales
    if let Err(e) = hotplug_manager.process_hotplug_events() {
        serial_write_str(&alloc::format!(
            "USB_HOTPLUG: Error al procesar eventos: {:?}\n",
            e
        ));
    }
    
    // Mostrar estadísticas
    let stats = hotplug_manager.get_system_stats();
    serial_write_str(&alloc::format!(
        "USB_HOTPLUG: Sistema listo - {} dispositivos, {} controladores\n",
        stats.total_devices,
        stats.total_controllers
    ));
    
    serial_write_str("USB_HOTPLUG: Sistema de hot-plug USB iniciado\n");
}
