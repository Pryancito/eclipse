//! DRM (Direct Rendering Manager) Scheme for zCore
//!
//! Exposes the DRM subsystem to userspace via IOCTLs and memory mapping.

use alloc::sync::Arc;
use core::any::Any;

use rcore_fs::vfs::*;
use zircon_object::vm::{pages, VmObject};

use super::drm;

/// DRM Device INode
pub struct DrmDev {
    inode_id: usize,
    minor: u32,
}

impl DrmDev {
    pub fn new(minor: u32) -> Self {
        use rcore_fs_devfs::DevFS;
        Self {
            inode_id: DevFS::new_inode_id(),
            minor,
        }
    }

    /// Returns the [`VmObject`] representing the file with given `offset` and `len`.
    pub fn get_vmo(&self, offset: usize, len: usize) -> Result<Arc<VmObject>> {
        let handle_id = offset as u32;
        if let Some(handle) = drm::get_handle(handle_id) {
            let len = len.min(handle.size);
            Ok(VmObject::new_physical(
                handle.phys_addr as usize,
                pages(len),
            ))
        } else {
            Err(FsError::InvalidParam)
        }
    }
}

// DRM IOCTL numbers (Linux x86_64)
const DRM_IOCTL_VERSION: u32 = 0xC0406400;
const DRM_IOCTL_GET_UNIQUE: u32 = 0xC0106401;
const DRM_IOCTL_GET_CAP: u32 = 0xC010640C;
const DRM_IOCTL_SET_CLIENT_CAP: u32 = 0x4010640D;
const DRM_IOCTL_GEM_CLOSE: u32 = 0x40086409;

const DRM_IOCTL_MODE_GETRESOURCES: u32 = 0xC04064A0;
const DRM_IOCTL_MODE_GETCRTC: u32 = 0xC06864A1;
const DRM_IOCTL_MODE_GETCONNECTOR: u32 = 0xC05064A7;

const DRM_IOCTL_MODE_CREATE_DUMB: u32 = 0xC02064B2;
const DRM_IOCTL_MODE_MAP_DUMB: u32 = 0xC01064B3;
const DRM_IOCTL_MODE_ADDFB: u32 = 0xC01C64AE;
const DRM_IOCTL_MODE_PAGE_FLIP: u32 = 0xC01864B0;

