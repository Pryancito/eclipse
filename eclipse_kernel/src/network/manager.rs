//! Gestor principal del sistema de red
//!
//! Coordina todos los componentes del stack de red

#![allow(dead_code)] // Permitir código no utilizado - API completa del kernel

use alloc::format;
use alloc::string::String;

use super::arp::MacAddress;
use super::arp::{ArpProcessor, ArpStats};
use super::buffer::{BufferManager, BufferManagerStats};
use super::icmp::{IcmpProcessor, IcmpStats};
use super::interface::{InterfaceManager, InterfaceType};
use super::ip::{IpAddress, IpPacket, IpStats};
use super::routing::{RoutingAlgorithm, RoutingTableStats};
use super::socket::{SocketManager, SocketManagerStats};
use super::tcp::TcpStats;
use super::udp::UdpStats;
use super::{NetworkError, NetworkResult};

/// Estadísticas globales de red
#[derive(Debug, Clone)]
pub struct NetworkStats {
    pub ip_stats: IpStats,
    pub tcp_stats: TcpStats,
    pub udp_stats: UdpStats,
    pub icmp_stats: IcmpStats,
    pub arp_stats: ArpStats,
    pub routing_stats: RoutingTableStats,
    pub socket_stats: SocketManagerStats,
    pub buffer_stats: BufferManagerStats,
    pub interfaces_count: usize,
    pub active_interfaces: usize,
}

impl NetworkStats {
    pub fn new() -> Self {
        Self {
            ip_stats: IpStats::new(),
            tcp_stats: TcpStats::new(),
            udp_stats: UdpStats::new(),
            icmp_stats: IcmpStats::new(),
            arp_stats: ArpStats::new(),
            routing_stats: RoutingTableStats {
                total_routes: 0,
                active_routes: 0,
                direct_routes: 0,
                gateway_routes: 0,
                default_routes: 0,
                max_routes: 0,
            },
            socket_stats: SocketManagerStats {
                total_sockets: 0,
                active_sockets: 0,
                tcp_sockets: 0,
                udp_sockets: 0,
                connected_sockets: 0,
                listening_sockets: 0,
                max_sockets: 0,
            },
            buffer_stats: BufferManagerStats {
                rx_pool: super::buffer::BufferPoolStats {
                    max_buffers: 0,
                    buffer_size: 0,
                    allocated: 0,
                    free: 0,
                    pool_size: 0,
                },
                tx_pool: super::buffer::BufferPoolStats {
                    max_buffers: 0,
                    buffer_size: 0,
                    allocated: 0,
                    free: 0,
                    pool_size: 0,
                },
                rx_queue: super::buffer::PacketQueueStats {
                    current_size: 0,
                    max_size: 0,
                    dropped_packets: 0,
                },
                tx_queue: super::buffer::PacketQueueStats {
                    current_size: 0,
                    max_size: 0,
                    dropped_packets: 0,
                },
            },
            interfaces_count: 0,
            active_interfaces: 0,
        }
    }
}

/// Gestor principal de red
pub struct NetworkManager {
    pub interfaces: InterfaceManager,
    pub routing: RoutingAlgorithm,
    pub sockets: SocketManager,
    pub buffers: BufferManager,
    pub icmp: IcmpProcessor,
    pub arp: ArpProcessor,
    pub stats: NetworkStats,
    pub initialized: bool,
}

impl NetworkManager {
    /// Crear nuevo gestor de red
    pub fn new() -> Self {
        Self {
            interfaces: InterfaceManager::new(),
            routing: RoutingAlgorithm::new(),
            sockets: SocketManager::new(),
            buffers: BufferManager::new(),
            icmp: IcmpProcessor::new(),
            arp: ArpProcessor::new(MacAddress::zero(), IpAddress::zero()),
            stats: NetworkStats::new(),
            initialized: false,
        }
    }

    /// Inicializar sistema de red
    pub fn initialize(&mut self) -> NetworkResult<()> {
        if self.initialized {
            return Err(NetworkError::ProtocolError);
        }

        // Crear interfaz de loopback
        let loopback_mac = MacAddress::new(0x00, 0x00, 0x00, 0x00, 0x00, 0x01);
        let loopback_ip = IpAddress::loopback();

        let mut loopback_info = super::interface::InterfaceInfo::new(
            String::from("lo"),
            InterfaceType::Loopback,
            loopback_mac,
            1,
        );

        let mut config =
            super::interface::InterfaceConfig::new(loopback_ip, IpAddress::new(255, 0, 0, 0));
        config.set_mtu(65535); // MTU grande para loopback
        loopback_info.set_config(config);

        self.interfaces.add_interface(loopback_info)?;

        // Configurar ARP con dirección local
        self.arp = ArpProcessor::new(loopback_mac, loopback_ip);

        // Agregar ruta de loopback
        let loopback_route =
            super::routing::Route::direct(IpAddress::loopback(), IpAddress::new(255, 0, 0, 0), 1);
        self.routing.add_route(loopback_route)?;

        self.initialized = true;
        Ok(())
    }

