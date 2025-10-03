//! Sistema de sockets de red
//!
//! API de sockets para aplicaciones

#![allow(dead_code)] // Permitir código no utilizado - API completa del kernel

use alloc::vec::Vec;
use core::fmt;

use super::ip::IpAddress;
use super::tcp::{TcpAddress, TcpPort, TcpSocket};
use super::udp::{UdpAddress, UdpPort, UdpSocket};
use super::NetworkError;

/// Tipos de socket
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SocketType {
    Stream,   // TCP
    Datagram, // UDP
    Raw,      // Raw IP
    Unknown,
}

impl From<u8> for SocketType {
    fn from(value: u8) -> Self {
        match value {
            1 => SocketType::Stream,
            2 => SocketType::Datagram,
            3 => SocketType::Raw,
            _ => SocketType::Unknown,
        }
    }
}

/// Estados de socket
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SocketState {
    Unbound,
    Bound,
    Listening,
    Connected,
    Closed,
    Error,
}

impl From<u8> for SocketState {
    fn from(value: u8) -> Self {
        match value {
            0 => SocketState::Unbound,
            1 => SocketState::Bound,
            2 => SocketState::Listening,
            3 => SocketState::Connected,
            4 => SocketState::Closed,
            5 => SocketState::Error,
            _ => SocketState::Error,
        }
    }
}

/// Dirección de socket
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SocketAddress {
    pub ip: IpAddress,
    pub port: u16,
}

impl SocketAddress {
    /// Crear nueva dirección de socket
    pub fn new(ip: IpAddress, port: u16) -> Self {
        Self { ip, port }
    }

    /// Crear dirección de loopback
    pub fn loopback(port: u16) -> Self {
        Self::new(IpAddress::loopback(), port)
    }

    /// Crear dirección de broadcast
    pub fn broadcast(port: u16) -> Self {
        Self::new(IpAddress::broadcast(), port)
    }

    /// Crear dirección cero
    pub fn zero() -> Self {
        Self::new(IpAddress::zero(), 0)
    }
}

impl fmt::Display for SocketAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.ip, self.port)
    }
}

/// Socket genérico
pub enum Socket {
    Tcp(TcpSocket),
    Udp(UdpSocket),
}

impl Socket {
    /// Crear nuevo socket TCP
    pub fn new_tcp(local_addr: SocketAddress) -> Self {
        let tcp_addr = TcpAddress::new(local_addr.ip, local_addr.port as TcpPort);
        Socket::Tcp(TcpSocket::new(tcp_addr))
    }

    /// Crear nuevo socket UDP
    pub fn new_udp(local_addr: SocketAddress) -> Self {
        let udp_addr = UdpAddress::new(local_addr.ip, local_addr.port as UdpPort);
        Socket::Udp(UdpSocket::new(udp_addr))
    }

    /// Obtener tipo de socket
    pub fn get_type(&self) -> SocketType {
        match self {
            Socket::Tcp(_) => SocketType::Stream,
            Socket::Udp(_) => SocketType::Datagram,
        }
    }

    /// Obtener estado del socket
    pub fn get_state(&self) -> SocketState {
        match self {
            Socket::Tcp(socket) => match socket.get_state() {
                super::tcp::TcpState::Closed => SocketState::Closed,
                super::tcp::TcpState::Listen => SocketState::Listening,
                super::tcp::TcpState::Established => SocketState::Connected,
                _ => SocketState::Bound,
            },
            Socket::Udp(socket) => {
                if socket.is_bound() {
                    SocketState::Bound
                } else {
                    SocketState::Unbound
                }
            }
        }
    }

    /// Vincular socket
    pub fn bind(&mut self) -> Result<(), NetworkError> {
        match self {
            Socket::Tcp(socket) => {
                // TCP no se vincula explícitamente, se conecta directamente
                Err(NetworkError::NotSupported)
            }
            Socket::Udp(socket) => socket.bind(),
        }
    }

    /// Escuchar en socket (solo TCP)
    pub fn listen(&mut self) -> Result<(), NetworkError> {
        match self {
            Socket::Tcp(socket) => socket.listen(),
            Socket::Udp(_) => Err(NetworkError::NotSupported),
        }
    }

