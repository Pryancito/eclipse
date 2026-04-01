#![no_std]

/// The magical tag for SideWind messages: b"SWND"
pub const SIDEWIND_TAG: u32 = 0x444E5753; // "SWND"
pub const SIDEWIND_VERSION: u32 = 1;

/// IPC message types for general compositor communication
pub const MSG_TYPE_GRAPHICS: u32 = 0x00000010;
pub const MSG_TYPE_INPUT: u32 = 0x00000040;
pub const MSG_TYPE_WAYLAND: u32 = 0x00000080;
pub const MSG_TYPE_X11: u32 = 0x00000100;

/// SideWind Operations
pub const SWND_OP_CREATE: u32 = 1;
pub const SWND_OP_DESTROY: u32 = 2;
pub const SWND_OP_UPDATE: u32 = 3;
pub const SWND_OP_COMMIT: u32 = 4; // New for Phase 4: explicitly signal buffer swap/update

/// SideWind Event Types (Compositor -> Client)
pub const SWND_EVENT_TYPE_KEY: u32 = 1;
pub const SWND_EVENT_TYPE_MOUSE_MOVE: u32 = 2;
pub const SWND_EVENT_TYPE_MOUSE_BUTTON: u32 = 3;
pub const SWND_EVENT_TYPE_RESIZE: u32 = 4;
pub const SWND_EVENT_TYPE_CLOSE: u32 = 5;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SideWindEvent {
    pub event_type: u32,
    pub data1: i32, // key code, mouse x, new width
    pub data2: i32, // key value, mouse y, new height
    pub data3: i32, // mouse button state, etc.
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SideWindMessage {
    pub tag: u32,  // Should be SIDEWIND_TAG
    pub op: u32,   // Operation (Create, Destroy, etc.)
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub name: [u8; 32], // Shared memory file name in /tmp/
}

impl SideWindMessage {
    pub fn new_create(x: i32, y: i32, w: u32, h: u32, name: &str) -> Self {
        let mut msg = Self {
            tag: SIDEWIND_TAG,
            op: SWND_OP_CREATE,
            x, y, w, h,
            name: [0; 32],
        };
        let bytes = name.as_bytes();
        let len = bytes.len().min(32);
        msg.name[..len].copy_from_slice(&bytes[..len]);
        msg
    }

    pub fn new_commit() -> Self {
        Self {
            tag: SIDEWIND_TAG,
            op: SWND_OP_COMMIT,
            x: 0, y: 0, w: 0, h: 0,
            name: [0; 32],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::size_of;

    #[test]
    fn test_constants() {
        assert_eq!(SIDEWIND_TAG, 0x444E5753);
        assert_eq!(SWND_OP_CREATE, 1);
        assert_eq!(SWND_OP_DESTROY, 2);
        assert_eq!(SWND_OP_UPDATE, 3);
        assert_eq!(SWND_OP_COMMIT, 4);
        assert_eq!(SWND_EVENT_TYPE_KEY, 1);
        assert_eq!(SWND_EVENT_TYPE_RESIZE, 4);
        assert_eq!(SWND_EVENT_TYPE_CLOSE, 5);
    }

    #[test]
    fn test_new_create() {
        let msg = SideWindMessage::new_create(10, 20, 400, 300, "surface");
        assert_eq!(msg.tag, SIDEWIND_TAG);
        assert_eq!(msg.op, SWND_OP_CREATE);
        assert_eq!(msg.x, 10);
        assert_eq!(msg.y, 20);
        assert_eq!(msg.w, 400);
        assert_eq!(msg.h, 300);
        assert_eq!(&msg.name[..7], b"surface");
        assert_eq!(msg.name[7], 0);
    }

    #[test]
    fn test_new_create_name_truncated() {
        let long = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let msg = SideWindMessage::new_create(0, 0, 100, 100, long);
        assert_eq!(msg.name[..32], *b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    }

    #[test]
    fn test_new_commit() {
        let msg = SideWindMessage::new_commit();
        assert_eq!(msg.tag, SIDEWIND_TAG);
        assert_eq!(msg.op, SWND_OP_COMMIT);
        assert_eq!(msg.x, 0);
        assert_eq!(msg.y, 0);
        assert_eq!(msg.w, 0);
        assert_eq!(msg.h, 0);
        assert_eq!(msg.name, [0u8; 32]);
    }

    #[test]
    fn test_sidewind_event_size() {
        assert_eq!(size_of::<SideWindEvent>(), 16);
    }

    #[test]
    fn test_sidewind_message_size() {
        assert_eq!(size_of::<SideWindMessage>(), 56);
    }

    /// Stress: crear muchos new_create y new_commit en bucle.
    #[test]
    fn test_stress_new_create_commit_loop() {
        const ITERS: u32 = 100_000;
        for i in 0..ITERS {
            let msg = SideWindMessage::new_create(
                (i % 1000) as i32,
                (i % 500) as i32,
                (100 + (i % 500)) as u32,
                (100 + (i % 300)) as u32,
                "surface",
            );
            assert_eq!(msg.tag, SIDEWIND_TAG);
            assert_eq!(msg.op, SWND_OP_CREATE);
            let c = SideWindMessage::new_commit();
            assert_eq!(c.op, SWND_OP_COMMIT);
        }
    }
}
