//! Display/DRM/KMS abstraction for the Eclipse compositor.
//!
//! Implementa una capa de acceso a DRM usando IOCTLs compatibles con Linux,
//! a través del esquema `drm:control` del kernel de Eclipse.

#![allow(dead_code)]

#[cfg(target_vendor = "eclipse")]
use eclipse_syscall as syscall;

use alloc::vec::Vec;

pub mod buffer {
    /// Handle a un buffer (GEM handle).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct Handle(pub u32);
    
    /// Información sobre un dumb buffer.
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
        /// Handle a un framebuffer.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub struct Handle(pub u32);
    }
    
    use super::buffer;
    
    /// Información sobre un framebuffer.
    #[derive(Debug, Clone, Copy)]
    pub struct FramebufferInfo {
        pub handle: framebuffer::Handle,
        pub gem_handle: buffer::Handle,
        pub width: u32,
        pub height: u32,
        pub pitch: u32,
    }

    #[derive(Debug, Clone, Copy)]
    pub struct ConnectorHandle(pub u32);

    #[derive(Debug, Clone, Copy)]
    pub struct ConnectorInfo {
        pub handle: ConnectorHandle,
        pub connected: bool,
        pub width_mm: u32,
        pub height_mm: u32,
    }

    #[derive(Debug, Clone, Copy)]
    pub struct CrtcHandle(pub u32);

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct PlaneHandle(pub u32);

    #[derive(Debug, Clone, Copy)]
    pub struct PlaneInfo {
        pub handle: PlaneHandle,
        pub crtc_id: CrtcHandle,
        pub fb_id: framebuffer::Handle,
        pub possible_crtcs: u32,
        pub plane_type: u32,
    }
}

// Constantes de IOCTL DRM (específicas de Eclipse, compatibles con Linux)
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

/// Trait común para dispositivos tipo DRM.
pub trait Device {
    type Error;
    
    /// Obtiene las capacidades del dispositivo de pantalla.
    fn get_caps(&self) -> Result<DisplayCaps, Self::Error>;
}

/// Trait para operaciones de control de pantalla (similar a KMS).
pub trait ControlDevice: Device {
    /// Crea un dumb buffer.
    fn create_dumb_buffer(&self, width: u32, height: u32, bpp: u32) -> Result<buffer::DumbBuffer, Self::Error>;
    
    /// Mapea un handle de buffer al espacio de direcciones del proceso.
    fn map_buffer(&self, handle: buffer::Handle, size: usize) -> Result<*mut u8, Self::Error>;
    
    /// Añade un framebuffer usando un handle de buffer.
    fn add_framebuffer(&self, handle: buffer::Handle, width: u32, height: u32, pitch: u32) -> Result<control::framebuffer::Handle, Self::Error>;
    
    /// Realiza un page flip hacia un framebuffer concreto.
    fn page_flip(&self, fb: control::framebuffer::Handle) -> Result<(), Self::Error>;

    /// Espera al vblank de un CRTC.
    fn wait_vblank(&self, crtc: control::CrtcHandle) -> Result<(), Self::Error>;

    /// Mueve el cursor hardware.
    fn set_cursor(&self, crtc: control::CrtcHandle, x: i32, y: i32, handle: buffer::Handle) -> Result<(), Self::Error>;

    /// Cierra un handle GEM.
    fn gem_close(&self, handle: buffer::Handle) -> Result<(), Self::Error>;

    /// Destruye un framebuffer.
    fn destroy_framebuffer(&self, fb: control::framebuffer::Handle) -> Result<(), Self::Error>;

    /// Obtiene recursos de pantalla.
    fn resource_handles(&self) -> Result<(Vec<control::framebuffer::Handle>, Vec<control::CrtcHandle>, Vec<control::ConnectorHandle>), Self::Error>;

    /// Obtiene recursos de planos.
    fn plane_resources(&self) -> Result<Vec<control::PlaneHandle>, Self::Error>;

    /// Obtiene información de un plano.
    fn get_plane(&self, plane: control::PlaneHandle) -> Result<control::PlaneInfo, Self::Error>;

    /// Obtiene información de un conector.
    fn get_connector(&self, connector: control::ConnectorHandle) -> Result<control::ConnectorInfo, Self::Error>;

    /// Configura un plano.
    fn set_plane(
        &self,
        plane: control::PlaneHandle,
        crtc: control::CrtcHandle,
        fb: control::framebuffer::Handle,
        crtc_x: i32, crtc_y: i32, crtc_w: u32, crtc_h: u32,
        src_x: u32, src_y: u32, src_w: u32, src_h: u32,
    ) -> Result<(), Self::Error>;
}

