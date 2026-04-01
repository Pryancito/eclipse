//! wl_output — display output information.

use crate::wl::{ObjectId, NewId, Interface, Message, Connection, Payload, PayloadType, DeserializeError, RawMessage};
use crate::wl::wire::Opcode;
use alloc::rc::Rc;
use alloc::string::String;
use core::cell::RefCell;
use smallvec::smallvec;

// ── Requests ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum Request {
    Release,
}

impl Message for Request {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        RawMessage { sender, opcode: Opcode(0), args: smallvec![] }
    }
    fn from_raw(_: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 { 0 => Ok(Request::Release), _ => Err(DeserializeError::UnknownOpcode) }
    }
}

// ── Events ────────────────────────────────────────────────────────────────────

/// `wl_output_subpixel` — pixel arrangement.
pub const SUBPIXEL_UNKNOWN: i32 = 0;
/// `wl_output_transform` — no transform.
pub const TRANSFORM_NORMAL: i32 = 0;
/// `wl_output_mode` flags — current mode.
pub const MODE_CURRENT: u32 = 0x1;

#[derive(Debug)]
pub enum Event {
    /// opcode 0 — position, physical size and make/model
    Geometry {
        x: i32, y: i32,
        physical_width: i32, physical_height: i32,
        subpixel: i32,
        make: String, model: String,
        transform: i32,
    },
    /// opcode 1 — current display mode
    Mode { flags: u32, width: i32, height: i32, refresh: i32 },
    /// opcode 2 — all data sent; client should process
    Done,
    /// opcode 3 — device-pixel scaling factor
    Scale { factor: i32 },
}

impl Message for Event {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Event::Geometry { x, y, physical_width, physical_height, subpixel, make, model, transform } => RawMessage {
                sender, opcode: Opcode(0),
                args: smallvec![
                    x.into(), y.into(),
                    physical_width.into(), physical_height.into(),
                    subpixel.into(),
                    crate::wl::Payload::String(make),
                    crate::wl::Payload::String(model),
                    transform.into(),
                ],
            },
            Event::Mode { flags, width, height, refresh } => RawMessage {
                sender, opcode: Opcode(1),
                args: smallvec![flags.into(), width.into(), height.into(), refresh.into()],
            },
            Event::Done => RawMessage { sender, opcode: Opcode(2), args: smallvec![] },
            Event::Scale { factor } => RawMessage {
                sender, opcode: Opcode(3), args: smallvec![factor.into()],
            },
        }
    }

    fn from_raw(_: Rc<RefCell<dyn Connection>>, _m: &RawMessage) -> Result<Self, DeserializeError> {
        // Clients don't send wl_output events; this is compositor → client only.
        Err(DeserializeError::UnknownOpcode)
    }
}

// ── Interface ─────────────────────────────────────────────────────────────────

pub struct WlOutput {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for WlOutput {
    type Event = Event;
    type Request = Request;

    const NAME: &'static str = "wl_output";
    const VERSION: u32 = 4;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[], // 0: release
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}
