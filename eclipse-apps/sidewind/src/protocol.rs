//! Sidewind Native Protocol (SNP) v2 - Unified Ring Buffer (URB)
//!
//! SNP v2 uses a fixed-size 64-byte command structure for high-performance,
//! zero-copy communication between clients and the compositor.
//!
//! Synchronisation is performed via 64-bit atomic fences.

use alloc::vec::Vec;

/// Magic tag for SNP messages (b"SNPV2" for Sidewind Native Protocol v2)
pub const SNP_MAGIC: u32 = 0x32565053; 

/// Special Layer IDs
pub const SNP_LAYER_ROOT: u32 = 0;

/// Opcodes for the SNP Protocol core interfaces.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SnpOpcode {
    Nop = 0,
    LayerCreate = 1,
    BitBlit = 2,
    Commit = 3,
    Destroy = 4,
    
    // Input / Events (Server -> Client)
    EventKey = 100,
    EventPointerMove = 101,
    EventPointerButton = 102,
    EventConfigure = 103, // Resize/Move
    EventClose = 104,
}

/// A fixed-size 64-byte command structure optimized for CPU cache lines.
#[repr(C, align(64))]
#[derive(Debug, Clone, Copy)]
pub struct SnpCommand {
    /// 0x00: Command operation code
    pub opcode: u32,
    /// 0x04: Target window/layer ID
    pub layer_id: u32,
    /// 0x08: Atomic synchronization fence value
    pub fence: u64,
    /// 0x10: Damage or affected area (x, y, w, h)
    pub rect: [i32; 4],
    /// 0x20: Specific command data
    pub payload: [u8; 32],
}

impl Default for SnpCommand {
    fn default() -> Self {
        Self {
            opcode: SnpOpcode::Nop as u32,
            layer_id: 0,
            fence: 0,
            rect: [0; 4],
            payload: [0; 32],
        }
    }
}

/// Structure of a Command Ring Buffer head/tail control
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SnpRingControl {
    pub head: u32,
    pub tail: u32,
    pub size: u32, // Number of entries (must be power of 2)
    pub padding: u32,
}

/// Payload for LayerCreate
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct SnpPayloadLayerCreate {
    pub width: u16,
    pub height: u16,
    pub format: u32,
    pub name: [u8; 24], // Shared memory name
}

/// Payload for EventKey
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct SnpPayloadEventKey {
    pub key: u32,
    pub state: u32,
}

/// Payload for EventPointer
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct SnpPayloadEventPointer {
    pub x: i32,
    pub y: i32,
    pub buttons: u32,
}

// Utility for serializing payloads into the 32-byte space.
impl SnpCommand {
    pub fn new(opcode: SnpOpcode, layer_id: u32) -> Self {
        Self {
            opcode: opcode as u32,
            layer_id,
            ..Default::default()
        }
    }

    pub unsafe fn set_payload<T: Copy>(&mut self, data: &T) {
        let src = data as *const T as *const u8;
        let dest = self.payload.as_mut_ptr();
        core::ptr::copy_nonoverlapping(src, dest, core::mem::size_of::<T>().min(32));
    }
    
    pub unsafe fn get_payload<T: Copy>(&self) -> T {
        core::ptr::read_unaligned(self.payload.as_ptr() as *const T)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::size_of;

    #[test]
    fn test_command_size_and_alignment() {
        assert_eq!(size_of::<SnpCommand>(), 64);
        assert_eq!(core::mem::align_of::<SnpCommand>(), 64);
    }

    #[test]
    fn test_payload_layer_create() {
        let mut cmd = SnpCommand::new(SnpOpcode::LayerCreate, 1);
        let payload = SnpPayloadLayerCreate {
            width: 800,
            height: 600,
            format: 1,
            name: *b"test_surface\0\0\0\0\0\0\0\0\0\0\0\0",
        };
        unsafe { cmd.set_payload(&payload); }
        
        let extracted: SnpPayloadLayerCreate = unsafe { cmd.get_payload() };
        let width = extracted.width;
        let height = extracted.height;
        let format = extracted.format;
        assert_eq!(width, 800);
        assert_eq!(height, 600);
        assert_eq!(format, 1);
        assert_eq!(&extracted.name[..12], b"test_surface");
    }
}
