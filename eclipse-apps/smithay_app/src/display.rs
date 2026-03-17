//! Display/DRM/KMS abstraction for the Eclipse compositor.
//!
//! Provides a thin wrapper over the eclipse-syscall DRM primitives.
//! On non-Eclipse targets (e.g. Linux host for tests) every constructor
//! returns `Err(DisplayError::NotAvailable)` so tests can use the mock
//! path without touching real hardware.

pub mod buffer {
    /// Opaque handle to a DRM GEM/dumb buffer.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Handle(pub u32);
}

pub mod control {
    /// Opaque handle to a DRM CRTC.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct CrtcHandle(pub u32);

    /// Opaque handle to a DRM connector.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct ConnectorHandle(pub u32);

    /// Opaque handle to a DRM plane.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct PlaneHandle(pub u32);

    /// Metadata about a single DRM overlay or primary plane.
    #[derive(Debug, Clone, Copy)]
    pub struct PlaneInfo {
        pub handle: PlaneHandle,
        /// 0 = overlay, 1 = primary, 2 = cursor (DRM plane type values).
        pub plane_type: u32,
    }

    pub mod framebuffer {
        /// Opaque handle to a DRM framebuffer object.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
        pub struct Handle(pub u32);
    }
}

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum DisplayError {
    /// DRM device not present or failed to open.
    NotAvailable,
    /// An ioctl returned an error code.
    IoctlFailed(i32),
}

// ── Capability / descriptor types ─────────────────────────────────────────────

/// Capabilities reported by the DRM device.
#[derive(Debug, Clone, Copy, Default)]
pub struct DisplayCaps {
    pub width: u32,
    pub height: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub pitch: u32,
}

/// Allocated framebuffer (front or back buffer).
pub struct FramebufferDesc {
    /// Virtual address of the mapped pixel data.
    pub addr: usize,
    /// DRM framebuffer handle (used for page-flip).
    pub fb_id: control::framebuffer::Handle,
}

/// Allocated dumb buffer (cursor / HUD overlay storage).
pub struct DumbBuffer {
    pub handle: buffer::Handle,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub size: usize,
}

// ── ControlDevice trait (structural sub-typing marker) ────────────────────────

/// Marker used by callers that treat `DisplayDevice` as a generic DRM controller.
pub struct ControlDevice;

// ── DisplayDevice ─────────────────────────────────────────────────────────────

/// Handle to the Eclipse OS DRM display device.
///
/// On the Eclipse target all methods delegate to `eclipse-syscall` DRM calls.
/// On other targets (Linux host / tests) every method returns
/// `Err(DisplayError::NotAvailable)`.
pub struct DisplayDevice {
    /// Raw DRM file-descriptor (usize to avoid libc dependency in non-eclipse builds).
    pub fd: usize,
    pub caps: DisplayCaps,
    pub crtc: control::CrtcHandle,
    pub connector: control::ConnectorHandle,
}

impl DisplayDevice {
    /// Open the primary DRM device and query its capabilities.
    pub fn open() -> Result<Self, DisplayError> {
        #[cfg(target_vendor = "eclipse")]
        {
            let caps = eclipse_syscall::drm_get_caps()
                .map_err(|_| DisplayError::IoctlFailed(-1))?;

            // Compute pitch: align width to 64-pixel boundary and multiply by 4 bytes/pixel.
            let width = caps.max_width.min(1920).max(640);
            let height = caps.max_height.min(1080).max(480);
            let pitch = ((width + 63) & !63) * 4;

            Ok(Self {
                fd: 0,
                caps: DisplayCaps {
                    width,
                    height,
                    max_width: caps.max_width,
                    max_height: caps.max_height,
                    pitch,
                },
                crtc: control::CrtcHandle(0),
                connector: control::ConnectorHandle(0),
            })
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            Err(DisplayError::NotAvailable)
        }
    }

    /// Allocate a front or back framebuffer backed by kernel memory.
    pub fn create_framebuffer(&self) -> Result<FramebufferDesc, DisplayError> {
        #[cfg(target_vendor = "eclipse")]
        {
            let fb_size = (self.caps.pitch as usize) * (self.caps.height as usize);
            let handle = eclipse_syscall::drm_alloc_buffer(fb_size)
                .map_err(|_| DisplayError::IoctlFailed(-1))?;
            let fb_id = eclipse_syscall::drm_create_fb(
                handle,
                self.caps.width,
                self.caps.height,
                self.caps.pitch,
            )
            .map_err(|_| DisplayError::IoctlFailed(-1))?;
            let addr = eclipse_syscall::drm_map_handle(handle)
                .map_err(|_| DisplayError::IoctlFailed(-1))?;
            Ok(FramebufferDesc {
                addr,
                fb_id: control::framebuffer::Handle(fb_id),
            })
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = self;
            Err(DisplayError::NotAvailable)
        }
    }

