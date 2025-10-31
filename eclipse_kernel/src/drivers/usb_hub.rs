#![no_std]

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr;

/// Driver de USB Hub para Eclipse OS
/// Implementa soporte para concentradores USB (USB Hubs)

/// Estados de un puerto USB
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbPortState {
    Disabled,
    Enabled,
    Suspended,
    OverCurrent,
    Reset,
    PowerOff,
    PowerOn,
}

impl UsbPortState {
    pub fn as_str(&self) -> &'static str {
        match self {
            UsbPortState::Disabled => "Disabled",
            UsbPortState::Enabled => "Enabled",
            UsbPortState::Suspended => "Suspended",
            UsbPortState::OverCurrent => "OverCurrent",
            UsbPortState::Reset => "Reset",
            UsbPortState::PowerOff => "PowerOff",
            UsbPortState::PowerOn => "PowerOn",
        }
    }
}

/// Características de un puerto USB
#[derive(Debug, Clone)]
pub struct UsbPortFeatures {
    pub port_number: u8,
    pub state: UsbPortState,
    pub power_control: bool,
    pub over_current_protection: bool,
    pub port_indicator: bool,
    pub port_connection: bool,
    pub port_enable: bool,
    pub port_suspend: bool,
    pub port_reset: bool,
    pub port_power: bool,
    pub low_speed_device: bool,
    pub high_speed_device: bool,
    pub super_speed_device: bool,
}

impl UsbPortFeatures {
    pub fn new(port_number: u8) -> Self {
        Self {
            port_number,
            state: UsbPortState::Disabled,
            power_control: false,
            over_current_protection: false,
            port_indicator: false,
            port_connection: false,
            port_enable: false,
            port_suspend: false,
            port_reset: false,
            port_power: false,
            low_speed_device: false,
            high_speed_device: false,
            super_speed_device: false,
        }
    }
}

/// Información del USB Hub
#[derive(Debug, Clone)]
pub struct UsbHubInfo {
    pub vendor_id: u16,
    pub product_id: u16,
    pub manufacturer: String,
    pub product: String,
    pub version: u16,
    pub device_address: u8,
    pub num_ports: u8,
    pub hub_type: UsbHubType,
    pub power_switching: UsbPowerSwitching,
    pub over_current_protection: UsbOverCurrentProtection,
    pub tt_think_time: u8,
    pub port_indicators: bool,
    pub compound_device: bool,
}

/// Tipo de hub USB
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbHubType {
    RootHub,
    Usb2Hub,
    Usb3Hub,
    MultiTtHub,
    SingleTtHub,
}

/// Modo de conmutación de energía
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbPowerSwitching {
    Global,     // Todos los puertos se encienden/apagan juntos
    Individual, // Cada puerto se controla individualmente
}

/// Protección contra sobrecorriente
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbOverCurrentProtection {
    Global,     // Protección global
    Individual, // Protección por puerto
}

/// Evento del hub USB
#[derive(Debug, Clone, PartialEq)]
pub enum UsbHubEvent {
    PortConnected {
        port: u8,
        device_speed: UsbDeviceSpeed,
    },
    PortDisconnected {
        port: u8,
    },
    PortEnabled {
        port: u8,
    },
    PortDisabled {
        port: u8,
    },
    PortSuspended {
        port: u8,
    },
    PortResumed {
        port: u8,
    },
    PortReset {
        port: u8,
    },
    OverCurrent {
        port: u8,
    },
    HubError {
        error: String,
    },
}

/// Velocidad del dispositivo USB
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbDeviceSpeed {
    LowSpeed,       // 1.5 Mbps
    FullSpeed,      // 12 Mbps
    HighSpeed,      // 480 Mbps
    SuperSpeed,     // 5 Gbps
    SuperSpeedPlus, // 10 Gbps
}

impl UsbDeviceSpeed {
    pub fn as_str(&self) -> &'static str {
        match self {
            UsbDeviceSpeed::LowSpeed => "Low Speed (1.5 Mbps)",
            UsbDeviceSpeed::FullSpeed => "Full Speed (12 Mbps)",
            UsbDeviceSpeed::HighSpeed => "High Speed (480 Mbps)",
            UsbDeviceSpeed::SuperSpeed => "Super Speed (5 Gbps)",
            UsbDeviceSpeed::SuperSpeedPlus => "Super Speed+ (10 Gbps)",
        }
    }
}

