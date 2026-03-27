//! DRM display abstraction for Lunas desktop.
//! Provides traits and structures for display device management.

/// Display capabilities queried from the DRM device.
#[derive(Debug, Clone, Copy)]
pub struct DisplayCaps {
    pub width: u32,
    pub height: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub pitch: u32,
}

/// Framebuffer descriptor for a DRM framebuffer.
#[derive(Debug, Clone, Copy)]
pub struct FramebufferDesc {
    pub fb_id: u32,
    pub handle: u32,
    pub addr: usize,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
}

/// Information about an overlay plane.
#[derive(Debug, Clone, Copy)]
pub struct PlaneInfo {
    pub plane_id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
}

/// DRM display device handle.
pub struct DisplayDevice {
    pub fd: i32,
    pub caps: DisplayCaps,
    pub crtc: u32,
    pub connector: u32,
}

/// Trait for querying display device capabilities.
pub trait Device {
    fn get_caps(&self) -> &DisplayCaps;
}

/// Trait for controlling display output (extends Device).
pub trait ControlDevice: Device {
    fn create_dumb_buffer(&self, w: u32, h: u32, bpp: u32) -> Option<(u32, u32, u32)>;
    fn map_buffer(&self, handle: u32) -> Option<usize>;
    fn add_framebuffer(&self, w: u32, h: u32, pitch: u32, bpp: u32, handle: u32) -> Option<u32>;
    fn page_flip(&self, crtc: u32, fb_id: u32) -> bool;
    fn wait_vblank(&self) -> bool;
    fn set_cursor(&self, crtc: u32, handle: u32, w: u32, h: u32) -> bool;
    fn set_cursor_position(&self, crtc: u32, x: i32, y: i32) -> bool;
}

impl DisplayDevice {
    /// Open a DRM control device. Returns None if not available.
    #[cfg(target_vendor = "eclipse")]
    pub fn open() -> Option<Self> {
        use libc::{open, O_RDWR};
        let fd = unsafe { open(b"drm:control\0".as_ptr() as *const core::ffi::c_char, O_RDWR, 0) };
        if fd < 0 { return None; }

        // Query capabilities (simplified)
        let caps = DisplayCaps {
            width: 1280,
            height: 800,
            max_width: 3840,
            max_height: 2160,
            pitch: 1280 * 4,
        };

        Some(Self { fd, caps, crtc: 0, connector: 0 })
    }

    #[cfg(not(target_vendor = "eclipse"))]
    pub fn open() -> Option<Self> {
        None
    }
}

impl Device for DisplayDevice {
    fn get_caps(&self) -> &DisplayCaps {
        &self.caps
    }
}
