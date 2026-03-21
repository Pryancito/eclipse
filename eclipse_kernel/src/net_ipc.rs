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
    Response = 255,
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
