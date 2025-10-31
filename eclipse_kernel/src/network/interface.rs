//! Gestión de interfaces de red
//!
//! Manejo de interfaces de red físicas y virtuales

#![allow(dead_code)] // Permitir código no utilizado - API completa del kernel

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use super::arp::MacAddress;
use super::ip::IpAddress;
use super::NetworkError;

/// Tipos de interfaz de red
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InterfaceType {
    Ethernet,
    Wifi,
    Loopback,
    Virtual,
    Unknown,
}

impl From<u8> for InterfaceType {
    fn from(value: u8) -> Self {
        match value {
            1 => InterfaceType::Ethernet,
            2 => InterfaceType::Wifi,
            3 => InterfaceType::Loopback,
            4 => InterfaceType::Virtual,
            _ => InterfaceType::Unknown,
        }
    }
}

/// Estados de interfaz
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InterfaceState {
    Down,
    Up,
    Unknown,
    Error,
}

impl From<u8> for InterfaceState {
    fn from(value: u8) -> Self {
        match value {
            0 => InterfaceState::Down,
            1 => InterfaceState::Up,
            2 => InterfaceState::Unknown,
            3 => InterfaceState::Error,
            _ => InterfaceState::Unknown,
        }
    }
}

/// Configuración de interfaz
#[derive(Debug, Clone)]
pub struct InterfaceConfig {
    pub ip_address: IpAddress,
    pub netmask: IpAddress,
    pub gateway: Option<IpAddress>,
    pub mtu: u16,
    pub promiscuous_mode: bool,
}

impl InterfaceConfig {
    /// Crear nueva configuración
    pub fn new(ip_address: IpAddress, netmask: IpAddress) -> Self {
        Self {
            ip_address,
            netmask,
            gateway: None,
            mtu: 1500,
            promiscuous_mode: false,
        }
    }

    /// Establecer gateway
    pub fn set_gateway(&mut self, gateway: IpAddress) {
        self.gateway = Some(gateway);
    }

    /// Establecer MTU
    pub fn set_mtu(&mut self, mtu: u16) {
        self.mtu = mtu;
    }

    /// Habilitar modo promiscuo
    pub fn enable_promiscuous(&mut self) {
        self.promiscuous_mode = true;
    }

    /// Deshabilitar modo promiscuo
    pub fn disable_promiscuous(&mut self) {
        self.promiscuous_mode = false;
    }
}

/// Información de interfaz
#[derive(Debug, Clone)]
pub struct InterfaceInfo {
    pub name: String,
    pub interface_type: InterfaceType,
    pub mac_address: MacAddress,
    pub config: InterfaceConfig,
    pub state: InterfaceState,
    pub index: u32,
    pub driver_name: String,
}

impl InterfaceInfo {
    /// Crear nueva información de interfaz
    pub fn new(
        name: String,
        interface_type: InterfaceType,
        mac_address: MacAddress,
        index: u32,
    ) -> Self {
        Self {
            name,
            interface_type,
            mac_address,
            config: InterfaceConfig::new(IpAddress::zero(), IpAddress::zero()),
            state: InterfaceState::Down,
            index,
            driver_name: String::new(),
        }
    }

    /// Establecer configuración
    pub fn set_config(&mut self, config: InterfaceConfig) {
        self.config = config;
    }

    /// Establecer estado
    pub fn set_state(&mut self, state: InterfaceState) {
        self.state = state;
    }

    /// Establecer nombre del driver
    pub fn set_driver_name(&mut self, driver_name: String) {
        self.driver_name = driver_name;
    }

    /// Verificar si la interfaz está activa
    pub fn is_active(&self) -> bool {
        self.state == InterfaceState::Up
    }

    /// Verificar si es interfaz de loopback
    pub fn is_loopback(&self) -> bool {
        self.interface_type == InterfaceType::Loopback
    }

    /// Obtener dirección de red
    pub fn get_network_address(&self) -> IpAddress {
        let network_bytes = [
            self.config.ip_address.bytes[0] & self.config.netmask.bytes[0],
            self.config.ip_address.bytes[1] & self.config.netmask.bytes[1],
            self.config.ip_address.bytes[2] & self.config.netmask.bytes[2],
            self.config.ip_address.bytes[3] & self.config.netmask.bytes[3],
        ];
        IpAddress::from_bytes(network_bytes)
    }

    /// Verificar si una IP pertenece a esta interfaz
    pub fn contains_ip(&self, ip: IpAddress) -> bool {
        if self.is_loopback() {
            return ip.is_loopback();
        }

        let network = self.get_network_address();
        let netmask = self.config.netmask;

        (ip.bytes[0] & netmask.bytes[0]) == network.bytes[0]
            && (ip.bytes[1] & netmask.bytes[1]) == network.bytes[1]
            && (ip.bytes[2] & netmask.bytes[2]) == network.bytes[2]
            && (ip.bytes[3] & netmask.bytes[3]) == network.bytes[3]
    }
}

