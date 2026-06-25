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
const DRM_IOCTL_GET_MAGIC: u32 = 0xC0046402;
const DRM_IOCTL_AUTH_MAGIC: u32 = 0x40046411;
const DRM_IOCTL_GET_CAP: u32 = 0xC010640C;
const DRM_IOCTL_SET_CLIENT_CAP: u32 = 0x4010640D;
const DRM_IOCTL_GEM_CLOSE: u32 = 0x40086409;
const DRM_IOCTL_SET_MASTER: u32 = 0x0000641E;
const DRM_IOCTL_DROP_MASTER: u32 = 0x0000641F;

const DRM_IOCTL_MODE_GETRESOURCES: u32 = 0xC04064A0;
const DRM_IOCTL_MODE_GETCRTC: u32 = 0xC06864A1;
const DRM_IOCTL_MODE_SETCRTC: u32 = 0xC06864A2;
const DRM_IOCTL_MODE_GETENCODER: u32 = 0xC01464A6;
const DRM_IOCTL_MODE_GETCONNECTOR: u32 = 0xC05064A7;

const DRM_IOCTL_MODE_CREATE_DUMB: u32 = 0xC02064B2;
const DRM_IOCTL_MODE_MAP_DUMB: u32 = 0xC01064B3;
const DRM_IOCTL_MODE_DESTROY_DUMB: u32 = 0xC00464B4;
const DRM_IOCTL_MODE_ADDFB: u32 = 0xC01C64AE;
const DRM_IOCTL_MODE_ADDFB2: u32 = 0xC06864B8;
const DRM_IOCTL_MODE_RMFB: u32 = 0xC00464AF;
const DRM_IOCTL_MODE_PAGE_FLIP: u32 = 0xC01864B0;

const DRM_IOCTL_MODE_GETPLANERESOURCES: u32 = 0xC01064B5;
const DRM_IOCTL_MODE_GETPLANE: u32 = 0xC02064B6;
const DRM_IOCTL_MODE_SETPLANE: u32 = 0xC03064B7;
const DRM_IOCTL_MODE_OBJ_GETPROPERTIES: u32 = 0xC02064B9;
const DRM_IOCTL_MODE_OBJ_SETPROPERTY: u32 = 0xC01864BA;
const DRM_IOCTL_MODE_GETPROPERTY: u32 = 0xC04064AA;

// Core (non-MODE) vblank wait.
const DRM_IOCTL_WAIT_VBLANK: u32 = 0xC018643A;
// Query an existing framebuffer object.
const DRM_IOCTL_MODE_GETFB: u32 = 0xC01C64AD;
const DRM_IOCTL_MODE_GETFB2: u32 = 0xC06864CE;
// Flush framebuffer damage to the display.
const DRM_IOCTL_MODE_DIRTYFB: u32 = 0xC01864B1;

// WAIT_VBLANK request type flags (`<drm/drm.h>`).
const _DRM_VBLANK_EVENT: u32 = 0x0400_0000;

// Synthetic property ids (software KMS). Only the plane "type" is mandatory for
// wlroots' legacy backend to classify the primary plane.
const PROP_TYPE: u32 = 10;

