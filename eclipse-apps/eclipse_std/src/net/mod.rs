//! Network Module - Network operations (Stubs)
//!
//! Provides placeholders for standard networking types.

use crate::io::{Result, Error, ErrorKind};

/// An IP address, either IPv4 or IPv6.
pub enum IpAddr {
    V4(Ipv4Addr),
    V6(Ipv6Addr),
}

/// An IPv4 address.
pub struct Ipv4Addr([u8; 4]);

/// An IPv6 address.
pub struct Ipv6Addr([u16; 8]);

/// A TCP stream between a local and a remote socket.
pub struct TcpStream;

impl TcpStream {
    pub fn connect(_addr: &str) -> Result<Self> {
        Err(Error::new(ErrorKind::Other))
    }
}

/// A TCP socket server, listening for connections.
pub struct TcpListener;

impl TcpListener {
    pub fn bind(_addr: &str) -> Result<Self> {
        Err(Error::new(ErrorKind::Other))
    }
}

/// A UDP socket.
pub struct UdpSocket;

impl UdpSocket {
    pub fn bind(_addr: &str) -> Result<Self> {
        Err(Error::new(ErrorKind::Other))
    }
}
