//! Display/DRM/KMS abstraction for the Eclipse compositor.
//!
//! Provides a thin wrapper over the eclipse-syscall DRM primitives.
//! This module handles buffer allocation (Dumb buffers), framebuffer registration,
//! and page flipping for the Eclipse OS display system.

#![allow(dead_code)]

#[cfg(target_vendor = "eclipse")]
use eclipse_syscall as syscall;

use std::vec::Vec;

pub mod buffer {
    /// A handle to a buffer (GEM handle).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct Handle(pub u32);
    
    /// Information about a dumb buffer.
    #[derive(Debug, Clone, Copy)]
    pub struct DumbBuffer {
        pub handle: Handle,
        pub width: u32,
        pub height: u32,
        pub pitch: u32,
        pub size: usize,
    }
}

pub mod control {
    pub mod framebuffer {
        /// A handle to a framebuffer.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
        pub struct Handle(pub u32);
    }
    
    use super::buffer;
    
    /// Information about a framebuffer.
    #[derive(Debug, Clone, Copy)]
    pub struct FramebufferInfo {
        pub handle: framebuffer::Handle,
        pub gem_handle: buffer::Handle,
        pub width: u32,
        pub height: u32,
        pub pitch: u32,
    }

    #[derive(Debug, Clone, Copy, Default)]
    pub struct ConnectorHandle(pub u32);

    #[derive(Debug, Clone, Copy)]
    pub struct ConnectorInfo {
        pub handle: ConnectorHandle,
        pub connected: bool,
        pub width_mm: u32,
        pub height_mm: u32,
    }

    #[derive(Debug, Clone, Copy, Default)]
    pub struct CrtcHandle(pub u32);

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct PlaneHandle(pub u32);

    #[derive(Debug, Clone, Copy)]
    pub struct PlaneInfo {
        pub handle: PlaneHandle,
        pub crtc_id: CrtcHandle,
        pub fb_id: framebuffer::Handle,
        pub possible_crtcs: u32,
        /// 0 = overlay, 1 = primary, 2 = cursor (DRM plane type values).
        pub plane_type: u32,
    }
}

// DRM IOCTL Constants (Eclipse OS specific, matching kernel)
const DRM_IOCTL_GET_CAP: u32 = 0xC0106405;
const DRM_IOCTL_MODE_CREATE_DUMB: u32 = 0xC02064B2;
const DRM_IOCTL_MODE_MAP_DUMB: u32 = 0xC01064B3;
const DRM_IOCTL_MODE_ADDFB: u32 = 0xC01C64AE;
const DRM_IOCTL_MODE_PAGE_FLIP: u32 = 0xC01864B0;
const DRM_IOCTL_GEM_CLOSE: u32 = 0x40086444;
const DRM_IOCTL_MODE_DESTROYFB: u32 = 0xC00464AF;
const DRM_IOCTL_MODE_GETRESOURCES: u32 = 0xC04064A0;
const DRM_IOCTL_MODE_GETCONNECTOR: u32 = 0xC05064A7;
const DRM_IOCTL_MODE_GETCRTC: u32 = 0xC06864A1;
const DRM_IOCTL_WAIT_VBLANK: u32 = 0xC018643A;
const DRM_IOCTL_MODE_CURSOR: u32 = 0xC01C64A3;

const DRM_IOCTL_MODE_GETPLANERESOURCES: u32 = 0xC01064B5;
const DRM_IOCTL_MODE_GETPLANE: u32 = 0xC02064B6;
const DRM_IOCTL_MODE_SETPLANE: u32 = 0xC04464B7;

