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

    /// Read-only register/state dump for GPU bring-up debugging, surfaced at
    /// `/proc/gpudbg`. Default: nothing. Hardware drivers override it to read
    /// (never write) device registers post-boot — early BAR0 access can hang
    /// some GPUs, so this is only ever invoked on demand from userspace. With
    /// multiple GPUs every DRM device is dumped, each labelled by its own name.
    fn debug_dump(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// GPU copy-engine bring-up **Step 2**, surfaced (opt-in) at
    /// `/proc/gpustep2`: write the channel instance block into sysmem and issue
    /// the GMMU flush — the first real GPU register writes of the bring-up.
    /// Default: nothing. The hardware driver auto-targets the GPU that does NOT
    /// drive the boot console (so a wedge cannot blank the only output) and
    /// returns a human-readable report. Unlike `debug_dump` this is NOT
    /// read-only, hence its own node: `/proc/gpudbg` stays safe to poll.
    fn bringup_step2(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

    /// GPU copy-engine bring-up **Step 3** (`/proc/gpustep3`): enable the
    /// doorbell, commit the channel runlist, and enable the channel in the
    /// scheduler — with an empty GPFIFO so nothing actually executes. Same
    /// non-console auto-targeting as `bringup_step2`. Default: nothing.
    fn bringup_step3(&self) -> alloc::string::String {
        alloc::string::String::new()
    }

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
