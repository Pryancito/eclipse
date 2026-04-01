use crate::utils::bytes_reader::{BytesReader, BytesReaderError};
use crate::utils::bytes_writer::{BytesWriter, BytesWriterError};
use crate::wl::connection::Connection;
use crate::wl::interface::Interface;
use eclipse_ipc::prelude::MAX_MSG_LEN;
use alloc::rc::Rc;
use alloc::str::{self, Utf8Error};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;
use smallvec::SmallVec;

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct Opcode(pub u16);

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(pub u32);

impl ObjectId {
    pub fn null() -> ObjectId {
        ObjectId(0)
    }

    pub fn is_null(self) -> bool {
        self.0 == 0
    }
}

impl From<ObjectId> for Payload {
    fn from(id: ObjectId) -> Self {
        Payload::ObjectId(id)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NewId(pub u32);

impl NewId {
    pub fn as_id(&self) -> ObjectId {
        ObjectId(self.0)
    }
}

impl From<NewId> for Payload {
    fn from(id: NewId) -> Self {
        Payload::NewId(id)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct Handle(pub i32);

impl From<Handle> for Payload {
    fn from(h: Handle) -> Self {
        Payload::Handle(h)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Array(pub Vec<u8>);

impl Array {
    pub fn from_bytes(bytes: &[u8]) -> Array {
        Array(Vec::from(bytes))
    }
}

impl From<Array> for Payload {
    fn from(a: Array) -> Self {
        Payload::Array(a)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
    UInt(u32),
    Int(i32),
    Fixed(f32),
    ObjectId(ObjectId),
    NewId(NewId),
    Handle(Handle),
    Array(Array),
    String(String),
}

impl From<u32> for Payload {
    fn from(val: u32) -> Self {
        Payload::UInt(val)
    }
}

impl From<i32> for Payload {
    fn from(val: i32) -> Self {
        Payload::Int(val)
    }
}

impl From<String> for Payload {
    fn from(val: String) -> Self {
        Payload::String(val)
    }
}

impl From<&str> for Payload {
    fn from(val: &str) -> Self {
        Payload::String(val.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PayloadType {
    UInt,
    Int,
    Fixed,
    ObjectId,
    NewId,
    Handle,
    Array,
    String,
}

#[derive(Debug, PartialEq)]
pub enum SerializeError {
    WriterError(BytesWriterError),
}

#[derive(Debug, PartialEq)]
pub enum DeserializeError {
    ReaderError(BytesReaderError),
    InvalidLength,
    TooFewHandles,
    ObjectIsNull,
    UnknownOpcode,
    UnexpectedType,
    NonTerminatedString,
    Utf8Error(Utf8Error),
}

#[derive(Debug, PartialEq)]
pub struct RawMessage {
    pub sender: ObjectId,
    pub opcode: Opcode,
    pub args: SmallVec<[Payload; 8]>,
}

impl RawMessage {
    pub fn serialize(
        &self,
        buf: &mut [u8],
        handles: &mut Vec<Handle>,
    ) -> Result<usize, SerializeError> {
        let mut writer = BytesWriter::new(buf);
        writer
            .append_le_u32(self.sender.0)
            .map_err(SerializeError::WriterError)?;
        writer
            .append_le_u32(0 /* TBD length + opcode */)
            .map_err(SerializeError::WriterError)?;

        let bytes_remaining_before_args = writer.remaining_len();
        for arg in &self.args {
            match arg {
                Payload::UInt(value) => {
                    writer
                        .append_le_u32(*value)
                        .map_err(SerializeError::WriterError)?;
                }
                Payload::Int(value) => {
                    writer
                        .append_le_i32(*value)
                        .map_err(SerializeError::WriterError)?;
                }
                Payload::Fixed(value) => {
                    writer.append_le_i32((*value * 256.0) as i32)
                        .map_err(SerializeError::WriterError)?;
                }
                Payload::ObjectId(value) => {
                    writer
                        .append_le_u32(value.0)
                        .map_err(SerializeError::WriterError)?;
                }
                Payload::NewId(value) => {
                    writer
                        .append_le_u32(value.0)
                        .map_err(SerializeError::WriterError)?;
                }
                Payload::Handle(value) => {
                    handles.push(*value);
                }
                Payload::Array(value) => {
                    writer
                        .append_le_u32(value.0.len() as u32)
                        .map_err(SerializeError::WriterError)?;
                    writer
                        .append_bytes(value.0.as_slice())
                        .map_err(SerializeError::WriterError)?;
                    writer
                        .append_until_alignment(0, 4)
                        .map_err(SerializeError::WriterError)?;
                }
                Payload::String(value) => {
                    writer
                        .append_le_u32(value.len() as u32 + 1)
                        .map_err(SerializeError::WriterError)?;
                    writer
                        .append_bytes(value.as_bytes())
                        .map_err(SerializeError::WriterError)?;
                    writer.append_u8(0).map_err(SerializeError::WriterError)?;
                    writer
                        .append_until_alignment(0, 4)
                        .map_err(SerializeError::WriterError)?;
                }
            }
        }

        let total_len = 8 + bytes_remaining_before_args - writer.remaining_len();
        writer
            .write_le_u32(4, (((total_len as u32) << 16) | (self.opcode.0 as u32)))
            .map_err(SerializeError::WriterError)?;
        
        Ok(total_len)
    }

    pub fn deserialize_header(buf: &[u8]) -> Result<(ObjectId, Opcode, usize), DeserializeError> {
        let mut reader = BytesReader::new(buf);
        let sender = ObjectId(
            reader
                .consume_le_u32()
                .map_err(DeserializeError::ReaderError)?,
        );
        let word = reader
            .consume_le_u32()
            .map_err(DeserializeError::ReaderError)?;
        let len = (word >> 16) as usize;
        let opcode = Opcode((word & 0xffff) as u16);

        if len < 8 {
            return Err(DeserializeError::InvalidLength);
        }

        Ok((sender, opcode, len))
    }

    pub fn deserialize(
        buf: &[u8],
        arg_types: &[PayloadType],
        handles: &[Handle],
    ) -> Result<RawMessage, DeserializeError> {
        let (sender, opcode, total_len) = Self::deserialize_header(buf)?;
        if total_len > buf.len() {
            return Err(DeserializeError::InvalidLength);
        }
        // Only read within this Wayland message; avoids running into a second concatenated frame.
        let mut reader = BytesReader::new(&buf[..total_len]);
        reader.skip(8).map_err(DeserializeError::ReaderError)?;

        let mut args = SmallVec::new();
        let mut handles_iter = handles.iter();
        for arg_type in arg_types {
            match arg_type {
                PayloadType::UInt => {
                    args.push(Payload::UInt(
                        reader
                            .consume_le_u32()
                            .map_err(DeserializeError::ReaderError)?,
                    ));
                }
                PayloadType::Int => {
                    args.push(Payload::Int(
                        reader
                            .consume_le_i32()
                            .map_err(DeserializeError::ReaderError)?,
                    ));
                }
                PayloadType::Fixed => {
                    let val = reader.consume_le_i32().map_err(DeserializeError::ReaderError)?;
                    args.push(Payload::Fixed(val as f32 / 256.0));
                }
                PayloadType::ObjectId => {
                    args.push(Payload::ObjectId(ObjectId(
                        reader
                            .consume_le_u32()
                            .map_err(DeserializeError::ReaderError)?,
                    )));
                }
                PayloadType::NewId => {
                    args.push(Payload::NewId(NewId(
                        reader
                            .consume_le_u32()
                            .map_err(DeserializeError::ReaderError)?,
                    )));
                }
                PayloadType::Handle => {
                    let handle = handles_iter
                        .next()
                        .copied()
                        .ok_or(DeserializeError::TooFewHandles)?;
                    args.push(Payload::Handle(handle));
                }
                PayloadType::Array => {
                    let array_len = reader
                        .consume_le_u32()
                        .map_err(DeserializeError::ReaderError)?
                        as usize;
                    let rem = reader.remaining();
                    if rem.len() < array_len {
                        return Err(DeserializeError::ReaderError(
                            BytesReaderError::TooShort,
                        ));
                    }
                    let array = Vec::from(&rem[..array_len]);
                    reader.skip(array_len).map_err(DeserializeError::ReaderError)?;
                    reader
                        .skip_until_alignment(4)
                        .map_err(DeserializeError::ReaderError)?;
                    args.push(Payload::Array(Array(array)));
                }
                PayloadType::String => {
                    let str_len = reader
                        .consume_le_u32()
                        .map_err(DeserializeError::ReaderError)?
                        as usize;
                    if str_len == 0 {
                        args.push(Payload::String(String::new()));
                        continue;
                    }
                    let rem = reader.remaining();
                    if rem.len() < str_len {
                        return Err(DeserializeError::ReaderError(
                            BytesReaderError::TooShort,
                        ));
                    }
                    let body_len = str_len - 1;
                    let string = str::from_utf8(&rem[..body_len])
                        .map_err(DeserializeError::Utf8Error)?
                        .to_string();
                    reader.skip(str_len).map_err(DeserializeError::ReaderError)?;
                    reader
                        .skip_until_alignment(4)
                        .map_err(DeserializeError::ReaderError)?;
                    args.push(Payload::String(string));
                }
            }
        }

        Ok(RawMessage {
            sender,
            opcode,
            args,
        })
    }
}

pub trait Message: Sized {
    fn into_raw(self, sender: ObjectId) -> RawMessage;
    fn from_raw(
        con: Rc<RefCell<dyn Connection>>,
        m: &RawMessage,
    ) -> Result<Self, DeserializeError>;
}