const DRM_CURSOR_SET: u32 = 0x01;
const DRM_CURSOR_MOVE: u32 = 0x02;
// Framebuffer IOCTLs
const FBIOGET_VSCREENINFO: u32 = 0x4600;
const FBIOGET_FSCREENINFO: u32 = 0x4602;

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct fb_bitfield {
    pub offset: u32,
    pub length: u32,
    pub msb_right: u32,
}

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct fb_var_screeninfo {
    pub xres: u32,
    pub yres: u32,
    pub xres_virtual: u32,
    pub yres_virtual: u32,
    pub xoffset: u32,
    pub yoffset: u32,
    pub bits_per_pixel: u32,
    pub grayscale: u32,
    pub red: fb_bitfield,
    pub green: fb_bitfield,
    pub blue: fb_bitfield,
    pub transp: fb_bitfield,
    pub nonstd: u32,
    pub activate: u32,
    pub height: u32,
    pub width: u32,
    pub accel_flags: u32,
    pub pixclock: u32,
    pub left_margin: u32,
    pub right_margin: u32,
    pub upper_margin: u32,
    pub lower_margin: u32,
    pub hsync_len: u32,
    pub vsync_len: u32,
    pub sync: u32,
    pub vmode: u32,
    pub rotate: u32,
    pub colorspace: u32,
    pub reserved: [u32; 4],
}

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct fb_fix_screeninfo {
    pub id: [u8; 16],
    pub smem_start: u64,
    pub smem_len: u32,
    pub type_: u32,
    pub type_aux: u32,
    pub visual: u32,
    pub xpanstep: u16,
    pub ypanstep: u16,
    pub ywrapstep: u16,
    pub line_length: u32,
    pub mmio_start: u64,
    pub mmio_len: u32,
    pub accel: u32,
    pub capabilities: u16,
    pub reserved: [u16; 2],
}

/// Common trait for DRM-like devices.
pub trait Device {
    type Error;
    
    /// Get the capabilities of the display device.
    fn get_caps(&self) -> Result<DisplayCaps, Self::Error>;
}

/// Trait for display control operations (KMS-like).
pub trait ControlDevice: Device {
    /// Create a dumb buffer.
    fn create_dumb_buffer(&self, width: u32, height: u32, bpp: u32) -> Result<buffer::DumbBuffer, Self::Error>;
    
    /// Map a buffer handle into the process's address space.
    fn map_buffer(&self, handle: buffer::Handle, size: usize) -> Result<*mut u8, Self::Error>;
    
    /// Add a framebuffer using a buffer handle.
    fn add_framebuffer(&self, handle: buffer::Handle, width: u32, height: u32, pitch: u32) -> Result<control::framebuffer::Handle, Self::Error>;
    
    /// Perform a page flip to a specific framebuffer.
    fn page_flip(&self, fb: control::framebuffer::Handle) -> Result<(), Self::Error>;

    /// Wait for vblank on a CRTC.
    fn wait_vblank(&self, crtc: control::CrtcHandle) -> Result<(), Self::Error>;

    /// Move the hardware cursor.
    fn set_cursor(&self, crtc: control::CrtcHandle, x: i32, y: i32, handle: buffer::Handle) -> Result<(), Self::Error>;

    /// Close a GEM handle.
    fn gem_close(&self, handle: buffer::Handle) -> Result<(), Self::Error>;

    /// Destroy a framebuffer.
    fn destroy_framebuffer(&self, fb: control::framebuffer::Handle) -> Result<(), Self::Error>;

    /// Enumerate DRM resources.
    fn resource_handles(&self) -> Result<(Vec<control::framebuffer::Handle>, Vec<control::CrtcHandle>, Vec<control::ConnectorHandle>), Self::Error>;

    /// Enumerate DRM planes.
    fn plane_resources(&self) -> Result<Vec<control::PlaneHandle>, Self::Error>;

    /// Query plane metadata.
    fn get_plane(&self, plane: control::PlaneHandle) -> Result<control::PlaneInfo, Self::Error>;

    /// Query connector metadata.
    fn get_connector(&self, connector: control::ConnectorHandle) -> Result<control::ConnectorInfo, Self::Error>;

    /// Configure a plane.
    fn set_plane(
        &self,
        plane: control::PlaneHandle,
        crtc: control::CrtcHandle,
        fb: control::framebuffer::Handle,
        crtc_x: i32, crtc_y: i32, crtc_w: u32, crtc_h: u32,
        src_x: u32, src_y: u32, src_w: u32, src_h: u32,
    ) -> Result<(), Self::Error>;
}

/// Capabilities reported by the DRM device.
#[derive(Debug, Clone, Copy, Default)]
pub struct DisplayCaps {
    pub width: u32,
    pub height: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub pitch: u32,
}

