//! Wayland Protocol Definitions
//!
//! This module defines the core Wayland protocol messages and object IDs

use heapless::Vec;

/// Maximum number of arguments in a Wayland message
pub const MAX_ARGS: usize = 8;

/// Wayland protocol opcodes for wl_display
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DisplayOpcode {
    Sync = 0,
    GetRegistry = 1,
}

/// Wayland protocol opcodes for wl_registry
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RegistryOpcode {
    Bind = 0,
}

/// Wayland protocol opcodes for wl_compositor
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CompositorOpcode {
    CreateSurface = 0,
    CreateRegion = 1,
}

/// Wayland protocol opcodes for wl_surface
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SurfaceOpcode {
    Destroy = 0,
    Attach = 1,
    Damage = 2,
    Frame = 3,
    SetOpaqueRegion = 4,
    SetInputRegion = 5,
    Commit = 6,
}

/// Wayland object interface types
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InterfaceType {
    Display,
    Registry,
    Compositor,
    Surface,
    ShmPool,
    Shm,
    Buffer,
    Seat,
    Pointer,
    Keyboard,
    Touch,
    Output,
    Shell,
    ShellSurface,
}

impl InterfaceType {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "wl_display" => Some(Self::Display),
            "wl_registry" => Some(Self::Registry),
            "wl_compositor" => Some(Self::Compositor),
            "wl_surface" => Some(Self::Surface),
            "wl_shm_pool" => Some(Self::ShmPool),
            "wl_shm" => Some(Self::Shm),
            "wl_buffer" => Some(Self::Buffer),
            "wl_seat" => Some(Self::Seat),
            "wl_pointer" => Some(Self::Pointer),
            "wl_keyboard" => Some(Self::Keyboard),
            "wl_touch" => Some(Self::Touch),
            "wl_output" => Some(Self::Output),
            "wl_shell" => Some(Self::Shell),
            "wl_shell_surface" => Some(Self::ShellSurface),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Display => "wl_display",
            Self::Registry => "wl_registry",
            Self::Compositor => "wl_compositor",
            Self::Surface => "wl_surface",
            Self::ShmPool => "wl_shm_pool",
            Self::Shm => "wl_shm",
            Self::Buffer => "wl_buffer",
            Self::Seat => "wl_seat",
            Self::Pointer => "wl_pointer",
            Self::Keyboard => "wl_keyboard",
            Self::Touch => "wl_touch",
            Self::Output => "wl_output",
            Self::Shell => "wl_shell",
            Self::ShellSurface => "wl_shell_surface",
        }
    }
}

/// Wayland message argument types
#[derive(Debug, Clone, Copy)]
pub enum ArgumentType {
    Int(i32),
    Uint(u32),
    Fixed(i32), // Fixed point 24.8
    String(u32, u32), // offset, length
    Object(u32),
    NewId(u32),
    Array(u32, u32), // offset, length
    Fd(i32),
}

/// Wayland wire message header
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MessageHeader {
    pub object_id: u32,
    pub size_and_opcode: u32, // size in upper 16 bits, opcode in lower 16
}

impl MessageHeader {
    pub fn new(object_id: u32, size: u16, opcode: u16) -> Self {
        let size_and_opcode = ((size as u32) << 16) | (opcode as u32);
        Self {
            object_id,
            size_and_opcode,
        }
    }

    pub fn size(&self) -> u16 {
        (self.size_and_opcode >> 16) as u16
    }

    pub fn opcode(&self) -> u16 {
        (self.size_and_opcode & 0xFFFF) as u16
    }
}

/// Wayland protocol message
#[derive(Debug)]
pub struct Message {
    pub header: MessageHeader,
    pub args: Vec<ArgumentType, MAX_ARGS>,
}

impl Message {
    pub fn new(object_id: u32, opcode: u16) -> Self {
        Self {
            header: MessageHeader::new(object_id, 8, opcode), // 8 bytes for header
            args: Vec::new(),
        }
    }

    pub fn add_arg(&mut self, arg: ArgumentType) -> Result<(), &'static str> {
        self.args.push(arg).map_err(|_| "Too many arguments")?;
        
        // Update size
        let arg_size = match arg {
            ArgumentType::Int(_) | ArgumentType::Uint(_) | 
            ArgumentType::Fixed(_) | ArgumentType::Object(_) | 
            ArgumentType::NewId(_) | ArgumentType::Fd(_) => 4,
            ArgumentType::String(_, len) | ArgumentType::Array(_, len) => {
                // Align to 4 bytes
                ((len + 3) / 4) * 4 + 4 // +4 for length field
            }
        };
        
        let current_size = self.header.size();
        let new_size = current_size + arg_size as u16;
        self.header = MessageHeader::new(
            self.header.object_id,
            new_size,
            self.header.opcode()
        );
        
        Ok(())
    }
}
