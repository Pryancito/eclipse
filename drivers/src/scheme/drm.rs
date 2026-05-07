//! DRM (Direct Rendering Manager) Scheme for drivers
//!
//! This trait allows drivers to implement DRM/KMS functionality.

use super::Scheme;
use alloc::vec::Vec;

/// DRM Device capabilities
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DrmCaps {
    pub has_3d: bool,
    pub has_cursor: bool,
    pub max_width: u32,
    pub max_height: u32,
}

/// GEM (Graphics Execution Manager) handle
#[derive(Debug, Clone, Copy)]
pub struct GemHandle {
    pub id: u32,
    pub size: usize,
    pub phys_addr: u64,
}

/// DRM Connector (output)
#[derive(Debug, Clone, Copy)]
pub struct DrmConnector {
    pub id: u32,
    pub connected: bool,
    pub mm_width: u32,
    pub mm_height: u32,
}

/// DRM CRTC (display controller)
#[derive(Debug, Clone, Copy)]
pub struct DrmCrtc {
    pub id: u32,
    pub fb_id: u32,
    pub x: u32,
    pub y: u32,
}

/// DRM Plane (Overlay, Primary, or Cursor)
#[derive(Debug, Clone, Copy)]
pub struct DrmPlane {
    pub id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub possible_crtcs: u32,
    pub plane_type: u32, // 1=Primary, 2=Cursor, 0=Overlay
}

/// Abstract trait for DRM Driver implementations
pub trait DrmScheme: Scheme {
    fn get_caps(&self) -> DrmCaps;

    /// Import a buffer allocated by the kernel (DRM core)
    fn import_buffer(&self, _handle: GemHandle) -> bool {
        true
    }

    /// Free a buffer
    fn free_buffer(&self, _handle: GemHandle) {}

    /// Create a framebuffer from a GEM handle
    fn create_fb(&self, handle_id: u32, width: u32, height: u32, pitch: u32) -> Option<u32>;

    /// Page flip: atomically switch to a new framebuffer
    fn page_flip(&self, fb_id: u32) -> bool;

    /// Set hardware cursor position and/or image
    fn set_cursor(&self, crtc_id: u32, x: i32, y: i32, handle: u32, flags: u32) -> bool;

    /// Wait for vertical blank on a CRTC
    fn wait_vblank(&self, crtc_id: u32) -> bool;

    /// Get driver resources (fb_ids, crtc_ids, connector_ids)
    fn get_resources(&self) -> (Vec<u32>, Vec<u32>, Vec<u32>);

    /// Get connector info
    fn get_connector(&self, id: u32) -> Option<DrmConnector>;

    /// Get crtc info
    fn get_crtc(&self, id: u32) -> Option<DrmCrtc>;

    /// Get plane info
    fn get_plane(&self, id: u32) -> Option<DrmPlane>;

    /// Get all planes supported by the driver
    fn get_planes(&self) -> Vec<u32>;

    /// Set plane properties
    fn set_plane(
        &self,
        plane_id: u32,
        crtc_id: u32,
        fb_id: u32,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        src_x: u32,
        src_y: u32,
        src_w: u32,
        src_h: u32,
    ) -> bool;

    /// Driver-specific IOCTLs
    fn ioctl(&self, _request: u32, _arg: usize) -> Result<usize, i32> {
        Err(38) // ENOSYS
    }
}