    /// Conectar socket
    pub fn connect(&mut self, remote_addr: SocketAddress) -> Result<(), NetworkError> {
        match self {
            Socket::Tcp(socket) => {
                let tcp_addr = TcpAddress::new(remote_addr.ip, remote_addr.port as TcpPort);
                socket.connect(tcp_addr).map(|_| ())
            }
            Socket::Udp(_) => {
                // UDP no tiene conexión
                Err(NetworkError::NotSupported)
            }
        }
    }

    /// Enviar datos
    pub fn send(&mut self, data: &[u8]) -> Result<usize, NetworkError> {
        match self {
            Socket::Tcp(socket) => {
                if socket.is_connected() {
                    socket.send(data).map(|_| data.len())
                } else {
                    Err(NetworkError::ConnectionRefused)
                }
            }
            Socket::Udp(socket) => {
                if socket.is_bound() {
                    // UDP necesita dirección de destino
                    Err(NetworkError::InvalidParameter)
                } else {
                    Err(NetworkError::ProtocolError)
                }
            }
        }
    }

    /// Enviar datos a dirección específica (UDP)
    pub fn send_to(
        &mut self,
        data: &[u8],
        dest_addr: SocketAddress,
    ) -> Result<usize, NetworkError> {
        match self {
            Socket::Tcp(_) => Err(NetworkError::NotSupported),
            Socket::Udp(socket) => {
                let udp_addr = UdpAddress::new(dest_addr.ip, dest_addr.port as UdpPort);
                socket.send_to(udp_addr, data).map(|_| data.len())
            }
        }
    }