#[derive(Debug, Clone, Copy)]
/// Allocated framebuffer (front or back buffer).
pub struct FramebufferDesc {
    /// DRM framebuffer handle (used for page-flip).
    pub fb_id: control::framebuffer::Handle,
    /// Buffer handle backing the framebuffer.
    pub handle: buffer::Handle,
    /// Virtual address of the mapped pixel data.
    pub addr: usize,
    /// Width of the framebuffer in pixels.
    pub width: u32,
    /// Height of the framebuffer in pixels.
    pub height: u32,
    /// Pitch (bytes per scanline).
    pub pitch: u32,
}

#[derive(Debug)]
pub enum DisplayError {
    #[cfg(target_vendor = "eclipse")]
    Syscall(syscall::Error),
    InvalidMapping,
    OpenFailed,
    NotAvailable,
}

#[cfg(target_vendor = "eclipse")]
impl From<syscall::Error> for DisplayError {
    fn from(e: syscall::Error) -> Self {
        DisplayError::Syscall(e)
    }
}

/// Handle to the Eclipse OS DRM display device.
#[derive(Debug)]
pub struct DisplayDevice {
    pub fd: usize,
    pub caps: DisplayCaps,
    pub crtc: control::CrtcHandle,
    pub connector: control::ConnectorHandle,
    pub is_fallback: bool,
    pub fb_ptr: Option<*mut u8>,
}

impl Device for DisplayDevice {
    type Error = DisplayError;
    
    fn get_caps(&self) -> Result<DisplayCaps, Self::Error> {
        Ok(self.caps)
    }
}

impl ControlDevice for DisplayDevice {
    fn create_dumb_buffer(&self, width: u32, height: u32, bpp: u32) -> Result<buffer::DumbBuffer, Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            if self.is_fallback {
                let size = (width * height * (bpp / 8)) as usize;
                // Use anonymous mmap for fallback buffers
                let addr = syscall::mmap(
                    0,
                    size,
                    syscall::flag::PROT_READ | syscall::flag::PROT_WRITE,
                    syscall::flag::MAP_PRIVATE | syscall::flag::MAP_ANONYMOUS,
                    -1,
                    0
                )?;
                // The handle will be the address itself for simple tracking
                return Ok(buffer::DumbBuffer {
                    handle: buffer::Handle(addr as u32),
                    width,
                    height,
                    pitch: width * (bpp / 8),
                    size,
                });
            }

