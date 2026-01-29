//! Protocol module
//! 
//! Wayland protocol definitions and helpers

/// Wayland protocol version
pub const WAYLAND_VERSION: u32 = 1;

/// Wayland protocol interface names
pub mod interfaces {
    pub const WL_DISPLAY: &str = "wl_display";
    pub const WL_REGISTRY: &str = "wl_registry";
    pub const WL_COMPOSITOR: &str = "wl_compositor";
    pub const WL_SURFACE: &str = "wl_surface";
    pub const WL_OUTPUT: &str = "wl_output";
    pub const WL_SEAT: &str = "wl_seat";
    pub const WL_SHELL: &str = "wl_shell";
    pub const XDG_WM_BASE: &str = "xdg_wm_base";
}

/// Wayland message opcodes
pub mod opcodes {
    // wl_display opcodes
    pub const WL_DISPLAY_ERROR: u16 = 0;
    pub const WL_DISPLAY_DELETE_ID: u16 = 1;
    
    // wl_registry opcodes
    pub const WL_REGISTRY_GLOBAL: u16 = 0;
    pub const WL_REGISTRY_GLOBAL_REMOVE: u16 = 1;
    
    // wl_compositor opcodes
    pub const WL_COMPOSITOR_CREATE_SURFACE: u16 = 0;
    pub const WL_COMPOSITOR_CREATE_REGION: u16 = 1;
}

/// Protocol message structure
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Message {
    pub sender_id: u32,
    pub opcode: u16,
    pub size: u16,
}

impl Message {
    /// Create a new protocol message
    pub fn new(sender_id: u32, opcode: u16, size: u16) -> Self {
        Message {
            sender_id,
            opcode,
            size,
        }
    }
}

/// Protocol object
#[derive(Debug, Clone)]
pub struct Object {
    pub id: u32,
    pub interface: &'static str,
    pub version: u32,
}

impl Object {
    /// Create a new protocol object
    pub fn new(id: u32, interface: &'static str, version: u32) -> Self {
        Object {
            id,
            interface,
            version,
        }
    }
}
