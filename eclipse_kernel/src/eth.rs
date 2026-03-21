//! Ethernet Scheme for raw packet access
//! 
//! Exposes VirtIO-Net and other network cards as eth:0, eth:1, etc.

use alloc::sync::Arc;
use crate::scheme::{Scheme, Stat, error};
use crate::virtio::{VirtIONetDevice, get_net_device};

pub struct EthScheme;

impl Scheme for EthScheme {
    fn open(&self, path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        let id = path.parse::<usize>().map_err(|_| error::EINVAL)?;
        if get_net_device(id).is_some() {
            Ok(id)
        } else {
            Err(error::ENOENT)
        }
    }

    fn read(&self, id: usize, buffer: &mut [u8]) -> Result<usize, usize> {
        let dev = get_net_device(id).ok_or(error::EBADF)?;
        match dev.receive_packet(buffer) {
            Some(len) => Ok(len),
            None => Err(error::EAGAIN),
        }
    }

    fn write(&self, id: usize, buffer: &[u8]) -> Result<usize, usize> {
        let dev = get_net_device(id).ok_or(error::EBADF)?;
        dev.send_packet(buffer).map(|_| buffer.len()).map_err(|_| error::EIO)
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize) -> Result<usize, usize> {
        Err(error::ESPIPE)
    }

    fn close(&self, _id: usize) -> Result<usize, usize> {
        Ok(0)
    }

    fn fstat(&self, id: usize, stat: &mut Stat) -> Result<usize, usize> {
        let _dev = get_net_device(id).ok_or(error::EBADF)?;
        
        stat.size = 0;
        stat.mode = 0o666 | 0x2000; // Character device
        Ok(0)
    }

    fn ioctl(&self, id: usize, request: usize, arg: usize) -> Result<usize, usize> {
        let dev = get_net_device(id).ok_or(error::EBADF)?;
        match request {
            0x8001 => { // ETH_GET_MAC
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