            #[repr(C)]
            struct DrmModeCreateDumb {
                height: u32, width: u32, bpp: u32, flags: u32,
                handle: u32, pitch: u32, size: u64,
            }
            let mut args = DrmModeCreateDumb {
                height, width, bpp, flags: 0,
                handle: 0, pitch: 0, size: 0,
            };
            syscall::ioctl(self.fd, DRM_IOCTL_MODE_CREATE_DUMB as usize, &mut args as *mut _ as usize)?;
            Ok(buffer::DumbBuffer {
                handle: buffer::Handle(args.handle),
                width,
                height,
                pitch: args.pitch,
                size: args.size as usize,
            })
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = (width, height, bpp);
            Err(DisplayError::NotAvailable)
        }
    }

    fn map_buffer(&self, handle: buffer::Handle, size: usize) -> Result<*mut u8, Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            if self.is_fallback {
                // In fallback mode, the handle IS the address
                return Ok(handle.0 as *mut u8);
            }

            #[repr(C)]
            struct DrmModeMapDumb {
                handle: u32, pad: u32, offset: u64,
            }
            let mut args = DrmModeMapDumb { handle: handle.0, pad: 0, offset: 0 };
            syscall::ioctl(self.fd, DRM_IOCTL_MODE_MAP_DUMB as usize, &mut args as *mut _ as usize)?;
            let addr = syscall::mmap(
                0, 
                size, 
                syscall::flag::PROT_READ | syscall::flag::PROT_WRITE, 
                syscall::flag::MAP_SHARED, 
                self.fd as isize, 
                args.offset as usize
            )?;
            Ok(addr as *mut u8)
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = (handle, size);
            Err(DisplayError::NotAvailable)
        }
    }

    fn add_framebuffer(&self, handle: buffer::Handle, width: u32, height: u32, pitch: u32) -> Result<control::framebuffer::Handle, Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            if self.is_fallback {
                // In fallback mode, use the buffer handle as the FB handle
                return Ok(control::framebuffer::Handle(handle.0));
            }

            #[repr(C)]
            struct DrmModeFbCmd {
                fb_id: u32, width: u32, height: u32, pitch: u32,
                bpp: u32, depth: u32, handle: u32,
            }
            let mut args = DrmModeFbCmd {
                fb_id: 0, width, height, pitch,
                bpp: 32, depth: 24, handle: handle.0,
            };
            syscall::ioctl(self.fd, DRM_IOCTL_MODE_ADDFB as usize, &mut args as *mut _ as usize)?;
            Ok(control::framebuffer::Handle(args.fb_id))
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = (handle, width, height, pitch);
            Err(DisplayError::NotAvailable)
        }
    }

    fn page_flip(&self, fb: control::framebuffer::Handle) -> Result<(), Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            if self.is_fallback {
                if let Some(dest) = self.fb_ptr {
                    let src = fb.0 as *const u8;
                    let size = (self.caps.pitch * self.caps.height) as usize;
                    unsafe {
                        core::ptr::copy_nonoverlapping(src, dest, size);
                    }
                }
                return Ok(());
            }

            #[repr(C)]
            struct DrmModeCrtcPageFlip {
                crtc_id: u32, fb_id: u32, flags: u32, reserved: u32, user_data: u64,
            }
            let args = DrmModeCrtcPageFlip {
                crtc_id: self.crtc.0,
                fb_id: fb.0,
                flags: 0,
                reserved: 0,
                user_data: 0,
            };
            syscall::ioctl(self.fd, DRM_IOCTL_MODE_PAGE_FLIP as usize, &args as *const _ as usize)?;
            Ok(())
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = fb;
            Err(DisplayError::NotAvailable)
        }
    }

    fn wait_vblank(&self, crtc: control::CrtcHandle) -> Result<(), Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            if self.is_fallback {
                // TODO: Implement a better yield or wait if needed
                return Ok(());
            }

            #[repr(C)]
            struct DrmWaitVblank {
                request: u32,
                crtc_id: u32,
                reply: u64,
            }
            let mut args = DrmWaitVblank {
                request: 0,
                crtc_id: crtc.0,
                reply: 0,
            };
            // Best effort, kernel might not support it yet
            let _ = syscall::ioctl(self.fd, DRM_IOCTL_WAIT_VBLANK as usize, &mut args as *mut _ as usize);
            Ok(())
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = crtc;
            Err(DisplayError::NotAvailable)
        }
    }

    fn set_cursor(&self, crtc: control::CrtcHandle, x: i32, y: i32, handle: buffer::Handle) -> Result<(), Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            if self.is_fallback {
                // Software cursor should be handled by the compositor's renderer
                return Ok(());
            }

            #[repr(C)]
            struct DrmModeCursor {
                flags: u32, crtc_id: u32, x: i32, y: i32, width: u32, height: u32, handle: u32,
            }
            let args = DrmModeCursor {
                flags: if handle.0 != 0 { DRM_CURSOR_SET | DRM_CURSOR_MOVE } else { DRM_CURSOR_MOVE },
                crtc_id: crtc.0,
                x,
                y,
                width: 64, height: 64,
                handle: handle.0,
            };
            // Best effort
            let _ = syscall::ioctl(self.fd, DRM_IOCTL_MODE_CURSOR as usize, &args as *const _ as usize);
            Ok(())
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = (crtc, x, y, handle);
            Err(DisplayError::NotAvailable)
        }
    }

    fn gem_close(&self, handle: buffer::Handle) -> Result<(), Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            if self.is_fallback {
                // For fallback, we don't know the size easily here without tracking.
                // However, since Lunas usually keeps buffers for the lifetime of the process,
                // or we could track sizes in a map. For now, we'll leak if it's dynamic,
                // but Lunas usually only has 2-3 persistent buffers.
                return Ok(());
            }

            #[repr(C)]
            struct DrmGemClose { handle: u32, pad: u32 }
            let args = DrmGemClose { handle: handle.0, pad: 0 };
            syscall::ioctl(self.fd, DRM_IOCTL_GEM_CLOSE as usize, &args as *const _ as usize)?;
            Ok(())
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = handle;
            Err(DisplayError::NotAvailable)
        }
    }

    fn destroy_framebuffer(&self, fb: control::framebuffer::Handle) -> Result<(), Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            syscall::ioctl(self.fd, DRM_IOCTL_MODE_DESTROYFB as usize, &fb.0 as *const _ as usize)?;
            Ok(())
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = fb;
            Err(DisplayError::NotAvailable)
        }
    }

    fn resource_handles(&self) -> Result<(Vec<control::framebuffer::Handle>, Vec<control::CrtcHandle>, Vec<control::ConnectorHandle>), Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            #[repr(C)]
            #[derive(Default)]
            struct DrmModeCardRes {
                fb_id_ptr: u64,
                crtc_id_ptr: u64,
                connector_id_ptr: u64,
                encoder_id_ptr: u64,
                count_fbs: u32,
                count_crtcs: u32,
                count_connectors: u32,
                count_encoders: u32,
                min_width: u32,
                max_width: u32,
                min_height: u32,
                max_height: u32,
            }
            let mut args = DrmModeCardRes::default();
            
            // First call to get counts
            syscall::ioctl(self.fd, DRM_IOCTL_MODE_GETRESOURCES as usize, &mut args as *mut _ as usize)?;
            
            let mut fbs_raw = std::vec![0u32; args.count_fbs as usize];
            let mut crtcs_raw = std::vec![0u32; args.count_crtcs as usize];
            let mut conns_raw = std::vec![0u32; args.count_connectors as usize];
            let mut encoders_raw = std::vec![0u32; args.count_encoders as usize];
            
            unsafe {
                if args.count_fbs > 0 {
                    args.fb_id_ptr = fbs_raw.as_mut_ptr() as u64;
                }
                if args.count_crtcs > 0 {
                    args.crtc_id_ptr = crtcs_raw.as_mut_ptr() as u64;
                }
                if args.count_connectors > 0 {
                    args.connector_id_ptr = conns_raw.as_mut_ptr() as u64;
                }
                if args.count_encoders > 0 {
                    args.encoder_id_ptr = encoders_raw.as_mut_ptr() as u64;
                }

                // Second call to fill buffers
                syscall::ioctl(self.fd, DRM_IOCTL_MODE_GETRESOURCES as usize, &mut args as *mut _ as usize)?;
            }

            let mut fbs = Vec::with_capacity(fbs_raw.len());
            for id in fbs_raw { fbs.push(control::framebuffer::Handle(id)); }
            
            let mut crtcs = Vec::with_capacity(crtcs_raw.len());
            for id in crtcs_raw { crtcs.push(control::CrtcHandle(id)); }
            
            let mut conns = Vec::with_capacity(conns_raw.len());
            for id in conns_raw { conns.push(control::ConnectorHandle(id)); }

            Ok((fbs, crtcs, conns))
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            Err(DisplayError::NotAvailable)
        }
    }

    fn plane_resources(&self) -> Result<Vec<control::PlaneHandle>, Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            #[repr(C)]
            struct DrmModeGetPlaneRes {
                plane_id_ptr: u64,
                count_planes: u32,
            }
            let mut args = DrmModeGetPlaneRes {
                plane_id_ptr: 0,
                count_planes: 0,
            };
            if let Err(_) = syscall::ioctl(self.fd, DRM_IOCTL_MODE_GETPLANERESOURCES as usize, &mut args as *mut _ as usize) {
                return Ok(Vec::new());
            }
            
            let mut planes = Vec::with_capacity(args.count_planes as usize);
            unsafe {
                planes.set_len(args.count_planes as usize);
                args.plane_id_ptr = planes.as_ptr() as u64;
                syscall::ioctl(self.fd, DRM_IOCTL_MODE_GETPLANERESOURCES as usize, &mut args as *mut _ as usize)?;
            }
            Ok(planes.into_iter().map(control::PlaneHandle).collect())
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            Err(DisplayError::NotAvailable)
        }
    }

    fn get_plane(&self, plane: control::PlaneHandle) -> Result<control::PlaneInfo, Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            #[repr(C)]
            struct DrmModeGetPlane {
                plane_id: u32, crtc_id: u32, fb_id: u32,
                possible_crtcs: u32, gamma_size: u32,
                count_formats: u32, format_type_ptr: u64,
            }
            let mut args = DrmModeGetPlane {
                plane_id: plane.0, crtc_id: 0, fb_id: 0,
                possible_crtcs: 0, gamma_size: 0,
                count_formats: 0, format_type_ptr: 0,
            };
            syscall::ioctl(self.fd, DRM_IOCTL_MODE_GETPLANE as usize, &mut args as *mut _ as usize)?;
            Ok(control::PlaneInfo {
                handle: plane,
                crtc_id: control::CrtcHandle(args.crtc_id),
                fb_id: control::framebuffer::Handle(args.fb_id),
                possible_crtcs: args.possible_crtcs,
                plane_type: 0, 
            })
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = plane;
            Err(DisplayError::NotAvailable)
        }
    }

    fn set_plane(
        &self,
        plane: control::PlaneHandle,
        crtc: control::CrtcHandle,
        fb: control::framebuffer::Handle,
        crtc_x: i32, crtc_y: i32, crtc_w: u32, crtc_h: u32,
        src_x: u32, src_y: u32, src_w: u32, src_h: u32,
    ) -> Result<(), Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            #[repr(C)]
            struct DrmModeSetPlane {
                plane_id: u32, crtc_id: u32, fb_id: u32, flags: u32,
                crtc_x: i32, crtc_y: i32, crtc_w: u32, crtc_h: u32,
                src_x: u32, src_y: u32, src_w: u32, src_h: u32,
            }
            let args = DrmModeSetPlane {
                plane_id: plane.0,
                crtc_id: crtc.0,
                fb_id: fb.0,
                flags: 0,
                crtc_x, crtc_y, crtc_w, crtc_h,
                src_x, src_y, src_w, src_h,
            };
            syscall::ioctl(self.fd, DRM_IOCTL_MODE_SETPLANE as usize, &args as *const _ as usize)?;
            Ok(())
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = (plane, crtc, fb, crtc_x, crtc_y, crtc_w, crtc_h, src_x, src_y, src_w, src_h);
            Err(DisplayError::NotAvailable)
        }
    }

    fn get_connector(&self, connector: control::ConnectorHandle) -> Result<control::ConnectorInfo, Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            #[repr(C)]
            struct DrmModeGetConnector {
                encoders_ptr: u64, modes_ptr: u64, props_ptr: u64, prop_values_ptr: u64,
                count_modes: u32, count_props: u32, count_encoders: u32,
                encoder_id: u32, connector_id: u32, connector_type: u32, connector_type_id: u32,
                connection: u32, mm_width: u32, mm_height: u32, subpixel: u32, pad: u32,
            }
            let mut args = DrmModeGetConnector {
                encoders_ptr: 0, modes_ptr: 0, props_ptr: 0, prop_values_ptr: 0,
                count_modes: 0, count_props: 0, count_encoders: 0,
                encoder_id: 0, connector_id: connector.0, connector_type: 0, connector_type_id: 0,
                connection: 0, mm_width: 0, mm_height: 0, subpixel: 0, pad: 0,
            };
            syscall::ioctl(self.fd, DRM_IOCTL_MODE_GETCONNECTOR as usize, &mut args as *mut _ as usize)?;
            Ok(control::ConnectorInfo {
                handle: connector,
                connected: args.connection == 1,
                width_mm: args.mm_width,
                height_mm: args.mm_height,
            })
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = connector;
            Err(DisplayError::NotAvailable)
        }
    }
}