    /// Recibir datos
    pub fn receive(&mut self) -> Result<Vec<u8>, NetworkError> {
        match self {
            Socket::Tcp(socket) => {
                if socket.is_connected() {
                    // En un sistema real, esto recibiría datos del buffer
                    Err(NetworkError::Timeout)
                } else {
                    Err(NetworkError::ConnectionRefused)
                }
            }
            Socket::Udp(socket) => {
                if socket.is_bound() {
                    match socket.receive_from() {
                        Ok((_datagram, _addr)) => {
                            // En un sistema real, extraería los datos del datagrama
                            Ok(Vec::new())
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    Err(NetworkError::ProtocolError)
                }
            }
        }
    }

    /// Recibir datos con información de origen (UDP)
    pub fn receive_from(&mut self) -> Result<(Vec<u8>, SocketAddress), NetworkError> {
        match self {
            Socket::Tcp(_) => Err(NetworkError::NotSupported),
            Socket::Udp(socket) => {
                if socket.is_bound() {
                    match socket.receive_from() {
                        Ok((_datagram, addr)) => {
                            let socket_addr = SocketAddress::new(addr.ip, addr.port as u16);
                            // En un sistema real, extraería los datos del datagrama
                            Ok((Vec::new(), socket_addr))
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    Err(NetworkError::ProtocolError)
                }
            }
        }
    }

    /// Cerrar socket
    pub fn close(&mut self) -> Result<(), NetworkError> {
        match self {
            Socket::Tcp(socket) => socket.close().map(|_| ()),
            Socket::Udp(socket) => {
                // UDP no tiene estado de conexión
                Ok(())
            }
        }
    }

    /// Obtener dirección local
    pub fn get_local_address(&self) -> SocketAddress {
        match self {
            Socket::Tcp(socket) => {
                let addr = socket.connection.local_addr;
                SocketAddress::new(addr.ip, addr.port as u16)
            }
            Socket::Udp(socket) => {
                let addr = socket.get_local_addr();
                SocketAddress::new(addr.ip, addr.port as u16)
            }
        }
    }

    /// Verificar si el socket está conectado
    pub fn is_connected(&self) -> bool {
        match self {
            Socket::Tcp(socket) => socket.is_connected(),
            Socket::Udp(_) => false, // UDP no tiene conexión
        }
    }

    /// Verificar si el socket está escuchando
    pub fn is_listening(&self) -> bool {
        match self {
            Socket::Tcp(socket) => socket.is_listening(),
            Socket::Udp(_) => false,
        }
    }
}

/// Gestor de sockets
pub struct SocketManager {
    pub sockets: Vec<Option<Socket>>,
    pub next_fd: u32,
    pub max_sockets: usize,
}

impl SocketManager {
    /// Crear nuevo gestor de sockets
    pub fn new() -> Self {
        Self {
            sockets: Vec::new(),
            next_fd: 0,
            max_sockets: super::MAX_SOCKETS,
        }
    }

    /// Crear nuevo socket
    pub fn create_socket(
        &mut self,
        socket_type: SocketType,
        local_addr: SocketAddress,
    ) -> Result<u32, NetworkError> {
        if self.sockets.len() >= self.max_sockets {
            return Err(NetworkError::OutOfMemory);
        }

        let socket = match socket_type {
            SocketType::Stream => Socket::new_tcp(local_addr),
            SocketType::Datagram => Socket::new_udp(local_addr),
            _ => return Err(NetworkError::NotSupported),
        };

        let fd = self.next_fd;
        self.next_fd += 1;

        self.sockets.push(Some(socket));
        Ok(fd)
    }

    /// Obtener socket por descriptor
    pub fn get_socket(&self, fd: u32) -> Option<&Socket> {
        self.sockets.get(fd as usize)?.as_ref()
    }

    /// Obtener socket mutable por descriptor
    pub fn get_socket_mut(&mut self, fd: u32) -> Option<&mut Socket> {
        self.sockets.get_mut(fd as usize)?.as_mut()
    }

    /// Cerrar socket
    pub fn close_socket(&mut self, fd: u32) -> Result<(), NetworkError> {
        if let Some(socket) = self.sockets.get_mut(fd as usize) {
            if let Some(mut socket) = socket.take() {
                socket.close()?;
                Ok(())
            } else {
                Err(NetworkError::NotFound)
            }
        } else {
            Err(NetworkError::NotFound)
        }
    }

    /// Obtener número de sockets activos
    pub fn get_active_socket_count(&self) -> usize {
        self.sockets.iter().filter(|s| s.is_some()).count()
    }

    /// Obtener estadísticas
    pub fn get_stats(&self) -> SocketManagerStats {
        let mut tcp_sockets = 0;
        let mut udp_sockets = 0;
        let mut connected_sockets = 0;
        let mut listening_sockets = 0;

        for socket in &self.sockets {
            if let Some(socket) = socket {
                match socket {
                    Socket::Tcp(_) => tcp_sockets += 1,
                    Socket::Udp(_) => udp_sockets += 1,
                }

                if socket.is_connected() {
                    connected_sockets += 1;
                }
                if socket.is_listening() {
                    listening_sockets += 1;
                }
            }
        }

        SocketManagerStats {
            total_sockets: self.sockets.len(),
            active_sockets: self.get_active_socket_count(),
            tcp_sockets,
            udp_sockets,
            connected_sockets,
            listening_sockets,
            max_sockets: self.max_sockets,
        }
    }
}

/// Estadísticas del gestor de sockets
#[derive(Debug, Clone)]
pub struct SocketManagerStats {
    pub total_sockets: usize,
    pub active_sockets: usize,
    pub tcp_sockets: usize,
    pub udp_sockets: usize,
    pub connected_sockets: usize,
    pub listening_sockets: usize,
    pub max_sockets: usize,
}

/// Instancia global del gestor de sockets
static mut SOCKET_MANAGER: Option<SocketManager> = None;

/// Inicializar sistema de sockets
pub fn init_socket_system() -> Result<(), NetworkError> {
    unsafe {
        if SOCKET_MANAGER.is_some() {
            return Err(NetworkError::ProtocolError);
        }

        SOCKET_MANAGER = Some(SocketManager::new());
        Ok(())
    }
}

/// Obtener gestor de sockets
pub fn get_socket_manager() -> Option<&'static mut SocketManager> {
    unsafe { SOCKET_MANAGER.as_mut() }
}

/// Obtener estadísticas de sockets
pub fn get_socket_stats() -> Option<SocketManagerStats> {
    unsafe { SOCKET_MANAGER.as_ref().map(|sm| sm.get_stats()) }
}

impl fmt::Display for SocketType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SocketType::Stream => write!(f, "Stream (TCP)"),
            SocketType::Datagram => write!(f, "Datagram (UDP)"),
            SocketType::Raw => write!(f, "Raw"),
            SocketType::Unknown => write!(f, "Unknown"),
        }
    }
}

impl fmt::Display for SocketState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SocketState::Unbound => write!(f, "Unbound"),
            SocketState::Bound => write!(f, "Bound"),
            SocketState::Listening => write!(f, "Listening"),
            SocketState::Connected => write!(f, "Connected"),
            SocketState::Closed => write!(f, "Closed"),
            SocketState::Error => write!(f, "Error"),
        }
    }
}
