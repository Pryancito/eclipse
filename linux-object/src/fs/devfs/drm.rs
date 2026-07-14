//! DRM (Direct Rendering Manager) Subsystem for zCore
//!
//! Provides a unified interface for graphics drivers (NVIDIA, VirtIO, etc.)
//! and handles buffer management (GEM) and mode setting (KMS).

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use lock::Mutex;

use kernel_hal::drivers;
use kernel_hal::mem::phys_to_virt;
pub use zcore_drivers::scheme::drm::{DrmCaps, DrmConnector, DrmCrtc, DrmPlane, GemHandle};
use zcore_drivers::scheme::{DisplayScheme, DrmScheme};
use zircon_object::vm::{pages, MMUFlags, VmObject};

/// Synthetic KMS object IDs used when there is no real DRM/KMS driver — only a
/// dumb framebuffer (`DisplayScheme`, e.g. the UEFI GOP display on bare metal).
/// wlroots' legacy modeset path needs at least one CRTC, connector and encoder
/// to drive an output; we synthesize them around the framebuffer and scan dumb
/// buffers out via [`DisplayScheme::blit_from`].
///
/// The ids must be **distinct across object types**: libdrm identifies objects
/// (for OBJ_GETPROPERTIES etc.) by id alone, often passing obj_type=ANY, so
/// reusing one id for CRTC/connector/plane makes them indistinguishable.
const SYNTH_CRTC_ID: u32 = 1;
const SYNTH_CONNECTOR_ID: u32 = 2;
/// Encoder id exposed to userspace for the synthetic output.
pub const SYNTH_ENCODER_ID: u32 = 3;
/// Primary plane id exposed to userspace for the synthetic output.
pub const SYNTH_PLANE_ID: u32 = 4;

/// Sequence counter for delivered page-flip / vblank events.
static FLIP_SEQ: AtomicU32 = AtomicU32::new(0);

/// One-shot guard so the first scanout logs (every-frame logging would spam).
static SCANOUT_LOGGED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Return the primary framebuffer display, if any.
fn primary_display() -> Option<Arc<dyn DisplayScheme>> {
    drivers::all_display().first()
}

/// Whether the software KMS path should drive the output.
///
/// True whenever a framebuffer display exists. The only DRM drivers in this
/// tree (the nvidia stub, virtio-gpu) cannot drive a real legacy-KMS scanout
/// for wlroots' dumb-buffer + pixman path — the nvidia driver in particular
/// advertises a CRTC/connector but its `page_flip`/`create_fb` are no-ops, so
/// deferring to it leaves the screen black. Scanning the dumb buffer out to the
/// framebuffer display (`blit_from`) is the authoritative output on every
/// machine that has a framebuffer, so we always prefer it. (virtio-gpu also
/// registers a framebuffer display, so its host-shared buffer still updates.)
pub fn software_kms_active() -> bool {
    primary_display().is_some()
}

/// A DRM Framebuffer object
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct DrmFramebuffer {
    pub id: u32,
    /// GEM handle that backs this framebuffer
    pub gem_handle_id: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub phys_addr: u64,
    pub size: usize,
}

struct DrmState {
    drivers: Vec<Arc<dyn DrmScheme>>,
    next_handle_id: u32,
    next_fb_id: u32,
    handles: Vec<(GemHandle, Arc<VmObject>)>,
    framebuffers: Vec<DrmFramebuffer>,
    /// Framebuffer currently bound to the (synthetic) CRTC, reported by GETCRTC.
    crtc_fb: u32,
    /// Pending DRM events (page-flip completions) waiting to be `read()` from
    /// the card fd. Each entry is one fully-encoded `struct drm_event_vblank`.
    events: VecDeque<Vec<u8>>,
}

lazy_static::lazy_static! {
    static ref DRM_STATE: Mutex<DrmState> = Mutex::new(DrmState {
        drivers: Vec::new(),
        next_handle_id: 1,
        next_fb_id: 1,
        handles: Vec::new(),
        framebuffers: Vec::new(),
        crtc_fb: 0,
        events: VecDeque::new(),
    });
}

