use crate::wl::connection::{Connection, SendError, RecvError};
use crate::wl::wire::{RawMessage, ObjectId, Opcode, Payload, Handle};
use eclipse_ipc::prelude::*;
use core::cell::RefCell;
use alloc::vec::Vec;
use smallvec::SmallVec;

pub struct EclipseWaylandConnection {
    pub channel: RefCell<IpcChannel>,
    pub dest_pid: u32,
    pub self_pid: u32,
}

impl EclipseWaylandConnection {
    pub fn new(dest_pid: u32, self_pid: u32) -> Self {
        Self {
            channel: RefCell::new(IpcChannel::new()),
            dest_pid,
            self_pid,
        }
    }
}

impl Connection for EclipseWaylandConnection {
    fn send(&self, sender: ObjectId, opcode: Opcode, args: &[Payload], handles: &[Handle]) -> Result<(), SendError> {
        let mut h_vec = Vec::new();
        for h in handles {
            h_vec.push(*h);
        }

        let raw = RawMessage {
            sender,
            opcode,
            args: SmallVec::from_iter(args.iter().cloned()),
        };

        let mut serial_buf = [0u8; MAX_MSG_LEN - 4];
        let len = raw.serialize(&mut serial_buf, &mut h_vec).map_err(|_| SendError::IoError)?;

        let mut channel = self.channel.borrow_mut();
        if channel.send_wayland(self.dest_pid, &serial_buf[..len]) {
            Ok(())
        } else {
            Err(SendError::IoError)
        }
    }

    fn recv(&self) -> Result<(Vec<u8>, Vec<Handle>), RecvError> {
        let mut channel = self.channel.borrow_mut();
        // Use a reasonable timeout for blocking recv in this context
        if let Some(msg) = channel.recv_blocking_for(1000) {
            match msg {
                EclipseMessage::Wayland { data, len, from: _ } => {
                    let mut payload = Vec::with_capacity(len);
                    payload.extend_from_slice(&data[..len]);
                    Ok((payload, Vec::new()))
                }
                _ => Err(RecvError::InvalidMessage),
            }
        } else {
            Err(RecvError::IoError)
        }
    }
}