/// Capacidades reportadas por el dispositivo de pantalla.
#[derive(Debug, Clone, Copy, Default)]
pub struct DisplayCaps {
    pub width: u32,
    pub height: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub pitch: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct FramebufferDesc {
    pub fb_id: control::framebuffer::Handle,
    pub handle: buffer::Handle,
    pub addr: usize,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
}

#[derive(Debug)]
pub struct DisplayDevice {
    pub fd: usize,
    pub caps: DisplayCaps,
    pub crtc: control::CrtcHandle,
    pub connector: control::ConnectorHandle,
}

#[derive(Debug)]
pub enum DisplayError {
    #[cfg(target_vendor = "eclipse")]
    Syscall(syscall::Error),
    InvalidMapping,
    OpenFailed,
}

#[cfg(target_vendor = "eclipse")]
impl From<syscall::Error> for DisplayError {
    fn from(e: syscall::Error) -> Self {
        DisplayError::Syscall(e)
    }
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
            unimplemented!()
        }
    }

    fn map_buffer(&self, handle: buffer::Handle, size: usize) -> Result<*mut u8, Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
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
            unimplemented!()
        }
    }

    fn add_framebuffer(&self, handle: buffer::Handle, width: u32, height: u32, pitch: u32) -> Result<control::framebuffer::Handle, Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
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
            unimplemented!()
        }
    }

    fn page_flip(&self, fb: control::framebuffer::Handle) -> Result<(), Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            #[repr(C)]
            struct DrmModeCrtcPageFlip {
                crtc_id: u32, fb_id: u32, flags: u32, reserved: u32, user_data: u64,
            }
            let args = DrmModeCrtcPageFlip {
                crtc_id: 100, // CRTC fijo por ahora
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
            unimplemented!()
        }
    }

    fn wait_vblank(&self, crtc: control::CrtcHandle) -> Result<(), Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
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
            syscall::ioctl(self.fd, DRM_IOCTL_WAIT_VBLANK as usize, &mut args as *mut _ as usize)?;
            Ok(())
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = crtc;
            unimplemented!()
        }
    }

    fn set_cursor(&self, crtc: control::CrtcHandle, x: i32, y: i32, handle: buffer::Handle) -> Result<(), Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            #[repr(C)]
            struct DrmModeCursor {
                flags: u32, crtc_id: u32, x: i32, y: i32, width: u32, height: u32, handle: u32,
            }
            let args = DrmModeCursor {
                flags: DRM_CURSOR_MOVE,
                crtc_id: crtc.0,
                x,
                y,
                width: 0, height: 0,
                handle: handle.0,
            };
            syscall::ioctl(self.fd, DRM_IOCTL_MODE_CURSOR as usize, &args as *const _ as usize)?;
            Ok(())
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = (crtc, x, y, handle);
            unimplemented!()
        }
    }

    fn gem_close(&self, handle: buffer::Handle) -> Result<(), Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            #[repr(C)]
            struct DrmGemClose { handle: u32, pad: u32 }
            let args = DrmGemClose { handle: handle.0, pad: 0 };
            syscall::ioctl(self.fd, DRM_IOCTL_GEM_CLOSE as usize, &args as *const _ as usize)?;
            Ok(())
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = handle;
            unimplemented!()
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
            unimplemented!()
        }
    }

    fn resource_handles(&self) -> Result<(Vec<control::framebuffer::Handle>, Vec<control::CrtcHandle>, Vec<control::ConnectorHandle>), Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            #[repr(C)]
            struct DrmModeCardRes {
                fb_id_ptr: u64, crtc_id_ptr: u64, connector_id_ptr: u64, encoder_id_ptr: u64,
                count_fbs: u32, count_crtcs: u32, count_connectors: u32, count_encoders: u32,
                min_width: u32, max_width: u32, min_height: u32, max_height: u32,
            }
            let mut args = DrmModeCardRes {
                fb_id_ptr: 0, crtc_id_ptr: 0, connector_id_ptr: 0, encoder_id_ptr: 0,
                count_fbs: 0, count_crtcs: 0, count_connectors: 0, count_encoders: 0,
                min_width: 0, max_width: 0, min_height: 0, max_height: 0,
            };
            syscall::ioctl(self.fd, DRM_IOCTL_MODE_GETRESOURCES as usize, &mut args as *mut _ as usize)?;
            
            let mut fbs = Vec::with_capacity(args.count_fbs as usize);
            let mut crtcs = Vec::with_capacity(args.count_crtcs as usize);
            let mut conns = Vec::with_capacity(args.count_connectors as usize);
            
            unsafe {
                fbs.set_len(args.count_fbs as usize);
                crtcs.set_len(args.count_crtcs as usize);
                conns.set_len(args.count_connectors as usize);

                args.fb_id_ptr = fbs.as_ptr() as u64;
                args.crtc_id_ptr = crtcs.as_ptr() as u64;
                args.connector_id_ptr = conns.as_ptr() as u64;

                syscall::ioctl(self.fd, DRM_IOCTL_MODE_GETRESOURCES as usize, &mut args as *mut _ as usize)?;
            }

            Ok((
                fbs.into_iter().map(control::framebuffer::Handle).collect(),
                crtcs.into_iter().map(control::CrtcHandle).collect(),
                conns.into_iter().map(control::ConnectorHandle).collect(),
            ))
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            unimplemented!()
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
            syscall::ioctl(self.fd, DRM_IOCTL_MODE_GETPLANERESOURCES as usize, &mut args as *mut _ as usize)?;
            
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
            unimplemented!()
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
                plane_type: 0, // Universal planes requerirían más lógica de propiedades
            })
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = plane;
            unimplemented!()
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
            unimplemented!()
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
            unimplemented!()
        }
    }
}