/// Register a new DRM driver
pub fn register_driver(driver: Arc<dyn DrmScheme>) {
    let mut state = DRM_STATE.lock();
    if driver.name() == "simplefb" {
        state.drivers.push(driver);
    } else {
        state.drivers.insert(0, driver);
    }
}

/// Get the primary DRM driver
pub fn get_primary_driver() -> Option<Arc<dyn DrmScheme>> {
    DRM_STATE.lock().drivers.first().cloned()
}

/// Allocate a buffer (GEM object).
///
/// Backed by contiguous physical memory so it can both be mmap'd by userspace
/// (the dumb-buffer mapping) and scanned out by a software framebuffer display.
/// When a hardware DRM driver is present it is told about the new buffer; with
/// no driver (plain framebuffer) the buffer is purely software.
pub fn alloc_buffer(size: usize) -> Option<GemHandle> {
    if size == 0 {
        return None;
    }

    // Reserve an id and snapshot the driver under the lock, then RELEASE it.
    // `DRM_STATE`'s `lock::Mutex` is an IRQ-disabling spinlock, so the heavy
    // work below must run with it dropped: zeroing a full-screen dumb buffer
    // (1080p ≈ 8 MiB = 2048 pages) under the lock keeps interrupts off for the
    // whole memset — starving the timer, scheduler and input — and serializes
    // every other DRM ioctl behind it. Calling into the driver under the lock
    // is also a latent deadlock if `import_buffer` ever waits on the GPU.
    // (Reserving the id unconditionally can leave a gap on failure; handle ids
    // only need to be unique, so a skipped id is harmless.)
    let (id, driver) = {
        let mut state = DRM_STATE.lock();
        let id = state.next_handle_id;
        state.next_handle_id += 1;
        (id, state.drivers.first().cloned())
    };

    // Allocate contiguous physical memory via VMO (lock released).
    let vmo = VmObject::new_contiguous(pages(size), 12).ok()?;
    let phys_addr = vmo.commit_page(0, MMUFlags::READ).ok()? as u64;

    let handle = GemHandle {
        id,
        size,
        phys_addr,
    };

    // Tell the driver about the new buffer (if any). Without a driver the dumb
    // buffer is software-only and always succeeds.
    let accepted = match driver {
        Some(driver) => driver.import_buffer(handle),
        None => true,
    };
    if accepted {
        // Re-acquire only for the bookkeeping push.
        DRM_STATE.lock().handles.push((handle, vmo));
        Some(handle)
    } else {
        None
    }
}

/// Export a GEM handle for PRIME: return its `(phys_addr, size, backing VMO)`
/// so it can be wrapped in a dma-buf and shared with another DRM node.
pub fn export_handle(handle_id: u32) -> Option<(u64, usize, Arc<VmObject>)> {
    let state = DRM_STATE.lock();
    state
        .handles
        .iter()
        .find(|(h, _)| h.id == handle_id)
        .map(|(h, vmo)| (h.phys_addr, h.size, vmo.clone()))
}

/// Import a dma-buf (PRIME): register a new GEM handle over the same backing
/// frames and return its id. The `VmObject` keeps the memory alive.
pub fn import_dmabuf(phys_addr: u64, size: usize, vmo: Arc<VmObject>) -> u32 {
    let mut state = DRM_STATE.lock();
    let id = state.next_handle_id;
    state.next_handle_id += 1;
    let handle = GemHandle {
        id,
        size,
        phys_addr,
    };
    state.handles.push((handle, vmo));
    id
}

pub fn get_handle(handle_id: u32) -> Option<GemHandle> {
    DRM_STATE
        .lock()
        .handles
        .iter()
        .find(|(h, _)| h.id == handle_id)
        .map(|(h, _)| *h)
}

/// Look up a framebuffer object by id (`DRM_IOCTL_MODE_GETFB`/`GETFB2`).
pub fn get_fb(fb_id: u32) -> Option<DrmFramebuffer> {
    DRM_STATE
        .lock()
        .framebuffers
        .iter()
        .find(|f| f.id == fb_id)
        .copied()
}

