use crate::scheme::{Scheme, Stat, error as scheme_error};
use crate::drm;
use crate::serial;
use alloc::vec::Vec;
use alloc::string::String;
use spin::Mutex;

/// Virtual encoder ID used for the single simulated display path (encoder → CRTC 200).
const VIRTUAL_ENCODER_ID: u32 = 101;

/// DRM Scheme implementation
pub struct DrmScheme;

#[derive(Clone, Copy)]
enum DrmResourceKind {
    Control,
}

/// A DRM resource handle with a reference count.
/// The resource is kept alive until `ref_count` reaches zero.
#[derive(Clone, Copy)]
struct DrmResource {
    _kind: DrmResourceKind,
    /// Number of processes (or duplicated fds) that currently hold this resource.
    /// Starts at 1 on open, incremented on dup, decremented on close.
    /// The slot is freed when it reaches 0.
    ref_count: usize,
}

static OPEN_RESOURCES: Mutex<Vec<Option<DrmResource>>> = Mutex::new(Vec::new());

impl Scheme for DrmScheme {
    fn open(&self, path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        serial::serial_printf(format_args!("[DRM-SCHEME] open({})\n", path));
        
        let kind = if path.is_empty() || path == "/" || path == "control" || path == "card0" {
            DrmResourceKind::Control
        } else {
            return Err(scheme_error::ENOENT);
        };

        let resource = DrmResource { _kind: kind, ref_count: 1 };
        let mut resources = OPEN_RESOURCES.lock();
        for (i, slot) in resources.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(resource);
                return Ok(i);
            }
        }
        let id = resources.len();
        resources.push(Some(resource));
        Ok(id)
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
        let resources = OPEN_RESOURCES.lock();
        let _resource = resources.get(id).and_then(|s| s.as_ref()).ok_or(scheme_error::EBADF)?;
        
        // DRM IOCTL ranges usually start with 0x64 ('d')
        // Standard codes:
        const DRM_IOCTL_GET_CAP: u32 = 0xC0106405;
        const DRM_IOCTL_SET_CLIENT_CAP: u32 = 0x4010641D;
        const DRM_IOCTL_MODE_CREATE_DUMB: u32 = 0xC02064B2;
        const DRM_IOCTL_MODE_MAP_DUMB: u32 = 0xC01064B3;
        const DRM_IOCTL_MODE_ADDFB: u32 = 0xC01C64AE;
        const DRM_IOCTL_MODE_ADDFB2: u32 = 0xC08064B8;
        const DRM_IOCTL_MODE_PAGE_FLIP: u32 = 0xC01864B0;
        const DRM_IOCTL_GEM_CLOSE: u32 = 0x40086444;
        const DRM_IOCTL_MODE_DESTROYFB: u32 = 0xC00464AF;
        const DRM_IOCTL_MODE_GETRESOURCES: u32 = 0xC04064A0;
        const DRM_IOCTL_MODE_GETCONNECTOR: u32 = 0xC05064A7;
        const DRM_IOCTL_MODE_GETCRTC: u32 = 0xC06864A1;
        const DRM_IOCTL_MODE_SETCRTC: u32 = 0xC06864A2;
        const DRM_IOCTL_WAIT_VBLANK: u32 = 0xC018643A;
        const DRM_IOCTL_MODE_CURSOR: u32 = 0xC01C64A3;
        const DRM_IOCTL_MODE_GETPLANERESOURCES: u32 = 0xC01064B5;
        const DRM_IOCTL_MODE_GETPLANE: u32 = 0xC02064B6;
        const DRM_IOCTL_MODE_SETPLANE: u32 = 0xC04464B7;
        const DRM_IOCTL_MODE_GETPROPERTY: u32 = 0xC04064AA;
        const DRM_IOCTL_MODE_GETPROPBLOB: u32 = 0xC01064AC;
        const DRM_IOCTL_MODE_OBJ_GETPROPERTIES: u32 = 0xC01C64B9;
        const DRM_IOCTL_VERSION: u32 = 0xC0406400;
        const DRM_IOCTL_MODE_GETENCODER: u32 = 0xC01464A6;

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
            DRM_IOCTL_GET_CAP => {
                #[repr(C)]
                struct DrmGetCap {
                    capability: u64,
                    value: u64,
                }
                let cap = unsafe { &mut *(arg as *mut DrmGetCap) };
                if let Some(drm_caps) = drm::get_caps() {
                    match cap.capability {
                        1 => cap.value = if drm_caps.has_cursor { 1 } else { 0 }, // DRM_CAP_CURSOR_BITMAP
                        3 => cap.value = 1, // DRM_CAP_DUMB_BUFFER: dumb (CPU-mapped) buffers supported
                        5 => cap.value = 0, // DRM_CAP_PRIME: buffer sharing not supported (software renderer)
                        6 => cap.value = 1, // DRM_CAP_TIMESTAMP_MONOTONIC
                        8 => cap.value = drm_caps.max_width as u64,  // DRM_CAP_CURSOR_WIDTH
                        9 => cap.value = drm_caps.max_height as u64, // DRM_CAP_CURSOR_HEIGHT
                        0xB => cap.value = 1, // DRM_CAP_CRTC_IN_VBLANK_EVENT
                        0x10 => cap.value = 0, // DRM_CAP_ADDFB2_MODIFIERS: no format modifier support
                        _ => cap.value = 0,
                    }
                    Ok(0)
                } else {
                    Err(scheme_error::EIO)
                }
            }
            DRM_IOCTL_SET_CLIENT_CAP => {
                // struct drm_set_client_cap { uint64_t capability; uint64_t value; }
                let cap_ptr = arg as *const u64;
                let capability = unsafe { *cap_ptr };
                match capability {
                    // DRM_CLIENT_CAP_STEREO_3D (1), DRM_CLIENT_CAP_UNIVERSAL_PLANES (2),
                    // DRM_CLIENT_CAP_ATOMIC (3), DRM_CLIENT_CAP_ASPECT_RATIO (4),
                    // DRM_CLIENT_CAP_WRITEBACK_CONNECTORS (5)
                    1..=5 => Ok(0),
                    _ => Err(scheme_error::EINVAL),
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
                    conn.encoder_id = VIRTUAL_ENCODER_ID; // Virtual encoder linked to CRTC 200

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
                // Virtual encoder VIRTUAL_ENCODER_ID, linked to CRTC 200 (from simplefb resources)
                if enc.encoder_id == VIRTUAL_ENCODER_ID {
                    enc.encoder_type = 1; // DRM_MODE_ENCODER_DAC
                    enc.crtc_id = 200;
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
                let vbl = unsafe { &mut *(arg as *mut DrmWaitVblank) };
                if drm::wait_vblank(vbl.crtc_id) {
                    Ok(0)
                } else {
                    Err(scheme_error::EINVAL)
                }
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
                    plane_id: u32, crtc_id: u32, fb_id: u32,
                    possible_crtcs: u32, gamma_size: u32,
                    count_formats: u32, format_type_ptr: u64,
                }
                let p = unsafe { &mut *(arg as *mut DrmModeGetPlane) };
                if let Some(info) = drm::get_plane(p.plane_id) {
                    p.crtc_id = info.crtc_id;
                    p.fb_id = info.fb_id;
                    p.possible_crtcs = info.possible_crtcs;
                    p.count_formats = 0;
                    Ok(0)
                } else {
                    Err(scheme_error::ENOENT)
                }
            }
            DRM_IOCTL_MODE_SETPLANE => {
                #[repr(C)]
                #[derive(Clone, Copy)]
                struct DrmModeSetPlane {
                    plane_id: u32, crtc_id: u32, fb_id: u32, flags: u32,
                    crtc_x: i32, crtc_y: i32, crtc_w: u32, crtc_h: u32,
                    src_x: u32, src_y: u32, src_w: u32, src_h: u32,
                }
                let p = unsafe { &*(arg as *const DrmModeSetPlane) };
                if drm::set_plane(p.plane_id, p.crtc_id, p.fb_id, p.crtc_x, p.crtc_y, p.crtc_w, p.crtc_h, p.src_x, p.src_y, p.src_w, p.src_h) {
                    Ok(0)
                } else {
                    Err(scheme_error::EIO)
                }
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
                let p = unsafe { &mut *(arg as *mut DrmModeGetProperty) };
                // Stub for property discovery: return generic "type" for prop_id 1
                if p.prop_id == 1 {
                    p.flags = 0x8; // DRM_MODE_PROP_ENUM
                    let name = b"type\0";
                    let len = core::cmp::min(name.len(), 32);
                    p.name[..len].copy_from_slice(&name[..len]);
                    p.count_values = 3; // Overlay, Primary, Cursor
                    p.count_enum_blobs = 0;
                    Ok(0)
                } else {
                    // For any other property, just return OK with no data to avoid crashes
                    p.count_values = 0;
                    p.count_enum_blobs = 0;
                    Ok(0)
                }
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
                // struct drm_mode_get_blob { uint32_t blob_id; uint32_t length; uint64_t data; }
                // Return blob not found for any unknown blob id
                #[repr(C)]
                struct DrmModeGetBlob { blob_id: u32, length: u32, data: u64 }
                let blob = unsafe { &mut *(arg as *mut DrmModeGetBlob) };
                blob.length = 0;
                Ok(0)
            }
            DRM_IOCTL_MODE_OBJ_GETPROPERTIES => {
                // struct drm_mode_obj_get_properties { props_ptr(u64), values_ptr(u64), count_props(u32), obj_id(u32), obj_type(u32) }
                #[repr(C)]
                struct DrmModeObjGetProperties {
                    props_ptr: u64, values_ptr: u64,
                    count_props: u32, obj_id: u32, obj_type: u32,
                }
                let props = unsafe { &mut *(arg as *mut DrmModeObjGetProperties) };
                props.count_props = 0;
                Ok(0)
            }
            _ => {
                Err(scheme_error::ENOSYS)
            }
        }
    }

    fn fmap(&self, id: usize, offset: usize, _len: usize) -> Result<usize, usize> {
        let resources = OPEN_RESOURCES.lock();
        let _resource = resources.get(id).and_then(|s| s.as_ref()).ok_or(scheme_error::EBADF)?;
        
        // Offset here is what was returned in DRM_IOCTL_MODE_MAP_DUMB
        let handle_id = offset as u32;
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

    fn fstat(&self, _id: usize, stat: &mut Stat) -> Result<usize, usize> {
        stat.mode = 0o20666; // Character device
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
        Ok(list)
    }
}
