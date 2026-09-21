//! DRM (Direct Rendering Manager) Subsystem for zCore
//!
//! Provides a unified interface for graphics drivers (NVIDIA, VirtIO, etc.)
//! and handles buffer management (GEM) and mode setting (KMS).

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::convert::TryFrom;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::time::Duration;
use lock::Mutex;

use crate::sync::{Event, EventBus};
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
/// Synthetic CRTC id exposed to userspace for the synthetic output.
pub const SYNTH_CRTC_ID: u32 = 1;
const SYNTH_CONNECTOR_ID: u32 = 2;
/// Encoder id exposed to userspace for the synthetic output.
pub const SYNTH_ENCODER_ID: u32 = 3;
/// Primary plane id exposed to userspace for the synthetic output.
pub const SYNTH_PLANE_ID: u32 = 4;

/// One-shot guard so the first scanout logs (every-frame logging would spam).
static SCANOUT_LOGGED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// One-shot latch for the "framebuffer has no backing" scanout warning.
static SCANOUT_NULL_LOGGED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
/// One-shot latch for "CE present enabled but every GPU declined the copy".
static CE_NO_TAKER_LOGGED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
/// One-shot latch for the CE staging-buffer allocation failure warning.
static CE_STAGING_ALLOC_FAILED_LOGGED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
/// Staging buffer for the pitch-mismatch CE present: `(vaddr, paddr, size,
/// keepalive VMO)`. The compositor's rows (client pitch) are CPU-repacked into
/// this sysmem buffer at the scanout pitch — cheap cached WB→WB copies — so
/// the flat CE copy's equal-stride requirement is met and the slow CPU→BAR1
/// store path (measured 42 MB/s on real hardware even with a verified-WC
/// mapping) is bypassed entirely. Allocated once at first use.
#[allow(clippy::type_complexity)]
static CE_STAGING: Mutex<Option<(usize, u64, usize, Arc<VmObject>)>> = Mutex::new(None);
/// Full-frame presents completed, for the rate-limited phase-timing klog.
static PRESENT_FRAME_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// CE staging repacks completed, for the rate-limited repack-timing klog.
static CE_REPACK_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Master switch for the per-frame GPU copy-engine present (`ce_present`).
/// Enabled automatically when a GPU finishes bring-up -- a compute GPU at boot
/// (dual RTX: P2P copy into the console GOP FB), or the console GPU itself
/// after the deferred `nvidia.console_gpu` bring-up -- or explicitly via
/// `nvidia.cepresent`. Opt out with `nvidia.nocepresent`. On failure the path
/// auto-wedges and falls back to the CPU blit for the rest of the boot.
static CE_PRESENT_ENABLED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// When set, `present_now_region` / CE present no-op so the console GPU's
/// BAR1 stays quiet during a deferred GSP-RM bring-up (hwcursor path).
static SCANOUT_PAUSED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Whether the CRTC has been turned off by the client: DPMS off, a `SETCRTC`
/// with `fb_id = 0`, or an atomic commit staging `ACTIVE = 0`.
///
/// All three were accepted and ignored, so the panel kept showing the last
/// frame forever while the compositor's own state said the output was off.
/// That is idle blanking, `wlr-output-power-management` and closing a laptop
/// lid, none of which could turn a screen off. Linux disables the pipe:
/// `drm_mode_setcrtc` with a null fb calls `set_config` with `.fb = NULL`, and
/// DPMS off goes through `drm_atomic_helper_connector_dpms`.
static CRTC_BLANKED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Nanoseconds-since-boot after which a pause set by [`set_scanout_paused_for`]
/// expires by itself. `0` means "no watchdog" (a plain [`set_scanout_paused`]).
static SCANOUT_PAUSE_DEADLINE_NS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// How long a deferred console-GPU bring-up may hold scanout paused before the
/// watchdog resumes it anyway. Generous, because a real GSP boot + state-load
/// on cold hardware is tens of seconds and cutting one short would put
/// labwc's BAR1 traffic right back into the SEC2 window this exists to keep
/// quiet. Bounded, because the alternative is a permanently frozen desktop.
pub const SCANOUT_PAUSE_MAX: core::time::Duration = core::time::Duration::from_secs(90);

/// Turn the CRTC off (paint the panel black and stop repainting it) or back on.
///
/// There is no hardware pipe to disable on the software-KMS path, so "off" is
/// black pixels plus a latch that keeps the kernel's own repaints -- cursor
/// compositing, damage re-scans -- from lighting it back up.
///
/// **Divergence from Linux, on purpose.** With the CRTC off, Linux fails a
/// page flip with EINVAL until the client turns it back on. Here any explicit
/// present un-blanks instead, because this panel is also the only console: a
/// stray or mis-ordered DPMS write must cost one black frame, not a machine
/// with no way to show anything again. Everything that really turns an output
/// off -- wlroots, Xorg's DPMS -- stops presenting when it does, so the blank
/// holds exactly as long as it should.
pub fn set_crtc_blanked(on: bool) {
    if CRTC_BLANKED.swap(on, Ordering::SeqCst) == on {
        return;
    }
    if on {
        if let Some(display) = primary_display() {
            display.clear(zcore_drivers::prelude::RgbColor::new(0, 0, 0));
            let _ = display.flush();
        }
        kernel_hal::klog_info!("[drm] CRTC off: panel blanked");
    } else {
        kernel_hal::klog_info!("[drm] CRTC on");
    }
}

/// Whether the CRTC is currently off. Drives `GETCRTC`'s readback and the
/// DPMS property, and gates the kernel's own repaints.
pub fn crtc_blanked() -> bool {
    CRTC_BLANKED.load(Ordering::SeqCst)
}

/// Enable/disable the per-frame CE-offloaded present. Set at boot from the
/// cmdline flags, and again after a deferred console-GPU bring-up, which can
/// make a GPU able to present that was not able to when boot decided.
pub fn set_ce_present_enabled(on: bool) {
    CE_PRESENT_ENABLED.store(on, Ordering::Relaxed);
}

/// Whether the CE present path is currently on, so the deferred bring-up can
/// tell "boot already enabled this" from "boot found nothing ready".
pub fn ce_present_enabled() -> bool {
    CE_PRESENT_ENABLED.load(Ordering::Relaxed)
}

/// Pause/resume software+CE scanout. Used by deferred console-GPU GSP bring-up
/// so labwc's frame loop does not write BAR1 during SEC2 STARTCPU.
///
/// Prefer [`set_scanout_paused_for`] when what follows the pause is a call into
/// the hardware that can fail to return: this variant latches until someone
/// calls it again with `false`, and nobody else will.
pub fn set_scanout_paused(on: bool) {
    SCANOUT_PAUSE_DEADLINE_NS.store(0, Ordering::SeqCst);
    SCANOUT_PAUSED.store(on, Ordering::SeqCst);
    if on {
        kernel_hal::klog_info!("[drm] scanout PAUSED (console GSP bring-up window)");
    } else {
        kernel_hal::klog_info!("[drm] scanout RESUMED");
    }
}

/// Pause scanout with a watchdog: the pause lifts by itself after `max`, even
/// if the code that set it never runs again.
///
/// The caller of the deferred console bring-up parks scanout around
/// `bringup_step14`, which drives the SEC2 STARTCPU path -- the one known way
/// this hardware wedges. A wedge (or a panic) there never reaches the matching
/// `set_scanout_paused(false)`, and a latched pause is not a quiet
/// degradation: `present_now_region` keeps *acknowledging* every flip while
/// touching nothing, so the compositor runs happily and the screen is frozen
/// forever. Expiring on a clock read, rather than from a second task, is what
/// makes the recovery independent of any thread surviving.
pub fn set_scanout_paused_for(max: core::time::Duration) {
    let deadline = kernel_hal::timer::timer_now() + max;
    SCANOUT_PAUSE_DEADLINE_NS.store(deadline.as_nanos() as u64, Ordering::SeqCst);
    SCANOUT_PAUSED.store(true, Ordering::SeqCst);
    kernel_hal::klog_info!(
        "[drm] scanout PAUSED (console GSP bring-up window, watchdog {}s)",
        max.as_secs()
    );
}

/// Whether scanout is currently paused, expiring a watchdogged pause whose
/// deadline has passed. Every reader goes through here so the expiry happens
/// on the frame that needs the answer.
pub fn scanout_paused() -> bool {
    if !SCANOUT_PAUSED.load(Ordering::SeqCst) {
        return false;
    }
    let deadline = SCANOUT_PAUSE_DEADLINE_NS.load(Ordering::SeqCst);
    if deadline == 0 || (kernel_hal::timer::timer_now().as_nanos() as u64) < deadline {
        return true;
    }
    // Deadline passed: resume, once. Whoever loses the race just sees a
    // resumed scanout, which is the point.
    SCANOUT_PAUSE_DEADLINE_NS.store(0, Ordering::SeqCst);
    if SCANOUT_PAUSED.swap(false, Ordering::SeqCst) {
        kernel_hal::klog_warn!(
            "[drm] scanout watchdog: la ventana de bring-up de la GPU de consola expiro sin \
             reanudar (bring-up colgado o abortado) -- se reanuda el scanout para no dejar \
             el escritorio congelado"
        );
    }
    false
}

/// Master switch for the atomic-modesetting uAPI (`DRM_CLIENT_CAP_ATOMIC` +
/// `DRM_IOCTL_MODE_ATOMIC`). OFF by default: the legacy-KMS path is the one
/// proven on real hardware, so — like nouveau, which shipped its atomic
/// support behind `nouveau.atomic=1` — the atomic path is strictly opt-in via
/// the `drm.atomic` kernel cmdline flag (see zCore/src/main.rs) until it has
/// equivalent mileage. With the flag off, `SET_CLIENT_CAP(ATOMIC)` is refused
/// with `EOPNOTSUPP` exactly like a Linux driver without `DRIVER_ATOMIC`, and
/// compositors fall back to legacy KMS.
static ATOMIC_ENABLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Enable the atomic uAPI (set once at boot from the `drm.atomic` flag).
pub fn set_atomic_enabled(on: bool) {
    ATOMIC_ENABLED.store(on, Ordering::Relaxed);
}

/// Whether the atomic uAPI is enabled for this boot.
pub fn atomic_enabled() -> bool {
    ATOMIC_ENABLED.load(Ordering::Relaxed)
}

/// Per-open DRM file state (Linux `struct drm_file`).
///
/// `ATOMIC_CLIENT` and the readable event queue belong to the fd that
/// negotiated / queued them. GEM handles stay global for now (full F-M7
/// isolation is a larger change). Each `open(/dev/dri/card*)` gets a fresh
/// [`DrmFileState`] via [`super::drm_scheme::DrmDev::open_client`].
pub struct DrmFileState {
    atomic_client: AtomicBool,
    events: Mutex<VecDeque<Vec<u8>>>,
    eventbus: Arc<Mutex<EventBus>>,
}

impl DrmFileState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            atomic_client: AtomicBool::new(false),
            events: Mutex::new(VecDeque::new()),
            eventbus: EventBus::new(),
        })
    }

    pub fn set_atomic_client(&self, on: bool) {
        self.atomic_client.store(on, Ordering::Relaxed);
    }

    pub fn atomic_client(&self) -> bool {
        self.atomic_client.load(Ordering::Relaxed)
    }

    pub fn eventbus(&self) -> Arc<Mutex<EventBus>> {
        self.eventbus.clone()
    }

    pub fn has_events(&self) -> bool {
        !self.events.lock().is_empty()
    }

    /// Fill `buf` with as many whole pending DRM events as fit, like
    /// `drm_read()`.
    ///
    /// The three outcomes are distinct on purpose. Collapsing "the buffer is
    /// too small for the first event" into "nothing to read" livelocked the
    /// kernel: the caller saw EAGAIN, but the queue was NOT empty so
    /// `Event::READABLE` stayed set, so a blocking reader's wait resolved
    /// instantly, re-read, got EAGAIN again, and spun with no yield point.
    /// Linux puts the event back and returns EINVAL when nothing has been read
    /// yet, and never blocks in that case.
    pub fn read_events(&self, buf: &mut [u8]) -> EventRead {
        let mut events = self.events.lock();
        let mut total = 0usize;
        while let Some(len) = events.front().map(|e| e.len()) {
            if total + len > buf.len() {
                // A buffer that cannot hold even one whole event is the
                // caller's error, not an empty queue.
                if total == 0 {
                    return EventRead::TooSmall;
                }
                break;
            }
            let ev = events.pop_front().expect("front() just returned it");
            buf[total..total + len].copy_from_slice(&ev[..len]);
            total += len;
        }
        if events.is_empty() {
            self.eventbus.lock().clear(Event::READABLE);
        }
        if total == 0 {
            EventRead::Empty
        } else {
            EventRead::Read(total)
        }
    }

    fn push_event(&self, bytes: Vec<u8>) {
        let mut events = self.events.lock();
        events.push_back(bytes);
        self.eventbus.lock().set(Event::READABLE);
    }
}

/// What one `read()` on a DRM fd found. See [`DrmFileState::read_events`].
#[derive(Debug, PartialEq, Eq)]
pub enum EventRead {
    /// Nothing queued: EAGAIN, or block until one arrives.
    Empty,
    /// The buffer cannot hold even the first queued event: EINVAL, as
    /// `drm_read()` answers when it has read nothing yet.
    TooSmall,
    /// This many bytes of whole events were copied.
    Read(usize),
}

impl Drop for DrmFileState {
    fn drop(&mut self) {
        // Linux destroys pending events when the drm_file closes. Timer jobs
        // hold only a Weak to us; drain any that still name this file so a
        // later tick does not clear FLIP_EVENT_PENDING for a stranger's flip.
        cancel_pending_timers_for_file(self as *const DrmFileState);
    }
}

/// Return the primary framebuffer display, if any.
fn primary_display() -> Option<Arc<dyn DisplayScheme>> {
    drivers::all_display().first()
}

/// Whether the software KMS path should drive the output.
///
/// True when a framebuffer display exists but no registered DRM driver declares
/// hardware-KMS scanout support. In that case we keep the software fallback:
/// dumb-buffer blits to the primary framebuffer display (`blit_from`).
pub fn software_kms_active() -> bool {
    let have_display = primary_display().is_some();
    let driver_can_scanout = get_primary_driver()
        .map(|d| d.has_hardware_kms())
        .unwrap_or(false);
    have_display && !driver_can_scanout
}

/// A DRM Framebuffer object
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct DrmFramebuffer {
    pub id: u32,
    /// Optional driver-private framebuffer id returned by `DrmScheme::create_fb`.
    pub driver_fb_id: Option<u32>,
    /// GEM handle that backs this framebuffer
    pub gem_handle_id: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub phys_addr: u64,
    pub size: usize,
    /// The pid that created this framebuffer, or 0 for one the kernel made.
    ///
    /// Linux keeps framebuffers on a per-`drm_file` list and `drm_mode_rmfb`
    /// walks it, answering ENOENT for an id that is not the caller's; the same
    /// list is what `drm_fb_release` cleans up on close. Here they were one
    /// global set with no owner at all, so any process could `RMFB` the
    /// compositor's scanout framebuffer -- after which every `SETCRTC` and
    /// `PAGE_FLIP` on it fails and wlroots retries the modeset forever.
    pub owner: u64,
}

struct DrmState {
    drivers: Vec<Arc<dyn DrmScheme>>,
    next_handle_id: u32,
    next_fb_id: u32,
    /// Live GEM objects: `(handle, backing VMO, owning pid)`.
    ///
    /// The pid is what makes these releasable. Linux frees a file's GEM
    /// objects from the DRM fd's release handler; this list had no owner at
    /// all and was only ever shrunk by an explicit DESTROY_DUMB/GEM_CLOSE
    /// ioctl, so anything that died without issuing one -- a crashed Xorg, a
    /// session torn down by its manager, an fd swept away by execve's CLOEXEC
    /// pass -- leaked its buffers forever. Each is a physically CONTIGUOUS
    /// VMO of up to 64 MiB (`MAX_DUMB_SIZE`), and it belongs to no process's
    /// address space, so per-process accounting cannot see it either.
    handles: Vec<(GemHandle, Arc<VmObject>, u64)>,
    framebuffers: Vec<DrmFramebuffer>,
    /// The backing VMO of every framebuffer built on a dumb buffer, keyed by
    /// fb id. This is the framebuffer's *reference* on the GEM object: Linux
    /// keeps a `drm_gem_object` alive while any framebuffer (or mapping, or
    /// dma-buf) still refers to it, so `GEM_CLOSE`/`DESTROY_DUMB` on a handle
    /// that an fb was built on does not free the pixels. Before this existed
    /// the fb stored only `phys_addr`, and the sequence
    /// CREATE_DUMB -> ADDFB -> SETCRTC -> DESTROY_DUMB left `scanout()` and
    /// every cursor move reading frames that had gone back to the allocator.
    fb_backing: Vec<(u32, Arc<VmObject>)>,
    /// Framebuffer currently bound to the (synthetic) CRTC, reported by GETCRTC.
    crtc_fb: u32,
    /// The last few framebuffer ids that went away, and what took them —
    /// newest last, capped at [`FB_RETIRE_HISTORY`].
    ///
    /// Purely diagnostic, and it answers the one question a
    /// `PresentError::NoSuchFb` on the console cannot answer by itself: did
    /// the client scan out an id it never had, or one the kernel took from it?
    /// Those two have opposite fixes, and only the second is our bug.
    /// `retire_framebuffers_for_handle` and the process-exit sweep both drop a
    /// framebuffer while its owner may still hold the id -- Linux never does,
    /// because there a `drm_framebuffer` holds its own reference on the GEM
    /// object, and a nouveau-backed fb here holds nothing. `RMFB` is recorded
    /// too: "the client removed it itself and then presented it" is a real
    /// answer, and a different one.
    fb_retirements: VecDeque<(u32, FbRetired)>,
    /// The VT the compositor owns the display on, established on its first
    /// present. While the active VT differs (the user switched to a text
    /// console with Ctrl+Alt+Fn), the compositor's blits are suppressed so the
    /// text console stays visible, and its input is paused. The kernel does
    /// emit the Linux `VT_PROCESS` relsig/acqsig handshake (see
    /// `stdio::request_vt_switch`), but this suppression is the safety net that
    /// keeps VT switching working even when a compositor ignores the handshake
    /// or hangs. Cleared on DROP_MASTER (compositor exit) so a later text-only
    /// session is never gated.
    graphics_vt: Option<usize>,
    /// Monotonic time the next synthetic vblank / page-flip completion is
    /// allowed to fire. A software framebuffer has no real vblank, so a
    /// `DRM_IOCTL_MODE_PAGE_FLIP` used to complete *instantly*: the compositor's
    /// `flip -> poll -> read event -> render -> flip` loop then never blocked
    /// and re-rendered (and full-screen-blitted) as fast as the CPU allowed —
    /// pegging a host core under QEMU. Deferring the flip-complete event to this
    /// deadline paces the loop to ~60 Hz, which is what a real vblank would do.
    ///
    /// Pending readable DRM events live on [`DrmFileState`] (per open), not here.
    next_vblank: Duration,
    /// Kernel-composited hardware cursor (legacy `DRM_IOCTL_MODE_CURSOR`). The
    /// bitmap is a copy of the client's cursor BO (premultiplied ARGB8888,
    /// `w`x`h`), drawn on top of every scanned-out frame at `(x, y)`.
    cursor: CursorState,
    /// KMS property blobs (`CREATEPROPBLOB`/`GETPROPBLOB`), both user-created
    /// (e.g. the `MODE_ID` blob an atomic compositor uploads) and
    /// kernel-created (the current-mode blob echoed via OBJ_GETPROPERTIES).
    blobs: Vec<DrmBlob>,
    /// Next blob id. Starts high above the synthetic KMS ids, fb ids and the
    /// EDID blob ids (20000+connector) so the object-id namespaces never
    /// collide — libdrm identifies blobs purely by id.
    next_blob_id: u32,
    /// Software-KMS state mirrored back to atomic clients (see
    /// [`AtomicKmsState`]).
    atomic: AtomicKmsState,
}

/// A KMS property blob (`struct drm_property_blob`).
struct DrmBlob {
    id: u32,
    /// Whether userspace created it (`CREATEPROPBLOB`). Kernel-created blobs
    /// (current mode, EDID) refuse `DESTROYPROPBLOB` with EPERM like Linux.
    user_created: bool,
    data: Vec<u8>,
}

/// Last-committed atomic state of the synthetic pipeline, echoed back through
/// `OBJ_GETPROPERTIES` so an atomic compositor's state readback matches what
/// it committed. The software scanout itself only consumes the framebuffer id
/// (full-screen blit); the rects are bookkeeping for the uAPI contract.
#[derive(Default, Clone, Copy)]
pub struct AtomicKmsState {
    /// CRTC "ACTIVE" property.
    pub active: bool,
    /// Kernel-owned blob id holding the current mode (0 = none committed).
    pub mode_blob_id: u32,
    /// Plane "CRTC_X"/"CRTC_Y" (output position).
    pub crtc_x: i32,
    /// See [`Self::crtc_x`].
    pub crtc_y: i32,
    /// Plane "CRTC_W"/"CRTC_H" (output size).
    pub crtc_w: u32,
    /// See [`Self::crtc_w`].
    pub crtc_h: u32,
    /// Plane "SRC_X".."SRC_H" in 16.16 fixed point.
    pub src_x: u32,
    /// See [`Self::src_x`].
    pub src_y: u32,
    /// See [`Self::src_x`].
    pub src_w: u32,
    /// See [`Self::src_x`].
    pub src_h: u32,
}

/// State for the kernel-composited cursor. wlroots (forced to legacy KMS) sets
/// the pointer image with `DRM_MODE_CURSOR_BO` and moves it with
/// `DRM_MODE_CURSOR_MOVE`; `scanout()` composites it over the frame.
#[derive(Default)]
struct CursorState {
    visible: bool,
    /// Top-left position of the cursor bitmap in output pixels (already
    /// hotspot-adjusted by the compositor for the legacy path).
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    /// Client cursor pixels (premultiplied ARGB8888, tightly packed). Held in
    /// an `Arc` so every `scanout()` / cursor move can share the pixels without
    /// cloning the Vec — a resize/redraw storm was cloning this on every flip
    /// and amplifying heap churn for ~30–50 min until a null fn-ptr #PF.
    bitmap: Option<Arc<[u32]>>,
    /// The rectangle `(x, y, w, h)` currently composited on the display, or
    /// `None` if nothing is drawn. A cursor move restores exactly this rect from
    /// the CRTC framebuffer before compositing the new one — so a move touches
    /// two ~64x64 windows instead of re-blitting the whole ~16 MB frame. This is
    /// what makes the pointer cheap enough to feel like a hardware cursor.
    drawn: Option<(i32, i32, u32, u32)>,
    /// The REAL display-engine cursor plane owns the pointer (the driver's
    /// `hw_cursor_set` accepted the image). While set, the kernel's software
    /// compositing stands down completely: `scanout()` does not blend the
    /// bitmap and `repaint_for_cursor()` is a no-op — the display hardware
    /// composites the plane during scanout and a move is one PIO write in the
    /// driver. Opt-in via the `nvidia.hwcursor` kernel cmdline flag; any
    /// driver failure falls back to the software path with `hw == false`.
    hw: bool,
}

lazy_static::lazy_static! {
    /// Reused compose buffer for software-cursor patches (avoids a heap alloc
    /// on every pointer motion).
    static ref CURSOR_PATCH: Mutex<Vec<u32>> = Mutex::new(Vec::new());
    static ref DRM_STATE: Mutex<DrmState> = Mutex::new(DrmState {
        drivers: Vec::new(),
        next_handle_id: 1,
        next_fb_id: 1,
        handles: Vec::new(),
        framebuffers: Vec::new(),
        fb_backing: Vec::new(),
        crtc_fb: 0,
        fb_retirements: VecDeque::new(),
        graphics_vt: None,
        next_vblank: Duration::ZERO,
        cursor: CursorState {
            visible: false,
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            bitmap: None,
            drawn: None,
            hw: false,
        },
        blobs: Vec::new(),
        next_blob_id: 30000,
        atomic: AtomicKmsState::default(),
    });
    /// Shared CPU-mmap VMOs for nouveau-uAPI GEM handles. Without this,
    /// every `get_vmo`/`export_handle` built a fresh `new_physical` over the
    /// same frames — GEM_CLOSE could free while an older Arc still mapped
    /// them. One Arc per handle, cloned to every mmap, matches dumb-buffer
    /// semantics (`handle_vmo`).
    static ref NOUVEAU_CPU_VMOS: Mutex<alloc::collections::BTreeMap<u32, Arc<VmObject>>> =
        Mutex::new(alloc::collections::BTreeMap::new());
}

/// `nvidia.gem_uc` on the cmdline: keep the old uncached mapping for nouveau
/// GEM objects. Escape hatch for [`nouveau_cpu_vmo`] -- see the note there on
/// why WB is the coherent choice on x86 and what would falsify it.
static GEM_MAP_UNCACHED: AtomicBool = AtomicBool::new(false);

/// Read the `nvidia.gem_uc` cmdline token once and latch it.
pub fn init_gem_cache_policy() {
    let uc = kernel_hal::boot::cmdline()
        .split([':', ' ', '\t', '\n'])
        .any(|t| t == "nvidia.gem_uc");
    GEM_MAP_UNCACHED.store(uc, Ordering::Relaxed);
    if uc {
        kernel_hal::klog_info!(
            "[drm] nvidia.gem_uc -- nouveau GEM CPU mappings forced UNCACHED (slow; diagnostic only)"
        );
    }
}

/// Shared physical VMO for a nouveau GEM handle's CPU mmap / PRIME export.
///
/// The mapping is **cached (WB)**, not uncached. `VmObject::new_physical`
/// starts every physical VMO at `CachePolicy::Uncached`
/// (`zircon-object/src/vm/vmo/physical.rs`), and this path never overrode it,
/// so every CPU touch of an NVK/zink staging buffer, pushbuffer or swapchain
/// image was a serialized UC transaction -- roughly eight bytes per round
/// trip to RAM, with no combining and no caching.
///
/// Uncached was never right here, because **only sysmem reaches this
/// function**: `GEM_NEW` publishes a `phys_addr` exclusively when
/// `gem_map_cpu` reports `ADDR_SYSMEM`, and refuses to publish one for an
/// `ADDR_FBMEM` (VRAM) object at all, since a VRAM offset is not a host
/// physical address. So every handle that gets here is ordinary host RAM
/// behind GART -- the same class of memory the dumb-buffer path already
/// maps `Cached` for exactly this reason (see `handle_vmo` below: "the
/// compositor no longer renders into UC memory on real hardware").
///
/// WB also *removes* an aliasing hazard rather than adding one: the kernel
/// already reaches these same frames through the WB physmap alias (that is
/// what `dma_sync_scanout_src_from_device`'s clflush is maintaining), so a UC
/// user mapping and a WB kernel mapping of one page were conflicting memory
/// types. Now both ends agree.
///
/// Coherence against GPU DMA rests on x86 PCIe reads being snooped, which is
/// the architectural default and what Linux's nouveau relies on for GART
/// objects. The one thing that would falsify it is the GPU issuing No-Snoop
/// TLPs for these reads; the symptom would be the GPU consuming stale bytes
/// (garbage or a frame behind) rather than anything crashing. `nvidia.gem_uc`
/// restores the old behaviour in place for a boot, so that hypothesis can be
/// tested on hardware without a rebuild.
pub fn nouveau_cpu_vmo(handle: u32, phys_addr: u64, size: usize) -> Arc<VmObject> {
    let mut map = NOUVEAU_CPU_VMOS.lock();
    if let Some(v) = map.get(&handle) {
        return v.clone();
    }
    let vmo = VmObject::new_physical(phys_addr as usize, pages(size));
    if !GEM_MAP_UNCACHED.load(Ordering::Relaxed) {
        // Must happen before the VMO is handed out: `set_cache_policy`
        // refuses once a mapping exists. Nothing can have mapped it yet --
        // it was constructed on the line above and is still unpublished.
        if let Err(e) = vmo.set_cache_policy(kernel_hal::CachePolicy::Cached) {
            static CACHE_POLICY_FAILED: AtomicBool = AtomicBool::new(false);
            if !CACHE_POLICY_FAILED.swap(true, Ordering::Relaxed) {
                kernel_hal::klog_warn!(
                    "[drm] nouveau GEM mmap: set_cache_policy(Cached) failed ({:?}) -- \
                     falling back to UNCACHED; CPU access to GEM objects will be slow",
                    e
                );
            }
        }
    }
    map.insert(handle, vmo.clone());
    vmo
}

/// Drop the cached CPU VMO for a handle (best-effort; live Arc clones keep the pin).
pub fn nouveau_cpu_vmo_forget(handle: u32) {
    NOUVEAU_CPU_VMOS.lock().remove(&handle);
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

/// Base minor of the render-node range, as Linux allocates them: primary
/// nodes are `card0..card63`, render nodes `renderD128..renderD191`.
pub const RENDER_MINOR_BASE: u32 = 128;
/// How many GPUs get a node pair. This is Linux's primary-minor range
/// (`card0..card63`), which is stricter than the point where the two ranges
/// would actually collide (`card128` would be `renderD128`'s minor). Staying
/// inside the convention keeps `/dev/dri` names meaning to userspace what they
/// mean on Linux, and leaves the render range untouched with 64 to spare.
pub const MAX_GPU_NODES: u32 = 64;

/// The node name Linux would give this minor: `card{n}` below the render base,
/// `renderD{n}` at or above it.
///
/// Derived, not a lookup table. The four names this used to `match` on were
/// the whole vocabulary — a third GPU's node came out as the literal `card?`.
pub fn node_name(minor: u32) -> alloc::string::String {
    if minor >= RENDER_MINOR_BASE {
        alloc::format!("renderD{}", minor)
    } else {
        alloc::format!("card{}", minor)
    }
}

/// Which pair of `/dev/dri` minors belongs to the GPU at a given index.
///
/// Separate from [`GpuNode`] because the layout is a property of the index
/// alone: sysfs asks "what minor would GPU 2 have" without holding a driver,
/// and the arithmetic is testable on its own.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NodeMinors {
    index: u32,
}

