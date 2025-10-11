//! Driver USB Network para Eclipse OS
//! 
//! Implementa soporte para dispositivos de red USB según USB CDC (Communication Device Class)

use crate::debug::serial_write_str;
use crate::drivers::usb_events::{UsbDeviceInfo, UsbControllerType, UsbDeviceSpeed};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Configuración de red USB
#[derive(Debug, Clone)]
pub struct UsbNetworkConfig {
    pub interface_type: NetworkInterfaceType,
    pub max_speed: u32,      // Mbps
    pub mtu: u32,
    pub mac_address: [u8; 6],
    pub ip_address: [u8; 4],
    pub subnet_mask: [u8; 4],
    pub gateway: [u8; 4],
}

/// Tipos de interfaz de red
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NetworkInterfaceType {
    Ethernet,   // Ethernet over USB
    WiFi,       // WiFi USB adapter
    Bluetooth,  // Bluetooth USB adapter
    Cellular,   // USB modem/cellular
    Unknown,
}

/// Estados del dispositivo de red
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NetworkDeviceState {
    Disconnected,
    Connected,
    Initialized,
    Up,         // Interface up
    Down,       // Interface down
    Error,
}

/// Información del dispositivo de red USB
#[derive(Debug, Clone)]
pub struct UsbNetworkDevice {
    pub device_info: UsbDeviceInfo,
    pub config: UsbNetworkConfig,
    pub state: NetworkDeviceState,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub link_speed: u32,     // Mbps actual
    pub link_up: bool,
    pub device_id: u32,
}

/// Driver de red USB
pub struct UsbNetworkDriver {
    devices: Vec<UsbNetworkDevice>,
    current_device_id: AtomicU32,
    driver_initialized: AtomicBool,
    total_bytes_sent: AtomicU64,
    total_bytes_received: AtomicU64,
}

impl UsbNetworkDriver {
    /// Crear nuevo driver de red USB
    pub fn new() -> Self {
        serial_write_str("USB_NETWORK: Inicializando driver de red USB\n");
        
        Self {
            devices: Vec::new(),
            current_device_id: AtomicU32::new(0),
            driver_initialized: AtomicBool::new(false),
            total_bytes_sent: AtomicU64::new(0),
            total_bytes_received: AtomicU64::new(0),
        }
    }

    /// Inicializar el driver
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        serial_write_str("USB_NETWORK: Configurando driver de red USB...\n");
        
        // Simular detección de dispositivos de red USB
        self.detect_network_devices()?;
        
        self.driver_initialized.store(true, Ordering::SeqCst);
        serial_write_str("USB_NETWORK: Driver de red USB inicializado\n");
        