/// Driver de USB Hub
#[derive(Debug)]
pub struct UsbHubDriver {
    pub info: UsbHubInfo,
    pub ports: Vec<UsbPortFeatures>,
    pub event_buffer: VecDeque<UsbHubEvent>,
    pub initialized: bool,
    pub error_count: u32,
    pub power_management: bool,
    pub remote_wakeup: bool,
}

impl UsbHubDriver {
    /// Crear nuevo driver de USB Hub
    pub fn new(info: UsbHubInfo) -> Self {
        let num_ports = info.num_ports as usize;
        let mut ports = Vec::with_capacity(num_ports);

        for i in 0..num_ports {
            ports.push(UsbPortFeatures::new(i as u8 + 1));
        }

        Self {
            info,
            ports,
            event_buffer: VecDeque::new(),
            initialized: false,
            error_count: 0,
            power_management: true,
            remote_wakeup: false,
        }
    }

    /// Inicializar el hub USB
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        // Configurar el hub
        self.configure_hub()?;

        // Inicializar todos los puertos
        self.initialize_ports()?;

        // Habilitar detección de cambios
        self.enable_port_change_detection()?;

        self.initialized = true;
        Ok(())
    }

    /// Configurar el hub
    fn configure_hub(&mut self) -> Result<(), &'static str> {
        // En una implementación real, aquí se configuraría el hub USB
        // Por ahora simulamos la configuración
        Ok(())
    }

    /// Inicializar todos los puertos
    fn initialize_ports(&mut self) -> Result<(), &'static str> {
        for port in &mut self.ports {
            port.state = UsbPortState::PowerOff;
            port.power_control = true;
            port.over_current_protection = true;
            port.port_indicator = true;
        }
        Ok(())
    }

    /// Habilitar detección de cambios en puertos
    fn enable_port_change_detection(&mut self) -> Result<(), &'static str> {
        // En una implementación real, aquí se habilitaría la detección de cambios
        // Por ahora simulamos la habilitación
        Ok(())
    }

    /// Procesar cambios en los puertos
    pub fn process_port_changes(&mut self) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("Hub no inicializado");
        }

        // En una implementación real, aquí se leerían los registros de estado del hub
        // Por ahora simulamos algunos cambios
        self.simulate_port_changes();

        Ok(())
    }

    /// Simular cambios en los puertos (para demostración)
    fn simulate_port_changes(&mut self) {
        // Simular conexión de dispositivo en puerto 1
        if let Some(port) = self.ports.get_mut(0) {
            if port.state == UsbPortState::PowerOff {
                port.state = UsbPortState::Enabled;
                port.port_connection = true;
                port.high_speed_device = true;

                let event = UsbHubEvent::PortConnected {
                    port: 1,
                    device_speed: UsbDeviceSpeed::HighSpeed,
                };
                self.event_buffer.push_back(event);
            }
        }

        // Simular conexión de dispositivo en puerto 2
        if let Some(port) = self.ports.get_mut(1) {
            if port.state == UsbPortState::PowerOff {
                port.state = UsbPortState::Enabled;
                port.port_connection = true;
                // port.full_speed_device = true; // Campo no existe

                let event = UsbHubEvent::PortConnected {
                    port: 2,
                    device_speed: UsbDeviceSpeed::FullSpeed,
                };
                self.event_buffer.push_back(event);
            }
        }
    }

    /// Habilitar puerto
    pub fn enable_port(&mut self, port_number: u8) -> Result<(), &'static str> {
        if let Some(port) = self.ports.get_mut((port_number - 1) as usize) {
            port.state = UsbPortState::Enabled;
            port.port_enable = true;
            port.port_power = true;

            let event = UsbHubEvent::PortEnabled { port: port_number };
            self.event_buffer.push_back(event);
            Ok(())
        } else {
            Err("Puerto no encontrado")
        }
    }

    /// Deshabilitar puerto
    pub fn disable_port(&mut self, port_number: u8) -> Result<(), &'static str> {
        if let Some(port) = self.ports.get_mut((port_number - 1) as usize) {
            port.state = UsbPortState::Disabled;
            port.port_enable = false;
            port.port_power = false;

            let event = UsbHubEvent::PortDisabled { port: port_number };
            self.event_buffer.push_back(event);
            Ok(())
        } else {
            Err("Puerto no encontrado")
        }
    }

    /// Resetear puerto
    pub fn reset_port(&mut self, port_number: u8) -> Result<(), &'static str> {
        if let Some(port) = self.ports.get_mut((port_number - 1) as usize) {
            port.state = UsbPortState::Reset;
            port.port_reset = true;

            let event = UsbHubEvent::PortReset { port: port_number };
            self.event_buffer.push_back(event);
            Ok(())
        } else {
            Err("Puerto no encontrado")
        }
    }

    /// Suspender puerto
    pub fn suspend_port(&mut self, port_number: u8) -> Result<(), &'static str> {
        if let Some(port) = self.ports.get_mut((port_number - 1) as usize) {
            port.state = UsbPortState::Suspended;
            port.port_suspend = true;

            let event = UsbHubEvent::PortSuspended { port: port_number };
            self.event_buffer.push_back(event);
            Ok(())
        } else {
            Err("Puerto no encontrado")
        }
    }

    /// Reanudar puerto
    pub fn resume_port(&mut self, port_number: u8) -> Result<(), &'static str> {
        if let Some(port) = self.ports.get_mut((port_number - 1) as usize) {
            port.state = UsbPortState::Enabled;
            port.port_suspend = false;

            let event = UsbHubEvent::PortResumed { port: port_number };
            self.event_buffer.push_back(event);
            Ok(())
        } else {
            Err("Puerto no encontrado")
        }
    }

    /// Obtener siguiente evento
    pub fn get_next_event(&mut self) -> Option<UsbHubEvent> {
        self.event_buffer.pop_front()
    }

    /// Verificar si hay eventos pendientes
    pub fn has_events(&self) -> bool {
        !self.event_buffer.is_empty()
    }

    /// Obtener información de un puerto
    pub fn get_port_info(&self, port_number: u8) -> Option<&UsbPortFeatures> {
        self.ports.get((port_number - 1) as usize)
    }

    /// Obtener información de todos los puertos
    pub fn get_all_ports(&self) -> &[UsbPortFeatures] {
        &self.ports
    }

    /// Obtener número de puertos
    pub fn get_port_count(&self) -> u8 {
        self.info.num_ports
    }

    /// Obtener puertos activos
    pub fn get_active_ports(&self) -> Vec<u8> {
        let mut active_ports = Vec::new();
        for (i, port) in self.ports.iter().enumerate() {
            if port.port_connection && port.port_enable {
                active_ports.push(i as u8 + 1);
            }
        }
        active_ports
    }

    /// Obtener estadísticas del hub
    pub fn get_stats(&self) -> UsbHubStats {
        UsbHubStats {
            total_ports: self.info.num_ports,
            active_ports: self.get_active_ports().len() as u8,
            error_count: self.error_count,
            power_management: self.power_management,
            remote_wakeup: self.remote_wakeup,
        }
    }

    /// Verificar si el hub está inicializado
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Limpiar buffer de eventos
    pub fn clear_events(&mut self) {
        self.event_buffer.clear();
    }

    /// Obtener información del hub
    pub fn get_info(&self) -> &UsbHubInfo {
        &self.info
    }
}

/// Estadísticas del USB Hub
#[derive(Debug, Clone)]
pub struct UsbHubStats {
    pub total_ports: u8,
    pub active_ports: u8,
    pub error_count: u32,
    pub power_management: bool,
    pub remote_wakeup: bool,
}

/// Función de conveniencia para crear un hub USB
pub fn create_usb_hub(info: UsbHubInfo) -> UsbHubDriver {
    UsbHubDriver::new(info)
}

/// Función de conveniencia para crear un hub USB estándar
pub fn create_standard_usb_hub(
    vendor_id: u16,
    product_id: u16,
    device_address: u8,
    num_ports: u8,
) -> UsbHubDriver {
    let info = UsbHubInfo {
        vendor_id,
        product_id,
        manufacturer: String::from("Generic"),
        product: String::from("USB Hub"),
        version: 0x0100,
        device_address,
        num_ports,
        hub_type: UsbHubType::Usb2Hub,
        power_switching: UsbPowerSwitching::Individual,
        over_current_protection: UsbOverCurrentProtection::Individual,
        tt_think_time: 8,
        port_indicators: true,
        compound_device: false,
    };

    UsbHubDriver::new(info)
}