impl NodeMinors {
    /// The node pair for the GPU at `index` (0 = the primary GPU).
    pub const fn new(index: u32) -> Self {
        Self { index }
    }
    /// Minor of the primary node (`card{index}`).
    pub const fn card(&self) -> u32 {
        self.index
    }
    /// Minor of the render node (`renderD{128 + index}`).
    pub const fn render(&self) -> u32 {
        RENDER_MINOR_BASE + self.index
    }
    /// True when `minor` is either of this pair.
    pub const fn owns(&self, minor: u32) -> bool {
        minor == self.card() || minor == self.render()
    }
}

/// One GPU and the pair of `/dev/dri` nodes that belong to it.
///
/// Index 0 is the GPU behind `card0` / `renderD128`: the one that scans out
/// the console, which is what every KMS client must land on. Index 1 and up
/// are the compute GPUs, in a **stable** order (see [`build_gpu_nodes`]), so
/// `card1` names the same physical card across reboots.
#[derive(Clone)]
pub struct GpuNode {
    /// 0 for the console/primary GPU, 1.. for each compute GPU.
    pub index: u32,
    /// The driver that serves both of this GPU's nodes.
    pub driver: Arc<dyn DrmScheme>,
}

impl GpuNode {
    /// The minors this GPU's two nodes use.
    pub fn minors(&self) -> NodeMinors {
        NodeMinors::new(self.index)
    }
    /// Minor of this GPU's primary node (`card{n}`).
    pub fn card_minor(&self) -> u32 {
        self.minors().card()
    }
    /// Minor of this GPU's render node (`renderD{128 + n}`).
    pub fn render_minor(&self) -> u32 {
        self.minors().render()
    }
    /// True when `minor` is either of this GPU's two nodes.
    pub fn owns_minor(&self, minor: u32) -> bool {
        self.minors().owns(minor)
    }
}

static GPU_NODES: Mutex<Vec<GpuNode>> = Mutex::new(Vec::new());

/// Work out which GPU owns which `/dev/dri` node, once, at filesystem setup.
///
/// The order is deliberate, because it is what userspace sees, and the first
/// two entries are deliberately **exactly what this box already had** before
/// the table existed:
///
/// * Index 0 is [`get_primary_driver`] -- the node every KMS client opens.
///   Note this is NOT necessarily the console GPU: `register_driver` does
///   `insert(0)`, so on a dual-card box the primary is whichever card was
///   probed last, which is the compute one. Pointing `card0` at the console
///   GPU instead would be a policy change, and a breaking one today: the
///   console GPU is cold unless `nvidia.console_gpu` brought it up, so every
///   nouveau ioctl on `card0` would start answering `ENODEV`.
/// * Index 1 is [`get_compute_driver`], honouring an explicit
///   `nvidia.compute=BB.DD.F` pin. On a two-card box that is the same driver
///   as index 0, which is exactly the situation today: `card1` is a headless
///   compute view of the same card, and `/sys/class/drm` gives it a distinct
///   fake BDF so libdrm does not merge the two node pairs.
/// * Indices 2.. are the remaining compute GPUs **sorted by PCI BDF**. Sorting
///   rather than taking registration order is the point: that list is the
///   reverse of the PCI probe order, so which card became `card2` would be an
///   accident that could differ between boots. A BDF sort is stable, so a node
///   names the same card every time.
///
/// Only the first [`MAX_GPU_NODES`] GPUs get nodes: past that, `card{n}` would
/// collide with the render range and two GPUs would answer to one minor.
pub fn build_gpu_nodes() {
    let Some(primary) = get_primary_driver() else {
        *GPU_NODES.lock() = Vec::new();
        return;
    };
    let mut nodes = vec![GpuNode {
        index: 0,
        driver: primary,
    }];

    if let Some(compute) = get_compute_driver() {
        nodes.push(GpuNode {
            index: 1,
            driver: compute,
        });
    }

    // Everything else that can compute and is not already spoken for.
    let drivers = kernel_hal::drivers::all_drm();
    let list = drivers.as_vec();
    let mut rest: Vec<Arc<dyn DrmScheme>> = list
        .iter()
        .filter(|d| d.is_compute_gpu() && !nodes.iter().any(|n| Arc::ptr_eq(&n.driver, d)))
        .cloned()
        .collect();
    rest.sort_by_key(|d| d.pci_bdf());

    for driver in rest {
        let index = nodes.len() as u32;
        if index >= MAX_GPU_NODES {
            kernel_hal::klog_warn!(
                "[drm] more than {} GPUs registered -- the extra ones get no /dev/dri node \
                 (card{} would collide with the renderD range)",
                MAX_GPU_NODES,
                index
            );
            break;
        }
        nodes.push(GpuNode { index, driver });
    }

    for n in &nodes {
        kernel_hal::klog_info!(
            "[drm] /dev/dri/{} + /dev/dri/{} -> {:?} pci_bdf={:x?} console={}",
            node_name(n.card_minor()),
            node_name(n.render_minor()),
            n.driver.name(),
            n.driver.pci_bdf(),
            n.driver.is_console_gpu(),
        );
    }
    *GPU_NODES.lock() = nodes;
}

/// Every GPU that has a `/dev/dri` node pair, index order.
pub fn gpu_nodes() -> Vec<GpuNode> {
    GPU_NODES.lock().clone()
}

/// The driver that owns `minor`, or `None` when no node has that minor.
///
/// This is what makes a node mean a GPU. Before the table existed every ioctl
/// on every node went to `get_primary_driver()` regardless of which node it
/// arrived on, so `card1` was the compute GPU only in its sysfs identity and
/// in `ECLIPSE_COMPUTE` -- every nouveau ioctl on it was served by a different
/// card than the one userspace had identified.
pub fn driver_for_minor(minor: u32) -> Option<Arc<dyn DrmScheme>> {
    GPU_NODES
        .lock()
        .iter()
        .find(|n| n.owns_minor(minor))
        .map(|n| n.driver.clone())
}

/// Whether `minor` is one of the compute-only nodes (index 1 and up). Those
/// advertise themselves as `eclipse-compute` and report no CRTCs, so Mesa and
/// wlroots leave them alone and stay on `card0`.
pub fn is_compute_minor(minor: u32) -> bool {
    GPU_NODES
        .lock()
        .iter()
        .any(|n| n.index > 0 && n.owns_minor(minor))
}

/// `nvidia.compute=BB.DD.F` on the kernel cmdline (hex, dots — the cmdline
/// already uses `:` as its token separator, so a PCI BDF with colons cannot
/// be a single token). Example: `nvidia.compute=65.00.0`.
fn parse_nvidia_compute_bdf() -> Option<(u8, u8, u8)> {
    let cmdline = kernel_hal::boot::cmdline();
    for tok in cmdline.split([':', ' ', '\t', '\n']) {
        let Some(rest) = tok.strip_prefix("nvidia.compute=") else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        let mut parts = rest.split('.');
        let bus = u8::from_str_radix(parts.next()?, 16).ok()?;
        let dev = u8::from_str_radix(parts.next()?, 16).ok()?;
        let func = parts
            .next()
            .and_then(|p| u8::from_str_radix(p, 16).ok())
            .unwrap_or(0);
        return Some((bus, dev, func));
    }
    None
}

/// The NVIDIA GPU that owns compute (SAXPY, NVK EXEC, CE-present): either
/// the `nvidia.compute=BB.DD.F` pin, or the first driver with
/// [`DrmScheme::is_compute_gpu`]. Never the console GPU — its GSP resume
/// can wedge the bus.
pub fn get_compute_driver() -> Option<Arc<dyn DrmScheme>> {
    let drivers = kernel_hal::drivers::all_drm();
    let list = drivers.as_vec();
    if let Some((bus, dev, func)) = parse_nvidia_compute_bdf() {
        if let Some(d) = list.iter().find(|d| {
            matches!(
                d.pci_bdf(),
                Some((_, b, dv, f)) if b == bus && dv == dev && f == func
            )
        }) {
            if d.is_console_gpu() {
                kernel_hal::klog_info!(
                    "[drm] nvidia.compute={:02x}.{:02x}.{:x} is the console GPU — ignoring pin \
                     (GSP on the GOP card can wedge the bus)",
                    bus,
                    dev,
                    func
                );
            } else {
                return Some(d.clone());
            }
        } else {
            kernel_hal::klog_info!(
                "[drm] nvidia.compute={:02x}.{:02x}.{:x} does not match any DRM GPU",
                bus,
                dev,
                func
            );
        }
    }
    list.iter().find(|d| d.is_compute_gpu()).cloned()
}

/// Hard ceiling on live GEM objects. Contiguous dumb buffers are expensive
/// (up to `MAX_DUMB_SIZE` each) and sit outside process VMAs. Under a QEMU
/// window-resize / redraw storm the compositor can CREATE_DUMB faster than
/// it DESTROYs; without a cap, frame-allocator pressure eventually smashes
/// heap metadata and shows up as a null fn-ptr #PF after tens of minutes.
const MAX_LIVE_GEMS: usize = 64;

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
        let live = state.handles.len();
        if live >= MAX_LIVE_GEMS {
            log::error!(
                "[drm] alloc_buffer: {} live GEMs (>= {}), refusing CREATE_DUMB (size={})",
                live,
                MAX_LIVE_GEMS,
                size
            );
            return None;
        }
        if live >= MAX_LIVE_GEMS / 2 {
            log::warn!(
                "[drm] alloc_buffer: elevated GEM pressure ({} live, cap {})",
                live,
                MAX_LIVE_GEMS
            );
        }
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
        DRM_STATE.lock().handles.push((handle, vmo, current_pid()));
        Some(handle)
    } else {
        None
    }
}

/// Export a GEM handle for PRIME: return its `(phys_addr, size, backing VMO)`
/// so it can be wrapped in a dma-buf and shared with another DRM node.
pub fn export_handle(handle_id: u32) -> Option<(u64, usize, Arc<VmObject>)> {
    // PRIME export is a uAPI entry point, so it resolves the handle the way
    // Linux's `drm_gem_object_lookup(file_priv, handle)` does -- against what
    // the CALLER may touch. Without this any process could hand
    // `PRIME_HANDLE_TO_FD` a small integer, get a dma-buf fd over someone
    // else's buffer and mmap it: handle ids are sequential from 1, so reading
    // the compositor's scanout took no guessing at all.
    let pid = current_pid();
    {
        let state = DRM_STATE.lock();
        if let Some(v) = state
            .handles
            .iter()
            .find(|(h, _, owner)| h.id == handle_id && owned_by(*owner, pid))
            .map(|(h, vmo, _)| (h.phys_addr, h.size, vmo.clone()))
        {
            return Some(v);
        }
    }
    // Driver-private GEM object (nouveau-uAPI GEM_NEW): same fallback the
    // mmap path (`DrmDev::get_vmo`) already uses. Without it, NVK's
    // `vkGetMemoryFdKHR` -> `drmPrimeHandleToFD` failed EINVAL for every
    // handle it ever allocated, wlroots' GBM allocator could not export a
    // single swapchain buffer (`gbm_bo_get_fd_for_plane failed`), and the
    // compositor died at "Swapchain for output ... failed test" -- AFTER
    // rendering itself already worked. The physical range registered at
    // GEM_NEW time is exactly what a dma-buf needs.
    // Same ownership rule on the driver-private side: `lookup_for` is
    // `lookup` plus `holds(handle, pid)`, which is the nouveau table's own
    // record of who took a reference.
    let (phys_addr, size) = zcore_drivers::scheme::gem_mmap::lookup_for(handle_id, pid)?;
    let vmo = nouveau_cpu_vmo(handle_id, phys_addr, size as usize);
    Some((phys_addr, size as usize, vmo))
}

/// Reverse lookup for PRIME self-import: the nouveau-uAPI GEM handle whose
/// backing starts at `phys_addr`, if any. Thin re-export of
/// `gem_mmap::lookup_by_phys` so `linux-syscall` (which has no
/// `zcore-drivers` dependency of its own) can resolve a dma-buf back to the
/// original driver-private handle -- see `sys_drm_prime`'s import arm.
pub fn nouveau_handle_for_phys(phys_addr: u64) -> Option<u32> {
    zcore_drivers::scheme::gem_mmap::lookup_by_phys(phys_addr).map(|(handle, _)| handle)
}

/// Take a PRIME reference on a driver-private GEM handle resolved by a
/// self-import (see `sys_drm_prime`). Thin re-export of `gem_mmap::add_ref` so
/// `linux-syscall` can keep the shared buffer alive: the importer (NVK/EGL) will
/// `GEM_CLOSE` this same handle when it is done, and without this bump that
/// close would free the buffer while the exporter (wlroots) still owns it.
/// Returns the new share count, or `None` if the handle is not tracked.
pub fn nouveau_gem_add_ref(handle: u32) -> Option<u32> {
    zcore_drivers::scheme::gem_mmap::add_ref(handle, current_pid())
}

/// The holder id a KMS framebuffer's own reference on a nouveau GEM object is
/// recorded under.
///
/// `gem_mmap` attributes every reference to a pid, so that a process exit can
/// drop exactly what that process held and nothing else. A framebuffer is not
/// a process: it outlives the `GEM_CLOSE` that drops the client's handle, and
/// it must survive the owner's exit sweep for as long as the fb object itself
/// exists. `u64::MAX` is never a real pid, so this entry belongs to no
/// process, is never taken by `release_pid`, and is dropped only when the
/// framebuffer is.
const KMS_FB_HOLDER: u64 = u64::MAX;

/// Take the framebuffer's reference on a nouveau GEM object.
///
/// This is Linux's contract, and the bug it fixes is the whole reason the
/// desktop never came up on the GL/Vulkan path. wlroots creates a scanout
/// buffer with `GEM_NEW`, calls `ADDFB2` on the handle, and then CLOSES the
/// handle immediately -- the normal, required dance, because on Linux a
/// `drm_framebuffer` holds its own reference on the GEM object and the buffer
/// stays alive until `RMFB`. Here nothing held that reference: the close was
/// the last one, `nouveau_gem_close` handed the VRAM back to the RM, and
/// `retire_framebuffers_for_handle` retired the framebuffer that had just been
/// created. Every `SETCRTC` on it then answered `ENOENT` -- the
/// "connector HDMI-A-1: Failed to set CRTC: No such file or directory" storm,
/// at frame rate, for the life of the session.
///
/// A dumb buffer has held this reference all along, as an `Arc<VmObject>` in
/// `fb_backing`; a nouveau GEM object has no `VmObject` to take one on, so the
/// reference goes in `gem_mmap` instead. Low-range (dumb/generic) handles are
/// not tracked there and are left alone.
fn fb_take_gem_ref(handle_id: u32) {
    if handle_id < zcore_drivers::scheme::gem_mmap::DRIVER_HANDLE_BASE {
        return;
    }
    zcore_drivers::scheme::gem_mmap::add_ref(handle_id, KMS_FB_HOLDER);
}

/// Release the reference [`fb_take_gem_ref`] took, freeing the GEM object when
/// it was the last one.
///
/// Routed through the driver's own `GEM_CLOSE` rather than a bare `dec_ref`,
/// because dropping the LAST reference has to do everything a client's close
/// does -- drain the VM_BIND mappings and give the memory back to the RM --
/// not merely forget the mapping. When other holders remain (the client still
/// has its handle open) nothing is freed and this is just one holder letting
/// go.
fn fb_drop_gem_ref(handle_id: u32) {
    if handle_id < zcore_drivers::scheme::gem_mmap::DRIVER_HANDLE_BASE {
        return;
    }
    if zcore_drivers::scheme::gem_mmap::lookup(handle_id).is_none() {
        // Already gone (the object was freed under us and the fb is being
        // retired in the same breath -- see `retire_framebuffers_for_handle`).
        return;
    }
    match get_primary_driver() {
        Some(driver) => {
            driver.nouveau_gem_close(handle_id, KMS_FB_HOLDER);
        }
        // No driver to hand the memory back to. Still give up the reference,
        // or the table keeps a holder that nothing can ever release and the
        // object stays "alive" forever.
        None => {
            zcore_drivers::scheme::gem_mmap::dec_ref(handle_id, KMS_FB_HOLDER);
        }
    }
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
    state.handles.push((handle, vmo, current_pid()));
    id
}

pub fn get_handle(handle_id: u32) -> Option<GemHandle> {
    DRM_STATE
        .lock()
        .handles
        .iter()
        .find(|(h, _, _)| h.id == handle_id)
        .map(|(h, _, _)| *h)
}

/// The owning VMO of a dumb buffer, for `mmap` of its `MAP_DUMB` offset.
///
/// The mapping must share THIS object, not a fresh `VmObject::new_physical`
/// over the same frames: a physical VMO owns nothing, so a client that
/// `DESTROY_DUMB`ed a buffer it still had mapped kept writing through a
/// mapping whose frames had already been handed to the next allocation. With
/// the mapping holding the `Arc`, the frames live until the last mapping goes
/// -- exactly `drm_gem_object`'s refcount in Linux. The contiguous VMO is
/// also `Cached`, where the physical one defaulted to `Uncached`, so the
/// compositor no longer renders into UC memory on real hardware.
pub fn handle_vmo(handle_id: u32) -> Option<Arc<VmObject>> {
    let pid = current_pid();
    DRM_STATE
        .lock()
        .handles
        .iter()
        .find(|(h, _, owner)| h.id == handle_id && owned_by(*owner, pid))
        .map(|(_, vmo, _)| vmo.clone())
}

/// Whether a process may act on a GEM object owned by `owner`: its own, an
/// unowned (boot-time) one, or when there is no current thread. Handles are
/// per-file in Linux; this global table cannot do better than per-pid, but
/// without it any process could `mmap` or `GEM_CLOSE` another's buffers by
/// guessing a (small, sequential) handle.
fn owned_by(owner: u64, pid: u64) -> bool {
    pid == 0 || owner == 0 || owner == pid
}

