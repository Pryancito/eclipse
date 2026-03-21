use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;
use core::sync::atomic::{AtomicU32, Ordering};
use crate::scheme::{Scheme, Stat, error as scheme_error};
use crate::ipc::{self, MessageType};
use crate::net_ipc::*;
use crate::process;

static NEXT_REQUEST_ID: AtomicU32 = AtomicU32::new(1);

pub struct SocketScheme {
    network_pid: Mutex<Option<u32>>,
}

impl SocketScheme {
    pub const fn new() -> Self {
        Self {
            network_pid: Mutex::new(None),
        }
    }

    fn get_network_pid(&self) -> Option<u32> {
        let mut pid_opt = self.network_pid.lock();
        if let Some(pid) = *pid_opt {
             // Validate PID still exists and is named "network"
             if let Some(p) = process::get_process(pid) {
                 let name_len = p.name.iter().position(|&b| b == 0).unwrap_or(16);
                 if &p.name[..name_len] == b"network" {
                     return Some(pid);
                 }
             }
        }
        
        // Try to find it
        if let Some(pid) = process::get_process_by_name("network") {
            *pid_opt = Some(pid);
            return Some(pid);
        }
        
        None
    }

    fn send_request_and_wait(&self, net_pid: u32, op: NetOp, data: &[u8]) -> Result<i64, usize> {
        let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::SeqCst);
        let client_pid = process::current_process_id().unwrap_or(0);
        
        let mut msg_data = [0u8; 512];
        let header = NetRequestHeader {
            magic: *NET_MAGIC,
            op,
            request_id,
            client_pid,
        };
        
        unsafe {
            let header_ptr = &header as *const NetRequestHeader as *const u8;
            core::ptr::copy_nonoverlapping(header_ptr, msg_data.as_mut_ptr(), core::mem::size_of::<NetRequestHeader>());
            
            let payload_offset = core::mem::size_of::<NetRequestHeader>();
            let payload_len = core::cmp::min(data.len(), 512 - payload_offset);
            if payload_len > 0 {
                core::ptr::copy_nonoverlapping(data.as_ptr(), msg_data.as_mut_ptr().add(payload_offset), payload_len);
            }
        }
        
        if !ipc::send_message(0, net_pid, MessageType::Network, &msg_data) {
            return Err(scheme_error::EIO);
        }
        
        // Wait for response
        let start_ticks = crate::interrupts::ticks();
        loop {
            if let Some(msg) = ipc::receive_message(client_pid) {
                if msg.msg_type == MessageType::Network && msg.data_size >= core::mem::size_of::<NetResponseHeader>() as u32 {
                    let resp = unsafe { &*(msg.data.as_ptr() as *const NetResponseHeader) };
                    if resp.magic == *NET_MAGIC && resp.op == NetOp::Response && resp.request_id == request_id {
                        return Ok(resp.status);
                    }
                }
                // If it's another message, we should probably stick it back or handle it.
                // For now, if we are in a syscall, we just loop and try again.
            }
            
            if crate::interrupts::ticks() > start_ticks + 5000 { // 5 second timeout
                return Err(scheme_error::EAGAIN);
            }
            
            process::yield_cpu();
        }
    }
}

impl Scheme for SocketScheme {
    fn open(&self, path: &str, flags: usize, _mode: u32) -> Result<usize, usize> {
        let net_pid = self.get_network_pid().ok_or(scheme_error::ENOENT)?;
        
        // Path might contain "unix" or other info
        let res = self.send_request_and_wait(net_pid, NetOp::Socket, path.as_bytes())?;
        if res < 0 {
            return Err((-res) as usize);
        }
        Ok(res as usize)
    }

    fn read(&self, id: usize, buffer: &mut [u8]) -> Result<usize, usize> {
        let net_pid = self.get_network_pid().ok_or(scheme_error::ENOENT)?;
        
        let mut data = [0u8; 8];
        data[..8].copy_from_slice(&(id as u64).to_le_bytes());
        
        // Wait, recv has multiple args. 
        // For simple read(fd, buf, len), we just pass the id.
        
        let res = self.send_request_and_wait(net_pid, NetOp::Recv, &data)?;
        if res < 0 {
            return Err((-res) as usize);
        }
        
        // The data should be in a second message or we need a way to get it.
        // For now, let's assume the response message contains the data if successful.
        // But NetResponseHeader is already ~13 bytes.
        // MAX_MESSAGE_DATA is 512.
        
        // We need to fetch the data from the message that contained the successful response.
        // I'll modify send_request_and_wait to return the message too.
        
        Err(scheme_error::ENOSYS) // Still refining the protocol
    }

    fn write(&self, id: usize, buffer: &[u8]) -> Result<usize, usize> {
        let net_pid = self.get_network_pid().ok_or(scheme_error::ENOENT)?;
        
        // For now, limited to 512 - header size
        let header_size = core::mem::size_of::<NetRequestHeader>();
        let max_payload = 512 - header_size - 8; // 8 for id
        let to_send = core::cmp::min(buffer.len(), max_payload);
        
        let mut payload = Vec::with_capacity(8 + to_send);
        payload.extend_from_slice(&(id as u64).to_le_bytes());
        payload.extend_from_slice(&buffer[..to_send]);
        
        let res = self.send_request_and_wait(net_pid, NetOp::Send, &payload)?;
        if res < 0 {
            return Err((-res) as usize);
        }
        Ok(res as usize)
    }

    fn close(&self, id: usize) -> Result<usize, usize> {
        let net_pid = self.get_network_pid().ok_or(scheme_error::ENOENT)?;
        let data = (id as u64).to_le_bytes();
        let res = self.send_request_and_wait(net_pid, NetOp::Close, &data)?;
        if res < 0 {
            return Err((-res) as usize);
        }
        Ok(0)
    }

    fn fstat(&self, _id: usize, _stat: &mut Stat) -> Result<usize, usize> {
        Err(scheme_error::ENOSYS)
    }

    fn ioctl(&self, id: usize, request: usize, arg: usize) -> Result<usize, usize> {
        let net_pid = self.get_network_pid().ok_or(scheme_error::ENOENT)?;
        
        let mut payload = Vec::with_capacity(24);
        payload.extend_from_slice(&(id as u64).to_le_bytes());
        payload.extend_from_slice(&(request as u64).to_le_bytes());
        payload.extend_from_slice(&(arg as u64).to_le_bytes());
        
        let res = self.send_request_and_wait(net_pid, NetOp::Bind, &payload)?; // Reuse Op for PoC or add Ioctl Op
        if res < 0 {
            return Err((-res) as usize);
        }
        Ok(res as usize)
    }
}