    /// Allocate a dumb buffer (cursor image, HUD overlay, etc.).
    pub fn create_dumb_buffer(
        &self,
        width: u32,
        height: u32,
        _bpp: u32,
    ) -> Result<DumbBuffer, DisplayError> {
        #[cfg(target_vendor = "eclipse")]
        {
            let pitch = width * 4;
            let size = (pitch as usize) * (height as usize);
            let handle = eclipse_syscall::drm_alloc_buffer(size)
                .map_err(|_| DisplayError::IoctlFailed(-1))?;
            Ok(DumbBuffer {
                handle: buffer::Handle(handle),
                width,
                height,
                pitch,
                size,
            })
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = (self, width, height, _bpp);
            Err(DisplayError::NotAvailable)
        }
    }

    /// Register a dumb buffer as a DRM framebuffer (needed for page-flip / overlay).
    pub fn add_framebuffer(
        &self,
        handle: buffer::Handle,
        width: u32,
        height: u32,
        pitch: u32,
    ) -> Result<control::framebuffer::Handle, DisplayError> {
        #[cfg(target_vendor = "eclipse")]
        {
            let fb_id =
                eclipse_syscall::drm_create_fb(handle.0, width, height, pitch)
                    .map_err(|_| DisplayError::IoctlFailed(-1))?;
            Ok(control::framebuffer::Handle(fb_id))
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = (self, handle, width, height, pitch);
            Err(DisplayError::NotAvailable)
        }
    }

    /// Map a dumb-buffer handle into the process's virtual address space.
    /// Returns the virtual address on success.
    pub fn map_buffer(
        &self,
        handle: buffer::Handle,
        _size: usize,
    ) -> Result<usize, DisplayError> {
        #[cfg(target_vendor = "eclipse")]
        {
            eclipse_syscall::drm_map_handle(handle.0)
                .map_err(|_| DisplayError::IoctlFailed(-1))
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = (self, handle, _size);
            Err(DisplayError::NotAvailable)
        }
    }

    /// Enumerate DRM planes available on this device.
    pub fn plane_resources(
        &self,
    ) -> Result<alloc::vec::Vec<control::PlaneHandle>, DisplayError> {
        // Eclipse OS does not yet expose a plane-enumeration syscall;
        // return an empty list so the caller skips overlay setup gracefully.
        let _ = self;
        Ok(alloc::vec::Vec::new())
    }

    /// Query metadata for a single plane handle.
    pub fn get_plane(
        &self,
        ph: control::PlaneHandle,
    ) -> Result<control::PlaneInfo, DisplayError> {
        let _ = self;
        Ok(control::PlaneInfo {
            handle: ph,
            plane_type: 0,
        })
    }

    /// Block until the next vertical-blank interrupt for `crtc`.
    pub fn wait_vblank(
        &self,
        _crtc: control::CrtcHandle,
    ) -> Result<(), DisplayError> {
        // Eclipse OS currently does not expose a vblank-wait syscall.
        // The caller ignores the return value, so returning Ok is safe.
        Ok(())
    }

    /// Move the hardware cursor to (x, y) and, if `handle` is non-zero,
    /// set the cursor image to that dumb buffer.
    pub fn set_cursor(
        &self,
        _crtc: control::CrtcHandle,
        _x: i32,
        _y: i32,
        _handle: buffer::Handle,
    ) -> Result<(), DisplayError> {
        // Hardware cursor movement is performed via a DRM ioctl; the Eclipse
        // kernel does not yet expose this, so we fall through silently.
        Ok(())
    }

    /// Configure an overlay plane (position + source crop in 16.16 fixed point).
    #[allow(clippy::too_many_arguments)]
    pub fn set_plane(
        &self,
        _plane: control::PlaneHandle,
        _crtc: control::CrtcHandle,
        _fb_id: control::framebuffer::Handle,
        _x: i32,
        _y: i32,
        _w: u32,
        _h: u32,
        _src_x: u32,
        _src_y: u32,
        _src_w: u32,
        _src_h: u32,
    ) -> Result<(), DisplayError> {
        Ok(())
    }
}