/// Whether the calling process created `fb`, i.e. whether `GETFB`/`GETFB2`
/// may hand it the backing GEM handle.
///
/// Linux gates that field on DRM master or `CAP_SYS_ADMIN` and zeroes it
/// otherwise. There is no master state here, so the framebuffer's creator is
/// the closest honest stand-in: the compositor still gets the handles for its
/// own framebuffers, and nobody else gets a route to them.
pub fn fb_owned_by_caller(fb: &DrmFramebuffer) -> bool {
    owned_by(fb.owner, current_pid())
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

/// Resolve a GEM handle to its backing `(phys_addr, size)`, from EITHER
/// buffer table — the one canonical place that knows both:
///   * `state.handles` — generic DRM dumb buffers (`CREATE_DUMB`, the
///     pixman/software path), and
///   * `gem_mmap` — nouveau-uAPI `GEM_NEW` objects (high-range handles), what
///     the GL/nouveau renderer scans out.
///
/// The present-path consumers of a buffer's physical backing go through here —
/// `create_fb`/ADDFB2, `scanout` (via the fb it stamped), and the cursor
/// upload. `get_vmo` (mmap) and `export_handle` (PRIME) do the same two-table
/// lookup, but `export_handle` must keep the generic path's ORIGINAL `VmObject`
/// alive (not a fresh `new_physical`), so it can't fold into a `(phys, size)`
/// return — left as its own thing. Before this existed the lookup was
/// copy-pasted at each site with subtly different guards, and several swallowed
/// a miss silently (`?` / a bare `return false`) — so a buffer the present path
/// could not read left no trace. One resolver, one set of guards, one place to
/// log a miss. Must be called WITHOUT `DRM_STATE` held.
pub fn resolve_gem_backing(handle_id: u32) -> Option<(u64, usize)> {
    if let Some(h) = get_handle(handle_id) {
        return Some((h.phys_addr, h.size));
    }
    zcore_drivers::scheme::gem_mmap::lookup(handle_id).map(|(pa, sz)| (pa, sz as usize))
}

/// [`resolve_gem_backing`] restricted to what `pid` may touch.
///
/// The unchecked resolver is right for the kernel's own present path, which
/// already holds the framebuffer; it is wrong for `ADDFB`/`ADDFB2`, which take
/// a handle straight from userspace. Linux resolves those through
/// `drm_gem_object_lookup(file, handle)` and answers ENOENT for a handle that
/// is not the caller's. Here it meant process B could build its own
/// framebuffer over process A's buffer -- and then `SETCRTC` or `PAGE_FLIP`
/// it onto the panel, or have A's pixels blitted somewhere B could read.
pub fn resolve_gem_backing_for(handle_id: u32, pid: u64) -> Option<(u64, usize)> {
    if let Some((h, _, owner)) = DRM_STATE
        .lock()
        .handles
        .iter()
        .find(|(h, _, _)| h.id == handle_id)
        .map(|(h, vmo, owner)| (*h, vmo.clone(), *owner))
    {
        return owned_by(owner, pid).then_some((h.phys_addr, h.size));
    }
    zcore_drivers::scheme::gem_mmap::lookup_for(handle_id, pid).map(|(pa, sz)| (pa, sz as usize))
}

/// Create a framebuffer from a GEM handle
pub fn create_fb(handle_id: u32, width: u32, height: u32, pitch: u32) -> Option<u32> {
    // Resolve the backing buffer from EITHER source:
    //  - a DRM dumb buffer in our own handle table (CREATE_DUMB / pixman), or
    //  - a nouveau-uAPI GEM object (GEM_NEW), whose high-range handle lives in
    //    `gem_mmap`, not here.
    // wlroots' GL/Vulkan renderer scans out GBM buffers backed by the latter,
    // so ADDFB2 MUST accept those handles or the output swapchain test fails
    // ("create_fb returned None") before any atomic commit — the exact RTX
    // bring-up blocker seen as "Swapchain for output 'HDMI-A-1' failed test".
    let (phys_addr, buf_size) = match resolve_gem_backing_for(handle_id, current_pid()) {
        Some(v) => v,
        None => {
            // Loud, not a silent `?`: an unresolvable ADDFB2 handle is exactly
            // "the swapchain buffer has no backing the present path can read",
            // and it used to fail with no kernel-side line at all.
            warn!(
                "[drm] create_fb (ADDFB2): handle={:#x} not in dumb table nor nouveau GEM \
                 -- cannot back a framebuffer (the output's present will fail)",
                handle_id
            );
            return None;
        }
    };

    // The framebuffer must fit within the backing buffer: `scanout()` maps
    // `size` bytes from the buffer's contiguous phys range and blits them, so a
    // fb larger than its buffer would read past the VMO into adjacent physical
    // RAM (info leak / fault). Compute in usize with a checked multiply and
    // reject ADDFB whose dimensions overflow or exceed the buffer.
    let size = (pitch as usize).checked_mul(height as usize)?;
    if size == 0 || size > buf_size || (pitch as usize) < (width as usize).saturating_mul(4) {
        return None;
    }

    // The pitch must be a whole number of XRGB8888 pixels. Every consumer of
    // `fb.pitch` on the CPU present path divides it by 4 to get a row stride in
    // pixels (`src_stride` in `scanout_region` and in `repaint_for_cursor`), so
    // a pitch of, say, `width * 4 + 2` makes each successive row start two
    // bytes early -- a diagonal shear that grows down the screen. The copy
    // engine does NOT truncate (`ce_present_2d_pitched` takes the raw pitch),
    // so the two present paths would also disagree about the same image.
    // Linux rejects this in `drm_mode_addfb2` through the format's cpp
    // alignment; here the format is always 4 bytes per pixel.
    if !pitch.is_multiple_of(4) {
        warn!(
            "[drm] create_fb (ADDFB2): pitch={} is not a multiple of 4 bytes              (XRGB8888) -- rejecting rather than scanning out a sheared image",
            pitch
        );
        return None;
    }

    // Create a driver-private fb when hardware/CE/surface flip may need it.
    // Under pure software KMS (no surfaceflip) skip it to avoid leaking
    // driver fbs with no destroy path.
    let want_driver_fb = !software_kms_active()
        || zcore_drivers::display::surfaceflip_enabled()
        || zcore_drivers::display::hwflip_enabled();
    let driver_fb_id = if want_driver_fb {
        get_primary_driver().and_then(|driver| driver.create_fb(handle_id, width, height, pitch))
    } else {
        None
    };

    // Before `DRM_STATE`: this takes `gem_mmap`'s lock, and no other path
    // nests the two in this order.
    fb_take_gem_ref(handle_id);

    let mut state = DRM_STATE.lock();
    let fb_id = state.next_fb_id;
    state.next_fb_id += 1;

    let fb = DrmFramebuffer {
        id: fb_id,
        driver_fb_id,
        gem_handle_id: handle_id,
        width,
        height,
        pitch,
        phys_addr,
        size,
        owner: current_pid(),
    };

    // A dumb buffer's reference is its VMO `Arc` (see `fb_backing`); a nouveau
    // GEM object's is the `gem_mmap` holder taken just above. Either way the
    // framebuffer now owns one, exactly as a `drm_framebuffer` does.
    let backing = state
        .handles
        .iter()
        .find(|(h, _, _)| h.id == handle_id)
        .map(|(_, vmo, _)| vmo.clone());
    if let Some(vmo) = backing {
        state.fb_backing.push((fb_id, vmo));
    }
    state.framebuffers.push(fb);
    Some(fb_id)
}

/// Remove a framebuffer (DRM_IOCTL_MODE_RMFB / DRM_IOCTL_MODE_CLOSEFB).
///
/// Drops the fb object and the reference it held on its dumb buffer (see
/// `fb_backing`). If the fb was the one bound to the CRTC, the CRTC simply
/// stops naming it: the software scanout keeps showing the last blitted
/// frame until the next present, which is CLOSEFB's "close without
/// disabling" contract and harmless for RMFB.
///
/// This used to defer the removal while a page-flip completion was pending
/// and, for the CRTC fb, push a hand-made `DRM_EVENT_FLIP_COMPLETE` with
/// `user_data = 0` -- while the real completion stayed queued. Two events for
/// one flip, and wlroots dereferences `user_data` as its `wlr_drm_page_flip`,
/// so the zero one was a NULL deref in the compositor. Linux never emits an
/// event for RMFB; the buffer-lifetime worry it was papering over is now
/// handled by the fb's own VMO reference.
/// Retire every framebuffer built on a *driver-private* GEM handle
/// (`>= gem_mmap::DRIVER_HANDLE_BASE`, i.e. a nouveau `GEM_NEW` object),
/// returning how many were dropped.
///
/// Why this exists, and why it does NOT apply to dumb buffers. A dumb-buffer
/// framebuffer holds an `Arc<VmObject>` in `fb_backing`, so Linux's rule
/// applies literally: `GEM_CLOSE` drops the *handle*, the object lives on, and
/// the framebuffer stays scannable until RMFB
/// (`gem_close_keeps_a_framebuffer_and_its_memory_alive` asserts exactly
/// that). A nouveau GEM object has no such reference to hold: `create_fb`
/// resolves it through `gem_mmap` and stores a bare `phys_addr`/`size`, and
/// `nouveau_gem_close` hands the memory straight back to the RM. Once that
/// happens the framebuffer describes memory that belongs to somebody else, and
/// nothing here used to notice -- `gem_close` only looks at `state.handles`,
/// and `release_process` builds its `doomed` list from the same table, so a
/// nouveau-backed fb survived both. A compositor crash therefore left
/// `crtc_fb` pointing at freed VRAM, and the next `repaint_for_cursor` or
/// re-present blitted whatever had been allocated there since -- persistent
/// garbage on the panel, not a single bad frame.
///
/// This is now a SAFETY NET, not the normal path. It used to fire on every
/// `ADDFB2` -> `GEM_CLOSE` a GL/Vulkan compositor performs -- which is the
/// normal dance, not a teardown: wlroots closes the buffer handle immediately
/// after `ADDFB2` because on Linux the framebuffer holds its own reference and
/// the buffer lives until `RMFB`. Retiring the fb there destroyed the
/// compositor's scanout buffer seconds after it was created, and every
/// `SETCRTC` on it answered `ENOENT` for the rest of the session. The fb now
/// takes that reference itself (`fb_take_gem_ref`), so a client's close is
/// never the last one while an fb exists and this cannot be reached from it.
/// What remains is the case it was written for: the object really was freed
/// (by a path that did not go through [`rmfb`]), and an fb left pointing into
/// freed VRAM would be scanned out.
pub fn retire_framebuffers_for_handle(handle_id: u32) -> usize {
    if handle_id < zcore_drivers::scheme::gem_mmap::DRIVER_HANDLE_BASE {
        return 0;
    }
    // Only when the GEM object is REALLY gone. `nouveau_gem_close` answers
    // `true` for "this close was handled", which includes the ordinary case of
    // one holder letting go of a buffer others still reference -- so the
    // GEM_CLOSE arm calls this on every close, not only on the last one.
    // Retiring there destroyed live framebuffers: a compositor closing its
    // buffer handle right after `ADDFB2` (the normal dance -- the fb holds the
    // reference, see `fb_take_gem_ref`) lost the framebuffer it had just made.
    // `gem_mmap`'s entry is removed by `dec_ref` the moment the last reference
    // goes and before the RM free, so "still tracked" means "still alive" and
    // the fb over it is still valid.
    if zcore_drivers::scheme::gem_mmap::lookup(handle_id).is_some() {
        return 0;
    }
    let mut state = DRM_STATE.lock();
    let before = state.framebuffers.len();
    let taken: Vec<u32> = state
        .framebuffers
        .iter()
        .filter(|fb| fb.gem_handle_id == handle_id)
        .map(|fb| fb.id)
        .collect();
    state
        .framebuffers
        .retain(|fb| fb.gem_handle_id != handle_id);
    let dropped = before - state.framebuffers.len();
    if dropped == 0 {
        return 0;
    }
    for fb_id in taken {
        note_fb_retired(&mut state, fb_id, FbRetired::HandleClosed);
    }
    let live: Vec<u32> = state.framebuffers.iter().map(|fb| fb.id).collect();
    state.fb_backing.retain(|(id, _)| live.contains(id));
    if !state.framebuffers.iter().any(|fb| fb.id == state.crtc_fb) {
        state.crtc_fb = 0;
    }
    drop(state);
    warn!(
        "[drm] handle {:#x} closed with {} framebuffer(s) still on it -- retired \
         them rather than scanning out freed GEM memory",
        handle_id, dropped
    );
    dropped
}

/// Remove a framebuffer on behalf of `pid` (`RMFB`/`CLOSEFB`).
///
/// `false` covers both "no such id" and "not yours", which the caller reports
/// as ENOENT either way -- the same answer `drm_mode_rmfb` gives for an id
/// that is not on the calling file's list, and it does not tell a prober
/// whether someone else's framebuffer exists.
pub fn rmfb_for(fb_id: u32, pid: u64) -> bool {
    let handle_id = {
        let mut state = DRM_STATE.lock();
        let Some(pos) = state
            .framebuffers
            .iter()
            .position(|f| f.id == fb_id && owned_by(f.owner, pid))
        else {
            return false;
        };
        let fb = state.framebuffers.remove(pos);
        state.fb_backing.retain(|(id, _)| *id != fb_id);
        if state.crtc_fb == fb_id {
            state.crtc_fb = 0;
        }
        note_fb_retired(&mut state, fb_id, FbRetired::Removed);
        fb.gem_handle_id
    };
    // Outside the lock, and only once the fb is really gone: this is the fb's
    // half of the GEM object's lifetime. For a dumb buffer the `fb_backing`
    // `Arc` above was it; for a nouveau object it is this, and if the client
    // has already closed its own handle then dropping it here is what finally
    // returns the memory to the RM -- the `RMFB` that Linux frees on too.
    fb_drop_gem_ref(handle_id);
    true
}

/// [`rmfb_for`] for the kernel's own teardown paths, which own everything.
pub fn rmfb(fb_id: u32) -> bool {
    rmfb_for(fb_id, 0)
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

/// The framebuffer [`set_crtc_fb`] last bound — what `GETCRTC` reports.
pub fn crtc_fb() -> u32 {
    DRM_STATE.lock().crtc_fb
}

/// Rows per [`blit_chunked`] band. 128 rows is ~2 MiB at 1920-wide ARGB —
/// large enough to amortize the intr_off/intr_on overhead on real hardware
/// (where WC framebuffer writes are fast) while still keeping each band well
/// under a timer tick.
const BLIT_CHUNK_ROWS: u32 = 128;

/// Blit `pixels` (row-major, `src_stride` u32s per row, already offset so
/// `pixels[0]` is the rectangle's top-left) into `display` at
/// `(dst_x, dst_y)`, `width`x`height`, in horizontal bands with interrupts
/// re-enabled between bands.
///
/// IRQs nest on the coroutine stack, so any single blit must run with
/// interrupts fully off end-to-end — that's what stopped the nested-IRQ
/// coroutine-stack overflow (`rip=0x3` / `[rsp0]=0x13486`) seen at labwc
/// bring-up (APIC timer -> xHCI EventListener / DRM timer re-entering
/// mid-scanout). But holding IF clear for an entire full-frame copy means
/// every timer tick, keyboard IRQ and xHCI completion queues up behind it,
/// which was a visible chunk of the "labwc feels slow" gap versus Linux.
/// Chunking bounds the continuous interrupts-off window to one band — pending
/// IRQs are serviced promptly between bands — while each individual blit
/// still runs fully protected, preserving the original crash fix.
/// Repack the frame into the CE staging buffer at `dst_pitch` and return the
/// staging `(phys_addr, bytes)` for a flat CE copy. Returns `None` (caller
/// falls back to the CPU blit) if the staging buffer cannot be allocated.
///
/// The row copies are ordinary cached memory writes (~GB/s), and x86 PCIe DMA
/// snoops the cache, so the CE reads coherent data with no explicit flush.
fn ce_repack_to_staging(
    pixels: &[u32],
    src_pa: u64,
    src_stride: usize,
    dst_pitch: usize,
    dst_visible_bytes: usize,
    w: u32,
    h: u32,
) -> Option<(u64, u64)> {
    let t0 = kernel_hal::timer::timer_now();
    let row_bytes = (w as usize).checked_mul(4)?;
    if row_bytes > dst_pitch {
        return None;
    }
    // The caller asks the CE for a FLAT copy of `dst_pitch * h` bytes, so every
    // byte of every destination row is overwritten -- including the
    // `row_bytes..dst_pitch` tail, which the row loop below never writes. When
    // that tail reaches past the visible width it is off-screen padding and
    // harmless; when it starts INSIDE the visible width, the CE paints the
    // previous frame's leftovers (or, on the first frame, whatever
    // `commit_page` left) over real on-screen columns. Decline instead: the
    // caller falls back to the CPU blit, which writes only the columns it has
    // pixels for and leaves the rest of the screen alone.
    if row_bytes < dst_visible_bytes {
        return None;
    }
    let need = dst_pitch.checked_mul(h as usize)?;
    let cached = {
        let slot = CE_STAGING.lock();
        slot.as_ref()
            .filter(|s| s.2 >= need)
            .map(|s| (s.0, s.1, s.3.clone()))
    };
    let (va, pa, _keepalive) = match cached {
        Some(s) => s,
        None => {
            // Allocate OUTSIDE the IRQ-disabling spinlock: a contiguous
            // multi-MB allocation (plus zeroing) is far too heavy to run with
            // interrupts off. A racing double-alloc then has to be resolved
            // when publishing -- see below; it is NOT "harmless (one wins)",
            // as this comment used to claim.
            let vmo = match VmObject::new_contiguous(pages(need), 12) {
                Ok(v) => v,
                Err(_) => {
                    if !CE_STAGING_ALLOC_FAILED_LOGGED.swap(true, Ordering::Relaxed) {
                        kernel_hal::klog_warn!(
                            "[drm] CE repack: staging alloc failed ({} bytes) -- CPU blit",
                            need
                        );
                    }
                    return None;
                }
            };
            let pa = vmo.commit_page(0, MMUFlags::READ).ok()? as u64;
            let va = phys_to_virt(pa as usize);
            // Publish under the lock, and re-check first. Two presents can
            // reach the allocation at once; the store used to overwrite the
            // slot unconditionally, so the LOSER returned a `pa` whose only
            // owner was the local `_keepalive` -- which drops when this
            // function returns, i.e. before the caller hands the address to
            // `ce_present`. The copy engine then DMA'd out of frames already
            // back in the allocator. The loser must adopt the winner's buffer
            // instead and let its own drop.
            let mut slot = CE_STAGING.lock();
            match slot.as_ref().filter(|s| s.2 >= need) {
                Some(s) => (s.0, s.1, s.3.clone()),
                None => {
                    *slot = Some((va, pa, need, vmo.clone()));
                    (va, pa, vmo)
                }
            }
        }
    };
    for r in 0..h as usize {
        let s = r.checked_mul(src_stride)?;
        if s + row_bytes / 4 > pixels.len() {
            break;
        }
        // SAFETY: staging spans `need` bytes and r*dst_pitch + row_bytes <=
        // need; the source slice bound was checked above.
        unsafe {
            core::ptr::copy_nonoverlapping(
                pixels[s..].as_ptr() as *const u8,
                (va + r * dst_pitch) as *mut u8,
                row_bytes,
            );
        }
        // Define the off-screen tail rather than letting the CE carry over
        // whatever a previous, wider repack left in it. The guard above makes
        // this range entirely off-screen padding, so its contents are not
        // visible -- but the CE copies them, and leaving DMA'd bytes
        // undefined is how the stale-column bug looked in the first place.
        // In the common full-screen case the tail is empty and this is a
        // no-op: `blit_w` is capped at the scanout pitch in pixels.
        if row_bytes < dst_pitch {
            // SAFETY: same span as the copy above -- `r * dst_pitch +
            // dst_pitch <= need`, and the region is the staging buffer's own.
            unsafe {
                core::ptr::write_bytes(
                    (va + r * dst_pitch + row_bytes) as *mut u8,
                    0,
                    dst_pitch - row_bytes,
                );
            }
        }
    }
    // The repack is suspected to be the remaining present cost: the GEM
    // source's CPU address comes from the RM's AT_CPU resolution (see
    // nouveau_uapi's NouveauGemObject::phys_addr, "BAR1-relative"), so these
    // reads may be uncached PCIe reads at ~90 MB/s. Log the measured repack
    // time on the usual cadence, plus a one-shot timed re-read of the first
    // 256 KiB of the source: ~25us means cached RAM (repack is NOT the
    // bottleneck), milliseconds mean uncached BAR reads (the CE must learn
    // to read the source directly; no CPU path can be fast).
    let n = CE_REPACK_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 2 || n.is_multiple_of(64) {
        let repack_us = kernel_hal::timer::timer_now()
            .saturating_sub(t0)
            .as_micros();
        let mut probe_us = 0u128;
        if n == 1 && pixels.len() >= (256 << 10) / 4 {
            let p0 = kernel_hal::timer::timer_now();
            let mut sink = 0u64;
            for px in pixels.iter().take((256 << 10) / 4).step_by(16) {
                sink = sink.wrapping_add(*px as u64);
            }
            probe_us = kernel_hal::timer::timer_now()
                .saturating_sub(p0)
                .as_micros();
            // Keep the probe's reads observable.
            core::sync::atomic::compiler_fence(Ordering::SeqCst);
            let _ = sink;
        }
        kernel_hal::klog_info!(
            "[drm] CE repack #{}: {}us src_pa={:#x} ({} rows x {}B){}",
            n,
            repack_us,
            src_pa,
            h,
            row_bytes,
            if n == 1 {
                alloc::format!(" src read probe 256KiB(step16)={}us", probe_us)
            } else {
                alloc::string::String::new()
            }
        );
    }
    Some((pa, need as u64))
}

fn blit_chunked(
    display: &Arc<dyn DisplayScheme>,
    dst_x: u32,
    dst_y: u32,
    pixels: &[u32],
    src_stride: usize,
    width: u32,
    height: u32,
) {
    let mut row = 0u32;
    while row < height {
        let band_h = BLIT_CHUNK_ROWS.min(height - row);
        let src_off = (row as usize) * src_stride;
        if src_off >= pixels.len() {
            break;
        }
        let irq_on = kernel_hal::interrupt::intr_get();
        if irq_on {
            kernel_hal::interrupt::intr_off();
        }
        display.blit_from(
            dst_x,
            dst_y + row,
            &pixels[src_off..],
            src_stride,
            width,
            band_h,
        );
        if irq_on {
            kernel_hal::interrupt::intr_on();
        }
        row += band_h;
    }
}

/// Offers the frame to each registered DRM driver until one takes it, best CE
/// presenter first: the console GPU, then everyone else. Returns whether any
/// driver took it.
///
/// The order matters because the two copies are not equivalent. A console GPU
/// writes its OWN framebuffer, so the copy stays inside the card. A compute
/// GPU writes the console card's BAR1 across PCIe peer-to-peer, which is
/// slower and only works while ACS/IOMMU let P2P through. Registration order
/// picked neither on purpose: `register_driver` does `insert(0)`, so the list
/// is simply the reverse of the PCI probe order and whichever card happened to
/// be probed last won.
///
/// This changes nothing unless a console GPU is actually state-loaded, which
/// only happens with `nvidia.console_gpu` or a manual `/proc/gpustep14` -- a
/// cold console GPU declines every CE call regardless of where it sits in the
/// list. It is also written for any number of cards: N compute GPUs keep their
/// relative order behind the console one.
///
/// Two filtered passes over the device list rather than a sorted copy, because
/// this runs on the per-frame present path and must not allocate. The read
/// guard is held across the calls, exactly as the plain
/// `for d in all_drm().as_vec().iter()` loops it replaced did.
fn ce_try_in_order(
    mut try_one: impl FnMut(&Arc<dyn zcore_drivers::scheme::DrmScheme>) -> bool,
) -> bool {
    let all = kernel_hal::drivers::all_drm();
    let all = all.as_vec();
    all.iter().filter(|d| d.is_console_gpu()).any(&mut try_one)
        || all.iter().filter(|d| !d.is_console_gpu()).any(&mut try_one)
}

/// FromDevice clflush of the CPU-mapped GEM span covering a scanout damage
/// rect (including pitch padding between the first and last pixel). Needed
/// only when the CPU will *read* the buffer (CPU blit / CE staging repack).
fn dma_sync_scanout_src_from_device(
    vaddr: usize,
    fb_size: usize,
    src_stride: usize,
    blit_x: u32,
    blit_y: u32,
    blit_w: u32,
    blit_h: u32,
) {
    let sync_start_px = (blit_y as usize)
        .saturating_mul(src_stride)
        .saturating_add(blit_x as usize);
    let sync_end_px = (blit_y as usize)
        .saturating_add((blit_h as usize).saturating_sub(1))
        .saturating_mul(src_stride)
        .saturating_add(blit_x as usize)
        .saturating_add(blit_w as usize);
    if sync_end_px <= sync_start_px {
        return;
    }
    let byte_off = sync_start_px.saturating_mul(4).min(fb_size);
    let byte_len = sync_end_px
        .saturating_sub(sync_start_px)
        .saturating_mul(4)
        .min(fb_size.saturating_sub(byte_off));
    zcore_drivers::utils::dma_sync::dma_sync_wb_from_device(vaddr + byte_off, byte_len);
}

/// FromDevice clflush of one rectangle, row by row, so a 64×64 cursor does not
/// clflush a megabyte of pitch padding. Used after CE-direct present so
/// [`blit_cursor_patch`] can read GPU pixels from sysmem without a full-frame
/// sync.
#[allow(clippy::too_many_arguments)]
fn dma_sync_gem_rect_from_device(
    vaddr: usize,
    fb_size: usize,
    stride_px: usize,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    fb_w: u32,
    fb_h: u32,
) {
    if stride_px == 0 || w == 0 || h == 0 {
        return;
    }
    let x0 = x.max(0) as usize;
    let y0 = y.max(0) as usize;
    let x1 = (x + w as i32).min(fb_w as i32).max(0) as usize;
    let y1 = (y + h as i32).min(fb_h as i32).max(0) as usize;
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let row_bytes = (x1 - x0).saturating_mul(4);
    for row in y0..y1 {
        let byte_off = row
            .saturating_mul(stride_px)
            .saturating_add(x0)
            .saturating_mul(4);
        if byte_off >= fb_size {
            break;
        }
        let len = row_bytes.min(fb_size.saturating_sub(byte_off));
        zcore_drivers::utils::dma_sync::dma_sync_wb_from_device(vaddr + byte_off, len);
    }
}

/// How many retired framebuffer ids [`DrmState::fb_retirements`] remembers.
/// A double-buffered swapchain retires two per recreate, so sixteen covers
/// several recreates -- far enough back to still cover the id a stuck
/// compositor keeps re-presenting, and small enough to stay a fixed cost.
const FB_RETIRE_HISTORY: usize = 16;

/// What took a framebuffer away. See `DrmState::fb_retirements`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FbRetired {
    /// The client asked, with `RMFB`/`CLOSEFB`. Its own doing.
    Removed,
    /// `GEM_CLOSE` on the nouveau handle underneath it took it with the
    /// memory (see [`retire_framebuffers_for_handle`]). The client was never
    /// told, and on Linux this would not have happened at all.
    HandleClosed,
    /// The owning process exited and the sweep in [`release_process`] took it.
    ProcessExited,
}

impl FbRetired {
    /// Short text for the console line.
    pub fn as_str(self) -> &'static str {
        match self {
            FbRetired::Removed => "RMFB/CLOSEFB by the client",
            FbRetired::HandleClosed => "GEM_CLOSE of the nouveau handle under it",
            FbRetired::ProcessExited => "the owning process exited",
        }
    }
}

/// Remember that `fb_id` is gone, and why. Caller holds the `DRM_STATE` lock.
fn note_fb_retired(state: &mut DrmState, fb_id: u32, why: FbRetired) {
    if state.fb_retirements.len() >= FB_RETIRE_HISTORY {
        state.fb_retirements.pop_front();
    }
    state.fb_retirements.push_back((fb_id, why));
}

/// What took `fb_id` away, if it is one of the last `FB_RETIRE_HISTORY` to
/// go. `None` means it was never a framebuffer of ours (or went long ago).
pub fn fb_retired_reason(fb_id: u32) -> Option<FbRetired> {
    DRM_STATE
        .lock()
        .fb_retirements
        .iter()
        .rev()
        .find(|(id, _)| *id == fb_id)
        .map(|(_, why)| *why)
}

/// Why a present could not put the caller's pixels on the screen.
///
/// The present path used to answer a bare `false`, and every ioctl arm turned
/// that into `EIO`. Two things went wrong with that. wlroots' legacy backend
/// treats a failed `drmModeSetCrtc` as a failure of the *output*, so it retries
/// the whole modeset on the next frame and never advances to page-flipping:
/// one unpresentable frame cost the entire desktop, at the 8 Hz storm of
/// "connector HDMI-A-1: Failed to set CRTC: I/O error" the compositor log
/// shows. And `EIO` named none of the causes below, while the kernel side of
/// each one is a `warn!` that a rig booted at `LOG=error` never prints -- so
/// the console said nothing at all about which it was.
///
/// Both halves need the reason: the arms answer with the errno Linux answers
/// with (a bad fb id is `ENOENT`, not `EIO`), and say on the console which of
/// these happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentError {
    /// No framebuffer object carries this id. Linux's `drm_mode_setcrtc` and
    /// `drm_mode_setplane` both answer `ENOENT` here ("Unknown FB ID"), and a
    /// client that lost its fb behind its back -- see
    /// [`retire_framebuffers_for_handle`] -- needs to be told *that*, not
    /// "I/O error".
    NoSuchFb,
    /// No display scheme is registered to blit into.
    NoDisplay,
    /// The fb exists but describes no memory (`phys_addr`/`size` of 0), so
    /// there is nothing to copy from.
    NoBacking,
}

impl PresentError {
    /// Short, stable text for the console line — the tag a bug report greps for.
    pub fn as_str(self) -> &'static str {
        match self {
            PresentError::NoSuchFb => "no such fb id",
            PresentError::NoDisplay => "no display to blit into",
            PresentError::NoBacking => "fb has no backing memory",
        }
    }
}

/// Copy a framebuffer's pixels to the hardware display ("scan out").
///
/// Used by the software KMS path (no GPU driver): the dumb buffer is contiguous
/// physical memory, which we map and blit into the display framebuffer.
pub fn scanout(fb_id: u32) -> bool {
    scanout_region(fb_id, None)
}

/// Write-combining GOP/BAR1 combines stores into 64-byte PCIe bursts (16 XRGB
/// pixels). A blit whose left/right edge sits mid-line flushes a stale combine
/// buffer into neighbouring pixels — leftover squares and stripes. Expand `x`/`w`
/// to those 16-pixel boundaries; `limit` is the largest X that is safe to write
/// (visible width, or the pitch in pixels so the right-edge tail can land in
/// off-screen padding).
fn expand_x_for_wc(x: u32, w: u32, limit: u32) -> (u32, u32) {
    const WC_PX: u32 = 16;
    if w == 0 || limit == 0 {
        return (x.min(limit), 0);
    }
    let x0 = x - (x % WC_PX);
    let x1 = x.saturating_add(w).min(limit);
    let x1 = ((x1.saturating_add(WC_PX - 1) / WC_PX) * WC_PX).min(limit);
    (x0, x1.saturating_sub(x0))
}

/// Like [`scanout`], but when `rect` (`x, y, width, height`, in the
/// framebuffer's own coordinates) is given, blits only that region instead of
/// the whole frame.
///
/// `DRM_IOCTL_MODE_DIRTYFB` uses this so the GOP keeps pixels the client did
/// not repaint — a software-KMS swapchain often has only the damage boxes
/// drawn, and copying the rest smears stale tiles onto the screen. `None`
/// (page-flip / modeset) always repaints everything, as does an out-of-range
/// or degenerate `rect`. Horizontal edges are expanded to 64-byte WC lines.
pub fn scanout_region(fb_id: u32, rect: Option<(u32, u32, u32, u32)>) -> bool {
    scanout_region_checked(fb_id, rect).is_ok()
}

