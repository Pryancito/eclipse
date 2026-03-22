//! Network IPC Protocol Definitions

pub const NET_MAGIC: &[u8; 4] = b"NETW";

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
    pub resource_id: u64, // For ops on existing sockets
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct NetResponseHeader {
    pub magic: [u8; 4],
    pub op: NetOp, // Should be NetOp::Response
    pub request_id: u32,
    pub status: i64, // 0 for success, negative for error
    pub data_size: u32,
}

pub mod scheme_error {
    pub const ENOENT: usize = 2;   // No such file or directory
    pub const EIO: usize = 5;      // I/O error
    pub const EEXIST: usize = 17;  // File exists (e.g. O_CREAT | O_EXCL)
    pub const EBADF: usize = 9;   // Bad file descriptor
    pub const EAGAIN: usize = 11;  // Try again
    pub const EINVAL: usize = 22;  // Invalid argument
    pub const ESPIPE: usize = 29;  // Illegal seek
    pub const ENOSYS: usize = 38;  // Function not implemented
    pub const EFAULT: usize = 14;  // Bad address
    pub const EISCONN: usize = 106; // Transport endpoint is already connected
    pub const ENOTCONN: usize = 107; // Transport endpoint is not connected
    pub const EPIPE: usize = 32;   // Broken pipe
    pub const EAFNOSUPPORT: usize = 97; // Address family not supported
}