/// Create a framebuffer from a GEM handle
pub fn create_fb(handle_id: u32, width: u32, height: u32, pitch: u32) -> Option<u32> {
    let handle = get_handle(handle_id)?;

    // If a hardware DRM driver is present, let it create its own framebuffer
    // object; otherwise the framebuffer is purely software (scanned out via the
    // display's `blit_from`).
    if let Some(driver) = get_primary_driver() {
        driver.create_fb(handle_id, width, height, pitch)?;
    }

    let mut state = DRM_STATE.lock();
    let fb_id = state.next_fb_id;
    state.next_fb_id += 1;

    let fb = DrmFramebuffer {
        id: fb_id,
        gem_handle_id: handle_id,
        width,
        height,
        pitch,
        phys_addr: handle.phys_addr,
        size: (pitch as usize) * (height as usize),
    };

    state.framebuffers.push(fb);
    Some(fb_id)
}

/// Remove a framebuffer (DRM_IOCTL_MODE_RMFB).
pub fn rmfb(fb_id: u32) -> bool {
    let mut state = DRM_STATE.lock();
    if state.crtc_fb == fb_id {
        state.crtc_fb = 0;
    }
    if let Some(pos) = state.framebuffers.iter().position(|f| f.id == fb_id) {
        state.framebuffers.remove(pos);
        true
    } else {
        false
    }
}

/// Native mode of the primary framebuffer display: `(width, height, pitch)`.
pub fn display_mode() -> Option<(u32, u32, u32)> {
    let info = primary_display()?.info();
    Some((info.width, info.height, info.pitch()))
}

/// Bind a framebuffer to a CRTC (the value reported back by GETCRTC).
pub fn set_crtc_fb(_crtc_id: u32, fb_id: u32) {
    DRM_STATE.lock().crtc_fb = fb_id;
}

/// Copy a framebuffer's pixels to the hardware display ("scan out").
///
/// Used by the software KMS path (no GPU driver): the dumb buffer is contiguous
/// physical memory, which we map and blit into the display framebuffer.
pub fn scanout(fb_id: u32) -> bool {
    let fb = {
        let state = DRM_STATE.lock();
        match state.framebuffers.iter().find(|f| f.id == fb_id) {
            Some(f) => *f,
            None => {
                debug!("[drm] scanout: fb_id={} not found", fb_id);
                return false;
            }
        }
    };
    let display = match primary_display() {
        Some(d) => d,
        None => {
            debug!("[drm] scanout: no display");
            return false;
        }
    };
    if fb.phys_addr == 0 || fb.size == 0 {
        return false;
    }
    // Log the first scanout so a console photo confirms pixels are flowing.
    if !SCANOUT_LOGGED.swap(true, Ordering::Relaxed) {
        debug!(
            "[drm] scanout: fb={} {}x{} pitch={} phys={:#x} -> display {}x{}",
            fb_id,
            fb.width,
            fb.height,
            fb.pitch,
            fb.phys_addr,
            display.info().width,
            display.info().height
        );
    }
    let info = display.info();
    let vaddr = phys_to_virt(fb.phys_addr as usize);
    // SAFETY: the buffer is contiguous physical memory of `fb.size` bytes,
    // identity-mapped into the kernel's physmap window at `vaddr`.
    let pixels = unsafe { core::slice::from_raw_parts(vaddr as *const u32, fb.size / 4) };
    let src_stride = (fb.pitch / 4) as usize;
    let width = fb.width.min(info.width);
    let height = fb.height.min(info.height);
    display.blit_from(0, 0, pixels, src_stride, width, height);
    let _ = display.flush();
    // A DRM client owns the framebuffer now: stop the kernel text console from
    // drawing over it (like fbcon yielding to KMS). Restored on DROP_MASTER.
    kernel_hal::console::set_kd_mode(kernel_hal::console::KD_GRAPHICS);
    true
}

/// Page-flip to `fb_id` and queue a completion event for the card fd.
///
/// `crtc_id`/`user_data` come from the page-flip request and are echoed back in
/// the `drm_event_vblank` so libdrm's event loop can match the flip.
pub fn page_flip(fb_id: u32, crtc_id: u32, user_data: u64) -> bool {
    let flipped = if software_kms_active() {
        // No usable hardware KMS: blit the dumb buffer to the framebuffer.
        scanout(fb_id)
    } else if let Some(driver) = get_primary_driver() {
        // Hardware driver owns scanout; fall back to a software blit if it
        // declines (e.g. a framebuffer-only "simplefb" DRM shim).
        driver.page_flip(fb_id) || scanout(fb_id)
    } else {
        scanout(fb_id)
    };
    if flipped {
        set_crtc_fb(crtc_id, fb_id);
        queue_flip_event(crtc_id, user_data);
    }
    flipped
}