        Ok(())
    }

    /// Detectar dispositivos de red USB
    fn detect_network_devices(&mut self) -> Result<(), &'static str> {
        serial_write_str("USB_NETWORK: Detectando dispositivos de red USB...\n");
        
        // Simular dispositivos de red conectados
        let network_devices = vec![
            // Adaptador Ethernet USB
            UsbNetworkDevice {
                device_info: UsbDeviceInfo::new(
                    self.get_next_device_id(),
                    0x0BDA, // Realtek
                    0x8153, // USB 3.0 Gigabit Ethernet
                    0x02,   // Communications Class
                    0x06,   // Ethernet Networking
                    0x00,   // No protocol
                    1,      // Puerto 1
                    UsbControllerType::XHCI,
                    UsbDeviceSpeed::Super,
                ),
                config: UsbNetworkConfig {
                    interface_type: NetworkInterfaceType::Ethernet,
                    max_speed: 1000, // 1 Gbps
                    mtu: 1500,
                    mac_address: [0x00, 0x15, 0x5D, 0x01, 0x02, 0x03],
                    ip_address: [192, 168, 1, 100],
                    subnet_mask: [255, 255, 255, 0],
                    gateway: [192, 168, 1, 1],
                },
                state: NetworkDeviceState::Connected,
                bytes_sent: 0,
                bytes_received: 0,
                packets_sent: 0,
                packets_received: 0,
                link_speed: 1000,
                link_up: true,
                device_id: self.get_next_device_id(),
            },
            // Adaptador WiFi USB
            UsbNetworkDevice {
                device_info: UsbDeviceInfo::new(
                    self.get_next_device_id(),
                    0x0BDA, // Realtek
                    0x8176, // USB WiFi Adapter
                    0x02,   // Communications Class
                    0x06,   // Ethernet Networking
                    0x00,   // No protocol
                    2,      // Puerto 2
                    UsbControllerType::XHCI,
                    UsbDeviceSpeed::High,
                ),
                config: UsbNetworkConfig {
                    interface_type: NetworkInterfaceType::WiFi,
                    max_speed: 150, // 150 Mbps
                    mtu: 1500,
                    mac_address: [0x00, 0x15, 0x5D, 0x02, 0x04, 0x05],
                    ip_address: [192, 168, 0, 150],
                    subnet_mask: [255, 255, 255, 0],
                    gateway: [192, 168, 0, 1],
                },
                state: NetworkDeviceState::Connected,
                bytes_sent: 0,
                bytes_received: 0,
                packets_sent: 0,
                packets_received: 0,
                link_speed: 150,
                link_up: true,
                device_id: self.get_next_device_id(),
            },
            // Modem USB
            UsbNetworkDevice {
                device_info: UsbDeviceInfo::new(
                    self.get_next_device_id(),
                    0x12D1, // Huawei
                    0x1506, // USB Modem
                    0x02,   // Communications Class
                    0x06,   // Ethernet Networking
                    0x00,   // No protocol
                    3,      // Puerto 3
                    UsbControllerType::EHCI,
                    UsbDeviceSpeed::High,
                ),
                config: UsbNetworkConfig {
                    interface_type: NetworkInterfaceType::Cellular,
                    max_speed: 50, // 50 Mbps (4G)
                    mtu: 1500,
                    mac_address: [0x00, 0x15, 0x5D, 0x03, 0x06, 0x07],
                    ip_address: [10, 0, 0, 1],
                    subnet_mask: [255, 0, 0, 0],
                    gateway: [10, 0, 0, 1],
                },
                state: NetworkDeviceState::Connected,
                bytes_sent: 0,
                bytes_received: 0,
                packets_sent: 0,
                packets_received: 0,
                link_speed: 50,
                link_up: true,
                device_id: self.get_next_device_id(),
            },
        ];

        for device in network_devices {
            self.devices.push(device.clone());
            let interface_name = match device.config.interface_type {
                NetworkInterfaceType::Ethernet => "Ethernet",
                NetworkInterfaceType::WiFi => "WiFi",
                NetworkInterfaceType::Cellular => "Cellular",
                _ => "Unknown",
            };
            
            serial_write_str(&alloc::format!(
                "USB_NETWORK: Dispositivo detectado - {} {} ({} Mbps, {}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X})\n",
                device.device_info.get_vendor_name(),
                interface_name,
                device.config.max_speed,
                device.config.mac_address[0],
                device.config.mac_address[1],
                device.config.mac_address[2],
                device.config.mac_address[3],
                device.config.mac_address[4],
                device.config.mac_address[5]
            ));
        }

        serial_write_str(&alloc::format!(
            "USB_NETWORK: {} dispositivos de red detectados\n",
            self.devices.len()
        ));

        Ok(())
    }

    /// Obtener siguiente ID de dispositivo
    fn get_next_device_id(&self) -> u32 {
        self.current_device_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Inicializar dispositivo de red
    pub fn initialize_device(&mut self, device_id: u32) -> Result<(), &'static str> {
        if let Some(device) = self.devices.iter_mut().find(|d| d.device_id == device_id) {
            let interface_name = match device.config.interface_type {
                NetworkInterfaceType::Ethernet => "Ethernet",
                NetworkInterfaceType::WiFi => "WiFi",
                NetworkInterfaceType::Cellular => "Cellular",
                _ => "Unknown",
            };

            serial_write_str(&alloc::format!(
                "USB_NETWORK: Inicializando dispositivo {} - {} {}\n",
                device_id,
                device.device_info.get_vendor_name(),
                interface_name
            ));

            // Configurar parámetros de red
            Self::configure_network_device_static(device)?;
            
            device.state = NetworkDeviceState::Initialized;
            serial_write_str("USB_NETWORK: Dispositivo inicializado correctamente\n");
            
            Ok(())
        } else {
            Err("Dispositivo de red no encontrado")
        }
    }

    /// Configurar dispositivo de red
    fn configure_network_device_static(device: &mut UsbNetworkDevice) -> Result<(), &'static str> {
        serial_write_str(&alloc::format!(
            "USB_NETWORK: Configurando {} - {} Mbps, MTU: {}, IP: {}.{}.{}.{}\n",
            device.device_info.get_vendor_name(),
            device.config.max_speed,
            device.config.mtu,
            device.config.ip_address[0],
            device.config.ip_address[1],
            device.config.ip_address[2],
            device.config.ip_address[3]
        ));

        // Simular configuración de hardware
        // En un sistema real, esto configuraría los registros del controlador USB
        
        Ok(())
    }

    /// Activar interfaz de red
    pub fn bring_up_interface(&mut self, device_id: u32) -> Result<(), &'static str> {
        if let Some(device) = self.devices.iter_mut().find(|d| d.device_id == device_id) {
            if device.state != NetworkDeviceState::Initialized {
                return Err("Dispositivo no inicializado");
            }

            device.state = NetworkDeviceState::Up;
            device.link_up = true;
            
            serial_write_str(&alloc::format!(
                "USB_NETWORK: Interfaz {} activada ({} Mbps)\n",
                device_id,
                device.link_speed
            ));

            // En un sistema real, esto activaría la interfaz de red
            
            Ok(())
        } else {
            Err("Dispositivo de red no encontrado")
        }
    }

    /// Desactivar interfaz de red
    pub fn bring_down_interface(&mut self, device_id: u32) -> Result<(), &'static str> {
        if let Some(device) = self.devices.iter_mut().find(|d| d.device_id == device_id) {
            if device.state != NetworkDeviceState::Up {
                return Err("Interfaz no está activa");
            }

            device.state = NetworkDeviceState::Down;
            device.link_up = false;
            
            serial_write_str(&alloc::format!(
                "USB_NETWORK: Interfaz {} desactivada\n",
                device_id
            ));

            // En un sistema real, esto desactivaría la interfaz de red
            
            Ok(())
        } else {
            Err("Dispositivo de red no encontrado")
        }
    }

    /// Enviar paquete de red
    pub fn send_packet(&mut self, device_id: u32, packet: &[u8]) -> Result<(), &'static str> {
        if let Some(device) = self.devices.iter_mut().find(|d| d.device_id == device_id) {
            if device.state != NetworkDeviceState::Up {
                return Err("Interfaz no está activa");
            }

            if packet.len() > device.config.mtu as usize {
                return Err("Paquete excede MTU");
            }

            device.bytes_sent += packet.len() as u64;
            device.packets_sent += 1;
            self.total_bytes_sent.fetch_add(packet.len() as u64, Ordering::SeqCst);
            
            serial_write_str(&alloc::format!(
                "USB_NETWORK: Enviado paquete de {} bytes por dispositivo {} (Total: {} bytes, {} paquetes)\n",
                packet.len(),
                device_id,
                device.bytes_sent,
                device.packets_sent
            ));

            // En un sistema real, esto enviaría el paquete al controlador USB
            
            Ok(())
        } else {
            Err("Dispositivo de red no encontrado")
        }
    }

    /// Recibir paquete de red
    pub fn receive_packet(&mut self, device_id: u32, buffer: &mut [u8]) -> Result<usize, &'static str> {
        if let Some(device) = self.devices.iter_mut().find(|d| d.device_id == device_id) {
            if device.state != NetworkDeviceState::Up {
                return Err("Interfaz no está activa");
            }

            // Simular recepción de paquete
            // En un sistema real, esto leería un paquete del controlador USB
            let packet_size = buffer.len().min(1500); // MTU típico
            
            device.bytes_received += packet_size as u64;
            device.packets_received += 1;
            self.total_bytes_received.fetch_add(packet_size as u64, Ordering::SeqCst);
            
            serial_write_str(&alloc::format!(
                "USB_NETWORK: Recibido paquete de {} bytes del dispositivo {} (Total: {} bytes, {} paquetes)\n",
                packet_size,
                device_id,
                device.bytes_received,
                device.packets_received
            ));

            Ok(packet_size)
        } else {
            Err("Dispositivo de red no encontrado")
        }
    }

    /// Configurar dirección IP
    pub fn set_ip_address(&mut self, device_id: u32, ip: [u8; 4], subnet: [u8; 4], gateway: [u8; 4]) -> Result<(), &'static str> {
        if let Some(device) = self.devices.iter_mut().find(|d| d.device_id == device_id) {
            device.config.ip_address = ip;
            device.config.subnet_mask = subnet;
            device.config.gateway = gateway;
            
            serial_write_str(&alloc::format!(
                "USB_NETWORK: IP del dispositivo {} configurada: {}.{}.{}.{}/{}.{}.{}.{} (Gateway: {}.{}.{}.{})\n",
                device_id,
                ip[0], ip[1], ip[2], ip[3],
                subnet[0], subnet[1], subnet[2], subnet[3],
                gateway[0], gateway[1], gateway[2], gateway[3]
            ));

            // En un sistema real, esto configuraría la dirección IP en el dispositivo
            
            Ok(())
        } else {
            Err("Dispositivo de red no encontrado")
        }
    }

    /// Obtener estadísticas de dispositivo
    pub fn get_device_stats(&self, device_id: u32) -> Result<&UsbNetworkDevice, &'static str> {
        self.devices.iter().find(|d| d.device_id == device_id).ok_or("Dispositivo no encontrado")
    }

    /// Obtener dispositivos de red disponibles
    pub fn get_network_devices(&self) -> Vec<&UsbNetworkDevice> {
        self.devices.iter().collect()
    }

    /// Obtener estadísticas globales del driver
    pub fn get_driver_stats(&self) -> UsbNetworkDriverStats {
        let total_devices = self.devices.len();
        let initialized_devices = self.devices.iter().filter(|d| d.state == NetworkDeviceState::Initialized).count();
        let up_devices = self.devices.iter().filter(|d| d.state == NetworkDeviceState::Up).count();
        let ethernet_devices = self.devices.iter().filter(|d| d.config.interface_type == NetworkInterfaceType::Ethernet).count();
        let wifi_devices = self.devices.iter().filter(|d| d.config.interface_type == NetworkInterfaceType::WiFi).count();
        let cellular_devices = self.devices.iter().filter(|d| d.config.interface_type == NetworkInterfaceType::Cellular).count();

        UsbNetworkDriverStats {
            total_devices: total_devices as u32,
            initialized_devices: initialized_devices as u32,
            up_devices: up_devices as u32,
            ethernet_devices: ethernet_devices as u32,
            wifi_devices: wifi_devices as u32,
            cellular_devices: cellular_devices as u32,
            total_bytes_sent: self.total_bytes_sent.load(Ordering::SeqCst),
            total_bytes_received: self.total_bytes_received.load(Ordering::SeqCst),
            driver_initialized: self.driver_initialized.load(Ordering::SeqCst),
        }
    }
}

