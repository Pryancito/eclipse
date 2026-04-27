use crate::scheme::{Scheme, Stat, error as scheme_error};
use crate::drm;
use crate::serial;
use alloc::vec::Vec;
use alloc::string::String;
use alloc::collections::BTreeMap;
use spin::Mutex;

/// Virtual encoder ID used for the single simulated display path (encoder → CRTC 200).
const VIRTUAL_ENCODER_ID: u32 = 101;

/// DRM Scheme implementation
pub struct DrmScheme;

#[derive(Clone, Copy)]
enum DrmResourceKind {
    Directory,
    Control { minor: u32 },
    PrimeBuf { handle: u32 },
}

/// A DRM resource handle with a reference count.
/// The resource is kept alive until `ref_count` reaches zero.
#[derive(Clone, Copy)]
struct DrmResource {
    kind: DrmResourceKind,
    /// Number of processes (or duplicated fds) that currently hold this resource.
    /// Starts at 1 on open, incremented on dup, decremented on close.
    /// The slot is freed when it reaches 0.
    ref_count: usize,
}

static OPEN_RESOURCES: Mutex<Vec<Option<DrmResource>>> = Mutex::new(Vec::new());

/// User-created property blobs (MODE_ID, etc.).
/// Key: blob_id, Value: raw bytes.
static PROP_BLOBS: Mutex<BTreeMap<u32, Vec<u8>>> = Mutex::new(BTreeMap::new());
static NEXT_BLOB_ID: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);

#[derive(Clone, Copy)]
struct KmsState {
    connector_crtc_id: u32,
    crtc_active: u64,
    crtc_mode_blob_id: u32,
}

static KMS_STATE: Mutex<KmsState> = Mutex::new(KmsState {
    connector_crtc_id: 0,
    crtc_active: 0,
    crtc_mode_blob_id: 0,
});

fn alloc_resource(kind: DrmResourceKind) -> usize {
    let resource = DrmResource { kind, ref_count: 1 };
    let mut resources = OPEN_RESOURCES.lock();
    for (i, slot) in resources.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(resource);
            return i;
        }
    }
    let id = resources.len();
    resources.push(Some(resource));
    id
}

impl Scheme for DrmScheme {
    fn open(&self, path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        serial::serial_printf(format_args!("[DRM-SCHEME] open({})\n", path));
        
        let kind = if path.is_empty() || path == "/" {
            DrmResourceKind::Directory
        } else if path == "card0" {
            DrmResourceKind::Control { minor: 0 }
        } else if path == "control" {
            DrmResourceKind::Control { minor: 64 }
        } else if path == "renderD128" {
            DrmResourceKind::Control { minor: 128 }
        } else {
            return Err(scheme_error::ENOENT);
        };
        Ok(alloc_resource(kind))
    }

    fn close(&self, id: usize) -> Result<usize, usize> {
        let mut resources = OPEN_RESOURCES.lock();
        let slot = resources.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;
        slot.ref_count = slot.ref_count.saturating_sub(1);
        if slot.ref_count == 0 {
            resources[id] = None;
        }
        Ok(0)
    }

    fn dup(&self, id: usize) -> Result<usize, usize> {
        let mut resources = OPEN_RESOURCES.lock();
        let slot = resources.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;
        slot.ref_count += 1;
        Ok(0)
    }

    fn read(&self, _id: usize, _buffer: &mut [u8], _offset: u64) -> Result<usize, usize> {
        Ok(0)
    }

    fn write(&self, _id: usize, _buffer: &[u8], _offset: u64) -> Result<usize, usize> {
        Ok(_buffer.len())
    }