/// Encode and enqueue a `struct drm_event_vblank` for the card fd.
///
/// Shared by page-flip completions (`DRM_EVENT_FLIP_COMPLETE`) and vblank waits
/// (`DRM_EVENT_VBLANK`), which use the identical 32-byte wire layout — only the
/// `type` field distinguishes them for libdrm's event dispatcher.
fn push_drm_event(ev_type: u32, crtc_id: u32, seq: u32, user_data: u64) {
    let now = kernel_hal::timer::timer_now();
    // struct drm_event_vblank { u32 type; u32 length; u64 user_data;
    //   u32 tv_sec; u32 tv_usec; u32 sequence; u32 crtc_id; }  (32 bytes)
    let mut ev = Vec::with_capacity(32);
    ev.extend_from_slice(&ev_type.to_ne_bytes());
    ev.extend_from_slice(&32u32.to_ne_bytes());
    ev.extend_from_slice(&user_data.to_ne_bytes());
    ev.extend_from_slice(&(now.as_secs() as u32).to_ne_bytes());
    ev.extend_from_slice(&now.subsec_micros().to_ne_bytes());
    ev.extend_from_slice(&seq.to_ne_bytes());
    ev.extend_from_slice(&crtc_id.to_ne_bytes());
    DRM_STATE.lock().events.push_back(ev);
}

/// Enqueue a `DRM_EVENT_FLIP_COMPLETE` for a completed page flip.
fn queue_flip_event(crtc_id: u32, user_data: u64) {
    const DRM_EVENT_FLIP_COMPLETE: u32 = 2;
    let seq = FLIP_SEQ.fetch_add(1, Ordering::Relaxed);
    push_drm_event(DRM_EVENT_FLIP_COMPLETE, crtc_id, seq, user_data);
}

/// Enqueue a `DRM_EVENT_VBLANK` for a `WAIT_VBLANK` request that asked for an
/// event (`_DRM_VBLANK_EVENT`) instead of blocking.
pub fn queue_vblank_event(seq: u32, user_data: u64) {
    const DRM_EVENT_VBLANK: u32 = 1;
    push_drm_event(DRM_EVENT_VBLANK, SYNTH_CRTC_ID, seq, user_data);
}

/// Synthetic ~60 Hz vertical-blank counter derived from the monotonic clock.
///
/// A software framebuffer has no real vblank interrupt, but `WAIT_VBLANK`
/// callers expect a monotonically increasing sequence; deriving one from time
/// keeps both absolute and relative queries sane.
pub fn vblank_seq_now() -> u32 {
    let now = kernel_hal::timer::timer_now();
    (now.as_nanos() * 60 / 1_000_000_000) as u32
}

/// Pop one pending DRM event into `buf`, returning the number of bytes copied,
/// or `None` if there are no events queued.
pub fn read_event(buf: &mut [u8]) -> Option<usize> {
    let mut state = DRM_STATE.lock();
    let ev = state.events.front()?;
    if buf.len() < ev.len() {
        // Caller's buffer is too small for a whole event; libdrm always reads
        // with a large buffer, so just report "nothing yet" rather than
        // delivering a truncated, unparsable event.
        return None;
    }
    let n = ev.len();
    buf[..n].copy_from_slice(&ev[..n]);
    state.events.pop_front();
    Some(n)
}

/// Whether any DRM events are queued for reading.
pub fn has_events() -> bool {
    !DRM_STATE.lock().events.is_empty()
}

pub fn get_caps() -> Option<DrmCaps> {
    if !software_kms_active() {
        if let Some(d) = get_primary_driver() {
            return Some(d.get_caps());
        }
    }
    // Software framebuffer fallback.
    let (w, h, _) = display_mode()?;
    Some(DrmCaps {
        has_3d: false,
        has_cursor: false,
        max_width: w,
        max_height: h,
    })
}