/// Interfaz de red
pub struct NetworkInterface {
    pub info: InterfaceInfo,
    pub stats: InterfaceStats,
    pub rx_queue: Vec<Vec<u8>>,
    pub tx_queue: Vec<Vec<u8>>,
    pub max_queue_size: usize,
}

impl NetworkInterface {
    /// Crear nueva interfaz de red
    pub fn new(info: InterfaceInfo) -> Self {
        Self {
            info,
            stats: InterfaceStats::new(),
            rx_queue: Vec::new(),
            tx_queue: Vec::new(),
            max_queue_size: 1024,
        }
    }

    /// Inicializar interfaz
    pub fn initialize(&mut self) -> Result<(), NetworkError> {
        if self.info.state != InterfaceState::Down {
            return Err(NetworkError::ProtocolError);
        }

        self.info.set_state(InterfaceState::Up);
        self.stats.interface_up += 1;
        Ok(())
    }

    /// Cerrar interfaz
    pub fn shutdown(&mut self) -> Result<(), NetworkError> {
        if self.info.state == InterfaceState::Down {
            return Err(NetworkError::ProtocolError);
        }

        self.info.set_state(InterfaceState::Down);
        self.stats.interface_down += 1;
        Ok(())
    }

    /// Configurar interfaz
    pub fn configure(&mut self, config: InterfaceConfig) -> Result<(), NetworkError> {
        if self.info.state != InterfaceState::Up {
            return Err(NetworkError::ProtocolError);
        }

        self.info.set_config(config);
        self.stats.configurations += 1;
        Ok(())
    }

    /// Enviar paquete
    pub fn send_packet(&mut self, packet: Vec<u8>) -> Result<(), NetworkError> {
        if self.info.state != InterfaceState::Up {
            return Err(NetworkError::ProtocolError);
        }

        if self.tx_queue.len() >= self.max_queue_size {
            return Err(NetworkError::BufferFull);
        }

        self.tx_queue.push(packet.clone());
        self.stats.packets_sent += 1;
        self.stats.bytes_sent += packet.len() as u64;
        Ok(())
    }

    /// Recibir paquete
    pub fn receive_packet(&mut self) -> Option<Vec<u8>> {
        if self.info.state != InterfaceState::Up {
            return None;
        }

        if let Some(packet) = self.rx_queue.pop() {
            self.stats.packets_received += 1;
            self.stats.bytes_received += packet.len() as u64;
            Some(packet)
        } else {
            None
        }
    }

    /// Simular recepción de paquete (para testing)
    pub fn simulate_receive(&mut self, packet: Vec<u8>) -> Result<(), NetworkError> {
        if self.info.state != InterfaceState::Up {
            return Err(NetworkError::ProtocolError);
        }

        if self.rx_queue.len() >= self.max_queue_size {
            return Err(NetworkError::BufferFull);
        }

        self.rx_queue.push(packet);
        Ok(())
    }

    /// Verificar si hay paquetes en cola de recepción
    pub fn has_packets(&self) -> bool {
        !self.rx_queue.is_empty()
    }

    /// Obtener número de paquetes en cola de recepción
    pub fn get_rx_queue_size(&self) -> usize {
        self.rx_queue.len()
    }

    /// Obtener número de paquetes en cola de envío
    pub fn get_tx_queue_size(&self) -> usize {
        self.tx_queue.len()
    }

    /// Limpiar colas
    pub fn clear_queues(&mut self) {
        self.rx_queue.clear();
        self.tx_queue.clear();
    }

    /// Obtener estadísticas
    pub fn get_stats(&self) -> &InterfaceStats {
        &self.stats
    }

    /// Obtener información de la interfaz
    pub fn get_info(&self) -> &InterfaceInfo {
        &self.info
    }

    /// Verificar si la interfaz está activa
    pub fn is_active(&self) -> bool {
        self.info.is_active()
    }

    /// Obtener MTU
    pub fn get_mtu(&self) -> u16 {
        self.info.config.mtu
    }

    /// Establecer MTU
    pub fn set_mtu(&mut self, mtu: u16) -> Result<(), NetworkError> {
        if mtu < 68 || mtu > 65535 {
            return Err(NetworkError::InvalidParameter);
        }

        self.info.config.set_mtu(mtu);
        Ok(())
    }
}

/// Estadísticas de interfaz
#[derive(Debug)]
pub struct InterfaceStats {
    pub packets_sent: u64,
    pub packets_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub errors: u64,
    pub dropped: u64,
    pub collisions: u64,
    pub interface_up: u64,
    pub interface_down: u64,
    pub configurations: u64,
}