impl DisplayDevice {
    /// Open the DRM device and discover resources.
    pub fn open() -> Result<Self, DisplayError> {
        #[cfg(target_vendor = "eclipse")]
        {
            let mut is_fallback = false;
            let mut fd = match syscall::open("drm:control", 0) {
                Ok(f) => f,
                Err(_) => {
                    // Fallback to legacy framebuffer
                    let f = syscall::open("dev:fb0", 0).map_err(|_| DisplayError::OpenFailed)?;
                    is_fallback = true;
                    f
                }
            };

            if is_fallback {
                let mut var_info = fb_var_screeninfo::default();
                let mut fix_info = fb_fix_screeninfo::default();
                
                syscall::ioctl(fd, FBIOGET_VSCREENINFO as usize, &mut var_info as *mut _ as usize).map_err(|_| DisplayError::OpenFailed)?;
                syscall::ioctl(fd, FBIOGET_FSCREENINFO as usize, &mut fix_info as *mut _ as usize).map_err(|_| DisplayError::OpenFailed)?;

                let width = var_info.xres;
                let height = var_info.yres;
                let pitch = fix_info.line_length;
                let size = fix_info.smem_len as usize;

                let fb_ptr = if let Ok(addr) = syscall::mmap(
                    0,
                    size,
                    syscall::flag::PROT_READ | syscall::flag::PROT_WRITE,
                    syscall::flag::MAP_SHARED,
                    fd as isize,
                    0
                ) {
                    Some(addr as *mut u8)
                } else {
                    None
                };

                return Ok(DisplayDevice {
                    fd,
                    caps: DisplayCaps {
                        width,
                        height,
                        max_width: width,
                        max_height: height,
                        pitch,
                    },
                    crtc: control::CrtcHandle(0),
                    connector: control::ConnectorHandle(0),
                    is_fallback: true,
                    fb_ptr,
                });
            }
            
            // Standard DRM Path
            let caps = match syscall::drm_get_caps() {
                Ok(c) => c,
                Err(_) => return Err(DisplayError::OpenFailed),
            };

            let width = if caps.max_width > 0 { caps.max_width } else { 1280 };
            let height = if caps.max_height > 0 { caps.max_height } else { 800 };
            let pitch = width * 4;
            
            let mut device = DisplayDevice {
                fd,
                caps: DisplayCaps {
                    width,
                    height,
                    max_width: caps.max_width,
                    max_height: caps.max_height,
                    pitch,
                },
                crtc: control::CrtcHandle(0),
                connector: control::ConnectorHandle(0),
                is_fallback: false,
                fb_ptr: None,
            };

            // Discover resources dynamically
            let (_, crtcs, conns) = device.resource_handles()?;
            
            let mut active_conn = None;
            for &c_handle in &conns {
                if let Ok(info) = device.get_connector(c_handle) {
                    if info.connected {
                        active_conn = Some(c_handle);
                        break;
                    }
                }
            }

            let conn = active_conn.or_else(|| {
                let first: Option<&control::ConnectorHandle> = conns.first();
                first.copied()
            }).ok_or(DisplayError::OpenFailed)?;
            
            // Find an associated CRTC. Mapping follows VirtIO 1000/2000 convention.
            let crtc = if conn.0 >= 1000 && conn.0 < 1016 {
                control::CrtcHandle(2000 + (conn.0 - 1000))
            } else if conn.0 == 6000 {
                control::CrtcHandle(7000)
            } else {
                let first: Option<&control::CrtcHandle> = crtcs.first();
                first.copied().ok_or(DisplayError::OpenFailed)?
            };

            device.connector = conn;
            device.crtc = crtc;
            
            Ok(device)
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            Err(DisplayError::NotAvailable)
        }
    }