/// Estadísticas del driver de red USB
#[derive(Debug, Clone)]
pub struct UsbNetworkDriverStats {
    pub total_devices: u32,
    pub initialized_devices: u32,
    pub up_devices: u32,
    pub ethernet_devices: u32,
    pub wifi_devices: u32,
    pub cellular_devices: u32,
    pub total_bytes_sent: u64,
    pub total_bytes_received: u64,
    pub driver_initialized: bool,
}

/// Función principal del driver de red USB
pub fn usb_network_main() {
    serial_write_str("USB_NETWORK: Iniciando driver de red USB\n");
    
    let mut network_driver = UsbNetworkDriver::new();
    
    if let Err(e) = network_driver.initialize() {
        serial_write_str(&alloc::format!("USB_NETWORK: Error al inicializar: {}\n", e));
        return;
    }

    // Inicializar dispositivos detectados
    let device_ids: Vec<u32> = network_driver.get_network_devices().iter().map(|d| d.device_id).collect();
    for device_id in device_ids {
        if let Err(e) = network_driver.initialize_device(device_id) {
            serial_write_str(&alloc::format!("USB_NETWORK: Error al inicializar dispositivo {}: {}\n", device_id, e));
        } else {
            // Activar interfaz después de inicializar
            if let Err(e) = network_driver.bring_up_interface(device_id) {
                serial_write_str(&alloc::format!("USB_NETWORK: Error al activar interfaz {}: {}\n", device_id, e));
            }
        }
    }

    // Mostrar estadísticas
    let stats = network_driver.get_driver_stats();
    serial_write_str(&alloc::format!(
        "USB_NETWORK: Driver listo - {} dispositivos totales, {} inicializados, {} activos\n",
        stats.total_devices,
        stats.initialized_devices,
        stats.up_devices
    ));
}

