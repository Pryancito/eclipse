//! Módulo de red TCP/IP para Eclipse OS
//! 
//! Este módulo implementa un stack de red completo con soporte para:
//! - Protocolos de capa de red (IP, ICMP, ARP)
//! - Protocolos de capa de transporte (TCP, UDP)
//! - Gestión de interfaces de red
//! - Sistema de routing
//! - API de sockets

pub mod ip;
pub mod tcp;
pub mod udp;
pub mod icmp;
pub mod arp;
pub mod interface;
pub mod buffer;
pub mod routing;
pub mod socket;
pub mod manager;

// Re-exportar tipos principales

// Constantes del sistema de red
pub const MAX_INTERFACES: usize = 16;
pub const MAX_ROUTES: usize = 256;
pub const MAX_SOCKETS: usize = 1024;
pub const MAX_CONNECTIONS: usize = 512;
pub const BUFFER_POOL_SIZE: usize = 1024;
pub const MAX_PACKET_SIZE: usize = 65536;

// Tipos de error de red
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NetworkError {
    NoInterface,
    NoRoute,
    BufferFull,
    InvalidPacket,
    ProtocolError,
    Timeout,
    ConnectionRefused,
    ConnectionReset,
    AddressInUse,
    InvalidAddress,
    InvalidParameter,
    NotFound,
    NotSupported,
    OutOfMemory,
    IoError,
    Unknown,
}

pub type NetworkResult<T> = Result<T, NetworkError>;

// Inicialización del sistema de red
pub fn init_network_system() -> NetworkResult<()> {
    manager::init_network_manager()?;
    buffer::init_buffer_pool()?;
    routing::init_routing_table()?;
    socket::init_socket_system()?;
    Ok(())
}

// Información del sistema de red
pub fn get_network_system_info() -> &'static str {
    "Eclipse Network Stack v1.0 - TCP/IP, UDP, ICMP, ARP"
}

/// Inicializar red (compatible con main.rs)
pub fn init_network() {
    // Inicializar sistema de red
    // En una implementación real, esto configuraría el sistema global
    let _ = init_network_system();
}