impl DisplayDevice {
    /// Abre el dispositivo DRM y descubre recursos.
    #[cfg(target_vendor = "eclipse")]
    pub fn open() -> Result<Self, DisplayError> {
        let fd = syscall::open("drm:control", 0).map_err(|_| DisplayError::OpenFailed)?;
        
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
        };

        // Descubrir recursos dinámicamente
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

        let conn = active_conn.or_else(|| conns.first().copied()).ok_or(DisplayError::OpenFailed)?;
        
        // Buscar un CRTC asociado. Para VirtIO seguimos el mapeo 1000/2000.
        let crtc = if conn.0 >= 1000 && conn.0 < 1016 {
            control::CrtcHandle(2000 + (conn.0 - 1000))
        } else if conn.0 == 6000 {
            control::CrtcHandle(7000)
        } else {
            crtcs.first().copied().ok_or(DisplayError::OpenFailed)?
        };

        device.connector = conn;
        device.crtc = crtc;
        
        Ok(device)
    }
}

/// Framebuffer gestionado que libera recursos al salir de scope.
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

impl DisplayDevice {
    /// Crea un framebuffer mapeado en memoria de usuario.
    #[cfg(target_vendor = "eclipse")]
    pub fn create_framebuffer(&self) -> Result<FramebufferDesc, DisplayError> {
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
            pitch: self.caps.pitch,
        })
    }
}

<<<<<<< HEAD
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
=======
#[cfg(target_vendor = "eclipse")]
use eclipse_syscall as syscall;

use alloc::vec::Vec;

pub mod buffer {
    /// A handle to a buffer (GEM handle).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

    #[derive(Debug, Clone, Copy)]
    pub struct ConnectorHandle(pub u32);

    #[derive(Debug, Clone, Copy)]
    pub struct ConnectorInfo {
        pub handle: ConnectorHandle,
        pub connected: bool,
        pub width_mm: u32,
        pub height_mm: u32,
    }

    #[derive(Debug, Clone, Copy)]
    pub struct CrtcHandle(pub u32);

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct PlaneHandle(pub u32);

    #[derive(Debug, Clone, Copy)]
    pub struct PlaneInfo {
        pub handle: PlaneHandle,
        pub crtc_id: CrtcHandle,
        pub fb_id: framebuffer::Handle,
        pub possible_crtcs: u32,
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

    /// Get display resources.
    fn resource_handles(&self) -> Result<(Vec<control::framebuffer::Handle>, Vec<control::CrtcHandle>, Vec<control::ConnectorHandle>), Self::Error>;

    /// Get plane resources.
    fn plane_resources(&self) -> Result<Vec<control::PlaneHandle>, Self::Error>;

    /// Get information about a plane.
    fn get_plane(&self, plane: control::PlaneHandle) -> Result<control::PlaneInfo, Self::Error>;

    /// Get information about a connector.
    fn get_connector(&self, connector: control::ConnectorHandle) -> Result<control::ConnectorInfo, Self::Error>;

