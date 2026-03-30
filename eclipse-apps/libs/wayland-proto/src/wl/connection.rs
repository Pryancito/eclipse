use crate::wl::wire::RawMessage;
use crate::wl::{ObjectId, Opcode, Payload, Handle};
use alloc::vec::Vec;

#[derive(Debug, PartialEq)]
pub enum SendError {
    IoError,
}

#[derive(Debug, PartialEq)]
pub enum RecvError {
    IoError,
    InvalidMessage,
}

pub trait Connection {
    fn send(&self, sender: ObjectId, opcode: Opcode, args: &[Payload], handles: &[Handle]) -> Result<(), SendError>;
    fn recv(&self) -> Result<(Vec<u8>, Vec<Handle>), RecvError>;
}
