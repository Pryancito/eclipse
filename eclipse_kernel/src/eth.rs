//! Ethernet Scheme for raw packet access
//!
//! Exposes network cards (VirtIO-Net, Intel e1000e, …) as `eth:0`, `eth:1`, …
//! via the kernel scheme interface.
//!
//! ## Design
//! Each driver calls [`eth_register_device`] during its `init()` to insert an
//! `Arc<dyn NetworkDevice>` into the global [`NET_DEVICE_REGISTRY`].  The
//! `EthScheme` then resolves `eth:N` to slot *N* of that registry, so the
//! ordering of devices depends on the order in which drivers are initialised.

use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;
use crate::scheme::{Scheme, Stat, error};

// ───────────────────────────────────────────────────────────────────────────
// NetworkDevice trait
// ───────────────────────────────────────────────────────────────────────────

/// Common interface implemented by every Ethernet driver.
///
/// All methods take `&self` so the trait is object-safe and can be stored as
/// `dyn NetworkDevice`.  Individual drivers protect mutable state internally
/// with a `Mutex`.
pub trait NetworkDevice: Send + Sync {
    fn get_mac_address(&self) -> [u8; 6];
    fn send_packet(&self, data: &[u8]) -> Result<(), &'static str>;
    fn receive_packet(&self, buffer: &mut [u8]) -> Option<usize>;
}

// ───────────────────────────────────────────────────────────────────────────
// E1000EDevice blanket impl
// ───────────────────────────────────────────────────────────────────────────

impl NetworkDevice for crate::e1000e::E1000EDevice {
    fn get_mac_address(&self) -> [u8; 6] {
        crate::e1000e::E1000EDevice::get_mac_address(self)
    }

    fn send_packet(&self, data: &[u8]) -> Result<(), &'static str> {
        crate::e1000e::E1000EDevice::send_packet(self, data)
    }

    fn receive_packet(&self, buffer: &mut [u8]) -> Option<usize> {
        crate::e1000e::E1000EDevice::receive_packet(self, buffer)
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Global device registry
// ───────────────────────────────────────────────────────────────────────────

/// Ordered list of all registered network devices.
/// Index N corresponds to `eth:N`.
static NET_DEVICE_REGISTRY: Mutex<Vec<Arc<dyn NetworkDevice>>> = Mutex::new(Vec::new());

/// Register a network device so it becomes available as `eth:N`.
///
/// Drivers call this from their `init()` functions.  The device receives the
/// next available index (0, 1, 2, …).
pub fn eth_register_device(dev: Arc<dyn NetworkDevice>) {
    NET_DEVICE_REGISTRY.lock().push(dev);
}

/// Look up a registered device by index.
fn get_device(id: usize) -> Option<Arc<dyn NetworkDevice>> {
    NET_DEVICE_REGISTRY.lock().get(id).cloned()
}

// ───────────────────────────────────────────────────────────────────────────
// EthScheme
// ───────────────────────────────────────────────────────────────────────────

pub struct EthScheme;

impl Scheme for EthScheme {
    fn open(&self, path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        let id = path.parse::<usize>().map_err(|_| error::EINVAL)?;
        if get_device(id).is_some() {
            Ok(id)
        } else {
            Err(error::ENOENT)
        }
    }

    fn read(&self, id: usize, buffer: &mut [u8]) -> Result<usize, usize> {
        let dev = get_device(id).ok_or(error::EBADF)?;
        match dev.receive_packet(buffer) {
            Some(len) => Ok(len),
            None => Err(error::EAGAIN),
        }
    }

    fn write(&self, id: usize, buffer: &[u8]) -> Result<usize, usize> {
        let dev = get_device(id).ok_or(error::EBADF)?;
        dev.send_packet(buffer).map(|_| buffer.len()).map_err(|_| error::EIO)
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize) -> Result<usize, usize> {
        Err(error::ESPIPE)
    }

    fn close(&self, _id: usize) -> Result<usize, usize> {
        Ok(0)
    }

    fn fstat(&self, id: usize, stat: &mut Stat) -> Result<usize, usize> {
        if NET_DEVICE_REGISTRY.lock().get(id).is_none() {
            return Err(error::EBADF);
        }
        stat.size = 0;
        stat.mode = 0o666 | 0x2000; // Character device
        Ok(0)
    }

    fn ioctl(&self, id: usize, request: usize, arg: usize) -> Result<usize, usize> {
        let dev = get_device(id).ok_or(error::EBADF)?;
        match request {
            0x8001 => {
                // ETH_GET_MAC — copy the 6-byte MAC into the caller's buffer
                let mac = dev.get_mac_address();
                let buf = arg as *mut u8;
                unsafe {
                    core::ptr::copy_nonoverlapping(mac.as_ptr(), buf, 6);
                }
                Ok(0)
            }
            _ => Err(error::ENOSYS),
        }
    }
}
