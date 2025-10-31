//! Gestor de red
//! 
//! Coordina todos los protocolos de red y gestiona las conexiones

use super::ethernet::{EthernetManager, MacAddress};
use super::ip::{IpManager, IpAddress};
use super::tcp::TcpManager;

/// Estadísticas de red
#[derive(Debug, Clone, Copy)]
pub struct NetworkStatistics {
    pub ethernet_frames_sent: u64,
    pub ethernet_frames_received: u64,
    pub ethernet_bytes_sent: u64,
    pub ethernet_bytes_received: u64,
    pub ip_packets_sent: u64,
    pub ip_packets_received: u64,
    pub ip_bytes_sent: u64,
    pub ip_bytes_received: u64,
    pub tcp_segments_sent: u64,
    pub tcp_segments_received: u64,
    pub tcp_bytes_sent: u64,
    pub tcp_bytes_received: u64,
    pub active_tcp_connections: u32,
}

/// Gestor principal de red
pub struct NetworkManager {
    pub ethernet: EthernetManager,
    pub ip: IpManager,
    pub tcp: TcpManager,
    pub is_initialized: bool,
}

impl NetworkManager {
    /// Crear nuevo gestor de red
    pub fn new(local_mac: MacAddress, local_ip: IpAddress) -> Self {
        Self {
            ethernet: EthernetManager::new(local_mac),
            ip: IpManager::new(local_ip),
            tcp: TcpManager::new(),
            is_initialized: false,
        }
    }
    
    /// Inicializar el stack de red
    pub fn init(&mut self) {
        self.is_initialized = true;
    }
    
    /// Procesar eventos de red
    pub fn process_events(&mut self) {
        if !self.is_initialized {
            return;
        }
        
        // En una implementación real, aquí se procesarían los eventos de red
        // como paquetes entrantes, timeouts, etc.
    }
    
    /// Crear conexión TCP
    pub fn create_tcp_connection(&mut self, local_port: u16, remote_port: u16) -> Option<usize> {
        self.tcp.create_connection(local_port, remote_port)
    }
    
    /// Cerrar conexión TCP
    pub fn close_tcp_connection(&mut self, connection_id: usize) -> bool {
        self.tcp.close_connection(connection_id)
    }
    
    /// Enviar datos por TCP
    pub fn send_tcp_data(&mut self, connection_id: usize, data: &[u8]) -> bool {
        if connection_id >= 64 {
            return false;
        }
        
        if let Some(conn) = &mut self.tcp.connections[connection_id] {
            return conn.send_data(data);
        }
        
        false
    }
    
    /// Recibir datos por TCP
    pub fn receive_tcp_data(&mut self, connection_id: usize, data: &mut [u8]) -> usize {
        if connection_id >= 64 {
            return 0;
        }
        
        if let Some(conn) = &mut self.tcp.connections[connection_id] {
            return conn.receive_data(data);
        }
        
        0
    }
    
    /// Obtener estadísticas completas
    pub fn get_statistics(&self) -> NetworkStatistics {
        let (eth_sent, eth_recv, eth_bytes_sent, eth_bytes_recv) = self.ethernet.get_statistics();
        let (ip_sent, ip_recv, ip_bytes_sent, ip_bytes_recv) = self.ip.get_statistics();
        let (tcp_sent, tcp_recv, tcp_bytes_sent, tcp_bytes_recv) = self.tcp.get_statistics();
        
        NetworkStatistics {
            ethernet_frames_sent: eth_sent,
            ethernet_frames_received: eth_recv,
            ethernet_bytes_sent: eth_bytes_sent,
            ethernet_bytes_received: eth_bytes_recv,
            ip_packets_sent: ip_sent,
            ip_packets_received: ip_recv,
            ip_bytes_sent: ip_bytes_sent,
            ip_bytes_received: ip_bytes_recv,
            tcp_segments_sent: tcp_sent,
            tcp_segments_received: tcp_recv,
            tcp_bytes_sent: tcp_bytes_sent,
            tcp_bytes_received: tcp_bytes_recv,
            active_tcp_connections: self.tcp.get_active_connections(),
        }
    }
}

/// Gestor global de red
static mut NETWORK_MANAGER: Option<NetworkManager> = None;

/// Inicializar el stack de red
pub fn init_network_stack() {
    unsafe {
        // Configuración de ejemplo
        let local_mac = MacAddress::new([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
        let local_ip = IpAddress::new([192, 168, 1, 100]);
        
        NETWORK_MANAGER = Some(NetworkManager::new(local_mac, local_ip));
        
        if let Some(manager) = &mut NETWORK_MANAGER {
            manager.init();
        }
    }
}

/// Procesar eventos de red
pub fn process_network_events() {
    unsafe {
        if let Some(manager) = &mut NETWORK_MANAGER {
            manager.process_events();
        }
    }
}

/// Crear conexión TCP
pub fn create_tcp_connection(local_port: u16, remote_port: u16) -> Option<usize> {
    unsafe {
        if let Some(manager) = &mut NETWORK_MANAGER {
            manager.create_tcp_connection(local_port, remote_port)
        } else {
            None
        }
    }
}

/// Cerrar conexión TCP
pub fn close_tcp_connection(connection_id: usize) -> bool {
    unsafe {
        if let Some(manager) = &mut NETWORK_MANAGER {
            manager.close_tcp_connection(connection_id)
        } else {
            false
        }
    }
}

/// Enviar datos por TCP
pub fn send_tcp_data(connection_id: usize, data: &[u8]) -> bool {
    unsafe {
        if let Some(manager) = &mut NETWORK_MANAGER {
            manager.send_tcp_data(connection_id, data)
        } else {
            false
        }
    }
}

/// Recibir datos por TCP
pub fn receive_tcp_data(connection_id: usize, data: &mut [u8]) -> usize {
    unsafe {
        if let Some(manager) = &mut NETWORK_MANAGER {
            manager.receive_tcp_data(connection_id, data)
        } else {
            0
        }
    }
}

/// Obtener estadísticas de red
pub fn get_network_statistics() -> NetworkStatistics {
    unsafe {
        if let Some(manager) = &NETWORK_MANAGER {
            manager.get_statistics()
        } else {
            NetworkStatistics {
                ethernet_frames_sent: 0,
                ethernet_frames_received: 0,
                ethernet_bytes_sent: 0,
                ethernet_bytes_received: 0,
                ip_packets_sent: 0,
                ip_packets_received: 0,
                ip_bytes_sent: 0,
                ip_bytes_received: 0,
                tcp_segments_sent: 0,
                tcp_segments_received: 0,
                tcp_bytes_sent: 0,
                tcp_bytes_received: 0,
                active_tcp_connections: 0,
            }
        }
    }
}