pub fn gem_close(handle_id: u32) -> bool {
    let mut state = DRM_STATE.lock();
    if let Some(pos) = state.handles.iter().position(|(h, _)| h.id == handle_id) {
        let (handle, _) = state.handles[pos];
        let driver = state.drivers.first().cloned();
        state.handles.remove(pos);
        drop(state);

        if let Some(d) = driver {
            d.free_buffer(handle);
        }
        true
    } else {
        false
    }
}

pub fn get_resources() -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    let state = DRM_STATE.lock();
    let fbs: Vec<u32> = state.framebuffers.iter().map(|fb| fb.id).collect();

    let mut crtcs = Vec::new();
    let mut connectors = Vec::new();
    for driver in &state.drivers {
        let (_, d_crtcs, d_conns) = driver.get_resources();
        crtcs.extend(d_crtcs);
        connectors.extend(d_conns);
    }
    drop(state);

    // Prefer the software framebuffer KMS path: synthesize one CRTC + connector
    // so `drmIsKMS()` passes and wlroots drives the output through our scanout
    // (the hardware DRM stubs here cannot). Falls through to driver-provided
    // resources only when there is no framebuffer display at all.
    if software_kms_active() {
        debug!(
            "[drm] GETRESOURCES: software KMS -> 1 crtc, 1 connector ({:?}) [drivers offered crtcs={} conns={}]",
            display_mode(),
            crtcs.len(),
            connectors.len()
        );
        return (fbs, vec![SYNTH_CRTC_ID], vec![SYNTH_CONNECTOR_ID]);
    }

    debug!(
        "[drm] GETRESOURCES: crtcs={} connectors={} fbs={} (driver-provided, no display)",
        crtcs.len(),
        connectors.len(),
        fbs.len()
    );
    (fbs, crtcs, connectors)
}

pub fn get_connector(id: u32) -> Option<DrmConnector> {
    {
        let state = DRM_STATE.lock();
        for driver in &state.drivers {
            if let Some(conn) = driver.get_connector(id) {
                return Some(conn);
            }
        }
    }
    // Software framebuffer fallback (no driver, or driver without KMS).
    if id != SYNTH_CONNECTOR_ID {
        return None;
    }
    let (w, h, _) = display_mode()?;
    Some(DrmConnector {
        id: SYNTH_CONNECTOR_ID,
        connected: true,
        // Physical size assuming ~96 DPI (1 inch = 25.4 mm, 96 px/inch).
        mm_width: (w * 254 / 960).max(1),
        mm_height: (h * 254 / 960).max(1),
    })
}

pub fn get_crtc(id: u32) -> Option<DrmCrtc> {
    {
        let state = DRM_STATE.lock();
        for driver in &state.drivers {
            if let Some(crtc) = driver.get_crtc(id) {
                return Some(crtc);
            }
        }
    }
    // Software framebuffer fallback (no driver, or driver without KMS).
    if id != SYNTH_CRTC_ID {
        return None;
    }
    display_mode()?;
    let fb_id = DRM_STATE.lock().crtc_fb;
    Some(DrmCrtc {
        id: SYNTH_CRTC_ID,
        fb_id,
        x: 0,
        y: 0,
    })
}

pub fn get_planes() -> Vec<u32> {
    if software_kms_active() {
        // One synthetic primary plane bound to the synthetic CRTC.
        return vec![SYNTH_PLANE_ID];
    }
    let state = DRM_STATE.lock();
    let mut planes = Vec::new();
    for driver in &state.drivers {
        planes.extend(driver.get_planes());
    }
    planes
}

pub fn get_plane(id: u32) -> Option<DrmPlane> {
    {
        let state = DRM_STATE.lock();
        for driver in &state.drivers {
            if let Some(plane) = driver.get_plane(id) {
                return Some(plane);
            }
        }
    }
    if software_kms_active() && id == SYNTH_PLANE_ID {
        return Some(DrmPlane {
            id: SYNTH_PLANE_ID,
            crtc_id: SYNTH_CRTC_ID,
            fb_id: 0,
            possible_crtcs: 1, // bitmask: CRTC index 0
            plane_type: 1,     // DRM_PLANE_TYPE_PRIMARY
        });
    }
    None
}