impl InterfaceStats {
    pub fn new() -> Self {
        Self {
            packets_sent: 0,
            packets_received: 0,
            bytes_sent: 0,
            bytes_received: 0,
            errors: 0,
            dropped: 0,
            collisions: 0,
            interface_up: 0,
            interface_down: 0,
            configurations: 0,
        }
    }

    pub fn increment_errors(&mut self) {
        self.errors += 1;
    }

    pub fn increment_dropped(&mut self) {
        self.dropped += 1;
    }

    pub fn increment_collisions(&mut self) {
        self.collisions += 1;
    }
}

/// Gestor de interfaces
pub struct InterfaceManager {
    pub interfaces: Vec<NetworkInterface>,
    pub next_index: u32,
}

impl InterfaceManager {
    /// Crear nuevo gestor de interfaces
    pub fn new() -> Self {
        Self {
            interfaces: Vec::new(),
            next_index: 1,
        }
    }

    /// Agregar interfaz
    pub fn add_interface(&mut self, mut info: InterfaceInfo) -> Result<u32, NetworkError> {
        if self.interfaces.len() >= super::MAX_INTERFACES {
            return Err(NetworkError::OutOfMemory);
        }

        info.index = self.next_index;
        let interface = NetworkInterface::new(info);
        self.interfaces.push(interface);

        let index = self.next_index;
        self.next_index += 1;
        Ok(index)
    }

    /// Obtener interfaz por índice
    pub fn get_interface(&self, index: u32) -> Option<&NetworkInterface> {
        self.interfaces
            .iter()
            .find(|iface| iface.info.index == index)
    }

    /// Obtener interfaz mutable por índice
    pub fn get_interface_mut(&mut self, index: u32) -> Option<&mut NetworkInterface> {
        self.interfaces
            .iter_mut()
            .find(|iface| iface.info.index == index)
    }

    /// Obtener interfaz por nombre
    pub fn get_interface_by_name(&self, name: &str) -> Option<&NetworkInterface> {
        self.interfaces.iter().find(|iface| iface.info.name == name)
    }

    /// Obtener interfaz mutable por nombre
    pub fn get_interface_mut_by_name(&mut self, name: &str) -> Option<&mut NetworkInterface> {
        self.interfaces
            .iter_mut()
            .find(|iface| iface.info.name == name)
    }

    /// Obtener interfaz para una IP específica
    pub fn get_interface_for_ip(&self, ip: IpAddress) -> Option<&NetworkInterface> {
        self.interfaces
            .iter()
            .find(|iface| iface.is_active() && iface.info.contains_ip(ip))
    }

    /// Obtener interfaz de loopback
    pub fn get_loopback_interface(&self) -> Option<&NetworkInterface> {
        self.interfaces
            .iter()
            .find(|iface| iface.info.is_loopback())
    }

    /// Obtener interfaz de loopback mutable
    pub fn get_loopback_interface_mut(&mut self) -> Option<&mut NetworkInterface> {
        self.interfaces
            .iter_mut()
            .find(|iface| iface.info.is_loopback())
    }

    /// Remover interfaz
    pub fn remove_interface(&mut self, index: u32) -> Result<(), NetworkError> {
        if let Some(pos) = self
            .interfaces
            .iter()
            .position(|iface| iface.info.index == index)
        {
            self.interfaces.remove(pos);
            Ok(())
        } else {
            Err(NetworkError::NotFound)
        }
    }

    /// Obtener número de interfaces
    pub fn get_interface_count(&self) -> usize {
        self.interfaces.len()
    }

    /// Obtener todas las interfaces
    pub fn get_all_interfaces(&self) -> &[NetworkInterface] {
        &self.interfaces
    }

    /// Obtener interfaces activas
    pub fn get_active_interfaces(&self) -> Vec<&NetworkInterface> {
        self.interfaces
            .iter()
            .filter(|iface| iface.is_active())
            .collect()
    }

    /// Limpiar todas las interfaces
    pub fn clear(&mut self) {
        self.interfaces.clear();
        self.next_index = 1;
    }
}

impl fmt::Display for InterfaceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InterfaceType::Ethernet => write!(f, "Ethernet"),
            InterfaceType::Wifi => write!(f, "WiFi"),
            InterfaceType::Loopback => write!(f, "Loopback"),
            InterfaceType::Virtual => write!(f, "Virtual"),
            InterfaceType::Unknown => write!(f, "Unknown"),
        }
    }
}

impl fmt::Display for InterfaceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InterfaceState::Down => write!(f, "Down"),
            InterfaceState::Up => write!(f, "Up"),
            InterfaceState::Unknown => write!(f, "Unknown"),
            InterfaceState::Error => write!(f, "Error"),
        }
    }
}