/// [`scanout_region`], but naming the reason it could not put pixels on the
/// screen instead of collapsing every one of them to `false`. See
/// [`PresentError`] for why the ioctl arms need the distinction.
pub fn scanout_region_checked(
    fb_id: u32,
    rect: Option<(u32, u32, u32, u32)>,
) -> Result<(), PresentError> {
    let fb = {
        let state = DRM_STATE.lock();
        match state.framebuffers.iter().find(|f| f.id == fb_id) {
            Some(f) => *f,
            None => {
                warn!("[drm] scanout: fb_id={} not found", fb_id);
                return Err(PresentError::NoSuchFb);
            }
        }
    };
    // Before the display, because this one is the framebuffer's OWN defect and
    // holds whether or not anything is plugged in: a fb with no backing can
    // never present, on any display. Checking the display first reported the
    // environment when the buffer was the problem.
    if fb.phys_addr == 0 || fb.size == 0 {
        // Once: a framebuffer that ADDFB2 registered with no backing (phys 0 /
        // size 0) can never present. Silent before, so "nothing on screen"
        // named nothing; now it points straight at the unresolved buffer.
        if !SCANOUT_NULL_LOGGED.swap(true, Ordering::Relaxed) {
            warn!(
                "[drm] scanout: fb={} has no backing (phys={:#x} size={}) -- nothing to present",
                fb_id, fb.phys_addr, fb.size
            );
        }
        return Err(PresentError::NoBacking);
    }
    let display = match primary_display() {
        Some(d) => d,
        None => {
            warn!("[drm] scanout: no display -- software_kms inactive, nothing to blit to");
            return Err(PresentError::NoDisplay);
        }
    };
    // Log the first scanout so a console photo confirms pixels are flowing.
    if !SCANOUT_LOGGED.swap(true, Ordering::Relaxed) {
        warn!(
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
    let fb_width = fb.width.min(info.width);
    let fb_height = fb.height.min(info.height);
    // Pitch in pixels is the WC-safe right limit: writing the padding after
    // `fb_width` is off-screen but completes the last combine buffer.
    let pitch_px = src_stride.min((info.pitch() / 4) as usize) as u32;
    let (blit_x, blit_y, blit_w, blit_h) = match rect {
        Some((x, y, w, h)) => {
            let x = x.min(fb_width);
            let y = y.min(fb_height);
            let w = w.min(fb_width.saturating_sub(x));
            let h = h.min(fb_height.saturating_sub(y));
            let (x, w) = expand_x_for_wc(x, w, pitch_px);
            (x, y, w, h)
        }
        None => {
            let (x, w) = expand_x_for_wc(0, fb_width, pitch_px);
            (x, 0, w, fb_height)
        }
    };
    if blit_w == 0 || blit_h == 0 {
        return Ok(());
    }
    let src_off = (blit_y as usize)
        .saturating_mul(src_stride)
        .saturating_add(blit_x as usize);
    let t0 = kernel_hal::timer::timer_now();
    let mut sync_elapsed = Duration::ZERO;
    let mut cpu_src_synced = false;
    let gem_cpu_mapped = zcore_drivers::scheme::gem_mmap::lookup(fb.gem_handle_id).is_some();

    // CE-offloaded present: copy the frame (sysmem) into the scanout FB (the
    // console GPU's VRAM) with a GPU copy engine instead of CPU stores over
    // PCIe — on real hardware the console GPU's BAR1 serves CPU stores at a
    // measured 42 MB/s even through a verified write-combining mapping
    // (~99 ms/frame, the 7-11 FPS desktop), while GPU-initiated writes burst
    // at PCIe speed.
    //
    // A damage rect takes the pitched 2D path, which walks rows at a stride
    // and now also takes a destination offset, so the engine can write just
    // the damaged region. This used to be full-frame only (`rect.is_none()`),
    // which inverted the economics of the whole present path: the cheap case
    // — a small damage box — was handed to the CPU, and the expensive one to
    // the GPU. The flat copy still needs a full frame, because it copies from
    // the buffer's start with no stride at all.
    //
    // OPT-IN: gated on `nvidia.cepresent` (see CE_PRESENT_ENABLED) — the CE
    // DMA per present destabilized the desktop on real hardware once, so the
    // default present is still the CPU blit (ce_present wedges itself off on
    // the first confirmed failure).
    //
    // Cache sync vs. CE-direct: `dma_sync_wb_from_device` is a clflush of the
    // whole GEM (~4.2 MB, milliseconds). That exists so a CPU *read* of the
    // WB physmap alias sees GPU-rendered pixels (CPU blit / CE staging
    // repack). CE-direct (`ce_present_2d_pitched` or flat `ce_present` from
    // `fb.phys_addr`) DMAs from physical sysmem itself and does not consult
    // the CPU cache, so the full FromDevice is wasted. NVK writes the GEM via
    // the GPU, not CPU-mapped WB stores, so a ToDevice clflush is not needed
    // either (that would be the same 4.2 MB cost). Skip both; if CE frames
    // look stale, restore a sync here — the present klog's `sync Xus` is the
    // tell (should be ~0 on CE-direct).
    let mut blitted_by_ce = false;
    if CE_PRESENT_ENABLED.load(Ordering::Relaxed) {
        // Byte offsets of the damaged region in each buffer. Both are zero for
        // a full-frame present, which is exactly the old behaviour.
        let src_byte_off = (blit_y as u64)
            .saturating_mul(fb.pitch as u64)
            .saturating_add((blit_x as u64).saturating_mul(4));
        let dst_byte_off = (blit_y as u64)
            .saturating_mul(info.pitch as u64)
            .saturating_add((blit_x as u64).saturating_mul(4));
        // The flat path has no stride and no destination offset, so it can
        // only serve a full frame with matching pitches.
        let flat_ok = rect.is_none() && fb.pitch == info.pitch;
        if !flat_ok {
            // Pitched 2D CE path: the GPU copy engine reads directly from the
            // source buffer at fb.pitch and writes into the scanout FB at
            // info.pitch — no CPU staging repack.  On the common dual-RTX case
            // (client pitch 5504 → GOP scanout pitch 8192) this eliminates the
            // slow CPU reads from the uncached BAR1-backed source buffer.
            //
            // Fallback chain: 2D fails (CE_PRESENT_WEDGED latched) →
            // FromDevice + repack+flat CE → CPU blit.
            let row_bytes = (blit_w as usize).saturating_mul(4) as u32;
            let src_pa = fb.phys_addr.saturating_add(src_byte_off);
            blitted_by_ce = ce_try_in_order(|d| {
                d.ce_present_2d_pitched(
                    src_pa,
                    fb.pitch,
                    dst_byte_off,
                    info.pitch,
                    row_bytes,
                    blit_h,
                )
            });
            // Fallback: CPU reads the GEM, so FromDevice first, then repack
            // into staging at scanout pitch and flat CE.
            //
            // Full frames only. The repack lands the rows at the start of the
            // staging buffer and the flat CE copies them to the start of the
            // scanout FB, so with a damage rect it would paint the damaged
            // region in the top-left corner of the screen. A rect that cannot
            // take the 2D path falls through to the CPU blit, which does
            // honour it.
            if !blitted_by_ce && rect.is_none() {
                if gem_cpu_mapped {
                    let ts = kernel_hal::timer::timer_now();
                    dma_sync_scanout_src_from_device(
                        vaddr, fb.size, src_stride, blit_x, blit_y, blit_w, blit_h,
                    );
                    sync_elapsed = kernel_hal::timer::timer_now().saturating_sub(ts);
                    cpu_src_synced = true;
                }
                let (ce_src_pa, ce_size) = ce_repack_to_staging(
                    pixels,
                    fb.phys_addr,
                    src_stride,
                    info.pitch as usize,
                    (info.width as usize).saturating_mul(4),
                    blit_w,
                    blit_h,
                )
                .unwrap_or((0, 0));
                if ce_src_pa != 0 && ce_size != 0 {
                    blitted_by_ce = ce_try_in_order(|d| d.ce_present(ce_src_pa, ce_size));
                }
            }
        } else {
            // Flat CE path: pitches match, a single flat copy covers the frame.
            let ce_src_pa = fb.phys_addr;
            let ce_size = (info.pitch as u64) * (blit_h as u64);
            blitted_by_ce = ce_try_in_order(|d| d.ce_present(ce_src_pa, ce_size));
        }
        if !blitted_by_ce && !CE_NO_TAKER_LOGGED.swap(true, Ordering::Relaxed) {
            // Every GPU declined (wedged, not state-loaded, or no boot FB):
            // cepresent is set but the CPU is still doing the copy, and
            // until now that degradation was completely silent. The
            // per-GPU reason is one-shot-logged by `ce_present` itself.
            kernel_hal::klog_warn!(
                "[drm] CE-offload present enabled but NO GPU took the copy -- CPU blit \
                 (see [NVIDIA] ce_present lines for the reason)"
            );
        }
    }
    if !blitted_by_ce {
        // CPU reads the GEM through the WB physmap alias. Without FromDevice,
        // stale lines from the previous frame stay resident and the screen
        // stops repainting. (MOVNTDQA does not lift this: non-temporal loads
        // only bypass the cache on WC memory, not on this WB alias.)
        if gem_cpu_mapped && !cpu_src_synced {
            let ts = kernel_hal::timer::timer_now();
            dma_sync_scanout_src_from_device(
                vaddr, fb.size, src_stride, blit_x, blit_y, blit_w, blit_h,
            );
            sync_elapsed = kernel_hal::timer::timer_now().saturating_sub(ts);
            cpu_src_synced = true;
        }
        // Banded blit with IRQs briefly re-enabled between bands — see
        // [`blit_chunked`]. Honours a DIRTYFB damage rect when present.
        if src_off < pixels.len() {
            blit_chunked(
                &display,
                blit_x,
                blit_y,
                &pixels[src_off..],
                src_stride,
                blit_w,
                blit_h,
            );
        }
    }
    let t_blit = kernel_hal::timer::timer_now();
    // Composite the kernel cursor on top of the just-blitted frame, so a
    // page-flip never erases the pointer. Snapshot under the lock, then
    // compose in cached sysmem and write-only blit to GOP — same pattern as
    // [`repaint_for_cursor`] / [`blit_cursor_patch`]. Never RMW BAR1 here:
    // `blit_argb_over` reads the slow PCIe framebuffer for alpha blend
    // (~64×64 RMW every frame) and that hitch is what this avoids.
    // Hardware cursor (`c.hw`) still skips software compositing entirely.
    let cursor = {
        let mut state = DRM_STATE.lock();
        let c = &state.cursor;
        // `!c.hw`: when the display-engine plane owns the pointer, the
        // hardware composites it over scanout -- blending it here too would
        // draw the cursor twice (and bake a stale copy into the frame).
        let snap = if !c.hw && c.visible && c.w > 0 && c.h > 0 {
            c.bitmap
                .as_ref()
                .filter(|b| !b.is_empty())
                .map(|b| (c.x, c.y, c.w, c.h, Arc::clone(b)))
        } else {
            None
        };
        // A full scanout repaints the whole frame and composites the cursor at
        // its current position, so that — not whatever the last partial move
        // left — is now what's drawn. Keeping `drawn` in step here stops the
        // next `repaint_for_cursor` from leaving a ghost of the pre-flip cursor.
        state.cursor.drawn = snap.as_ref().map(|(x, y, w, h, _)| (*x, *y, *w, *h));
        snap
    };
    if let Some((cx, cy, cw, ch, bmp)) = cursor {
        // CE-direct skipped the full-frame FromDevice. Invalidate just the
        // cursor window so the CPU blend sees GPU pixels; counted in cursor
        // time, not `sync`, so the klog keeps showing ~0us sync on CE.
        if blitted_by_ce && !cpu_src_synced && gem_cpu_mapped {
            dma_sync_gem_rect_from_device(
                vaddr, fb.size, src_stride, cx, cy, cw, ch, fb_width, fb_height,
            );
        }
        // Clip to what the framebuffer covers (`fb_width`/`fb_height`), not to
        // the screen: a client fb narrower or shorter than the display would
        // otherwise have the patch read past the end of a row -- the next
        // row's pixels -- and paint that onto the scanout as a shifted square
        // trailing the pointer. `repaint_for_cursor` has always clipped this
        // way (see its `(fw, fh)`); this call site did not, even though the
        // cache invalidate right above it already used the clipped pair.
        blit_cursor_patch(
            &*display, pixels, src_stride, fb_width, fb_height, cx, cy, cw, ch, cx, cy, cw, ch,
            &bmp,
        );
    }
    let t_cursor = kernel_hal::timer::timer_now();
    if rect.is_none() {
        let n = PRESENT_FRAME_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 2 || n.is_multiple_of(64) {
            kernel_hal::klog_info!(
                "[drm] present #{}: sync {}us + {} blit {}us + cursor {}us ({}x{})",
                n,
                sync_elapsed.as_micros(),
                if blitted_by_ce { "CE" } else { "cpu" },
                t_blit
                    .saturating_sub(t0)
                    .saturating_sub(sync_elapsed)
                    .as_micros(),
                t_cursor.saturating_sub(t_blit).as_micros(),
                blit_w,
                blit_h
            );
        }
    }
    let _ = display.flush();
    // A DRM client owns the framebuffer now: stop the kernel text console from
    // drawing over it (like fbcon yielding to KMS). Restored on DROP_MASTER.
    claim_graphics_vt();
    Ok(())
}

/// Put the compositor's OWN VT into `KD_GRAPHICS` after a present.
///
/// This used to be `set_kd_mode(KD_GRAPHICS)` = "whatever VT is active now".
/// The VT gate in [`present_now_region`] runs before the blit, but
/// [`blit_chunked`] re-enables interrupts between bands, so a Ctrl+Alt+Fn
/// during a 16-100 ms present could complete the switch mid-frame: the
/// trailing bands landed on the text console, and then THAT VT was stamped
/// KD_GRAPHICS -- no keyboard echo, no repaint, a dead console until a
/// userspace KDSETMODE. Now the mode goes to the owner VT only while it is
/// still the active one; if the display moved away under us, the text VT
/// keeps its mode and is repainted over the stray bands.
fn claim_graphics_vt() {
    use kernel_hal::console::{active_vt, kd_mode_vt, set_kd_mode_vt, KD_GRAPHICS, KD_TEXT};
    let owner = DRM_STATE.lock().graphics_vt;
    let active = active_vt();
    match owner {
        Some(vt) if vt == active => set_kd_mode_vt(vt, KD_GRAPHICS),
        Some(_) => {
            if kd_mode_vt(active) == KD_TEXT {
                kernel_hal::console::redraw_active_console();
            }
        }
        None => set_kd_mode_vt(active, KD_GRAPHICS),
    }
}

/// Set (or hide) the cursor bitmap from a GEM handle (`DRM_MODE_CURSOR_BO`).
///
/// `handle_id == 0` (or a zero-sized image) hides the cursor. Otherwise the
/// client's cursor BO is a tightly-packed premultiplied-ARGB8888 dumb buffer of
/// `w`x`h` pixels; copy it out so a later GEM_CLOSE / reuse can't tear the image
/// mid-scanout. Returns true if the cursor state changed enough to warrant a
/// repaint.
pub const MAX_CURSOR_DIM: u32 = 64;

pub fn set_cursor_bo(handle_id: u32, w: u32, h: u32) -> bool {
    let mut state = DRM_STATE.lock();
    if handle_id == 0 || w == 0 || h == 0 {
        let was_visible = state.cursor.visible;
        let was_hw = state.cursor.hw;
        state.cursor.visible = false;
        state.cursor.hw = false;
        drop(state);
        // The display-engine plane (if it owned the pointer) must actually
        // switch off, or the last image stays composited by hardware forever.
        if was_hw {
            for d in kernel_hal::drivers::all_drm().as_vec().iter() {
                if d.hw_cursor_hide() {
                    break;
                }
            }
        }
        return was_visible;
    }
    // Resolve the cursor BO's backing physical range. wlroots allocates it two
    // ways depending on the output's renderer: a generic CREATE_DUMB buffer
    // (tracked in `state.handles`) on the pixman/software path, OR a
    // nouveau-uAPI GEM_NEW object (high-range handle tracked in `gem_mmap`, NOT
    // in `state.handles`) once the compositor runs on the GL/nouveau renderer
    // (`renderer=gl:nvidia`). Resolving only `state.handles` -- as this did --
    // meant the moment the output went GL, the cursor BO handle missed here,
    // `visible` dropped to false, and the pointer vanished / froze in place
    // (its MOVE ioctls still updated x/y, but nothing was ever drawn). Fall
    // back to the same nouveau lookup create_fb (ADDFB2) and the mmap path use.
    // `resolve_gem_backing` takes DRM_STATE itself, so drop our guard first.
    drop(state);
    let (phys_addr, size) = match resolve_gem_backing(handle_id) {
        Some(v) => v,
        None => {
            DRM_STATE.lock().cursor.visible = false;
            return true;
        }
    };
    // The advertised DRM_CAP_CURSOR_WIDTH/HEIGHT is 64; the ioctl arm rejects
    // anything larger with EINVAL like Linux, so `px * 4` cannot overflow and
    // the bitmap is at most 16 KiB. Copy it BEFORE taking `DRM_STATE`: the
    // lock is an IRQ-disabling spinlock and this used to run a user-sized
    // memcpy (bounded only by the GEM's 64 MiB) with interrupts off.
    let px = (w as usize).saturating_mul(h as usize);
    let bytes = px.saturating_mul(4);
    if px == 0 || w > MAX_CURSOR_DIM || h > MAX_CURSOR_DIM || size < bytes || phys_addr == 0 {
        DRM_STATE.lock().cursor.visible = false;
        return true;
    }
    let vaddr = phys_to_virt(phys_addr as usize);
    // The image was written by someone else — the GPU into a nouveau GEM on
    // the `renderer=gl:nvidia` path, or userspace through a write-combining
    // mapping of a dumb buffer — and we are about to read it through the
    // kernel's cached WB alias. Without a FromDevice invalidate the snapshot
    // below can be lines of a PREVIOUS cursor image, and since it is cached in
    // `state.cursor.bitmap` the garbled pointer then persists until the next
    // CURSOR_BO. One clflush of at most 64x64x4 bytes, on image change only —
    // never on a move.
    zcore_drivers::utils::dma_sync::dma_sync_wb_from_device(vaddr, px * 4);
    // SAFETY: contiguous physical buffer of `size` bytes, identity-mapped at
    // `vaddr`; we read exactly `px` u32 pixels (<= size/4).
    let src = unsafe { core::slice::from_raw_parts(vaddr as *const u32, px) };
    // Fresh Arc shared by subsequent scanouts (no per-flip Vec clone).
    let bitmap: Arc<[u32]> = Arc::from(src);
    let mut state = DRM_STATE.lock();
    state.cursor.bitmap = Some(bitmap);
    state.cursor.w = w;
    state.cursor.h = h;
    state.cursor.visible = true;
    // REAL display-engine cursor (opt-in via `nvidia.hwcursor`): offer the
    // image to the driver's cursor plane. Done OUTSIDE the DRM lock -- the
    // upload goes through the RM gate and can take a few ms, and nothing in
    // the driver ever takes DRM_STATE. On success the hardware composites the
    // pointer during scanout (scanout()/repaint_for_cursor stand down); on
    // any failure the software path set up above simply stays in charge.
    if hw_cursor_wanted() {
        let (cx, cy, bmp) = (state.cursor.x, state.cursor.y, state.cursor.bitmap.clone());
        drop(state);
        let mut hw_ok = false;
        if let Some(bmp) = bmp {
            for d in kernel_hal::drivers::all_drm().as_vec().iter() {
                if d.hw_cursor_set(&bmp, w, h) {
                    // Land the plane on the pointer's current position.
                    let _ = d.hw_cursor_move(cx, cy);
                    hw_ok = true;
                    break;
                }
            }
        }
        let mut state = DRM_STATE.lock();
        state.cursor.hw = hw_ok;
        if hw_ok {
            // Whatever the software compositor last drew is erased by the
            // next full scanout (which no longer blends); nothing to restore.
            state.cursor.drawn = None;
        }
    }
    true
}

/// `true` when the kernel cmdline opts into the display-engine hardware
/// cursor (`nvidia.hwcursor`). Cached after the first look: this is on every
/// pointer-motion path.
fn hw_cursor_wanted() -> bool {
    use core::sync::atomic::AtomicU8;
    static WANTED: AtomicU8 = AtomicU8::new(0); // 0 unknown, 1 yes, 2 no
    match WANTED.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let yes = kernel_hal::boot::cmdline().contains("nvidia.hwcursor");
            WANTED.store(if yes { 1 } else { 2 }, Ordering::Relaxed);
            yes
        }
    }
}

/// Move the cursor's top-left to `(x, y)` in output pixels
/// (`DRM_MODE_CURSOR_MOVE`). The compositor has already applied the hotspot.
pub fn move_cursor(x: i32, y: i32) {
    let hw = {
        let mut state = DRM_STATE.lock();
        state.cursor.x = x;
        state.cursor.y = y;
        state.cursor.hw
    };
    // Hardware plane: one PIO write in the driver, outside the DRM lock.
    if hw {
        for d in kernel_hal::drivers::all_drm().as_vec().iter() {
            if d.hw_cursor_move(x, y) {
                break;
            }
        }
    }
}

