//! Wayland Object Management
//!
//! Manages Wayland protocol objects (surfaces, buffers, etc.)

use crate::protocol::InterfaceType;

/// Object state
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ObjectState {
    Active,
    Destroyed,
}

/// Wayland object representation
#[derive(Debug, Clone, Copy)]
pub struct WaylandObject {
    pub id: u32,
    pub interface: InterfaceType,
    pub state: ObjectState,
}

impl WaylandObject {
    pub fn new(id: u32, interface: InterfaceType, state: ObjectState) -> Self {
        Self {
            id,
            interface,
            state,
        }
    }
}

/// Surface data
pub struct Surface {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub buffer_id: Option<u32>,
    pub committed: bool,
}

impl Surface {
    pub fn new(id: u32) -> Self {
        Self {
            id,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            buffer_id: None,
            committed: false,
        }
    }

    pub fn attach(&mut self, buffer_id: u32) {
        self.buffer_id = Some(buffer_id);
    }

    pub fn commit(&mut self) {
        self.committed = true;
    }

    pub fn damage(&mut self, x: i32, y: i32, width: i32, height: i32) {
        // Mark region as damaged for redraw
    }
}

/// Buffer data
pub struct Buffer {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32,
    pub data_offset: usize,
}

impl Buffer {
    pub fn new(id: u32, width: u32, height: u32, stride: u32, format: u32) -> Self {
        Self {
            id,
            width,
            height,
            stride,
            format,
            data_offset: 0,
        }
    }
}