    /// Set plane properties.
    fn set_plane(
        &self,
        plane: control::PlaneHandle,
        crtc: control::CrtcHandle,
        fb: control::framebuffer::Handle,
        crtc_x: i32, crtc_y: i32, crtc_w: u32, crtc_h: u32,
        src_x: u32, src_y: u32, src_w: u32, src_h: u32,
    ) -> Result<(), Self::Error>;
}

#[derive(Debug, Clone, Copy)]
>>>>>>> 9985d476 (añadidos algunos archivos faltantes.)
pub struct DisplayCaps {
    pub width: u32,
    pub height: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub pitch: u32,
}

<<<<<<< HEAD
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
=======
#[derive(Debug, Clone, Copy)]
pub struct FramebufferDesc {
    pub fb_id: control::framebuffer::Handle,
    pub handle: buffer::Handle,
    pub addr: usize,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
}

#[derive(Debug)]
pub struct DisplayDevice {
>>>>>>> 9985d476 (añadidos algunos archivos faltantes.)
    pub fd: usize,
    pub caps: DisplayCaps,
    pub crtc: control::CrtcHandle,
    pub connector: control::ConnectorHandle,
}

<<<<<<< HEAD
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
=======
#[derive(Debug)]
pub enum DisplayError {
    #[cfg(target_vendor = "eclipse")]
    Syscall(syscall::Error),
    InvalidMapping,
    OpenFailed,
}

#[cfg(target_vendor = "eclipse")]
impl From<syscall::Error> for DisplayError {
    fn from(e: syscall::Error) -> Self {
        DisplayError::Syscall(e)
    }
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
        unimplemented!()
    }

    fn map_buffer(&self, handle: buffer::Handle, size: usize) -> Result<*mut u8, Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
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
        unimplemented!()
    }

    fn add_framebuffer(&self, handle: buffer::Handle, width: u32, height: u32, pitch: u32) -> Result<control::framebuffer::Handle, Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
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
        unimplemented!()
    }

    fn page_flip(&self, fb: control::framebuffer::Handle) -> Result<(), Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            #[repr(C)]
            struct DrmModeCrtcPageFlip {
                crtc_id: u32, fb_id: u32, flags: u32, reserved: u32, user_data: u64,
            }
            let args = DrmModeCrtcPageFlip {
                crtc_id: 100, // Fixed CRTC for now
                fb_id: fb.0,
                flags: 0,
                reserved: 0,
                user_data: 0,
            };
            syscall::ioctl(self.fd, DRM_IOCTL_MODE_PAGE_FLIP as usize, &args as *const _ as usize)?;
            Ok(())
        }
        #[cfg(not(target_vendor = "eclipse"))]
        unimplemented!()
    }

    fn wait_vblank(&self, crtc: control::CrtcHandle) -> Result<(), Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
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
            syscall::ioctl(self.fd, DRM_IOCTL_WAIT_VBLANK as usize, &mut args as *mut _ as usize)?;
            Ok(())
        }
        #[cfg(not(target_vendor = "eclipse"))]
        unimplemented!()
    }

    fn set_cursor(&self, crtc: control::CrtcHandle, x: i32, y: i32, handle: buffer::Handle) -> Result<(), Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            #[repr(C)]
            struct DrmModeCursor {
                flags: u32, crtc_id: u32, x: i32, y: i32, width: u32, height: u32, handle: u32,
            }
            const DRM_CURSOR_MOVE: u32 = 0x02;
            let args = DrmModeCursor {
                flags: DRM_CURSOR_MOVE,
                crtc_id: crtc.0,
                x,
                y,
                width: 0, height: 0,
                handle: handle.0,
            };
            syscall::ioctl(self.fd, DRM_IOCTL_MODE_CURSOR as usize, &args as *const _ as usize)?;
            Ok(())
        }
        #[cfg(not(target_vendor = "eclipse"))]
        unimplemented!()
    }

    fn gem_close(&self, handle: buffer::Handle) -> Result<(), Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            #[repr(C)]
            struct DrmGemClose { handle: u32, pad: u32 }
            let args = DrmGemClose { handle: handle.0, pad: 0 };
            syscall::ioctl(self.fd, DRM_IOCTL_GEM_CLOSE as usize, &args as *const _ as usize)?;
            Ok(())
        }
        #[cfg(not(target_vendor = "eclipse"))]
        unimplemented!()
    }

    fn destroy_framebuffer(&self, fb: control::framebuffer::Handle) -> Result<(), Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            syscall::ioctl(self.fd, DRM_IOCTL_MODE_DESTROYFB as usize, &fb.0 as *const _ as usize)?;
            Ok(())
        }
        #[cfg(not(target_vendor = "eclipse"))]
        unimplemented!()
    }

    fn resource_handles(&self) -> Result<(Vec<control::framebuffer::Handle>, Vec<control::CrtcHandle>, Vec<control::ConnectorHandle>), Self::Error> {
        #[cfg(target_vendor = "eclipse")]
        {
            #[repr(C)]
            struct DrmModeCardRes {
                fb_id_ptr: u64, crtc_id_ptr: u64, connector_id_ptr: u64, encoder_id_ptr: u64,
                count_fbs: u32, count_crtcs: u32, count_connectors: u32, count_encoders: u32,
                min_width: u32, max_width: u32, min_height: u32, max_height: u32,
            }
            let mut args = DrmModeCardRes {
                fb_id_ptr: 0, crtc_id_ptr: 0, connector_id_ptr: 0, encoder_id_ptr: 0,
                count_fbs: 0, count_crtcs: 0, count_connectors: 0, count_encoders: 0,
                min_width: 0, max_width: 0, min_height: 0, max_height: 0,
            };
            syscall::ioctl(self.fd, DRM_IOCTL_MODE_GETRESOURCES as usize, &mut args as *mut _ as usize)?;
            
            let mut fbs = Vec::with_capacity(args.count_fbs as usize);
            let mut crtcs = Vec::with_capacity(args.count_crtcs as usize);
            let mut conns = Vec::with_capacity(args.count_connectors as usize);
            
            unsafe {
                fbs.set_len(args.count_fbs as usize);
                crtcs.set_len(args.count_crtcs as usize);
                conns.set_len(args.count_connectors as usize);

                args.fb_id_ptr = fbs.as_ptr() as u64;
                args.crtc_id_ptr = crtcs.as_ptr() as u64;
                args.connector_id_ptr = conns.as_ptr() as u64;

                syscall::ioctl(self.fd, DRM_IOCTL_MODE_GETRESOURCES as usize, &mut args as *mut _ as usize)?;
            }

            Ok((
                fbs.into_iter().map(control::framebuffer::Handle).collect(),
                crtcs.into_iter().map(control::CrtcHandle).collect(),
                conns.into_iter().map(control::ConnectorHandle).collect(),
            ))
        }
        #[cfg(not(target_vendor = "eclipse"))]
        unimplemented!()
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
            syscall::ioctl(self.fd, DRM_IOCTL_MODE_GETPLANERESOURCES as usize, &mut args as *mut _ as usize)?;
            
            let mut planes = Vec::with_capacity(args.count_planes as usize);
            unsafe {
                planes.set_len(args.count_planes as usize);
                args.plane_id_ptr = planes.as_ptr() as u64;
                syscall::ioctl(self.fd, DRM_IOCTL_MODE_GETPLANERESOURCES as usize, &mut args as *mut _ as usize)?;
            }
            Ok(planes.into_iter().map(control::PlaneHandle).collect())
        }
        #[cfg(not(target_vendor = "eclipse"))]
        unimplemented!()
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
                plane_type: 0, // Universal planes would require more GETPROP logic
            })
        }
        #[cfg(not(target_vendor = "eclipse"))]
        unimplemented!()
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
        unimplemented!()
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
        unimplemented!()
    }
}