/// Make a cursor set/move take effect immediately (the legacy cursor ioctls
/// carry no page-flip of their own) WITHOUT re-blitting the whole frame.
///
/// The pointer moves far more often than the scene changes — wlroots issues a
/// `DRM_MODE_CURSOR_MOVE` per input event — so a full `scanout()` per move was
/// blitting ~16 MB every time the mouse twitched, which is precisely why the
/// "software cursor" pegged the CPU on real hardware. Instead, restore just the
/// rectangle the old cursor occupied from the CRTC framebuffer (the composited
/// scene, which has no cursor baked in) and composite the new cursor on top.
/// Only two ~64x64 windows are touched per move.
pub fn repaint_for_cursor() {
    // `crtc_blanked`: a pointer move is the kernel's own repaint, not a client
    // present, so it must not light a panel the client turned off. This is the
    // latch half of `set_crtc_blanked` -- without it a mouse twitch redrew the
    // whole frame and undid the blank.
    if !software_kms_active() || scanout_paused() || crtc_blanked() {
        return;
    }
    // Snapshot everything needed under the lock, and record the rect we are
    // about to draw so the *next* move knows what to erase.
    let (fb_id, old_rect, new) = {
        let mut st = DRM_STATE.lock();
        // While a text VT is foreground the compositor's pixels are suppressed;
        // don't scribble a cursor over the console.
        if st.graphics_vt != Some(kernel_hal::console::active_vt()) {
            return;
        }
        // Display-engine plane owns the pointer: the hardware composites it
        // during scanout and the move already went to the driver as one PIO
        // write -- there is nothing to erase or blend here.
        if st.cursor.hw {
            return;
        }
        let fb_id = st.crtc_fb;
        let old_rect = st.cursor.drawn;
        let c = &st.cursor;
        let new = if c.visible && c.w > 0 && c.h > 0 {
            c.bitmap
                .as_ref()
                .filter(|b| !b.is_empty())
                .map(|b| (c.x, c.y, c.w, c.h, Arc::clone(b)))
        } else {
            None
        };
        st.cursor.drawn = new.as_ref().map(|(x, y, w, h, _)| (*x, *y, *w, *h));
        (fb_id, old_rect, new)
    };
    if fb_id == 0 {
        return;
    }
    let fb = {
        let state = DRM_STATE.lock();
        match state.framebuffers.iter().find(|f| f.id == fb_id) {
            Some(f) => *f,
            None => return,
        }
    };
    let display = match primary_display() {
        Some(d) => d,
        None => return,
    };
    if fb.phys_addr == 0 || fb.size == 0 {
        return;
    }
    let info = display.info();
    // Clip to what the framebuffer actually covers, not merely to the screen.
    // A client fb narrower or shorter than the display would otherwise have
    // the patch read past the end of a row -- i.e. the next row's pixels -- and
    // paint that garbage onto the scanout.
    let (fw, fh) = (info.width.min(fb.width), info.height.min(fb.height));
    let vaddr = phys_to_virt(fb.phys_addr as usize);
    // SAFETY: contiguous physical framebuffer of `fb.size` bytes, identity
    // mapped at `vaddr`; read as `fb.size / 4` u32 pixels.
    let pixels = unsafe { core::slice::from_raw_parts(vaddr as *const u32, fb.size / 4) };
    let src_stride = (fb.pitch / 4) as usize;
    // Every rectangle below is a CPU *read* of the compositor's scene through
    // the WB physmap alias of the GEM. On the nouveau/NVK path the GPU writes
    // that GEM, so the read needs the same FromDevice invalidate `scanout()`
    // does before its CPU blit: without it the erase paints whichever lines
    // the cache still holds and the pointer drags stale squares of an older
    // frame across the screen. Row-by-row over one ~64x64 window, so this is
    // not the full-frame clflush -- it is the cost `scanout()` already pays per
    // present, restricted to the two windows a move touches.
    let gem_cpu_mapped = zcore_drivers::scheme::gem_mmap::lookup(fb.gem_handle_id).is_some();
    let sync_rect = |x: i32, y: i32, w: u32, h: u32| {
        if gem_cpu_mapped {
            dma_sync_gem_rect_from_device(vaddr, fb.size, src_stride, x, y, w, h, fw, fh);
        }
    };
    // Compose in cached sysmem (the CRTC dumb buffer), then one write-only
    // blit to GOP. Never read the display aperture: that RMW is why the
    // pointer felt sticky compared to eclipse-old's sw_cursor (tiny dirty
    // writes). When old and new rects overlap, one union blit covers erase
    // + draw; otherwise restore then paint.
    let old_expanded = old_rect.map(|(ox, oy, ow, oh)| (ox - 1, oy - 1, ow + 2, oh + 2));
    match (old_expanded, new.as_ref()) {
        (Some((ox, oy, ow, oh)), Some((nx, ny, nw, nh, bmp))) => {
            let (ux, uy, uw, uh) = union_i32(ox, oy, ow, oh, *nx, *ny, *nw, *nh);
            if rects_overlap(ox, oy, ow, oh, *nx, *ny, *nw, *nh) {
                sync_rect(ux, uy, uw, uh);
                blit_cursor_patch(
                    &*display, pixels, src_stride, fw, fh, ux, uy, uw, uh, *nx, *ny, *nw, *nh, bmp,
                );
            } else {
                sync_rect(ox, oy, ow, oh);
                restore_rect(&*display, pixels, src_stride, fw, fh, ox, oy, ow, oh);
                sync_rect(*nx, *ny, *nw, *nh);
                blit_cursor_patch(
                    &*display, pixels, src_stride, fw, fh, *nx, *ny, *nw, *nh, *nx, *ny, *nw, *nh,
                    bmp,
                );
            }
        }
        (Some((ox, oy, ow, oh)), None) => {
            sync_rect(ox, oy, ow, oh);
            restore_rect(&*display, pixels, src_stride, fw, fh, ox, oy, ow, oh);
        }
        (None, Some((nx, ny, nw, nh, bmp))) => {
            sync_rect(*nx, *ny, *nw, *nh);
            blit_cursor_patch(
                &*display, pixels, src_stride, fw, fh, *nx, *ny, *nw, *nh, *nx, *ny, *nw, *nh, bmp,
            );
        }
        (None, None) => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn rects_overlap(ax: i32, ay: i32, aw: u32, ah: u32, bx: i32, by: i32, bw: u32, bh: u32) -> bool {
    let ax1 = ax.saturating_add(aw as i32);
    let ay1 = ay.saturating_add(ah as i32);
    let bx1 = bx.saturating_add(bw as i32);
    let by1 = by.saturating_add(bh as i32);
    ax < bx1 && bx < ax1 && ay < by1 && by < ay1
}

#[allow(clippy::too_many_arguments)]
fn union_i32(
    ax: i32,
    ay: i32,
    aw: u32,
    ah: u32,
    bx: i32,
    by: i32,
    bw: u32,
    bh: u32,
) -> (i32, i32, u32, u32) {
    let x0 = ax.min(bx);
    let y0 = ay.min(by);
    let x1 = ax
        .saturating_add(aw as i32)
        .max(bx.saturating_add(bw as i32));
    let y1 = ay
        .saturating_add(ah as i32)
        .max(by.saturating_add(bh as i32));
    (
        x0,
        y0,
        x1.saturating_sub(x0).max(0) as u32,
        y1.saturating_sub(y0).max(0) as u32,
    )
}

/// Copy `patch` of the CRTC fb, blend the cursor into it in RAM, blit once.
#[allow(clippy::too_many_arguments)]
fn blit_cursor_patch(
    display: &dyn DisplayScheme,
    pixels: &[u32],
    src_stride: usize,
    fw: u32,
    fh: u32,
    px: i32,
    py: i32,
    pw: u32,
    ph: u32,
    cx: i32,
    cy: i32,
    cw: u32,
    ch: u32,
    bmp: &[u32],
) {
    if src_stride == 0 || pw == 0 || ph == 0 {
        return;
    }
    let pitch_px = (src_stride as u32).min(display.info().pitch() / 4).max(fw);
    let x0 = px.max(0) as u32;
    let y0 = py.max(0);
    let x1 = (px + pw as i32).max(0) as u32;
    let y1 = (py + ph as i32).min(fh as i32);
    let width = x1.saturating_sub(x0);
    if y1 <= y0 || width == 0 {
        return;
    }
    let (x0, tw_u) = expand_x_for_wc(x0, width, pitch_px);
    if tw_u == 0 {
        return;
    }
    let x0 = x0 as i32;
    let tw = tw_u as usize;
    let th = (y1 - y0) as usize;
    let need = tw.saturating_mul(th);
    let mut slot = CURSOR_PATCH.lock();
    if slot.len() < need {
        slot.resize(need, 0);
    }
    let patch = &mut slot[..need];
    // Rows actually sourced from the framebuffer. CURSOR_PATCH is scratch
    // REUSED across moves, so a row the source could not fill still holds the
    // previous patch's pixels — blitting `th` rows unconditionally would paint
    // that onto the screen as a stale square. Blit only what was filled.
    let mut rows = 0usize;
    for r in 0..th {
        let src_y = y0 as usize + r;
        let src_off = src_y.saturating_mul(src_stride).saturating_add(x0 as usize);
        let dst_off = r * tw;
        let n = tw.min(pixels.len().saturating_sub(src_off));
        if n < tw {
            break;
        }
        rows = r + 1;
        patch[dst_off..dst_off + n].copy_from_slice(&pixels[src_off..src_off + n]);
        let cr = (y0 + r as i32) - cy;
        if cr < 0 || cr >= ch as i32 {
            continue;
        }
        let bmp_row = cr as usize * cw as usize;
        for c in 0..n {
            let cc = (x0 + c as i32) - cx;
            if cc < 0 || cc >= cw as i32 {
                continue;
            }
            let si = bmp_row + cc as usize;
            if si >= bmp.len() {
                break;
            }
            let s = bmp[si];
            let a = s >> 24;
            if a == 0 {
                continue;
            }
            let di = dst_off + c;
            patch[di] = if a == 0xff {
                s | 0xFF00_0000
            } else {
                let d = patch[di];
                let inv = 255 - a;
                let (sr, sg, sb) = ((s >> 16) & 0xff, (s >> 8) & 0xff, s & 0xff);
                let (dr, dg, db) = ((d >> 16) & 0xff, (d >> 8) & 0xff, d & 0xff);
                let or = (sr + dr * inv / 255).min(0xff);
                let og = (sg + dg * inv / 255).min(0xff);
                let ob = (sb + db * inv / 255).min(0xff);
                0xFF00_0000 | (or << 16) | (og << 8) | ob
            };
        }
    }
    if rows == 0 {
        return;
    }
    display.blit_from(x0 as u32, y0 as u32, patch, tw, tw as u32, rows as u32);
}

/// Restore the `(x, y, w, h)` window of the display from the CRTC framebuffer
/// `pixels` (row-major, `src_stride` pixels/row), clipped to the visible
/// `fw`x`fh` area. Used to erase the old cursor before drawing the new one.
#[allow(clippy::too_many_arguments)]
fn restore_rect(
    display: &dyn DisplayScheme,
    pixels: &[u32],
    src_stride: usize,
    fw: u32,
    fh: u32,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
) {
    if src_stride == 0 {
        return;
    }
    let pitch_px = (src_stride as u32).min(display.info().pitch() / 4).max(fw);
    let y0 = y.max(0);
    let y1 = (y + h as i32).min(fh as i32);
    if y1 <= y0 {
        return;
    }
    let x0 = x.max(0) as u32;
    let x1 = (x + w as i32).max(0) as u32;
    let width = x1.saturating_sub(x0);
    if width == 0 {
        return;
    }
    let (x0u, cw) = expand_x_for_wc(x0, width, pitch_px);
    if cw == 0 {
        return;
    }
    let x0 = x0u as i32;
    let ch = (y1 - y0) as u32;
    let off = y0 as usize * src_stride + x0 as usize;
    if off >= pixels.len() {
        return;
    }
    display.blit_from(x0 as u32, y0 as u32, &pixels[off..], src_stride, cw, ch);
}

/// Page-flip to `fb_id` and queue a completion event for the card fd.
///
/// `crtc_id`/`user_data` come from the page-flip request and are echoed back in
/// the `drm_event_vblank` so libdrm's event loop can match the flip.
/// Fallback synthetic vblank rate when no CRTC mode is active. Pacing flip
/// completions to the active mode's refresh (or this fallback) keeps an
/// unthrottled compositor loop from spinning.
///
/// Period and [`vblank_seq_now`] must always use the same divisor: the old
/// pair (period 16_666_667 ns vs sequence `now_ns * 60 / 1e9`) drifted and
/// compositors that derive nsec/MSC from both clocks reported ~55 Hz.
const FALLBACK_VBLANK_HZ: u64 = 60;

/// Nanoseconds per synthetic vblank. Updated from the active CRTC mode
/// ([`set_vblank_period_from_modeinfo`]); starts at 60 Hz.
static VBLANK_PERIOD_NS: AtomicU64 = AtomicU64::new(1_000_000_000 / FALLBACK_VBLANK_HZ);

/// Current synthetic vblank period in nanoseconds (at least 1).
#[inline]
fn vblank_period_ns() -> u64 {
    VBLANK_PERIOD_NS.load(Ordering::Relaxed).max(1)
}

/// Refresh rate (Hz) from a `drm_mode_modeinfo` blob: prefer `vrefresh`, else
/// derive it from the timings the way Linux's `drm_mode_vrefresh` does.
///
/// The derivation ROUNDS TO NEAREST (`DIV_ROUND_CLOSEST`), and that is not
/// cosmetic. A pixel clock is stored in whole kHz, so it cannot express an
/// exact 60 Hz for most timings: the 1920x1080 mode this driver itself
/// advertises comes to 139900 kHz over a 2080x1121 total, which is 59.9995 Hz.
/// Truncating division called that **59**, and `set_vblank_period_from_modeinfo`
/// turned it into a 16.95 ms synthetic vblank instead of 16.67 ms -- every
/// frame paced 1.7% slow. Nine of the thirteen common modes `make_modeinfo`
/// builds were affected. This path is reached whenever a client leaves
/// `vrefresh` at 0 and lets the kernel compute it, which is legal and what
/// Linux expects (SETCRTC and the atomic MODE_ID blob both land here).
///
/// `None` if the blob is unusable.
pub fn refresh_hz_from_modeinfo(data: &[u8]) -> Option<u64> {
    if data.len() < 28 {
        return None;
    }
    let vrefresh = u32::from_ne_bytes([data[24], data[25], data[26], data[27]]) as u64;
    if vrefresh > 0 {
        return Some(vrefresh);
    }
    let clock_khz = u32::from_ne_bytes([data[0], data[1], data[2], data[3]]) as u64;
    let htotal = u16::from_ne_bytes([data[10], data[11]]) as u64;
    let vtotal = u16::from_ne_bytes([data[20], data[21]]) as u64;
    if clock_khz == 0 || htotal == 0 || vtotal == 0 {
        return None;
    }
    let num = clock_khz * 1000;
    let den = htotal * vtotal;
    Some((num + den / 2) / den)
}

/// Set the synthetic vblank period from a `drm_mode_modeinfo` (68 bytes).
/// Falls back to [`FALLBACK_VBLANK_HZ`] when the mode has no usable refresh.
pub fn set_vblank_period_from_modeinfo(data: &[u8]) {
    let hz = refresh_hz_from_modeinfo(data)
        .unwrap_or(FALLBACK_VBLANK_HZ)
        .max(1);
    VBLANK_PERIOD_NS.store(1_000_000_000 / hz, Ordering::Relaxed);
}

/// Reset synthetic vblank pacing to the 60 Hz fallback (no active mode).
pub fn reset_vblank_period() {
    VBLANK_PERIOD_NS.store(1_000_000_000 / FALLBACK_VBLANK_HZ, Ordering::Relaxed);
}

/// Outcome of a legacy/`PAGE_FLIP` or atomic flip-with-event request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlipError {
    /// A previous flip's completion event has not been posted yet (Linux EBUSY).
    Busy,
    /// Present/scanout failed.
    Failed,
}

/// `want_event`: the request carried `DRM_MODE_PAGE_FLIP_EVENT`. Without it
/// Linux queues nothing; this used to queue a completion regardless, so a
/// client flipping without events (and therefore never reading the fd) grew
/// the event queue without bound and left the fd permanently readable.
pub fn page_flip(
    fb_id: u32,
    crtc_id: u32,
    user_data: u64,
    want_event: bool,
    file: &Arc<DrmFileState>,
) -> Result<(), FlipError> {
    // One outstanding flip-complete per CRTC — same rule as Linux. The
    // coalesced timer keeps a *queue* (never an overwritten `Option`), so a
    // second flip / WAIT_VBLANK never silently drops the first completion — that
    // drop once left wlroots handling a stale event and `wl_list_remove`ing an
    // already-cleared listener (SIGSEGV @ 0x8 in labwc).
    //
    // If a completion is still outstanding, do NOT refuse the flip with EBUSY:
    // this is a software framebuffer with no hardware flip FIFO, and wlroots
    // escalates an unexpected page-flip EBUSY into an output teardown — it drops
    // DRM master and the desktop falls back to the text console ("se dibuja y
    // desaparece"). Deliver the outstanding completion NOW and accept the flip;
    // one event per flip is still preserved, only this frame is un-paced.
    if FLIP_EVENT_PENDING.load(Ordering::Acquire) {
        flush_pending_flip_completions();
        if FLIP_EVENT_PENDING.load(Ordering::Acquire) {
            clear_stale_flip_pending();
        }
        if FLIP_EVENT_PENDING.load(Ordering::Acquire) {
            // Should not reach here after the atomic set-and-push fix in
            // schedule_flip_event, but keep as a safety net. Returning EBUSY
            // here is a last resort; the alternative (proceeding with
            // FLIP_EVENT_PENDING still set) risks a double-delivery.
            return Err(FlipError::Busy);
        }
    }
    let flipped = present_now(fb_id, crtc_id);
    if !flipped {
        return Err(FlipError::Failed);
    }
    if want_event {
        schedule_flip_event(crtc_id, user_data, file);
    }
    Ok(())
}

/// Deliver a page-flip completion at the next synthetic vblank boundary rather
/// than immediately. This is the throttle that keeps a Wayland compositor's
/// frame loop from spinning: it renders a frame, page-flips, then blocks in
/// `poll()`/`read()` on the card fd until we post the flip event here — so the
/// loop runs at most once per [`vblank_period_ns`]. If that slot is already in
/// the past (present took ≥ one period, or the compositor was idle) the event
/// is delivered immediately: waiting *another* full period on top of a missed
/// slot locked the desktop at half rate. Catch-up still never exceeds the
/// active refresh, because a frame that finished on time keeps its remaining
/// wait.
///
/// Pending DRM completions share one `timer_set` arm (labwc/lunarbar used to
/// enqueue a fresh Box per flip/vblank), but the jobs themselves live in a
/// queue — never overwrite — so every successful flip still gets its event.
enum PendingDrmTimer {
    Flip {
        crtc_id: u32,
        user_data: u64,
        /// Deliver onto this open's event queue (Weak so close can Drop).
        file: Weak<DrmFileState>,
    },
    // `seq` is intentionally not stored: we recompute it from `vblank_seq_now()`
    // at delivery time so the event carries the sequence that actually just
    // completed (the timer fires at the next vblank boundary), rather than a
    // sequence that was stale by the time the timer fires. `due_seq` is the
    // vblank the caller asked for (`drm_wait_vblank.request.sequence`,
    // absolute): the job stays queued, one vblank tick at a time, until the
    // counter reaches it.
    Vblank {
        signal: u64,
        due_seq: u32,
        file: Weak<DrmFileState>,
    },
}

lazy_static::lazy_static! {
    static ref PENDING_DRM_TIMERS: Mutex<VecDeque<PendingDrmTimer>> =
        Mutex::new(VecDeque::new());
}
static DRM_TIMER_ARMED: AtomicBool = AtomicBool::new(false);
/// True between [`schedule_flip_event`] and the matching [`queue_flip_event`].
static FLIP_EVENT_PENDING: AtomicBool = AtomicBool::new(false);

fn deliver_pending_drm_timer() {
    DRM_TIMER_ARMED.store(false, Ordering::Release);
    // Everything scheduled for this vblank boundary completes together — the
    // queue holds at most one Flip (flush_pending_flip_completions drains any
    // outstanding one before accepting a new flip) plus any vblank waits.
    let jobs: Vec<PendingDrmTimer> = PENDING_DRM_TIMERS.lock().drain(..).collect();
    let now_seq = vblank_seq_now();
    for job in jobs {
        match job {
            PendingDrmTimer::Flip {
                crtc_id,
                user_data,
                file,
            } => {
                if let Some(file) = file.upgrade() {
                    queue_flip_event(&file, crtc_id, user_data);
                } else {
                    // drm_file closed before delivery — drop the event.
                    FLIP_EVENT_PENDING.store(false, Ordering::Release);
                }
            }
            PendingDrmTimer::Vblank {
                signal,
                due_seq,
                file,
            } => {
                // Not due yet (wrap-safe compare): keep it for a later vblank.
                if (due_seq.wrapping_sub(now_seq) as i32) > 0 {
                    PENDING_DRM_TIMERS
                        .lock()
                        .push_back(PendingDrmTimer::Vblank {
                            signal,
                            due_seq,
                            file,
                        });
                } else if let Some(file) = file.upgrade() {
                    queue_vblank_event(&file, now_seq, signal);
                }
            }
        }
    }
    // Something scheduled while we delivered — arm one more shot.
    if !PENDING_DRM_TIMERS.lock().is_empty() {
        arm_coalesced_drm_timer_locked();
    }
}

fn arm_coalesced_drm_timer_locked() {
    // `swap(true)` returns the previous value: if it was already armed, done.
    if DRM_TIMER_ARMED.swap(true, Ordering::AcqRel) {
        return;
    }
    let deadline = next_vblank_deadline();
    let now = kernel_hal::timer::timer_now();
    if deadline <= now {
        // Missed the 60 Hz slot: post the completion on this thread so the
        // compositor does not sit idle for another 16.7 ms (see
        // [`next_vblank_deadline`]). `deliver_pending_drm_timer` clears
        // `DRM_TIMER_ARMED`.
        deliver_pending_drm_timer();
        return;
    }
    kernel_hal::timer::timer_set(deadline, Box::new(move |_| deliver_pending_drm_timer()));
}

fn arm_coalesced_drm_timer(pending: PendingDrmTimer) {
    PENDING_DRM_TIMERS.lock().push_back(pending);
    arm_coalesced_drm_timer_locked();
}

/// The pid whose page-flip completion is (or was last) in flight. Flip events
/// belong to the compositor that submitted the flip -- in Linux they are
/// per-drm_file and survive DROP_MASTER; only closing the file destroys them.
/// This tag lets `cancel_events_for_exit` cancel them when THAT process dies
/// (the freed-user_data hazard) without letting any other client's
/// DROP_MASTER/exit swallow a live compositor's completion.
static LAST_FLIP_PID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn schedule_flip_event(crtc_id: u32, user_data: u64, file: &Arc<DrmFileState>) {
    // Set FLIP_EVENT_PENDING and push the job INSIDE the same lock acquisition
    // as arm_coalesced_drm_timer, so that flush_pending_flip_completions can
    // never observe FLIP_EVENT_PENDING = true with an empty queue on SMP. Without
    // this, a timer-interrupt on one CPU could drain the queue between the store
    // and the push on another CPU, leaving a second CPU's concurrent page_flip
    // flush seeing an empty queue yet FLIP_EVENT_PENDING = true -> EBUSY ->
    // wlroots tears down the output on the very first few frames (real hardware,
    // where multi-core interleaving makes the race non-negligible).
    LAST_FLIP_PID.store(current_pid(), Ordering::Relaxed);
    {
        let mut q = PENDING_DRM_TIMERS.lock();
        FLIP_EVENT_PENDING.store(true, Ordering::Release);
        q.push_back(PendingDrmTimer::Flip {
            crtc_id,
            user_data,
            file: Arc::downgrade(file),
        });
    }
    arm_coalesced_drm_timer_locked();
}

/// Deliver every queued page-flip completion to the card fd *now* instead of
/// waiting for the synthetic vblank. Called when a new flip arrives while one
/// is still outstanding: a software framebuffer has no hardware flip FIFO, so
/// refusing the new flip with EBUSY is wrong — wlroots escalates an unexpected
/// page-flip / atomic-commit EBUSY into an output teardown, which drops DRM
/// master and drops the desktop back to the text console. Flushing preserves
/// the "one completion outstanding" invariant (we return to zero pending before
/// accepting the new flip) and still delivers exactly one event per flip — no
/// dropped or overwritten completion, so no stale `wl_listener` free in labwc.
/// Pending vblank-wait jobs keep their own pacing and stay queued.
fn flush_pending_flip_completions() {
    let flips: Vec<(u32, u64, Weak<DrmFileState>)> = {
        let mut q = PENDING_DRM_TIMERS.lock();
        let mut kept = VecDeque::with_capacity(q.len());
        let mut flips = Vec::new();
        while let Some(job) = q.pop_front() {
            match job {
                PendingDrmTimer::Flip {
                    crtc_id,
                    user_data,
                    file,
                } => flips.push((crtc_id, user_data, file)),
                other => kept.push_back(other),
            }
        }
        *q = kept;
        flips
    };
    // queue_flip_event locks the file's event queue and clears FLIP_EVENT_PENDING;
    // the PENDING_DRM_TIMERS guard above is already dropped, so no nested lock.
    for (crtc_id, user_data, file) in flips {
        if let Some(file) = file.upgrade() {
            queue_flip_event(&file, crtc_id, user_data);
        } else {
            FLIP_EVENT_PENDING.store(false, Ordering::Release);
        }
    }
}

/// Self-heal a stale "flip pending" latch.
///
/// A true pending flag with no queued flip is inconsistent and can make
/// userspace see a persistent EBUSY, which wlroots escalates into output
/// teardown. If no queued flip exists, clear the latch and continue.
fn clear_stale_flip_pending() {
    let has_queued_flip = {
        let q = PENDING_DRM_TIMERS.lock();
        q.iter()
            .any(|job| matches!(job, PendingDrmTimer::Flip { .. }))
    };
    if !has_queued_flip {
        FLIP_EVENT_PENDING.store(false, Ordering::Release);
    }
}

/// Deliver a `DRM_EVENT_VBLANK` (from a `WAIT_VBLANK` that asked for an event)
/// once the synthetic vblank counter reaches `due_seq` -- and never before the
/// next vblank boundary even if `due_seq` has already passed, for the same
/// anti-spin reason as [`schedule_flip_event`]. `signal` is the caller's
/// opaque token echoed back in the event's `user_data`.
pub fn schedule_vblank_event(signal: u64, due_seq: u32, file: &Arc<DrmFileState>) {
    arm_coalesced_drm_timer(PendingDrmTimer::Vblank {
        signal,
        due_seq,
        file: Arc::downgrade(file),
    });
}

/// Drop not-yet-posted flip/vblank timer jobs (device-wide).
///
/// Readable event bytes live on each [`DrmFileState`] and are cleared when that
/// file drops. NOT called from `DROP_MASTER` any more -- that was Linux-divergent
/// and broke real-hardware boots: pending DRM events belong to the drm_file that
/// queued them and SURVIVE a master drop (Linux only destroys them when the
/// file closes). Eclipse's single global event stream meant a TRANSIENT
/// DROP_MASTER from a probing client (Xwayland during session bring-up)
/// landed inside the ~one-vblank window between labwc's page-flip and its
/// completion, swallowed the flip event, and wlroots then waited on it
/// forever: the desktop froze on its very first frame until a VT
/// switch-away/back forced seatd to re-enable the session (fresh modeset +
/// flip). Cancellation now happens on file close ([`DrmFileState`]'s Drop) and
/// on the flip owner's EXIT (see `cancel_events_for_exit`).
pub fn cancel_pending_events() {
    PENDING_DRM_TIMERS.lock().clear();
    DRM_TIMER_ARMED.store(false, Ordering::Release);
    FLIP_EVENT_PENDING.store(false, Ordering::Release);
}

/// Drain timer jobs that target a closing drm_file (by raw pointer identity).
fn cancel_pending_timers_for_file(file_ptr: *const DrmFileState) {
    let mut cleared_flip = false;
    {
        let mut q = PENDING_DRM_TIMERS.lock();
        q.retain(|job| {
            let job_ptr = match job {
                PendingDrmTimer::Flip { file, .. } => file.as_ptr(),
                PendingDrmTimer::Vblank { file, .. } => file.as_ptr(),
            };
            if core::ptr::eq(job_ptr, file_ptr) {
                if matches!(job, PendingDrmTimer::Flip { .. }) {
                    cleared_flip = true;
                }
                false
            } else {
                true
            }
        });
    }
    if cleared_flip {
        FLIP_EVENT_PENDING.store(false, Ordering::Release);
    }
}

/// Cancel pending flip/vblank completions IF `pid` is the process whose flip
/// is in flight. Called from `release_process` (process exit): a dead
/// compositor must not leave a timer that later feeds a reader with a stale
/// `user_data`, but an unrelated DRM client's exit must never swallow a live
/// compositor's completion.
pub fn cancel_events_for_exit(pid: u64) {
    if pid != 0 && LAST_FLIP_PID.load(Ordering::Relaxed) == pid {
        kernel_hal::klog_info!(
            "[drm] pid={} exited with its flip completion pending -- cancelling events",
            pid
        );
        cancel_pending_events();
    }
}

/// Advance the synthetic vblank clock and return the monotonic instant the next
/// completion event may fire at: one [`vblank_period_ns`] past the previous vblank.
///
/// Cap at the active refresh when the compositor is on time. When the slot is
/// already in the past, return `now` (catch-up) instead of waiting until the
/// *next* grid boundary: that extra wait was the half-rate lock. Catch-up still
/// never exceeds the active refresh, because a frame that finished inside the
/// period keeps the remaining wait. The completed slot is snapped to the
/// lattice shared with [`vblank_seq_now`], so a catch-up does not leave the
/// phase sitting off-grid.
fn next_vblank_deadline() -> Duration {
    let now = kernel_hal::timer::timer_now();
    let now_ns = u64::try_from(now.as_nanos()).unwrap_or(u64::MAX);
    let period = vblank_period_ns();
    let mut st = DRM_STATE.lock();
    let last_ns = u64::try_from(st.next_vblank.as_nanos()).unwrap_or(0);
    let next_from_last = last_ns.saturating_add(period);
    if next_from_last <= now_ns {
        static CATCHUP_LOGGED: AtomicBool = AtomicBool::new(false);
        if !CATCHUP_LOGGED.swap(true, Ordering::Relaxed) {
            kernel_hal::klog_info!(
                "[drm] vblank catch-up: missed refresh slot ({}us since last vblank) -- \
                 delivering immediately; waiting another full period here was locking half rate",
                now.saturating_sub(st.next_vblank).as_micros()
            );
        }
        // Record the period cell we just completed, not `now`, so the next
        // frame waits only the remainder of the current period.
        let completed = (now_ns / period) * period;
        st.next_vblank = Duration::from_nanos(completed);
        now
    } else {
        st.next_vblank = Duration::from_nanos(next_from_last);
        Duration::from_nanos(next_from_last)
    }
}

/// Present a framebuffer immediately on `crtc_id` without queuing a DRM flip
/// event. Used by SETCRTC/SETPLANE paths that are not page-flip ioctls.
/// Re-blit the compositor's last frame IF its VT is the active one. Called
/// right after a VT switch so returning to the compositor (Ctrl+Alt+F1) shows
/// its last frame immediately instead of a stale/blank screen; a no-op when the
/// switch was TO a text console (which the kernel repaints itself).
pub fn represent_if_owner_active() -> bool {
    let fb = {
        let st = DRM_STATE.lock();
        if st.graphics_vt != Some(kernel_hal::console::active_vt()) {
            return false;
        }
        st.crtc_fb
    };
    if fb == 0 {
        return false;
    }
    scanout(fb)
}

/// Forget the compositor's VT ownership (on DROP_MASTER / compositor exit) so a
/// subsequent text-only session is never gated off the display or input.
pub fn clear_graphics_owner() {
    DRM_STATE.lock().graphics_vt = None;
}

/// One-shot: log the first present so a black-screen bring-up shows whether the
/// compositor is presenting at all, and via which path.
static PRESENT_LOGGED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// One-shot: log the first VT-gated drop so we can tell "compositor never
/// presented" (no present log) from "presents are being suppressed because a
/// text VT is foreground" (this log).
static PRESENT_VT_DROP_LOGGED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

pub fn present_now(fb_id: u32, crtc_id: u32) -> bool {
    present_now_region(fb_id, crtc_id, None)
}

/// Like [`present_now`], but `rect` (`x, y, width, height`) restricts the
/// software-KMS blit to that region instead of the whole frame — see
/// [`scanout_region`]. `DRM_IOCTL_MODE_DIRTYFB`'s clip rects flow through
/// here. Ignored on the hardware-KMS path: a real driver's `page_flip` scans
/// out via its own GPU DMA, not the CPU blit this exists to shrink.
pub fn present_now_region(fb_id: u32, crtc_id: u32, rect: Option<(u32, u32, u32, u32)>) -> bool {
    present_now_checked(fb_id, crtc_id, rect).is_ok()
}

/// [`present_now_region`], but naming the reason on failure — see
/// [`PresentError`]. `SETCRTC`/`SETPLANE` use this so a modeset is not failed
/// with `EIO` over a frame that merely could not be copied.
pub fn present_now_checked(
    fb_id: u32,
    crtc_id: u32,
    rect: Option<(u32, u32, u32, u32)>,
) -> Result<(), PresentError> {
    // Deferred console GSP bring-up: acknowledge the flip to keep the
    // compositor alive, but do not touch the GOP framebuffer / CE path.
    if scanout_paused() {
        set_crtc_fb(crtc_id, fb_id);
        return Ok(());
    }
    // An explicit present is a client putting pixels on this CRTC, so it is on
    // again. See `set_crtc_blanked` for why this un-blanks rather than failing
    // the flip the way Linux does for a disabled CRTC.
    set_crtc_blanked(false);
    if !PRESENT_LOGGED.swap(true, Ordering::Relaxed) {
        // Read `graphics_vt` into a local FIRST: `DRM_STATE.lock()` as a direct
        // argument to `warn!` keeps the MutexGuard temporary alive for the
        // WHOLE macro statement (Rust extends a temporary's lifetime to the end
        // of its enclosing statement) -- i.e. across the log line's formatting
        // and serial write, not just the field read. The VT-gating block right
        // below takes the SAME lock again a few lines later; on a slow serial
        // console that window was wide enough to strand another CPU on it for
        // >8s, tripping the deadlock detector (see `drm.rs` HOLDER traces).
        let graphics_vt = DRM_STATE.lock().graphics_vt;
        // klog (not warn!) so this survives the default LOG=error boot level --
        // this is THE black-screen bring-up line and it was invisible under it.
        kernel_hal::klog_info!(
            "[drm] first present: fb_id={} crtc={} active_vt={} graphics_vt={:?} software_kms={}",
            fb_id,
            crtc_id,
            kernel_hal::console::active_vt(),
            graphics_vt,
            software_kms_active(),
        );
        // One-shot ground truth for the blit's memory type: how THIS context
        // (the compositor's page-table tree, on the CPU actually blitting)
        // maps the boot framebuffer — then retype it to WC in this tree in
        // case the boot-time passes edited a different one (idempotent when
        // the trees share PTEs). A UC mapping here is a 42 MB/s blit and a
        // 7-11 FPS desktop; the two lines say whether the retype ever
        // reached the mapping the present really writes through.
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        {
            kernel_hal::klog_info!(
                "[drm] fb map at first present: {}",
                kernel_hal::x86_64::fb_mapping_diag()
            );
            kernel_hal::x86_64::ensure_framebuffer_wc();
            kernel_hal::klog_info!(
                "[drm] fb map after WC ensure: {}",
                kernel_hal::x86_64::fb_mapping_diag()
            );
            // Quantify the raw store throughput through that mapping: with the
            // PTE verified WC, a UC-like number here pins the 42 MB/s blit on
            // the device side of the BAR, not on the MMU.
            kernel_hal::x86_64::fb_store_bench_klog("first-present ctx");
        }
    }
    // Establish / enforce compositor VT ownership. The first present claims the
    // active VT; later presents while a *different* VT is foreground (the user
    // switched to a text console) are dropped — reported as complete so the
    // compositor's frame loop keeps running, but not blitted over the console.
    {
        let active = kernel_hal::console::active_vt();
        let mut st = DRM_STATE.lock();
        match st.graphics_vt {
            None => st.graphics_vt = Some(active),
            Some(owner) if owner != active => {
                if !PRESENT_VT_DROP_LOGGED.swap(true, Ordering::Relaxed) {
                    // klog so it survives LOG=error: this line means the
                    // compositor IS presenting, but onto a VT that is not the
                    // foreground -- the desktop is being suppressed on purpose.
                    kernel_hal::klog_info!(
                        "[drm] present DROPPED (VT-gated): owner_vt={} active_vt={} -- compositor frames are suppressed because a different VT is foreground",
                        owner, active
                    );
                }
                return Ok(());
            }
            _ => {}
        }
    }
    // Prefer a driver page_flip (NVC57E surfaceflip / CE hwflip) when the
    // driver accepted the fb; fall back to GOP blit so a failed HW flip
    // never blacks the panel.
    let hw = get_primary_driver().and_then(|driver| {
        let driver_fb_id = DRM_STATE
            .lock()
            .framebuffers
            .iter()
            .find(|f| f.id == fb_id)
            .and_then(|f| f.driver_fb_id)?;
        Some(driver.page_flip(driver_fb_id)).filter(|&ok| ok)
    });
    if !hw.unwrap_or(false) {
        scanout_region_checked(fb_id, rect)?;
    }
    set_crtc_fb(crtc_id, fb_id);
    // A DRM client owns the framebuffer now: stop text console drawing.
    claim_graphics_vt();
    Ok(())
}

/// Encode and enqueue a `struct drm_event_vblank` for the given card fd.
///
/// Shared by page-flip completions (`DRM_EVENT_FLIP_COMPLETE`) and vblank waits
/// (`DRM_EVENT_VBLANK`), which use the identical 32-byte wire layout — only the
/// `type` field distinguishes them for libdrm's event dispatcher.
fn push_drm_event(file: &DrmFileState, ev_type: u32, crtc_id: u32, seq: u32, user_data: u64) {
    let now = kernel_hal::timer::timer_now();
    // struct drm_event_vblank { u32 type; u32 length; u64 user_data;
    //   u32 tv_sec; u32 tv_usec; u32 sequence; u32 crtc_id; }  (32 bytes)
    // Fixed stack buffer — no heap alloc from the timer IRQ path.
    let mut buf = [0u8; 32];
    buf[0..4].copy_from_slice(&ev_type.to_ne_bytes());
    buf[4..8].copy_from_slice(&32u32.to_ne_bytes());
    buf[8..16].copy_from_slice(&user_data.to_ne_bytes());
    buf[16..20].copy_from_slice(&(now.as_secs() as u32).to_ne_bytes());
    buf[20..24].copy_from_slice(&now.subsec_micros().to_ne_bytes());
    buf[24..28].copy_from_slice(&seq.to_ne_bytes());
    buf[28..32].copy_from_slice(&crtc_id.to_ne_bytes());
    file.push_event(buf.to_vec());
}

/// Enqueue a `DRM_EVENT_FLIP_COMPLETE` for a completed page flip.
fn queue_flip_event(file: &DrmFileState, crtc_id: u32, user_data: u64) {
    const DRM_EVENT_FLIP_COMPLETE: u32 = 2;
    // Linux stamps a flip completion with the CRTC's vblank counter -- the
    // same counter WAIT_VBLANK reports. This used to be a private per-flip
    // count from 0, so a client that read the MSC via WAIT_VBLANK (millions,
    // time-based) and then got completions in the hundreds saw the MSC run
    // backwards (Xorg Present's target-MSC scheduling).
    let seq = vblank_seq_now();
    push_drm_event(file, DRM_EVENT_FLIP_COMPLETE, crtc_id, seq, user_data);
    // Flip is complete from the KMS POV once the event is on the card fd —
    // a new PAGE_FLIP may be accepted even before userspace reads it.
    FLIP_EVENT_PENDING.store(false, Ordering::Release);
}

/// Enqueue a `DRM_EVENT_VBLANK` for a `WAIT_VBLANK` request that asked for an
/// event (`_DRM_VBLANK_EVENT`) instead of blocking.
pub fn queue_vblank_event(file: &DrmFileState, seq: u32, user_data: u64) {
    const DRM_EVENT_VBLANK: u32 = 1;
    push_drm_event(file, DRM_EVENT_VBLANK, SYNTH_CRTC_ID, seq, user_data);
}

/// Synthetic vertical-blank counter derived from the monotonic clock and the
/// active CRTC mode's refresh ([`vblank_period_ns`]).
///
/// A software framebuffer has no real vblank interrupt, but `WAIT_VBLANK`
/// callers expect a monotonically increasing sequence; deriving one from time
/// keeps both absolute and relative queries sane. Uses the same period as
/// flip pacing so MSC and flip completions cannot disagree.
pub fn vblank_seq_now() -> u32 {
    let now_ns = u64::try_from(kernel_hal::timer::timer_now().as_nanos()).unwrap_or(0);
    (now_ns / vblank_period_ns()) as u32
}

/// Monotonic deadline at which the synthetic vblank counter reaches `target`,
/// or `None` if that sequence is already in the past.
///
/// The counter is a pure function of the clock ([`vblank_seq_now`]), so the
/// deadline is exact rather than a poll interval — a caller can sleep straight
/// to it instead of waking up to re-check. `target` is the 32-bit sequence the
/// uAPI carries, so the comparison is wrap-safe: a request made across a
/// wrap-around still resolves to "soon", not "two years from now".
pub fn vblank_deadline_for_seq(target: u32) -> Option<Duration> {
    let now_ns = u64::try_from(kernel_hal::timer::timer_now().as_nanos()).unwrap_or(0);
    let period = vblank_period_ns();
    let now_seq_full = now_ns / period;
    let ahead = target.wrapping_sub(now_seq_full as u32) as i32;
    if ahead <= 0 {
        return None;
    }
    let full = now_seq_full.saturating_add(ahead as u64);
    Some(Duration::from_nanos(full.saturating_mul(period)))
}

/// Create a KMS property blob (`DRM_IOCTL_MODE_CREATEPROPBLOB`) and return its
/// id. `user_created` distinguishes client blobs (destroyable) from
/// kernel-owned ones (current mode), mirroring Linux's ownership rule.
pub fn create_blob(data: Vec<u8>, user_created: bool) -> u32 {
    let mut state = DRM_STATE.lock();
    let id = state.next_blob_id;
    state.next_blob_id += 1;
    state.blobs.push(DrmBlob {
        id,
        user_created,
        data,
    });
    id
}

/// Look up a blob's contents (`DRM_IOCTL_MODE_GETPROPBLOB`).
pub fn get_blob(id: u32) -> Option<Vec<u8>> {
    DRM_STATE
        .lock()
        .blobs
        .iter()
        .find(|b| b.id == id)
        .map(|b| b.data.clone())
}

/// Outcome of `DRM_IOCTL_MODE_DESTROYPROPBLOB` (Linux: ENOENT / EPERM split).
pub enum BlobDestroy {
    /// Blob removed.
    Destroyed,
    /// No blob with that id.
    NotFound,
    /// Kernel-created blob (current mode, …): only the creator may destroy.
    KernelOwned,
}

/// Destroy a user-created blob.
pub fn destroy_blob(id: u32) -> BlobDestroy {
    let mut state = DRM_STATE.lock();
    match state.blobs.iter().position(|b| b.id == id) {
        None => BlobDestroy::NotFound,
        Some(pos) if !state.blobs[pos].user_created => BlobDestroy::KernelOwned,
        Some(pos) => {
            state.blobs.remove(pos);
            BlobDestroy::Destroyed
        }
    }
}

/// Snapshot `(atomic KMS state, current CRTC fb id)` for property readback.
pub fn atomic_snapshot() -> (AtomicKmsState, u32) {
    let state = DRM_STATE.lock();
    (state.atomic, state.crtc_fb)
}

/// Staged property updates parsed from one `DRM_IOCTL_MODE_ATOMIC` request
/// against the synthetic (software-KMS) pipeline. `None` = property not in
/// the request; object state not touched persists across commits, as the
/// atomic uAPI requires.
#[derive(Default, Clone, Copy)]
pub struct AtomicUpdate {
    /// Plane "FB_ID" (`Some(0)` disables the plane).
    pub plane_fb_id: Option<u32>,
    /// Plane "CRTC_ID" (`Some(0)` detaches the plane).
    pub plane_crtc_id: Option<u32>,
    /// Plane "CRTC_X".
    pub crtc_x: Option<i32>,
    /// Plane "CRTC_Y".
    pub crtc_y: Option<i32>,
    /// Plane "CRTC_W".
    pub crtc_w: Option<u32>,
    /// Plane "CRTC_H".
    pub crtc_h: Option<u32>,
    /// Plane "SRC_X" (16.16 fixed point).
    pub src_x: Option<u32>,
    /// Plane "SRC_Y" (16.16).
    pub src_y: Option<u32>,
    /// Plane "SRC_W" (16.16).
    pub src_w: Option<u32>,
    /// Plane "SRC_H" (16.16).
    pub src_h: Option<u32>,
    /// Plane "IN_FENCE_FD" (`-1` = none). The wait happens before the commit
    /// reaches here, in `DrmDev::atomic_in_fence_sleep`: `atomic_commit` is
    /// synchronous and has no process context to resolve the fd against, so
    /// the async syscall path resolves and waits, and this field records that
    /// the property was accepted.
    pub in_fence_fd: Option<i32>,
    /// CRTC "ACTIVE".
    pub active: Option<bool>,
    /// CRTC "MODE_ID" blob id (`Some(0)` clears the mode).
    pub mode_blob: Option<u32>,
    /// CRTC "OUT_FENCE_PTR": userspace `*mut i32` to receive a sync_file fd.
    /// Real out-fences need HW flip completion; we write a signaled stub fd
    /// (or `-1`) so clients do not SIGBUS on an uninitialized pointer.
    pub out_fence_ptr: Option<u64>,
    /// Connector "CRTC_ID" (`Some(0)` detaches the connector).
    pub connector_crtc_id: Option<u32>,
    /// Plane "FB_DAMAGE_CLIPS": blob id holding an array of `drm_mode_rect`,
    /// the region the client actually repainted. `Some(0)` or absent means
    /// the whole plane is damaged, as in Linux.
    pub damage_clips: Option<u32>,
}

/// One `struct drm_mode_rect` (`drm_mode.h`): an inclusive-exclusive damage
/// rectangle in framebuffer pixels. Signed, because the uAPI is.
const DRM_MODE_RECT_SIZE: usize = 16;

/// Resolve an `FB_DAMAGE_CLIPS` blob into one bounding rectangle clamped to
/// the framebuffer, or `None` for "present the whole frame".
///
/// `None` is returned for every case Linux also treats as full damage: no
/// property in the commit, a zero blob id, a blob that is not a whole number
/// of `drm_mode_rect`s, and an empty or degenerate clip list. A damage hint
/// that cannot be trusted must widen to the full frame, never narrow -- the
/// failure mode of guessing small is stale tiles left on screen.
fn damage_rect_from_blob(blob_id: u32, fb_w: u32, fb_h: u32) -> Option<(u32, u32, u32, u32)> {
    if blob_id == 0 {
        return None;
    }
    damage_rect_from_clips(&get_blob(blob_id)?, fb_w, fb_h)
}

/// The parsing half of [`damage_rect_from_blob`], split out so it can be
/// tested without a live blob table.
fn damage_rect_from_clips(data: &[u8], fb_w: u32, fb_h: u32) -> Option<(u32, u32, u32, u32)> {
    if fb_w == 0 || fb_h == 0 {
        return None;
    }
    if data.is_empty() || !data.len().is_multiple_of(DRM_MODE_RECT_SIZE) {
        return None;
    }
    let mut union: Option<(u32, u32, u32, u32)> = None;
    for chunk in data.as_chunks::<DRM_MODE_RECT_SIZE>().0 {
        let rd =
            |o: usize| i32::from_ne_bytes([chunk[o], chunk[o + 1], chunk[o + 2], chunk[o + 3]]);
        let (x1, y1, x2, y2) = (rd(0), rd(4), rd(8), rd(12));
        if x2 <= x1 || y2 <= y1 {
            continue;
        }
        // Clamp into the framebuffer. A clip reaching outside it is not a
        // reason to refuse the commit (Linux does not), just to trim.
        let x1 = x1.max(0) as u32;
        let y1 = y1.max(0) as u32;
        let x2 = (x2.max(0) as u32).min(fb_w);
        let y2 = (y2.max(0) as u32).min(fb_h);
        if x2 <= x1 || y2 <= y1 {
            continue;
        }
        union = Some(match union {
            Some((ux, uy, uw, uh)) => {
                let nx = ux.min(x1);
                let ny = uy.min(y1);
                let fx = (ux + uw).max(x2);
                let fy = (uy + uh).max(y2);
                (nx, ny, fx - nx, fy - ny)
            }
            None => (x1, y1, x2 - x1, y2 - y1),
        });
    }
    union
}

/// Why an atomic check/commit was refused. Mapped to errno by the ioctl
/// dispatcher (`Invalid` → EINVAL, `NotFound` → ENOENT, `Device` → EIO,
/// `Busy` → EBUSY).
pub enum AtomicError {
    /// Malformed value, rect out of bounds, or a modeset without
    /// `DRM_MODE_ATOMIC_ALLOW_MODESET`.
    Invalid,
    /// A referenced object (framebuffer, blob, CRTC) does not exist.
    NotFound,
    /// The present itself failed.
    Device,
    /// A previous flip-complete event is still outstanding.
    Busy,
}

/// Validate and (unless `test_only`) apply an atomic update, queueing one
/// page-flip event when `want_event` — the `DRM_MODE_ATOMIC_TEST_ONLY` /
/// `ALLOW_MODESET` / `PAGE_FLIP_EVENT` semantics of `DRM_IOCTL_MODE_ATOMIC`.
///
/// The pipeline is fixed (one CRTC/connector/plane, one mode), so "check"
/// means: referenced objects exist, the mode blob is a well-formed
/// `drm_mode_modeinfo` matching the native mode, source rects fit the
/// framebuffer, and mode/active changes carry `ALLOW_MODESET`.
/// Put back the state [`atomic_commit`] saved before its commit phase, so a
/// commit that fails at the present is all-or-nothing the way Linux's is.
fn restore_atomic_state(saved: (AtomicKmsState, u32, Option<(u32, Vec<u8>)>)) {
    let (atomic, crtc_fb, blob) = saved;
    let mut state = DRM_STATE.lock();
    state.atomic = atomic;
    state.crtc_fb = crtc_fb;
    if let Some((id, data)) = blob {
        if let Some(existing) = state.blobs.iter_mut().find(|b| b.id == id) {
            existing.data = data;
        }
    }
    drop(state);
    reset_vblank_period();
    if atomic.mode_blob_id != 0 {
        if let Some(data) = get_blob(atomic.mode_blob_id) {
            set_vblank_period_from_modeinfo(&data);
        }
    }
}

pub fn atomic_commit(
    upd: &AtomicUpdate,
    test_only: bool,
    allow_modeset: bool,
    want_event: bool,
    user_data: u64,
    file: &Arc<DrmFileState>,
) -> Result<(), AtomicError> {
    let (cur, _) = atomic_snapshot();

    // --- Check phase (no state touched) ---
    // [swapchain-diag] Every rejection below is logged at error! so it is
    // visible at LOG=error (the default cmdline): a wlroots "Swapchain for
    // output failed test" is exactly one of these Err returns on a TEST_ONLY
    // commit, and the reason was previously only a debug! line.
    // CRTC references must name the synthetic CRTC (or 0 = detach).
    for crtc_ref in [upd.plane_crtc_id, upd.connector_crtc_id].iter().flatten() {
        if *crtc_ref != 0 && *crtc_ref != SYNTH_CRTC_ID {
            log::error!(
                "[drm] ATOMIC reject (test_only={}): CRTC ref {:#x} is not the synthetic CRTC {:#x}",
                test_only, *crtc_ref, SYNTH_CRTC_ID
            );
            return Err(AtomicError::NotFound);
        }
    }

    // MODE_ID blob: must exist, be exactly one drm_mode_modeinfo (68 bytes),
    // and name the panel's fixed mode — there is nothing else to program.
    let mut mode_change = false;
    if let Some(blob_id) = upd.mode_blob {
        if blob_id != 0 {
            let Some(data) = get_blob(blob_id) else {
                log::error!(
                    "[drm] ATOMIC reject (test_only={}): MODE_ID blob {:#x} not found",
                    test_only,
                    blob_id
                );
                return Err(AtomicError::NotFound);
            };
            if data.len() != 68 {
                log::error!(
                    "[drm] ATOMIC reject (test_only={}): MODE_ID blob {:#x} is {} bytes, expected 68",
                    test_only, blob_id, data.len()
                );
                return Err(AtomicError::Invalid);
            }
            let hdisplay = u16::from_ne_bytes([data[4], data[5]]) as u32;
            let vdisplay = u16::from_ne_bytes([data[14], data[15]]) as u32;
            let (w, h, _) = display_mode().ok_or(AtomicError::Device)?;
            if hdisplay != w || vdisplay != h {
                log::error!(
                    "[drm] ATOMIC reject (test_only={}): requested mode {}x{} != panel fixed mode {}x{} \
                     (wlroots picked a mode we don't scan out; the connector should advertise only {}x{})",
                    test_only, hdisplay, vdisplay, w, h, w, h
                );
                return Err(AtomicError::Invalid);
            }
            mode_change = cur.mode_blob_id == 0;
        } else {
            mode_change = cur.mode_blob_id != 0;
        }
    }
    let active_change = upd.active.map(|a| a != cur.active).unwrap_or(false);
    if (mode_change || active_change) && !allow_modeset {
        // Linux: "[CRTC] requires full modeset" -> EINVAL without the flag.
        log::error!(
            "[drm] ATOMIC reject (test_only={}): mode/active change needs ALLOW_MODESET \
             (mode_change={} active_change={})",
            test_only,
            mode_change,
            active_change
        );
        return Err(AtomicError::Invalid);
    }
    // ACTIVE=1 needs a mode (committed now or earlier).
    let ends_with_mode = match upd.mode_blob {
        Some(b) => b != 0,
        None => cur.mode_blob_id != 0,
    };
    if upd.active == Some(true) && !ends_with_mode {
        log::error!(
            "[drm] ATOMIC reject (test_only={}): ACTIVE=1 but no mode set (upd.mode_blob={:?} cur.mode_blob_id={:#x})",
            test_only, upd.mode_blob, cur.mode_blob_id
        );
        return Err(AtomicError::Invalid);
    }

    // Plane: an attached framebuffer must exist and contain the source rect.
    if let Some(fb_id) = upd.plane_fb_id {
        if fb_id != 0 {
            let Some(fb) = get_fb(fb_id) else {
                log::error!(
                    "[drm] ATOMIC reject (test_only={}): plane FB {:#x} not found (ADDFB2 never registered it?)",
                    test_only, fb_id
                );
                return Err(AtomicError::NotFound);
            };
            if upd.plane_crtc_id == Some(0) {
                // FB without a CRTC is invalid atomic plane state.
                log::error!(
                    "[drm] ATOMIC reject (test_only={}): plane has FB {:#x} but CRTC_ID=0 (no CRTC)",
                    test_only, fb_id
                );
                return Err(AtomicError::Invalid);
            }
            let end_x = (upd.src_x.unwrap_or(cur.src_x) >> 16)
                .saturating_add(upd.src_w.unwrap_or(cur.src_w) >> 16);
            let end_y = (upd.src_y.unwrap_or(cur.src_y) >> 16)
                .saturating_add(upd.src_h.unwrap_or(cur.src_h) >> 16);
            if end_x > fb.width || end_y > fb.height {
                log::error!(
                    "[drm] ATOMIC reject (test_only={}): source rect end ({},{}) exceeds FB {:#x} size {}x{}",
                    test_only, end_x, end_y, fb_id, fb.width, fb.height
                );
                return Err(AtomicError::Invalid);
            }
        }
    }

    if test_only {
        return Ok(());
    }

    // A prior flip's completion is still outstanding. Flush it now — *before*
    // mutating scanout state — rather than refusing the commit with EBUSY:
    // wlroots turns an unexpected atomic-commit EBUSY into an output teardown
    // (DROP_MASTER -> the desktop drops to the text console). See [`page_flip`].
    // With schedule_flip_event now setting FLIP_EVENT_PENDING inside the queue
    // lock (atomically with push_back), flush always finds the job; the inner
    // EBUSY is a safety net that should not trigger in normal operation.
    if want_event && FLIP_EVENT_PENDING.load(Ordering::Acquire) {
        flush_pending_flip_completions();
        if FLIP_EVENT_PENDING.load(Ordering::Acquire) {
            clear_stale_flip_pending();
        }
        if FLIP_EVENT_PENDING.load(Ordering::Acquire) {
            return Err(AtomicError::Busy);
        }
    }

    // --- Commit phase ---
    //
    // Everything below is undone if the present at the end fails. Linux builds
    // a duplicated `drm_atomic_state` and only swaps it in once the check AND
    // the commit tail have succeeded (`drm_atomic_helper_swap_state`), so a
    // failed commit leaves every object exactly as it was. Applying first and
    // failing afterwards left `ACTIVE` and `MODE_ID` reporting a modeset that
    // never reached the screen: wlroots' own connector state then agreed with
    // the readback, so its next commit computed an empty diff and never
    // retried -- a black output the compositor believes is on.
    let rollback = {
        let state = DRM_STATE.lock();
        let blob_id = state.atomic.mode_blob_id;
        let blob = state
            .blobs
            .iter()
            .find(|b| b.id == blob_id)
            .map(|b| (b.id, b.data.clone()));
        (state.atomic, state.crtc_fb, blob)
    };
    {
        let mut state = DRM_STATE.lock();
        if let Some(blob_id) = upd.mode_blob {
            if blob_id == 0 {
                state.atomic.mode_blob_id = 0;
                reset_vblank_period();
            } else if let Some(data) = state
                .blobs
                .iter()
                .find(|b| b.id == blob_id)
                .map(|b| b.data.clone())
            {
                // Copy the client's mode into a kernel-owned blob: the client
                // may DESTROYPROPBLOB its own right after the commit (wlroots
                // does), and MODE_ID readback must survive that.
                set_vblank_period_from_modeinfo(&data);
                let cur_id = state.atomic.mode_blob_id;
                if let Some(existing) = state.blobs.iter_mut().find(|b| b.id == cur_id) {
                    existing.data = data;
                } else {
                    let id = state.next_blob_id;
                    state.next_blob_id += 1;
                    state.blobs.push(DrmBlob {
                        id,
                        user_created: false,
                        data,
                    });
                    state.atomic.mode_blob_id = id;
                }
            }
        }
        if let Some(a) = upd.active {
            state.atomic.active = a;
        }
        let a = &mut state.atomic;
        if let Some(v) = upd.crtc_x {
            a.crtc_x = v;
        }
        if let Some(v) = upd.crtc_y {
            a.crtc_y = v;
        }
        if let Some(v) = upd.crtc_w {
            a.crtc_w = v;
        }
        if let Some(v) = upd.crtc_h {
            a.crtc_h = v;
        }
        if let Some(v) = upd.src_x {
            a.src_x = v;
        }
        if let Some(v) = upd.src_y {
            a.src_y = v;
        }
        if let Some(v) = upd.src_w {
            a.src_w = v;
        }
        if let Some(v) = upd.src_h {
            a.src_h = v;
        }
    }

    // ACTIVE=0 turns the pipe off, as `drm_atomic_helper_commit` does for a
    // CRTC whose new state is inactive. Staging it and never acting on it was
    // the atomic half of "a screen that cannot be blanked".
    if upd.active == Some(false) {
        set_crtc_blanked(true);
    }

    match upd.plane_fb_id {
        Some(0) => set_crtc_fb(SYNTH_CRTC_ID, 0),
        Some(fb_id) => {
            // Honour FB_DAMAGE_CLIPS. Without it every commit presented the
            // whole framebuffer no matter how little changed -- 8.3 MB of CPU
            // stores at 1080p for a moved cursor or a blinking caret.
            let rect = upd.damage_clips.and_then(|blob_id| {
                let fb = get_fb(fb_id)?;
                damage_rect_from_blob(blob_id, fb.width, fb.height)
            });
            if !present_now_region(fb_id, SYNTH_CRTC_ID, rect) {
                restore_atomic_state(rollback);
                return Err(AtomicError::Device);
            }
        }
        _ => {}
    }

    if want_event {
        // One event per CRTC in the commit; the pipeline has exactly one.
        // Paced to the synthetic vblank (not delivered now) for the same
        // anti-spin reason as the legacy page-flip path. Busy was checked
        // above before present.
        schedule_flip_event(SYNTH_CRTC_ID, user_data, file);
    }
    Ok(())
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

/// The pid to charge a new GEM object to, or 0 when there is no current thread
/// (a buffer allocated during boot, which no process exit should ever reclaim).
///
/// `pub(super)`: also called from `drm_scheme.rs`'s ioctl dispatch, to push
/// the caller's pid down into `DrmScheme::ioctl_owned` for drivers with
/// their own per-process resources (see `NvidiaGpu::nouveau_release_process`).
pub(super) fn current_pid() -> u64 {
    use zircon_object::object::KernelObject;
    kernel_hal::thread::get_current_thread()
        .and_then(|t| t.downcast::<zircon_object::task::Thread>().ok())
        .map(|t| t.proc().id())
        .unwrap_or(0)
}

/// Release every GEM object owned by `pid`, plus any framebuffer that was built
/// on one. Called once per process teardown.
///
/// This is the counterpart Linux gets for free from `drm_gem_release()` on the
/// DRM fd's last close. Without it a dumb buffer outlived its creator forever:
/// Xorg's modesetting driver allocates full-screen dumb buffers, the XFCE
/// session dies and respawns, and each generation's buffers stayed committed --
/// physically contiguous, up to 64 MiB each, owned by no address space.
///
/// Safe to do at process teardown specifically: the process's mappings are gone
/// by then, so nothing can still be writing through the `VmObject::new_physical`
/// view that `DrmDev::get_vmo` handed out over the same frames.
///
/// The driver's `free_buffer` runs with `DRM_STATE` RELEASED, for the reason
/// `snapshot_drivers` documents: the lock is an IRQ-disabling spinlock and a
/// driver call is not guaranteed to be cheap.
pub fn release_process(pid: u64) -> usize {
    if pid == 0 {
        return 0;
    }
    // If this process died with its page-flip completion still pending, drop
    // those events NOW: a later timer must not feed a reader a dead process's
    // user_data. (This is the fd-lifetime cancellation Linux does at file
    // close; DROP_MASTER deliberately no longer cancels events -- see
    // cancel_pending_events.)
    cancel_events_for_exit(pid);
    // Driver-private (nouveau `GEM_NEW`) framebuffers first, and BEFORE the
    // early return below: `nouveau_release_process`, which runs right after
    // this hook, gives their memory back to the RM, and the framebuffer holds
    // no reference that could keep it -- so a framebuffer left behind here
    // means `crtc_fb` aimed at freed GEM memory and the next repaint blitting
    // whatever took its place. This has to happen while `gem_mmap` still
    // records who held what.
    //
    // It cannot live inside the block below, because that returns early when
    // the pid owns no entry in `state.handles` -- and a compositor using the
    // GL/Vulkan renderer owns NOTHING there: its buffers are all nouveau GEM
    // objects tracked in `gem_mmap`. That early return is precisely why a
    // crashed wlroots left its scanout framebuffer in place.
    //
    // Dumb buffers are deliberately not touched here; see
    // `retire_framebuffers_for_handle` for why their fb outlives its handle.
    {
        let mut state = DRM_STATE.lock();
        let before = state.framebuffers.len();
        let taken: Vec<(u32, u32)> = state
            .framebuffers
            .iter()
            .filter(|fb| {
                fb.gem_handle_id >= zcore_drivers::scheme::gem_mmap::DRIVER_HANDLE_BASE
                    && zcore_drivers::scheme::gem_mmap::holds(fb.gem_handle_id, pid)
            })
            .map(|fb| (fb.id, fb.gem_handle_id))
            .collect();
        state.framebuffers.retain(|fb| {
            fb.gem_handle_id < zcore_drivers::scheme::gem_mmap::DRIVER_HANDLE_BASE
                || !zcore_drivers::scheme::gem_mmap::holds(fb.gem_handle_id, pid)
        });
        for (fb_id, _) in &taken {
            note_fb_retired(&mut state, *fb_id, FbRetired::ProcessExited);
        }
        if state.framebuffers.len() != before {
            let live: Vec<u32> = state.framebuffers.iter().map(|fb| fb.id).collect();
            state.fb_backing.retain(|(id, _)| live.contains(id));
            if !state.framebuffers.iter().any(|fb| fb.id == state.crtc_fb) {
                state.crtc_fb = 0;
            }
        }
        drop(state);
        // Outside the lock: these framebuffers are gone, so the references they
        // held go with them. Otherwise a compositor that crashed would leak
        // every scanout buffer it ever had -- `release_pid`, which runs next in
        // `nouveau_release_process`, drops only what the PID itself held, and
        // the fb's reference belongs to no pid at all.
        for (_, handle_id) in taken {
            fb_drop_gem_ref(handle_id);
        }
    }
    let (doomed, driver) = {
        let mut state = DRM_STATE.lock();
        if !state.handles.iter().any(|(_, _, owner)| *owner == pid) {
            return 0;
        }
        let mut doomed = Vec::new();
        state.handles.retain(|(handle, _, owner)| {
            if *owner == pid {
                doomed.push(*handle);
                false
            } else {
                true
            }
        });
        // A framebuffer backed by a handle we just dropped must go too, or
        // scanout would keep presenting freed physical memory.
        state
            .framebuffers
            .retain(|fb| !doomed.iter().any(|h| h.id == fb.gem_handle_id));
        let live_fbs: Vec<u32> = state.framebuffers.iter().map(|fb| fb.id).collect();
        state.fb_backing.retain(|(id, _)| live_fbs.contains(id));
        if !state.framebuffers.iter().any(|fb| fb.id == state.crtc_fb) {
            state.crtc_fb = 0;
        }
        let driver = state.drivers.first().cloned();
        (doomed, driver)
    };
    if let Some(d) = driver {
        for handle in &doomed {
            d.free_buffer(*handle);
        }
    }
    let freed: usize = doomed.iter().map(|h| h.size).sum();
    log::info!(
        "[drm] pid={} exited: released {} GEM object(s), {} KiB",
        pid,
        doomed.len(),
        freed / 1024
    );
    doomed.len()
}

pub fn gem_close(handle_id: u32) -> bool {
    let pid = current_pid();
    let mut state = DRM_STATE.lock();
    if let Some(pos) = state
        .handles
        .iter()
        .position(|(h, _, owner)| h.id == handle_id && owned_by(*owner, pid))
    {
        let (handle, _, _) = state.handles[pos];
        let driver = state.drivers.first().cloned();
        let _ = state.handles.remove(pos);
        drop(state);

        if let Some(d) = driver {
            d.free_buffer(handle);
        }
        true
    } else {
        false
    }
}

/// Snapshot the registered drivers so they can be called with `DRM_STATE`
/// RELEASED. `DRM_STATE`'s `lock::Mutex` is an IRQ-disabling spinlock, and a
/// driver query is NOT guaranteed to be cheap: the NVIDIA RM one runs GSP RPCs
/// and a DDC/EDID probe on its first use (labwc's initial GETRESOURCES /
/// GETCONNECTOR), which can take hundreds of milliseconds — or wedge outright
/// on flaky hardware. Holding the spinlock across that stalls this CPU with
/// interrupts off and piles every other DRM caller (scanout, page flips,
/// events) up behind it, each also spinning with interrupts off: the whole
/// machine freezes. Same rule `alloc_buffer` already documents.
fn snapshot_drivers() -> Vec<Arc<dyn DrmScheme>> {
    DRM_STATE.lock().drivers.clone()
}

pub fn get_resources() -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    let (fbs, drivers) = {
        let state = DRM_STATE.lock();
        let fbs: Vec<u32> = state.framebuffers.iter().map(|fb| fb.id).collect();
        (fbs, state.drivers.clone())
    };

    // Software KMS path: synthesize one CRTC + connector so `drmIsKMS()`
    // passes and wlroots drives output through software scanout. Checked
    // FIRST, before touching any driver: `driver.get_resources()` on the
    // NVIDIA driver runs a live NV0073 display-control + DDC/EDID probe over
    // GSP (see nvidia.rs `rm_display_state`), and both GPU instances — the
    // console GPU AND the compute GPU — are registered as DRM drivers. Calling
    // it here would drive display RPCs through the compute GPU on every
    // GETRESOURCES and then throw the result away for the synthetic topology.
    if software_kms_active() {
        debug!(
            "[drm] GETRESOURCES: software KMS -> 1 crtc, 1 connector ({:?})",
            display_mode(),
        );
        return (fbs, vec![SYNTH_CRTC_ID], vec![SYNTH_CONNECTOR_ID]);
    }

    // No framebuffer display: fall back to driver-provided resources. When a
    // hardware-KMS driver is present, only include resources from hardware-KMS
    // drivers. Mixing non-KMS drivers (e.g. VirtIO) alongside them produces a
    // broken DRM topology (multiple CRTCs sharing one synthetic encoder), which
    // makes wlroots fail with "Failed to create DRM backend". If no driver has
    // hardware KMS, include all drivers (e.g. VirtIO-only with no display).
    let has_hardware_kms = drivers.iter().any(|d| d.has_hardware_kms());
    let mut crtcs: Vec<u32> = Vec::new();
    let mut connectors: Vec<u32> = Vec::new();
    for driver in &drivers {
        if !has_hardware_kms || driver.has_hardware_kms() {
            let (_, d_crtcs, d_conns) = driver.get_resources();
            // De-duplicate IDs that can appear across multiple drivers sharing
            // the same global DRM_STATE (e.g. two NVIDIA GPUs that both return
            // the same synthetic CRTC/connector IDs). Duplicate IDs confuse
            // wlroots into creating multiple outputs with identical resource
            // IDs, which leads to 0×0 dumb-buffer allocations and EINVAL.
            for id in d_crtcs {
                if !crtcs.contains(&id) {
                    crtcs.push(id);
                }
            }
            for id in d_conns {
                if !connectors.contains(&id) {
                    connectors.push(id);
                }
            }
        }
    }

    warn!(
        "[drm] GETRESOURCES: crtcs={} connectors={} fbs={} (driver-provided, NO framebuffer display -> scanout will NOT blit)",
        crtcs.len(),
        connectors.len(),
        fbs.len()
    );
    (fbs, crtcs, connectors)
}

pub fn get_connector(id: u32) -> Option<DrmConnector> {
    // Software KMS: serve the synthetic connector directly. Do NOT fall out to
    // the drivers first — `nvidia.get_connector` runs a live GSP/DDC EDID probe
    // (rm_display_state) before it even range-checks the id, so a GETCONNECTOR
    // for the synthetic id would still drive an RPC on both GPUs (incl. the
    // compute GPU) and stall wlroots' bind. See get_resources for detail.
    if !software_kms_active() {
        // Driver calls run with DRM_STATE released — see `snapshot_drivers`.
        for driver in snapshot_drivers() {
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
    // Prefer the real panel size from the UEFI-captured EDID (bytes 21/22 =
    // max image size in cm); fall back to a ~96 DPI estimate from the mode.
    let (mm_width, mm_height) = match zcore_drivers::display::boot_edid() {
        Some((e, len)) if len >= 23 && (e[21] != 0 || e[22] != 0) => {
            (e[21] as u32 * 10, e[22] as u32 * 10)
        }
        _ => ((w * 254 / 960).max(1), (h * 254 / 960).max(1)),
    };
    Some(DrmConnector {
        id: SYNTH_CONNECTOR_ID,
        connected: true,
        mm_width,
        mm_height,
        connector_type: 11,
    })
}

pub fn get_connector_edid(id: u32) -> Option<[u8; 128]> {
    // Driver calls run with DRM_STATE released — see `snapshot_drivers`.
    for driver in snapshot_drivers() {
        if let Some(edid) = driver.get_connector_edid(id) {
            return Some(edid);
        }
    }
    if id == SYNTH_CONNECTOR_ID {
        if let Some((e, len)) = zcore_drivers::display::boot_edid() {
            if len >= 128 {
                return Some(e);
            }
        }
    }
    None
}

pub fn get_crtc(id: u32) -> Option<DrmCrtc> {
    // Software KMS: serve the synthetic CRTC directly (see get_resources —
    // avoids driving a GSP/EDID probe through the drivers).
    if !software_kms_active() {
        // Driver calls run with DRM_STATE released — see `snapshot_drivers`.
        for driver in snapshot_drivers() {
            if let Some(mut crtc) = driver.get_crtc(id) {
                // Keep userspace-facing fb ids in the DRM core namespace.
                // Read crtc_fb once (a second lock could observe a torn update).
                let crtc_fb = DRM_STATE.lock().crtc_fb;
                if crtc_fb != 0 {
                    crtc.fb_id = crtc_fb;
                }
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
    // Driver calls run with DRM_STATE released — see `snapshot_drivers`.
    // Mirror the get_resources() filter: when a hardware-KMS driver exists,
    // only expose its planes to avoid a mixed 2-plane topology.
    let drivers = snapshot_drivers();
    let has_hardware_kms = drivers.iter().any(|d| d.has_hardware_kms());
    let mut planes = Vec::new();
    for driver in &drivers {
        if !has_hardware_kms || driver.has_hardware_kms() {
            planes.extend(driver.get_planes());
        }
    }
    planes
}

pub fn get_plane(id: u32) -> Option<DrmPlane> {
    // Software KMS: serve the synthetic plane directly (see get_resources —
    // avoids driving a GSP/EDID probe through the drivers).
    if !software_kms_active() {
        // Driver calls run with DRM_STATE released — see `snapshot_drivers`.
        for driver in snapshot_drivers() {
            if let Some(mut plane) = driver.get_plane(id) {
                let crtc_fb = DRM_STATE.lock().crtc_fb;
                if crtc_fb != 0 {
                    plane.fb_id = crtc_fb;
                }
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

#[cfg(test)]
pub(super) mod test_globals {
    extern crate std;

    /// `DRM_STATE`, the GEM table, `gem_mmap` and the CRTC's current fb are
    /// process-wide, and cargo runs a crate's tests in threads. Every test
    /// module that touches them takes this first, so one test's framebuffers
    /// are not another's.
    ///
    /// Without it the suite failed about one run in two with a different test
    /// each time -- `crtc_fb` still holding a neighbour's framebuffer, or an
    /// `ADDFB2` refused because a neighbour had filled the table. That reads
    /// as a real regression and is not one, which is the worst kind of noise
    /// to leave in a suite people are meant to trust.
    static LOCK: self::std::sync::Mutex<()> = self::std::sync::Mutex::new(());

    pub(crate) fn lock() -> self::std::sync::MutexGuard<'static, ()> {
        // A test that panics while holding this poisons it. The poison is not
        // a failure for the tests that follow, so step over it -- the panicking
        // test has already been reported.
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod release_tests {
    use super::*;

    /// Hand-build a GEM entry owned by `pid`, bypassing `alloc_buffer` (which
    /// would need a current thread and real contiguous frames). What is under
    /// test is the bookkeeping: who owns an entry and what releases it.
    fn plant(id: u32, size: usize, pid: u64) {
        let vmo = VmObject::new_paged(1);
        let handle = GemHandle {
            id,
            size,
            phys_addr: 0,
        };
        DRM_STATE.lock().handles.push((handle, vmo, pid));
    }

    fn live_ids() -> Vec<u32> {
        DRM_STATE
            .lock()
            .handles
            .iter()
            .map(|(h, _, _)| h.id)
            .collect()
    }

    #[test]
    fn process_exit_releases_only_that_process_buffers() {
        let _serialised = super::test_globals::lock();
        // Distinct ids/pids so this cannot collide with another test's state.
        plant(9001, 4096, 77_001);
        plant(9002, 8192, 77_001);
        plant(9003, 4096, 77_002);

        // The bug: nothing but an explicit ioctl ever dropped these, so a
        // process that died without one leaked every buffer it had made.
        assert_eq!(release_process(77_001), 2);

        let ids = live_ids();
        assert!(!ids.contains(&9001), "9001 should be gone with its owner");
        assert!(!ids.contains(&9002), "9002 should be gone with its owner");
        assert!(ids.contains(&9003), "another process's buffer must survive");

        // Idempotent: a second teardown for the same pid frees nothing more.
        assert_eq!(release_process(77_001), 0);

        assert_eq!(release_process(77_002), 1);
        assert!(!live_ids().contains(&9003));
    }

    #[test]
    fn unowned_buffers_are_never_reclaimed() {
        let _serialised = super::test_globals::lock();
        // pid 0 means "allocated with no current thread" (boot-time). A process
        // exit must never take those: pid 0 is not a real owner.
        plant(9101, 4096, 0);
        assert_eq!(release_process(0), 0);
        assert!(live_ids().contains(&9101));
        DRM_STATE.lock().handles.retain(|(h, _, _)| h.id != 9101);
    }

    #[test]
    fn releasing_a_buffer_drops_the_framebuffer_built_on_it() {
        let _serialised = super::test_globals::lock();
        plant(9201, 4096, 77_003);
        {
            let mut state = DRM_STATE.lock();
            state.framebuffers.push(DrmFramebuffer {
                id: 9299,
                driver_fb_id: None,
                gem_handle_id: 9201,
                width: 1,
                height: 1,
                pitch: 4,
                phys_addr: 0,
                size: 4096,
                owner: 77_003,
            });
            state.crtc_fb = 9299;
        }
        assert_eq!(release_process(77_003), 1);
        let state = DRM_STATE.lock();
        assert!(
            !state.framebuffers.iter().any(|fb| fb.id == 9299),
            "a framebuffer over a freed handle would scan out released memory"
        );
        assert_eq!(state.crtc_fb, 0, "the CRTC must not point at a dropped fb");
    }

    /// Linux semantics: closing the handle drops the *handle*, not the
    /// object. A framebuffer built on it keeps the memory (its own `Arc` on
    /// the VMO) and stays scannable until RMFB, which then releases it.
    #[test]
    fn gem_close_keeps_a_framebuffer_and_its_memory_alive() {
        let _serialised = super::test_globals::lock();
        plant(9301, 4096, 77_004);
        let vmo = handle_vmo(9301).expect("planted handle resolves to its VMO");
        {
            let mut state = DRM_STATE.lock();
            state.framebuffers.push(DrmFramebuffer {
                id: 9399,
                driver_fb_id: None,
                gem_handle_id: 9301,
                width: 1,
                height: 1,
                pitch: 4,
                phys_addr: 0,
                size: 4096,
                owner: 77_004,
            });
            state.fb_backing.push((9399, vmo.clone()));
            state.crtc_fb = 9399;
        }
        // Two owners besides the handle table: the test's `vmo` and the fb.
        assert_eq!(Arc::strong_count(&vmo), 3);

        assert!(gem_close(9301), "the handle existed");
        assert!(handle_vmo(9301).is_none(), "the handle is gone");
        {
            let state = DRM_STATE.lock();
            assert!(
                state.framebuffers.iter().any(|fb| fb.id == 9399),
                "the fb outlives its handle"
            );
            assert_eq!(state.crtc_fb, 9399, "and the CRTC still scans it out");
        }
        // Only the fb's reference dropped away with the handle table entry.
        assert_eq!(Arc::strong_count(&vmo), 2);

        assert!(rmfb(9399));
        assert_eq!(
            Arc::strong_count(&vmo),
            1,
            "RMFB released the fb's reference"
        );
        let state = DRM_STATE.lock();
        assert!(!state.framebuffers.iter().any(|fb| fb.id == 9399));
        assert!(!state.fb_backing.iter().any(|(id, _)| *id == 9399));
        assert_eq!(state.crtc_fb, 0, "RMFB of the CRTC fb unbinds it");
    }
}

#[cfg(test)]
mod damage_tests {
    use super::damage_rect_from_clips;

    /// Build a `drm_mode_rect` blob from `(x1, y1, x2, y2)` tuples.
    fn clips(rects: &[(i32, i32, i32, i32)]) -> alloc::vec::Vec<u8> {
        let mut v = alloc::vec::Vec::new();
        for (x1, y1, x2, y2) in rects {
            for n in [x1, y1, x2, y2] {
                v.extend_from_slice(&n.to_ne_bytes());
            }
        }
        v
    }

    #[test]
    fn single_clip_becomes_that_rect() {
        let b = clips(&[(10, 20, 30, 50)]);
        assert_eq!(
            damage_rect_from_clips(&b, 1920, 1080),
            Some((10, 20, 20, 30))
        );
    }

    #[test]
    fn several_clips_union_into_their_bounding_box() {
        let b = clips(&[(10, 10, 20, 20), (100, 200, 110, 210)]);
        assert_eq!(
            damage_rect_from_clips(&b, 1920, 1080),
            Some((10, 10, 100, 200))
        );
    }

    /// A clip reaching past the framebuffer is trimmed, not refused -- Linux
    /// does the same.
    #[test]
    fn clips_are_clamped_to_the_framebuffer() {
        let b = clips(&[(1900, 1070, 4000, 4000)]);
        assert_eq!(
            damage_rect_from_clips(&b, 1920, 1080),
            Some((1900, 1070, 20, 10))
        );
    }

    /// Negative origins are part of the uAPI (the fields are signed); clamp
    /// rather than wrap into a huge unsigned rect.
    #[test]
    fn negative_origin_clamps_to_zero() {
        let b = clips(&[(-50, -50, 10, 10)]);
        assert_eq!(damage_rect_from_clips(&b, 1920, 1080), Some((0, 0, 10, 10)));
    }

    /// Every "cannot be trusted" case must widen to the whole frame (None),
    /// never narrow: guessing small leaves stale tiles on screen.
    #[test]
    fn untrustworthy_input_means_full_damage() {
        // Empty list.
        assert_eq!(damage_rect_from_clips(&[], 1920, 1080), None);
        // Not a whole number of drm_mode_rects.
        assert_eq!(damage_rect_from_clips(&[0u8; 20], 1920, 1080), None);
        // Degenerate (x2 <= x1) and inverted rects contribute nothing.
        let b = clips(&[(10, 10, 10, 20), (30, 40, 20, 30)]);
        assert_eq!(damage_rect_from_clips(&b, 1920, 1080), None);
        // Entirely off-screen, so nothing survives the clamp.
        let b = clips(&[(5000, 5000, 6000, 6000)]);
        assert_eq!(damage_rect_from_clips(&b, 1920, 1080), None);
        // A framebuffer with no area.
        let b = clips(&[(0, 0, 10, 10)]);
        assert_eq!(damage_rect_from_clips(&b, 0, 1080), None);
    }

    /// A degenerate clip next to a good one must not poison the union.
    #[test]
    fn degenerate_clips_are_skipped_not_fatal() {
        let b = clips(&[(10, 10, 10, 10), (40, 50, 60, 80)]);
        assert_eq!(
            damage_rect_from_clips(&b, 1920, 1080),
            Some((40, 50, 20, 30))
        );
    }
}

/// The node naming and minor arithmetic, which is what `/dev/dri`,
/// `/sys/class/drm` and `/sys/dev/char` all derive their entries from.
/// Building the table itself needs registered drivers, so it is not testable
/// here; this covers the part that used to be four hand-written names and is
/// now shared arithmetic.
#[cfg(test)]
mod node_tests {
    use super::*;

    #[test]
    fn the_first_gpu_keeps_card0_and_render128() {
        let n = NodeMinors::new(0);
        assert_eq!(n.card(), 0);
        assert_eq!(n.render(), 128);
    }

    #[test]
    fn names_are_derived_for_any_index() {
        // The four the old `match` knew, plus the third GPU that used to come
        // out as the literal string "card?".
        assert_eq!(node_name(0), "card0");
        assert_eq!(node_name(1), "card1");
        assert_eq!(node_name(2), "card2");
        assert_eq!(node_name(128), "renderD128");
        assert_eq!(node_name(129), "renderD129");
        assert_eq!(node_name(130), "renderD130");
    }

    #[test]
    fn a_gpu_owns_exactly_its_own_two_minors() {
        let n = NodeMinors::new(2);
        assert_eq!(n.card(), 2);
        assert_eq!(n.render(), 130);
        assert!(n.owns(2));
        assert!(n.owns(130));
        // Not its neighbours', which is the whole point of the table.
        assert!(!n.owns(1));
        assert!(!n.owns(3));
        assert!(!n.owns(129));
        assert!(!n.owns(131));
    }

    #[test]
    fn no_two_gpus_can_claim_one_minor_below_the_cap() {
        // Every minor under the cap is claimed by at most one GPU. This is
        // what `build_gpu_nodes` stops at, so assert it rather than trusting
        // the constant to have been chosen correctly.
        for i in 0..MAX_GPU_NODES {
            for j in 0..MAX_GPU_NODES {
                if i == j {
                    continue;
                }
                let a = NodeMinors::new(i);
                let b = NodeMinors::new(j);
                assert!(!b.owns(a.card()), "card{} also belongs to GPU {}", i, j);
                assert!(
                    !b.owns(a.render()),
                    "renderD{} also belongs to GPU {}",
                    a.render(),
                    j
                );
            }
        }
    }

    #[test]
    fn the_cap_stays_inside_the_primary_minor_range() {
        // `card{n}` only stops being a primary node name at RENDER_MINOR_BASE,
        // where it would name a render node instead and two GPUs would answer
        // to one minor. The cap is deliberately well short of that, at Linux's
        // own card0..card63 range.
        assert!(MAX_GPU_NODES <= RENDER_MINOR_BASE);
        assert!(NodeMinors::new(MAX_GPU_NODES - 1).card() < RENDER_MINOR_BASE);
        // And past RENDER_MINOR_BASE is where it really breaks, which is why
        // the cap exists at all.
        assert!(NodeMinors::new(0).owns(NodeMinors::new(RENDER_MINOR_BASE).card()));
    }
}

/// Tests for the write-combining edge arithmetic of the present path.
///
/// Nothing here touches a display: [`expand_x_for_wc`] is pure arithmetic, and
/// it is the whole of the mitigation for a hardware behaviour the module
/// documents -- a blit whose left or right edge sits mid-line makes the GOP/BAR1
/// aperture flush a half-full combine buffer over the neighbouring pixels,
/// which is seen as leftover squares and stripes.
#[cfg(test)]
mod wc_edge_tests {
    use super::expand_x_for_wc;

    /// 16 XRGB8888 pixels are one 64-byte PCIe burst. Both edges of the
    /// returned span must sit on such a boundary whenever the limit allows it,
    /// or the burst the hardware combines is not the burst we wrote.
    #[test]
    fn both_edges_land_on_a_sixteen_pixel_boundary() {
        // A damage box in the middle of the screen: 100..150 becomes 96..160.
        let (x, w) = expand_x_for_wc(100, 50, 1920);
        assert_eq!((x, w), (96, 64));
        assert_eq!(x % 16, 0);
        assert_eq!((x + w) % 16, 0);
    }

    /// The expansion may only ever GROW the requested region: a damage rect
    /// that came back clipped would leave the pixels the client just drew
    /// unpresented.
    #[test]
    fn the_expansion_always_covers_what_was_asked_for() {
        for limit in [64u32, 640, 1366, 1920, 1936] {
            for x in 0..limit {
                for w in [1u32, 3, 15, 16, 17, 64, 100] {
                    let (ex, ew) = expand_x_for_wc(x, w, limit);
                    if ew == 0 {
                        // Only when there was nothing inside the limit to draw.
                        assert!(x >= limit, "x={} w={} limit={} vanished", x, w, limit);
                        continue;
                    }
                    assert!(ex <= x, "left edge moved right: {} > {}", ex, x);
                    let want_right = (x + w).min(limit);
                    assert!(
                        ex + ew >= want_right,
                        "right edge {} short of {} (x={} w={} limit={})",
                        ex + ew,
                        want_right,
                        x,
                        w,
                        limit
                    );
                    // And never past the limit, which is the caller's promise
                    // that the bytes are inside the destination row.
                    assert!(
                        ex + ew <= limit,
                        "ran past the limit: {} > {}",
                        ex + ew,
                        limit
                    );
                }
            }
        }
    }

    /// The right limit the present path passes is the PITCH in pixels, not the
    /// visible width, precisely so the tail can spill into a scanline's
    /// off-screen padding and complete the last burst. On a 1366-wide mode
    /// (1366 % 16 == 6) with a padded pitch that is the only way the last six
    /// visible pixels are ever written as part of a whole line.
    #[test]
    fn a_padded_pitch_lets_the_right_edge_reach_its_boundary() {
        // Visible width 1366, pitch 1536 pixels.
        let (x, w) = expand_x_for_wc(1360, 6, 1536);
        assert_eq!((x, w), (1360, 16), "1366 rounds up to 1376");
        assert_eq!(x + w, 1376);
        assert!(x + w > 1366, "the tail is off-screen, which is the point");

        // With no padding at all there is nowhere to put the tail, so the span
        // stops at the limit and stays short of a boundary. That is expected,
        // and is why `scanout_region` prefers the pitch.
        let (x, w) = expand_x_for_wc(1360, 6, 1366);
        assert_eq!((x, w), (1360, 6));
    }

    /// Degenerate inputs must not produce a span at all: an empty damage rect
    /// and a zero-width destination are both "present nothing".
    #[test]
    fn nothing_to_draw_expands_to_nothing() {
        // A zero-width rect short-circuits before any alignment: there is no
        // burst to complete, so the origin is returned as given (clamped).
        assert_eq!(expand_x_for_wc(100, 0, 1920), (100, 0));
        assert_eq!(expand_x_for_wc(0, 0, 1920).1, 0);
        assert_eq!(expand_x_for_wc(0, 64, 0), (0, 0));
        // An origin already outside the destination yields no width, whatever
        // was asked for -- not a wrapped or negative span.
        assert_eq!(expand_x_for_wc(4096, 64, 1920).1, 0);
    }
}

/// Tests for ADDFB2 validation and for the refresh rate the synthetic vblank
/// is paced from. Both are pure functions of their arguments (`create_fb` needs
/// only a GEM entry in `DRM_STATE`, planted the way `release_tests` does).
#[cfg(test)]
mod fb_validation_tests {
    use super::*;

    /// A GEM entry of `size` bytes, owned by `pid`, planted directly so the
    /// test does not need a current thread or real contiguous frames.
    fn plant(id: u32, size: usize, pid: u64) {
        let vmo = VmObject::new_paged(size.div_ceil(4096));
        DRM_STATE.lock().handles.push((
            GemHandle {
                id,
                size,
                phys_addr: 0,
            },
            vmo,
            pid,
        ));
    }

    fn unplant(id: u32) {
        let mut state = DRM_STATE.lock();
        state.handles.retain(|(h, _, _)| h.id != id);
        state.framebuffers.retain(|fb| fb.gem_handle_id != id);
    }

    /// The regression this guards. Every CPU consumer of `fb.pitch` turns it
    /// into a row stride in PIXELS with `fb.pitch / 4` (`src_stride` in
    /// `scanout_region` and in `repaint_for_cursor`), so a pitch that is not a
    /// whole number of XRGB8888 pixels makes each row start a couple of bytes
    /// early -- a shear that grows down the screen. The copy engine does not
    /// truncate, so the two present paths would not even agree on the image.
    /// ADDFB2 used to accept it.
    #[test]
    fn addfb_rejects_a_pitch_that_is_not_whole_pixels() {
        let _serialised = super::test_globals::lock();
        let (w, h) = (64u32, 4u32);
        // Generous backing so the size guard never decides these cases: what
        // is under test is the pitch alignment, nothing else.
        plant(9401, 64 * 1024, 78_001);

        // The aligned pitch for this width is accepted.
        assert!(create_fb(9401, w, h, w * 4).is_some(), "w*4 must be valid");
        // A padded but still 4-byte-aligned pitch is fine too: padding is
        // legal, a fractional pixel is not.
        assert!(create_fb(9401, w, h, w * 4 + 16).is_some());
        // Two bytes past a whole pixel is not.
        assert!(
            create_fb(9401, w, h, w * 4 + 2).is_none(),
            "a fractional pitch would scan out a sheared image"
        );
        for bad in [1u32, 2, 3] {
            assert!(create_fb(9401, w, h, w * 4 + bad).is_none(), "+{}", bad);
        }

        unplant(9401);
    }

    /// The pre-existing guards, asserted so the new pitch check cannot be
    /// mistaken for the whole of the validation: a framebuffer must fit inside
    /// its backing buffer, and must be at least as wide as it claims.
    #[test]
    fn addfb_still_rejects_a_framebuffer_that_does_not_fit_its_buffer() {
        let _serialised = super::test_globals::lock();
        plant(9402, 4096, 78_002);
        // 64x16 at 4 bytes = 4096, exactly the buffer.
        assert!(create_fb(9402, 64, 16, 256).is_some());
        // One row more does not fit.
        assert!(create_fb(9402, 64, 17, 256).is_none());
        // A pitch too small for the claimed width would make `scanout_region`
        // read the next row's pixels as this row's tail.
        assert!(create_fb(9402, 64, 4, 128).is_none());
        // Degenerate sizes are not framebuffers.
        assert!(create_fb(9402, 64, 0, 256).is_none());
        assert!(create_fb(9402, 0, 4, 0).is_none());
        // An unknown handle has no backing to scan out.
        assert!(create_fb(9499, 64, 4, 256).is_none());
        unplant(9402);
    }
}

/// Tests for the refresh rate the synthetic vblank is paced from.
#[cfg(test)]
mod refresh_tests {
    use super::*;

    /// Build the fields of a `drm_mode_modeinfo` that the reader looks at.
    fn modeinfo(clock_khz: u32, htotal: u16, vtotal: u16, vrefresh: u32) -> [u8; 68] {
        let mut m = [0u8; 68];
        m[0..4].copy_from_slice(&clock_khz.to_ne_bytes());
        m[10..12].copy_from_slice(&htotal.to_ne_bytes());
        m[20..22].copy_from_slice(&vtotal.to_ne_bytes());
        m[24..28].copy_from_slice(&vrefresh.to_ne_bytes());
        m
    }

    /// A mode that states its own refresh is believed, timings or not: that is
    /// the field Linux fills in and the one every mode this driver advertises
    /// carries.
    #[test]
    fn a_stated_vrefresh_wins_over_the_timings() {
        assert_eq!(
            refresh_hz_from_modeinfo(&modeinfo(139_900, 2080, 1121, 144)),
            Some(144)
        );
    }

    /// The regression this guards. A pixel clock is stored in whole kHz, so it
    /// cannot express an exact 60 Hz for most timings: the 1920x1080 mode this
    /// driver itself advertises is 139900 kHz over 2080x1121, i.e. 59.9995 Hz.
    /// Truncating division called that 59 and
    /// `set_vblank_period_from_modeinfo` paced the synthetic vblank at 16.95 ms
    /// instead of 16.67 ms. Linux rounds to nearest here
    /// (`drm_mode_vrefresh`'s `DIV_ROUND_CLOSEST`), and a client that leaves
    /// `vrefresh` at 0 -- legal, and what makes the kernel compute it -- is
    /// exactly how SETCRTC and an atomic MODE_ID blob reach this path.
    #[test]
    fn a_derived_refresh_rounds_to_nearest_like_linux() {
        // 1920x1080, the mode `make_modeinfo` builds.
        assert_eq!(
            refresh_hz_from_modeinfo(&modeinfo(139_900, 2080, 1121, 0)),
            Some(60),
            "59.9995 Hz is 60, not 59"
        );
        // A few more of the modes that were reading one Hz slow.
        assert_eq!(
            refresh_hz_from_modeinfo(&modeinfo(65_750, 1440, 761, 0)),
            Some(60),
            "1280x720"
        );
        assert_eq!(
            refresh_hz_from_modeinfo(&modeinfo(74_072, 1526, 809, 0)),
            Some(60),
            "1366x768"
        );
        assert_eq!(
            refresh_hz_from_modeinfo(&modeinfo(528_236, 4000, 2201, 0)),
            Some(60),
            "3840x2160"
        );
        // Rounding to nearest, not simply up: a mode that really is closer to
        // 59 must not be promoted.
        assert_eq!(
            refresh_hz_from_modeinfo(&modeinfo(137_600, 2080, 1121, 0)),
            Some(59)
        );
    }

    /// An unusable blob must not be mistaken for a refresh rate; the caller
    /// falls back to 60 Hz rather than dividing by zero or pacing off garbage.
    #[test]
    fn an_unusable_modeinfo_has_no_refresh() {
        assert_eq!(refresh_hz_from_modeinfo(&[]), None);
        assert_eq!(refresh_hz_from_modeinfo(&[0u8; 27]), None, "truncated blob");
        assert_eq!(refresh_hz_from_modeinfo(&modeinfo(0, 2080, 1121, 0)), None);
        assert_eq!(
            refresh_hz_from_modeinfo(&modeinfo(139_900, 0, 1121, 0)),
            None
        );
        assert_eq!(
            refresh_hz_from_modeinfo(&modeinfo(139_900, 2080, 0, 0)),
            None
        );
    }

    /// The vblank period the timer is armed from. It must never be 0 (a zero
    /// period is an immediately-and-forever-due timer), and it must follow the
    /// mode.
    #[test]
    fn the_vblank_period_tracks_the_mode_and_is_never_zero() {
        set_vblank_period_from_modeinfo(&modeinfo(139_900, 2080, 1121, 0));
        assert_eq!(vblank_period_ns(), 1_000_000_000 / 60);
        set_vblank_period_from_modeinfo(&modeinfo(0, 0, 0, 144));
        assert_eq!(vblank_period_ns(), 1_000_000_000 / 144);
        // No usable refresh at all falls back rather than producing 0.
        set_vblank_period_from_modeinfo(&[0u8; 68]);
        assert_eq!(vblank_period_ns(), 1_000_000_000 / FALLBACK_VBLANK_HZ);
        assert!(vblank_period_ns() > 0);
        reset_vblank_period();
        assert_eq!(vblank_period_ns(), 1_000_000_000 / FALLBACK_VBLANK_HZ);
    }
}

/// Tests for the rectangle algebra the software cursor uses to decide what to
/// repaint. Both helpers are pure, and both feed blits into the scanout
/// aperture, so an over-small union leaves cursor debris and an over-large one
/// costs a full-screen copy per mouse move.
#[cfg(test)]
mod cursor_rect_tests {
    use super::{rects_overlap, union_i32};

    #[test]
    fn touching_rectangles_do_not_overlap() {
        // Adjacent, sharing an edge: repainting one cannot disturb the other.
        assert!(!rects_overlap(0, 0, 10, 10, 10, 0, 10, 10));
        assert!(!rects_overlap(0, 0, 10, 10, 0, 10, 10, 10));
        // One pixel of genuine intersection does.
        assert!(rects_overlap(0, 0, 10, 10, 9, 9, 10, 10));
        // Fully contained.
        assert!(rects_overlap(0, 0, 100, 100, 40, 40, 10, 10));
        // An empty rectangle overlaps nothing, including itself.
        assert!(!rects_overlap(0, 0, 0, 10, 0, 0, 10, 10));
        assert!(!rects_overlap(0, 0, 10, 0, 0, 0, 10, 10));
    }

    /// A cursor can be partly off the left or top edge, so these coordinates
    /// are genuinely negative and the union has to keep them.
    #[test]
    fn a_union_covers_both_rectangles_including_negative_origins() {
        assert_eq!(union_i32(0, 0, 10, 10, 20, 20, 10, 10), (0, 0, 30, 30));
        assert_eq!(union_i32(-5, -5, 10, 10, 0, 0, 10, 10), (-5, -5, 15, 15));
        // Identical rectangles union to themselves.
        assert_eq!(union_i32(7, 9, 3, 4, 7, 9, 3, 4), (7, 9, 3, 4));
        // The union is symmetric.
        assert_eq!(
            union_i32(-3, 12, 8, 2, 40, -1, 5, 60),
            union_i32(40, -1, 5, 60, -3, 12, 8, 2)
        );
    }

    /// The property that matters: whatever the union returns must contain both
    /// inputs, or the repaint misses pixels the cursor moved over.
    #[test]
    fn the_union_contains_both_inputs() {
        let cases = [
            (0i32, 0i32, 64u32, 64u32),
            (-32, -32, 64, 64),
            (1900, 1050, 64, 64),
            (10, 10, 1, 1),
        ];
        for a in cases {
            for b in cases {
                let (ux, uy, uw, uh) = union_i32(a.0, a.1, a.2, a.3, b.0, b.1, b.2, b.3);
                for r in [a, b] {
                    assert!(ux <= r.0, "union left {} > {}", ux, r.0);
                    assert!(uy <= r.1, "union top {} > {}", uy, r.1);
                    assert!(
                        ux + uw as i32 >= r.0 + r.2 as i32,
                        "union right {} < {}",
                        ux + uw as i32,
                        r.0 + r.2 as i32
                    );
                    assert!(
                        uy + uh as i32 >= r.1 + r.3 as i32,
                        "union bottom {} < {}",
                        uy + uh as i32,
                        r.1 + r.3 as i32
                    );
                }
            }
        }
    }
}

/// Tests for the copy-engine staging repack: the buffer the CPU packs a frame
/// into when the GPU's pitched 2D path is unavailable, before the copy engine
/// DMAs it flat into the scanout framebuffer.
///
/// This runs for real under libos -- `VmObject::new_contiguous` and
/// `phys_to_virt` both work on the host -- so the test can read the staging
/// buffer back byte for byte and see exactly what the copy engine would have
/// carried to the screen.
#[cfg(test)]
mod ce_staging_tests {
    use super::*;

    /// Read `len` bytes of the staging buffer back through the physmap, the
    /// same way the copy engine reaches them.
    fn staging_bytes(pa: u64, len: usize) -> alloc::vec::Vec<u8> {
        let va = phys_to_virt(pa as usize);
        // SAFETY: `pa` is the start of the contiguous staging VMO, which the
        // repack just reported as holding at least `len` bytes, and it stays
        // alive in `CE_STAGING` for the rest of the process.
        unsafe { core::slice::from_raw_parts(va as *const u8, len).to_vec() }
    }

    /// Source pixels numbered from `base` so each one is identifiable.
    fn numbered(base: u32, stride: usize, rows: usize) -> alloc::vec::Vec<u32> {
        (0..stride * rows).map(|n| base + n as u32).collect()
    }

    /// The regression this guards. The repack writes `w * 4` bytes per row, but
    /// the caller then asks the copy engine for a FLAT copy of
    /// `dst_pitch * h` bytes -- so the `row_bytes..dst_pitch` tail of every row
    /// is carried to the screen without anyone having written it. A first,
    /// wider frame leaves its pixels there; a later, narrower frame does not
    /// overwrite them, and the copy engine paints them. When that tail is
    /// off-screen padding it is invisible, so the repack now defines it;
    /// when it would start inside the visible width the repack DECLINES (see
    /// the next test) rather than clobbering real columns.
    #[test]
    fn the_row_tail_the_copy_engine_carries_is_never_left_stale() {
        let _serialised = super::test_globals::lock();
        const PITCH: usize = 64; // 16 pixels per destination row
        const H: u32 = 4;
        // Frame 1 fills whole rows: 16 pixels of 4 bytes = the full pitch.
        let wide = numbered(0xAAA0_0000, 16, H as usize);
        let (pa, size) = ce_repack_to_staging(&wide, 0, 16, PITCH, PITCH, 16, H)
            .expect("a whole-row repack must be accepted");
        assert_eq!(size, (PITCH * H as usize) as u64);
        let before = staging_bytes(pa, PITCH * H as usize);
        assert_eq!(
            &before[60..64],
            &0xAAA0_000Fu32.to_ne_bytes(),
            "frame 1 wrote the end of row 0"
        );

        // Frame 2 is narrower -- 8 pixels -- with a destination whose visible
        // part is only those 8 pixels, so the remaining 32 bytes of each row
        // are off-screen padding and the repack is allowed to proceed.
        let narrow = numbered(0xBBB0_0000, 8, H as usize);
        let (pa2, size2) = ce_repack_to_staging(&narrow, 0, 8, PITCH, 32, 8, H)
            .expect("an off-screen tail must still be accepted");
        assert_eq!(size2, (PITCH * H as usize) as u64);
        let after = staging_bytes(pa2, PITCH * H as usize);
        for r in 0..H as usize {
            let row = &after[r * PITCH..(r + 1) * PITCH];
            // The 8 pixels the frame actually has.
            assert_eq!(
                u32::from_ne_bytes([row[0], row[1], row[2], row[3]]),
                0xBBB0_0000 + (r * 8) as u32,
                "row {} first pixel",
                r
            );
            // And the tail the copy engine will carry regardless: defined, not
            // frame 1's leftovers. This is the assertion that failed before.
            assert!(
                row[32..].iter().all(|&b| b == 0),
                "row {} tail still holds a previous frame: {:02x?}",
                r,
                &row[32..]
            );
        }
    }

    /// A tail that would START inside the visible width must make the repack
    /// decline, so the caller falls back to the CPU blit -- which writes only
    /// the columns it has pixels for and leaves the rest of the screen alone.
    /// Filling that tail (with zeros or with anything else) would paint over
    /// on-screen columns the frame says nothing about.
    #[test]
    fn a_repack_that_cannot_cover_the_visible_width_is_declined() {
        let _serialised = super::test_globals::lock();
        const PITCH: usize = 64;
        let src = numbered(0xCCC0_0000, 8, 4);
        // 8 pixels of source, but 12 pixels (48 bytes) of the row are visible.
        assert!(
            ce_repack_to_staging(&src, 0, 8, PITCH, 48, 8, 4).is_none(),
            "would have clobbered visible columns 8..12"
        );
        // Exactly covering the visible width is fine.
        assert!(ce_repack_to_staging(&src, 0, 8, PITCH, 32, 8, 4).is_some());
        // A row wider than the destination pitch is refused as before: it
        // would spill each row into the next.
        assert!(ce_repack_to_staging(&src, 0, 8, PITCH, 32, 17, 4).is_none());
        // Degenerate geometry.
        assert!(ce_repack_to_staging(&src, 0, 8, PITCH, 32, 0, 4).is_none());
    }
}

/// Tests for the lifetime of a framebuffer built on a *nouveau* GEM object, as
/// opposed to a dumb buffer. The two are deliberately different, and getting
/// them the same way round is what keeps a crashed compositor from leaving the
/// panel scanning out memory that now belongs to somebody else.
#[cfg(test)]
mod nouveau_fb_lifetime_tests {
    use super::*;
    use zcore_drivers::scheme::gem_mmap;

    /// Plant a framebuffer over a driver-private handle, the way `create_fb`
    /// does for a nouveau `GEM_NEW` object: a bare `phys_addr`/`size` resolved
    /// through `gem_mmap`, and **no** `fb_backing` reference, because there is
    /// no `VmObject` to take one on.
    fn plant_nouveau_fb(fb_id: u32, handle: u32, pid: u64) {
        gem_mmap::register(handle, 0x1_0000, 4096, pid);
        let mut state = DRM_STATE.lock();
        state.framebuffers.push(DrmFramebuffer {
            id: fb_id,
            driver_fb_id: None,
            gem_handle_id: handle,
            width: 1,
            height: 1,
            pitch: 4,
            phys_addr: 0x1_0000,
            size: 4096,
            owner: pid,
        });
        state.crtc_fb = fb_id;
    }

    fn fb_exists(fb_id: u32) -> bool {
        DRM_STATE
            .lock()
            .framebuffers
            .iter()
            .any(|fb| fb.id == fb_id)
    }

    /// The regression this guards. `nouveau_gem_close` returns the object's
    /// memory to the RM, and the framebuffer over it holds no reference that
    /// could stop that -- so the framebuffer has to go with it. It did not:
    /// `gem_close` only ever looked at `state.handles`, where a nouveau handle
    /// never appears, so the fb (and `crtc_fb` pointing at it) survived the
    /// close and the next repaint blitted freed GEM memory. Persistent garbage
    /// on the panel, because the scanout keeps reading that address.
    ///
    /// "Freed" is now the precondition, not merely "somebody called close":
    /// `gem_mmap` drops its entry when the last reference goes and before the
    /// RM free, so an absent entry is what "the memory is gone" means here.
    #[test]
    fn closing_a_nouveau_handle_retires_the_framebuffer_over_it() {
        let _serialised = super::test_globals::lock();
        let handle = gem_mmap::DRIVER_HANDLE_BASE + 0x55;
        plant_nouveau_fb(9501, handle, 0);
        assert!(fb_exists(9501));

        // The last reference is gone and the object with it.
        gem_mmap::unregister(handle);
        assert_eq!(retire_framebuffers_for_handle(handle), 1);
        assert!(!fb_exists(9501), "the fb outlived the memory it points at");
        assert_eq!(
            DRM_STATE.lock().crtc_fb,
            0,
            "the CRTC must not keep scanning out a retired fb"
        );
        // Idempotent: a second close finds nothing left to retire.
        assert_eq!(retire_framebuffers_for_handle(handle), 0);
    }

    /// The other half, and the one that cost a desktop. A `GEM_CLOSE` that is
    /// NOT the last reference must leave the framebuffer alone.
    ///
    /// `nouveau_gem_close` answers `true` for "this close was handled", which
    /// includes one holder letting go of a buffer others still reference -- so
    /// the `GEM_CLOSE` arm calls `retire_framebuffers_for_handle` on every
    /// close, not just the final one. wlroots closes its buffer handle
    /// immediately after `ADDFB2`, because on Linux the framebuffer holds its
    /// own reference; here that close retired the scanout buffer seconds after
    /// it was created, and every `SETCRTC` on it answered `ENOENT` for the rest
    /// of the session -- the "Failed to set CRTC: No such file or directory"
    /// storm, at frame rate.
    #[test]
    fn a_close_that_is_not_the_last_reference_leaves_the_framebuffer_alone() {
        let _serialised = super::test_globals::lock();
        let handle = gem_mmap::DRIVER_HANDLE_BASE + 0x56;
        plant_nouveau_fb(9505, handle, 0);
        // Still registered: other references remain, so the memory is alive.
        assert!(gem_mmap::lookup(handle).is_some());

        assert_eq!(
            retire_framebuffers_for_handle(handle),
            0,
            "a live GEM object's framebuffer must survive a close"
        );
        assert!(fb_exists(9505), "the scanout buffer was destroyed under it");
        assert_eq!(
            DRM_STATE.lock().crtc_fb,
            9505,
            "and the CRTC still scans it out"
        );

        DRM_STATE.lock().framebuffers.retain(|fb| fb.id != 9505);
        DRM_STATE.lock().crtc_fb = 0;
        gem_mmap::unregister(handle);
    }

    /// The other half of the contract, so the fix above cannot creep into the
    /// dumb-buffer path: a dumb-buffer fb holds an `Arc` on its VMO, so closing
    /// the handle drops only the handle and the fb stays scannable until RMFB
    /// -- which is what Linux does, and what
    /// `gem_close_keeps_a_framebuffer_and_its_memory_alive` asserts end to end.
    /// This helper must therefore refuse to touch a low-range handle at all.
    #[test]
    fn a_dumb_buffer_framebuffer_is_never_retired_by_this_path() {
        let _serialised = super::test_globals::lock();
        let mut state = DRM_STATE.lock();
        state.framebuffers.push(DrmFramebuffer {
            id: 9502,
            driver_fb_id: None,
            gem_handle_id: 42, // low range: a CREATE_DUMB handle
            width: 1,
            height: 1,
            pitch: 4,
            phys_addr: 0,
            size: 4096,
            owner: 0,
        });
        drop(state);

        assert_eq!(retire_framebuffers_for_handle(42), 0);
        assert!(fb_exists(9502), "a dumb fb outlives its handle by design");
        DRM_STATE.lock().framebuffers.retain(|fb| fb.id != 9502);
    }

    /// A process exit is the case that actually matters -- a compositor that
    /// crashed never sends RMFB or GEM_CLOSE. `release_process` builds its
    /// `doomed` list from `state.handles`, where a nouveau handle never
    /// appears, so it used to leave every nouveau-backed fb behind while
    /// `nouveau_release_process` (which runs immediately after) freed the
    /// memory underneath it.
    #[test]
    fn a_process_exit_retires_the_nouveau_framebuffers_that_process_held() {
        let _serialised = super::test_globals::lock();
        let mine = gem_mmap::DRIVER_HANDLE_BASE + 0x66;
        let theirs = gem_mmap::DRIVER_HANDLE_BASE + 0x67;
        plant_nouveau_fb(9503, mine, 78_101);
        plant_nouveau_fb(9504, theirs, 78_102);

        release_process(78_101);
        assert!(!fb_exists(9503), "the dead process's fb must be retired");
        assert!(fb_exists(9504), "another process's fb must survive");

        // And the survivor goes when its own owner exits.
        release_process(78_102);
        assert!(!fb_exists(9504));
        assert_eq!(DRM_STATE.lock().crtc_fb, 0);
        gem_mmap::unregister(mine);
        gem_mmap::unregister(theirs);
    }
}

/// What a present that puts no pixels on the screen reports back.
///
/// The compositor log that prompted these was a wall of
/// `[backend/drm/legacy.c:123] connector HDMI-A-1: Failed to set CRTC: I/O
/// error` at ~8 Hz — wlroots' legacy backend retrying a modeset that the
/// kernel kept answering `EIO`, never advancing to page-flips, while the
/// kernel side printed nothing at all (every reason in here is a `warn!`,
/// and the rig boots at `LOG=error`). `EIO` also named none of the causes,
/// so the photo of the screen could not be turned into a diagnosis.
///
/// These pin the distinction the ioctl arms now depend on: which reason it
/// was, and — for the arms — whether it is the caller's fault.
#[cfg(test)]
mod present_error_tests {
    use super::*;

    /// Plant a framebuffer directly in the table, bypassing `create_fb` (which
    /// refuses a fb with no backing — the point here is to build the state a
    /// live system can reach anyway, e.g. a driver fb whose GEM went away).
    fn plant_fb(fb_id: u32, phys_addr: u64, size: usize) {
        let mut state = DRM_STATE.lock();
        state.framebuffers.retain(|fb| fb.id != fb_id);
        state.framebuffers.push(DrmFramebuffer {
            id: fb_id,
            driver_fb_id: None,
            gem_handle_id: 0,
            width: 1,
            height: 1,
            pitch: 4,
            phys_addr,
            size,
            owner: 0,
        });
    }

    fn drop_fb(fb_id: u32) {
        DRM_STATE.lock().framebuffers.retain(|fb| fb.id != fb_id);
    }

    /// An fb id nothing answers to is the one failure that IS the caller's
    /// fault, and it has to be distinguishable from the rest: it is the only
    /// reason the ioctl arms still fail on, and they answer `ENOENT` for it
    /// (Linux's "Unknown FB ID"), not `EIO`.
    ///
    /// This is not a hypothetical id: `retire_framebuffers_for_handle` drops a
    /// nouveau-backed fb the instant its GEM handle closes, so a compositor
    /// that still holds the id from `ADDFB2` lands here through no fault of
    /// its scanout path.
    #[test]
    fn an_unknown_fb_id_is_reported_as_no_such_fb() {
        let _serialised = super::test_globals::lock();
        drop_fb(9601);
        assert_eq!(
            scanout_region_checked(9601, None),
            Err(PresentError::NoSuchFb)
        );
        assert_eq!(
            present_now_checked(9601, 1, None),
            Err(PresentError::NoSuchFb),
            "the reason must survive the page-flip/scanout fallback chain"
        );
    }

    /// A framebuffer that describes no memory is the framebuffer's own defect,
    /// so it is reported as such whether or not a display is attached — the
    /// backing check runs first for exactly this reason. Getting `NoDisplay`
    /// here would send the reader looking at the wrong half of the system.
    #[test]
    fn a_framebuffer_with_no_backing_is_reported_as_no_backing() {
        let _serialised = super::test_globals::lock();
        plant_fb(9602, 0, 4096);
        assert_eq!(
            scanout_region_checked(9602, None),
            Err(PresentError::NoBacking)
        );
        // Zero size, same verdict: there is nothing to copy either way.
        plant_fb(9602, 0x1_0000, 0);
        assert_eq!(
            scanout_region_checked(9602, None),
            Err(PresentError::NoBacking)
        );
        drop_fb(9602);
    }

    /// The `bool` wrappers the rest of the tree still calls must keep behaving
    /// exactly as they did — the reason is additive, not a change of contract.
    #[test]
    fn the_bool_wrappers_still_report_failure_the_old_way() {
        let _serialised = super::test_globals::lock();
        drop_fb(9603);
        assert!(!scanout_region(9603, None));
        assert!(!present_now(9603, 1));
        assert!(!present_now_region(9603, 1, Some((0, 0, 1, 1))));
    }

    /// The fork a `NoSuchFb` on the console cannot resolve on its own: a
    /// framebuffer the client removed itself, versus one the kernel took out
    /// from under it when the nouveau GEM handle closed. On Linux only the
    /// first can happen -- a `drm_framebuffer` there holds its own reference
    /// on the GEM object -- so the second is our bug to fix, and the log line
    /// has to say which one the compositor hit.
    #[test]
    fn a_retired_fb_id_remembers_what_took_it() {
        let _serialised = super::test_globals::lock();
        let handle = zcore_drivers::scheme::gem_mmap::DRIVER_HANDLE_BASE + 0x71;
        zcore_drivers::scheme::gem_mmap::register(handle, 0x2_0000, 4096, 0);
        {
            let mut state = DRM_STATE.lock();
            state.framebuffers.push(DrmFramebuffer {
                id: 9604,
                driver_fb_id: None,
                gem_handle_id: handle,
                width: 1,
                height: 1,
                pitch: 4,
                phys_addr: 0x2_0000,
                size: 4096,
                owner: 0,
            });
        }
        // The object is really gone (last reference dropped) -- the only
        // condition under which a framebuffer is retired behind its owner.
        zcore_drivers::scheme::gem_mmap::unregister(handle);
        assert_eq!(retire_framebuffers_for_handle(handle), 1);
        assert_eq!(fb_retired_reason(9604), Some(FbRetired::HandleClosed));

        // The client's own RMFB reads differently, because it is a different
        // answer: nothing was taken from anyone.
        plant_fb(9605, 0x3_0000, 4096);
        assert!(rmfb(9605));
        assert_eq!(fb_retired_reason(9605), Some(FbRetired::Removed));

        // An id that was never a framebuffer of ours has no story to tell.
        assert_eq!(fb_retired_reason(9699), None);
    }

    /// The history is a fixed cost: a compositor that recreates its swapchain
    /// all session long retires framebuffers forever, and this must not grow
    /// with it.
    #[test]
    fn the_retirement_history_is_bounded() {
        let _serialised = super::test_globals::lock();
        for i in 0..(FB_RETIRE_HISTORY as u32 * 4) {
            plant_fb(9700 + i, 0x3_0000, 4096);
            assert!(rmfb(9700 + i));
        }
        assert!(DRM_STATE.lock().fb_retirements.len() <= FB_RETIRE_HISTORY);
        // And it is the NEWEST that are kept -- the id a stuck compositor is
        // still re-presenting is the one that has to be explainable.
        let newest = 9700 + (FB_RETIRE_HISTORY as u32 * 4) - 1;
        assert_eq!(fb_retired_reason(newest), Some(FbRetired::Removed));
    }

    /// Each reason prints as itself: these strings are what a boot log carries
    /// and what a bug report gets grepped for.
    #[test]
    fn every_reason_has_its_own_console_text() {
        let _serialised = super::test_globals::lock();
        let all = [
            PresentError::NoSuchFb,
            PresentError::NoDisplay,
            PresentError::NoBacking,
        ];
        for (i, a) in all.iter().enumerate() {
            assert!(!a.as_str().is_empty());
            for b in &all[i + 1..] {
                assert_ne!(a.as_str(), b.as_str(), "{:?} and {:?} read alike", a, b);
            }
        }
    }
}

/// The lifetime contract a KMS framebuffer keeps over a nouveau GEM object,
/// and the bug that made the GL/Vulkan desktop impossible.
///
/// A compositor using the GL/Vulkan renderer allocates its scanout buffer with
/// `GEM_NEW`, calls `ADDFB2` on the handle, and then CLOSES the handle right
/// away. That is not teardown -- it is the normal, required dance, because on
/// Linux a `drm_framebuffer` holds its own reference on the GEM object and the
/// buffer lives until `RMFB`. Closing the handle is how a client avoids
/// leaking handles for every buffer it ever scans out.
///
/// Nothing here held that reference. The close was therefore the LAST one:
/// `nouveau_gem_close` handed the VRAM back to the RM and retired the
/// framebuffer that had just been created. Every `SETCRTC` on it answered
/// `ENOENT` from then on -- the wall of
/// `connector HDMI-A-1: Failed to set CRTC: No such file or directory` at
/// frame rate, for the whole session, with the desktop never appearing.
///
/// The dumb-buffer half of this contract is
/// `gem_close_keeps_a_framebuffer_and_its_memory_alive`, which has always
/// passed because an `Arc<VmObject>` in `fb_backing` was the reference. These
/// are its nouveau counterpart.
#[cfg(test)]
mod nouveau_fb_gem_reference_tests {
    use super::*;
    use zcore_drivers::scheme::gem_mmap::{self, DecRef};

    const CLIENT: u64 = 88_201;

    /// Register a nouveau GEM object the way `GEM_NEW` does: one holder, its
    /// creator.
    fn gem_new(handle: u32, pid: u64) {
        gem_mmap::register(handle, 0x40_0000, 4096, pid);
    }

    /// The regression, end to end and in the compositor's own order:
    /// `ADDFB2`, then `GEM_CLOSE`. The close must NOT be the last reference,
    /// because the framebuffer holds one.
    #[test]
    fn addfb2_then_gem_close_leaves_the_framebuffer_backed() {
        let _serialised = super::test_globals::lock();
        let handle = gem_mmap::DRIVER_HANDLE_BASE + 0x81;
        gem_new(handle, CLIENT);

        let fb_id = create_fb(handle, 1, 1, 4).expect("ADDFB2 over a nouveau GEM object");

        // The client lets go of its handle, exactly as wlroots does the
        // instant ADDFB2 returns. Before the fb took a reference this was the
        // last one: the memory went back to the RM and the fb went with it.
        assert_eq!(
            gem_mmap::dec_ref(handle, CLIENT),
            DecRef::StillReferenced(1),
            "the framebuffer's own reference must outlive the client's handle",
        );
        assert!(
            gem_mmap::lookup(handle).is_some(),
            "the GEM object must still be alive for the fb to scan out",
        );

        // And the framebuffer is still there to be presented -- this is the
        // ENOENT storm, reduced to one assertion.
        assert_ne!(
            present_now_checked(fb_id, 1, None),
            Err(PresentError::NoSuchFb),
            "SETCRTC would have answered ENOENT for the rest of the session",
        );

        // RMFB is what finally releases it, as on Linux.
        assert!(rmfb(fb_id));
        assert!(
            gem_mmap::lookup(handle).is_none(),
            "RMFB dropped the last reference, so the object is freed",
        );
    }

    /// The reference is the framebuffer's, not the creating process's: it has
    /// to survive that process's exit sweep, or a buffer still being scanned
    /// out is freed under the compositor.
    #[test]
    fn the_framebuffers_reference_belongs_to_no_process() {
        let _serialised = super::test_globals::lock();
        let handle = gem_mmap::DRIVER_HANDLE_BASE + 0x82;
        gem_new(handle, CLIENT);
        let fb_id = create_fb(handle, 1, 1, 4).expect("ADDFB2 over a nouveau GEM object");

        // Everything the client held goes; the fb's reference is not the
        // client's to give up.
        gem_mmap::release_pid(CLIENT);
        assert!(
            gem_mmap::lookup(handle).is_some(),
            "a process exit must not free memory a framebuffer still names",
        );

        assert!(rmfb(fb_id));
        assert!(gem_mmap::lookup(handle).is_none());
    }

    /// Two framebuffers over one buffer take two references, and it takes both
    /// `RMFB`s to free it. A compositor really does this -- one fb per
    /// modifier/format it tests a buffer with.
    #[test]
    fn each_framebuffer_takes_its_own_reference() {
        let _serialised = super::test_globals::lock();
        let handle = gem_mmap::DRIVER_HANDLE_BASE + 0x83;
        gem_new(handle, CLIENT);
        let a = create_fb(handle, 1, 1, 4).expect("first ADDFB2");
        let b = create_fb(handle, 1, 1, 4).expect("second ADDFB2");
        assert_ne!(a, b);

        assert_eq!(
            gem_mmap::dec_ref(handle, CLIENT),
            DecRef::StillReferenced(2)
        );
        assert!(rmfb(a));
        assert!(
            gem_mmap::lookup(handle).is_some(),
            "the second framebuffer still names this memory",
        );
        assert!(rmfb(b));
        assert!(gem_mmap::lookup(handle).is_none());
    }

    /// A dumb buffer is not tracked in `gem_mmap` at all, and must not be
    /// touched by any of this: its reference is the `Arc<VmObject>` in
    /// `fb_backing`, and `gem_close_keeps_a_framebuffer_and_its_memory_alive`
    /// owns that half of the contract.
    #[test]
    fn a_dumb_buffer_framebuffer_takes_no_gem_reference() {
        let _serialised = super::test_globals::lock();
        let low = 4242; // below DRIVER_HANDLE_BASE: a CREATE_DUMB handle
        assert!(gem_mmap::lookup(low).is_none());
        fb_take_gem_ref(low);
        assert!(
            gem_mmap::lookup(low).is_none(),
            "a dumb handle must never appear in the nouveau table",
        );
        fb_drop_gem_ref(low); // and dropping one that was never taken is safe
    }
}

/// `drm_read()` semantics for the DRM event queue: as many whole events as
/// fit, and a buffer too small for the first one is the caller's error, not an
/// empty queue.
#[cfg(test)]
mod drm_event_read_tests {
    use super::*;

    fn event(tag: u8, len: usize) -> Vec<u8> {
        alloc::vec![tag; len]
    }

    /// The livelock. A short read left the event queued with `READABLE` still
    /// set, and answered EAGAIN -- so a blocking reader's wait resolved
    /// immediately, it re-read, got EAGAIN again, and spun a core with no
    /// yield point. `drm_read()` returns EINVAL there and never blocks.
    #[test]
    fn a_buffer_too_small_for_the_first_event_is_distinguishable_from_empty() {
        let file = DrmFileState::new();
        let mut buf = [0u8; 8];
        assert_eq!(file.read_events(&mut buf), EventRead::Empty);

        file.push_event(event(0xAB, 32));
        assert_eq!(
            file.read_events(&mut buf),
            EventRead::TooSmall,
            "a short read must not look like an empty queue"
        );
        // And the event is still there, unconsumed.
        assert!(file.has_events());
        let mut big = [0u8; 32];
        assert_eq!(file.read_events(&mut big), EventRead::Read(32));
        assert!(big.iter().all(|&b| b == 0xAB));
        assert!(!file.has_events());
    }

    /// Linux fills the buffer with every whole event that fits, not just one.
    #[test]
    fn one_read_drains_as_many_whole_events_as_fit() {
        let file = DrmFileState::new();
        file.push_event(event(1, 32));
        file.push_event(event(2, 32));
        file.push_event(event(3, 32));

        // Room for two and a half: two come back, the third stays queued.
        let mut buf = [0u8; 80];
        assert_eq!(file.read_events(&mut buf), EventRead::Read(64));
        assert!(buf[..32].iter().all(|&b| b == 1));
        assert!(buf[32..64].iter().all(|&b| b == 2));
        assert!(file.has_events());

        assert_eq!(file.read_events(&mut buf), EventRead::Read(32));
        assert_eq!(file.read_events(&mut buf), EventRead::Empty);
    }
}

/// Handles and framebuffers are one process-wide table here, not the per-`drm_file`
/// idr and fb list Linux keeps. The owner pid is what stands in for that, and
/// these are the uAPI entry points where Linux resolves an id against the
/// calling file: `drm_gem_object_lookup` for PRIME export and ADDFB, and
/// `file_priv->fbs` for RMFB. Without the checks, handle ids are sequential
/// from 1 and fb ids from 1, so reading the compositor's screen from an
/// unprivileged process took no guessing.
#[cfg(test)]
mod gem_ownership_tests {
    use super::*;

    const A: u64 = 91_001;
    const B: u64 = 91_002;

    fn plant_handle(id: u32, pid: u64) {
        let vmo = VmObject::new_paged(1);
        DRM_STATE.lock().handles.push((
            GemHandle {
                id,
                size: 4096,
                phys_addr: 0x5_0000,
            },
            vmo,
            pid,
        ));
    }

    fn plant_fb(fb_id: u32, owner: u64) {
        DRM_STATE.lock().framebuffers.push(DrmFramebuffer {
            id: fb_id,
            driver_fb_id: None,
            gem_handle_id: 0,
            width: 1,
            height: 1,
            pitch: 4,
            phys_addr: 0x5_0000,
            size: 4096,
            owner,
        });
    }

    fn forget(handle: u32, fb: u32) {
        let mut state = DRM_STATE.lock();
        state.handles.retain(|(h, _, _)| h.id != handle);
        state.framebuffers.retain(|f| f.id != fb);
    }

    /// `ADDFB2` with someone else's handle. Building a framebuffer over it is
    /// how process B gets an id it can `SETCRTC`/`PAGE_FLIP` — A's pixels onto
    /// the panel, or blitted somewhere B can read.
    #[test]
    fn a_framebuffer_cannot_be_built_over_another_process_handle() {
        let _serialised = super::test_globals::lock();
        plant_handle(9801, A);

        assert!(
            resolve_gem_backing_for(9801, A).is_some(),
            "its owner resolves it"
        );
        assert!(
            resolve_gem_backing_for(9801, B).is_none(),
            "another process must not"
        );
        // The kernel's own paths (pid 0) still resolve everything.
        assert!(resolve_gem_backing_for(9801, 0).is_some());
        assert!(
            resolve_gem_backing(9801).is_some(),
            "and the unchecked resolver the present path uses is unchanged"
        );

        forget(9801, 0);
    }

    /// `RMFB` of someone else's framebuffer. Removing the compositor's
    /// scanout fb makes every later SETCRTC and PAGE_FLIP on it fail, and
    /// wlroots retries the modeset forever.
    #[test]
    fn a_framebuffer_cannot_be_removed_by_another_process() {
        let _serialised = super::test_globals::lock();
        plant_fb(9802, A);

        assert!(!rmfb_for(9802, B), "another process must not remove it");
        assert!(
            DRM_STATE.lock().framebuffers.iter().any(|f| f.id == 9802),
            "and it must still be there afterwards"
        );
        // Indistinguishable from "no such framebuffer", so a prober learns
        // nothing about what other clients own.
        assert!(!rmfb_for(9899, B));

        assert!(rmfb_for(9802, A), "its owner removes it");
        assert!(!DRM_STATE.lock().framebuffers.iter().any(|f| f.id == 9802));
    }

    /// `GETFB`'s handle field: the enumeration half. Linux zeroes it for a
    /// non-master caller rather than failing the call, so the geometry still
    /// comes back.
    #[test]
    fn the_backing_handle_goes_only_to_the_framebuffers_creator() {
        let _serialised = super::test_globals::lock();
        plant_fb(9803, A);
        let fb = get_fb(9803).expect("planted");

        assert!(owned_by(fb.owner, A), "its creator sees the handle");
        assert!(!owned_by(fb.owner, B), "another process gets zero");
        assert!(owned_by(fb.owner, 0), "kernel-internal callers still do");

        forget(0, 9803);
    }
}

/// The same rule seen from the other side: the hops an X11 GL client's buffer
/// legitimately makes must all be allowed.
///
/// `gem_ownership_tests` above pins that process B cannot touch process A's
/// handle. That is only half a rule — the half a hardening change gets right
/// by construction. The other half is that a buffer handed **on** stays
/// reachable by whoever it was handed to, and under Xwayland it is handed on
/// twice: the client allocates it, Xwayland imports and re-exports it, and the
/// compositor imports it. `zcore_drivers::scheme::gem_mmap`'s
/// `xwayland_chain_tests` cover that chain for a driver-private (nouveau) GEM
/// object; these cover it for a generic one, which takes a different route —
/// a fresh handle per importer out of `import_dmabuf` rather than a shared
/// reference on the original.
#[cfg(test)]
mod prime_import_chain_tests {
    use super::*;

    const CLIENT: u64 = 91_101;
    const XWAYLAND: u64 = 91_102;
    const STRANGER: u64 = 91_103;

    fn forget_handles(ids: &[u32]) {
        let mut state = DRM_STATE.lock();
        state.handles.retain(|(h, _, _)| !ids.contains(&h.id));
    }

    /// An imported dma-buf belongs to the process that imported it, so that
    /// process can use it and hand it on. Without this an importer would hold
    /// a handle it is not allowed to resolve — a buffer it can name and
    /// nothing else — which is how the middle of the chain breaks while both
    /// ends look fine.
    #[test]
    fn an_imported_dmabuf_belongs_to_the_importer() {
        let _serialised = super::test_globals::lock();
        // The client's own buffer, exported and then imported by Xwayland.
        // `import_dmabuf` reads the caller from the current thread, which a
        // host test does not have (pid 0), so the entry it makes is planted
        // here with the importer's pid, exactly as it would be made on the
        // target.
        let imported = 9_901;
        let vmo = VmObject::new_paged(1);
        DRM_STATE.lock().handles.push((
            GemHandle {
                id: imported,
                size: 4096,
                phys_addr: 0x7_0000,
            },
            vmo,
            XWAYLAND,
        ));

        assert!(
            resolve_gem_backing_for(imported, XWAYLAND).is_some(),
            "the importer can resolve what it imported, and so re-export it"
        );
        assert!(
            resolve_gem_backing_for(imported, STRANGER).is_none(),
            "a process that imported nothing still gets nothing"
        );
        assert!(
            resolve_gem_backing_for(imported, CLIENT).is_none(),
            "not even the process that exported it in the first place: its own \
             handle is a separate entry, with its own lifetime"
        );

        forget_handles(&[imported]);
    }

    /// Two processes importing the same dma-buf get two handles, each its
    /// own. Closing one must not disturb the other — the compositor releasing
    /// a frame cannot invalidate Xwayland's handle on the same memory.
    #[test]
    fn two_importers_of_one_buffer_hold_independent_handles() {
        let _serialised = super::test_globals::lock();
        let phys = 0x7_1000;
        let (xwl_handle, comp_handle) = (9_902, 9_903);
        for (id, pid) in [(xwl_handle, XWAYLAND), (comp_handle, STRANGER)] {
            let vmo = VmObject::new_paged(1);
            DRM_STATE.lock().handles.push((
                GemHandle {
                    id,
                    size: 4096,
                    phys_addr: phys,
                },
                vmo,
                pid,
            ));
        }

        assert!(resolve_gem_backing_for(xwl_handle, XWAYLAND).is_some());
        assert!(resolve_gem_backing_for(comp_handle, STRANGER).is_some());
        assert!(
            resolve_gem_backing_for(comp_handle, XWAYLAND).is_none(),
            "neither importer can name the other's handle"
        );

        forget_handles(&[xwl_handle]);
        assert!(
            resolve_gem_backing_for(comp_handle, STRANGER).is_some(),
            "one importer letting go leaves the other's handle intact"
        );

        forget_handles(&[comp_handle]);
    }
}

/// Turning a screen off, and a commit that fails leaving nothing behind.
#[cfg(test)]
mod blanking_and_atomic_rollback_tests {
    use super::*;

    /// The latch. There is no display backend in a host test, so what is
    /// observable here is the state every consumer reads: whether the CRTC
    /// counts as off, and whether the kernel's own repaints are suppressed.
    #[test]
    fn the_crtc_stays_off_until_something_presents() {
        let _serialised = super::test_globals::lock();
        set_crtc_blanked(false);
        assert!(!crtc_blanked());

        set_crtc_blanked(true);
        assert!(crtc_blanked(), "DPMS off / SETCRTC(fb=0) turns it off");
        // Idempotent: a compositor that writes DPMS off twice must not repaint.
        set_crtc_blanked(true);
        assert!(crtc_blanked());

        set_crtc_blanked(false);
        assert!(!crtc_blanked(), "and DPMS on turns it back on");
    }

    /// A commit that fails at the present must leave nothing behind. Applying
    /// first and failing afterwards left `ACTIVE` and `MODE_ID` describing a
    /// modeset that never reached the screen, so wlroots' next commit saw an
    /// empty diff and never retried.
    #[test]
    fn a_failed_commit_leaves_the_state_exactly_as_it_was() {
        let _serialised = super::test_globals::lock();
        let before = {
            let mut state = DRM_STATE.lock();
            state.atomic.active = true;
            state.atomic.crtc_w = 1920;
            state.crtc_fb = 4242;
            (state.atomic, state.crtc_fb)
        };

        // Stand in for the commit phase having already run: mutate, then roll
        // back the way the present-failure path does.
        {
            let mut state = DRM_STATE.lock();
            state.atomic.active = false;
            state.atomic.crtc_w = 640;
            state.crtc_fb = 7;
        }
        restore_atomic_state((before.0, before.1, None));

        let after = {
            let state = DRM_STATE.lock();
            (state.atomic, state.crtc_fb)
        };
        assert!(after.0.active, "ACTIVE must be what it was");
        assert_eq!(after.0.crtc_w, 1920, "and so must the plane geometry");
        assert_eq!(after.1, before.1, "and the CRTC's framebuffer");

        let mut state = DRM_STATE.lock();
        state.crtc_fb = 0;
        state.atomic = AtomicKmsState::default();
    }
}