// DRM client capabilities (DRM_IOCTL_SET_CLIENT_CAP).
const DRM_CLIENT_CAP_ATOMIC: u64 = 3;
const DRM_CLIENT_CAP_WRITEBACK_CONNECTORS: u64 = 5;

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
    encoder_id: u32, // current encoder
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
struct DrmModeGetEncoder {
    encoder_id: u32,
    encoder_type: u32,
    crtc_id: u32,
    possible_crtcs: u32,
    possible_clones: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeFbCmd2 {
    fb_id: u32,
    width: u32,
    height: u32,
    pixel_format: u32,
    flags: u32,
    handles: [u32; 4],
    pitches: [u32; 4],
    offsets: [u32; 4],
    modifier: [u64; 4],
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
struct DrmModeObjGetProperties {
    props_ptr: u64,
    prop_values_ptr: u64,
    count_props: u32,
    obj_id: u32,
    obj_type: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeGetProperty {
    values_ptr: u64,
    enum_blob_ptr: u64,
    prop_id: u32,
    flags: u32,
    name: [u8; 32],
    count_values: u32,
    count_enum_blobs: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModePropertyEnum {
    value: u64,
    name: [u8; 32],
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

/// `union drm_wait_vblank` (24 bytes). The request side is `{ type, sequence,
/// signal }`; the reply side reuses the trailing 16 bytes as `{ tval_sec,
/// tval_usec }`. We model the union as one struct and read/write the overlap by
/// field.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmWaitVblank {
    typ: u32,
    sequence: u32,
    /// request: `signal`; reply: `tval_sec`.
    val1: u64,
    /// request: unused; reply: `tval_usec`.
    val2: u64,
}

/// `struct drm_mode_set_plane` (48 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeSetPlane {
    plane_id: u32,
    crtc_id: u32,
    fb_id: u32,
    flags: u32,
    crtc_x: i32,
    crtc_y: i32,
    crtc_w: u32,
    crtc_h: u32,
    // Source values are 16.16 fixed point.
    src_x: u32,
    src_y: u32,
    src_h: u32,
    src_w: u32,
}

/// `struct drm_mode_obj_set_property` (24 bytes after u64 alignment padding).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeObjSetProperty {
    value: u64,
    prop_id: u32,
    obj_id: u32,
    obj_type: u32,
}

/// `struct drm_mode_fb_dirty_cmd` (24 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmModeFbDirtyCmd {
    fb_id: u32,
    flags: u32,
    color: u32,
    num_clips: u32,
    clips_ptr: u64,
}

// Compile-time guards: each DRM ioctl number encodes `sizeof(struct)` in its
// _IOC size field, so a wrong struct layout silently mismatches the ioctl and
// the handler never fires. Assert the sizes that the constants above depend on.
const _: () = {
    use core::mem::size_of;
    assert!(size_of::<DrmModeGetConnector>() == 80); // DRM_IOCTL_MODE_GETCONNECTOR 0x..50..
    assert!(size_of::<DrmModeGetEncoder>() == 20); // DRM_IOCTL_MODE_GETENCODER  0x..14..
    assert!(size_of::<DrmModeFbCmd2>() == 104); // DRM_IOCTL_MODE_ADDFB2      0x..68..
    assert!(size_of::<DrmModeGetCrtc>() == 104); // DRM_IOCTL_MODE_{GET,SET}CRTC 0x..68..
    assert!(size_of::<DrmModeCrtcPageFlip>() == 24); // DRM_IOCTL_MODE_PAGE_FLIP 0x..18..
    assert!(size_of::<DrmModeObjGetProperties>() == 32); // OBJ_GETPROPERTIES 0x..20..
    assert!(size_of::<DrmModeGetProperty>() == 64); // GETPROPERTY        0x..40..
    assert!(size_of::<DrmModePropertyEnum>() == 40);
    assert!(size_of::<DrmModeGetPlane>() == 32); // DRM_IOCTL_MODE_GETPLANE 0x..20..
    assert!(size_of::<DrmWaitVblank>() == 24); // DRM_IOCTL_WAIT_VBLANK   0x..18..
    assert!(size_of::<DrmModeSetPlane>() == 48); // DRM_IOCTL_MODE_SETPLANE 0x..30..
    assert!(size_of::<DrmModeObjSetProperty>() == 24); // OBJ_SETPROPERTY  0x..18..
    assert!(size_of::<DrmModeFbDirtyCmd>() == 24); // DRM_IOCTL_MODE_DIRTYFB 0x..18..
};

/// Build a `struct drm_mode_modeinfo` (68 bytes) for a simple 60 Hz mode at
/// `w`x`h`. Timings are nominal — a software framebuffer never programs real CRT
/// timings — but wlroots needs a populated, "preferred" mode to drive output.
fn make_modeinfo(w: u32, h: u32) -> [u8; 68] {
    let mut m = [0u8; 68];
    let refresh: u32 = 60;
    let clock = ((w as u64 * h as u64 * refresh as u64) / 1000) as u32; // kHz, nominal
    m[0..4].copy_from_slice(&clock.to_ne_bytes());
    let wh = w as u16;
    m[4..6].copy_from_slice(&wh.to_ne_bytes()); // hdisplay
    m[6..8].copy_from_slice(&wh.to_ne_bytes()); // hsync_start
    m[8..10].copy_from_slice(&wh.to_ne_bytes()); // hsync_end
    m[10..12].copy_from_slice(&wh.to_ne_bytes()); // htotal
                                                  // hskew @12..14 = 0
    let hh = h as u16;
    m[14..16].copy_from_slice(&hh.to_ne_bytes()); // vdisplay
    m[16..18].copy_from_slice(&hh.to_ne_bytes()); // vsync_start
    m[18..20].copy_from_slice(&hh.to_ne_bytes()); // vsync_end
    m[20..22].copy_from_slice(&hh.to_ne_bytes()); // vtotal
                                                  // vscan @22..24 = 0
    m[24..28].copy_from_slice(&refresh.to_ne_bytes()); // vrefresh
                                                       // flags @28..32 = 0
    // type @32..36: DRM_MODE_TYPE_DRIVER(0x40) | DRM_MODE_TYPE_PREFERRED(0x08)
    m[32..36].copy_from_slice(&0x48u32.to_ne_bytes());
    // name @36..68 ("WxH")
    let mut name = [0u8; 32];
    let mut i = 0;
    let put = |buf: &mut [u8; 32], i: &mut usize, val: u32| {
        if val == 0 {
            if *i < buf.len() {
                buf[*i] = b'0';
                *i += 1;
            }
            return;
        }
        let mut digits = [0u8; 10];
        let mut n = 0;
        let mut v = val;
        while v > 0 {
            digits[n] = b'0' + (v % 10) as u8;
            v /= 10;
            n += 1;
        }
        while n > 0 && *i < buf.len() {
            n -= 1;
            buf[*i] = digits[n];
            *i += 1;
        }
    };
    put(&mut name, &mut i, w);
    if i < name.len() {
        name[i] = b'x';
        i += 1;
    }
    put(&mut name, &mut i, h);
    m[36..68].copy_from_slice(&name);
    m
}

impl INode for DrmDev {
    fn read_at(&self, _offset: usize, buf: &mut [u8]) -> Result<usize> {
        // Deliver queued DRM events (page-flip completions). When none are
        // pending report `Again` so a non-blocking reader gets EAGAIN and an
        // epoll/poll waiter re-checks on the next tick.
        match drm::read_event(buf) {
            Some(n) => Ok(n),
            None => Err(FsError::Again),
        }
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Ok(_buf.len())
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: drm::has_events(),
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
                let node = if self.minor >= 128 {
                    "renderD128"
                } else {
                    "card0"
                };
                log::error!(
                    "[drm] VERSION — /dev/dri/{} opened by userspace (minor={})",
                    node,
                    self.minor
                );
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
                    0x1 => cap.value = 1, // DRM_CAP_DUMB_BUFFER
                    // DRM_CAP_PRIME: IMPORT|EXPORT. wlroots' check_drm_features
                    // *requires* DRM_PRIME_CAP_IMPORT or the whole DRM backend
                    // fails ("PRIME import not supported") — it is mandatory for
                    // any output (pixman or GL), not just GBM clients. We now
                    // implement PRIME_HANDLE_TO_FD / FD_TO_HANDLE (dma-buf), so
                    // advertise it.
                    0x5 => cap.value = 3,
                    0x6 => cap.value = 1,  // DRM_CAP_TIMESTAMP_MONOTONIC
                    0x8 => cap.value = 64, // DRM_CAP_CURSOR_WIDTH
                    0x9 => cap.value = 64, // DRM_CAP_CURSOR_HEIGHT
                    0x10 => cap.value = 1, // DRM_CAP_ADDFB2_MODIFIERS
                    // DRM_CAP_CRTC_IN_VBLANK_EVENT: our page-flip event carries
                    // the crtc_id, so report support (wlroots requires it).
                    0x12 => cap.value = 1,
                    _ => cap.value = 0,
                }
                log::error!(
                    "[drm] GET_CAP minor={} cap={:#x} -> {}",
                    self.minor,
                    cap.capability,
                    cap.value
                );
                Ok(0)
            }
            // A single DRM client on the primary node is implicitly master;
            // accept (drop-)master so seatd/wlroots session activation succeeds.
            // Magic/auth: `drmIsMaster()` authenticates magic 0 and treats
            // success as "this fd is DRM master". wlroots' dumb-buffer allocator
            // (pixman path) requires master, so always succeed — the single
            // client on the primary node is implicitly master here.
            DRM_IOCTL_GET_MAGIC => {
                // struct drm_auth { __u32 magic; }
                unsafe { *(data as *mut u32) = 1 };
                Ok(0)
            }
            DRM_IOCTL_AUTH_MAGIC => Ok(0),
            DRM_IOCTL_SET_MASTER => {
                // Become DRM master, but do NOT switch the console to graphics
                // yet: defer that to the first real scanout (`drm::scanout`). If
                // the client stalls before presenting a frame (e.g. its renderer
                // fails to init), the kernel text console stays usable and its
                // logs visible instead of freezing on a black screen.
                log::error!("[drm] SET_MASTER (minor={})", self.minor);
                Ok(0)
            }
            DRM_IOCTL_DROP_MASTER => {
                log::error!(
                    "[drm] DROP_MASTER (minor={}) — restoring text console",
                    self.minor
                );
                kernel_hal::console::set_kd_mode(kernel_hal::console::KD_TEXT);
                Ok(0)
            }
            DRM_IOCTL_SET_CLIENT_CAP => {
                // struct drm_set_client_cap { __u64 capability; __u64 value; }
                let cap = unsafe { *(data as *const u64) };
                match cap {
                    // Reject atomic modesetting and writeback so wlroots falls
                    // back to the legacy KMS path, which the software
                    // framebuffer scanout implements.
                    DRM_CLIENT_CAP_ATOMIC | DRM_CLIENT_CAP_WRITEBACK_CONNECTORS => {
                        log::error!(
                            "[drm] SET_CLIENT_CAP cap={} -> rejected (force legacy KMS)",
                            cap
                        );
                        Err(FsError::InvalidParam)
                    }
                    // STEREO_3D, UNIVERSAL_PLANES, ASPECT_RATIO: accept.
                    _ => {
                        log::error!("[drm] SET_CLIENT_CAP cap={} -> accepted", cap);
                        Ok(0)
                    }
                }
            }
            DRM_IOCTL_MODE_CREATE_DUMB => {
                let info = unsafe { &mut *(data as *mut DrmModeCreateDumb) };
                let bpp = info.bpp.max(32);
                let pitch = (info.width * bpp / 8 + 63) & !63;
                let size = (pitch * info.height) as usize;

                if let Some(handle) = drm::alloc_buffer(size) {
                    info.handle = handle.id;
                    info.pitch = pitch;
                    info.size = size as u64;
                    log::error!(
                        "[drm] CREATE_DUMB {}x{} bpp={} -> handle={} pitch={} size={}",
                        info.width,
                        info.height,
                        bpp,
                        handle.id,
                        pitch,
                        size
                    );
                    Ok(0)
                } else {
                    log::error!(
                        "[drm] CREATE_DUMB {}x{} bpp={} -> alloc failed",
                        info.width,
                        info.height,
                        bpp
                    );
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
            DRM_IOCTL_MODE_ADDFB2 => {
                let cmd = unsafe { &mut *(data as *mut DrmModeFbCmd2) };
                if let Some(fb_id) =
                    drm::create_fb(cmd.handles[0], cmd.width, cmd.height, cmd.pitches[0])
                {
                    cmd.fb_id = fb_id;
                    Ok(0)
                } else {
                    Err(FsError::DeviceError)
                }
            }
            DRM_IOCTL_MODE_RMFB => {
                let fb_id = unsafe { *(data as *const u32) };
                drm::rmfb(fb_id);
                Ok(0)
            }
            DRM_IOCTL_MODE_MAP_DUMB => {
                let map = unsafe { &mut *(data as *mut DrmModeMapDumb) };
                map.offset = map.handle as u64;
                Ok(0)
            }
            DRM_IOCTL_MODE_DESTROY_DUMB => {
                let handle = unsafe { *(data as *const u32) };
                drm::gem_close(handle);
                Ok(0)
            }
            DRM_IOCTL_MODE_SETCRTC => {
                // struct drm_mode_crtc has the same layout as DrmModeGetCrtc.
                let req = unsafe { &mut *(data as *mut DrmModeGetCrtc) };
                log::error!(
                    "[drm] SETCRTC crtc={} fb={} ({}x{}+{})",
                    req.crtc_id,
                    req.fb_id,
                    req.x,
                    req.y,
                    req.mode_valid
                );
                if req.fb_id != 0 {
                    drm::set_crtc_fb(req.crtc_id, req.fb_id);
                    drm::scanout(req.fb_id);
                }
                Ok(0)
            }
            DRM_IOCTL_MODE_PAGE_FLIP => {
                let flip = unsafe { *(data as *const DrmModeCrtcPageFlip) };
                if drm::page_flip(flip.fb_id, flip.crtc_id, flip.user_data) {
                    Ok(0)
                } else {
                    Err(FsError::DeviceError)
                }
            }
            DRM_IOCTL_WAIT_VBLANK => {
                // union drm_wait_vblank. A software framebuffer has no real
                // vblank, so synthesize the next sequence from the monotonic
                // clock. If the caller asked for an event (`_DRM_VBLANK_EVENT`)
                // deliver a DRM_EVENT_VBLANK on the card fd; otherwise fill the
                // reply and return immediately instead of blocking.
                let req = unsafe { &mut *(data as *mut DrmWaitVblank) };
                let typ = req.typ;
                let signal = req.val1;
                let seq = drm::vblank_seq_now().wrapping_add(1);
                if typ & _DRM_VBLANK_EVENT != 0 {
                    drm::queue_vblank_event(seq, signal);
                } else {
                    let now = kernel_hal::timer::timer_now();
                    req.typ = 0; // _DRM_VBLANK_ABSOLUTE
                    req.sequence = seq;
                    req.val1 = now.as_secs(); // tval_sec
                    req.val2 = now.subsec_micros() as u64; // tval_usec
                }
                Ok(0)
            }
            DRM_IOCTL_MODE_SETPLANE => {
                // Software KMS: a primary-plane update is equivalent to scanning
                // the framebuffer out (the legacy SETCRTC path). fb_id == 0
                // disables the plane, which we treat as a no-op.
                let req = unsafe { *(data as *const DrmModeSetPlane) };
                if req.fb_id != 0 {
                    drm::set_crtc_fb(req.crtc_id, req.fb_id);
                    drm::scanout(req.fb_id);
                }
                Ok(0)
            }
            DRM_IOCTL_MODE_GETFB => {
                let cmd = unsafe { &mut *(data as *mut DrmModeFbCmd) };
                if let Some(fb) = drm::get_fb(cmd.fb_id) {
                    cmd.width = fb.width;
                    cmd.height = fb.height;
                    cmd.pitch = fb.pitch;
                    cmd.bpp = 32;
                    cmd.depth = 24;
                    // A single client is implicitly DRM master on the primary
                    // node here, so handing back the backing GEM handle is safe.
                    cmd.handle = fb.gem_handle_id;
                    Ok(0)
                } else {
                    Err(FsError::InvalidParam)
                }
            }
            DRM_IOCTL_MODE_GETFB2 => {
                let cmd = unsafe { &mut *(data as *mut DrmModeFbCmd2) };
                if let Some(fb) = drm::get_fb(cmd.fb_id) {
                    cmd.width = fb.width;
                    cmd.height = fb.height;
                    cmd.pixel_format = 0x3432_5258; // DRM_FORMAT_XRGB8888 ("XR24")
                    cmd.flags = 0;
                    cmd.handles = [fb.gem_handle_id, 0, 0, 0];
                    cmd.pitches = [fb.pitch, 0, 0, 0];
                    cmd.offsets = [0; 4];
                    cmd.modifier = [0; 4];
                    Ok(0)
                } else {
                    Err(FsError::InvalidParam)
                }
            }
            DRM_IOCTL_MODE_DIRTYFB => {
                // Flush accumulated damage by re-scanning the framebuffer out.
                // Clients that keep one persistent FB and signal damage with
                // DIRTYFB (X's modesetting shadow, simple toolkits) rely on this
                // to update the screen.
                let cmd = unsafe { *(data as *const DrmModeFbDirtyCmd) };
                if drm::software_kms_active() {
                    drm::scanout(cmd.fb_id);
                }
                Ok(0)
            }
            DRM_IOCTL_MODE_OBJ_SETPROPERTY => {
                // Legacy property writes (connector DPMS, plane rotation, …).
                // The software scanout has no programmable object state, so
                // accept and ignore rather than failing the client's modeset.
                let req = unsafe { *(data as *const DrmModeObjSetProperty) };
                log::error!(
                    "[drm] OBJ_SETPROPERTY obj={} type={:#x} prop={} val={} (accepted, no-op)",
                    req.obj_id,
                    req.obj_type,
                    req.prop_id,
                    req.value
                );
                Ok(0)
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

                // Software framebuffer path: expose the one synthetic encoder
                // here too, consistent with GETCONNECTOR/GETENCODER. With a
                // KMS-capable driver we don't enumerate encoders (no trait API).
                if drm::software_kms_active() && !connectors.is_empty() {
                    if res.encoder_id_ptr != 0 && res.count_encoders >= 1 {
                        unsafe {
                            *(res.encoder_id_ptr as *mut u32) = drm::SYNTH_ENCODER_ID;
                        }
                    }
                    res.count_encoders = 1;
                } else {
                    res.count_encoders = 0;
                }

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
                    conn_res.encoder_id = drm::SYNTH_ENCODER_ID;
                    conn_res.subpixel = 0; // SubPixelUnknown

                    // Report exactly one encoder. wlroots calls this twice: once
                    // to learn the counts, then again with allocated arrays.
                    if conn_res.encoders_ptr != 0 && conn_res.count_encoders >= 1 {
                        unsafe {
                            *(conn_res.encoders_ptr as *mut u32) = drm::SYNTH_ENCODER_ID;
                        }
                    }
                    conn_res.count_encoders = 1;

                    // Report exactly one mode: the display's native resolution.
                    if let Some((w, h, _)) = drm::display_mode() {
                        if conn_res.modes_ptr != 0 && conn_res.count_modes >= 1 {
                            let mode = make_modeinfo(w, h);
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    mode.as_ptr(),
                                    conn_res.modes_ptr as *mut u8,
                                    mode.len(),
                                );
                            }
                        }
                        conn_res.count_modes = 1;
                    } else {
                        conn_res.count_modes = 0;
                    }
                    conn_res.count_props = 0;
                    log::error!(
                        "[drm] GETCONNECTOR id={} connected={} modes={} mode={:?}",
                        conn_res.connector_id,
                        conn.connected,
                        conn_res.count_modes,
                        drm::display_mode()
                    );
                    Ok(0)
                } else {
                    log::error!(
                        "[drm] GETCONNECTOR id={} -> NOT FOUND",
                        conn_res.connector_id
                    );
                    Err(FsError::InvalidParam)
                }
            }
            DRM_IOCTL_MODE_GETENCODER => {
                let enc = unsafe { &mut *(data as *mut DrmModeGetEncoder) };
                enc.encoder_id = drm::SYNTH_ENCODER_ID;
                enc.encoder_type = 0; // DRM_MODE_ENCODER_NONE
                enc.crtc_id = 1; // synthetic CRTC
                enc.possible_crtcs = 1; // bitmask: CRTC index 0
                enc.possible_clones = 0;
                Ok(0)
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
                    // Advertise the formats the software scanout consumes, via
                    // the two-call pattern (count first, then fill).
                    const FORMATS: [u32; 2] = [
                        0x3432_5258, // DRM_FORMAT_XRGB8888 ("XR24")
                        0x3432_5241, // DRM_FORMAT_ARGB8888 ("AR24")
                    ];
                    if res.format_type_ptr != 0
                        && res.count_format_types >= FORMATS.len() as u32
                    {
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                FORMATS.as_ptr(),
                                res.format_type_ptr as *mut u32,
                                FORMATS.len(),
                            );
                        }
                    }
                    res.count_format_types = FORMATS.len() as u32;
                    Ok(0)
                } else {
                    Err(FsError::InvalidParam)
                }
            }
            DRM_IOCTL_MODE_OBJ_GETPROPERTIES => {
                let res = unsafe { &mut *(data as *mut DrmModeObjGetProperties) };
                // Identify the object by id (libdrm often passes obj_type=ANY).
                // Only the plane "type" property is mandatory; connectors/CRTCs
                // report none (the legacy backend tolerates their absence).
                let props: &[(u32, u64)] = if res.obj_id == drm::SYNTH_PLANE_ID {
                    &[(PROP_TYPE, 1)] // value 1 = DRM_PLANE_TYPE_PRIMARY
                } else {
                    &[]
                };
                let n = props.len();
                if n > 0 && res.props_ptr != 0 && (res.count_props as usize) >= n {
                    for (i, (pid, val)) in props.iter().enumerate() {
                        unsafe {
                            *(res.props_ptr as *mut u32).add(i) = *pid;
                            *(res.prop_values_ptr as *mut u64).add(i) = *val;
                        }
                    }
                }
                res.count_props = n as u32;
                log::error!(
                    "[drm] OBJ_GETPROPERTIES obj_id={} obj_type={:#x} -> {} props",
                    res.obj_id,
                    res.obj_type,
                    n
                );
                Ok(0)
            }
            DRM_IOCTL_MODE_GETPROPERTY => {
                let res = unsafe { &mut *(data as *mut DrmModeGetProperty) };
                match res.prop_id {
                    PROP_TYPE => {
                        // Immutable enum "type" with {Overlay, Primary, Cursor}.
                        res.flags = 8 | 4; // DRM_MODE_PROP_ENUM | IMMUTABLE
                        let mut name = [0u8; 32];
                        name[..4].copy_from_slice(b"type");
                        res.name = name;

                        const ENUMS: [(u64, &[u8]); 3] = [
                            (0, b"Overlay"),
                            (1, b"Primary"),
                            (2, b"Cursor"),
                        ];
                        if res.enum_blob_ptr != 0
                            && (res.count_enum_blobs as usize) >= ENUMS.len()
                        {
                            for (i, (val, nm)) in ENUMS.iter().enumerate() {
                                let mut e = DrmModePropertyEnum {
                                    value: *val,
                                    name: [0u8; 32],
                                };
                                e.name[..nm.len()].copy_from_slice(nm);
                                unsafe {
                                    *(res.enum_blob_ptr as *mut DrmModePropertyEnum).add(i) = e;
                                }
                            }
                        }
                        res.count_enum_blobs = ENUMS.len() as u32;
                        // Enum properties also expose their raw value list.
                        const VALUES: [u64; 3] = [0, 1, 2];
                        if res.values_ptr != 0 && (res.count_values as usize) >= VALUES.len() {
                            for (i, v) in VALUES.iter().enumerate() {
                                unsafe {
                                    *(res.values_ptr as *mut u64).add(i) = *v;
                                }
                            }
                        }
                        res.count_values = VALUES.len() as u32;
                        Ok(0)
                    }
                    _ => Err(FsError::InvalidParam),
                }
            }
            _ => {
                // Reverse-engineering hook: log every DRM ioctl wlroots/labwc
                // issues that we do not handle. The DRM nr (`(cmd >> 8) & 0xff`)
                // maps to a DRM_IOCTL_* command, so a photo of this line tells us
                // exactly what labwc wants next. `dir` 1=W 2=R 3=RW, `size` is the
                // arg struct length.
                let nr = (cmd >> 8) & 0xff;
                let size = (cmd >> 16) & 0x3fff;
                let dir = cmd >> 30;
                log::error!(
                    "[drm] UNHANDLED ioctl cmd={:#010x} (drm nr={:#04x} size={} dir={})",
                    cmd,
                    nr,
                    size,
                    dir
                );
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