    /// Procesar paquete IP recibido
    pub fn process_ip_packet(
        &mut self,
        packet: IpPacket,
        interface_index: u32,
    ) -> NetworkResult<()> {
        if !self.initialized {
            return Err(NetworkError::ProtocolError);
        }

        self.stats
            .ip_stats
            .increment_received(packet.get_total_length() as u64);

        // Verificar TTL
        if packet.get_ttl() == 0 {
            self.stats.ip_stats.increment_ttl_expired();
            return Ok(());
        }

        // Decrementar TTL
        let mut packet = packet;
        if !packet.decrement_ttl() {
            self.stats.ip_stats.increment_ttl_expired();
            return Ok(());
        }

        // Verificar si es para nosotros
        if self.is_local_address(packet.get_destination()) {
            // Procesar paquete local
            self.process_local_packet(packet)?;
        } else {
            // Reenviar paquete
            self.forward_packet(packet)?;
        }

        Ok(())
    }

    /// Procesar paquete local
    fn process_local_packet(&mut self, packet: IpPacket) -> NetworkResult<()> {
        match packet.get_protocol() {
            super::ip::IpProtocol::ICMP => {
                // Procesar ICMP
                if let Some(icmp_data) = super::icmp::IcmpMessage::from_bytes(&packet.payload) {
                    if let Some(response) =
                        self.icmp.process_message(icmp_data, packet.get_source())?
                    {
                        // Enviar respuesta ICMP
                        self.send_icmp_response(response, packet.get_source())?;
                    }
                }
            }
            super::ip::IpProtocol::TCP => {
                // Procesar TCP
                if let Some(tcp_data) = super::tcp::TcpSegment::from_bytes(&packet.payload) {
                    // En un sistema real, aquí se procesaría el segmento TCP
                    self.stats.tcp_stats.segments_received += 1;
                }
            }
            super::ip::IpProtocol::UDP => {
                // Procesar UDP
                if let Some(udp_data) = super::udp::UdpDatagram::from_bytes(&packet.payload) {
                    // En un sistema real, aquí se procesaría el datagrama UDP
                    self.stats.udp_stats.datagrams_received += 1;
                }
            }
            _ => {
                // Protocolo no soportado
            }
        }

        Ok(())
    }

    /// Reenviar paquete
    fn forward_packet(&mut self, packet: IpPacket) -> NetworkResult<()> {
        // Buscar ruta
        if let Some(route) = self.routing.find_best_route(packet.get_destination()) {
            if let Some(interface) = self.interfaces.get_interface(route.get_interface()) {
                // En un sistema real, aquí se enviaría el paquete por la interfaz
                self.stats
                    .ip_stats
                    .increment_sent(packet.get_total_length() as u64);
            }
        } else {
            // No hay ruta, enviar ICMP Destination Unreachable
            self.send_icmp_destination_unreachable(packet)?;
        }

        Ok(())
    }

    /// Enviar respuesta ICMP
    fn send_icmp_response(
        &mut self,
        response: super::icmp::IcmpMessage,
        destination: IpAddress,
    ) -> NetworkResult<()> {
        // Crear paquete IP con respuesta ICMP
        let icmp_data = response.to_bytes();
        let ip_packet = IpPacket::new(
            IpAddress::loopback(),
            destination,
            super::ip::IpProtocol::ICMP,
            icmp_data,
        );

        // En un sistema real, aquí se enviaría el paquete
        self.stats.icmp_stats.increment_sent();
        Ok(())
    }

    /// Enviar ICMP Destination Unreachable
    fn send_icmp_destination_unreachable(
        &mut self,
        original_packet: IpPacket,
    ) -> NetworkResult<()> {
        let icmp_data = original_packet.to_bytes();
        let icmp_message = super::icmp::IcmpMessage::destination_unreachable(
            super::icmp::IcmpCode::HostUnreachable,
            &icmp_data,
        );

        self.send_icmp_response(icmp_message, original_packet.get_source())?;
        Ok(())
    }