const DRM_IOCTL_MODE_GETPLANERESOURCES: u32 = 0xC01064B5;
const DRM_IOCTL_MODE_GETPLANE: u32 = 0xC02064B6;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmVersion {
    version_major: i32,
    version_minor: i32,
    version_patchlevel: i32,
    name_len: usize,
    name: *mut u8,
    date_len: usize,
    date: *mut u8,
    desc_len: usize,
    desc: *mut u8,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmUnique {
    unique_len: usize,
    unique: *mut u8,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmGetCap {
    capability: u64,
    value: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
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

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeCreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeFbCmd {
    fb_id: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    depth: u32,
    handle: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeMapDumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeGetConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeGetCrtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: [u8; 68],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeGetPlaneRes {
    plane_id_ptr: u64,
    count_planes: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeGetPlane {
    plane_id: u32,
    crtc_id: u32,
    fb_id: u32,
    possible_crtcs: u32,
    gamma_size: u32,
    count_format_types: u32,
    format_type_ptr: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeCrtcPageFlip {
    crtc_id: u32,
    fb_id: u32,
    flags: u32,
    reserved: u32,
    user_data: u64,
}

impl INode for DrmDev {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Ok(_buf.len())
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: true,
            error: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 1,
            inode: self.inode_id,
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::CharDevice,
            mode: 0o660,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: make_rdev(0xe2, self.minor as usize), // 226 is DRM major
        })
    }

    #[allow(unsafe_code)]
    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        match cmd {
            DRM_IOCTL_VERSION => {
                let v = unsafe { &mut *(data as *mut DrmVersion) };
                v.version_major = 1;
                v.version_minor = 0;
                v.version_patchlevel = 0;

                let name = b"zcore\0";
                let date = b"20260503\0";
                let desc = b"zCore DRM Driver\0";

                unsafe {
                    if v.name_len > 0 && !v.name.is_null() {
                        let len = core::cmp::min(v.name_len, name.len());
                        core::ptr::copy_nonoverlapping(name.as_ptr(), v.name, len);
                    }
                    if v.date_len > 0 && !v.date.is_null() {
                        let len = core::cmp::min(v.date_len, date.len());
                        core::ptr::copy_nonoverlapping(date.as_ptr(), v.date, len);
                    }
                    if v.desc_len > 0 && !v.desc.is_null() {
                        let len = core::cmp::min(v.desc_len, desc.len());
                        core::ptr::copy_nonoverlapping(desc.as_ptr(), v.desc, len);
                    }
                }
                v.name_len = name.len();
                v.date_len = date.len();
                v.desc_len = desc.len();
                Ok(0)
            }
            DRM_IOCTL_GET_UNIQUE => {
                let u = unsafe { &mut *(data as *mut DrmUnique) };
                let name = b"zcore-gpu\0";
                unsafe {
                    if u.unique_len > 0 && !u.unique.is_null() {
                        let len = core::cmp::min(u.unique_len, name.len());
                        core::ptr::copy_nonoverlapping(name.as_ptr(), u.unique, len);
                    }
                }
                u.unique_len = name.len();
                Ok(0)
            }
            DRM_IOCTL_GET_CAP => {
                let cap = unsafe { &mut *(data as *mut DrmGetCap) };
                match cap.capability {
                    0x1 => cap.value = 1,  // DRM_CAP_DUMB_BUFFER
                    0x5 => cap.value = 3,  // DRM_CAP_PRIME: IMPORT|EXPORT
                    0x6 => cap.value = 1,  // DRM_CAP_TIMESTAMP_MONOTONIC
                    0x8 => cap.value = 64, // DRM_CAP_CURSOR_WIDTH
                    0x9 => cap.value = 64, // DRM_CAP_CURSOR_HEIGHT
                    0x10 => cap.value = 1, // DRM_CAP_ADDFB2_MODIFIERS
                    _ => cap.value = 0,
                }
                Ok(0)
            }
            DRM_IOCTL_SET_CLIENT_CAP => Ok(0),
            DRM_IOCTL_MODE_CREATE_DUMB => {
                let info = unsafe { &mut *(data as *mut DrmModeCreateDumb) };
                let bpp = info.bpp.max(32);
                let pitch = (info.width * bpp / 8 + 63) & !63;
                let size = (pitch * info.height) as usize;

                if let Some(handle) = drm::alloc_buffer(size) {
                    info.handle = handle.id;
                    info.pitch = pitch;
                    info.size = size as u64;
                    Ok(0)
                } else {
                    Err(FsError::NoDeviceSpace)
                }
            }
            DRM_IOCTL_MODE_ADDFB => {
                let cmd = unsafe { &mut *(data as *mut DrmModeFbCmd) };
                if let Some(fb_id) = drm::create_fb(cmd.handle, cmd.width, cmd.height, cmd.pitch) {
                    cmd.fb_id = fb_id;
                    Ok(0)
                } else {
                    Err(FsError::DeviceError)
                }
            }
            DRM_IOCTL_MODE_MAP_DUMB => {
                let map = unsafe { &mut *(data as *mut DrmModeMapDumb) };
                map.offset = map.handle as u64;
                Ok(0)
            }
            DRM_IOCTL_MODE_PAGE_FLIP => {
                let flip = unsafe { *(data as *const DrmModeCrtcPageFlip) };
                if drm::page_flip(flip.fb_id) {
                    Ok(0)
                } else {
                    Err(FsError::DeviceError)
                }
            }
            DRM_IOCTL_GEM_CLOSE => {
                let handle = unsafe { *(data as *const u32) };
                if drm::gem_close(handle) {
                    Ok(0)
                } else {
                    Err(FsError::InvalidParam)
                }
            }
            DRM_IOCTL_MODE_GETRESOURCES => {
                let res = unsafe { &mut *(data as *mut DrmModeCardRes) };
                let (fbs, crtcs, connectors) = drm::get_resources();

                if res.fb_id_ptr != 0 && res.count_fbs >= fbs.len() as u32 {
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            fbs.as_ptr(),
                            res.fb_id_ptr as *mut u32,
                            fbs.len(),
                        );
                    }
                }
                if res.crtc_id_ptr != 0 && res.count_crtcs >= crtcs.len() as u32 {
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            crtcs.as_ptr(),
                            res.crtc_id_ptr as *mut u32,
                            crtcs.len(),
                        );
                    }
                }
                if res.connector_id_ptr != 0 && res.count_connectors >= connectors.len() as u32 {
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            connectors.as_ptr(),
                            res.connector_id_ptr as *mut u32,
                            connectors.len(),
                        );
                    }
                }

                res.count_fbs = fbs.len() as u32;
                res.count_crtcs = crtcs.len() as u32;
                res.count_connectors = connectors.len() as u32;
                res.count_encoders = 0;

                if let Some(caps) = drm::get_caps() {
                    res.max_width = caps.max_width;
                    res.max_height = caps.max_height;
                }
                Ok(0)
            }
            DRM_IOCTL_MODE_GETCONNECTOR => {
                let conn_res = unsafe { &mut *(data as *mut DrmModeGetConnector) };
                if let Some(conn) = drm::get_connector(conn_res.connector_id) {
                    conn_res.connection = if conn.connected { 1 } else { 2 };
                    conn_res.mm_width = conn.mm_width;
                    conn_res.mm_height = conn.mm_height;
                    conn_res.connector_type = 11; // DRM_MODE_CONNECTOR_VIRTUAL
                    conn_res.connector_type_id = 1;
                    conn_res.count_modes = 0;
                    conn_res.count_encoders = 0;
                    Ok(0)
                } else {
                    Err(FsError::InvalidParam)
                }
            }
            DRM_IOCTL_MODE_GETCRTC => {
                let crtc_res = unsafe { &mut *(data as *mut DrmModeGetCrtc) };
                if let Some(crtc) = drm::get_crtc(crtc_res.crtc_id) {
                    crtc_res.fb_id = crtc.fb_id;
                    crtc_res.x = crtc.x;
                    crtc_res.y = crtc.y;
                    Ok(0)
                } else {
                    Err(FsError::InvalidParam)
                }
            }
            DRM_IOCTL_MODE_GETPLANERESOURCES => {
                let res = unsafe { &mut *(data as *mut DrmModeGetPlaneRes) };
                let planes = drm::get_planes();
                if res.plane_id_ptr != 0 && res.count_planes >= planes.len() as u32 {
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            planes.as_ptr(),
                            res.plane_id_ptr as *mut u32,
                            planes.len(),
                        );
                    }
                }
                res.count_planes = planes.len() as u32;
                Ok(0)
            }
            DRM_IOCTL_MODE_GETPLANE => {
                let res = unsafe { &mut *(data as *mut DrmModeGetPlane) };
                if let Some(plane) = drm::get_plane(res.plane_id) {
                    res.crtc_id = plane.crtc_id;
                    res.fb_id = plane.fb_id;
                    res.possible_crtcs = plane.possible_crtcs;
                    res.count_format_types = 0;
                    Ok(0)
                } else {
                    Err(FsError::InvalidParam)
                }
            }
            _ => {
                if let Some(driver) = drm::get_primary_driver() {
                    driver.ioctl(cmd, data).map_err(|e| match e {
                        38 => FsError::NotSupported, // ENOSYS
                        _ => FsError::DeviceError,
                    })
                } else {
                    Err(FsError::NotSupported)
                }
            }
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}
