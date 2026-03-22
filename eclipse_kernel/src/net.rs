//! Network IPC Protocol Definitions (kernel side)

pub use crate::eth::NetworkDevice;

pub const NET_MAGIC: [u8; 4] = *b"NETW";

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NetOp {
    Socket = 0,
    Bind = 1,
    Listen = 2,
    Accept = 3,
    Connect = 4,
    Send = 5,
    Recv = 6,
    Close = 7,
    Ioctl = 8,
    Resolve = 9,
    GetExtendedStats = 10,
    Response = 255,
}

/// Extended network statistics reported by the network service.
///
/// IP addresses are stored in network byte order (big-endian).
/// Prefix lengths are in CIDR notation (e.g. 24 = /24).
/// `rx_bytes`/`tx_bytes` are cumulative byte counters since driver init.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct NetExtendedStats {
    pub lo_ipv4: [u8; 4],
    pub lo_ipv4_prefix: u8,
    pub lo_ipv6: [u8; 16],
    pub lo_ipv6_prefix: u8,
    pub lo_up: u8,
    pub eth0_ipv4: [u8; 4],
    pub eth0_ipv4_prefix: u8,
    pub eth0_ipv6: [u8; 16],
    pub eth0_ipv6_prefix: u8,
    pub eth0_up: u8,
    pub eth0_gateway: [u8; 4],
    pub eth0_gateway_ipv6: [u8; 16],
    pub eth0_dns: [u8; 4],
    pub eth0_dns_ipv6: [u8; 16],
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct NetRequestHeader {
    pub magic: [u8; 4],
    pub op: NetOp,
    pub request_id: u32,
    pub client_pid: u32,
    pub resource_id: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct NetResponseHeader {
    pub magic: [u8; 4],
    pub op: NetOp,
    pub request_id: u32,
    pub status: i64,
    pub data_size: u32,
}
