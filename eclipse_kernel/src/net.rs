//! Network device registry and trait definition

use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;

/// Trait representing a network interface card (NIC).
pub trait NetworkDevice: Send + Sync {
    /// Get the 6-byte MAC address of the device.
    fn get_mac_address(&self) -> [u8; 6];
    
    /// Send a raw Ethernet packet.
    fn send_packet(&self, data: &[u8]) -> Result<(), &'static str>;
    
    /// Receive a raw Ethernet packet into the provided buffer.
    /// Returns the length of the packet if one was received.
    fn receive_packet(&self, buffer: &mut [u8]) -> Option<usize>;
    
    /// Get the name of the driver (e.g., "VirtIO-Net", "E1000").
    fn name(&self) -> &'static str;
}

static DEVICES: Mutex<Vec<Arc<dyn NetworkDevice>>> = Mutex::new(Vec::new());

/// Register a new network device in the global registry.
pub fn register_device(device: Arc<dyn NetworkDevice>) {
    let mut devices = DEVICES.lock();
    crate::serial::serial_print("[NET] Registered ");
    crate::serial::serial_print(device.name());
    crate::serial::serial_print(" device as eth:");
    crate::serial::serial_print_dec(devices.len() as u64);
    crate::serial::serial_print("\n");
    devices.push(device);
}

/// Get a network device by index.
pub fn get_device(index: usize) -> Option<Arc<dyn NetworkDevice>> {
    DEVICES.lock().get(index).cloned()
}

/// Get the number of registered network devices.
pub fn device_count() -> usize {
    DEVICES.lock().len()
}

// --- IPC Protocol (from old net_ipc.rs) ---

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