    fn ioctl(&self, id: usize, request: usize, arg: usize) -> Result<usize, usize> {
        // IMPORTANT: don't hold OPEN_RESOURCES lock across the whole ioctl handler.
        // Some ioctls (e.g. CREATE_LEASE) allocate new resources and would deadlock
        // if we tried to re-lock OPEN_RESOURCES while already holding it.
        {
            let resources = OPEN_RESOURCES.lock();
            let _resource = resources.get(id).and_then(|s| s.as_ref()).ok_or(scheme_error::EBADF)?;
            let _ = _resource; // validate fd resource exists
        }
        
        // Linux DRM UAPI ioctl numbers (x86_64), as in <drm/drm.h> + <drm/drm_mode.h>.
        // Keep these aligned with Linux so libdrm/wlroots work unmodified.
        const DRM_IOCTL_VERSION: u32 = 0xC0406400;
        const DRM_IOCTL_GET_UNIQUE: u32 = 0xC0106401;
        const DRM_IOCTL_GET_CAP: u32 = 0xC010640C;
        const DRM_IOCTL_SET_CLIENT_CAP: u32 = 0x4010640D;
        const DRM_IOCTL_SET_VERSION: u32 = 0xC0106407;
        const DRM_IOCTL_GEM_CLOSE: u32 = 0x40086409;

        const DRM_IOCTL_MODE_GETRESOURCES: u32 = 0xC04064A0;
        const DRM_IOCTL_MODE_GETCRTC: u32 = 0xC06864A1;
        const DRM_IOCTL_MODE_SETCRTC: u32 = 0xC06864A2;
        const DRM_IOCTL_MODE_GETCONNECTOR: u32 = 0xC05064A7;
        const DRM_IOCTL_MODE_GETENCODER: u32 = 0xC01464A6;

        const DRM_IOCTL_MODE_CREATE_DUMB: u32 = 0xC02064B2;
        const DRM_IOCTL_MODE_MAP_DUMB: u32 = 0xC01064B3;
        const DRM_IOCTL_MODE_ADDFB: u32 = 0xC01C64AE;
        const DRM_IOCTL_MODE_ADDFB2: u32 = 0xC06864B8;
        const DRM_IOCTL_MODE_DESTROYFB: u32 = 0xC00464AF; // RMFB
        const DRM_IOCTL_MODE_PAGE_FLIP: u32 = 0xC01864B0;

        const DRM_IOCTL_WAIT_VBLANK: u32 = 0xC018643A;
        const DRM_IOCTL_MODE_CURSOR: u32 = 0xC01C64A3;

        const DRM_IOCTL_MODE_GETPLANERESOURCES: u32 = 0xC01064B5;
        const DRM_IOCTL_MODE_GETPLANE: u32 = 0xC02064B6;
        const DRM_IOCTL_MODE_SETPLANE: u32 = 0xC03064B7;

        const DRM_IOCTL_MODE_GETPROPERTY: u32 = 0xC04064AA;
        const DRM_IOCTL_MODE_GETPROPBLOB: u32 = 0xC01064AC;
        const DRM_IOCTL_MODE_OBJ_GETPROPERTIES: u32 = 0xC02064B9;

        // Atomic KMS commit ioctl. Even if we don't fully support it yet,
        // the number must match Linux for correct feature probing.
        const DRM_IOCTL_MODE_ATOMIC: u32 = 0xC03864BC;
        // Property blobs (MODE_ID, etc.)
        // Linux: DRM_IOWR(0xBD, struct drm_mode_create_blob) and DRM_IOWR(0xBE, struct drm_mode_destroy_blob)
        const DRM_IOCTL_MODE_CREATEPROPBLOB: u32 = 0xC01064BD;
        const DRM_IOCTL_MODE_DESTROYPROPBLOB: u32 = 0xC00464BE;

        // DRM leases (used by wlroots allocator path)
        const DRM_IOCTL_MODE_CREATE_LEASE: u32 = 0xC01864C6;
        const DRM_IOCTL_MODE_REVOKE_LEASE: u32 = 0xC00464C9;

        // Legacy DRM auth/master ioctls (not strictly needed for our bring-up, but keep Linux values).
        const DRM_IOCTL_GET_MAGIC: u32 = 0x80046402;
        const DRM_IOCTL_AUTH_MAGIC: u32 = 0x40046411;
        const DRM_IOCTL_SET_MASTER: u32 = 0x0000641E;
        const DRM_IOCTL_DROP_MASTER: u32 = 0x0000641F;

        // PRIME dmabuf interop (Linux values).
        const DRM_IOCTL_PRIME_HANDLE_TO_FD: u32 = 0xC00C642D;
        const DRM_IOCTL_PRIME_FD_TO_HANDLE: u32 = 0xC00C642E;

        serial::serial_printf(format_args!("[DRM-SCHEME] ioctl: id={} request=0x{:x}\n", id, request));
        match request as u32 {
            DRM_IOCTL_VERSION => {
                #[repr(C)]
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
                let v = unsafe { &mut *(arg as *mut DrmVersion) };
                v.version_major = 1;
                v.version_minor = 0;
                v.version_patchlevel = 0;
                
                let name = b"eclipse\0";
                let date = b"20260418\0";
                let desc = b"Eclipse DRM Driver\0";
                
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
                #[repr(C)]
                struct DrmUnique {
                    unique_len: usize,
                    unique: *mut u8,
                }
                let u = unsafe { &mut *(arg as *mut DrmUnique) };
                let name = b"eclipse-gpu\0";
                unsafe {
                    if u.unique_len > 0 && !u.unique.is_null() {
                        let len = core::cmp::min(u.unique_len, name.len());
                        core::ptr::copy_nonoverlapping(name.as_ptr(), u.unique, len);
                    }
                }
                u.unique_len = name.len();
                Ok(0)
            }
            DRM_IOCTL_SET_VERSION => {
                // No-op for now, just accept whatever version userspace wants.
                Ok(0)
            }
            DRM_IOCTL_GET_CAP => {
                #[repr(C)]
                struct DrmGetCap {
                    capability: u64,
                    value: u64,
                }
                let cap = unsafe { &mut *(arg as *mut DrmGetCap) };
                if let Some(_drm_caps) = drm::get_caps() {
                    // Values must match <drm/drm.h> DRM_CAP_* (libdrm drmGetCap).
                    match cap.capability {
                        0x1 => cap.value = 1, // DRM_CAP_DUMB_BUFFER
                        0x2 => cap.value = 0, // DRM_CAP_VBLANK_HIGH_CRTC
                        0x3 => cap.value = 0, // DRM_CAP_DUMB_PREFERRED_DEPTH (optional)
                        0x4 => cap.value = 0, // DRM_CAP_DUMB_PREFER_SHADOW
                        0x5 => cap.value = 3, // DRM_CAP_PRIME: IMPORT|EXPORT
                        0x6 => cap.value = 1, // DRM_CAP_TIMESTAMP_MONOTONIC
                        0x7 => cap.value = 0, // DRM_CAP_ASYNC_PAGE_FLIP
                        0x8 => cap.value = 64, // DRM_CAP_CURSOR_WIDTH
                        0x9 => cap.value = 64, // DRM_CAP_CURSOR_HEIGHT
                        0x10 => cap.value = 0, // DRM_CAP_ADDFB2_MODIFIERS
                        0x11 => cap.value = 0, // DRM_CAP_PAGE_FLIP_TARGET
                        0x12 => cap.value = 1, // DRM_CAP_CRTC_IN_VBLANK_EVENT
                        0x13 | 0x14 | 0x15 => cap.value = 0, // SYNCOBJ / TIMELINE / ATOMIC_ASYNC_PAGE_FLIP
                        _ => cap.value = 0,
                    }
                    Ok(0)
                } else {
                    Err(scheme_error::EIO)
                }
            }
            DRM_IOCTL_PRIME_HANDLE_TO_FD => {
                #[repr(C)]
                struct DrmPrimeHandle {
                    handle: u32,
                    flags: u32,
                    fd: i32,
                }
                let ph = unsafe { &mut *(arg as *mut DrmPrimeHandle) };
                let _ = ph.flags;

                let pid = crate::process::current_process_id().ok_or(scheme_error::ESRCH)?;
                let drm_scheme_id = crate::scheme::get_scheme_id("drm").ok_or(scheme_error::EINVAL)?;

                // Create a new DRM scheme resource representing a dmabuf view of this GEM handle.
                let res_id = alloc_resource(DrmResourceKind::PrimeBuf { handle: ph.handle });
                // EMFILE isn't part of scheme_error; use EINVAL for "no fd slots".
                let new_fd = crate::fd::fd_open(pid, drm_scheme_id, res_id, 0).ok_or(scheme_error::EINVAL)?;
                ph.fd = new_fd as i32;
                Ok(0)
            }
            DRM_IOCTL_PRIME_FD_TO_HANDLE => {
                #[repr(C)]
                struct DrmPrimeHandle {
                    handle: u32,
                    flags: u32,
                    fd: i32,
                }
                let ph = unsafe { &mut *(arg as *mut DrmPrimeHandle) };
                let _ = ph.flags;

                let pid = crate::process::current_process_id().ok_or(scheme_error::ESRCH)?;
                if ph.fd < 0 {
                    return Err(scheme_error::EINVAL);
                }
                let fd_entry = crate::fd::fd_get(pid, ph.fd as usize).ok_or(scheme_error::EBADF)?;
                let drm_scheme_id = crate::scheme::get_scheme_id("drm").ok_or(scheme_error::EINVAL)?;
                if fd_entry.scheme_id != drm_scheme_id {
                    return Err(scheme_error::EINVAL);
                }
                let resources = OPEN_RESOURCES.lock();
                let res = resources
                    .get(fd_entry.resource_id)
                    .and_then(|slot| slot.as_ref())
                    .ok_or(scheme_error::EBADF)?;
                match res.kind {
                    DrmResourceKind::PrimeBuf { handle } => {
                        ph.handle = handle;
                        Ok(0)
                    }
                    DrmResourceKind::Control { .. } | DrmResourceKind::Directory => Err(scheme_error::EINVAL),
                }
            }
            DRM_IOCTL_SET_CLIENT_CAP => {
                #[repr(C)]
                struct DrmSetClientCap {
                    capability: u64,
                    value: u64,
                }

                let ptr = arg as u64;
                if !crate::syscalls::is_user_pointer(ptr, core::mem::size_of::<DrmSetClientCap>() as u64) {
                    return Err(scheme_error::EFAULT);
                }

                // SAFETY: userspace pointer validated above. Still use unaligned reads.
                let cap = unsafe { (ptr as *const DrmSetClientCap).read_unaligned() };

                // Accept the common caps wlroots requests. Value is usually 1 (enable).
                match cap.capability {
                    // DRM_CLIENT_CAP_STEREO_3D (1)
                    1 => Ok(0),
                    // DRM_CLIENT_CAP_UNIVERSAL_PLANES (2)
                    2 => Ok(0),
                    // DRM_CLIENT_CAP_ATOMIC (3)
                    3 => Ok(0),
                    // DRM_CLIENT_CAP_ASPECT_RATIO (4)
                    4 => Ok(0),
                    // DRM_CLIENT_CAP_WRITEBACK_CONNECTORS (5)
                    5 => Ok(0),
                    _ => Ok(0),
                }
            }
            DRM_IOCTL_MODE_CREATE_DUMB => {
                #[repr(C)]
                struct DrmModeCreateDumb {
                    height: u32,
                    width: u32,
                    bpp: u32,
                    flags: u32,
                    handle: u32,
                    pitch: u32,
                    size: u64,
                }
                let info = unsafe { &mut *(arg as *mut DrmModeCreateDumb) };
                if info.width == 0 || info.height == 0 || info.bpp == 0 {
                    return Err(scheme_error::EINVAL);
                }
                let bytes_per_pixel = ((info.bpp as u64) + 7) / 8;
                let pitch = ((info.width as u64) * bytes_per_pixel + 255) & !255u64;
                let size_u64 = pitch.saturating_mul(info.height as u64);
                if size_u64 > crate::drm::MAX_GEM_BUFFER_SIZE as u64 {
                    return Err(scheme_error::EINVAL);
                }
                let size = size_u64 as usize;
                
                if let Some(handle) = drm::alloc_buffer(size) {
                    info.handle = handle.id;
                    info.pitch = pitch as u32;
                    info.size = size_u64;
                    Ok(0)
                } else {
                    Err(scheme_error::EIO)
                }
            }
            DRM_IOCTL_MODE_ADDFB => {
                #[repr(C)]
                struct DrmModeFbCmd {
                    fb_id: u32,
                    width: u32,
                    height: u32,
                    pitch: u32,
                    bpp: u32,
                    depth: u32,
                    handle: u32,
                }
                let cmd = unsafe { &mut *(arg as *mut DrmModeFbCmd) };
                if let Some(fb_id) = drm::create_fb(cmd.handle, cmd.width, cmd.height, cmd.pitch) {
                    cmd.fb_id = fb_id;
                    Ok(0)
                } else {
                    Err(scheme_error::EIO)
                }
            }
            DRM_IOCTL_MODE_PAGE_FLIP => {
                #[repr(C)]
                #[derive(Clone, Copy)]
                struct DrmModeCrtcPageFlip {
                    crtc_id: u32,
                    fb_id: u32,
                    flags: u32,
                    reserved: u32,
                    user_data: u64,
                }
                let flip = unsafe { *(arg as *const DrmModeCrtcPageFlip) };
                if drm::page_flip(flip.fb_id) {
                    Ok(0)
                } else {
                    Err(scheme_error::EIO)
                }
            }
            DRM_IOCTL_MODE_MAP_DUMB => {
                #[repr(C)]
                struct DrmModeMapDumb {
                    handle: u32,
                    pad: u32,
                    offset: u64,
                }
                let map = unsafe { &mut *(arg as *mut DrmModeMapDumb) };
                // In our simple kernel, the "offset" can just be the handle ID
                // which fmap will then use to look up the physical address.
                map.offset = map.handle as u64;
                Ok(0)
            }
            DRM_IOCTL_GEM_CLOSE => {
                #[repr(C)]
                struct DrmGemClose {
                    handle: u32,
                    pad: u32,
                }
                let gc = unsafe { &*(arg as *const DrmGemClose) };
                if drm::gem_close(gc.handle) { Ok(0) } else { Err(scheme_error::EINVAL) }
            }
            DRM_IOCTL_MODE_DESTROYFB => {
                let fb_id = unsafe { *(arg as *const u32) };
                if drm::destroy_fb(fb_id) { Ok(0) } else { Err(scheme_error::EINVAL) }
            }
            DRM_IOCTL_MODE_GETRESOURCES => {
                #[repr(C)]
                struct DrmModeCardRes {
                    fb_id_ptr: u64,
                    crtc_id_ptr: u64,
                    connector_id_ptr: u64,
                    encoder_id_ptr: u64,
                    count_fbs: u32,
                    count_crtcs: u32,
                    count_connectors: u32,
                    count_encoders: u32,
                    min_width: u32, max_width: u32,
                    min_height: u32, max_height: u32,
                }
                let res = unsafe { &mut *(arg as *mut DrmModeCardRes) };
                let (fbs, crtcs, connectors) = drm::get_resources();
                let encoders: [u32; 1] = [VIRTUAL_ENCODER_ID];

                // Copy IDs if pointers are non-null and counts match
                if res.fb_id_ptr != 0 && res.count_fbs >= fbs.len() as u32 {
                    unsafe { core::ptr::copy_nonoverlapping(fbs.as_ptr(), res.fb_id_ptr as *mut u32, fbs.len()); }
                }
                if res.crtc_id_ptr != 0 && res.count_crtcs >= crtcs.len() as u32 {
                    unsafe { core::ptr::copy_nonoverlapping(crtcs.as_ptr(), res.crtc_id_ptr as *mut u32, crtcs.len()); }
                }
                if res.connector_id_ptr != 0 && res.count_connectors >= connectors.len() as u32 {
                    unsafe { core::ptr::copy_nonoverlapping(connectors.as_ptr(), res.connector_id_ptr as *mut u32, connectors.len()); }
                }
                if res.encoder_id_ptr != 0 && res.count_encoders >= encoders.len() as u32 {
                    unsafe { core::ptr::copy_nonoverlapping(encoders.as_ptr(), res.encoder_id_ptr as *mut u32, encoders.len()); }
                }

                res.count_fbs = fbs.len() as u32;
                res.count_crtcs = crtcs.len() as u32;
                res.count_connectors = connectors.len() as u32;
                res.count_encoders = encoders.len() as u32;

                if let Some(caps) = drm::get_caps() {
                    res.max_width = caps.max_width;
                    res.max_height = caps.max_height;
                }
                res.min_width = 0;
                res.min_height = 0;
                Ok(0)
            }
            DRM_IOCTL_MODE_GETCONNECTOR => {
                #[repr(C)]
                struct DrmModeGetConnector {
                    encoders_ptr: u64, modes_ptr: u64, props_ptr: u64, prop_values_ptr: u64,
                    count_modes: u32, count_props: u32, count_encoders: u32,
                    encoder_id: u32, connector_id: u32, connector_type: u32, connector_type_id: u32,
                    connection: u32, mm_width: u32, mm_height: u32, subpixel: u32, pad: u32,
                }
                let conn = unsafe { &mut *(arg as *mut DrmModeGetConnector) };
                if let Some(d_conn) = drm::get_connector(conn.connector_id) {
                    conn.connection = if d_conn.connected { 1 } else { 2 };
                    conn.mm_width = d_conn.mm_width;
                    conn.mm_height = d_conn.mm_height;
                    conn.connector_type = 11; // DRM_MODE_CONNECTOR_eDP
                    conn.connector_type_id = 1;
                    conn.encoder_id = VIRTUAL_ENCODER_ID; // Virtual encoder linked to CRTC 2000

                    // Populate one preferred mode using the framebuffer dimensions
                    let (fb_width, fb_height) = crate::boot::get_fb_info()
                        .map(|(_, w, h, _, _, _)| (w, h))
                        .unwrap_or((1920, 1080));

                    // Build a drm_mode_modeinfo (68 bytes) for the preferred mode.
                    // Layout: clock(u32), h*(u16 x5), v*(u16 x5), vrefresh(u32), flags(u32),
                    //         type(u32), name([u8;32])
                    // Approximate clock = htotal * vtotal * 60 / 1000 kHz
                    // Approximate horizontal total = hdisplay + ~12% blanking (typical CVT/GTF value).
                    // Approximate vertical total = vdisplay + ~5% blanking.
                    let htotal = fb_width + fb_width / 8;  // ~12.5% horizontal blanking
                    let vtotal = fb_height + fb_height / 20; // ~5% vertical blanking
                    let clock_khz = (htotal as u64 * vtotal as u64 * 60 / 1000) as u32;
                    let hsync_start = (fb_width + 8) as u16;
                    let hsync_end   = (fb_width + 40) as u16;
                    let vsync_start = (fb_height + 3) as u16;
                    let vsync_end   = (fb_height + 8) as u16;

                    #[repr(C, packed)]
                    struct ModeInfo {
                        clock:       u32,
                        hdisplay:    u16, hsync_start: u16, hsync_end: u16,
                        htotal:      u16, hskew: u16,
                        vdisplay:    u16, vsync_start: u16, vsync_end: u16,
                        vtotal:      u16, vscan: u16,
                        vrefresh:    u32,
                        flags:       u32,
                        mode_type:   u32,
                        name:        [u8; 32],
                    }

                    let mut mode_name = [0u8; 32];
                    let name_str = alloc::format!("{}x{}", fb_width, fb_height);
                    let name_bytes = name_str.as_bytes();
                    let copy_len = core::cmp::min(name_bytes.len(), 31);
                    mode_name[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

                    let mode = ModeInfo {
                        clock:       clock_khz,
                        hdisplay:    fb_width as u16,
                        hsync_start,
                        hsync_end,
                        htotal:      htotal as u16,
                        hskew:       0,
                        vdisplay:    fb_height as u16,
                        vsync_start,
                        vsync_end,
                        vtotal:      vtotal as u16,
                        vscan:       0,
                        vrefresh:    60,
                        flags:       0x5, // DRM_MODE_FLAG_PHSYNC | DRM_MODE_FLAG_PVSYNC
                        mode_type:   0x48, // DRM_MODE_TYPE_DRIVER | DRM_MODE_TYPE_PREFERRED
                        name:        mode_name,
                    };

                    if conn.modes_ptr != 0 && conn.count_modes >= 1 {
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                &mode as *const ModeInfo as *const u8,
                                conn.modes_ptr as *mut u8,
                                core::mem::size_of::<ModeInfo>(),
                            );
                        }
                    }
                    conn.count_modes = 1;

                    // Populate encoder list
                    let enc_id: u32 = VIRTUAL_ENCODER_ID;
                    if conn.encoders_ptr != 0 && conn.count_encoders >= 1 {
                        unsafe { (conn.encoders_ptr as *mut u32).write_unaligned(enc_id); }
                    }
                    conn.count_encoders = 1;
                    conn.count_props = 0;
                    Ok(0)
                } else {
                    Err(scheme_error::ENOENT)
                }
            }
            DRM_IOCTL_MODE_GETENCODER => {
                #[repr(C)]
                struct DrmModeGetEncoder {
                    encoder_id: u32,
                    encoder_type: u32,
                    crtc_id: u32,
                    possible_crtcs: u32,
                    possible_clones: u32,
                }
                let enc = unsafe { &mut *(arg as *mut DrmModeGetEncoder) };
                // VIRTUAL_ENCODER_ID is a virtual DAC encoder. We report crtc_id=0 (unset)
                // because this is the initial state before wlroots issues DRM_IOCTL_MODE_SETCRTC.
                // The old value of 2000 was incorrect (CRTC 200 is the only CRTC we expose).
                if enc.encoder_id == VIRTUAL_ENCODER_ID {
                    enc.encoder_type = 1; // DRM_MODE_ENCODER_DAC
                    enc.crtc_id = 0; // No CRTC currently active; wlroots assigns one via SETCRTC
                    enc.possible_crtcs = 1;
                    enc.possible_clones = 0;
                    Ok(0)
                } else {
                    Err(scheme_error::ENOENT)
                }
            }
            DRM_IOCTL_MODE_GETCRTC => {
                #[repr(C)]
                struct DrmModeCrtc {
                    set_connectors_ptr: u64, count_connectors: u32,
                    crtc_id: u32, fb_id: u32, x: u32, y: u32,
                    gamma_size: u32, mode_valid: u32,
                    mode: [u8; 68], // drm_mode_modeinfo
                }
                let crtc = unsafe { &mut *(arg as *mut DrmModeCrtc) };
                if let Some(d_crtc) = drm::get_crtc(crtc.crtc_id) {
                    crtc.fb_id = d_crtc.fb_id;
                    crtc.x = d_crtc.x;
                    crtc.y = d_crtc.y;
                    crtc.mode_valid = 0;
                    Ok(0)
                } else {
                    Err(scheme_error::ENOENT)
                }
            }
            DRM_IOCTL_MODE_SETCRTC => {
                #[repr(C)]
                struct DrmModeCrtc {
                    set_connectors_ptr: u64, count_connectors: u32,
                    crtc_id: u32, fb_id: u32, x: u32, y: u32,
                    gamma_size: u32, mode_valid: u32,
                    mode: [u8; 68], // drm_mode_modeinfo
                }
                let crtc = unsafe { &*(arg as *const DrmModeCrtc) };
                // If a framebuffer is specified, perform the page flip to display it
                if crtc.fb_id != 0 {
                    drm::page_flip(crtc.fb_id);
                }
                Ok(0)
            }
            DRM_IOCTL_WAIT_VBLANK => {
                #[repr(C)]
                struct DrmWaitVblank {
                    request: u32,
                    crtc_id: u32,
                    reply: u64,
                }
                #[repr(C)]
                struct DrmWaitVblankReply {
                    sequence: u32,
                    tv_sec: i32,
                    tv_usec: i32,
                }
                
                let _vbl = unsafe { &mut *(arg as *mut DrmWaitVblank) };
                
                // Simulate 60Hz: wait ~16ms to prevent busy loops and allow other threads to run
                crate::syscalls::process_sleep_ms(16);
                
                // Fill reply with fake but incrementing data (crucial for wlroots)
                let reply = unsafe { &mut *(arg as *mut DrmWaitVblankReply) };
                static VBLANK_SEQ: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);
                reply.sequence = VBLANK_SEQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                
                let ticks = crate::interrupts::ticks();
                reply.tv_sec = (ticks / 1000) as i32;
                reply.tv_usec = ((ticks % 1000) * 1000) as i32;

                Ok(0)
            }
            DRM_IOCTL_MODE_CURSOR => {
                #[repr(C)]
                struct DrmModeCursor {
                    flags: u32,
                    crtc_id: u32,
                    x: i32,
                    y: i32,
                    width: u32,
                    height: u32,
                    handle: u32,
                }
                let cur = unsafe { &*(arg as *const DrmModeCursor) };
                if drm::set_cursor(cur.crtc_id, cur.x, cur.y, cur.handle, cur.flags) {
                    Ok(0)
                } else {
                    Err(scheme_error::EINVAL)
                }
            }
            DRM_IOCTL_MODE_GETPLANERESOURCES => {
                #[repr(C)]
                #[derive(Clone, Copy)]
                struct DrmModeGetPlaneRes {
                    plane_id_ptr: u64,
                    count_planes: u32,
                    pad: u32,
                }
                let res = unsafe { &mut *(arg as *mut DrmModeGetPlaneRes) };
                let planes = drm::get_planes();
                serial::serial_printf(format_args!("DRM: GETPLANERESOURCES found {} planes\n", planes.len()));
                
                if res.plane_id_ptr != 0 && res.count_planes >= planes.len() as u32 {
                    unsafe { core::ptr::copy_nonoverlapping(planes.as_ptr(), res.plane_id_ptr as *mut u32, planes.len()); }
                }
                res.count_planes = planes.len() as u32;
                Ok(0)
            }
            DRM_IOCTL_MODE_GETPLANE => {
                #[repr(C)]
                #[derive(Clone, Copy)]
                struct DrmModeGetPlane {
                    plane_id: u32,
                    crtc_id: u32,
                    fb_id: u32,
                    possible_crtcs: u32,
                    gamma_size: u32,
                    count_format_types: u32,
                    format_type_ptr: u64,
                }
                let p = unsafe { &mut *(arg as *mut DrmModeGetPlane) };
                if let Some(info) = drm::get_plane(p.plane_id) {
                    p.crtc_id = info.crtc_id;
                    p.fb_id = info.fb_id;
                    p.possible_crtcs = info.possible_crtcs;
                    p.gamma_size = 0;
                    // Supported pixel formats: XRGB8888 and ARGB8888.
                    // Follow the Linux 2-step pattern: first call returns count (ptr=NULL or
                    // count=0), second call fills the array.
                    const FORMATS: [u32; 2] = [
                        0x34325258, // DRM_FORMAT_XRGB8888
                        0x34325241, // DRM_FORMAT_ARGB8888
                    ];
                    if p.format_type_ptr != 0 && p.count_format_types > 0 {
                        // Write only as many formats as the caller allocated (capped to our list).
                        let write_count = core::cmp::min(p.count_format_types as usize, FORMATS.len());
                        let byte_len = (write_count * core::mem::size_of::<u32>()) as u64;
                        if crate::syscalls::is_user_pointer(p.format_type_ptr, byte_len) {
                            for (i, &fmt) in FORMATS[..write_count].iter().enumerate() {
                                unsafe { (p.format_type_ptr as *mut u32).add(i).write_unaligned(fmt) };
                            }
                        } else {
                            return Err(scheme_error::EFAULT);
                        }
                    }
                    p.count_format_types = FORMATS.len() as u32;
                    Ok(0)
                } else {
                    Err(scheme_error::ENOENT)
                }
            }
            DRM_IOCTL_MODE_SETPLANE => {
                #[repr(C)]
                #[derive(Clone, Copy)]
                struct DrmModeSetPlane {
                    plane_id: u32,
                    crtc_id: u32,
                    fb_id: u32,
                    flags: u32,
                    crtc_x: i32,
                    crtc_y: i32,
                    crtc_w: u32,
                    crtc_h: u32,
                    src_x: u32,
                    src_y: u32,
                    src_w: u32,
                    src_h: u32,
                }
                let p = unsafe { &*(arg as *const DrmModeSetPlane) };
                if drm::set_plane(p.plane_id, p.crtc_id, p.fb_id, p.crtc_x, p.crtc_y, p.crtc_w, p.crtc_h, p.src_x, p.src_y, p.src_w, p.src_h) {
                    Ok(0)
                } else {
                    Err(scheme_error::EIO)
                }
            }
            DRM_IOCTL_MODE_CREATE_LEASE => {
                // Linux UAPI (include/uapi/drm/drm_mode.h): struct drm_mode_create_lease
                // On success the kernel ioctl returns 0 and fills lessee_id + fd in the struct.
                #[repr(C)]
                #[derive(Clone, Copy)]
                struct DrmModeCreateLease {
                    object_ids: u64,
                    object_count: u32,
                    flags: u32,
                    lessee_id: u32,
                    fd: u32,
                }

                let pid = crate::process::current_process_id().ok_or(scheme_error::ESRCH)?;
                let drm_scheme_id = crate::scheme::get_scheme_id("drm").ok_or(scheme_error::EINVAL)?;

                // Validate user pointer (best-effort).
                let ptr = arg as u64;
                if !crate::syscalls::is_user_pointer(ptr, core::mem::size_of::<DrmModeCreateLease>() as u64) {
                    return Err(scheme_error::EFAULT);
                }

                // SAFETY: userspace pointer validated above.
                let mut lease = unsafe { (ptr as *const DrmModeCreateLease).read_unaligned() };
                serial::serial_printf(format_args!(
                    "[DRM-SCHEME] CREATE_LEASE: pid={} object_count={} flags=0x{:x}\n",
                    pid,
                    lease.object_count,
                    lease.flags
                ));

                // Create a new DRM control resource and hand it out as a fresh fd.
                let res_id = alloc_resource(DrmResourceKind::Control { minor: 0 });
                let Some(new_fd) = crate::fd::fd_open(pid, drm_scheme_id, res_id, 0) else {
                    serial::serial_printf(format_args!(
                        "[DRM-SCHEME] CREATE_LEASE: fd_open failed (pid={})\n",
                        pid
                    ));
                    return Err(scheme_error::EINVAL);
                };

                // Non-zero lessee IDs (same style as Linux idr_alloc starting at 1).
                static NEXT_LESSEE_ID: core::sync::atomic::AtomicU32 =
                    core::sync::atomic::AtomicU32::new(0);
                let prev = NEXT_LESSEE_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                lease.lessee_id = prev.wrapping_add(1);
                lease.fd = new_fd as u32;
                unsafe { (ptr as *mut DrmModeCreateLease).write_unaligned(lease) };

                serial::serial_printf(format_args!(
                    "[DRM-SCHEME] CREATE_LEASE: ok lessee_id={} fd={} (ioctl ret 0)\n",
                    lease.lessee_id,
                    lease.fd
                ));
                Ok(0)
            }
            DRM_IOCTL_MODE_REVOKE_LEASE => {
                // Linux UAPI: struct drm_mode_revoke_lease { __u32 lease_id; __u32 _reserved; }
                // We accept and no-op for now.
                serial::serial_printf(format_args!("[DRM-SCHEME] REVOKE_LEASE\n"));
                Ok(0)
            }
            DRM_IOCTL_MODE_ADDFB2 => {
                // struct drm_mode_fb_cmd2: fb_id(u32), width(u32), height(u32),
                // pixel_format(u32), flags(u32), handles[4](u32), pitches[4](u32),
                // offsets[4](u32), modifier[4](u64)
                #[repr(C)]
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
                let cmd = unsafe { &mut *(arg as *mut DrmModeFbCmd2) };
                serial::serial_printf(format_args!("[DRM-SCHEME] ADDFB2: {}x{} format=0x{:x} handle={}\n", 
                    cmd.width, cmd.height, cmd.pixel_format, cmd.handles[0]));
                // Use the first handle/pitch (primary plane)
                let handle = cmd.handles[0];
                let pitch = cmd.pitches[0];
                if let Some(fb_id) = drm::create_fb(handle, cmd.width, cmd.height, pitch) {
                    cmd.fb_id = fb_id;
                    Ok(0)
                } else {
                    Err(scheme_error::EIO)
                }
            }
            DRM_IOCTL_MODE_GETPROPBLOB => {
                #[repr(C)]
                struct DrmModeGetBlob { blob_id: u32, length: u32, data: u64 }
                let blob = unsafe { &mut *(arg as *mut DrmModeGetBlob) };
                // EDID (optional): return minimal dummy EDID if requested
                if blob.blob_id == 10 {
                    let edid = [0u8; 128];
                    if blob.data != 0 && blob.length >= 128 {
                        unsafe { core::ptr::copy_nonoverlapping(edid.as_ptr(), blob.data as *mut u8, 128); }
                    }
                    blob.length = 128;
                    return Ok(0);
                }

                // User-created blobs (MODE_ID, etc.)
                let blobs = PROP_BLOBS.lock();
                if let Some(data) = blobs.get(&blob.blob_id) {
                    if blob.data != 0 && blob.length >= data.len() as u32 {
                        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), blob.data as *mut u8, data.len()); }
                    }
                    blob.length = data.len() as u32;
                    Ok(0)
                } else {
                    Err(scheme_error::ENOENT)
                }
            }
            DRM_IOCTL_MODE_OBJ_GETPROPERTIES => {
                #[repr(C)]
                struct DrmModeObjGetProperties {
                    props_ptr: u64,
                    prop_values_ptr: u64,
                    count_props: u32,
                    obj_id: u32,
                    obj_type: u32,
                }

                // Minimal stable property IDs (names must match wlroots tables)
                const PROP_CRTC_ACTIVE: u32 = 0x100;
                const PROP_CRTC_MODE_ID: u32 = 0x101;
                const PROP_CRTC_GAMMA_LUT: u32 = 0x102;
                const PROP_CRTC_GAMMA_LUT_SIZE: u32 = 0x103;
                const PROP_CRTC_VRR_ENABLED: u32 = 0x104;

                const PROP_CONN_CRTC_ID: u32 = 0x200;
                const PROP_CONN_EDID: u32 = 0x201;

                const PROP_PLANE_TYPE: u32 = 0x300;
                const PROP_PLANE_FB_ID: u32 = 0x301;
                const PROP_PLANE_CRTC_ID: u32 = 0x302;
                const PROP_PLANE_SRC_X: u32 = 0x303;
                const PROP_PLANE_SRC_Y: u32 = 0x304;
                const PROP_PLANE_SRC_W: u32 = 0x305;
                const PROP_PLANE_SRC_H: u32 = 0x306;
                const PROP_PLANE_CRTC_X: u32 = 0x307;
                const PROP_PLANE_CRTC_Y: u32 = 0x308;
                const PROP_PLANE_CRTC_W: u32 = 0x309;
                const PROP_PLANE_CRTC_H: u32 = 0x30A;

                // Object type values from drm_mode.h
                const DRM_MODE_OBJECT_ANY: u32 = 0;
                const DRM_MODE_OBJECT_CRTC: u32 = 0xcccccccc;
                const DRM_MODE_OBJECT_CONNECTOR: u32 = 0xc0c0c0c0;
                const DRM_MODE_OBJECT_PLANE: u32 = 0xeeeeeeee;

                let req = unsafe { &mut *(arg as *mut DrmModeObjGetProperties) };
                let obj_id = req.obj_id;

                // wlroots/libdrm frequently call with DRM_MODE_OBJECT_ANY.
                // In that case, infer the object type from the ID.
                let obj_type = if req.obj_type == DRM_MODE_OBJECT_ANY {
                    if drm::get_plane(obj_id).is_some() {
                        DRM_MODE_OBJECT_PLANE
                    } else if drm::get_crtc(obj_id).is_some() {
                        DRM_MODE_OBJECT_CRTC
                    } else if drm::get_connector(obj_id).is_some() {
                        DRM_MODE_OBJECT_CONNECTOR
                    } else {
                        0xFFFF_FFFF
                    }
                } else {
                    req.obj_type
                };

                // IDs are stable; values are dynamic per-object.
                let prop_ids: &[u32];
                let prop_vals: &[u64];
                let mut vals_small: [u64; 11] = [0; 11];

                match obj_type {
                    DRM_MODE_OBJECT_CRTC => {
                        const IDS: [u32; 5] = [
                            PROP_CRTC_ACTIVE,
                            PROP_CRTC_MODE_ID,
                            PROP_CRTC_GAMMA_LUT,
                            PROP_CRTC_GAMMA_LUT_SIZE,
                            PROP_CRTC_VRR_ENABLED,
                        ];
                        let st = *KMS_STATE.lock();
                        vals_small[0] = st.crtc_active; // ACTIVE
                        vals_small[1] = st.crtc_mode_blob_id as u64; // MODE_ID
                        vals_small[2] = 0; // GAMMA_LUT
                        vals_small[3] = 0; // GAMMA_LUT_SIZE
                        vals_small[4] = 0; // VRR_ENABLED
                        prop_ids = &IDS;
                        prop_vals = &vals_small[..IDS.len()];
                    }
                    DRM_MODE_OBJECT_CONNECTOR => {
                        const IDS: [u32; 2] = [PROP_CONN_CRTC_ID, PROP_CONN_EDID];
                        let st = *KMS_STATE.lock();
                        vals_small[0] = st.connector_crtc_id as u64; // CRTC_ID
                        vals_small[1] = 10; // EDID blob id (dummy)
                        prop_ids = &IDS;
                        prop_vals = &vals_small[..IDS.len()];
                    }
                    DRM_MODE_OBJECT_PLANE => {
                        const IDS: [u32; 11] = [
                            PROP_PLANE_TYPE,
                            PROP_PLANE_FB_ID,
                            PROP_PLANE_CRTC_ID,
                            PROP_PLANE_SRC_X,
                            PROP_PLANE_SRC_Y,
                            PROP_PLANE_SRC_W,
                            PROP_PLANE_SRC_H,
                            PROP_PLANE_CRTC_X,
                            PROP_PLANE_CRTC_Y,
                            PROP_PLANE_CRTC_W,
                            PROP_PLANE_CRTC_H,
                        ];
                        if let Some(p) = drm::get_plane(obj_id) {
                            vals_small[0] = p.plane_type as u64; // type: 1=PRIMARY, 2=CURSOR, 0=OVERLAY
                            vals_small[1] = p.fb_id as u64;
                            vals_small[2] = p.crtc_id as u64;
                        } else {
                            vals_small[0] = 0;
                        }
                        prop_ids = &IDS;
                        prop_vals = &vals_small[..IDS.len()];
                    }
                    _ => {
                        prop_ids = &[];
                        prop_vals = &[];
                    }
                }

                // Always report the required count.
                req.count_props = prop_ids.len() as u32;

                // Fill arrays if provided (2-step ioctl pattern).
                if req.props_ptr != 0 && (req.count_props as usize) <= prop_ids.len() {
                    let out = req.props_ptr as *mut u32;
                    for (i, pid) in prop_ids.iter().enumerate() {
                        unsafe { out.add(i).write_unaligned(*pid) };
                    }
                }
                if req.prop_values_ptr != 0 && (req.count_props as usize) <= prop_vals.len() {
                    let out = req.prop_values_ptr as *mut u64;
                    for (i, v) in prop_vals.iter().enumerate() {
                        unsafe { out.add(i).write_unaligned(*v) };
                    }
                }

                Ok(0)
            }
            DRM_IOCTL_MODE_GETPROPERTY => {
                #[repr(C)]
                struct DrmModeGetProperty {
                    values_ptr: u64,
                    enum_blob_ptr: u64,
                    prop_id: u32,
                    flags: u32,
                    name: [u8; 32],
                    count_values: u32,
                    count_enum_blobs: u32,
                }

                fn write_name(dst: &mut [u8; 32], s: &[u8]) {
                    dst.fill(0);
                    let n = core::cmp::min(dst.len() - 1, s.len());
                    dst[..n].copy_from_slice(&s[..n]);
                }

                let p = unsafe { &mut *(arg as *mut DrmModeGetProperty) };

                // Linux UAPI flags (from drm_mode.h)
                const DRM_MODE_PROP_RANGE: u32 = 0x2;
                const DRM_MODE_PROP_ENUM: u32 = 0x8;
                const DRM_MODE_PROP_BLOB: u32 = 0x10;
                const DRM_MODE_PROP_OBJECT: u32 = 0x40;

                #[repr(C)]
                struct DrmModePropertyEnum {
                    value: u64,
                    name: [u8; 32],
                }

                fn write_enum(dst: &mut DrmModePropertyEnum, value: u64, name: &[u8]) {
                    dst.value = value;
                    dst.name.fill(0);
                    let n = core::cmp::min(dst.name.len() - 1, name.len());
                    dst.name[..n].copy_from_slice(&name[..n]);
                }

                // Property naming + minimal type metadata so wlroots/libdrm can interpret them.
                match p.prop_id {
                    // CRTC
                    0x100 => { write_name(&mut p.name, b"ACTIVE"); p.flags = DRM_MODE_PROP_RANGE; p.count_values = 2; }
                    0x101 => { write_name(&mut p.name, b"MODE_ID"); p.flags = DRM_MODE_PROP_BLOB; p.count_values = 0; }
                    0x102 => { write_name(&mut p.name, b"GAMMA_LUT"); p.flags = DRM_MODE_PROP_BLOB; p.count_values = 0; }
                    0x103 => { write_name(&mut p.name, b"GAMMA_LUT_SIZE"); p.flags = DRM_MODE_PROP_RANGE; p.count_values = 2; }
                    0x104 => { write_name(&mut p.name, b"VRR_ENABLED"); p.flags = DRM_MODE_PROP_RANGE; p.count_values = 2; }

                    // Connector
                    0x200 => { write_name(&mut p.name, b"CRTC_ID"); p.flags = DRM_MODE_PROP_OBJECT; p.count_values = 0; }
                    0x201 => { write_name(&mut p.name, b"EDID"); p.flags = DRM_MODE_PROP_BLOB; p.count_values = 0; }

                    // Plane
                    0x300 => { write_name(&mut p.name, b"type"); p.flags = DRM_MODE_PROP_ENUM; p.count_enum_blobs = 3; }
                    0x301 => { write_name(&mut p.name, b"FB_ID"); p.flags = DRM_MODE_PROP_OBJECT; }
                    0x302 => { write_name(&mut p.name, b"CRTC_ID"); p.flags = DRM_MODE_PROP_OBJECT; }
                    0x303 => { write_name(&mut p.name, b"SRC_X"); p.flags = DRM_MODE_PROP_RANGE; p.count_values = 2; }
                    0x304 => { write_name(&mut p.name, b"SRC_Y"); p.flags = DRM_MODE_PROP_RANGE; p.count_values = 2; }
                    0x305 => { write_name(&mut p.name, b"SRC_W"); p.flags = DRM_MODE_PROP_RANGE; p.count_values = 2; }
                    0x306 => { write_name(&mut p.name, b"SRC_H"); p.flags = DRM_MODE_PROP_RANGE; p.count_values = 2; }
                    0x307 => { write_name(&mut p.name, b"CRTC_X"); p.flags = DRM_MODE_PROP_RANGE; p.count_values = 2; }
                    0x308 => { write_name(&mut p.name, b"CRTC_Y"); p.flags = DRM_MODE_PROP_RANGE; p.count_values = 2; }
                    0x309 => { write_name(&mut p.name, b"CRTC_W"); p.flags = DRM_MODE_PROP_RANGE; p.count_values = 2; }
                    0x30A => { write_name(&mut p.name, b"CRTC_H"); p.flags = DRM_MODE_PROP_RANGE; p.count_values = 2; }
                    _ => {
                        // Unknown property ID.
                        return Err(scheme_error::EINVAL);
                    }
                }

                // Fill range values if requested.
                if (p.flags & DRM_MODE_PROP_RANGE) != 0 {
                    // Defaults: [0, 1] for bool-ish, otherwise [0, 0].
                    let (min, max) = match p.prop_id {
                        0x100 => (0, 1), // ACTIVE
                        0x103 => (0, 4096), // GAMMA_LUT_SIZE
                        0x104 => (0, 1), // VRR_ENABLED
                        _ => (0, 0),
                    };
                    if p.values_ptr != 0 && p.count_values >= 2 {
                        let out = p.values_ptr as *mut u64;
                        unsafe {
                            out.add(0).write_unaligned(min);
                            out.add(1).write_unaligned(max);
                        }
                    }
                }

                // Fill enum list for plane "type" if requested.
                if p.prop_id == 0x300 && p.enum_blob_ptr != 0 && p.count_enum_blobs >= 3 {
                    let out = p.enum_blob_ptr as *mut DrmModePropertyEnum;
                    unsafe {
                        write_enum(&mut *out.add(0), 0, b"Overlay");
                        write_enum(&mut *out.add(1), 1, b"Primary");
                        write_enum(&mut *out.add(2), 2, b"Cursor");
                    }
                }

                Ok(0)
            }
            DRM_IOCTL_MODE_CREATEPROPBLOB => {
                // Linux UAPI: struct drm_mode_create_blob { __u64 data; __u32 length; __u32 blob_id; }
                #[repr(C)]
                #[derive(Clone, Copy)]
                struct DrmModeCreateBlob {
                    data: u64,
                    length: u32,
                    blob_id: u32,
                }
                let ptr = arg as u64;
                if !crate::syscalls::is_user_pointer(ptr, core::mem::size_of::<DrmModeCreateBlob>() as u64) {
                    return Err(scheme_error::EFAULT);
                }
                let mut req = unsafe { (ptr as *const DrmModeCreateBlob).read_unaligned() };
                if req.length == 0 || req.data == 0 {
                    return Err(scheme_error::EINVAL);
                }
                if !crate::syscalls::is_user_pointer(req.data, req.length as u64) {
                    return Err(scheme_error::EFAULT);
                }

                let blob_id = NEXT_BLOB_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                let mut v = Vec::with_capacity(req.length as usize);
                unsafe {
                    v.set_len(req.length as usize);
                    core::ptr::copy_nonoverlapping(req.data as *const u8, v.as_mut_ptr(), req.length as usize);
                }
                PROP_BLOBS.lock().insert(blob_id, v);
                req.blob_id = blob_id;
                unsafe { (ptr as *mut DrmModeCreateBlob).write_unaligned(req) };
                Ok(0)
            }
            DRM_IOCTL_MODE_DESTROYPROPBLOB => {
                // Linux UAPI: struct drm_mode_destroy_blob { __u32 blob_id; }
                let ptr = arg as u64;
                if !crate::syscalls::is_user_pointer(ptr, 4) {
                    return Err(scheme_error::EFAULT);
                }
                let blob_id = unsafe { (ptr as *const u32).read_unaligned() };
                PROP_BLOBS.lock().remove(&blob_id);
                Ok(0)
            }
            DRM_IOCTL_MODE_ATOMIC => {
                // Linux: struct drm_mode_atomic
                // { flags(u32), count_objs(u32), objs_ptr(u64), count_props_ptr(u64),
                //   props_ptr(u64), prop_values_ptr(u64), reserved(u64), user_data(u64) }
                #[repr(C)]
                #[derive(Clone, Copy)]
                struct DrmModeAtomic {
                    flags: u32,
                    count_objs: u32,
                    objs_ptr: u64,
                    count_props_ptr: u64,
                    props_ptr: u64,
                    prop_values_ptr: u64,
                    reserved: u64,
                    user_data: u64,
                }

                // Flags (subset) from drm_mode.h
                const DRM_MODE_ATOMIC_TEST_ONLY: u32 = 0x00000100;

                let ptr = arg as u64;
                if !crate::syscalls::is_user_pointer(ptr, core::mem::size_of::<DrmModeAtomic>() as u64) {
                    return Err(scheme_error::EFAULT);
                }
                let req = unsafe { (ptr as *const DrmModeAtomic).read_unaligned() };

                if req.count_objs == 0 {
                    return Ok(0);
                }
                if req.objs_ptr == 0 || req.count_props_ptr == 0 || req.props_ptr == 0 || req.prop_values_ptr == 0 {
                    return Err(scheme_error::EINVAL);
                }

                let objs_len = (req.count_objs as usize) * core::mem::size_of::<u32>();
                let counts_len = (req.count_objs as usize) * core::mem::size_of::<u32>();
                if !crate::syscalls::is_user_pointer(req.objs_ptr, objs_len as u64)
                    || !crate::syscalls::is_user_pointer(req.count_props_ptr, counts_len as u64)
                {
                    return Err(scheme_error::EFAULT);
                }

                // Read object IDs and per-object prop counts.
                let mut obj_ids: Vec<u32> = Vec::with_capacity(req.count_objs as usize);
                let mut prop_counts: Vec<u32> = Vec::with_capacity(req.count_objs as usize);
                unsafe {
                    obj_ids.set_len(req.count_objs as usize);
                    prop_counts.set_len(req.count_objs as usize);
                    core::ptr::copy_nonoverlapping(req.objs_ptr as *const u32, obj_ids.as_mut_ptr(), obj_ids.len());
                    core::ptr::copy_nonoverlapping(req.count_props_ptr as *const u32, prop_counts.as_mut_ptr(), prop_counts.len());
                }

                let total_props: usize = prop_counts.iter().map(|&c| c as usize).sum();
                if total_props == 0 {
                    return Ok(0);
                }

                let props_len = total_props * core::mem::size_of::<u32>();
                let vals_len = total_props * core::mem::size_of::<u64>();
                if !crate::syscalls::is_user_pointer(req.props_ptr, props_len as u64)
                    || !crate::syscalls::is_user_pointer(req.prop_values_ptr, vals_len as u64)
                {
                    return Err(scheme_error::EFAULT);
                }

                let mut prop_ids: Vec<u32> = Vec::with_capacity(total_props);
                let mut prop_vals: Vec<u64> = Vec::with_capacity(total_props);
                unsafe {
                    prop_ids.set_len(total_props);
                    prop_vals.set_len(total_props);
                    core::ptr::copy_nonoverlapping(req.props_ptr as *const u32, prop_ids.as_mut_ptr(), total_props);
                    core::ptr::copy_nonoverlapping(req.prop_values_ptr as *const u64, prop_vals.as_mut_ptr(), total_props);
                }

                // Apply the commit (very small subset) so wlroots can modeset via atomic.
                // We support: connector CRTC_ID, CRTC ACTIVE/MODE_ID, primary plane FB_ID/CRTC_ID/src/dst.
                let mut st = KMS_STATE.lock();
                let mut cursor = 0usize;
                let mut pending_fb: Option<u32> = None;
                let mut pending_plane: Option<(u32, u32, i32, i32, u32, u32, u32, u32, u32, u32)> = None;

                for (obj_i, &obj) in obj_ids.iter().enumerate() {
                    let n = prop_counts.get(obj_i).copied().unwrap_or(0) as usize;
                    for _ in 0..n {
                        let pid = prop_ids.get(cursor).copied().unwrap_or(0);
                        let val = prop_vals.get(cursor).copied().unwrap_or(0);
                        cursor += 1;

                        match pid {
                            // Connector CRTC_ID
                            0x200 => st.connector_crtc_id = val as u32,
                            // CRTC ACTIVE
                            0x100 => st.crtc_active = val,
                            // CRTC MODE_ID
                            0x101 => st.crtc_mode_blob_id = val as u32,

                            // Plane properties
                            0x301 => { pending_fb = Some(val as u32); }
                            0x302 => {
                                // Plane CRTC_ID; we infer primary plane id from obj.
                                let _ = obj;
                            }
                            0x303 | 0x304 | 0x305 | 0x306 | 0x307 | 0x308 | 0x309 | 0x30A => {
                                // Handled after we’ve collected everything; ignore here.
                            }
                            _ => {}
                        }
                    }
                }

                // Decode plane config from prop arrays (best-effort).
                // wlroots sets SRC_* as 16.16 fixed point; we keep raw values.
                // If anything is missing we still try a page flip.
                if let Some(fb_id) = pending_fb {
                    // Default full-screen, top-left.
                    let (w, h) = crate::boot::get_fb_info()
                        .map(|(_, w, h, _, _, _)| (w as u32, h as u32))
                        .unwrap_or((640, 480));
                    pending_plane = Some((300, st.connector_crtc_id, 0, 0, w, h, 0, 0, w << 16, h << 16));
                    pending_fb = Some(fb_id);
                }

                let is_test = (req.flags & DRM_MODE_ATOMIC_TEST_ONLY) != 0;
                if is_test {
                    return Ok(0);
                }
                drop(st);

                if let (Some(fb_id), Some((plane_id, crtc_id, x, y, w, h, src_x, src_y, src_w, src_h))) =
                    (pending_fb, pending_plane)
                {
                    // In our minimal KMS, setting the primary plane is equivalent to presenting.
                    let _ = drm::set_plane(plane_id, crtc_id, fb_id, x, y, w, h, src_x, src_y, src_w, src_h);
                }
                Ok(0)
            }
            DRM_IOCTL_GET_MAGIC => {
                let magic_ptr = arg as *mut u32;
                unsafe { *magic_ptr = 0x1234; }
                Ok(0)
            }
            DRM_IOCTL_AUTH_MAGIC => {
                Ok(0)
            }
            DRM_IOCTL_SET_MASTER => {
                Ok(0)
            }
            DRM_IOCTL_DROP_MASTER => {
                Ok(0)
            }
            _ => {
                if let Some(driver) = drm::get_primary_driver() {
                    driver.ioctl(request as u32, arg)
                } else {
                    serial::serial_printf(format_args!("[DRM-SCHEME] UNKNOWN ioctl (no driver): 0x{:x}\n", request));
                    Err(scheme_error::ENOSYS)
                }
            }
        }
    }

    fn fmap(&self, id: usize, offset: usize, _len: usize) -> Result<usize, usize> {
        let resources = OPEN_RESOURCES.lock();
        let resource = resources.get(id).and_then(|s| s.as_ref()).ok_or(scheme_error::EBADF)?;
        
        // Offset here is what was returned in DRM_IOCTL_MODE_MAP_DUMB
        // For PRIME dmabuf fds, we ignore offset and map the stored handle.
        let handle_id = match resource.kind {
            DrmResourceKind::Directory => return Err(scheme_error::EINVAL),
            DrmResourceKind::Control { .. } => offset as u32,
            DrmResourceKind::PrimeBuf { handle } => handle,
        };
        if let Some(handle) = drm::get_handle(handle_id) {
            let mut phys = handle.phys_addr as usize;
            
            // If the buffer is in VRAM (BAR1), signal that we want Write-Combining (WC)
            // by setting the highest bit of the physical address (which is unused on x86_64).
            if let Some(fb_info) = crate::nvidia::get_nvidia_fb_info() {
                let (_, bar1_phys, _, _, _) = fb_info;
                let bar1_size = 256 * 1024 * 1024; // Standard BAR1 size assumption
                if handle.phys_addr >= bar1_phys && handle.phys_addr < bar1_phys + bar1_size as u64 {
                    phys |= 1 << 63;
                }
            }
            
            Ok(phys)
        } else {
            Err(scheme_error::EINVAL)
        }
    }

    fn fstat(&self, id: usize, stat: &mut Stat) -> Result<usize, usize> {
        let resources = OPEN_RESOURCES.lock();
        let resource = resources.get(id).and_then(|s| s.as_ref()).ok_or(scheme_error::EBADF)?;
        
        match resource.kind {
            DrmResourceKind::Directory => {
                stat.mode = 0o40755; // Directory
                stat.dev = 0;
                stat.rdev = 0;
            }
            DrmResourceKind::Control { minor } => {
                stat.mode = 0o20666; // Character device
                // DRM primary node major is 226, card0 minor is 0, renderD128 is 128.
                stat.rdev = crate::syscalls::linux_makedev(226, minor as u32);
                stat.dev = 0;
            }
            DrmResourceKind::PrimeBuf { .. } => {
                stat.mode = 0o20666;
                stat.rdev = crate::syscalls::linux_makedev(226, 0);
                stat.dev = 0;
            }
        }
        Ok(0)
    }

    fn lseek(&self, _id: usize, offset: isize, _whence: usize, _current_offset: u64) -> Result<usize, usize> {
        // DRM devices usually don't support lseek on the control node.
        // We'll just return the offset or 0.
        Ok(offset as usize)
    }

    fn getdents(&self, _id: usize) -> Result<Vec<String>, usize> {
        // Return the virtual device nodes for DRM
        let mut list = Vec::new();
        list.push(String::from("card0"));
        list.push(String::from("control"));
        list.push(String::from("renderD128"));
        Ok(list)
    }
}