/// Buffer de paquetes de red
const MAX_NETWORK_PACKETS: usize = 256;
const MAX_PACKET_SIZE: usize = 2048;

#[derive(Clone)]
pub struct NetworkPacket {
    pub data: Vec<u8>,
    pub device_id: u32,
    pub timestamp: u64,
}

/// Buffers de transmisión y recepción
pub struct NetworkBuffers {
    tx_queue: Vec<NetworkPacket>,
    rx_queue: Vec<NetworkPacket>,
}

impl NetworkBuffers {
    pub fn new() -> Self {
        Self {
            tx_queue: Vec::with_capacity(MAX_NETWORK_PACKETS),
            rx_queue: Vec::with_capacity(MAX_NETWORK_PACKETS),
        }
    }

    /// Encolar paquete para transmisión
    pub fn enqueue_tx(&mut self, packet: NetworkPacket) -> Result<(), &'static str> {
        if self.tx_queue.len() >= MAX_NETWORK_PACKETS {
            return Err("Cola de transmisión llena");
        }
        self.tx_queue.push(packet);
        Ok(())
    }

    /// Desencolar paquete para transmisión
    pub fn dequeue_tx(&mut self) -> Option<NetworkPacket> {
        if !self.tx_queue.is_empty() {
            Some(self.tx_queue.remove(0))
        } else {
            None
        }
    }

    /// Encolar paquete recibido
    pub fn enqueue_rx(&mut self, packet: NetworkPacket) -> Result<(), &'static str> {
        if self.rx_queue.len() >= MAX_NETWORK_PACKETS {
            return Err("Cola de recepción llena");
        }
        self.rx_queue.push(packet);
        Ok(())
    }

    /// Desencolar paquete recibido
    pub fn dequeue_rx(&mut self) -> Option<NetworkPacket> {
        if !self.rx_queue.is_empty() {
            Some(self.rx_queue.remove(0))
        } else {
            None
        }
    }

    /// Obtener tamaño de colas
    pub fn get_queue_sizes(&self) -> (usize, usize) {
        (self.tx_queue.len(), self.rx_queue.len())
    }
}

/// Buffers globales de red
static mut NETWORK_BUFFERS: Option<NetworkBuffers> = None;

/// Inicializar buffers de red
pub fn init_network_buffers() {
    unsafe {
        NETWORK_BUFFERS = Some(NetworkBuffers::new());
    }
}

/// Obtener buffers de red
pub fn get_network_buffers() -> Option<&'static mut NetworkBuffers> {
    unsafe { NETWORK_BUFFERS.as_mut() }
}
