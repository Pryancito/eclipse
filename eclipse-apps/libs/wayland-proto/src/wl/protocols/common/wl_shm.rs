use crate::wl::{ObjectId, NewId, Interface, Message, Connection, Payload, PayloadType, DeserializeError, RawMessage};
use alloc::rc::Rc;
use core::cell::RefCell;
use smallvec::smallvec;

#[derive(Debug)]
pub enum Request {
    CreatePool { id: NewId, fd: i32, size: i32 },
}

impl Message for Request {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Request::CreatePool { id, fd, size } => RawMessage {
                sender,
                opcode: crate::wl::Opcode(0),
                args: smallvec![id.into(), fd.into(), size.into()],
            },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => {
                if m.args.len() != 3 { return Err(DeserializeError::InvalidLength); }
                let id = match m.args[0] {
                    Payload::NewId(id) => id,
                    _ => return Err(DeserializeError::UnexpectedType),
                };
                let fd = match m.args[1] {
                    Payload::Int(fd) => fd,
                    _ => return Err(DeserializeError::UnexpectedType),
                };
                let size = match m.args[2] {
                    Payload::Int(size) => size,
                    _ => return Err(DeserializeError::UnexpectedType),
                };
                Ok(Request::CreatePool { id, fd, size })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

#[derive(Debug)]
pub enum Event {
    Format { format: u32 },
}

impl Message for Event {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Event::Format { format } => RawMessage {
                sender,
                opcode: crate::wl::Opcode(0),
                args: smallvec![format.into()],
            },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => {
                if m.args.len() != 1 { return Err(DeserializeError::InvalidLength); }
                let format = match m.args[0] {
                    Payload::UInt(f) => f,
                    _ => return Err(DeserializeError::UnexpectedType),
                };
                Ok(Event::Format { format })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

pub struct WlShm {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for WlShm {
    type Event = Event;
    type Request = Request;

    const NAME: &'static str = "wl_shm";
    const VERSION: u32 = 1;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[PayloadType::NewId, PayloadType::Handle, PayloadType::Int], // create_pool(id, fd, size)
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self {
        Self { con, id }
    }

    fn connection(&self) -> &Rc<RefCell<dyn Connection>> {
        &self.con
    }

    fn id(&self) -> ObjectId {
        self.id
    }

    fn as_new_id(&self) -> NewId {
        NewId(self.id.0)
    }
}

// ---- WlShmPool ----

#[derive(Debug)]
pub enum PoolRequest {
    CreateBuffer { id: NewId, offset: i32, width: i32, height: i32, stride: i32, format: u32 },
    Destroy,
    Resize { size: i32 },
}

impl Message for PoolRequest {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            PoolRequest::CreateBuffer { id, offset, width, height, stride, format } => RawMessage {
                sender,
                opcode: crate::wl::Opcode(0),
                args: smallvec![id.into(), offset.into(), width.into(), height.into(), stride.into(), format.into()],
            },
            PoolRequest::Destroy => RawMessage {
                sender,
                opcode: crate::wl::Opcode(1),
                args: smallvec![],
            },
            PoolRequest::Resize { size } => RawMessage {
                sender,
                opcode: crate::wl::Opcode(2),
                args: smallvec![size.into()],
            },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => {
                if m.args.len() != 6 { return Err(DeserializeError::InvalidLength); }
                let id = match m.args[0] {
                    Payload::NewId(id) => id,
                    _ => return Err(DeserializeError::UnexpectedType),
                };
                let offset = match m.args[1] {
                    Payload::Int(v) => v,
                    _ => return Err(DeserializeError::UnexpectedType),
                };
                let width = match m.args[2] {
                    Payload::Int(v) => v,
                    _ => return Err(DeserializeError::UnexpectedType),
                };
                let height = match m.args[3] {
                    Payload::Int(v) => v,
                    _ => return Err(DeserializeError::UnexpectedType),
                };
                let stride = match m.args[4] {
                    Payload::Int(v) => v,
                    _ => return Err(DeserializeError::UnexpectedType),
                };
                let format = match m.args[5] {
                    Payload::UInt(v) => v,
                    _ => return Err(DeserializeError::UnexpectedType),
                };
                Ok(PoolRequest::CreateBuffer { id, offset, width, height, stride, format })
            }
            1 => Ok(PoolRequest::Destroy),
            2 => {
                 if m.args.len() != 1 { return Err(DeserializeError::InvalidLength); }
                 let size = match m.args[0] {
                     Payload::Int(v) => v,
                     _ => return Err(DeserializeError::UnexpectedType),
                 };
                 Ok(PoolRequest::Resize { size })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

pub enum PoolEvent {}
impl Message for PoolEvent {
    fn into_raw(self, _sender: ObjectId) -> RawMessage { unreachable!() }
    fn from_raw(_con: Rc<RefCell<dyn Connection>>, _m: &RawMessage) -> Result<Self, DeserializeError> { Err(DeserializeError::UnknownOpcode) }
}

pub struct WlShmPool {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for WlShmPool {
    type Event = PoolEvent;
    type Request = PoolRequest;

    const NAME: &'static str = "wl_shm_pool";
    const VERSION: u32 = 1;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[PayloadType::NewId, PayloadType::Int, PayloadType::Int, PayloadType::Int, PayloadType::Int, PayloadType::UInt], // create_buffer
        &[], // destroy
        &[PayloadType::Int], // resize
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self {
        Self { con, id }
    }

    fn connection(&self) -> &Rc<RefCell<dyn Connection>> {
        &self.con
    }

    fn id(&self) -> ObjectId {
        self.id
    }

    fn as_new_id(&self) -> NewId {
        NewId(self.id.0)
    }
}

impl WlShm {
    /// Send wl_shm.create_pool to the compositor.
    /// Returns a client-side WlShmPool object.
    pub fn create_pool(&mut self, id: NewId, fd: i32, size: i32) -> Result<WlShmPool, crate::wl::connection::SendError> {
        self.con.borrow_mut().send(
            self.id,
            crate::wl::Opcode(0),
            &[id.into(), fd.into(), size.into()],
            &[],
        )?;
        Ok(WlShmPool::new(self.con.clone(), id.as_id()))
    }
}

impl WlShmPool {
    /// Send wl_shm_pool.create_buffer to the compositor.
    /// Returns a client-side WlBuffer.
    pub fn create_buffer(
        &mut self,
        id: NewId,
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: u32,
    ) -> Result<crate::wl::protocols::common::wl_buffer::WlBuffer, crate::wl::connection::SendError> {
        self.con.borrow_mut().send(
            self.id,
            crate::wl::Opcode(0),
            &[id.into(), offset.into(), width.into(), height.into(), stride.into(), format.into()],
            &[],
        )?;
        Ok(crate::wl::protocols::common::wl_buffer::WlBuffer::new(self.con.clone(), id.as_id()))
    }

    /// Send wl_shm_pool.destroy.
    pub fn destroy(&mut self) -> Result<(), crate::wl::connection::SendError> {
        self.con.borrow_mut().send(self.id, crate::wl::Opcode(1), &[], &[])
    }

    /// Send wl_shm_pool.resize.
    pub fn resize(&mut self, new_size: i32) -> Result<(), crate::wl::connection::SendError> {
        self.con.borrow_mut().send(self.id, crate::wl::Opcode(2), &[new_size.into()], &[])
    }
}