    /// Verificar si una dirección es local
    fn is_local_address(&self, ip: IpAddress) -> bool {
        if ip.is_loopback() {
            return true;
        }

        // Verificar interfaces locales
        for interface in self.interfaces.get_all_interfaces() {
            if interface.is_active() && interface.info.contains_ip(ip) {
                return true;
            }
        }

        false
    }

    /// Agregar interfaz de red
    pub fn add_interface(
        &mut self,
        name: String,
        interface_type: InterfaceType,
        mac_address: MacAddress,
    ) -> NetworkResult<u32> {
        let info = super::interface::InterfaceInfo::new(name, interface_type, mac_address, 0);
        let index = self.interfaces.add_interface(info)?;

        // Inicializar interfaz
        if let Some(interface) = self.interfaces.get_interface_mut(index) {
            interface.initialize()?;
        }

        Ok(index)
    }

    /// Configurar interfaz
    pub fn configure_interface(
        &mut self,
        index: u32,
        ip_address: IpAddress,
        netmask: IpAddress,
        gateway: Option<IpAddress>,
    ) -> NetworkResult<()> {
        if let Some(interface) = self.interfaces.get_interface_mut(index) {
            let mut config = super::interface::InterfaceConfig::new(ip_address, netmask);
            if let Some(gw) = gateway {
                config.set_gateway(gw);
            }
            interface.configure(config)?;

            // Agregar ruta local
            let route = super::routing::Route::direct(ip_address, netmask, index);
            self.routing.add_route(route)?;

            // Agregar ruta por defecto si hay gateway
            if let Some(gw) = gateway {
                let default_route = super::routing::Route::default(gw, index, 1);
                self.routing.add_route(default_route)?;
            }
        } else {
            return Err(NetworkError::NotFound);
        }

        Ok(())
    }

    /// Crear socket
    pub fn create_socket(
        &mut self,
        socket_type: super::socket::SocketType,
        local_addr: super::socket::SocketAddress,
    ) -> NetworkResult<u32> {
        self.sockets.create_socket(socket_type, local_addr)
    }

    /// Obtener socket
    pub fn get_socket(&mut self, fd: u32) -> Option<&mut super::socket::Socket> {
        self.sockets.get_socket_mut(fd)
    }

    /// Cerrar socket
    pub fn close_socket(&mut self, fd: u32) -> NetworkResult<()> {
        self.sockets.close_socket(fd)
    }

    /// Actualizar estadísticas
    pub fn update_stats(&mut self) {
        self.stats.routing_stats = self.routing.get_stats();
        self.stats.socket_stats = self.sockets.get_stats();
        self.stats.buffer_stats = self.buffers.get_stats();
        self.stats.interfaces_count = self.interfaces.get_interface_count();
        self.stats.active_interfaces = self.interfaces.get_active_interfaces().len();
    }

    /// Obtener estadísticas
    pub fn get_stats(&self) -> &NetworkStats {
        &self.stats
    }

    /// Verificar si está inicializado
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Obtener información del sistema
    pub fn get_system_info(&self) -> String {
        format!(
            "Eclipse Network Stack v1.0\n\
             Interfaces: {}/{}\n\
             Routes: {}\n\
             Sockets: {}/{}\n\
             Buffers: {}/{}",
            self.stats.active_interfaces,
            self.stats.interfaces_count,
            self.stats.routing_stats.active_routes,
            self.stats.socket_stats.active_sockets,
            self.stats.socket_stats.max_sockets,
            self.stats.buffer_stats.rx_pool.allocated,
            self.stats.buffer_stats.rx_pool.max_buffers
        )
    }
}

/// Instancia global del gestor de red
static mut NETWORK_MANAGER: Option<NetworkManager> = None;

/// Inicializar gestor de red
pub fn init_network_manager() -> NetworkResult<()> {
    unsafe {
        if NETWORK_MANAGER.is_some() {
            return Err(NetworkError::ProtocolError);
        }

        let mut manager = NetworkManager::new();
        manager.initialize()?;
        NETWORK_MANAGER = Some(manager);
        Ok(())
    }
}

/// Obtener gestor de red
pub fn get_network_manager() -> Option<&'static mut NetworkManager> {
    unsafe { NETWORK_MANAGER.as_mut() }
}

/// Obtener estadísticas de red
pub fn get_network_stats() -> Option<NetworkStats> {
    unsafe { NETWORK_MANAGER.as_ref().map(|nm| nm.stats.clone()) }
}

/// Obtener información del sistema de red
pub fn get_network_system_info() -> Option<String> {
    unsafe { NETWORK_MANAGER.as_ref().map(|nm| nm.get_system_info()) }
}