    /// Create a framebuffer mapped in user memory.
    pub fn create_framebuffer(&self) -> Result<FramebufferDesc, DisplayError> {
        #[cfg(target_vendor = "eclipse")]
        {
            let db = self.create_dumb_buffer(self.caps.width, self.caps.height, 32)?;
            let fb_id = self.add_framebuffer(db.handle, db.width, db.height, db.pitch)?;
            let addr = self.map_buffer(db.handle, db.size)? as usize;

            let invalid = addr == 0
                || addr == usize::MAX
                || (addr & 0xffff_ffff_0000_0000) == 0xffff_ffff_0000_0000;
            if invalid {
                return Err(DisplayError::InvalidMapping);
            }

            Ok(FramebufferDesc {
                fb_id,
                handle: db.handle,
                addr,
                width: self.caps.width,
                height: self.caps.height,
                pitch: db.pitch,
            })
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            Err(DisplayError::NotAvailable)
        }
    }
}

/// Managed framebuffer that releases resources when dropped.
pub struct ManagedFramebuffer<'a> {
    pub desc: FramebufferDesc,
    pub device: &'a DisplayDevice,
}

impl<'a> Drop for ManagedFramebuffer<'a> {
    fn drop(&mut self) {
        let _ = self.device.destroy_framebuffer(self.desc.fb_id);
        let _ = self.device.gem_close(self.desc.handle);
    }
}
