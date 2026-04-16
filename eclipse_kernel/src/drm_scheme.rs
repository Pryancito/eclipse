use crate::scheme::{Scheme, Stat, error as scheme_error};
use crate::drm;
use crate::serial;
use alloc::vec::Vec;
use alloc::string::String;
use spin::Mutex;

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

    fn read(&self, _id: usize, _buffer: &mut [u8]) -> Result<usize, usize> {
        Ok(0)
    }

    fn write(&self, _id: usize, _buffer: &[u8]) -> Result<usize, usize> {
        Ok(_buffer.len())
    }

    fn ioctl(&self, id: usize, request: usize, arg: usize) -> Result<usize, usize> {
        let resources = OPEN_RESOURCES.lock();
        let _resource = resources.get(id).and_then(|s| s.as_ref()).ok_or(scheme_error::EBADF)?;
        
        // DRM IOCTL ranges usually start with 0x64 ('d')
        // Standard codes:
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
        const DRM_IOCTL_MODE_GETPROPERTY: u32 = 0xC04064AA;

        match request as u32 {
            DRM_IOCTL_GET_CAP => {
                #[repr(C)]
                struct DrmGetCap {
                    capability: u64,
                    value: u64,
                }
                let cap = unsafe { &mut *(arg as *mut DrmGetCap) };
                if let Some(drm_caps) = drm::get_caps() {
                    match cap.capability {
                        1 => cap.value = if drm_caps.has_cursor { 1 } else { 0 }, // DRM_CAP_CURSOR_WIDTH (oversimplified)
                        3 => cap.value = 1, // DRM_CAP_DUMB_BUFFER
                        _ => cap.value = 0,
                    }
                    Ok(0)
                } else {
                    Err(scheme_error::EIO)
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
                // Cap at 64 MiB per buffer to prevent OOM on bogus user input.
                const MAX_DUMB_BUFFER: u64 = 64 * 1024 * 1024;
                let size_u64 = pitch.saturating_mul(info.height as u64);
                if size_u64 > MAX_DUMB_BUFFER {
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
                    conn.count_modes = 0;
                    conn.count_props = 0;
                    conn.count_encoders = 0;
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
                // Stub for property discovery: return ENODEV or similar if not found
                // For now, just return OK with 0 properties to avoid client crashes
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

    fn lseek(&self, _id: usize, offset: isize, _whence: usize) -> Result<usize, usize> {
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