impl DisplayDevice {
    /// Abre el dispositivo de DRM.
    #[cfg(target_vendor = "eclipse")]
    pub fn open() -> Result<Self, DisplayError> {
        let fd = syscall::open("drm:control", 0).map_err(|_| DisplayError::OpenFailed)?;
        
        let mut caps_val = DisplayCaps { width: 0, height: 0, max_width: 0, max_height: 0, pitch: 0 };
        // Por compatibilidad temporal, seguimos usando la syscall directa de caps si falla el IOCTL
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

        let conn = active_conn.or_else(|| conns.first().copied()).ok_or(DisplayError::OpenFailed)?;
        
        // Find a matching CRTC. For VirtIO, we follow the 1000/2000 mapping.
        let crtc = if conn.0 >= 1000 && conn.0 < 1016 {
            control::CrtcHandle(2000 + (conn.0 - 1000))
        } else if conn.0 == 6000 {
            control::CrtcHandle(7000)
        } else {
            crtcs.first().copied().ok_or(DisplayError::OpenFailed)?
        };

        device.connector = conn;
        device.crtc = crtc;
        
        Ok(device)
    }
}

/// Implementamos Drop para FramebufferDesc opcionalmente si queremos que se autodestruya.
/// Sin embargo, FramebufferDesc es un struct de datos plano.
/// Para una gestión real, deberíamos usar un envoltorio que tenga el FD.
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

impl DisplayDevice {
    /// Crea un framebuffer mapeado en memoria de usuario (Helper legado mejorado).
    #[cfg(target_vendor = "eclipse")]
    pub fn create_framebuffer(&self) -> Result<FramebufferDesc, DisplayError> {
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
            pitch: self.caps.pitch,
        })
    }
}

>>>>>>> 9985d476 (añadidos algunos archivos faltantes.)
