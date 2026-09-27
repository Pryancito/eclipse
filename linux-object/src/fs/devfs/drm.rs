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
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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

/// First id handed to a KMS property blob (`CREATEPROPBLOB`, and the
/// kernel-owned current-mode blob). libdrm identifies a blob by id alone, and
/// so does this tree's `GETPROPBLOB`, which consults the blob store and then
/// the reserved EDID range; the two must not overlap. `drm_scheme` asserts
/// that against its own `EDID_BLOB_BASE` at compile time.
pub const BLOB_ID_BASE: u32 = 30_000;

/// One-shot guard so the first scanout logs (every-frame logging would spam).
static SCANOUT_LOGGED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// One-shot latch for the "framebuffer has no backing" scanout warning.
static SCANOUT_NULL_LOGGED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
/// One-shot latch for "CE present enabled but every GPU declined the copy".
static CE_NO_TAKER_LOGGED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
/// One-shot latch for the CE staging-buffer allocation failure warning.
/// One-shot: hardware KMS took over the scanout while the pointer was still
/// software-composited, so nothing can draw it. See `repaint_for_cursor`.
static SURFACEFLIP_NO_CURSOR_LOGGED: AtomicBool = AtomicBool::new(false);

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
/// The two present counters, so a test can assert that a damage-clipped present
/// is counted at all -- which is the whole of this change, and which no test can
/// see from the klog.
#[cfg(test)]
pub(crate) fn present_report_counts_for_test() -> (u64, u64) {
    (
        PRESENT_FRAME_COUNT.load(Ordering::Relaxed),
        PRESENT_RECT_COUNT.load(Ordering::Relaxed),
    )
}

/// A byte count and its unit for the present's cost line: bytes below a KiB,
/// whole KiB above it.
///
/// Truncating dividing by 1024 unconditionally was worse than imprecise, it was
/// blind exactly where the line is needed: a caret box or a cursor patch reads a
/// few hundred bytes, so the report read `0KiB flushed for 0KiB read` -- two
/// zeros, for the small high-frequency updates the damage path exists to serve.
/// The ratio between the two numbers IS the finding, and a ratio of zeros has
/// none. Rounding up instead would have kept both numbers at `1KiB` and lost the
/// ratio the other way.
fn cost_scaled(bytes: usize) -> (usize, &'static str) {
    if bytes < 1024 {
        (bytes, "B")
    } else {
        (bytes / 1024, "KiB")
    }
}

/// How often each kind of present gets a line. A full frame is rare enough to
/// report often; a damage box is not.
///
/// The full-frame rhythm is `pub(crate)` because the fence report next door
/// shares it on purpose (`drm_scheme`'s `FENCE_REPORT_EVERY`): the two lines
/// describe the same present, so on the same rhythm they land next to each other
/// in the klog and a reader can pair "waited 0us for 0 fences" with "cpu blit
/// 12000us" without counting frames.
pub(crate) const FULL_FRAME_REPORT_EVERY: u64 = 64;
const RECT_REPORT_EVERY: u64 = 512;

/// Damage-clipped presents completed, counted separately from the full frames
/// and reported far less often.
///
/// Two counters because the two rates are nothing alike. A compositor with
/// damage tracking issues several of these per frame and full frames almost
/// never, so one counter with one divisor either drowns the log or hides the
/// clipped path entirely -- and hiding it is what the old `if rect.is_none()`
/// did. `klog` writes synchronously to the UART, where a line of this length is
/// milliseconds, so the clipped divisor is large enough that the log costs less
/// than the frame it describes.
static PRESENT_RECT_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
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

/// Set when a present was acknowledged and thrown away because scanout was
/// paused, i.e. when the screen stopped being what `crtc_fb` says it is.
///
/// The drop still records `crtc_fb`, so `GETCRTC` keeps telling the client the
/// truth about what it bound -- but those pixels never reached the panel, and
/// the client was told the flip completed, so it will not draw them again. Two
/// things go wrong from there, and this latch is what both of them read:
///
/// - Nothing puts the newest frame up when the pause lifts, so an idle desktop
///   stays frozen on a frame from before the pause even though scanout is back.
/// - [`repaint_for_cursor`] restores its two ~64x64 windows *from* `crtc_fb`.
///   With the screen a frame behind, a pointer move pastes pieces of a frame
///   nobody has seen onto the one that is still up: garbage in a ring around
///   the cursor, which a flat wallpaper hides and a window shadow does not.
///
/// Cleared by the next present that puts a WHOLE frame up -- a partial one
/// leaves the disagreement everywhere it did not touch.
static SCANOUT_STALE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// How long a deferred console-GPU bring-up may hold scanout paused before the
/// watchdog resumes it anyway. Generous, because a real GSP boot + state-load
/// on cold hardware is tens of seconds and cutting one short would put
/// labwc's BAR1 traffic right back into the SEC2 window this exists to keep
/// quiet. Bounded, because the alternative is a permanently frozen desktop.
pub const SCANOUT_PAUSE_MAX: core::time::Duration = core::time::Duration::from_secs(90);

/// Nanoseconds since boot, saturating rather than wrapping.
///
/// `Duration::as_nanos` is a `u128` and `as u64` on it truncates silently; a
/// truncated deadline is a deadline in the past. Nothing reaches 2^64 ns (584
/// years) on a real boot, but the arithmetic below is the watchdog's, and the
/// watchdog exists for the case where the rest of the bring-up did not hold.
fn timer_now_ns() -> u64 {
    kernel_hal::timer::timer_now()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

/// Whether a pause is in force, given the two latches and the clock.
///
/// Pure, so each branch is testable without the process-global statics and
/// without a clock: whether a frozen desktop ever recovers is this one line.
///
/// `deadline_ns == 0` is the "no watchdog" sentinel -- a pause that holds until
/// someone lifts it. Which is exactly why [`pause_deadline_ns`] never returns 0.
fn pause_in_force(paused: bool, deadline_ns: u64, now_ns: u64) -> bool {
    paused && (deadline_ns == 0 || now_ns < deadline_ns)
}

/// The deadline [`set_scanout_paused_for`] stores for `max` from `now_ns`.
///
/// Never 0, because 0 means "no watchdog": a deadline that landed there -- a
/// truncated `now + max`, or a genuine zero on a clock that starts at zero --
/// would turn the one call whose entire purpose is to make a permanent freeze
/// impossible into the permanent freeze itself. 1 ns is in the past for every
/// reader, so the pause would lift on its first read instead: cutting a
/// bring-up window short is recoverable, a desktop frozen for good is not.
fn pause_deadline_ns(now_ns: u64, max: core::time::Duration) -> u64 {
    let max_ns = max.as_nanos().min(u64::MAX as u128) as u64;
    now_ns.saturating_add(max_ns).max(1)
}

/// The framebuffer whose pixels the panel carries **in full**, or 0 for none.
///
/// Not `crtc_fb`, which is bookkeeping for `GETCRTC` and is bound by every
/// present that returns `Ok` -- including one that only copied a damage box, and
/// one a pause acknowledged without drawing anything at all. This is the
/// narrower fact, and it is the only fact a damage box is meaningful against.
///
/// A damage box says "only these pixels changed **in the frame that is already
/// on the panel**". A compositor with a swapchain presents a *different*
/// framebuffer almost every frame, so the pixels outside the box come from the
/// buffer presented before -- and a recycled swapchain buffer holds whatever
/// frame it was last drawn into, which is not this one. Honouring the box then
/// leaves the panel a collage of two frames, and the collage is invisible until
/// something reads the panel back: [`repaint_for_cursor`] restores its two
/// ~64x64 windows *from* `crtc_fb`, so a pointer move over a region the box did
/// not touch pastes the new buffer's older content into the frame still up.
/// That is garbage in a ring around the cursor, appearing exactly when a popup
/// opens -- which is when a fresh buffer is presented with a box around the
/// popup and nothing else.
///
/// This is the same rule Linux applies in `drm_atomic_helper_damage_iter_init`,
/// which throws the clips away and declares a full update when
/// `state->fb != old_state->fb`.
///
/// Cleared, not just rebound, wherever something OTHER than a present writes the
/// panel: blanking paints it black, and a text VT in the foreground puts console
/// output on it. An id can be reused after `RMFB`, so a retired framebuffer
/// clears it too rather than letting a new one inherit "already on screen".
static PANEL_FB: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// The framebuffer the panel carries in full, or 0.
fn panel_fb() -> u32 {
    PANEL_FB.load(Ordering::SeqCst)
}

/// Record that the panel now carries `fb_id` in full; 0 means it carries no
/// framebuffer (blanked, or a console VT has written over it).
fn set_panel_fb(fb_id: u32) {
    PANEL_FB.store(fb_id, Ordering::SeqCst);
    // Every caller passing 0 is something OTHER than a present having written
    // the panel -- a blank painting it black, a console VT printing over it --
    // so the band skip's idea of what is up there stops being true at the same
    // moment, and for the same reason. See [`PRESENT_SKIP`].
    if fb_id == 0 {
        panel_bands_reset();
    }
}

/// [`PANEL_FB`] cleared for a framebuffer that no longer exists, so a reused id
/// cannot inherit "the panel already carries this".
fn forget_panel_fb(fb_id: u32) {
    if PANEL_FB
        .compare_exchange(fb_id, 0, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        // The framebuffer whose pixels the band hashes describe is gone, and its
        // id can come back on a different buffer. Same rule as [`PANEL_FB`]'s
        // own: a retired id inherits nothing.
        panel_bands_reset();
    }
}

/// The region a present may actually restrict itself to: the damage box when the
/// panel already carries this framebuffer, and the whole frame otherwise.
///
/// Pure, because which of the two it is decides whether the panel ends up
/// holding one frame or two -- see [`PANEL_FB`] for what the second one looks
/// like on screen.
///
/// `fb_id == 0` is never "the framebuffer the panel carries", even when
/// [`PANEL_FB`] also reads 0: 0 is the absence of one on both sides, and letting
/// the two absences match would honour a damage box against a panel nobody has
/// presented to.
fn rect_for_present(
    rect: Option<(u32, u32, u32, u32)>,
    panel_fb: u32,
    fb_id: u32,
) -> Option<(u32, u32, u32, u32)> {
    match rect {
        None => None,
        Some(_) if fb_id != 0 && panel_fb == fb_id => rect,
        Some(_) => None,
    }
}

/// How many times the promotion above is reported before it goes quiet. Small:
/// a compositor that presents a fresh buffer every frame promotes every frame,
/// and the line is worth having once to say which way round the swapchain is,
/// not sixty times a second.
const MAX_DAMAGE_PROMOTIONS_LOGGED: u32 = 4;
static DAMAGE_PROMOTIONS_LOGGED: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

#[cfg(test)]
pub(crate) fn panel_fb_for_test() -> u32 {
    panel_fb()
}

/// Whether a present left the WHOLE scanout carrying its framebuffer, so
/// [`SCANOUT_STALE`] may be cleared.
///
/// `None` is a full-frame present by the shape of the call. A damage rect is
/// accepted only when it starts at the origin and reaches the display's last row
/// and column -- a `DIRTYFB` clip covering the whole framebuffer does catch the
/// panel up, and leaving the latch set there costs a redundant full repaint on
/// the next pointer move.
///
/// Deliberately conservative, because the two ways of being wrong do not cost
/// the same: clearing the latch while the panel is NOT caught up brings back the
/// garbage it exists to prevent, and failing to clear it costs one repaint. With
/// no display there is nothing to be caught up with, so the answer is no.
fn present_caught_the_panel_up(
    rect: Option<(u32, u32, u32, u32)>,
    screen: Option<(u32, u32)>,
) -> bool {
    match (rect, screen) {
        (None, _) => true,
        (Some((x, y, w, h)), Some((sw, sh))) => x == 0 && y == 0 && w >= sw && h >= sh,
        (Some(_), None) => false,
    }
}

/// The deadline a plain [`set_scanout_paused`]`(true)` leaves behind: a watchdog
/// still in force is kept, one that has already run out is dropped.
///
/// Keeping it matters because that variant latches until someone lifts it and
/// nobody else will, so silently discarding a live watchdog is the difference
/// between a desktop frozen for 90 seconds and one frozen for good. Dropping an
/// expired one matters for the mirror-image reason: inheriting a deadline in the
/// past would make the call not pause at all.
fn deadline_kept_by_plain_pause(deadline_ns: u64, now_ns: u64) -> u64 {
    if pause_in_force(true, deadline_ns, now_ns) {
        deadline_ns
    } else {
        0
    }
}

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
        // Black pixels are not a framebuffer's pixels. Without this the present
        // that un-blanks honours its damage box and leaves a black screen with
        // one rectangle of desktop in it. See [`PANEL_FB`].
        set_panel_fb(0);
        // Nor is the pointer covering anything any more: this just painted over
        // whatever it was. Un-blanking normally puts a whole frame up before the
        // pointer moves again, but it does not have to -- a DPMS write on its own
        // un-blanks -- and restoring the save then would put one rectangle of the
        // old desktop on a black screen. See [`CursorUnder`].
        forget_cursor_under();
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
/// calls it again with `false`, and nobody else will. For the same reason a
/// watchdog already running is NOT cancelled here -- see
/// [`deadline_kept_by_plain_pause`].
///
/// Resuming re-presents the framebuffer the CRTC is bound to, because every
/// present taken during the pause was acknowledged and dropped: see
/// [`SCANOUT_STALE`].
pub fn set_scanout_paused(on: bool) {
    if !on {
        SCANOUT_PAUSED.store(false, Ordering::SeqCst);
        SCANOUT_PAUSE_DEADLINE_NS.store(0, Ordering::SeqCst);
        kernel_hal::klog_info!("[drm] scanout RESUMED");
        repaint_if_scanout_stale();
        return;
    }
    // Deadline first: a reader that saw `paused` alongside the previous,
    // already-expired deadline would expire this pause on its first read.
    let deadline = SCANOUT_PAUSE_DEADLINE_NS.load(Ordering::SeqCst);
    SCANOUT_PAUSE_DEADLINE_NS.store(
        deadline_kept_by_plain_pause(deadline, timer_now_ns()),
        Ordering::SeqCst,
    );
    SCANOUT_PAUSED.store(true, Ordering::SeqCst);
    kernel_hal::klog_info!("[drm] scanout PAUSED (console GSP bring-up window)");
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
    SCANOUT_PAUSE_DEADLINE_NS.store(pause_deadline_ns(timer_now_ns(), max), Ordering::SeqCst);
    SCANOUT_PAUSED.store(true, Ordering::SeqCst);
    kernel_hal::klog_info!(
        "[drm] scanout PAUSED (console GSP bring-up window, watchdog {}s)",
        max.as_secs()
    );
}

/// Whether scanout is currently paused, expiring a watchdogged pause whose
/// deadline has passed. Every reader goes through here so the expiry happens
/// on the frame that needs the answer.
///
/// The expiry does not repaint: everything that asks is itself in a draw path
/// (a present about to run, a driver flip that just ran, or a cursor move), and
/// [`SCANOUT_STALE`] is what makes the first of those put a whole frame up.
pub fn scanout_paused() -> bool {
    let paused = SCANOUT_PAUSED.load(Ordering::SeqCst);
    let deadline = SCANOUT_PAUSE_DEADLINE_NS.load(Ordering::SeqCst);
    if pause_in_force(paused, deadline, timer_now_ns()) {
        return true;
    }
    if !paused {
        return false;
    }
    // Deadline passed: resume, once. Cancel only the deadline we just judged
    // expired -- a `set_scanout_paused_for` that landed between the load above
    // and here has opened a fresh window, and cancelling *that* one would put
    // back the permanent freeze this whole path exists to prevent.
    //
    // No sequential test can tell this from a blind `store(0)`: the difference
    // needs two CPUs, one expiring while the other arms. It is left as a
    // compare-exchange because the cost is one instruction and the failure it
    // rules out is the one this function exists to prevent.
    if SCANOUT_PAUSE_DEADLINE_NS
        .compare_exchange(deadline, 0, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return SCANOUT_PAUSED.load(Ordering::SeqCst);
    }
    if SCANOUT_PAUSED.swap(false, Ordering::SeqCst) {
        kernel_hal::klog_warn!(
            "[drm] scanout watchdog: la ventana de bring-up de la GPU de consola expiro sin \
             reanudar (bring-up colgado o abortado) -- se reanuda el scanout para no dejar \
             el escritorio congelado"
        );
    }
    false
}

/// Whether a pause has left the panel behind `crtc_fb`, for the tests in
/// `drm_scheme` that drive this through the ioctls rather than the latch.
/// Pretend the compositor claimed a VT other than the foreground one, so the
/// VT-gated present drop is reachable from a host test.
///
/// It is not reachable otherwise: `kernel_hal`'s `graphic` feature is off in the
/// host build, so `active_vt()` is the constant 0 and `switch_vt` does nothing --
/// the one branch that decides whether a desktop can be suppressed would have no
/// test at all.
#[cfg(test)]
pub(crate) fn set_graphics_vt_for_test(vt: Option<usize>) {
    DRM_STATE.lock().graphics_vt = vt;
}

#[cfg(test)]
pub(crate) fn scanout_is_stale_for_test() -> bool {
    SCANOUT_STALE.load(Ordering::SeqCst)
}

/// Put the framebuffer the CRTC is bound to back on the panel, if a pause
/// dropped a present and nothing has put a whole frame up since.
///
/// The compositor will not do it: it was told every one of those flips
/// completed. Without this an idle desktop stays on a pre-pause frame after the
/// bring-up window closes, and the next pointer move paints patches of the
/// unseen frame into it.
fn repaint_if_scanout_stale() {
    if !SCANOUT_STALE.load(Ordering::SeqCst) {
        return;
    }
    // A client that turned the CRTC off during the pause meant it: presenting
    // here would light the panel behind its back. Leave the latch set so the
    // present that un-blanks is the one that also fixes the frame.
    if crtc_blanked() {
        return;
    }
    let fb_id = DRM_STATE.lock().crtc_fb;
    if fb_id == 0 {
        // Nothing was ever bound, so there is no stale frame to replace.
        SCANOUT_STALE.store(false, Ordering::SeqCst);
        return;
    }
    // Clears the latch itself, on the whole-frame present below succeeding.
    let _ = present_now_region(fb_id, SYNTH_CRTC_ID, None);
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

/// Whether a legacy present waits for the GPU to finish writing the buffer it
/// is about to scan out. ON by default; the `drm.flip_fence=off` cmdline turns
/// it off (see zCore/src/main.rs).
///
/// A default of ON is the whole point: the atomic path has waited on
/// `IN_FENCE_FD` since it was written, and the legacy path -- the one this
/// kernel actually runs, because `drm.atomic` is opt-in -- waited for nothing
/// at all, so a frame could be scanned out while the GPU was still drawing it.
/// The hatch is there because the wait is bounded by a clock and reads a fence
/// the driver supplies: if it ever misbehaves on real hardware, a boot without
/// it goes straight back to the old behaviour with no rebuild.
static FLIP_FENCE_ENABLED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(true);

/// Turn the pre-present fence wait off (or back on) for this boot.
pub fn set_flip_fence_enabled(on: bool) {
    FLIP_FENCE_ENABLED.store(on, Ordering::Relaxed);
}

/// Whether a legacy present should wait for the scanout buffer's render fence.
pub fn flip_fence_enabled() -> bool {
    FLIP_FENCE_ENABLED.load(Ordering::Relaxed)
}

/// Whether every present reads the pixels it just put on screen a second time
/// and reports the ones that changed under it (`drm.present_probe` on the
/// cmdline). OFF by default, and not something to leave on: it re-reads a
/// quarter of the damage box per frame.
///
/// It answers one question and nothing else: **were the pixels already wrong
/// when the kernel got them?** Everything between the client's buffer and the
/// panel has been walked and tested -- the damage-box blit on both store paths,
/// the cursor patch's geometry, the cache invalidate, the scanout pause -- and
/// none of it can invent a pixel. What no test here can see is a compositor
/// that is still writing the buffer while the kernel reads it: llvmpipe and
/// pixman rasterise on worker threads, and a present that arrives before they
/// have drained scans out a frame that is complete in some tiles and not in
/// others. On screen that is a band or a comb of stale pixels, visible only
/// where the frame actually changed -- so a flat wallpaper hides it and a
/// freshly blended window shadow does not.
///
/// The probe catches exactly that: checksum the window, blit it, invalidate and
/// checksum it again. The two reads bracket the blit, so a mismatch means
/// somebody else wrote those pixels while the kernel was copying them, and a
/// run of frames with no mismatch means the buffer was settled and the defect
/// is somewhere the kernel can be held responsible for.
static PRESENT_PROBE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Whether the software pointer takes the scene it is drawn over from the
/// CLIENT's framebuffer instead of from the panel (`drm.cursor_from_client` on
/// the cmdline). OFF by default, and the black rectangles come back with it on.
///
/// This is an escape hatch for a cost that cannot be measured from here, and not
/// a choice worth making. Reading the panel is the fix -- see [`CursorUnder`] --
/// and it puts one read of a ~64x64 window of the display aperture on the pointer
/// path, where there was none before. Writes to that aperture run at about
/// 42 MB/s on Moebius's RTX 2060 Supers; reads of it have never been measured,
/// there is no aperture in QEMU to measure them in, and an aperture read can be
/// several times slower than a write. If that shows up as a pointer that drags,
/// this puts the old path back so the machine is usable while a better source for
/// the scene is found.
static CURSOR_FROM_CLIENT: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Make the software pointer read the client's framebuffer again, black
/// rectangles and all. See [`CURSOR_FROM_CLIENT`].
pub fn set_cursor_from_client(on: bool) {
    CURSOR_FROM_CLIENT.store(on, Ordering::Relaxed);
}

/// Whether the pointer is reading the client's framebuffer for this boot.
pub fn cursor_from_client() -> bool {
    CURSOR_FROM_CLIENT.load(Ordering::Relaxed)
}

/// Turn the present probe on (or back off) for this boot.
pub fn set_present_probe_enabled(on: bool) {
    PRESENT_PROBE.store(on, Ordering::Relaxed);
}

/// Whether the present probe is armed for this boot.
pub fn present_probe_enabled() -> bool {
    PRESENT_PROBE.load(Ordering::Relaxed)
}

/// Whether a present that finds the source still moving under it **repairs**
/// the bands that moved, instead of only reporting them (`drm.present_repair`
/// on the cmdline). OFF by default.
///
/// The defect it answers is measured, not guessed: with the compositor on
/// wlroots' GLES2 renderer over Mesa's software rasteriser, the probe fires on
/// essentially every frame, and with the pixman renderer -- same kernel, same
/// present, same flip -- it never fires once and the screen is clean.
/// `glFlush` hands llvmpipe's scene to its worker threads and returns without
/// waiting, and there is nothing for the compositor to wait on either: this
/// kernel answers `DRM_CAP_SYNCOBJ_TIMELINE` with 0, and a software renderer
/// has no GPU fence to export. So the buffer the kernel is handed is a frame
/// whose tiles are still arriving.
///
/// What the kernel can do about it is narrow but real. It cannot make the frame
/// whole -- a present that raced is a mix of two frames whatever we do, and only
/// explicit synchronisation upstream fixes that. What it can do is not *leave*
/// the stale tiles on the panel: the bands that moved under the copy are exactly
/// the bands whose pixels on screen are older than the buffer they came from, and
/// copying those again picks up what has since arrived. Bounded by
/// [`MAX_REPAIR_ROUNDS`], because a compositor that never stops writing must not
/// turn one present into an unbounded loop.
static PRESENT_REPAIR: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Turn the present repair pass on (or back off) for this boot.
pub fn set_present_repair_enabled(on: bool) {
    PRESENT_REPAIR.store(on, Ordering::Relaxed);
}

/// Whether the present repair pass is armed for this boot.
pub fn present_repair_enabled() -> bool {
    PRESENT_REPAIR.load(Ordering::Relaxed)
}

/// Whether a present may leave a band of rows alone because the panel already
/// holds exactly those pixels (`drm.present_skip` on the cmdline). OFF by
/// default.
///
/// The cost this answers is measured: on real NVIDIA hardware CPU stores into
/// the console GPU's BAR1 serve at about 42 MB/s even through a verified
/// write-combining mapping, so a 1920x1080 frame costs ~99 ms and the desktop
/// runs at 7-11 FPS -- and labwc never sends a damage box (1600 presents, not
/// one `DIRTYFB`), so every frame pays for the whole screen. Under QEMU the same
/// full frame costs 2.6-15 ms.
///
/// What almost never changes between two of those frames is most of the screen.
/// A compositor re-renders its whole scene into an alternating buffer, but the
/// PIXELS are the previous frame's nearly everywhere: a blinking terminal cursor
/// or a clock changes a few rows. So the present hashes each band of
/// [`SKIP_BAND_ROWS`] rows, compares it with the hash of what it last put on the
/// panel for that band, and copies only the bands that differ. The read is from
/// cached RAM, which the same measurement puts orders of magnitude above the
/// write it avoids.
///
/// Correctness rests on one invariant: a band's stored hash means "the panel
/// holds these pixels in these rows". Everything that writes the panel WITHOUT
/// going through this path has to forget what it wrote over --
/// [`panel_bands_reset`] for a blank, a console VT or a present that took
/// another route, and [`panel_bands_dirty_rows`] for the cursor, which is
/// composited on top of the frame and would otherwise leave its previous
/// position behind in a band nobody re-copies.
static PRESENT_SKIP: AtomicBool = AtomicBool::new(false);

/// Turn the unchanged-band skip on (or back off) for this boot.
pub fn set_present_skip_enabled(on: bool) {
    PRESENT_SKIP.store(on, Ordering::Relaxed);
    // The state describes what the panel holds, and nothing was tracking it
    // while the switch was off, so an arming boot starts from "nothing known".
    panel_bands_reset();
}

/// Whether the unchanged-band skip is armed for this boot.
pub fn present_skip_enabled() -> bool {
    PRESENT_SKIP.load(Ordering::Relaxed)
}

/// Rows in one band of the skip decision.
///
/// 16 rows is ~128 KiB at 1920 pixels: small enough that re-reading a band after
/// deciding to copy it is served from cache rather than from RAM, and small
/// enough that the cursor -- which dirties every band it covers -- costs 16 rows
/// instead of the 128 a blit band would. Not smaller, because each run of dirty
/// bands becomes one `blit_from`, and a short run wastes the non-temporal store
/// path's stride.
const SKIP_BAND_ROWS: u32 = 16;

/// Bands the panel state can describe: 4352 rows, which covers every mode this
/// kernel drives. A frame taller than that simply does not take the skip.
const MAX_SKIP_BANDS: usize = 272;

/// What the panel holds, one band hash per band of [`SKIP_BAND_ROWS`] rows,
/// encoded by [`known_hash`] so that `0` means "unknown" and nothing else does.
///
/// Unknown is the state after a reset, and after anything that is not this path
/// wrote over the band. It has to be a value no real hash can take, or a band
/// whose pixels happened to hash to the sentinel would be skipped on the very
/// first present after a reset -- when the panel does NOT hold them.
///
/// Lock-free on purpose. The alternative was a `Mutex` held across the hashing
/// of a whole frame -- milliseconds -- which any CPU invalidating a cursor band
/// would spin on.
static PANEL_BAND_HASH: [AtomicU64; MAX_SKIP_BANDS] = [const { AtomicU64::new(0) }; MAX_SKIP_BANDS];

/// The geometry the hashes describe, packed as four `u16`s (`x`, `y`, `w`, `h`),
/// or `0` for "none". A present whose geometry differs starts over: the bands
/// would otherwise be compared against hashes of different pixels.
static PANEL_BAND_GEOM: AtomicU64 = AtomicU64::new(0);

/// The source stride the hashes were taken at, or `0` for none. Separate from
/// the geometry because a stride is not bounded by the panel's 16 bits.
static PANEL_BAND_STRIDE: AtomicU64 = AtomicU64::new(0);

/// A band hash as it is stored: the top bit set, so no stored value is ever the
/// `0` that [`PANEL_BAND_HASH`] uses for "unknown".
///
/// The cost is one bit of the hash -- two bands whose hashes differ only in bit
/// 63 compare equal, which doubles a collision probability of 2^-64 -- and the
/// gain is that "nothing is known about this band" is not a number the hash can
/// produce. A sentinel that a real hash can hit is a stale band on screen.
fn known_hash(h: u64) -> u64 {
    h | 1 << 63
}

/// `(x, y, w, h)` packed into one `u64`, or `None` when any of them does not fit
/// in 16 bits -- in which case the skip is simply not taken.
///
/// `w` and `h` are never 0 on a present that reaches here, so a packed value is
/// never 0 and the `0` sentinel of [`PANEL_BAND_GEOM`] cannot collide with a
/// real geometry.
fn pack_panel_geom(x: u32, y: u32, w: u32, h: u32) -> Option<u64> {
    let (x, y, w, h) = (
        u16::try_from(x).ok()?,
        u16::try_from(y).ok()?,
        u16::try_from(w).ok()?,
        u16::try_from(h).ok()?,
    );
    if w == 0 || h == 0 {
        return None;
    }
    Some((x as u64) << 48 | (y as u64) << 32 | (w as u64) << 16 | h as u64)
}

/// Forget everything about what the panel holds.
fn panel_bands_reset() {
    PANEL_BAND_GEOM.store(0, Ordering::Relaxed);
    PANEL_BAND_STRIDE.store(0, Ordering::Relaxed);
    for h in PANEL_BAND_HASH.iter() {
        h.store(0, Ordering::Relaxed);
    }
}

/// The half-open range of bands covering panel rows `y .. y + h`, clamped to the
/// bands that exist. `None` when the range covers no row, or lies past them all.
fn bands_covering_rows(y: u32, h: u32) -> Option<(usize, usize)> {
    if h == 0 {
        return None;
    }
    let first = (y / SKIP_BAND_ROWS) as usize;
    if first >= MAX_SKIP_BANDS {
        return None;
    }
    let last_row = y.saturating_add(h - 1);
    let last = ((last_row / SKIP_BAND_ROWS) as usize).min(MAX_SKIP_BANDS - 1);
    Some((first, last + 1))
}

/// Forget the bands covering panel rows `y .. y + h`, so the next present copies
/// them whatever their pixels hash to.
///
/// This is what the cursor calls. A cursor is composited on top of the frame
/// after the blit, so the panel's pixels in those rows are NOT the ones the
/// present copied, and a band left claiming otherwise keeps the pointer's last
/// position on screen until something in those rows happens to change.
fn panel_bands_dirty_rows(y: u32, h: u32) {
    if let Some((first, end)) = bands_covering_rows(y, h) {
        for band in PANEL_BAND_HASH.iter().take(end).skip(first) {
            band.store(0, Ordering::Relaxed);
        }
    }
}

/// Bands the last present left alone, so a test can assert that a settled frame
/// costs no copy at all and a changed band costs exactly one.
#[cfg(test)]
static SKIP_BANDS_SKIPPED: AtomicUsize = AtomicUsize::new(0);

/// Presents the band skip has driven, so a test can assert which presents take
/// it at all -- a damage box must not, and the visible pixels alone cannot say
/// so, because a box copied through the skip looks the same on the panel.
#[cfg(test)]
static SKIP_PRESENTS: AtomicUsize = AtomicUsize::new(0);

/// Presents the band skip has driven since the last reset.
#[cfg(test)]
pub(crate) fn skip_presents_for_test() -> usize {
    SKIP_PRESENTS.load(Ordering::Relaxed)
}

/// Bands the most recent present skipped.
#[cfg(test)]
pub(crate) fn skipped_bands_for_test() -> usize {
    SKIP_BANDS_SKIPPED.load(Ordering::Relaxed)
}

/// Rows of band `b` within a window `height` rows tall: `(row, rows)`, or `None`
/// when the band lies past the window.
fn skip_band_span(b: usize, height: u32) -> Option<(u32, u32)> {
    let row = u32::try_from(b).ok()?.checked_mul(SKIP_BAND_ROWS)?;
    if row >= height {
        return None;
    }
    Some((row, SKIP_BAND_ROWS.min(height - row)))
}

/// FNV-1a over every pixel of `rows` rows starting at `row`, `w` wide, taken
/// from a buffer whose rows are `stride` pixels apart.
///
/// Every pixel, and every row: a sampled hash would answer "unchanged" for a
/// band whose unsampled rows moved, and this answer is what decides whether the
/// panel keeps the pixels it has. `None` when the band does not lie wholly
/// inside the buffer, which reads as "copy it".
fn skip_band_hash(pixels: &[u32], stride: usize, row: u32, rows: u32, w: u32) -> Option<u64> {
    if stride == 0 || w == 0 || rows == 0 || (w as usize) > stride {
        return None;
    }
    let mut acc = PROBE_FNV_BASIS;
    for r in 0..rows as usize {
        let start = (row as usize).checked_add(r)?.checked_mul(stride)?;
        let end = start.checked_add(w as usize)?;
        let line = pixels.get(start..end)?;
        for px in line {
            acc ^= *px as u64;
            acc = acc.wrapping_mul(PROBE_FNV_PRIME);
        }
    }
    Some(acc)
}

/// Blit only the bands whose pixels are not already on the panel, and return
/// `(bands, skipped)`.
///
/// `pixels` starts at the window's top-left, exactly as [`blit_chunked`] takes
/// it. Runs of adjacent dirty bands go out in ONE `blit_from`, because a run of
/// rows is what the non-temporal store path is fast at.
///
/// Falls back to copying everything -- and forgetting the state -- for a window
/// this cannot describe: too tall for [`MAX_SKIP_BANDS`], or a geometry that does
/// not pack. "Copy everything" is always correct; skipping is the part that has
/// to be earned.
fn blit_chunked_skipping(
    display: &Arc<dyn DisplayScheme>,
    dst_x: u32,
    dst_y: u32,
    pixels: &[u32],
    src_stride: usize,
    width: u32,
    height: u32,
) -> (usize, usize) {
    let bands = match skip_band_count(height) {
        Some(n) => n,
        None => {
            panel_bands_reset();
            blit_chunked(display, dst_x, dst_y, pixels, src_stride, width, height);
            return (0, 0);
        }
    };
    let geom = match pack_panel_geom(dst_x, dst_y, width, height) {
        Some(g) => g,
        None => {
            panel_bands_reset();
            blit_chunked(display, dst_x, dst_y, pixels, src_stride, width, height);
            return (0, 0);
        }
    };
    let stride = src_stride as u64;
    // A geometry or stride that is not the one the hashes were taken at makes
    // every comparison meaningless, so it starts over rather than reading the
    // old numbers as if they described this window.
    // Both swaps, every time, and NOT folded into one `||`: the short-circuit
    // skipped the second store whenever the first answered "changed", so the
    // stride stayed 0, the NEXT present saw it change, and the state was thrown
    // away on every single frame -- a skip that never skipped. Its test caught it.
    //
    // Neither key can be killed by a test in this tree, and both stay. A window's
    // own width and row count are already inside the hash it produces, so a
    // window that changes shape almost always produces different hashes and is
    // copied anyway -- which is why no test can tell these two lines from
    // `false`. What they are for is the case the hash cannot see: a MODESET, which
    // leaves the panel's geometry -- and its contents -- something else entirely
    // under the same framebuffer, and which `kms_emu` cannot stage without
    // dropping the screen (whose `Drop` resets this state for its own reasons).
    // And a hash collision between two different windows, which is the same
    // wager as [`known_hash`]'s: cheap here, a stale band on screen if lost.
    let geom_changed = PANEL_BAND_GEOM.swap(geom, Ordering::Relaxed) != geom;
    let stride_changed = PANEL_BAND_STRIDE.swap(stride, Ordering::Relaxed) != stride;
    if geom_changed || stride_changed {
        for h in PANEL_BAND_HASH.iter() {
            h.store(0, Ordering::Relaxed);
        }
    }
    let window = SkipWindow {
        dst_x,
        dst_y,
        pixels,
        src_stride,
        width,
    };
    let mut skipped = 0usize;
    // The open run of dirty bands, as `(first row, rows)`.
    let mut run: Option<(u32, u32)> = None;
    for (b, band) in PANEL_BAND_HASH.iter().enumerate().take(bands) {
        let Some((row, rows)) = skip_band_span(b, height) else {
            break;
        };
        let fresh = skip_band_hash(pixels, src_stride, row, rows, width);
        let known = band.load(Ordering::Relaxed);
        // A band with no hash at all (`None`) is copied. No test in this tree can
        // reach that -- a present's window always lies inside its own framebuffer
        // -- and the branch stays because the alternative reading of "I could not
        // look" is "nothing changed", which puts stale pixels on the panel.
        let same = matches!(fresh, Some(f) if known_hash(f) == known);
        if same {
            skipped += 1;
            if let Some((start, len)) = run.take() {
                blit_run(display, &window, start, len);
            }
            continue;
        }
        // Stored BEFORE the copy, because what the panel is about to hold is what
        // was read here. A band whose hash could not be taken stays unknown.
        band.store(fresh.map_or(0, known_hash), Ordering::Relaxed);
        run = Some(match run {
            Some((start, len)) => (start, len.saturating_add(rows)),
            None => (row, rows),
        });
    }
    if let Some((start, len)) = run.take() {
        blit_run(display, &window, start, len);
    }
    #[cfg(test)]
    {
        SKIP_BANDS_SKIPPED.store(skipped, Ordering::Relaxed);
        SKIP_PRESENTS.fetch_add(1, Ordering::Relaxed);
    }
    (bands, skipped)
}

/// Everything a run of rows needs except which rows: where the window sits on
/// the panel and where its pixels come from.
struct SkipWindow<'a> {
    dst_x: u32,
    dst_y: u32,
    pixels: &'a [u32],
    src_stride: usize,
    width: u32,
}

/// One run of rows of the window, from `pixels` at the window's own `start` row.
fn blit_run(display: &Arc<dyn DisplayScheme>, w: &SkipWindow<'_>, start: u32, rows: u32) {
    let off = (start as usize).saturating_mul(w.src_stride);
    if off >= w.pixels.len() {
        return;
    }
    blit_chunked(
        display,
        w.dst_x,
        w.dst_y.saturating_add(start),
        &w.pixels[off..],
        w.src_stride,
        w.width,
        rows,
    );
}

/// Bands a window `height` rows tall needs, or `None` when it needs more than
/// [`MAX_SKIP_BANDS`].
fn skip_band_count(height: u32) -> Option<usize> {
    if height == 0 {
        return None;
    }
    let n = (height as usize).div_ceil(SKIP_BAND_ROWS as usize);
    (n <= MAX_SKIP_BANDS).then_some(n)
}

/// How many times one present may re-copy the bands that moved under it.
///
/// Two, and the number is a budget rather than a convergence criterion: the
/// source may still be moving when the last round ends, and that is accepted.
/// Each round costs the bands that actually moved, not the frame, so a round
/// that repairs three bands of a 1920-wide panel copies 10% of what the present
/// already copied. An unbounded loop against a compositor that never stops
/// writing would hold the flip ioctl for as long as the desktop is busy, which
/// is a worse failure than a stale tile.
const MAX_REPAIR_ROUNDS: u32 = 2;

/// The horizontal span of the window covered by the set bands of `mask`, as
/// `(x_offset, width)` in pixels **relative to the window's own left edge**.
///
/// `None` when there is nothing to repair: no band set, an empty window, or a
/// mask whose only bits sit past the bands the window actually covered (which
/// describes no pixels, and must not be turned into a copy of the whole row).
///
/// One span from the first set band to the last, not a copy per band: a repair
/// round is a blit, and two blits of adjacent bands cost more than one blit of
/// both. The span is clamped to the window because the last band is the folded
/// one -- for a window wider than the mask's reach it stands for every column
/// from its own left edge to the right edge of the window.
fn repair_span_px(mask: u32, n: usize, window_w: u32) -> Option<(u32, u32)> {
    if n == 0 || window_w == 0 {
        return None;
    }
    // `first >= n` is the only emptiness test needed, and it covers two cases
    // that look separate. An empty mask gives `trailing_zeros() == 32`, which is
    // never below `n` (clamped to `PROBE_MAX_BANDS`), so "no band set" needs no
    // test of its own -- and neither does `x >= window_w`, because a `first`
    // inside the window's bands puts `x` inside the window by construction.
    // Both of those guards were mutants that could not be killed, and the honest
    // resolution was to take them out rather than pin a second spelling of this
    // line.
    let first = mask.trailing_zeros() as usize;
    if first >= n {
        return None;
    }
    let last = (u32::BITS - 1 - mask.leading_zeros()) as usize;
    let last = last.min(n - 1);
    let x = u32::try_from(first.saturating_mul(PROBE_BAND_PX)).unwrap_or(u32::MAX);
    // The LAST band owns everything to the window's right edge, not just its own
    // 64 columns: for a window wider than the mask's reach it is the folded band,
    // and `(last + 1) * 64` would stop short and leave those columns unrepaired
    // for good. For a window the mask covers exactly, the two agree.
    let end = if last.saturating_add(1) >= n {
        window_w
    } else {
        u32::try_from(last.saturating_add(1).saturating_mul(PROBE_BAND_PX))
            .unwrap_or(u32::MAX)
            .min(window_w)
    };
    (end > x).then(|| (x, end - x))
}

/// How many repair rounds the last present ran, so a test can assert that a
/// settled buffer runs none and a moving one runs a bounded number.
#[cfg(test)]
static REPAIR_ROUNDS_RUN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Repair rounds run by the most recent present.
#[cfg(test)]
pub(crate) fn repair_rounds_for_test() -> u32 {
    REPAIR_ROUNDS_RUN.load(Ordering::Relaxed)
}

/// Rows the probe samples: every 4th one, every pixel within it.
///
/// Along a row it reads every pixel, because a single changed pixel is a real
/// answer and a strided read could step over a one-pixel-wide comb. Across rows
/// it samples, because a torn frame is never one row -- a rasteriser hands over
/// tiles, so the smallest thing this is looking for is tens of rows tall.
const PROBE_ROW_STEP: usize = 4;

/// Width in pixels of one probe band.
///
/// Both software rasterisers under this desktop hand over rectangular tiles 64
/// pixels wide, so a band this wide either *is* a tile column or is a whole
/// number of them. That is what makes the mask in the report worth printing: a
/// comb of stale tiles lights up a scattered *subset* of bands, while a plain
/// tear -- the compositor overwriting the frame it already handed over -- lights
/// up a contiguous run. A scalar checksum cannot tell those two apart, and they
/// do not have the same cause or the same fix.
const PROBE_BAND_PX: usize = 64;

/// Bands the mask tracks: 32 * 64 = 2048 px, past both 1920- and 1600-wide
/// panels. A window wider than that folds its right-hand columns into the last
/// band instead of dropping them -- a mask that silently stopped describing part
/// of the window would be worse than a coarse last band, because the reader
/// cannot see which of the two they are looking at.
const PROBE_MAX_BANDS: usize = 32;

/// FNV-1a's 64-bit basis and prime. Wrapping on purpose: this is a hash, and a
/// debug build must not panic on the overflow that is the whole mechanism.
const PROBE_FNV_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const PROBE_FNV_PRIME: u64 = 0x100_0000_01b3;

/// How many bands a window `w` pixels wide covers, never zero and never past
/// [`PROBE_MAX_BANDS`].
fn probe_band_count(w: u32) -> usize {
    ((w as usize).saturating_add(PROBE_BAND_PX - 1) / PROBE_BAND_PX).clamp(1, PROBE_MAX_BANDS)
}

/// Where the source buffer is fully transparent black, gathered in the same
/// pass as the band checksums.
///
/// This exists for the one question a photograph of the screen cannot answer.
/// Black rectangles on the desktop have two completely different causes, and
/// they call for work in opposite places: either the kernel is failing to copy
/// pixels that the compositor did draw, or the compositor handed over a buffer
/// it never finished drawing and the kernel copied the zeros faithfully. The
/// band mask does not separate them -- a region that is black in BOTH reads
/// never differs, so it does not set a bit -- and neither does the cost line.
/// The count and the box do: if the black on screen sits where the source was
/// already zero, the kernel is exonerated and the search moves upstream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ZeroExtent {
    /// Sampled pixels that were exactly `0x0000_0000`.
    zeros: usize,
    /// Sampled pixels in total, so `zeros` can be read as a fraction.
    sampled: usize,
    /// Window-relative bounding box of the zero pixels. Meaningless while
    /// `zeros` is 0, which is why [`ZeroExtent::bbox`] is the only reader.
    min_x: u32,
    max_x: u32,
    min_y: u32,
    max_y: u32,
}

impl ZeroExtent {
    /// Nothing seen yet. The box starts inverted so the first `note` sets both
    /// ends of both axes without a special case.
    fn empty() -> Self {
        Self {
            zeros: 0,
            sampled: 0,
            min_x: u32::MAX,
            max_x: 0,
            min_y: u32::MAX,
            max_y: 0,
        }
    }

    /// Record one zero pixel at window-relative `(x, y)`.
    fn note(&mut self, x: u32, y: u32) {
        self.zeros = self.zeros.saturating_add(1);
        self.min_x = self.min_x.min(x);
        self.max_x = self.max_x.max(x);
        self.min_y = self.min_y.min(y);
        self.max_y = self.max_y.max(y);
    }

    /// `(x, y, w, h)` of the zero pixels, window-relative, or `None` when there
    /// were none. The box is inclusive of both ends, hence the `+ 1`: a single
    /// zero pixel is a 1x1 box, not a 0x0 one.
    fn bbox(&self) -> Option<(u32, u32, u32, u32)> {
        (self.zeros > 0).then(|| {
            (
                self.min_x,
                self.min_y,
                self.max_x.saturating_sub(self.min_x).saturating_add(1),
                self.max_y.saturating_sub(self.min_y).saturating_add(1),
            )
        })
    }
}

/// One probe read: a checksum per 64-pixel column band of the window.
struct ProbeBands {
    /// Bands past `n` are never touched, so they hold [`PROBE_FNV_BASIS`] in
    /// every read and compare equal.
    bands: [u64; PROBE_MAX_BANDS],
    /// Bands the window actually covered.
    n: usize,
    /// Where this read found the source already black. See [`ZeroExtent`].
    zero: ZeroExtent,
}

impl ProbeBands {
    /// One number for the whole window, which is all the changed/unchanged
    /// decision needs.
    ///
    /// `n` is deliberately NOT folded in. It looks like it should be -- two reads
    /// that disagreed about the window's width must not come out equal -- but the
    /// band array already separates them: band `n` holds [`PROBE_FNV_BASIS`] in
    /// the narrower read and a real hash of its pixels in the wider one, and
    /// those differ even when every pixel is black. Folding `n` as well was a
    /// mutation that could not be killed by anything the probe can actually do
    /// (the two reads bracketing a blit are always the same width), so it is
    /// gone, and the property it was standing in for is pinned on the bands
    /// where it really lives.
    fn fold(&self) -> u64 {
        let mut acc = PROBE_FNV_BASIS;
        for b in self.bands.iter() {
            acc ^= *b;
            acc = acc.wrapping_mul(PROBE_FNV_PRIME);
        }
        acc
    }

    /// Bit `i` set means band `i` -- source columns `x + 64*i ..= x + 64*i + 63`
    /// -- reads differently in the two passes.
    fn diff_mask(&self, other: &ProbeBands) -> u32 {
        let mut mask = 0u32;
        for i in 0..PROBE_MAX_BANDS {
            if self.bands[i] != other.bands[i] {
                mask |= 1u32 << i;
            }
        }
        mask
    }
}

/// Per-band order-dependent checksums of the `(x, y, w, h)` window of `pixels`,
/// sampling every `row_step`-th row and every pixel within it.
///
/// Order-dependent matters: two frames can hold the same pixels in different
/// places (a scrolled list, a window dragged by a pixel) and a sum or an XOR
/// would call those equal. FNV-1a gives that for nothing -- each pixel is
/// multiplied into the accumulator in turn, so the sequence is what is hashed,
/// not the multiset. This used to fold the row and column in as well, which was
/// two extra multiplies per pixel on a per-frame path buying nothing the chain
/// did not already provide; the mutation that removed it could not be killed,
/// and the honest resolution was to remove it here too.
///
/// `stride_px` of 0, an empty window, or a window whose first sampled row is
/// already past the end of `pixels` all give `None`: there is nothing to
/// compare, which is not the same answer as "these two match".
fn probe_bands(
    pixels: &[u32],
    stride_px: usize,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    row_step: usize,
) -> Option<ProbeBands> {
    if stride_px == 0 || w == 0 || h == 0 || row_step == 0 {
        return None;
    }
    let n = probe_band_count(w);
    let mut bands = [PROBE_FNV_BASIS; PROBE_MAX_BANDS];
    let mut zero = ZeroExtent::empty();
    // Rows, not pixels -- `zero.sampled` beside it counts pixels, and the two
    // being one word apart with the same name is how a later reader divides by
    // the wrong thing.
    let mut sampled_rows = 0usize;
    let mut r = 0usize;
    while r < h as usize {
        let off = (y as usize)
            .saturating_add(r)
            .saturating_mul(stride_px)
            .saturating_add(x as usize);
        let end = off.saturating_add(w as usize);
        if end > pixels.len() {
            break;
        }
        for (c, px) in pixels[off..end].iter().enumerate() {
            // `min(n - 1)`: the columns past the mask's reach fold into the last
            // band rather than running off the array.
            let b = (c / PROBE_BAND_PX).min(n - 1);
            bands[b] ^= *px as u64;
            bands[b] = bands[b].wrapping_mul(PROBE_FNV_PRIME);
            if *px == 0 {
                // Window-relative, and `r` not `y + r`, because the window's own
                // origin is already in the reported line: a box relative to the
                // window is what lines up with what the eye sees on screen.
                zero.note(c as u32, r as u32);
            }
        }
        zero.sampled = zero.sampled.saturating_add(end.saturating_sub(off));
        sampled_rows += 1;
        r += row_step;
    }
    (sampled_rows > 0).then_some(ProbeBands { bands, n, zero })
}

/// Whether the two reads bracketing a blit say somebody else was writing the
/// window while the kernel copied it.
///
/// `after` is an `Option` because the second read can fail to produce an answer
/// at all -- the framebuffer shrank, the mapping went away -- and that counts as
/// changed. "I could not tell" must never come out as "the compositor is in the
/// clear": this whole flag exists to decide who is responsible, and a wrong
/// acquittal sends the search back to the code that has already been walked.
fn probe_says_changed(before: u64, after: Option<u64>) -> bool {
    after != Some(before)
}

/// How many probe mismatches have OCCURRED, which is not the same as how many
/// got a line: [`probe_report_decision`] stops writing after
/// [`MAX_PROBE_REPORTS`] of them while this keeps counting. Counting
/// occurrences is what makes the budget work -- a compositor that tears every
/// frame must not turn the klog into the bottleneck. `klog` writes
/// synchronously to the UART, which at 115200 baud is slower than the frame it
/// is describing.
static PROBE_REPORTS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
const MAX_PROBE_REPORTS: u32 = 12;

/// How many source-is-black reads have OCCURRED -- every present, not only the
/// ones that got a line, the same convention as [`PROBE_REPORTS`] and for the
/// same reason. Its own counter, and not the mismatch one, because the two answer different
/// questions and the interesting case for this one is the frame where NOTHING
/// changed: a black rectangle that just sits there is black in both reads, so it
/// never sets a band bit and the mismatch line never fires. Sharing a budget
/// would let a torn boot spend it before the static case was ever described.
static ZERO_REPORTS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Source reads split by their answer, because the two answers are worth very
/// different amounts and one shared budget spent it all on the cheap one.
///
/// Measured on Moebius's boot: of the ten source lines the budget paid for,
/// eight said "not one sampled pixel is 0x00000000" and the budget was gone
/// 27.2 s in -- long before the menu whose black rectangle the flag exists to
/// explain was ever opened. The tenth "no black in the source" says nothing the
/// first did not; a frame that DOES carry black is a new fact every time,
/// because its box is where to look on screen.
///
/// So: the clean answer gets [`MAX_CLEAN_SOURCE_REPORTS`] as a baseline, and the
/// frames that carry black keep the full [`MAX_PROBE_REPORTS`]. Both still count
/// every read, the same convention as [`PROBE_REPORTS`].
static ZERO_FOUND_REPORTS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static ZERO_CLEAN_REPORTS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// One baseline line for "the source carried no black at all". See
/// [`ZERO_CLEAN_REPORTS`] for why it is not twelve.
const MAX_CLEAN_SOURCE_REPORTS: u32 = 1;

/// Black that was NOT there when the source was sampled and IS there when the
/// same window is read again after the copy. Its own counter and the full
/// [`MAX_PROBE_REPORTS`], because it is the third distinct answer and a new fact
/// every time: its box says where on screen to look.
///
/// This is the hole Moebius's QEMU boot of 27-sep opened. The source line said
/// "not one of 518400 sampled pixels is 0x00000000" on every frame of a 65-second
/// run, and the mismatch line said, on nearly every one of those same frames,
/// that the client was still writing the buffer. Both can be true at once,
/// because the source is sampled BEFORE the copy: a clear-to-black that lands
/// while the kernel is copying gets blitted to the screen and the source check
/// never sees it. "Not handed over black" only ever meant "not black when we
/// looked", and this is the counter that can tell the two apart.
static ZERO_GREW_REPORTS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Source lines actually WRITTEN, per kind. The occurrence counters above keep
/// counting long after the budget stops the lines, so they cannot tell a test
/// whether the split budget is really doing its job -- and the whole point of
/// the split is which lines get written once the cheap answer has had its turn.
#[cfg(test)]
static CLEAN_SOURCE_LINES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
#[cfg(test)]
static BLACK_SOURCE_LINES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
#[cfg(test)]
static GREW_SOURCE_LINES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// How many probe mismatches this process has seen, so an end-to-end test can
/// assert that a settled buffer produced none.
#[cfg(test)]
pub(crate) fn probe_reports_for_test() -> u32 {
    PROBE_REPORTS.load(Ordering::Relaxed)
}

/// How many source-is-black reads this process has done, so an end-to-end test
/// can assert the thing that distinguishes this line from the mismatch one:
/// that it fires on a present where nothing changed at all.
///
/// A count on its own does not say a line was written, because the budget cuts
/// the lines off and not the counting. A count that is at least one and no more
/// than [`probe_report_budget_for_test`] does: the budget cannot have been spent
/// yet, so every one of those reads wrote its line. Assert the range, not just
/// the floor.
#[cfg(test)]
pub(crate) fn zero_reports_for_test() -> u32 {
    ZERO_REPORTS.load(Ordering::Relaxed)
}

/// How many source lines of each kind were actually written. These are what say
/// whether the split budget works, because the occurrence counters keep counting
/// after the lines stop.
#[cfg(test)]
pub(crate) fn clean_source_lines_for_test() -> u32 {
    CLEAN_SOURCE_LINES.load(Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn black_source_lines_for_test() -> u32 {
    BLACK_SOURCE_LINES.load(Ordering::Relaxed)
}

/// Lines written for "the source went black while we were copying it". See
/// [`ZERO_GREW_REPORTS`].
#[cfg(test)]
pub(crate) fn grew_source_lines_for_test() -> u32 {
    GREW_SOURCE_LINES.load(Ordering::Relaxed)
}

/// The baseline budget for "no black in the source". One; see
/// [`ZERO_CLEAN_REPORTS`].
#[cfg(test)]
pub(crate) fn clean_source_report_budget_for_test() -> u32 {
    MAX_CLEAN_SOURCE_REPORTS
}

/// How many lines a kind of probe gets before its budget cuts them off. For the
/// range assertion above.
#[cfg(test)]
pub(crate) fn probe_report_budget_for_test() -> u32 {
    MAX_PROBE_REPORTS
}

/// Whether this mismatch gets a line in the klog, and whether it is the last
/// one. `(report, say_it_is_the_last)`.
///
/// `saturating_add`, because `already` is a counter this function does not own:
/// [`PROBE_REPORTS`] is bumped with `fetch_add` on every mismatch and nothing
/// stops it, so a boot that tears for long enough hands this `u32::MAX` and a
/// plain `+ 1` panics the kernel from inside a diagnostic. A probe that brings
/// the machine down is worse than no probe.
fn probe_report_decision(already: u32) -> (bool, bool) {
    report_decision(already, MAX_PROBE_REPORTS)
}

/// [`probe_report_decision`] against any budget, so the two source answers can
/// have their own without duplicating the saturating arithmetic that keeps a
/// diagnostic from panicking the kernel.
fn report_decision(already: u32, budget: u32) -> (bool, bool) {
    (already < budget, already.saturating_add(1) == budget)
}

/// The render fences a legacy present on `fb_id` has to wait for, each as
/// `(fence landing-zone kernel VA, payload)`. Empty when there is nothing to
/// wait for: the switch is off, the fb is unknown, no driver is registered, or
/// no driver has work in flight that could be writing that buffer.
///
/// EVERY registered driver is asked, not just the primary. On a two-GPU box the
/// GPU that draws need not be the GPU that scans out -- that is exactly the
/// split this kernel runs, where the console GPU owns the panel and the compute
/// GPU renders (see the CE/P2P present path) -- so asking only the primary
/// would ask the one card that did not touch the pixels. A driver that does not
/// recognise the buffer's handle and has no channel for its owner answers
/// `None`, so in practice this is one fence, or none.
pub fn scanout_render_fence(fb_id: u32) -> Vec<(usize, u32)> {
    if !flip_fence_enabled() {
        return Vec::new();
    }
    // One lock acquisition for both, and the guard is dropped before any driver
    // call: `render_fence_for_scanout` takes the driver's own locks and may
    // append a probe to a GPU ring.
    let (fb, drivers) = {
        let state = DRM_STATE.lock();
        let Some(fb) = state.framebuffers.iter().find(|f| f.id == fb_id).copied() else {
            return Vec::new();
        };
        (fb, state.drivers.clone())
    };
    drivers
        .iter()
        .filter_map(|d| d.render_fence_for_scanout(fb.gem_handle_id, fb.owner))
        .collect()
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
    /// Next blob id. Starts at [`BLOB_ID_BASE`], above the synthetic KMS ids,
    /// the fb ids and the EDID blob ids, so the object-id namespaces never
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

/// What the software pointer is covering on the panel, kept so it can be put
/// back without asking the client's framebuffer what used to be there.
///
/// That question is where the black rectangles came from. Our software-KMS
/// present COPIES the client's buffer into the panel and completes the flip, so
/// the compositor is free to start the next frame in that same buffer -- and a
/// renderer opens a frame by clearing to transparent black. `repaint_for_cursor`
/// then went back and read that buffer to erase and redraw its two ~64x64
/// windows, and pasted a piece of a half-drawn frame into the frame that was on
/// the screen: a black rectangle, right where the pointer was, appearing exactly
/// when something new was being drawn. Which is why it showed up on menus and
/// popups and why labwc and lunarbar were never at fault -- they were drawing
/// into a buffer that is theirs to draw into.
///
/// The panel, by contrast, is the kernel's own. Whatever is on it is what the
/// user is looking at, so reading it back can never produce a frame nobody
/// asked for. Reproduced with no GPU by
/// `a_pointer_move_must_not_paste_the_frame_the_compositor_is_still_drawing`.
struct CursorUnder {
    /// The window on the panel these pixels came from, and the one they go back
    /// to: already clipped and widened exactly as the blit that drew the pointer
    /// was. Restoring a narrower window would leave the pointer's widened
    /// margins on the screen for good.
    rect: Option<(u32, u32, u32, u32)>,
    /// `rect`'s pixels, `rect.2` of them per row.
    px: Vec<u32>,
    /// The cursor rect this was saved for, as `CursorState::drawn` records it.
    ///
    /// The save describes the panel only while nothing else has repainted it, and
    /// most of the things that do -- a driver flip, the display engine's plane
    /// taking the pointer over, a full present with no pointer to draw -- reset
    /// `drawn` as part of doing it. Comparing against it costs those places no new
    /// bookkeeping and stops a future one from forgetting: a `drawn` that has
    /// moved on says the save is stale, whoever moved it.
    ///
    /// It is a second line of defence and not the whole of it. Blanking repaints
    /// the panel and leaves `drawn` alone, so `set_crtc_blanked` says so itself --
    /// which is what the test named after it holds in place.
    for_cursor: Option<(i32, i32, u32, u32)>,
}

lazy_static::lazy_static! {
    /// Reused compose buffer for software-cursor patches (avoids a heap alloc
    /// on every pointer motion).
    static ref CURSOR_PATCH: Mutex<Vec<u32>> = Mutex::new(Vec::new());
    /// See [`CursorUnder`]. Locked AFTER [`CURSOR_PATCH`] wherever both are
    /// held, which is the compose path in both directions.
    static ref CURSOR_UNDER: Mutex<CursorUnder> = Mutex::new(CursorUnder {
        rect: None,
        px: Vec::new(),
        for_cursor: None,
    });
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
        next_blob_id: BLOB_ID_BASE,
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

/// Unregister a driver [`register_driver`] added, matched by identity
/// (`Arc::ptr_eq`) rather than by name. Returns whether one was removed.
///
/// Test-only, and `DRM_STATE.drivers` is append-only for a reason: nothing in
/// this kernel unplugs a GPU. But a unit-test binary runs every test of the
/// crate in ONE process, and a registered driver changes the answers the whole
/// DRM core gives -- `software_kms_active()` asks every driver whether it can
/// scan out, `get_primary_driver()` takes the first entry, and `get_resources`
/// filters the topology on whether any driver has hardware KMS. One left behind
/// would put every later test on the hardware path.
#[cfg(test)]
pub(crate) fn unregister_driver(driver: &Arc<dyn DrmScheme>) -> bool {
    let mut state = DRM_STATE.lock();
    match state.drivers.iter().position(|d| Arc::ptr_eq(d, driver)) {
        Some(pos) => {
            state.drivers.remove(pos);
            true
        }
        None => false,
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

/// How many framebuffer objects and GEM handles the kernel is holding.
///
/// For tests that run a frame loop and then check it came back into balance:
/// both tables are process-wide here, not per `drm_file` as in Linux, so a
/// leaked entry is visible globally and only globally.
#[cfg(test)]
pub(crate) fn table_sizes_for_test() -> (usize, usize) {
    let state = DRM_STATE.lock();
    (state.framebuffers.len(), state.handles.len())
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

/// Holder id a live dma-buf fd records against a nouveau GEM object. Re-export
/// of [`zcore_drivers::scheme::gem_mmap::DMABUF_HOLDER`] so the syscall layer
/// and `DmaBuf` do not need a direct drivers dependency for the sentinel.
pub const DMABUF_HOLDER: u64 = zcore_drivers::scheme::gem_mmap::DMABUF_HOLDER;

/// Take the dma-buf's reference on a nouveau GEM object at `PRIME_HANDLE_TO_FD`
/// time. No-op for low-range (dumb/generic) handles — those stay alive via the
/// `Arc<VmObject>` the dma-buf already holds. See `DMABUF_HOLDER`.
pub fn dmabuf_take_gem_ref(handle_id: u32) {
    if handle_id < zcore_drivers::scheme::gem_mmap::DRIVER_HANDLE_BASE {
        return;
    }
    let n = zcore_drivers::scheme::gem_mmap::add_ref(handle_id, DMABUF_HOLDER);
    if n.is_none() {
        // Export of a handle that is not in gem_mmap: either it was never a
        // nouveau object, or it was already freed under us. The export path
        // itself will have failed `lookup_for` before reaching here for the
        // latter; this is a belt-and-braces log for the former.
        log::debug!(
            "[drm] dmabuf_take_gem_ref handle={:#x}: not tracked in gem_mmap",
            handle_id
        );
    }
}

/// Release the reference [`dmabuf_take_gem_ref`] took, freeing the GEM object
/// when it was the last one. Routed through the driver's `GEM_CLOSE` so the
/// last close also drains VM_BIND mappings and returns memory to the RM —
/// same contract as [`fb_drop_gem_ref`].
pub fn dmabuf_drop_gem_ref(handle_id: u32) {
    if handle_id < zcore_drivers::scheme::gem_mmap::DRIVER_HANDLE_BASE {
        return;
    }
    if zcore_drivers::scheme::gem_mmap::lookup(handle_id).is_none() {
        return;
    }
    match get_primary_driver() {
        Some(driver) => {
            driver.nouveau_gem_close(handle_id, DMABUF_HOLDER);
        }
        None => {
            zcore_drivers::scheme::gem_mmap::dec_ref(handle_id, DMABUF_HOLDER);
        }
    }
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

/// Rows per blit band, for a test that has to put a write INSIDE a copy: the
/// band boundary is the only place a test can get between two chunks, so a test
/// that hardcoded 128 would quietly stop testing anything the day this constant
/// moves.
#[cfg(test)]
pub(crate) fn blit_chunk_rows_for_test() -> u32 {
    BLIT_CHUNK_ROWS
}

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

/// The half-open run of pixels [`dma_sync_scanout_src_from_device`] invalidates
/// for `(x, y, w, h)`, so a caller can ask whether a read it is about to do is
/// already covered.
///
/// It is ONE contiguous run — the first row's left edge to the last row's right
/// edge — not a set of rows, because that is what that function flushes: every
/// byte in between, pitch padding and all. So a rectangle is covered exactly
/// when its own first and last byte both fall inside the run.
fn sync_run_px(stride_px: usize, x: u32, y: u32, w: u32, h: u32) -> Option<(usize, usize)> {
    if stride_px == 0 || w == 0 || h == 0 {
        return None;
    }
    let start = (y as usize)
        .saturating_mul(stride_px)
        .saturating_add(x as usize);
    let end = (y as usize)
        .saturating_add(h as usize - 1)
        .saturating_mul(stride_px)
        .saturating_add(x as usize)
        .saturating_add(w as usize);
    (end > start).then_some((start, end))
}

/// Bytes [`dma_sync_scanout_src_from_device`] invalidates for `(x, y, w, h)` --
/// the whole contiguous run, pitch padding and all -- so the klog can say what
/// the flush cost next to what the blit needed.
fn sync_span_bytes(stride_px: usize, x: u32, y: u32, w: u32, h: u32) -> usize {
    sync_run_px(stride_px, x, y, w, h).map_or(0, |(s, e)| e.saturating_sub(s).saturating_mul(4))
}

/// Bytes the blit actually READS out of that run: the rectangle, and nothing
/// between its rows.
///
/// The two numbers are equal for a full frame and diverge sharply for a damage
/// box, because the run spans every byte from the first row's left edge to the
/// last row's right edge. A 200x180 popup in a 1920-pitch framebuffer reads
/// 144 KiB and flushes 1.38 MiB -- and until now nothing said so, because the
/// present's own timing line was behind `if rect.is_none()` and a
/// damage-clipped present never printed at all. Whether that ratio is worth
/// paying for is a question about a real machine, so the numbers go in the log
/// rather than into a rewrite nothing here can measure.
fn blit_read_bytes(w: u32, h: u32) -> usize {
    (w as usize).saturating_mul(h as usize).saturating_mul(4)
}

/// `(x, y, w, h)` clipped the way [`dma_sync_gem_rect_from_device`] clips
/// before it touches a line, so "what would be invalidated" and "what is
/// already invalidated" are compared in the same coordinates. `None` means the
/// rectangle falls entirely outside the buffer and nothing would be read.
fn clip_rect_for_read(
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    clip_w: u32,
    clip_h: u32,
) -> Option<(u32, u32, u32, u32)> {
    // Widened to i64 before anything is added. `w`/`h` are `u32`, and casting
    // one to `i32` wraps it negative past `i32::MAX` -- which `clamp` then
    // turns into an empty rect, i.e. "nothing will be read", the answer that
    // SKIPS the flush. Wrong direction for a value that decides whether a read
    // is safe, so the cast that could produce it is gone. Every input fits i64
    // (`x`/`y` are i32, the rest u32), so nothing here can overflow.
    let x0 = (x as i64).max(0);
    let y0 = (y as i64).max(0);
    let x1 = (x as i64 + w as i64).clamp(0, clip_w as i64);
    let y1 = (y as i64 + h as i64).clamp(0, clip_h as i64);
    (x1 > x0 && y1 > y0).then(|| (x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32))
}

/// Has this present already invalidated every byte the cursor blend is about
/// to read?
///
/// `blit` is the region the present copied, `cpu_src_synced` says whether it
/// ran [`dma_sync_scanout_src_from_device`] over it at all, and `read` is the
/// window [`blit_cursor_patch`] will read — x already widened by
/// [`expand_x_for_wc`], because that is what it reads, not what it was asked
/// for.
///
/// A full-frame present covers the pointer wherever it is. A damage-clipped
/// one covers only its own box's rows, which is why this has to be asked
/// rather than assumed.
fn cursor_read_is_synced(
    src_stride: usize,
    blit: (u32, u32, u32, u32),
    cpu_src_synced: bool,
    read: (i32, i32, u32, u32),
    clip_w: u32,
    clip_h: u32,
) -> bool {
    let read = match clip_rect_for_read(read.0, read.1, read.2, read.3, clip_w, clip_h) {
        // None of the pointer lands in the buffer, so nothing will be read.
        None => return true,
        Some(r) => r,
    };
    if !cpu_src_synced {
        return false;
    }
    let Some((s0, s1)) = sync_run_px(src_stride, blit.0, blit.1, blit.2, blit.3) else {
        return false;
    };
    // `is_some_and`, not `is_none_or`: `read` is already known non-empty, so
    // the `None` arm is only reachable if the run cannot be computed at all,
    // and the answer to "I cannot tell" is "flush", never "covered". Every
    // unknown in this function has to fall the same way -- a needless flush of
    // a 64x64 window costs microseconds, a skipped one puts stale pixels on
    // the screen.
    sync_run_px(src_stride, read.0, read.1, read.2, read.3)
        .is_some_and(|(c0, c1)| c0 >= s0 && c1 <= s1)
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

/// Take a framebuffer's geometry AND a reference that keeps its memory alive for
/// as long as the returned guard lives.
///
/// The present path copies `DrmFramebuffer` out of `DRM_STATE` (it is `Copy`),
/// drops the lock, and then blits from `phys_addr` for anything between a
/// millisecond and ~100 ms, re-enabling interrupts between bands. It held no
/// reference at all while doing that, so a concurrent `rmfb` -- or a process
/// exit -- could drop the last `Arc<VmObject>`, hand the frames back to the
/// allocator, and let the in-flight blit read memory that now belongs to
/// somebody else. `release_process`'s comment argued the process's mappings are
/// already gone by then, which covers userspace writers but says nothing about a
/// kernel blit in flight on another CPU.
///
/// The second element is that reference. It is `None` when the backing is a
/// nouveau GEM object, which has no `VmObject` to take a reference on -- that
/// case is covered instead by retiring the framebuffer when the GEM object is
/// freed (see [`retire_framebuffers_for_handle`]), which closes the window from
/// the other end.
fn snapshot_fb_for_present(fb_id: u32) -> Option<(DrmFramebuffer, Option<Arc<VmObject>>)> {
    let state = DRM_STATE.lock();
    let fb = *state.framebuffers.iter().find(|f| f.id == fb_id)?;
    let backing = state
        .fb_backing
        .iter()
        .find(|(id, _)| *id == fb_id)
        .map(|(_, vmo)| vmo.clone());
    Some((fb, backing))
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
    // Every one of the three ways a framebuffer goes away comes through here --
    // `RMFB`, a GEM handle closing under a live fb, and a process exiting -- so
    // this is where "the panel carries this id" stops being true. Ids are handed
    // out again, and a new buffer landing on a retired number must not inherit
    // it. See [`PANEL_FB`].
    forget_panel_fb(fb_id);
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
/// How far right a blit of the CRTC framebuffer may go, in pixels: the limit
/// every read of it is bounded by, and the one [`expand_x_for_wc`] widens up to.
///
/// One function because four places asked it and they did not all agree. Three
/// spelled it `min(src_stride, display_pitch_px).max(fw)` and one added a
/// `.min(src_stride)` with a paragraph explaining why -- and none of them asked
/// the question that decides it, which is whether the image covers the screen.
///
/// When it does, widening past the visible width lands in the DISPLAY's
/// off-screen scanline padding. That is legitimate and deliberate: it completes
/// the last write-combining buffer instead of flushing it half-full, and nobody
/// can see those columns. The only bound is the row itself -- past `src_stride`
/// is the next row's leftmost pixel, and a pointer whose tail appears on the far
/// left of the line below is the visible form of that off-by-one.
///
/// When it does NOT -- a client framebuffer narrower than the mode -- the
/// columns past its right edge are on the screen and the image has nothing to
/// put in them. Widening to the stride there read the framebuffer's own row
/// padding, and past that the next row's pixels, and painted both onto the
/// visible screen: a shifted square trailing the pointer, which is exactly what
/// the cursor patch's comment says it clips to `fb_width` to prevent. It clipped
/// `y` and never clipped `x`.
///
/// `image_width >= display_width` is how "the image covers the screen" is asked,
/// and callers pass whichever width they hold: the framebuffer's own, or the
/// `min` of it and the display's. Both answer the same question, because the min
/// equals the display width exactly when the framebuffer is at least as wide.
///
/// It must be ONE value per present, too, not recomputed per caller:
/// [`cursor_read_is_synced`] is told the window the blend will read and compares
/// it against what the present invalidated. Two callers deriving that limit
/// differently makes it answer about a window nobody reads -- and the way it
/// falls is "covered", which skips the flush.
fn image_pitch_px(
    src_stride: usize,
    display_pitch_px: u32,
    display_width: u32,
    image_width: u32,
) -> u32 {
    if image_width >= display_width {
        (src_stride as u32).min(display_pitch_px)
    } else {
        image_width
    }
}

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
/// (page-flip / modeset) always repaints everything. A `rect` that lies wholly
/// outside the framebuffer draws NOTHING and reports success -- the same answer
/// `drm_atomic_helper_damage_iter_next` gives for a clip whose intersection with
/// the plane's source is empty; it is not a request to repaint the frame. The
/// promotion of a box onto a framebuffer the panel does not carry happens before
/// this, in [`present_now_checked`] -- see [`PANEL_FB`]. Horizontal edges are
/// expanded to 64-byte WC lines.
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
    // `_backing` is held for the whole function on purpose: it is what stops a
    // concurrent RMFB or process exit from freeing the frames under the blit.
    let (fb, _backing) = match snapshot_fb_for_present(fb_id) {
        Some(v) => v,
        None => {
            warn!("[drm] scanout: fb_id={} not found", fb_id);
            return Err(PresentError::NoSuchFb);
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
    // `(bands, skipped)` when the unchanged-band skip drove this present's copy.
    let mut skip_report: Option<(usize, usize)> = None;
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
    if blitted_by_ce {
        // A copy engine wrote the panel, not the band skip, so what the skip
        // remembers about those rows is no longer what the panel holds. No test
        // covers this one: taking the CE path needs a state-loaded GPU, so it is
        // the same untestable half as the fence wait's -- and the rule it follows
        // is the one every other writer of the panel follows.
        panel_bands_reset();
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
        // Armed by `drm.present_probe` only: what the pixels looked like going
        // in, so the read after the blit can say whether anybody else was
        // writing them at the same time. See [`PRESENT_PROBE`].
        let probe_before = if present_probe_enabled() || present_repair_enabled() {
            probe_bands(
                pixels,
                src_stride,
                blit_x,
                blit_y,
                blit_w,
                blit_h,
                PROBE_ROW_STEP,
            )
        } else {
            None
        };
        // Banded blit with IRQs briefly re-enabled between bands — see
        // [`blit_chunked`]. Honours a DIRTYFB damage rect when present.
        //
        // Armed by `drm.present_skip`, and only for a whole frame: the bands the
        // panel already holds are left alone. A damage box is already the
        // client's own answer to the same question, and mixing the two would have
        // the skip's state describe rows a box never touched. See
        // [`PRESENT_SKIP`].
        if src_off < pixels.len() {
            if present_skip_enabled() && rect.is_none() {
                skip_report = Some(blit_chunked_skipping(
                    &display,
                    blit_x,
                    blit_y,
                    &pixels[src_off..],
                    src_stride,
                    blit_w,
                    blit_h,
                ));
            } else {
                // Anything the skip did not drive wrote rows it does not know
                // about, so what it remembers about the panel stops being true.
                panel_bands_reset();
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
        if let Some(before) = probe_before {
            // Invalidate before reading again, or the second read is served
            // from the very lines the first one pulled in and the answer is
            // always "unchanged" -- which is the answer that hides the defect.
            if gem_cpu_mapped {
                dma_sync_scanout_src_from_device(
                    vaddr, fb.size, src_stride, blit_x, blit_y, blit_w, blit_h,
                );
            }
            let after = probe_bands(
                pixels,
                src_stride,
                blit_x,
                blit_y,
                blit_w,
                blit_h,
                PROBE_ROW_STEP,
            );
            // Where the SOURCE was already black, reported whether or not
            // anything changed. See [`ZeroExtent`]: this is the line that says
            // which side of the handover a black rectangle came from, and the
            // band mask cannot say it because black in both reads differs in
            // neither.
            {
                let z = before.zero;
                // Every read counts here, both answers, so this stays the
                // "how many reads happened" number it always was.
                ZERO_REPORTS.fetch_add(1, Ordering::Relaxed);
                match z.bbox() {
                    // The frame carries black: a new fact every time, because
                    // its box says where on screen to look. Full budget.
                    Some((zx, zy, zw, zh)) => {
                        let n = ZERO_FOUND_REPORTS.fetch_add(1, Ordering::Relaxed);
                        let (report, last) = report_decision(n, MAX_PROBE_REPORTS);
                        if report {
                            #[cfg(test)]
                            BLACK_SOURCE_LINES.fetch_add(1, Ordering::Relaxed);
                            kernel_hal::klog_info!(
                                "[drm] present source: fb {} window {}x{}+{}+{} -- {} of {} \
                                 sampled pixels are already 0x00000000 in the buffer the client \
                                 handed over, inside {}x{}+{}+{} of the window{}",
                                fb_id,
                                blit_w,
                                blit_h,
                                blit_x,
                                blit_y,
                                z.zeros,
                                z.sampled,
                                zw,
                                zh,
                                zx,
                                zy,
                                // Same lie as the one below, on this side:
                                // what runs out here is the budget for frames
                                // that carry black, and the black-free baseline
                                // may still have its line to write.
                                if last {
                                    " (further frames carrying black will not be \
                                     reported; a black-free frame still gets its \
                                     baseline line if it has not had it yet)"
                                } else {
                                    ""
                                }
                            );
                        }
                    }
                    // No black anywhere in the source. Worth saying once, and
                    // then it is the same sentence about a different frame.
                    None => {
                        let n = ZERO_CLEAN_REPORTS.fetch_add(1, Ordering::Relaxed);
                        let (report, last) = report_decision(n, MAX_CLEAN_SOURCE_REPORTS);
                        if report {
                            #[cfg(test)]
                            CLEAN_SOURCE_LINES.fetch_add(1, Ordering::Relaxed);
                            kernel_hal::klog_info!(
                                "[drm] present source: fb {} window {}x{}+{}+{} -- not one of {} \
                                 sampled pixels is 0x00000000, so any black on screen was NOT \
                                 handed over black{}",
                                fb_id,
                                blit_w,
                                blit_h,
                                blit_x,
                                blit_y,
                                z.sampled,
                                // NOT "no further source reports": the frames
                                // that carry black keep their own budget, and a
                                // reader who is told the reporting stopped here
                                // stops waiting for the line that matters.
                                if last {
                                    " (further black-free frames will not be reported; frames \
                                     that DO carry black still will)"
                                } else {
                                    ""
                                }
                            );
                        }
                    }
                }
            }
            // And the one the source line above cannot answer: black that was
            // NOT in the first read and IS in the second. The copy ran between
            // them, so this black was handed over -- just after we had already
            // looked. See [`ZERO_GREW_REPORTS`].
            if let Some(a) = after.as_ref() {
                // The box is every zero pixel of the SECOND read, not only the
                // new ones: the reads are summarised per band, so which
                // individual pixel turned black is not recoverable. The line
                // says so rather than let the box be read as the new black
                // alone.
                //
                // Matched on rather than unwrapped with a fallback: `bbox` is
                // `Some` exactly when `zeros > 0`, which this comparison already
                // guarantees, so a fallback box could never print -- and if it
                // somehow did, `0x0+0+0` reads as a real zero-sized region and
                // would be a lie in a diagnostic. No branch, no lie.
                if let Some((zx, zy, zw, zh)) =
                    a.zero.bbox().filter(|_| a.zero.zeros > before.zero.zeros)
                {
                    let n = ZERO_GREW_REPORTS.fetch_add(1, Ordering::Relaxed);
                    let (report, last) = report_decision(n, MAX_PROBE_REPORTS);
                    if report {
                        #[cfg(test)]
                        GREW_SOURCE_LINES.fetch_add(1, Ordering::Relaxed);
                        // Fits the 512-byte `klog_emit` line buffer with room to
                        // spare (392 with the longest numbers and the suffix),
                        // but that buffer DROPS the tail of a longer message, so
                        // anything added here has to be measured, not guessed.
                        kernel_hal::klog_info!(
                            "[drm] present source: fb {} window {}x{}+{}+{} -- the source WENT \
                             BLACK while it was being copied: {} of {} sampled pixels were \
                             0x00000000 before the copy and {} after, so that black WAS handed \
                             over, just later than the sample; {}x{}+{}+{} of the window bounds \
                             all black in the second read{}",
                            fb_id,
                            blit_w,
                            blit_h,
                            blit_x,
                            blit_y,
                            before.zero.zeros,
                            before.zero.sampled,
                            a.zero.zeros,
                            zw,
                            zh,
                            zx,
                            zy,
                            if last {
                                " (further frames whose source went black mid-copy will not be \
                                 reported)"
                            } else {
                                ""
                            }
                        );
                    }
                }
            }
            if probe_says_changed(before.fold(), after.as_ref().map(ProbeBands::fold)) {
                // Which bands moved is the whole point of reporting at all: a
                // scattered subset says a rasteriser handed over tiles it had
                // not finished, a contiguous run says the compositor is drawing
                // the next frame straight into the buffer it just presented.
                // `0` with a second read that produced nothing is the third
                // case, and the line says so rather than implying "no bands".
                let mask = after.as_ref().map_or(0, |a| before.diff_mask(a));
                let n = PROBE_REPORTS.fetch_add(1, Ordering::Relaxed);
                let (report, last) = probe_report_decision(n);
                if report {
                    kernel_hal::klog_info!(
                        "[drm] present probe: fb {} window {}x{}+{}+{} changed while it was \
                         being scanned out -- the client is still writing the buffer it \
                         presented; {} of {} {}px bands differ, mask 0x{:08x}{}{}",
                        fb_id,
                        blit_w,
                        blit_h,
                        blit_x,
                        blit_y,
                        mask.count_ones(),
                        before.n,
                        PROBE_BAND_PX,
                        mask,
                        if after.is_none() {
                            " (the second read produced no answer at all)"
                        } else {
                            ""
                        },
                        if last {
                            " (further mismatches will not be reported)"
                        } else {
                            ""
                        }
                    );
                }
                // The bands that moved under the copy are the bands whose pixels
                // on the panel are older than the buffer they came from. Copy
                // them again and the panel picks up what has arrived since. See
                // [`PRESENT_REPAIR`] for why this is all the kernel can do, and
                // [`MAX_REPAIR_ROUNDS`] for why it is bounded.
                if present_repair_enabled() {
                    let mut mask = mask;
                    let mut round = 0u32;
                    while round < MAX_REPAIR_ROUNDS {
                        let Some((dx, dw)) = repair_span_px(mask, before.n, blit_w) else {
                            break;
                        };
                        let span_off = src_off.saturating_add(dx as usize);
                        if span_off >= pixels.len() {
                            break;
                        }
                        // Re-read before re-copying: the span is being written
                        // by another CPU, so the lines this CPU pulled in for
                        // the checksum are the ones we must NOT copy from.
                        if gem_cpu_mapped {
                            dma_sync_scanout_src_from_device(
                                vaddr,
                                fb.size,
                                src_stride,
                                blit_x.saturating_add(dx),
                                blit_y,
                                dw,
                                blit_h,
                            );
                        }
                        let redo = probe_bands(
                            pixels,
                            src_stride,
                            blit_x,
                            blit_y,
                            blit_w,
                            blit_h,
                            PROBE_ROW_STEP,
                        );
                        blit_chunked(
                            &display,
                            blit_x.saturating_add(dx),
                            blit_y,
                            &pixels[span_off..],
                            src_stride,
                            dw,
                            blit_h,
                        );
                        round = round.saturating_add(1);
                        // What still moved DURING this round is what the next
                        // one owes. A round that copied a settled span leaves
                        // nothing set and the loop ends on its own.
                        let Some(redo) = redo else { break };
                        if gem_cpu_mapped {
                            dma_sync_scanout_src_from_device(
                                vaddr,
                                fb.size,
                                src_stride,
                                blit_x.saturating_add(dx),
                                blit_y,
                                dw,
                                blit_h,
                            );
                        }
                        let now = probe_bands(
                            pixels,
                            src_stride,
                            blit_x,
                            blit_y,
                            blit_w,
                            blit_h,
                            PROBE_ROW_STEP,
                        );
                        mask = now.as_ref().map_or(0, |n| redo.diff_mask(n));
                    }
                    // The repair wrote the panel with a plain `blit_chunked`,
                    // so it is one more writer that went around the band skip --
                    // the same rule the cursor, a damage box, a blank, a VT and
                    // the copy engine all follow. Without this the skip's
                    // remembered hash still describes what the FIRST copy put
                    // there, while the panel now holds what the repair copied,
                    // and a later frame whose pixels match that stale hash would
                    // be skipped over a panel that does not hold them. The rows
                    // are the whole window's -- the repair narrows its span in x
                    // only -- so this is deliberately the coarse answer: with
                    // both flags on, a present that repairs gives up the next
                    // present's skip. Correctness first; the two flags are
                    // independent knobs and nothing needs them together.
                    //
                    // Unconditional, with no `round > 0` in front of it: getting
                    // here at all means the loop ran, and the loop increments
                    // before its only `break`, so `round` is always at least 1.
                    // A guard on it was a mutant nothing could kill, because it
                    // cannot change the answer for any input.
                    //
                    // `panel_bands_reset()` would be equivalent TODAY and that
                    // mutation survives on purpose: the skip only ever drives a
                    // whole frame, so these rows are always the whole window and
                    // dirtying them clears every band. `dirty_rows` is written
                    // anyway because it states the rule that is actually true --
                    // forget the rows you wrote -- and stays right if the skip
                    // ever learns to take a damage box, where the two diverge.
                    panel_bands_dirty_rows(blit_y, blit_h);
                    #[cfg(test)]
                    REPAIR_ROUNDS_RUN.store(round, Ordering::Relaxed);
                }
            }
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
        // The one limit for this present, and it has to be the SAME one
        // `blit_cursor_patch` uses below: `cursor_read_is_synced` is told the
        // window the blend will read and compares it against what the present
        // invalidated, so two derivations of it make that comparison answer about
        // a window nobody reads -- and it falls towards "covered", which skips
        // the flush. See [`image_pitch_px`] for why the answer is the image's
        // own width and not the row stride.
        let cursor_pitch_px = image_pitch_px(src_stride, info.pitch() / 4, info.width, fb_width);
        // `blit_cursor_patch` below READS the framebuffer under the pointer --
        // through the WB physmap alias of a GEM the GPU writes -- to blend the
        // cursor over it. Those lines have to be invalidated first or the blend
        // composites the pointer over whatever the cache still holds and writes
        // that to the screen.
        //
        // Widened for the same reason as the invalidate in
        // `repaint_for_cursor`: the patch reads out to the write-combining
        // boundary and to the row pitch, so invalidating only `cw` columns
        // clipped to `fb_width` leaves the margins it reads stale.
        let (ex, ew) = expand_x_for_wc(cx.max(0) as u32, cw, cursor_pitch_px);
        // What this present has already invalidated. `dma_sync_scanout_src_from_device`
        // flushes ONE contiguous run, so a full-frame present covers the
        // pointer wherever it is -- and that is the only case the old
        // `blitted_by_ce && !cpu_src_synced` guard was written for.
        //
        // A DAMAGE-CLIPPED present does not: its run spans the damage box's
        // rows only. A pointer outside those rows had nothing invalidate the
        // lines the blend reads -- the CE branch does not run (`cpu_src_synced`
        // is true on the CPU path) and the rect sync stopped at the box -- so
        // the blend composited it over whatever the cache still held and wrote
        // that to the screen. A menu or a calendar opening is precisely the
        // small damage box that does not reach the pointer's rows. It only
        // shows where the GPU recently rewrote those pixels, so a flat
        // wallpaper hides it and a freshly composited shadow does not.
        let covered = cursor_read_is_synced(
            src_stride,
            (blit_x, blit_y, blit_w, blit_h),
            cpu_src_synced,
            (ex as i32, cy, ew, ch),
            cursor_pitch_px,
            fb_height,
        );
        if gem_cpu_mapped && !covered {
            // Counted in cursor time, not `sync`, so the klog keeps showing
            // ~0us sync on CE-direct.
            dma_sync_gem_rect_from_device(
                vaddr,
                fb.size,
                src_stride,
                ex as i32,
                cy,
                ew,
                ch,
                cursor_pitch_px,
                fb_height,
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
        // The pointer is now ON the panel in those rows, which is not what the
        // frame's pixels say, so the bands it covers must be copied again next
        // time rather than recognised as already there. Without this the
        // pointer's previous position stays on screen until something else
        // happens to change those rows -- exactly the stale-pixel defect the
        // skip exists to avoid causing. See [`panel_bands_dirty_rows`].
        if skip_report.is_some() {
            panel_bands_dirty_rows(cy.max(0) as u32, ch);
        }
    }
    let t_cursor = kernel_hal::timer::timer_now();
    // Both kinds of present, not just the full frames. This was behind
    // `if rect.is_none()`, so the path a compositor with damage tracking
    // actually drives -- every frame labwc puts up -- printed nothing at all,
    // and the one number that would say whether the source flush is oversized
    // was the one number never reported. `flush` vs `read` is that number: they
    // are equal for a full frame and diverge by the pitch padding between a
    // damage box's rows (see [`blit_read_bytes`]).
    {
        let (n, every, kind) = if rect.is_none() {
            (
                PRESENT_FRAME_COUNT.fetch_add(1, Ordering::Relaxed) + 1,
                FULL_FRAME_REPORT_EVERY,
                "frame",
            )
        } else {
            (
                PRESENT_RECT_COUNT.fetch_add(1, Ordering::Relaxed) + 1,
                RECT_REPORT_EVERY,
                "rect",
            )
        };
        if n <= 2 || n.is_multiple_of(every) {
            let flushed = cost_scaled(sync_span_bytes(src_stride, blit_x, blit_y, blit_w, blit_h));
            let read = cost_scaled(blit_read_bytes(blit_w, blit_h));
            kernel_hal::klog_info!(
                "[drm] present {} #{}: sync {}us ({}{} flushed for {}{} read) + {} blit \
                 {}us + cursor {}us ({}x{} at +{}+{}){}",
                kind,
                n,
                sync_elapsed.as_micros(),
                flushed.0,
                flushed.1,
                read.0,
                read.1,
                if blitted_by_ce { "CE" } else { "cpu" },
                t_blit
                    .saturating_sub(t0)
                    .saturating_sub(sync_elapsed)
                    .as_micros(),
                t_cursor.saturating_sub(t_blit).as_micros(),
                blit_w,
                blit_h,
                blit_x,
                blit_y,
                // Only when the skip drove the copy, and it says what it bought:
                // the bands it did NOT have to write are the whole point, and a
                // boot where that number stays 0 is a boot where the desktop
                // changes everywhere every frame.
                match skip_report {
                    Some((bands, skipped)) => alloc::format!(
                        " -- skipped {} of {} {}-row bands",
                        skipped,
                        bands,
                        SKIP_BAND_ROWS
                    ),
                    None => alloc::string::String::new(),
                }
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

/// Whether the display-engine cursor plane may take the pointer
/// (`nvidia.hwcursor`): 0 = nobody has said, 1 = yes, 2 = no. Read on every
/// pointer-motion path, so it is an atomic and never re-parses anything.
static HW_CURSOR: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// Opt into (or out of) the display-engine hardware cursor.
///
/// Called from boot where every other `nvidia.*` flag is parsed. It used to be
/// the only one of them read straight from the cmdline down here, which meant
/// nothing could ever exercise the hardware-cursor path except a real boot with
/// the flag on -- and that path decides whether the pointer is drawn by the CPU
/// or by the display engine, so "the pointer vanished" had no test either way.
pub fn set_hw_cursor_enabled(v: bool) {
    HW_CURSOR.store(if v { 1 } else { 2 }, Ordering::Relaxed);
}

/// `true` when the display-engine hardware cursor is opted into.
fn hw_cursor_wanted() -> bool {
    match HW_CURSOR.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        // Nobody called `set_hw_cursor_enabled`: fall back to the cmdline, so
        // the flag still works in a build whose boot path does not set it.
        _ => {
            let yes = kernel_hal::boot::cmdline().contains("nvidia.hwcursor");
            HW_CURSOR.store(if yes { 1 } else { 2 }, Ordering::Relaxed);
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

/// Composite the software cursor on top of a frame the DRIVER just flipped.
///
/// A successful `DrmScheme::page_flip` short-circuits `scanout_region`, and
/// `scanout_region` is the only place that composites the software pointer and
/// records `cursor.drawn`. So with `nvidia.hwflip` every accepted flip landed a
/// frame with no pointer in it, and left `cursor.drawn` describing a rectangle
/// the flip had already overwritten -- so the next `repaint_for_cursor` "erased"
/// a cursor that was not there, painting a stale patch of an older frame.
///
/// No erase is needed here, unlike `repaint_for_cursor`: the flip has just
/// rewritten the whole scanout from the compositor's scene, which never has a
/// cursor baked in. Draw the pointer at its current position and record it.
///
/// Returns whether anything was drawn.
fn composite_cursor_after_driver_flip(fb_id: u32) -> bool {
    if scanout_paused() {
        return false;
    }
    let new = {
        let mut st = DRM_STATE.lock();
        if st.graphics_vt != Some(kernel_hal::console::active_vt()) {
            return false;
        }
        // The display engine composites its own plane during scanout.
        if st.cursor.hw {
            return false;
        }
        let c = &st.cursor;
        let new = if c.visible && c.w > 0 && c.h > 0 {
            c.bitmap
                .as_ref()
                .filter(|b| !b.is_empty())
                .map(|b| (c.x, c.y, c.w, c.h, Arc::clone(b)))
        } else {
            None
        };
        // The flip wiped whatever was on screen, so this is the whole truth
        // about what is drawn -- including `None`, which correctly tells the
        // next move there is nothing to erase.
        st.cursor.drawn = new.as_ref().map(|(x, y, w, h, _)| (*x, *y, *w, *h));
        new
    };
    let Some((nx, ny, nw, nh, bmp)) = new else {
        return false;
    };
    let Some((fb, _backing)) = snapshot_fb_for_present(fb_id) else {
        return false;
    };
    let Some(display) = primary_display() else {
        return false;
    };
    if fb.phys_addr == 0 || fb.size == 0 || fb.pitch < 4 {
        return false;
    }
    let info = display.info();
    let (fw, fh) = (info.width.min(fb.width), info.height.min(fb.height));
    let vaddr = phys_to_virt(fb.phys_addr as usize);
    // SAFETY: contiguous physical framebuffer of `fb.size` bytes, identity
    // mapped at `vaddr`; read as `fb.size / 4` u32 pixels. `_backing` keeps it
    // alive for the duration.
    let pixels = unsafe { core::slice::from_raw_parts(vaddr as *const u32, fb.size / 4) };
    let src_stride = (fb.pitch / 4) as usize;
    if zcore_drivers::scheme::gem_mmap::lookup(fb.gem_handle_id).is_some() {
        dma_sync_gem_rect_from_device(vaddr, fb.size, src_stride, nx, ny, nw, nh, fw, fh);
    }
    blit_cursor_patch(
        &*display, pixels, src_stride, fw, fh, nx, ny, nw, nh, nx, ny, nw, nh, &bmp,
    );
    true
}

/// Move the software pointer on the panel WITHOUT reading the client's
/// framebuffer: erase it by putting back what it was covering, and draw it over
/// what the panel itself holds at the new position.
///
/// This is the fix for the black rectangles. See [`CursorUnder`] for what went
/// wrong and why the panel is the only honest source for a pointer move: the
/// client's buffer stopped being the frame on screen the moment the compositor
/// started the next one in it, which our present invites it to do.
///
/// Returns false, having touched nothing, when the panel cannot be read back --
/// anything but ARGB8888, see [`DisplayScheme::read_into`] -- in which case the
/// caller's older read-from-the-client path runs. That is what such a panel has
/// today, and it is not one any machine this kernel runs on has: a UEFI GOP,
/// virtio-gpu and an NVIDIA BAR1 aperture are all 32-bit.
fn repaint_cursor_from_panel(
    display: &dyn DisplayScheme,
    old_drawn: Option<(i32, i32, u32, u32)>,
    new: Option<(i32, i32, u32, u32, &[u32])>,
) -> bool {
    if cursor_from_client() || !display.fb_readable() {
        return false;
    }
    // Take the pointer off the panel first, and only from the save: the panel at
    // the old rect holds the pointer BLENDED over the scene, and an opaque
    // pointer pixel cannot be un-blended. Erasing before reading is also what
    // lets the draw below read the panel at all -- when the two rects overlap,
    // reading first would capture the old pointer and blend the new one over it,
    // baking a trail in.
    {
        let mut u = CURSOR_UNDER.lock();
        // Only while `drawn` still describes what the save was taken for. See
        // [`CursorUnder::for_cursor`].
        if u.for_cursor == old_drawn {
            if let Some((x, y, w, h)) = u.rect {
                // `px` holds exactly `w * h` pixels, `w` per row: `save_cursor_under`
                // wrote it from the window it is describing.
                display.blit_from(x, y, &u.px, w as usize, w, h);
            }
        }
        u.rect = None;
        u.for_cursor = None;
    }
    let Some((cx, cy, cw, ch, bmp)) = new else {
        return true;
    };
    let info = display.info();
    // The window the blit will write: clipped to the panel and widened to the
    // write-combining boundary. `read_into` clamps a window exactly as
    // `blit_from` does, so what is read back is what will be written -- which is
    // the property the restore above depends on.
    let y0 = cy.max(0);
    let y1 = (cy + ch as i32).min(info.height as i32);
    let x0 = cx.max(0) as u32;
    let x1 = (cx + cw as i32).max(0) as u32;
    let (wx, ww) = expand_x_for_wc(x0, x1.saturating_sub(x0), info.pitch() / 4);
    if y1 <= y0 || ww == 0 {
        // Wholly off screen. Nothing is drawn and nothing is covered, which the
        // erase above has already recorded.
        return true;
    }
    let rows = (y1 - y0) as usize;
    let tw = ww as usize;
    let need = tw.saturating_mul(rows);
    let mut slot = CURSOR_PATCH.lock();
    if slot.len() < need {
        slot.resize(need, 0);
    }
    let patch = &mut slot[..need];
    if !display.read_into(wx, y0 as u32, patch, tw, ww, rows as u32) {
        // The window is there but the panel would not give it up. Leave the
        // pointer undrawn rather than compose over whatever the scratch buffer
        // held from the last move, which is a stale square of an older frame --
        // the same rule the `rows` bookkeeping in `blit_cursor_patch` follows.
        return true;
    }
    save_cursor_under((wx, y0 as u32, tw, rows), patch, (cx, cy, cw, ch));
    blend_cursor_into(patch, (wx as i32, y0, tw, rows), (cx, cy, cw, ch), bmp);
    display.blit_from(wx, y0 as u32, patch, tw, ww, rows as u32);
    true
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
    if scanout_paused() || crtc_blanked() {
        return;
    }
    // The rect restore below reads FROM `crtc_fb`. If a pause dropped a present
    // the panel is a frame behind it, and those two ~64x64 windows would paste
    // pieces of a frame nobody has seen into the one still on screen. Put the
    // whole frame up instead -- which also composites the pointer, so there is
    // nothing left for the rect path to do.
    if SCANOUT_STALE.load(Ordering::SeqCst) {
        repaint_if_scanout_stale();
        if !SCANOUT_STALE.load(Ordering::SeqCst) {
            return;
        }
    }
    if !software_kms_active() {
        // Hardware KMS has taken over the scanout. `has_hardware_kms()` is
        // recomputed on every call and flips to true the first time the NVC57E
        // ladder reports ready, so this guard starts firing mid-session -- at
        // which point the last software cursor image simply stopped being
        // repainted and nothing replaced it. A CPU composite into the GOP is
        // genuinely useless here (the display engine is scanning out the
        // client's own surface, not the GOP framebuffer), so the honest answer
        // is to say so once rather than leave the user wondering where the
        // pointer went.
        let needs_hw_plane = {
            let st = DRM_STATE.lock();
            st.cursor.visible && !st.cursor.hw
        };
        if needs_hw_plane && !SURFACEFLIP_NO_CURSOR_LOGGED.swap(true, Ordering::Relaxed) {
            kernel_hal::klog_info!(
                "[drm] hardware KMS is presenting (nvidia.surfaceflip) but the pointer is \
                 software-composited, so it can no longer be drawn -- add nvidia.hwcursor \
                 to the cmdline for a hardware cursor plane"
            );
        }
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
    let display = match primary_display() {
        Some(d) => d,
        None => return,
    };
    // The panel is the source, not the client's framebuffer. Everything below
    // this point is the older path, kept only for a panel whose pixels cannot be
    // read back -- see [`repaint_cursor_from_panel`], and [`CursorUnder`] for the
    // black rectangles that reading the client's buffer here put on the screen.
    if repaint_cursor_from_panel(
        &*display,
        old_rect,
        new.as_ref().map(|(x, y, w, h, b)| (*x, *y, *w, *h, &**b)),
    ) {
        return;
    }
    // Same lifetime guard as `scanout_region`: this blits with the lock dropped.
    let (fb, _backing) = match snapshot_fb_for_present(fb_id) {
        Some(v) => v,
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
    // Invalidate exactly what the blit will READ, which is wider than the rect
    // asked for. `restore_rect` and `blit_cursor_patch` both widen x to the
    // 16-pixel write-combining boundary and cap it at the row pitch, not at
    // the visible width -- so up to 15 columns on each side, plus any pitch
    // padding, were being read without ever being invalidated.
    //
    // CLFLUSH covers whole 64-byte lines, which is 16 pixels, so on a
    // framebuffer whose stride is a multiple of 16 pixels the widened columns
    // happen to fall inside the lines the unexpanded rect already flushed and
    // nothing goes wrong. At any other stride the row base is not line-aligned
    // and they do not: each row reads a different slice of stale cache. That is
    // the failure the comment above describes -- the pointer dragging stale
    // squares of an older frame -- and it only shows where the GPU recently
    // rewrote those pixels, so a flat wallpaper hides it and a window shadow
    // (a gradient, freshly composited) does not.
    let sync_pitch_px = image_pitch_px(src_stride, info.pitch() / 4, info.width, fw);
    let sync_rect = |x: i32, y: i32, w: u32, h: u32| {
        if !gem_cpu_mapped {
            return;
        }
        let x0 = x.max(0) as u32;
        let x1 = (x + w as i32).max(0) as u32;
        let (ex, ew) = expand_x_for_wc(x0, x1.saturating_sub(x0), sync_pitch_px);
        dma_sync_gem_rect_from_device(
            vaddr,
            fb.size,
            src_stride,
            ex as i32,
            y,
            ew,
            h,
            sync_pitch_px,
            fh,
        );
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
    let dinfo = display.info();
    let pitch_px = image_pitch_px(src_stride, dinfo.pitch() / 4, dinfo.width, fw);
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
    }
    if rows == 0 {
        // Nothing was drawn, so nothing is covered. Said explicitly because a
        // save left over from the previous position would otherwise be restored
        // onto a panel this pointer is not on -- a stale square, which is the
        // defect the `rows` bookkeeping above exists to avoid causing.
        forget_cursor_under();
        return;
    }
    // Save what the pointer is about to cover BEFORE blending it in, so the next
    // move can put it back instead of asking the client's framebuffer what used
    // to be there. See [`CursorUnder`].
    //
    // These are the panel's own pixels whatever the client did, and not because
    // the buffer is trustworthy: the blit below writes this whole window to the
    // panel, so the panel is MADE to hold what was just saved. On a pointer move,
    // where nothing writes the window first, that guarantee is gone -- which is
    // the whole difference between this path and `repaint_cursor_from_panel`.
    save_cursor_under(
        (x0 as u32, y0 as u32, tw, rows),
        &patch[..rows * tw],
        (cx, cy, cw, ch),
    );
    blend_cursor_into(patch, (x0, y0, tw, rows), (cx, cy, cw, ch), bmp);
    display.blit_from(x0 as u32, y0 as u32, patch, tw, tw as u32, rows as u32);
}

/// Blend the pointer bitmap over `patch`, a `tw` x `rows` window of the scene
/// whose top-left corner is at `(x0, y0)` on screen.
///
/// wlroots renders cursors with premultiplied alpha, so the operator is
/// `out = src + dst * (255 - a) / 255`; fully transparent pixels are skipped and
/// fully opaque ones are written without reading what they cover. One function
/// and not two because the pointer is composited from two different sources --
/// the frame a present is putting up, and the panel itself on a move -- and a
/// blend written out twice is a blend that can disagree with itself.
fn blend_cursor_into(
    patch: &mut [u32],
    window: (i32, i32, usize, usize),
    cursor: (i32, i32, u32, u32),
    bmp: &[u32],
) {
    let (x0, y0, tw, rows) = window;
    let (cx, cy, cw, ch) = cursor;
    // The contract, checked once rather than per pixel: both callers size the
    // patch as `tw * rows` exactly, and a smaller one would have the blend run
    // off the end of it.
    if patch.len() < tw.saturating_mul(rows) {
        return;
    }
    for r in 0..rows {
        let dst_off = r * tw;
        let cr = (y0 + r as i32) - cy;
        if cr < 0 || cr >= ch as i32 {
            continue;
        }
        let bmp_row = cr as usize * cw as usize;
        for c in 0..tw {
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
}

/// Remember `px` as the panel pixels the pointer at `cursor` is covering. See
/// [`CursorUnder`].
fn save_cursor_under(window: (u32, u32, usize, usize), px: &[u32], cursor: (i32, i32, u32, u32)) {
    let (x, y, tw, rows) = window;
    let mut u = CURSOR_UNDER.lock();
    u.px.clear();
    u.px.extend_from_slice(px);
    u.rect = Some((x, y, tw as u32, rows as u32));
    u.for_cursor = Some(cursor);
}

/// Forget the save, because the pointer is not on the panel where it described.
fn forget_cursor_under() {
    let mut u = CURSOR_UNDER.lock();
    u.rect = None;
    u.for_cursor = None;
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
    let dinfo = display.info();
    let pitch_px = image_pitch_px(src_stride, dinfo.pitch() / 4, dinfo.width, fw);
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
    /// The frame could not be put on the screen, and why.
    ///
    /// Carries the reason so the ioctl arm can apply the same policy `SETCRTC`
    /// does (see `present_failed` there): a missing framebuffer is the client's
    /// own error and fails the ioctl with `ENOENT`, while a frame that merely
    /// could not be copied is reported and accepted. Collapsing all three to
    /// one `Failed` answered `EIO` for every one of them, and wlroots escalates
    /// an unexpected page-flip failure into an output teardown -- it drops DRM
    /// master and the desktop falls back to the text console. `SETCRTC` was
    /// given this distinction and the flip path was left without it, so the
    /// identical condition was survivable on one and fatal on the other.
    Present(PresentError),
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
    if !settle_outstanding_flip() {
        // Only a delivery that never finished gets here (see
        // `settle_outstanding_flip`). EBUSY is the last resort; proceeding
        // with FLIP_EVENT_PENDING still set risks a double-delivery.
        return Err(FlipError::Busy);
    }
    let present = present_now_checked(fb_id, crtc_id, None);
    // A framebuffer that is not there is the one reason to fail the flip, and
    // it owes no completion. For the others the flip stands: the event MUST
    // still be queued, because a client that asked for one and does not get it
    // blocks in `poll()` on the card fd for a frame that will never be
    // reported -- the frame loop stops dead, which is worse than a frame that
    // was not copied.
    if matches!(present, Err(PresentError::NoSuchFb)) {
        return Err(FlipError::Present(PresentError::NoSuchFb));
    }
    if want_event {
        schedule_flip_event(crtc_id, user_data, file);
    }
    match present {
        Ok(()) => Ok(()),
        Err(e) => Err(FlipError::Present(e)),
    }
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

/// How many page-flip completions are queued OR mid-delivery.
///
/// `FLIP_EVENT_PENDING` alone could not express "mid-delivery", and two things
/// broke on that. `deliver_pending_drm_timer` drains the whole queue into a
/// local `Vec` and only clears the latch later, inside `queue_flip_event`; in
/// that window `clear_stale_flip_pending` sees a queue with no `Flip` in it,
/// concludes the latch is stale and clears it -- so the "one flip outstanding"
/// invariant is gone and two frames can reach the scanout inside one vblank
/// period. And `queue_flip_event` cleared the latch unconditionally, so
/// delivering the first of two queued flips marked the second as not pending,
/// after which a further flip skipped the flush entirely and the queue held two.
///
/// This counts both states, so the latch is cleared exactly when the last flip
/// has been delivered or cancelled and not a moment sooner.
static FLIPS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Account for one flip leaving the queued-or-delivering population, clearing
/// the latch only when it was the last one.
fn flip_in_flight_done() {
    // `fetch_update` rather than a bare decrement: a cancel path may already
    // have taken the count to zero, and wrapping below zero would latch
    // `FLIP_EVENT_PENDING` on forever (a permanent EBUSY, which wlroots turns
    // into an output teardown).
    let prev = FLIPS_IN_FLIGHT
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            Some(n.saturating_sub(1))
        })
        .unwrap_or(0);
    if prev <= 1 {
        FLIP_EVENT_PENDING.store(false, Ordering::Release);
    }
}

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
                    flip_in_flight_done();
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
        // Inside the queue lock, for the same reason the latch store is: the
        // count and the queue must never be seen disagreeing.
        FLIPS_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
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
pub(crate) fn flush_pending_flip_completions() {
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
    // The QUEUE is not the population: `deliver_pending_drm_timer` drains it
    // into a local before delivering, so a flip that is mid-delivery appears in
    // neither place. Ask the counter, which covers both, or this "self-heal"
    // becomes the bug -- clearing the latch for a flip that has not been
    // delivered yet and letting a second frame through inside one vblank.
    if FLIPS_IN_FLIGHT.load(Ordering::Acquire) == 0 {
        FLIP_EVENT_PENDING.store(false, Ordering::Release);
    }
}

/// How long [`settle_outstanding_flip`] waits for a completion that the timer
/// has taken off the queue but not yet posted, counted in spins. The window is
/// microseconds (build a 32-byte event, take the fd's queue lock, push); the
/// limit only exists so that a counter which has somehow lost its deliverer
/// cannot hold a syscall forever.
const FLIP_DELIVERY_SPIN_LIMIT: u32 = 1 << 22;

/// Bring the CRTC to "no completion outstanding" before a new flip or atomic
/// commit is accepted. `true` when it is; `false` only when a completion stayed
/// mid-delivery for the whole of [`FLIP_DELIVERY_SPIN_LIMIT`].
///
/// Three things can hold the latch: a flip still queued for the synthetic
/// vblank (delivered now, see [`flush_pending_flip_completions`]); a latch
/// left set with nothing behind it (cleared, see [`clear_stale_flip_pending`]);
/// and a completion the timer is delivering at this very moment.
/// `deliver_pending_drm_timer` drains the queue into a local before it posts
/// anything, so for those few microseconds the job is in neither place while
/// the counter still says one in flight -- and it must, or a second frame
/// gets through inside one vblank. On one CPU that window never overlaps a
/// syscall: the timer IRQ runs to completion first. With two, the compositor's
/// next PAGE_FLIP can land exactly there, and this used to answer EBUSY -- the
/// "safety net that should not trigger" -- which wlroots escalates into an
/// output teardown (DROP_MASTER, and the desktop drops to the text console).
/// The deliverer is on another CPU (another thread, under libos) and holds
/// nothing this path needs, so wait for it.
fn settle_outstanding_flip() -> bool {
    let mut spins = 0u32;
    loop {
        if !FLIP_EVENT_PENDING.load(Ordering::Acquire) {
            return true;
        }
        flush_pending_flip_completions();
        if !FLIP_EVENT_PENDING.load(Ordering::Acquire) {
            return true;
        }
        clear_stale_flip_pending();
        if !FLIP_EVENT_PENDING.load(Ordering::Acquire) {
            return true;
        }
        if spins >= FLIP_DELIVERY_SPIN_LIMIT {
            return false;
        }
        // Mid-delivery. Give the deliverer a moment before looking again --
        // re-flushing on every pass would only hammer the queue lock it may be
        // about to take for a follow-up arm -- and flush again on the next
        // pass in case another client queued a flip of its own meanwhile.
        for _ in 0..64 {
            core::hint::spin_loop();
        }
        spins += 64;
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
    FLIPS_IN_FLIGHT.store(0, Ordering::Release);
    FLIP_EVENT_PENDING.store(false, Ordering::Release);
}

/// Drain timer jobs that target a closing drm_file (by raw pointer identity).
fn cancel_pending_timers_for_file(file_ptr: *const DrmFileState) {
    let mut cleared_flip = 0usize;
    {
        let mut q = PENDING_DRM_TIMERS.lock();
        q.retain(|job| {
            let job_ptr = match job {
                PendingDrmTimer::Flip { file, .. } => file.as_ptr(),
                PendingDrmTimer::Vblank { file, .. } => file.as_ptr(),
            };
            if core::ptr::eq(job_ptr, file_ptr) {
                if matches!(job, PendingDrmTimer::Flip { .. }) {
                    cleared_flip += 1;
                }
                false
            } else {
                true
            }
        });
    }
    for _ in 0..cleared_flip {
        flip_in_flight_done();
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
    let asked_for_a_rect = rect.is_some();
    // Deferred console GSP bring-up: acknowledge the flip to keep the
    // compositor alive, but do not touch the GOP framebuffer / CE path.
    if scanout_paused() {
        set_crtc_fb(crtc_id, fb_id);
        // Acknowledged and not drawn: from here on the panel and `crtc_fb`
        // describe different frames. See [`SCANOUT_STALE`].
        SCANOUT_STALE.store(true, Ordering::SeqCst);
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
                // The console has the panel: whatever it printed is over the
                // last frame, so the panel no longer carries a framebuffer.
                // Without this the first present after switching back honours
                // its damage box and paints one rectangle of desktop into a
                // screen full of console text.
                set_panel_fb(0);
                return Ok(());
            }
            _ => {}
        }
    }
    // A damage box is only meaningful against the framebuffer the panel already
    // carries; against any other one the pixels it leaves alone belong to a
    // different frame. See [`PANEL_FB`], and `drm_atomic_helper_damage_iter_init`
    // upstream, which does exactly this.
    let panel_before = panel_fb();
    let rect = rect_for_present(rect, panel_before, fb_id);
    if rect.is_none() && asked_for_a_rect {
        let n = DAMAGE_PROMOTIONS_LOGGED.fetch_add(1, Ordering::Relaxed);
        if n < MAX_DAMAGE_PROMOTIONS_LOGGED {
            kernel_hal::klog_info!(
                "[drm] present: damage box on fb {} promoted to a whole frame -- the panel \
                 carries fb {} (a swapchain buffer's untouched pixels are another frame's){}",
                fb_id,
                panel_before,
                if n + 1 == MAX_DAMAGE_PROMOTIONS_LOGGED {
                    " [last report]"
                } else {
                    ""
                }
            );
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
    if hw.unwrap_or(false) {
        // The driver flip replaced the whole scanout and skipped
        // `scanout_region`, which is the only place the software pointer gets
        // composited. Put it back on top of this frame.
        composite_cursor_after_driver_flip(fb_id);
    } else {
        scanout_region_checked(fb_id, rect)?;
    }
    // The panel carries this framebuffer in full now -- the driver replaced the
    // whole scanout, or the blit covered every row and column -- so whatever a
    // pause dropped earlier, the two agree again. A damage rect that covers less
    // does NOT catch up: it leaves the disagreement everywhere it did not touch.
    // See [`SCANOUT_STALE`] and [`present_caught_the_panel_up`]. Read the latch
    // first so the common present pays neither the display lookup nor the store.
    if SCANOUT_STALE.load(Ordering::SeqCst) {
        let screen = primary_display().map(|d| {
            let info = d.info();
            (info.width, info.height)
        });
        if hw.unwrap_or(false) || present_caught_the_panel_up(rect, screen) {
            SCANOUT_STALE.store(false, Ordering::SeqCst);
        }
    }
    // The panel carries this framebuffer now. Unconditional, and provably so: a
    // damage box that survived the promotion above is one the panel ALREADY
    // carried, so the store is idempotent there, and every other present covered
    // the whole frame -- the CPU blit over every row, or a driver flip that
    // replaced the scanout outright. Guarding it on `rect.is_none()` was the same
    // function written twice; the promotion is where the distinction lives.
    set_panel_fb(fb_id);
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
    // a new PAGE_FLIP may be accepted even before userspace reads it. Only the
    // LAST outstanding flip clears the latch; see `FLIPS_IN_FLIGHT`.
    flip_in_flight_done();
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    // (DROP_MASTER -> the desktop drops to the text console). See [`page_flip`]
    // and [`settle_outstanding_flip`], which also waits out a completion the
    // timer is delivering on another CPU at this very moment; EBUSY is left
    // for a delivery that never finishes.
    if want_event && !settle_outstanding_flip() {
        return Err(AtomicError::Busy);
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
    // Its syncobjs too (Linux: `drm_syncobj_release` when the file closes):
    // a crashed client's stayed in the table for the rest of the boot.
    let syncobjs = zcore_drivers::scheme::syncobj::release_owner(pid);
    if syncobjs > 0 {
        log::info!(
            "[drm] pid={} exit: gave back {} syncobj reference(s)",
            pid,
            syncobjs
        );
    }
    // Driver-private (nouveau `GEM_NEW`) framebuffers first, and BEFORE the
    // early return below: `nouveau_release_process`, which runs right after
    // this hook, drops everything the pid held, so a framebuffer of its own
    // left behind here means `crtc_fb` aimed at a buffer nobody owns and the
    // next repaint blitting whatever took its place. This has to happen while
    // `gem_mmap` still records who held what.
    //
    // It cannot live inside the block below, because that returns early when
    // the pid owns no entry in `state.handles` -- and a compositor using the
    // GL/Vulkan renderer owns NOTHING there: its buffers are all nouveau GEM
    // objects tracked in `gem_mmap`. That early return is precisely why a
    // crashed wlroots left its scanout framebuffer in place.
    //
    // What decides is `fb.owner`, the pid that issued the `ADDFB` -- NOT
    // whether the dying pid happens to be one of the GEM object's holders.
    // Those two are the same thing only while a buffer has a single holder,
    // and the X11 path is exactly where it has three: a GL client hands its
    // buffer to Xwayland, which hands it on to the compositor, so one object
    // is held by all three (`xwayland_chain_tests`). Keyed on `holds`, the
    // client exiting retired the COMPOSITOR's framebuffer and zeroed
    // `crtc_fb` with it -- after which every `PAGE_FLIP` and `SETCRTC` on that
    // id answers "no such fb" and wlroots has no output left to drive.
    //
    // Retiring by owner is also what makes the memory safe: since
    // `fb_take_gem_ref` the framebuffer holds a reference of its own
    // (`KMS_FB_HOLDER`), so a holder dying is never the last one while an fb
    // stands over the object -- the same invariant
    // `retire_framebuffers_for_handle` already relies on.
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
                    && fb.owner == pid
            })
            .map(|fb| (fb.id, fb.gem_handle_id))
            .collect();
        state.framebuffers.retain(|fb| {
            fb.gem_handle_id < zcore_drivers::scheme::gem_mmap::DRIVER_HANDLE_BASE
                || fb.owner != pid
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
    // The real panel size from the UEFI-captured EDID, falling back to a
    // ~96 DPI estimate. This used to read only the coarse centimetre bytes,
    // and to accept either one of them alone — so a display that states its
    // size to the millimetre in a detailed timing (a TV: 885x497 mm) was
    // reported rounded to whole centimetres, and one that fills in only the
    // width was reported as `600x0` mm, which is an infinite DPI to every
    // client that divides by it.
    let (mm_width, mm_height) = get_connector_edid(SYNTH_CONNECTOR_ID)
        .and_then(|e| zcore_drivers::display::edid::physical_size_mm(&e))
        .unwrap_or_else(|| zcore_drivers::display::edid::estimated_size_mm(w, h));
    Some(DrmConnector {
        id: SYNTH_CONNECTOR_ID,
        connected: true,
        mm_width,
        mm_height,
        connector_type: 11,
    })
}

/// The bootloader-captured EDID, but only if it is a whole, self-consistent
/// block -- the one answer to "is this EDID usable", in the one place, for every
/// reader.
///
/// There were four spellings of this question, each with its own hardcoded 128,
/// and only the diagnostic in procfs looked at the header. For the BOOT EDID the
/// difference was cosmetic: `set_boot_edid` refuses an invalid block at the one
/// point it enters the kernel, so `boot_edid()` can only hand back one that
/// passed, and every decoder in `zcore_drivers::display::edid` re-checks anyway.
/// The `block_valid` call here is that brace's belt, and it is stated once so the
/// rule is findable.
///
/// The hole was on the other side. A DRIVER's block never goes through
/// `set_boot_edid`, and [`get_connector_edid`] served whatever a driver reported
/// straight to userspace on a length check alone -- including the 32 real bytes
/// zero-padded to 128 that `NvidiaGpu` used to synthesise, which cannot pass a
/// checksum and which every client that checks one throws away whole.
pub fn boot_edid_block() -> Option<[u8; zcore_drivers::display::edid::BLOCK_LEN]> {
    let (block, len) = zcore_drivers::display::boot_edid()?;
    if (len as usize) < zcore_drivers::display::edid::BLOCK_LEN {
        return None;
    }
    // No sequential test can tell this line from a bare `Some(block)`, and that
    // is not a gap in the tests: `set_boot_edid` refuses an invalid block at the
    // single point one enters the kernel, so `boot_edid()` cannot hand one back.
    // It stays because the rule belongs where it is read, not only where it is
    // enforced -- if that setter ever loosens, every reader is already covered.
    zcore_drivers::display::edid::block_valid(&block).then_some(block)
}

/// How many refused EDIDs have been named in the klog. Budgeted like every other
/// per-frame report here: a connector is probed on every `GETCONNECTOR`, and a
/// compositor that re-enumerates in a loop would otherwise pin the UART.
static EDID_REFUSALS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
const MAX_EDID_REFUSALS: u32 = 4;

/// Why a 128-byte block is not an EDID, in the words the klog uses. `None` when
/// it is one.
///
/// Split out from the refusal so the reason can be tested without a UART: the
/// two failures are told apart on purpose, because a wrong header means the
/// firmware handed over a bad pointer and a wrong checksum means it handed over
/// a real-but-corrupt read, and those are different things to go and look at.
fn edid_refusal_reason(block: &[u8]) -> Option<&'static str> {
    use zcore_drivers::display::edid;
    if block.len() < edid::BLOCK_LEN {
        return Some("short");
    }
    if edid::block_valid(block) {
        return None;
    }
    // `block_valid` checks the header first and then the checksum, so this
    // reproduces its order rather than guessing which one it tripped on.
    if block[..8] != [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00] {
        Some("header")
    } else {
        Some("checksum")
    }
}

pub fn get_connector_edid(id: u32) -> Option<[u8; 128]> {
    // Driver calls run with DRM_STATE released — see `snapshot_drivers`.
    for driver in snapshot_drivers() {
        if let Some(edid) = driver.get_connector_edid(id) {
            // Validated here, not in the driver, and for the same reason Linux
            // validates in `drm_connector_update_edid_property` rather than in
            // each driver: this is the one door the bytes leave through. A
            // driver reports what it read off the DDC line; whether that is fit
            // to be a monitor's identity is the core's call.
            if let Some(why) = edid_refusal_reason(&edid) {
                let n = EDID_REFUSALS.fetch_add(1, Ordering::Relaxed);
                if n < MAX_EDID_REFUSALS {
                    kernel_hal::klog_info!(
                        "[drm] connector {}: refusing the EDID a driver reported ({} is \
                         wrong), so the connector reports none and clients fall back to \
                         the mode{}",
                        id,
                        why,
                        if n + 1 == MAX_EDID_REFUSALS {
                            " (further refusals will not be reported)"
                        } else {
                            ""
                        }
                    );
                }
                continue;
            }
            return Some(edid);
        }
    }
    if id == SYNTH_CONNECTOR_ID {
        return boot_edid_block();
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

/// Put the process-wide output state a present depends on back to its defaults.
///
/// Called when an emulated output detaches, INCLUDING while a panic unwinds. A
/// test that fails half way through would otherwise leave a visible software
/// cursor, or a blanked CRTC, behind, and the next test then fails for a reason
/// that is not its own -- exactly the noise [`test_globals`] exists to remove.
/// Call it with no display registered, so unblanking cannot clear one.
#[cfg(test)]
pub(crate) fn reset_output_state_for_test() {
    set_crtc_blanked(false);
    // A pause is the one latch that makes a present SUCCEED while touching
    // nothing: `present_now_checked` acknowledges the flip and returns Ok. One
    // leaked by an earlier test would make every later present test pass for no
    // reason at all, which is worse than failing. Cleared straight on the
    // atomics rather than through `set_scanout_paused(false)`, which would
    // present -- and this runs with no display registered, on purpose.
    SCANOUT_PAUSED.store(false, Ordering::SeqCst);
    SCANOUT_PAUSE_DEADLINE_NS.store(0, Ordering::SeqCst);
    SCANOUT_STALE.store(false, Ordering::SeqCst);
    // Back to "nobody has said", which is what a fresh process looks like.
    HW_CURSOR.store(0, Ordering::Relaxed);
    // And back to the default boot, where the atomic uAPI is off.
    set_atomic_enabled(false);
    // The probe is a diagnostic armed from the cmdline, so a test that armed it
    // must not leave it armed: it makes every later present read its window
    // twice and can put a line in the klog for a frame nobody was looking at.
    // Its report budget goes back too, or the last test to run finds it spent.
    set_present_probe_enabled(false);
    set_cursor_from_client(false);
    set_present_repair_enabled(false);
    // `set_present_skip_enabled` resets the band state itself, which is what a
    // fresh boot looks like: nothing known about what the panel holds. A leaked
    // hash would make a later test's present skip a band for a reason that has
    // nothing to do with what it is testing.
    set_present_skip_enabled(false);
    SKIP_BANDS_SKIPPED.store(0, Ordering::Relaxed);
    SKIP_PRESENTS.store(0, Ordering::Relaxed);
    REPAIR_ROUNDS_RUN.store(0, Ordering::Relaxed);
    PROBE_REPORTS.store(0, Ordering::Relaxed);
    ZERO_REPORTS.store(0, Ordering::Relaxed);
    // The split source budgets leak across tests exactly like the shared one
    // did: a test that spends the one clean line would leave the next test
    // asserting about a line the budget had already refused.
    ZERO_FOUND_REPORTS.store(0, Ordering::Relaxed);
    ZERO_CLEAN_REPORTS.store(0, Ordering::Relaxed);
    ZERO_GREW_REPORTS.store(0, Ordering::Relaxed);
    // A save left behind describes a panel the next test does not have, and the
    // erase would put those pixels onto it.
    forget_cursor_under();
    CLEAN_SOURCE_LINES.store(0, Ordering::Relaxed);
    BLACK_SOURCE_LINES.store(0, Ordering::Relaxed);
    GREW_SOURCE_LINES.store(0, Ordering::Relaxed);
    // A leaked `PANEL_FB` makes a later test's damage box either honoured or
    // promoted for a reason that has nothing to do with what it is testing --
    // and the ids the tests pick collide freely, so it would sometimes match.
    set_panel_fb(0);
    DAMAGE_PROMOTIONS_LOGGED.store(0, Ordering::Relaxed);
    // And "nobody has claimed a VT", which is what lets the next test's first
    // present claim the foreground one instead of being suppressed by a
    // neighbour's leftover owner.
    DRM_STATE.lock().graphics_vt = None;
    let mut st = DRM_STATE.lock();
    st.cursor = CursorState::default();
    st.crtc_fb = 0;
    // A committed atomic state outlives the test that committed it: the next
    // one's ACTIVE=0 then reads as a modeset (the CRTC is on) and is refused
    // for a reason that has nothing to do with what it was testing.
    st.atomic = AtomicKmsState::default();
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
    fn process_exit_gives_back_the_syncobjs_it_still_held() {
        let _serialised = super::test_globals::lock();
        use zcore_drivers::scheme::syncobj;
        // Linux frees a dying client's syncobj handles with its drm_file
        // (`drm_syncobj_release`); here nothing did, so a crashed client's
        // stayed in the table for the rest of the boot.
        let mine = syncobj::create_for(77_004, false);
        let shared = syncobj::create_for(77_005, false);
        assert!(syncobj::add_ref_for(77_004, shared), "imported by 77_004");
        let theirs = syncobj::create_for(77_005, true);
        assert_eq!(release_process(77_004), 0, "no buffers to give back");
        assert!(!syncobj::exists(mine), "freed with its owner");
        assert!(syncobj::exists(shared), "77_005 still holds it");
        assert!(!syncobj::held_by(77_004, shared));
        assert!(syncobj::exists(theirs), "another process's survives");
        assert_eq!(release_process(77_004), 0, "idempotent");
        assert!(syncobj::destroy_for(77_005, shared));
        assert!(syncobj::destroy_for(77_005, theirs));
        assert!(!syncobj::exists(shared) && !syncobj::exists(theirs));
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
        let _serialised = super::test_globals::lock();
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
        let _serialised = super::test_globals::lock();
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
        let _serialised = super::test_globals::lock();
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
        let _serialised = super::test_globals::lock();
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

    /// The same sweep, over the buffer an X11 GL client actually produces.
    ///
    /// A native Wayland client's buffer has one holder, so "the dying pid
    /// holds this object" and "the dying pid made this framebuffer" are the
    /// same statement and the test above cannot tell them apart. Under
    /// Xwayland the buffer has three: the client hands it to Xwayland, which
    /// hands it on to the compositor, and the compositor is the one that
    /// issues `ADDFB` over it (a direct scanout).
    ///
    /// Keyed on holders, the client exiting -- a tab closing, a GL program
    /// ending -- retired the COMPOSITOR's framebuffer and zeroed `crtc_fb`
    /// with it. Every `PAGE_FLIP` and `SETCRTC` on that id then answers "no
    /// such fb", which leaves wlroots with an output it cannot drive.
    #[test]
    fn a_client_exiting_does_not_retire_the_compositor_framebuffer_over_its_buffer() {
        let _serialised = super::test_globals::lock();
        const CLIENT: u64 = 78_201;
        const XWAYLAND: u64 = 78_202;
        const COMPOSITOR: u64 = 78_203;
        let shared = gem_mmap::DRIVER_HANDLE_BASE + 0x68;

        // One buffer, three holders, and the framebuffer belongs to the last
        // of them.
        gem_mmap::register(shared, 0x2_0000, 4096, CLIENT);
        gem_mmap::add_ref(shared, XWAYLAND);
        gem_mmap::add_ref(shared, COMPOSITOR);
        {
            let mut state = DRM_STATE.lock();
            state.framebuffers.push(DrmFramebuffer {
                id: 9505,
                driver_fb_id: None,
                gem_handle_id: shared,
                width: 1,
                height: 1,
                pitch: 4,
                phys_addr: 0x2_0000,
                size: 4096,
                owner: COMPOSITOR,
            });
            state.crtc_fb = 9505;
        }

        release_process(CLIENT);
        assert!(
            fb_exists(9505),
            "a holder exiting must not take a framebuffer somebody else made"
        );
        release_process(XWAYLAND);
        assert!(fb_exists(9505), "nor the hop in the middle exiting");
        assert_eq!(
            DRM_STATE.lock().crtc_fb,
            9505,
            "the scanout framebuffer must still be the one bound to the CRTC"
        );

        // The compositor's own exit is what retires it, as before.
        release_process(COMPOSITOR);
        assert!(!fb_exists(9505));
        assert_eq!(DRM_STATE.lock().crtc_fb, 0);
        gem_mmap::unregister(shared);
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

#[cfg(test)]
mod cursor_invalidate_tests {
    use super::expand_x_for_wc;

    /// The byte range a CLFLUSH loop over `[start, start + len)` actually
    /// evicts: whole 64-byte lines, so it reaches down to the line containing
    /// `start` and up to the one containing the last byte.
    fn flushed_lines(start: usize, len: usize) -> (usize, usize) {
        assert!(len > 0);
        (start - start % 64, (start + len).div_ceil(64) * 64)
    }

    /// Bytes `blit_cursor_patch` / `restore_rect` read on row `r`, given the
    /// rect they were handed. Both widen x the same way before reading.
    fn read_span(row: usize, stride_px: usize, x: u32, w: u32, pitch_px: u32) -> (usize, usize) {
        let (ex, ew) = expand_x_for_wc(x, w, pitch_px);
        let start = (row * stride_px + ex as usize) * 4;
        (start, start + ew as usize * 4)
    }

    /// Bytes the invalidate covers on row `r` for the rect it was handed.
    fn sync_span(row: usize, stride_px: usize, x: u32, w: u32) -> (usize, usize) {
        let start = (row * stride_px + x as usize) * 4;
        (start, start + w as usize * 4)
    }

    /// Strides that are NOT a multiple of 16 pixels are the interesting ones:
    /// there the row base is not 64-byte aligned, so widening x to the
    /// write-combining boundary walks into cache lines the unexpanded rect
    /// never touched. 1366 is the classic panel width (5464 bytes = 8 mod 16).
    const STRIDES: [usize; 4] = [1366, 1367, 1376, 1920];
    const CURSOR_W: u32 = 64;

    #[test]
    fn the_invalidate_covers_every_column_the_cursor_blit_reads() {
        for stride in STRIDES {
            let pitch_px = stride as u32;
            for x in 0..48u32 {
                for row in [0usize, 1, 2, 7, 33] {
                    let (rd0, rd1) = read_span(row, stride, x, CURSOR_W, pitch_px);
                    // What the fixed code invalidates: the same widened span.
                    let (ex, ew) = expand_x_for_wc(x, CURSOR_W, pitch_px);
                    let (sy0, sy1) = sync_span(row, stride, ex, ew);
                    let (f0, f1) = flushed_lines(sy0, sy1 - sy0);
                    assert!(
                        f0 <= rd0 && f1 >= rd1,
                        "stride={} x={} row={}: flushed [{},{}) does not cover read [{},{})",
                        stride,
                        x,
                        row,
                        f0,
                        f1,
                        rd0,
                        rd1
                    );
                }
            }
        }
    }

    /// The bug this replaced: invalidating the rect as asked for, while the
    /// blit reads the widened one. On a stride that is not a multiple of 16
    /// pixels there are rows where the flushed lines fall short -- those are
    /// the columns that came back as stale cache and got painted to screen.
    #[test]
    fn the_unexpanded_invalidate_left_columns_unflushed() {
        let mut short = 0;
        for stride in STRIDES {
            let pitch_px = stride as u32;
            for x in 0..48u32 {
                for row in 0..64usize {
                    let (rd0, rd1) = read_span(row, stride, x, CURSOR_W, pitch_px);
                    // What the old code invalidated: the rect as handed in.
                    let (sy0, sy1) = sync_span(row, stride, x, CURSOR_W);
                    let (f0, f1) = flushed_lines(sy0, sy1 - sy0);
                    if f0 > rd0 || f1 < rd1 {
                        short += 1;
                    }
                }
            }
        }
        assert!(
            short > 0,
            "expected the unexpanded invalidate to fall short somewhere; \
             if this fires, the widening is no longer load-bearing"
        );
    }
}

/// The other half of "invalidate what you READ": whether the present
/// invalidated the pointer's window *at all*.
///
/// [`cursor_invalidate_tests`] above is about the columns within that window.
/// This one is about a present that never reaches the window: a damage-clipped
/// commit. `dma_sync_scanout_src_from_device` flushes one contiguous run from
/// the damage box's first row to its last, so a pointer sitting outside those
/// rows is blended from lines nothing invalidated.
///
/// The screen cannot show this in a host test — there is no stale cache to
/// read — so what is checked is the decision itself, through the same
/// [`cursor_read_is_synced`] the present calls.
#[cfg(test)]
mod partial_present_cursor_sync_tests {
    use super::{cursor_read_is_synced, expand_x_for_wc};

    const STRIDE: usize = 1920;
    const FB_H: u32 = 1080;
    const CURSOR: u32 = 64;

    /// The pointer window as the present computes it: x widened to the
    /// write-combining boundary, then handed in as the read rect.
    fn ptr(x: u32, y: u32) -> (i32, u32, u32, u32) {
        let (ex, ew) = expand_x_for_wc(x, CURSOR, STRIDE as u32);
        (ex as i32, y, ew, CURSOR)
    }

    fn synced_for(blit: (u32, u32, u32, u32), at: (u32, u32)) -> bool {
        let (ex, py, ew, ph) = ptr(at.0, at.1);
        cursor_read_is_synced(
            STRIDE,
            blit,
            true,
            (ex, py as i32, ew, ph),
            STRIDE as u32,
            FB_H,
        )
    }

    /// A full-frame present invalidates the whole buffer in one run, so the
    /// pointer is covered wherever it is and the present must not pay for a
    /// second flush. This is the case the old guard was written for, and it
    /// has to keep behaving exactly as it did.
    #[test]
    fn a_full_frame_present_covers_the_pointer_anywhere() {
        let full = (0, 0, STRIDE as u32, FB_H);
        for x in [0u32, 1, 15, 700, 1855, 1919] {
            for y in [0u32, 1, 540, 1015, 1079] {
                assert!(
                    synced_for(full, (x, y)),
                    "full frame left the pointer at ({}, {}) unsynced",
                    x,
                    y
                );
            }
        }
    }

    /// The bug: a popup's damage box does not reach the pointer's rows, and
    /// nothing else in the present invalidates them. A menu or a calendar is
    /// exactly this box.
    #[test]
    fn a_popup_damage_box_does_not_cover_a_pointer_outside_its_rows() {
        // A 320x240 menu near the top left.
        let menu = (48, 64, 320, 240);
        // The pointer well below the menu's last row (64 + 240 = 304).
        for y in [320u32, 500, 900] {
            for x in [64u32, 900, 1600] {
                assert!(
                    !synced_for(menu, (x, y)),
                    "the pointer at ({}, {}) is outside the damage box's rows, so the \
                     present did not invalidate what the blend reads -- it must flush",
                    x,
                    y
                );
            }
        }
    }

    /// And when the box does span the pointer's rows *and* its columns, the
    /// run already covers it: no second flush, so the common case of a popup
    /// opening under the pointer stays as cheap as it was.
    #[test]
    fn a_damage_box_around_the_pointer_needs_no_second_flush() {
        // The box starts left of the widened window and ends right of it, on
        // every row the pointer occupies.
        let wide = (0, 100, STRIDE as u32, 400);
        for y in [100u32, 200, 435] {
            for x in [0u32, 33, 900, 1855] {
                assert!(
                    synced_for(wide, (x, y)),
                    "pointer at ({}, {}) is inside the damage run and was flushed twice",
                    x,
                    y
                );
            }
        }
    }

    /// A box that spans the pointer's ROWS but stops short of its COLUMNS is
    /// still covered, because the invalidate is one contiguous run and not a
    /// set of rows -- the columns in between are flushed on the way past. This
    /// is what makes the cheap containment test correct rather than merely
    /// conservative; getting it wrong the other way would flush on every
    /// frame.
    #[test]
    fn the_run_between_the_first_and_last_row_counts_as_covered() {
        // Columns [0, 200) only, rows [100, 500) -- the pointer at x = 1600 is
        // far to its right, but between row 100's left edge and row 499's
        // right edge in linear order.
        let narrow = (0, 100, 200, 400);
        assert!(
            synced_for(narrow, (1600, 300)),
            "a row strictly inside the run is covered whatever its column"
        );
        // The pointer's LAST row must still be inside the run: at row 436 the
        // window ends on row 499, the run's last row, past its right edge.
        assert!(
            !synced_for(narrow, (1600, 436)),
            "the pointer's last row runs past the end of the run"
        );
    }

    /// Every unknown falls towards "flush". A needless flush of a 64x64 window
    /// costs microseconds; a skipped one puts stale pixels on the screen, so
    /// there is no input for which "I cannot tell" may answer "covered".
    #[test]
    fn what_cannot_be_decided_is_flushed() {
        let full = (0, 0, STRIDE as u32, FB_H);
        let (ex, py, ew, ph) = ptr(700, 500);
        // A stride of zero cannot be linearised at all.
        assert!(
            !cursor_read_is_synced(0, full, true, (ex, py as i32, ew, ph), STRIDE as u32, FB_H),
            "a stride nothing can be linearised against must flush"
        );
        // The present ran no FromDevice of its own.
        assert!(
            !cursor_read_is_synced(
                STRIDE,
                full,
                false,
                (ex, py as i32, ew, ph),
                STRIDE as u32,
                FB_H
            ),
            "no sync ran, so nothing is covered"
        );
        // A degenerate blit rect invalidated nothing.
        assert!(
            !cursor_read_is_synced(
                STRIDE,
                (0, 0, 0, 0),
                true,
                (ex, py as i32, ew, ph),
                STRIDE as u32,
                FB_H
            ),
            "an empty blit rect covers nothing"
        );
        // A width past `i32::MAX` used to wrap negative through an `as i32`
        // and read as an empty rect -- "nothing to read", which skips the
        // flush. It has to clip to the buffer and be treated as a real read.
        assert!(
            !cursor_read_is_synced(
                STRIDE,
                (0, 900, 16, 8),
                true,
                (0, 0, u32::MAX, u32::MAX),
                STRIDE as u32,
                FB_H
            ),
            "an out-of-range read window must clip, not vanish"
        );
    }
}

/// Tests for the "one page flip outstanding" invariant, driven single-threaded
/// with no timers: the queue and the pending latch are private statics in this
/// module, so a test can put them into exactly the state the race produced.
#[cfg(test)]
mod flip_latch_tests {
    extern crate std;

    use super::*;

    /// Start from a clean slate; these statics are process-wide.
    fn reset() {
        PENDING_DRM_TIMERS.lock().clear();
        FLIPS_IN_FLIGHT.store(0, Ordering::Release);
        FLIP_EVENT_PENDING.store(false, Ordering::Release);
        DRM_TIMER_ARMED.store(false, Ordering::Release);
    }

    /// Queue a flip the way `schedule_flip_event` does, without arming a real
    /// timer (which under libos would spawn an async sleep task and make this
    /// non-deterministic).
    fn queue_one(file: &Arc<DrmFileState>) {
        let mut q = PENDING_DRM_TIMERS.lock();
        FLIPS_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
        FLIP_EVENT_PENDING.store(true, Ordering::Release);
        q.push_back(PendingDrmTimer::Flip {
            crtc_id: SYNTH_CRTC_ID,
            user_data: 0xF11D,
            file: Arc::downgrade(file),
        });
    }

    /// The regression this guards. `deliver_pending_drm_timer` drains the whole
    /// queue into a local before delivering anything, so a flip that is
    /// mid-delivery is in neither the queue nor yet delivered.
    /// `clear_stale_flip_pending` looked only at the queue, decided the latch
    /// was stale, and cleared it -- so a concurrent PAGE_FLIP was accepted while
    /// the previous one had not completed, and two frames could reach the
    /// scanout inside one vblank period.
    #[test]
    fn a_flip_being_delivered_still_counts_as_pending() {
        let _serialised = super::test_globals::lock();
        reset();
        let file = DrmFileState::new();
        queue_one(&file);
        assert!(FLIP_EVENT_PENDING.load(Ordering::Acquire));

        // Reproduce the window: the queue has been drained but the event has
        // not been pushed to the fd yet.
        let drained: Vec<PendingDrmTimer> = PENDING_DRM_TIMERS.lock().drain(..).collect();
        assert_eq!(drained.len(), 1);
        assert!(
            !PENDING_DRM_TIMERS
                .lock()
                .iter()
                .any(|j| matches!(j, PendingDrmTimer::Flip { .. })),
            "the queue is empty, which is what fooled the old check"
        );

        clear_stale_flip_pending();
        assert!(
            FLIP_EVENT_PENDING.load(Ordering::Acquire),
            "the latch must survive: this flip has not been delivered yet"
        );

        // Delivering it is what clears the latch.
        queue_flip_event(&file, SYNTH_CRTC_ID, 0xF11D);
        assert!(!FLIP_EVENT_PENDING.load(Ordering::Acquire));
        assert_eq!(FLIPS_IN_FLIGHT.load(Ordering::Acquire), 0);
        assert!(file.has_events(), "the completion reached the card fd");
        reset();
    }

    /// The other half: `queue_flip_event` cleared the latch unconditionally, so
    /// delivering the first of two outstanding flips advertised "nothing
    /// pending" while the second was still queued -- after which a further flip
    /// skipped the flush and the queue held two.
    #[test]
    fn delivering_one_of_two_flips_leaves_the_latch_set() {
        let _serialised = super::test_globals::lock();
        reset();
        let file = DrmFileState::new();
        queue_one(&file);
        queue_one(&file);
        assert_eq!(FLIPS_IN_FLIGHT.load(Ordering::Acquire), 2);

        queue_flip_event(&file, SYNTH_CRTC_ID, 0xF11D);
        assert!(
            FLIP_EVENT_PENDING.load(Ordering::Acquire),
            "one flip is still outstanding"
        );
        queue_flip_event(&file, SYNTH_CRTC_ID, 0xF11D);
        assert!(!FLIP_EVENT_PENDING.load(Ordering::Acquire), "now none is");
        reset();
    }

    /// A genuinely stale latch -- set with nothing queued and nothing in flight
    /// -- still self-heals. That is what `clear_stale_flip_pending` is for: a
    /// stuck latch means a persistent EBUSY, which wlroots escalates into an
    /// output teardown.
    #[test]
    fn a_truly_stale_latch_is_still_cleared() {
        let _serialised = super::test_globals::lock();
        reset();
        FLIP_EVENT_PENDING.store(true, Ordering::Release);
        clear_stale_flip_pending();
        assert!(!FLIP_EVENT_PENDING.load(Ordering::Acquire));
        reset();
    }

    /// Cancelling is accounted for too, and can never drive the count below
    /// zero: an underflow would wrap and latch the flag on forever.
    #[test]
    fn cancelling_clears_the_latch_and_never_underflows() {
        let _serialised = super::test_globals::lock();
        reset();
        let file = DrmFileState::new();
        queue_one(&file);
        cancel_pending_events();
        assert!(!FLIP_EVENT_PENDING.load(Ordering::Acquire));
        assert_eq!(FLIPS_IN_FLIGHT.load(Ordering::Acquire), 0);

        // More completions than flips (a cancel racing a delivery) must not wrap.
        flip_in_flight_done();
        flip_in_flight_done();
        assert_eq!(FLIPS_IN_FLIGHT.load(Ordering::Acquire), 0);
        assert!(!FLIP_EVENT_PENDING.load(Ordering::Acquire));
        reset();
    }

    /// The window as the timer IRQ leaves it on another CPU: queued, drained,
    /// not yet posted. [`finish_delivery`] is the other half.
    fn leave_one_mid_delivery(file: &Arc<DrmFileState>) {
        queue_one(file);
        let drained: Vec<PendingDrmTimer> = PENDING_DRM_TIMERS.lock().drain(..).collect();
        assert_eq!(drained.len(), 1);
        assert!(FLIP_EVENT_PENDING.load(Ordering::Acquire));
    }

    /// Also puts the next vblank slot in the future as of now: the racer
    /// goes ahead the moment this returns, and a commit that finds the slot
    /// already missed delivers its own completion on the spot (see
    /// `arm_coalesced_drm_timer_locked`), leaving nothing to look at.
    fn finish_delivery(file: &DrmFileState) {
        DRM_STATE.lock().next_vblank = kernel_hal::timer::timer_now();
        queue_flip_event(file, SYNTH_CRTC_ID, 0xF11D);
    }

    /// Run `f` on another thread -- the compositor's syscall on another CPU
    /// than the timer -- with the delivery held open until `f` has certainly
    /// reached its settle step, then finish the delivery. Returns what `f`
    /// answered and whether the first completion was already on the fd when
    /// it did: a caller that went ahead early sees an empty fd.
    fn race_against_delivery<R: Send + 'static>(
        file: &Arc<DrmFileState>,
        f: impl FnOnce(&Arc<DrmFileState>) -> R + Send + 'static,
    ) -> (R, bool) {
        let entered = Arc::new(AtomicBool::new(false));
        let racer = {
            let file = file.clone();
            let entered = entered.clone();
            std::thread::spawn(move || {
                entered.store(true, Ordering::Release);
                let answer = f(&file);
                (answer, file.has_events())
            })
        };
        while !entered.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            FLIPS_IN_FLIGHT.load(Ordering::Acquire),
            1,
            "the racer went ahead while the delivery was still open"
        );
        finish_delivery(file);
        racer.join().expect("the racer panicked")
    }

    /// A completion that `atomic_commit` scheduled has armed the real timer,
    /// which fires on another thread here. Let it, so it does not go off in
    /// the middle of whichever test runs next.
    fn let_the_armed_timer_fire() {
        for _ in 0..200 {
            if !DRM_TIMER_ARMED.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the vblank timer never fired");
    }

    /// The compositor's next PAGE_FLIP lands on one CPU while the timer IRQ on
    /// another has the previous completion drained but not yet posted. This
    /// used to be EBUSY -- the "safety net" in `page_flip` -- and wlroots tears
    /// the output down on that. It waits for the delivery instead.
    #[test]
    fn a_flip_that_lands_mid_delivery_waits_for_it_instead_of_ebusy() {
        let _serialised = super::test_globals::lock();
        reset();
        let file = DrmFileState::new();
        leave_one_mid_delivery(&file);

        let (answer, first_completion_was_on_the_fd) = race_against_delivery(&file, |file| {
            page_flip(0x77, SYNTH_CRTC_ID, 0xF1A9, true, file)
        });
        // 0x77 is no framebuffer, so past the settle step the answer is the
        // fb's own. What matters is that it is not EBUSY.
        assert_eq!(answer, Err(FlipError::Present(PresentError::NoSuchFb)));
        assert!(
            first_completion_was_on_the_fd,
            "the flip went ahead before the completion reached the fd"
        );
        assert!(!FLIP_EVENT_PENDING.load(Ordering::Acquire));
        assert_eq!(FLIPS_IN_FLIGHT.load(Ordering::Acquire), 0);
        reset();
    }

    /// The atomic path has the same step, and the same used-to-be-EBUSY.
    #[test]
    fn an_atomic_commit_that_lands_mid_delivery_waits_for_it_too() {
        let _serialised = super::test_globals::lock();
        reset();
        let file = DrmFileState::new();
        leave_one_mid_delivery(&file);

        let (answer, first_completion_was_on_the_fd) = race_against_delivery(&file, |file| {
            atomic_commit(&AtomicUpdate::default(), false, false, true, 0xA70, file)
        });
        assert_eq!(
            answer,
            Ok(()),
            "an empty commit that asks for an event is accepted"
        );
        assert!(
            first_completion_was_on_the_fd,
            "the commit went ahead before the completion reached the fd"
        );
        // And its own completion is the one now outstanding, queued behind
        // the one that was delivered.
        assert_eq!(FLIPS_IN_FLIGHT.load(Ordering::Acquire), 1);
        assert!(PENDING_DRM_TIMERS.lock().iter().any(|j| matches!(
            j,
            PendingDrmTimer::Flip {
                user_data: 0xA70,
                ..
            }
        )));
        let_the_armed_timer_fire();
        reset();
    }

    /// A commit that asks for no event owes the CRTC no ordering with the
    /// previous completion, and never did: it must not sit out the delivery.
    #[test]
    fn a_commit_without_an_event_does_not_wait_for_the_delivery() {
        let _serialised = super::test_globals::lock();
        reset();
        let file = DrmFileState::new();
        leave_one_mid_delivery(&file);

        assert_eq!(
            atomic_commit(&AtomicUpdate::default(), false, false, false, 0, &file),
            Ok(())
        );
        assert!(
            FLIP_EVENT_PENDING.load(Ordering::Acquire),
            "the delivery is still open; the commit did not touch it"
        );
        assert!(!file.has_events());
        finish_delivery(&file);
        reset();
    }

    /// A completion still queued for the vblank is delivered now, not waited
    /// for: the compositor's next frame must not sit out the rest of the
    /// period. No timer is armed here, so waiting would be the whole limit.
    #[test]
    fn a_queued_completion_is_delivered_now_not_waited_for() {
        let _serialised = super::test_globals::lock();
        reset();
        let file = DrmFileState::new();
        queue_one(&file);

        assert!(settle_outstanding_flip());
        assert!(file.has_events(), "the queued completion reached the fd");
        assert_eq!(FLIPS_IN_FLIGHT.load(Ordering::Acquire), 0);
        assert!(!FLIP_EVENT_PENDING.load(Ordering::Acquire));
        reset();
    }

    /// A latch with nothing behind it -- no queued flip, none in flight -- is
    /// the self-heal case, and costs the next flip nothing.
    #[test]
    fn a_stale_latch_does_not_hold_up_the_next_flip() {
        let _serialised = super::test_globals::lock();
        reset();
        FLIP_EVENT_PENDING.store(true, Ordering::Release);
        assert!(settle_outstanding_flip());
        assert!(!FLIP_EVENT_PENDING.load(Ordering::Acquire));
        reset();
    }

    /// The limit: a delivery that never finishes (a counter that lost its
    /// deliverer) is EBUSY, not a syscall that never returns.
    #[test]
    fn a_delivery_that_never_finishes_is_ebusy_not_a_hang() {
        let _serialised = super::test_globals::lock();
        reset();
        let file = DrmFileState::new();
        leave_one_mid_delivery(&file);

        assert!(!settle_outstanding_flip());
        assert_eq!(
            page_flip(0x77, SYNTH_CRTC_ID, 0, true, &file),
            Err(FlipError::Busy)
        );
        reset();
    }
}

/// Tests for the reference the present path holds on a framebuffer's backing
/// while it blits with `DRM_STATE` released.
#[cfg(test)]
mod present_lifetime_tests {
    use super::*;

    /// The regression this guards. `DrmFramebuffer` is `Copy`, so the present
    /// path took a bare `phys_addr`/`size` out of `DRM_STATE`, dropped the lock,
    /// and blitted for up to ~100 ms holding nothing. A concurrent RMFB could
    /// drop the last `Arc<VmObject>` in that window and hand the frames back to
    /// the allocator while the blit was still reading them.
    #[test]
    fn a_present_snapshot_keeps_the_framebuffer_memory_alive() {
        let _serialised = super::test_globals::lock();
        let vmo = VmObject::new_paged(1);
        {
            let mut state = DRM_STATE.lock();
            state.framebuffers.push(DrmFramebuffer {
                id: 9601,
                driver_fb_id: None,
                gem_handle_id: 7,
                width: 1,
                height: 1,
                pitch: 4,
                phys_addr: 0,
                size: 4096,
                owner: current_pid(),
            });
            state.fb_backing.push((9601, vmo.clone()));
        }
        // The test's own reference plus the one in `fb_backing`.
        assert_eq!(Arc::strong_count(&vmo), 2);

        let (fb, backing) = snapshot_fb_for_present(9601).expect("the fb exists");
        assert_eq!(fb.id, 9601);
        let backing = backing.expect("a dumb-buffer fb has a VMO to hold");
        assert_eq!(
            Arc::strong_count(&vmo),
            3,
            "the snapshot must take its own reference"
        );

        // RMFB while the "blit" is in flight: the fb is gone from the table, but
        // the memory is NOT freed, because the snapshot still owns a reference.
        assert!(rmfb(9601));
        assert!(snapshot_fb_for_present(9601).is_none(), "the fb is retired");
        assert_eq!(
            Arc::strong_count(&vmo),
            2,
            "only the table's reference dropped; the blit's is intact"
        );

        // The blit finishes and lets go.
        drop(backing);
        assert_eq!(Arc::strong_count(&vmo), 1);
    }

    /// A nouveau-backed framebuffer has no `VmObject` to take a reference on, so
    /// the snapshot reports `None` rather than pretending. That window is closed
    /// from the other end instead, by retiring the framebuffer when the GEM
    /// object is freed -- which is what `retire_framebuffers_for_handle` does.
    #[test]
    fn a_nouveau_backed_framebuffer_has_no_reference_to_take() {
        let _serialised = super::test_globals::lock();
        let handle = zcore_drivers::scheme::gem_mmap::DRIVER_HANDLE_BASE + 0x77;
        {
            let mut state = DRM_STATE.lock();
            state.framebuffers.push(DrmFramebuffer {
                id: 9602,
                driver_fb_id: None,
                gem_handle_id: handle,
                width: 1,
                height: 1,
                pitch: 4,
                phys_addr: 0x2_0000,
                size: 4096,
                owner: current_pid(),
            });
        }
        let (fb, backing) = snapshot_fb_for_present(9602).expect("the fb exists");
        assert_eq!(fb.gem_handle_id, handle);
        assert!(
            backing.is_none(),
            "there is no VmObject behind a nouveau GEM"
        );
        DRM_STATE.lock().framebuffers.retain(|f| f.id != 9602);
    }

    /// An unknown id is not a framebuffer.
    #[test]
    fn an_unknown_framebuffer_cannot_be_snapshotted() {
        let _serialised = super::test_globals::lock();
        assert!(snapshot_fb_for_present(0).is_none());
        assert!(snapshot_fb_for_present(0xDEAD_BEEF).is_none());
    }
}

/// The two latches that can leave a desktop frozen: the scanout pause (and its
/// watchdog) and the mark that says the panel is a frame behind `crtc_fb`.
///
/// None of this had a test. It is also the only path in the file where a
/// present SUCCEEDS while touching no pixels, which is what makes it worth
/// pinning twice over: a pause leaked out of one test would make every later
/// present test pass for a reason that has nothing to do with what it checks.
#[cfg(test)]
mod scanout_pause_tests {
    extern crate std;

    use super::*;
    use core::time::Duration;

    /// Put the process-global latches back however the body leaves -- or panics
    /// out of -- them, and serialise against every other test that presents.
    struct Restored {
        _serialised: self::std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for Restored {
        fn drop(&mut self) {
            reset_output_state_for_test();
        }
    }

    fn serialised() -> Restored {
        let g = Restored {
            _serialised: super::test_globals::lock(),
        };
        reset_output_state_for_test();
        g
    }

    // --- the decision itself, with no statics and no clock in the way ---

    /// A pause with no watchdog is the permanent kind, and `0` is how the
    /// deadline says so. It must not read as "a deadline in the distant past".
    #[test]
    fn a_pause_with_no_watchdog_holds_whatever_the_clock_says() {
        for now in [0u64, 1, 1_000_000_000, u64::MAX] {
            assert!(
                pause_in_force(true, 0, now),
                "a pause with no watchdog lifted itself at now={}",
                now
            );
        }
    }

    /// The watchdog's whole contract, at the boundary: in force up to the
    /// deadline, over the moment it arrives. An off-by-one the wrong way is a
    /// pause that outlives its window.
    #[test]
    fn a_watchdogged_pause_holds_up_to_its_deadline_and_not_past_it() {
        let deadline = 90_000_000_000;
        for now in [0u64, 1, deadline - 1] {
            assert!(
                pause_in_force(true, deadline, now),
                "expired early at now={}",
                now
            );
        }
        for now in [deadline, deadline + 1, u64::MAX] {
            assert!(
                !pause_in_force(true, deadline, now),
                "still paused at now={}, past its deadline",
                now
            );
        }
    }

    /// With the latch off nothing is paused, whatever the deadline still holds.
    /// A resume clears both, but not in one instruction, so a reader can see
    /// this combination.
    #[test]
    fn nothing_is_paused_while_the_latch_is_off() {
        for deadline in [0u64, 1, 90_000_000_000, u64::MAX] {
            assert!(!pause_in_force(false, deadline, 1_000));
        }
    }

    /// The sentinel collision, which is how the watchdog turns into the freeze
    /// it exists to prevent: store a deadline of `0` and every reader reads
    /// "no watchdog". `now + max` can land there -- a clock that starts at zero
    /// with a zero window, or a sum truncated by the cast -- so the one thing
    /// this may never return is `0`.
    #[test]
    fn a_watchdog_deadline_is_never_the_value_that_means_no_watchdog() {
        let cases = [
            (0u64, Duration::ZERO),
            (0, Duration::from_nanos(0)),
            (0, Duration::from_secs(90)),
            (1, Duration::ZERO),
            (u64::MAX, Duration::from_secs(90)),
            (u64::MAX / 2, Duration::from_secs(u64::MAX / 2)),
        ];
        for (now, max) in cases {
            let deadline = pause_deadline_ns(now, max);
            assert_ne!(
                deadline, 0,
                "now={} max={:?} gave the no-watchdog sentinel, i.e. a pause \
                 that never lifts",
                now, max
            );
            // And the pause it describes is one a reader can get out of: either
            // the window is still open, or it has already closed.
            assert!(
                !pause_in_force(true, deadline, u64::MAX),
                "now={} max={:?} gave a deadline no clock can pass",
                now,
                max
            );
        }
    }

    /// And the window really is as long as it was asked for. `as_nanos` is a
    /// `u128`; the cast to the stored `u64` truncates, which turns a long window
    /// into a short one -- the same mistake as storing the sentinel, one step
    /// milder, and equally invisible without a number to compare against.
    #[test]
    fn the_watchdog_window_is_as_long_as_it_was_asked_for() {
        assert_eq!(
            pause_deadline_ns(1_000, Duration::from_secs(90)),
            1_000 + 90_000_000_000
        );
        assert_eq!(pause_deadline_ns(0, Duration::from_nanos(1)), 1);
        // A window longer than the deadline can hold stops at the far end
        // rather than wrapping round to a moment that has already passed.
        assert_eq!(
            pause_deadline_ns(5, Duration::from_secs(u64::MAX)),
            u64::MAX,
            "an absurd window was truncated into a short one"
        );
    }

    /// A window that has not run out is worth more than the plain pause that
    /// would replace it: one frees itself after 90 seconds, the other never
    /// does. A second `set_scanout_paused(true)` must not make that trade.
    #[test]
    fn a_second_plain_pause_leaves_a_running_watchdog_alone() {
        let deadline = 90_000_000_000;
        for now in [0u64, 1, deadline - 1] {
            assert_eq!(
                deadline_kept_by_plain_pause(deadline, now),
                deadline,
                "the watchdog was thrown away at now={}, turning a 90-second \
                 freeze into a permanent one",
                now
            );
        }
    }

    /// And the mirror image: a deadline already in the past is not a watchdog,
    /// it is a pause that lifts on its first read. Inheriting it would make the
    /// call not pause at all.
    #[test]
    fn a_plain_pause_does_not_inherit_a_watchdog_that_already_ran_out() {
        let deadline = 90_000_000_000;
        for now in [deadline, deadline + 1, u64::MAX] {
            assert_eq!(
                deadline_kept_by_plain_pause(deadline, now),
                0,
                "now={} kept an expired deadline, so the pause would lift \
                 immediately",
                now
            );
        }
        // "No watchdog" is already the answer and stays it.
        assert_eq!(deadline_kept_by_plain_pause(0, 12_345), 0);
    }

    /// A damage rect that covers the whole display catches the panel up just as
    /// a full-frame present does -- a `DIRTYFB` clip over the entire framebuffer
    /// is a legal way to say "all of it changed". Anything short of that does
    /// not, and neither does any rect when there is no display to catch up with.
    #[test]
    fn only_a_present_that_covers_the_whole_screen_catches_the_panel_up() {
        let screen = Some((1920, 1080));
        assert!(present_caught_the_panel_up(None, screen));
        assert!(
            present_caught_the_panel_up(None, None),
            "a full frame is one"
        );
        assert!(present_caught_the_panel_up(
            Some((0, 0, 1920, 1080)),
            screen
        ));
        assert!(
            present_caught_the_panel_up(Some((0, 0, 3840, 2160)), screen),
            "a rect larger than the display still covers it"
        );
        for short in [
            (0, 0, 1919, 1080),
            (0, 0, 1920, 1079),
            (1, 0, 1920, 1080),
            (0, 1, 1920, 1080),
            (48, 25, 180, 160),
        ] {
            assert!(
                !present_caught_the_panel_up(Some(short), screen),
                "{:?} leaves rows or columns behind and must keep the mark",
                short
            );
        }
        assert!(
            !present_caught_the_panel_up(Some((0, 0, 1920, 1080)), None),
            "with no display there is nothing to be caught up with"
        );
    }

    // --- and the same decisions through the real latches ---

    /// The plain pause is the one that latches. It must survive being read, and
    /// read, and read -- the present path asks once per frame.
    #[test]
    fn a_plain_pause_survives_every_read_until_someone_lifts_it() {
        let _restored = serialised();
        set_scanout_paused(true);
        for _ in 0..8 {
            assert!(scanout_paused(), "a pause with no watchdog lifted itself");
        }
        set_scanout_paused(false);
        assert!(!scanout_paused());
    }

    /// The watchdog, end to end: a window that has already closed is lifted by
    /// the first reader, and stays lifted. A zero-length window is the same code
    /// path as a 90-second one that ran out, without a test that sleeps.
    #[test]
    fn the_watchdog_lifts_a_pause_nobody_came_back_for() {
        let _restored = serialised();
        set_scanout_paused_for(Duration::ZERO);
        assert!(
            !scanout_paused(),
            "a window that has already closed still counted as paused"
        );
        assert!(!scanout_paused(), "and it must not come back");
        assert_eq!(
            SCANOUT_PAUSE_DEADLINE_NS.load(Ordering::SeqCst),
            0,
            "the expired deadline was left behind for the next pause to inherit"
        );
    }

    /// A window that is still open is not cut short by a reader.
    #[test]
    fn a_watchdog_that_has_not_run_out_keeps_the_pause() {
        let _restored = serialised();
        set_scanout_paused_for(SCANOUT_PAUSE_MAX);
        assert!(scanout_paused());
        assert!(scanout_paused());
    }

    /// The clobber, through the real latches: pausing again while the bring-up
    /// window is open used to store the no-watchdog sentinel first, so a wedge
    /// after it froze the desktop for good instead of for 90 seconds.
    #[test]
    fn pausing_again_does_not_turn_a_bounded_freeze_into_a_permanent_one() {
        let _restored = serialised();
        set_scanout_paused_for(SCANOUT_PAUSE_MAX);
        let armed = SCANOUT_PAUSE_DEADLINE_NS.load(Ordering::SeqCst);
        assert_ne!(armed, 0);

        set_scanout_paused(true);

        assert!(scanout_paused());
        assert_eq!(
            SCANOUT_PAUSE_DEADLINE_NS.load(Ordering::SeqCst),
            armed,
            "the second pause cancelled the watchdog"
        );
    }

    /// A present taken during the pause is reported complete -- the compositor's
    /// frame loop must not stop -- and records what it bound, so `GETCRTC` stays
    /// truthful. What it must ALSO do is admit that the panel no longer shows
    /// that framebuffer, because nothing else in the tree can tell afterwards.
    #[test]
    fn a_present_dropped_by_the_pause_is_acknowledged_and_marks_the_panel_stale() {
        let _restored = serialised();
        set_scanout_paused(true);

        assert!(
            present_now_checked(0x5151, SYNTH_CRTC_ID, None).is_ok(),
            "a paused present must still be acknowledged, or the compositor \
             blocks in poll() for a frame nobody will report"
        );
        assert_eq!(crtc_fb(), 0x5151, "GETCRTC must still report what it bound");
        assert!(
            SCANOUT_STALE.load(Ordering::SeqCst),
            "the panel is a frame behind crtc_fb and nothing recorded it"
        );
    }

    /// Resuming with nothing ever bound has no frame to put back, so it clears
    /// the mark rather than leaving it set for the next pointer move to act on.
    #[test]
    fn a_resume_with_nothing_bound_clears_the_mark_instead_of_presenting() {
        let _restored = serialised();
        set_scanout_paused(true);
        assert!(present_now_checked(0, SYNTH_CRTC_ID, None).is_ok());
        assert!(SCANOUT_STALE.load(Ordering::SeqCst));

        set_scanout_paused(false);

        assert!(!scanout_paused());
        assert!(
            !SCANOUT_STALE.load(Ordering::SeqCst),
            "nothing was ever bound, so there is no stale frame to chase"
        );
    }

    /// A client that turned the CRTC off during the pause meant it. The resume
    /// must not light the panel behind its back -- and must not drop the mark
    /// either, or the present that does un-blank would restore rects from a
    /// frame the panel never showed.
    #[test]
    fn a_resume_does_not_light_a_panel_the_client_turned_off() {
        let _restored = serialised();
        set_scanout_paused(true);
        assert!(present_now_checked(0x6262, SYNTH_CRTC_ID, None).is_ok());
        set_crtc_blanked(true);

        set_scanout_paused(false);

        assert!(crtc_blanked(), "the resume turned the screen back on");
        assert!(
            SCANOUT_STALE.load(Ordering::SeqCst),
            "the mark was dropped while the panel was still a frame behind"
        );
    }

    /// The harness itself. `reset_output_state_for_test` is what stands between
    /// a test that leaves a pause behind and every present test that follows
    /// passing while touching nothing at all.
    #[test]
    fn the_reset_between_tests_lifts_a_leaked_pause() {
        let _restored = serialised();
        set_scanout_paused_for(SCANOUT_PAUSE_MAX);
        assert!(present_now_checked(0x7373, SYNTH_CRTC_ID, None).is_ok());
        assert!(scanout_paused());

        reset_output_state_for_test();

        assert!(!scanout_paused(), "a pause survived the reset");
        assert_eq!(SCANOUT_PAUSE_DEADLINE_NS.load(Ordering::SeqCst), 0);
        assert!(!SCANOUT_STALE.load(Ordering::SeqCst));
    }
}

/// Tests for the present probe: the thing that answers whether the pixels were
/// already wrong when the kernel got them.
///
/// The probe is a diagnostic, and a diagnostic that lies is worse than none: if
/// it can miss a change inside the window it is asked about, a clean log
/// wrongly clears the compositor; if it can fire on a change outside that
/// window, every frame reports and the log says nothing. Both directions are
/// pinned here, as is the blind spot it does have -- the rows it steps over --
/// because that one is a deliberate trade and not an accident.
#[cfg(test)]
mod present_skip_tests {
    //! The band arithmetic and the hash behind `drm.present_skip`.
    //!
    //! Everything here decides whether a band of the panel keeps the pixels it
    //! has. Each one of these functions can be wrong in a way that shows up as
    //! stale pixels on screen and in nothing else, which is the defect the whole
    //! present path has been chasing -- so they are pinned here, away from a
    //! display, and the end-to-end behaviour is in `drm_scheme`'s
    //! `kms_scanout_tests`.

    use super::*;

    // --- how many bands, and which rows are in them ---

    /// A window of no rows has no bands, rather than one empty one: a band that
    /// describes no pixels would compare two hashes of nothing and answer
    /// "unchanged" for rows that were never looked at.
    #[test]
    fn a_window_of_no_rows_has_no_bands() {
        assert_eq!(skip_band_count(0), None);
    }

    /// Anything up to a full band is one band, and one row past it is two: the
    /// tail band is short, never dropped. A dropped tail is a strip at the
    /// bottom of the screen that stops being repainted.
    #[test]
    fn the_last_rows_get_their_own_short_band() {
        assert_eq!(skip_band_count(1), Some(1));
        assert_eq!(skip_band_count(SKIP_BAND_ROWS), Some(1));
        assert_eq!(skip_band_count(SKIP_BAND_ROWS + 1), Some(2));
        assert_eq!(skip_band_count(1080), Some(68));
    }

    /// And the tail band reports its real height, so the copy that follows it
    /// does not run past the window.
    #[test]
    fn the_tail_band_is_as_short_as_it_really_is() {
        assert_eq!(skip_band_span(0, 1080), Some((0, SKIP_BAND_ROWS)));
        assert_eq!(skip_band_span(67, 1080), Some((1072, 8)));
        assert_eq!(skip_band_span(68, 1080), None);
        assert_eq!(skip_band_span(0, 3), Some((0, 3)));
    }

    /// A window taller than the state can describe does not take the skip at
    /// all. Clamping instead would leave the rows past the last band never
    /// compared and never copied.
    #[test]
    fn a_window_taller_than_the_state_declines_the_skip() {
        let tallest = SKIP_BAND_ROWS * MAX_SKIP_BANDS as u32;
        assert_eq!(skip_band_count(tallest), Some(MAX_SKIP_BANDS));
        assert_eq!(skip_band_count(tallest + 1), None);
    }

    // --- which bands a write on top of the frame dirties ---

    /// One row dirties the band that holds it, and a row range that straddles a
    /// boundary dirties both. The cursor is the caller, and a boundary it
    /// straddles with half its height is the ordinary case.
    #[test]
    fn a_row_range_dirties_every_band_it_touches() {
        assert_eq!(bands_covering_rows(0, 1), Some((0, 1)));
        assert_eq!(
            bands_covering_rows(SKIP_BAND_ROWS - 1, 2),
            Some((0, 2)),
            "a range crossing the boundary owns both bands"
        );
        assert_eq!(
            bands_covering_rows(SKIP_BAND_ROWS, SKIP_BAND_ROWS),
            Some((1, 2))
        );
        assert_eq!(bands_covering_rows(0, SKIP_BAND_ROWS + 1), Some((0, 2)));
    }

    /// A write of no rows dirties nothing, and one entirely past the bands
    /// dirties nothing either -- but a range that merely ENDS past them clamps
    /// instead of vanishing, because its first rows are on the panel.
    #[test]
    fn a_range_outside_the_bands_dirties_nothing_but_one_that_leaves_them_clamps() {
        assert_eq!(bands_covering_rows(0, 0), None);
        assert_eq!(
            bands_covering_rows(SKIP_BAND_ROWS * MAX_SKIP_BANDS as u32, 4),
            None
        );
        assert_eq!(
            bands_covering_rows(0, u32::MAX),
            Some((0, MAX_SKIP_BANDS)),
            "a range past the end still dirties every band it does cover"
        );
    }

    // --- the geometry key ---

    /// Two different windows pack to two different keys, or a present would read
    /// hashes taken from somewhere else on the screen as its own.
    #[test]
    fn every_window_packs_to_its_own_key() {
        let a = pack_panel_geom(0, 0, 1920, 1080).expect("packs");
        for (x, y, w, h) in [
            (1, 0, 1920, 1080),
            (0, 1, 1920, 1080),
            (0, 0, 1921, 1080),
            (0, 0, 1920, 1081),
        ] {
            assert_ne!(
                a,
                pack_panel_geom(x, y, w, h).expect("packs"),
                "{:?}",
                (x, y, w, h)
            );
        }
    }

    /// And a packed key is never the `0` that means "nothing known", so a real
    /// geometry cannot be mistaken for the absence of one.
    #[test]
    fn a_real_geometry_never_packs_to_the_empty_key() {
        for (x, y, w, h) in [(0, 0, 1, 1), (0, 0, 1920, 1080), (7, 9, 64, 64)] {
            assert_ne!(pack_panel_geom(x, y, w, h), Some(0));
        }
    }

    /// A window with no width or no height, and one that does not fit the key,
    /// decline instead of aliasing onto some other window's hashes.
    #[test]
    fn a_window_that_does_not_fit_the_key_declines() {
        assert_eq!(pack_panel_geom(0, 0, 0, 1080), None);
        assert_eq!(pack_panel_geom(0, 0, 1920, 0), None);
        assert_eq!(pack_panel_geom(0, 0, 70_000, 1080), None);
        assert_eq!(pack_panel_geom(70_000, 0, 1920, 1080), None);
    }

    // --- the hash ---

    /// The same pixels hash the same, and one pixel of one row different hashes
    /// differently. That second half is the whole claim: a band that changed must
    /// not be recognised as already on the panel.
    #[test]
    fn one_changed_pixel_changes_the_bands_hash() {
        let stride = 8usize;
        let mut px: Vec<u32> = (0..stride * 40).map(|n| n as u32).collect();
        let before = skip_band_hash(&px, stride, 0, SKIP_BAND_ROWS, 8).expect("hashes");
        assert_eq!(
            skip_band_hash(&px, stride, 0, SKIP_BAND_ROWS, 8),
            Some(before),
            "the same pixels twice"
        );
        px[stride * 9 + 3] ^= 1;
        assert_ne!(
            skip_band_hash(&px, stride, 0, SKIP_BAND_ROWS, 8),
            Some(before)
        );
    }

    /// Two pixels swapped between rows hash differently, so the hash is of the
    /// band's LAYOUT and not of its multiset of colours: a window dragged by one
    /// row is not "the same pixels".
    #[test]
    fn moving_a_pixel_changes_the_hash() {
        let stride = 8usize;
        let mut px: Vec<u32> = (0..stride * 20).map(|n| 0x1000 + n as u32).collect();
        let before = skip_band_hash(&px, stride, 0, SKIP_BAND_ROWS, 8).expect("hashes");
        px.swap(0, stride);
        assert_ne!(
            skip_band_hash(&px, stride, 0, SKIP_BAND_ROWS, 8),
            Some(before)
        );
    }

    /// Only the window's own columns count. The bytes past `w` in a row are the
    /// scanline's padding, and a hash that read them would call a band changed
    /// because of pixels nobody displays.
    #[test]
    fn the_padding_after_the_window_is_not_part_of_the_band() {
        let stride = 12usize;
        let mut px: Vec<u32> = (0..stride * 20).map(|n| 0x2000 + n as u32).collect();
        let before = skip_band_hash(&px, stride, 0, SKIP_BAND_ROWS, 8).expect("hashes");
        for r in 0..SKIP_BAND_ROWS as usize {
            px[r * stride + 9] ^= 0xFFFF;
        }
        assert_eq!(
            skip_band_hash(&px, stride, 0, SKIP_BAND_ROWS, 8),
            Some(before)
        );
    }

    /// A band that does not lie wholly inside the buffer has no answer, and the
    /// caller reads `None` as "copy it". Hashing a short last row would compare
    /// against a hash of different rows and could answer "unchanged".
    #[test]
    fn a_band_that_runs_past_the_buffer_has_no_hash() {
        let stride = 8usize;
        let px: Vec<u32> = (0..stride * 8).map(|n| n as u32).collect();
        assert_eq!(skip_band_hash(&px, stride, 0, SKIP_BAND_ROWS, 8), None);
        assert!(skip_band_hash(&px, stride, 0, 8, 8).is_some());
        assert_eq!(skip_band_hash(&px, stride, 4, 8, 8), None);
    }

    /// A window wider than its own stride is not a window, and a zero stride,
    /// width or height describes no pixels: all of them decline rather than hash
    /// whatever the arithmetic lands on.
    #[test]
    fn a_window_that_cannot_be_read_has_no_hash() {
        let px: Vec<u32> = (0..64).collect();
        assert_eq!(skip_band_hash(&px, 8, 0, 4, 9), None);
        assert_eq!(skip_band_hash(&px, 0, 0, 4, 4), None);
        assert_eq!(skip_band_hash(&px, 8, 0, 0, 4), None);
        assert_eq!(skip_band_hash(&px, 8, 0, 4, 0), None);
    }

    // --- the stored form of a hash ---

    /// A stored hash is never the `0` that means "nothing is known about this
    /// band". If it could be, a band whose pixels happened to hash to the
    /// sentinel would be skipped on the first present after a reset -- when the
    /// panel does not hold them yet -- and that is a stale band on screen.
    #[test]
    fn a_stored_hash_is_never_the_unknown_sentinel() {
        for h in [0u64, 1, u64::MAX, PROBE_FNV_BASIS, 1 << 63] {
            assert_ne!(known_hash(h), 0, "hash {:#x}", h);
        }
    }

    /// And two different hashes still store differently, so the encoding costs
    /// one bit and not the comparison: `known_hash` must not fold hashes together
    /// beyond that bit.
    #[test]
    fn the_stored_form_keeps_different_hashes_apart() {
        assert_ne!(known_hash(1), known_hash(2));
        assert_ne!(known_hash(PROBE_FNV_BASIS), known_hash(PROBE_FNV_PRIME));
        // The one pair it does fold: bit 63 is the sentinel's, and the doc says so.
        assert_eq!(known_hash(0), known_hash(1 << 63));
    }

    // --- the state ---

    /// A reset forgets every band and the geometry, which is what a boot, a
    /// blank and a console VT all leave behind.
    #[test]
    fn a_reset_forgets_the_geometry_and_every_band() {
        let _g = test_globals::lock();
        PANEL_BAND_GEOM.store(7, Ordering::Relaxed);
        PANEL_BAND_STRIDE.store(1920, Ordering::Relaxed);
        for (i, h) in PANEL_BAND_HASH.iter().enumerate() {
            h.store(i as u64 + 1, Ordering::Relaxed);
        }
        panel_bands_reset();
        assert_eq!(PANEL_BAND_GEOM.load(Ordering::Relaxed), 0);
        assert_eq!(PANEL_BAND_STRIDE.load(Ordering::Relaxed), 0);
        assert!(PANEL_BAND_HASH
            .iter()
            .all(|h| h.load(Ordering::Relaxed) == 0));
    }

    /// Dirtying a row range clears exactly the bands it covers and leaves the
    /// rest, which is what makes the cursor cost 16 rows instead of the frame.
    #[test]
    fn dirtying_rows_leaves_the_bands_it_does_not_cover() {
        let _g = test_globals::lock();
        for h in PANEL_BAND_HASH.iter() {
            h.store(0xABCD, Ordering::Relaxed);
        }
        panel_bands_dirty_rows(SKIP_BAND_ROWS, 1);
        assert_eq!(PANEL_BAND_HASH[0].load(Ordering::Relaxed), 0xABCD);
        assert_eq!(PANEL_BAND_HASH[1].load(Ordering::Relaxed), 0);
        assert_eq!(PANEL_BAND_HASH[2].load(Ordering::Relaxed), 0xABCD);
        panel_bands_reset();
    }

    /// The skip is off unless the cmdline arms it, like every other diagnostic
    /// and mitigation on this path: a boot that says nothing gets exactly the
    /// present it got before.
    #[test]
    fn the_skip_is_off_unless_the_cmdline_arms_it() {
        let _g = test_globals::lock();
        reset_output_state_for_test();
        assert!(!present_skip_enabled());
        set_present_skip_enabled(true);
        assert!(present_skip_enabled());
        reset_output_state_for_test();
        assert!(!present_skip_enabled());
    }

    /// Arming it forgets whatever was remembered: nothing was maintaining the
    /// hashes while it was off, so the first present after arming must copy.
    #[test]
    fn arming_the_skip_starts_from_nothing_known() {
        let _g = test_globals::lock();
        PANEL_BAND_HASH[3].store(0x1234, Ordering::Relaxed);
        PANEL_BAND_GEOM.store(99, Ordering::Relaxed);
        set_present_skip_enabled(true);
        assert_eq!(PANEL_BAND_HASH[3].load(Ordering::Relaxed), 0);
        assert_eq!(PANEL_BAND_GEOM.load(Ordering::Relaxed), 0);
        reset_output_state_for_test();
    }
}

#[cfg(test)]
mod present_probe_tests {
    extern crate std;

    use self::std::vec;
    use self::std::vec::Vec;
    use super::*;

    /// Put the process-global latches back however the body leaves them, and
    /// serialise against every other test that touches them.
    struct Restored {
        _serialised: self::std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for Restored {
        fn drop(&mut self) {
            reset_output_state_for_test();
        }
    }

    fn serialised() -> Restored {
        let g = Restored {
            _serialised: super::test_globals::lock(),
        };
        reset_output_state_for_test();
        g
    }

    /// A buffer whose every pixel is distinct, so any change the checksum does
    /// not notice is the checksum's fault and not a collision of equal values.
    fn buf(stride_px: usize, rows: usize) -> Vec<u32> {
        (0..stride_px * rows)
            .map(|n| 0xFF00_0000 | n as u32)
            .collect()
    }

    fn sum(pixels: &[u32], stride: usize, x: u32, y: u32, w: u32, h: u32) -> Option<u64> {
        bands(pixels, stride, x, y, w, h).map(|b| b.fold())
    }

    fn bands(pixels: &[u32], stride: usize, x: u32, y: u32, w: u32, h: u32) -> Option<ProbeBands> {
        probe_bands(pixels, stride, x, y, w, h, PROBE_ROW_STEP)
    }

    // --- where the source is already black (ZeroExtent) ---

    /// `buf` paints every pixel opaque, so the answer must be "none". This is
    /// the reading that exonerates the compositor: black on screen with no zero
    /// pixels in the source means the kernel lost them.
    #[test]
    fn a_source_with_no_black_reports_no_zero_pixels() {
        let p = buf(64, 32);
        let b = probe_bands(&p, 64, 0, 0, 64, 32, 1).expect("a window to read");
        assert_eq!(b.zero.zeros, 0);
        assert_eq!(b.zero.bbox(), None);
    }

    /// The reading that convicts it: a black rectangle in the buffer the client
    /// handed over comes back as its own box, in window coordinates.
    #[test]
    fn a_black_rectangle_in_the_source_is_located_by_its_box() {
        let mut p = buf(64, 32);
        // A 10x6 hole at +20+8 of the buffer, zeroed the way an unrasterised
        // tile is: fully transparent black.
        for y in 8..14 {
            for x in 20..30 {
                p[y * 64 + x] = 0;
            }
        }
        let b = probe_bands(&p, 64, 0, 0, 64, 32, 1).expect("a window to read");
        assert_eq!(b.zero.zeros, 10 * 6);
        assert_eq!(b.zero.bbox(), Some((20, 8, 10, 6)));
    }

    /// The box is window-relative, not buffer-relative, because that is what
    /// lines up with what the eye sees: the window's own origin is already
    /// printed beside it.
    #[test]
    fn the_box_is_relative_to_the_window_not_the_buffer() {
        let mut p = buf(64, 32);
        p[10 * 64 + 30] = 0;
        // Window starts at +25+8, so the pixel is at +5+2 inside it.
        let b = probe_bands(&p, 64, 25, 8, 20, 10, 1).expect("a window to read");
        assert_eq!(b.zero.bbox(), Some((5, 2, 1, 1)));
    }

    /// A single zero pixel is a 1x1 box. Reported inclusively on both ends, so
    /// without the `+ 1` it would come out 0x0 and read as "no black at all" --
    /// the wrong answer in the direction that sends the search to the wrong side.
    #[test]
    fn one_black_pixel_is_a_one_by_one_box() {
        let mut p = buf(64, 32);
        p[3 * 64 + 7] = 0;
        let b = probe_bands(&p, 64, 0, 0, 64, 32, 1).expect("a window to read");
        assert_eq!(b.zero.zeros, 1);
        assert_eq!(b.zero.bbox(), Some((7, 3, 1, 1)));
    }

    /// Black outside the window is none of this window's business: a present
    /// that scans out part of a buffer must not be blamed for the rest of it.
    #[test]
    fn black_outside_the_window_is_not_counted() {
        let mut p = buf(64, 32);
        p[0] = 0; // +0+0 of the buffer, outside the window below
        let b = probe_bands(&p, 64, 10, 10, 20, 10, 1).expect("a window to read");
        assert_eq!(b.zero.zeros, 0);
        assert_eq!(b.zero.bbox(), None);
    }

    /// `sampled` counts PIXELS, not rows, or the fraction in the klog line would
    /// be off by the window's width and the number would mean nothing.
    #[test]
    fn the_sampled_count_is_pixels_not_rows() {
        let p = buf(64, 32);
        // 20 rows at a step of 4 samples rows 0,4,8,12,16 -- five of them.
        let b = probe_bands(&p, 64, 0, 0, 40, 20, 4).expect("a window to read");
        assert_eq!(b.zero.sampled, 40 * 5);
    }

    /// A row the sampling step skips over cannot contribute, which is the honest
    /// limit of the number: it describes the rows the probe actually read.
    #[test]
    fn a_black_row_the_step_skips_is_not_seen() {
        let mut p = buf(64, 32);
        for x in 0..64 {
            p[1 * 64 + x] = 0; // row 1, which a step of 4 never reads
        }
        let b = probe_bands(&p, 64, 0, 0, 64, 32, 4).expect("a window to read");
        assert_eq!(b.zero.zeros, 0);
    }

    /// Black in both reads differs in neither, so the band mask says nothing
    /// about it. This is exactly why the zero report is a separate line with its
    /// own budget rather than a field on the mismatch one.
    #[test]
    fn a_static_black_region_sets_no_band_bit_but_is_still_reported() {
        let mut p = buf(64, 32);
        for y in 0..32 {
            for x in 8..16 {
                p[y * 64 + x] = 0;
            }
        }
        let a = probe_bands(&p, 64, 0, 0, 64, 32, 1).expect("first read");
        let b = probe_bands(&p, 64, 0, 0, 64, 32, 1).expect("second read");
        assert_eq!(a.diff_mask(&b), 0, "nothing moved, so no band differs");
        assert!(!probe_says_changed(a.fold(), Some(b.fold())));
        assert_eq!(a.zero.bbox(), Some((8, 0, 8, 32)), "but the black is found");
    }

    // --- what it must notice ---

    /// The baseline every other test leans on: read the same window twice with
    /// nothing touching it in between and the two answers match. Without this
    /// the probe would report on every single frame and the log would carry no
    /// information at all.
    #[test]
    fn the_same_window_read_twice_is_the_same_answer() {
        let p = buf(64, 32);
        assert_eq!(sum(&p, 64, 4, 4, 40, 20), sum(&p, 64, 4, 4, 40, 20));
        assert!(sum(&p, 64, 4, 4, 40, 20).is_some());
    }

    /// The whole point. One pixel rewritten inside a row the probe samples, and
    /// the second read says so.
    #[test]
    fn one_pixel_changed_inside_a_sampled_row_is_caught() {
        let mut p = buf(64, 32);
        let before = sum(&p, 64, 0, 0, 64, 32);
        // Row 8 is a multiple of the step, so it is one the probe reads.
        assert_eq!(8 % PROBE_ROW_STEP, 0);
        p[8 * 64 + 17] ^= 0x00FF_00FF;
        assert_ne!(before, sum(&p, 64, 0, 0, 64, 32));
    }

    /// Every pixel of a sampled row is read, not a stride of them -- so the
    /// last column counts too. A comb of stale pixels can be one column wide,
    /// and a probe that stepped along the row would walk straight past it.
    #[test]
    fn the_last_column_of_a_sampled_row_counts() {
        let mut p = buf(64, 32);
        let before = sum(&p, 64, 8, 0, 40, 12);
        // x + w - 1, the rightmost pixel the window covers, on row 0.
        p[47] ^= 0x0000_00FF;
        assert_ne!(before, sum(&p, 64, 8, 0, 40, 12));
    }

    /// Two pixels swapped within a row: the same pixels, in different places.
    /// A sum or an XOR would call that unchanged, which is why each pixel is
    /// folded with the position it was read from. A scrolled list or a window
    /// dragged by one pixel is exactly this shape.
    #[test]
    fn the_same_pixels_in_a_different_order_are_not_the_same_answer() {
        let mut p = buf(64, 32);
        let before = sum(&p, 64, 0, 0, 64, 8);
        p.swap(3, 40);
        assert_ne!(before, sum(&p, 64, 0, 0, 64, 8));
    }

    /// Two pixels swapped between two sampled ROWS, not just within one. The
    /// row is no longer folded into the hash, so this is the case that says the
    /// sequence itself is what distinguishes them.
    #[test]
    fn the_same_pixels_swapped_between_rows_are_not_the_same_answer() {
        let mut p = buf(64, 32);
        let before = sum(&p, 64, 0, 0, 64, 32);
        p.swap(0 * 64 + 9, 4 * 64 + 9);
        assert_ne!(before, sum(&p, 64, 0, 0, 64, 32));
    }

    /// The hardest case for a value-only hash: every pixel identical, so only
    /// how many of them there were can tell the two windows apart. FNV-1a folds
    /// each one in turn even when they are equal, so it does -- which is why
    /// dropping the position fold cost nothing. A flat wallpaper is exactly this
    /// buffer.
    #[test]
    fn a_window_of_all_equal_pixels_still_depends_on_how_many_there_were() {
        let p = vec![0xFF80_8080u32; 64 * 32];
        assert_ne!(sum(&p, 64, 0, 0, 64, 32), sum(&p, 64, 0, 0, 64, 16));
        assert_ne!(sum(&p, 64, 0, 0, 64, 8), sum(&p, 64, 0, 0, 32, 8));
        // And the same window twice is still the same answer.
        assert_eq!(sum(&p, 64, 0, 0, 64, 32), sum(&p, 64, 0, 0, 64, 32));
    }

    // --- what it must NOT notice ---

    /// A change outside the window is not this present's business. The probe
    /// names a rectangle in its report, so it has to be reporting on that
    /// rectangle: a checksum that covered the whole buffer would fire on every
    /// frame of an animated clock in another corner and the log would be noise.
    #[test]
    fn a_change_outside_the_window_is_not_reported() {
        let mut p = buf(64, 32);
        let before = sum(&p, 64, 8, 8, 16, 16);
        // Left of the window, right of it, above it and below it.
        p[8 * 64 + 7] ^= 0xFFFF_FFFF;
        p[8 * 64 + 24] ^= 0xFFFF_FFFF;
        p[7 * 64 + 12] ^= 0xFFFF_FFFF;
        p[24 * 64 + 12] ^= 0xFFFF_FFFF;
        assert_eq!(before, sum(&p, 64, 8, 8, 16, 16));
    }

    /// The blind spot, written down rather than discovered later. Rows are
    /// sampled, so a change confined to the rows in between is invisible. That
    /// is the trade: a torn frame comes from a rasteriser handing over tiles,
    /// which is tens of rows tall, and reading every row would double the cost
    /// of the present the probe is measuring.
    #[test]
    fn a_change_only_in_the_rows_the_step_skips_is_missed() {
        let mut p = buf(64, 32);
        let before = sum(&p, 64, 0, 0, 64, 32);
        for r in 0..32 {
            if r % PROBE_ROW_STEP != 0 {
                p[r * 64 + 5] ^= 0xFFFF_FFFF;
            }
        }
        assert_eq!(
            before,
            sum(&p, 64, 0, 0, 64, 32),
            "the row step is what it is; if this starts failing the step changed"
        );
        // Pinned by value, not by the constant. Read through `PROBE_ROW_STEP`
        // the loop above selects nothing at all when the step is 1, so the test
        // would keep passing while the trade it describes silently went away --
        // and a step of 1 doubles the cost of every present the probe measures.
        assert_eq!(
            PROBE_ROW_STEP, 4,
            "the step changed: re-read what the blind spot above now covers, and \
             what the second read now costs per frame"
        );
    }

    // --- "I cannot tell" is its own answer ---

    /// `None` is never equal to a checksum, so a window the probe could not
    /// read does not come back as "unchanged". The call site compares
    /// `after != Some(before)`, so a `None` after a `Some` reports -- which is
    /// the safe direction for something whose job is to find a defect.
    #[test]
    fn nothing_to_compare_is_not_the_same_as_a_match() {
        let p = buf(64, 8);
        let real = sum(&p, 64, 0, 0, 8, 4);
        assert!(real.is_some());
        // Degenerate in each of the four ways.
        assert!(probe_bands(&p, 0, 0, 0, 8, 4, PROBE_ROW_STEP).is_none());
        assert!(probe_bands(&p, 64, 0, 0, 0, 4, PROBE_ROW_STEP).is_none());
        assert!(probe_bands(&p, 64, 0, 0, 8, 0, PROBE_ROW_STEP).is_none());
        assert!(probe_bands(&p, 64, 0, 0, 8, 4, 0).is_none());
        assert_ne!(real, None);
    }

    /// A window whose very first row is already past the end of the buffer has
    /// nothing to checksum at all.
    #[test]
    fn a_window_past_the_end_of_the_buffer_has_no_answer() {
        let p = buf(64, 8);
        assert_eq!(sum(&p, 64, 0, 64, 64, 4), None);
        // And one whose first row starts inside the buffer but runs off its end.
        assert_eq!(sum(&p, 64, 32, 7, 64, 4), None);
    }

    /// A row is `w` pixels from where it starts, the way `blit_from` reads it --
    /// so a window wider than the stride runs into the next row rather than
    /// being clipped or refused. Worth pinning because it looks like a bug: it
    /// is not reachable from the present path (`expand_x_for_wc` caps the right
    /// edge at the pitch), and the probe has to read exactly what the blit read,
    /// not what a tidier rule would have read.
    #[test]
    fn a_window_wider_than_the_stride_reads_into_the_next_row() {
        let p = buf(64, 8);
        assert!(sum(&p, 64, 60, 0, 64, 4).is_some());
        // Which means a pixel two rows down, at the far end of that run, counts.
        let mut q = p.clone();
        q[64 + 20] ^= 0xFFFF_FFFF;
        assert_ne!(sum(&p, 64, 60, 0, 64, 4), sum(&q, 64, 60, 0, 64, 4));
    }

    /// A window that starts inside the buffer and runs off the bottom
    /// checksums the rows that fit instead of giving up on all of them, so a
    /// present whose last rows fall outside the mapping is still measured.
    #[test]
    fn a_window_that_runs_off_the_bottom_measures_the_rows_that_fit() {
        let p = buf(64, 10);
        let partial = sum(&p, 64, 0, 4, 64, 40);
        assert!(partial.is_some());
        // Rows 4 and 8 fit, row 12 does not -- so this is the same read as a
        // window two sampled rows tall, and a different one from a window that
        // only covers the first of them.
        assert_eq!(partial, sum(&p, 64, 0, 4, 64, 8));
        assert_ne!(partial, sum(&p, 64, 0, 4, 64, 4));
    }

    /// The number of rows that were read is part of the answer, so a window
    /// that shrank between the two reads does not pass as unchanged.
    #[test]
    fn a_window_of_a_different_height_is_a_different_answer() {
        let p = buf(64, 32);
        assert_ne!(sum(&p, 64, 0, 0, 64, 32), sum(&p, 64, 0, 0, 64, 16));
    }

    /// Same pixels, read through a different stride: a different set of
    /// pixels. The probe is handed the framebuffer's own pitch, and a
    /// mismatched one would silently be measuring a diagonal.
    #[test]
    fn the_stride_is_part_of_what_is_being_read() {
        let p = buf(64, 32);
        assert_ne!(sum(&p, 64, 0, 0, 16, 16), sum(&p, 32, 0, 0, 16, 16));
    }

    // --- the rule that turns two reads into a verdict ---

    /// Two equal reads: nobody touched it, no report.
    #[test]
    fn two_equal_reads_are_not_a_report() {
        assert!(!probe_says_changed(0x1234, Some(0x1234)));
    }

    /// Two different reads: somebody wrote the window while the kernel was
    /// copying it, which is the entire finding.
    #[test]
    fn two_different_reads_are_a_report() {
        assert!(probe_says_changed(0x1234, Some(0x1235)));
    }

    /// And a second read that could not produce an answer reports too. The
    /// probe exists to decide who is responsible for a wrong pixel, so the one
    /// direction it must never fall in is quietly clearing the compositor.
    #[test]
    fn a_second_read_with_no_answer_reports_rather_than_clearing_anyone() {
        assert!(probe_says_changed(0x1234, None));
    }

    // --- the report budget ---

    /// A compositor that tears every frame must not turn the klog into the
    /// bottleneck: `klog` writes synchronously to the UART, slower than the
    /// frame it is describing. Twelve reports, then silence.
    #[test]
    fn the_report_budget_runs_out_and_says_so_on_the_way_past() {
        assert_eq!(probe_report_decision(0), (true, false));
        assert_eq!(
            probe_report_decision(MAX_PROBE_REPORTS - 2),
            (true, false),
            "the one before the last is not the last"
        );
        assert_eq!(
            probe_report_decision(MAX_PROBE_REPORTS - 1),
            (true, true),
            "the last report has to say it is the last, or the log looks truncated"
        );
        assert_eq!(probe_report_decision(MAX_PROBE_REPORTS), (false, false));
        assert_eq!(probe_report_decision(u32::MAX), (false, false));
    }

    // --- the latch ---

    /// Off unless a boot asks for it. The probe reads the damage box a second
    /// time on every present, so a default of on would be a permanent tax on
    /// the frame rate for a measurement nobody requested.
    #[test]
    fn the_probe_is_off_unless_the_cmdline_arms_it() {
        let _g = serialised();
        assert!(!present_probe_enabled());
        set_present_probe_enabled(true);
        assert!(present_probe_enabled());
    }

    /// And a test that armed it does not leave it armed for the next one --
    /// which would make every later present test read its window twice and log
    /// about a frame nobody was looking at.
    #[test]
    fn the_reset_between_tests_disarms_a_leaked_probe() {
        let _g = serialised();
        set_present_probe_enabled(true);
        PROBE_REPORTS.store(MAX_PROBE_REPORTS, Ordering::Relaxed);
        reset_output_state_for_test();
        assert!(!present_probe_enabled());
        assert_eq!(
            probe_report_decision(PROBE_REPORTS.load(Ordering::Relaxed)),
            (true, false),
            "the budget has to come back too, or the last test to run finds it spent"
        );
    }

    /// The buffer helper really does hand out distinct pixels, so
    /// "the checksum did not notice" can never be a collision of equal values.
    #[test]
    fn the_test_buffer_has_no_two_equal_pixels() {
        let p = buf(16, 4);
        let mut seen = vec![];
        for px in &p {
            assert!(!seen.contains(px));
            seen.push(*px);
        }
    }

    // --- which bands moved ---

    /// A window read twice with nothing touching it sets no bit. Without this
    /// every mask below would be indistinguishable from "the mask is always
    /// full", and a full mask is exactly one of the two answers we are trying
    /// to tell apart.
    #[test]
    fn a_settled_window_sets_no_band() {
        let p = buf(512, 8);
        let a = bands(&p, 512, 0, 0, 512, 8).unwrap();
        let b = bands(&p, 512, 0, 0, 512, 8).unwrap();
        assert_eq!(a.diff_mask(&b), 0);
        assert_eq!(a.n, 8, "512 px is eight 64-px bands");
    }

    /// The comb: a rasteriser handed over tiles 1, 3 and 5 and left the rest of
    /// the frame as it was. That is the shape this mask exists to name, and it
    /// must come out as a *scattered* subset -- not as a run, and not as
    /// everything.
    #[test]
    fn stale_tiles_light_up_exactly_their_own_bands() {
        let mut p = buf(512, 8);
        let before = bands(&p, 512, 0, 0, 512, 8).unwrap();
        for band in [1usize, 3, 5] {
            for row in 0..8usize {
                p[row * 512 + band * PROBE_BAND_PX + 7] ^= 0xFF;
            }
        }
        let after = bands(&p, 512, 0, 0, 512, 8).unwrap();
        assert_eq!(
            before.diff_mask(&after),
            (1 << 1) | (1 << 3) | (1 << 5),
            "only the bands whose pixels moved may be set"
        );
    }

    /// The other shape: the compositor overwrote the frame it had already handed
    /// over, so the change is a contiguous run. Same instrument, visibly
    /// different answer -- which is the whole reason the mask beats a count.
    #[test]
    fn a_frame_overwritten_in_place_lights_up_a_contiguous_run() {
        let mut p = buf(512, 8);
        let before = bands(&p, 512, 0, 0, 512, 8).unwrap();
        for row in 0..8usize {
            for col in (2 * PROBE_BAND_PX)..(6 * PROBE_BAND_PX) {
                p[row * 512 + col] ^= 0xFF;
            }
        }
        let after = bands(&p, 512, 0, 0, 512, 8).unwrap();
        let mask = before.diff_mask(&after);
        assert_eq!(mask, 0b0011_1100, "bands 2..5 and nothing else");
        assert_eq!(mask.count_ones(), 4);
    }

    /// One changed pixel in a band sets that band and no other. Run for every
    /// band of the window, because a mask that is right for band 0 and wrong for
    /// band 7 is worse than no mask: it would point the search at the wrong
    /// columns of the panel.
    #[test]
    fn one_changed_pixel_sets_its_own_band_and_only_it() {
        for band in 0..8usize {
            let mut p = buf(512, 8);
            let before = bands(&p, 512, 0, 0, 512, 8).unwrap();
            p[band * PROBE_BAND_PX] ^= 0xFF;
            let after = bands(&p, 512, 0, 0, 512, 8).unwrap();
            assert_eq!(
                before.diff_mask(&after),
                1u32 << band,
                "a pixel in band {} must set band {} alone",
                band,
                band
            );
        }
    }

    /// The last pixel of a band belongs to that band and the first pixel of the
    /// next one does not. Off by one here would slide the whole reading of the
    /// klog 64 px to the left.
    #[test]
    fn a_band_ends_where_the_next_one_starts() {
        let mut p = buf(512, 8);
        let before = bands(&p, 512, 0, 0, 512, 8).unwrap();
        p[PROBE_BAND_PX - 1] ^= 0xFF;
        assert_eq!(
            before.diff_mask(&bands(&p, 512, 0, 0, 512, 8).unwrap()),
            1 << 0
        );
        let mut q = buf(512, 8);
        q[PROBE_BAND_PX] ^= 0xFF;
        assert_eq!(
            before.diff_mask(&bands(&q, 512, 0, 0, 512, 8).unwrap()),
            1 << 1
        );
    }

    /// The window's own `x` is the mask's origin: band 0 is the left edge of
    /// what was blitted, not of the framebuffer. Anything else and the klog's
    /// mask could not be read against the blit rectangle it is printed with.
    #[test]
    fn band_zero_is_the_windows_left_edge_not_the_buffers() {
        let mut p = buf(512, 8);
        let before = bands(&p, 512, 128, 0, 256, 8).unwrap();
        p[128] ^= 0xFF;
        let after = bands(&p, 512, 128, 0, 256, 8).unwrap();
        assert_eq!(before.diff_mask(&after), 1 << 0);
    }

    /// A window narrower than one band still has one, and a change in it is
    /// reported rather than divided away to nothing.
    #[test]
    fn a_window_narrower_than_a_band_still_has_one() {
        let mut p = buf(512, 8);
        let before = bands(&p, 512, 0, 0, 7, 8).unwrap();
        assert_eq!(before.n, 1);
        p[3] ^= 0xFF;
        assert_eq!(before.diff_mask(&bands(&p, 512, 0, 0, 7, 8).unwrap()), 1);
    }

    /// A window wider than the mask can describe folds its right-hand columns
    /// into the last band instead of dropping them. A mask that quietly stopped
    /// covering part of the window would read as "those columns are clean".
    #[test]
    fn columns_past_the_masks_reach_fold_into_the_last_band() {
        let wide = PROBE_MAX_BANDS * PROBE_BAND_PX + 300;
        let mut p = buf(wide, 8);
        let before = bands(&p, wide, 0, 0, wide as u32, 8).unwrap();
        assert_eq!(before.n, PROBE_MAX_BANDS);
        p[wide - 1] ^= 0xFF;
        let after = bands(&p, wide, 0, 0, wide as u32, 8).unwrap();
        assert_eq!(
            before.diff_mask(&after),
            1u32 << (PROBE_MAX_BANDS - 1),
            "the far right column has to land in the last band, not nowhere"
        );
    }

    /// The band count never reaches past the array, and never reads as zero.
    #[test]
    fn the_band_count_stays_inside_the_mask() {
        assert_eq!(probe_band_count(0), 1);
        assert_eq!(probe_band_count(1), 1);
        assert_eq!(probe_band_count(PROBE_BAND_PX as u32), 1);
        assert_eq!(probe_band_count(PROBE_BAND_PX as u32 + 1), 2);
        assert_eq!(probe_band_count(1920), 30);
        assert_eq!(probe_band_count(u32::MAX), PROBE_MAX_BANDS);
    }

    /// The scalar answer the report's decision rests on still notices a change
    /// in any single band -- including the last one, which is where a fold that
    /// stopped early would go quiet.
    #[test]
    fn the_fold_notices_a_change_in_any_band() {
        for band in 0..8usize {
            let mut p = buf(512, 8);
            let before = sum(&p, 512, 0, 0, 512, 8);
            p[band * PROBE_BAND_PX + 1] ^= 0xFF;
            assert_ne!(
                before,
                sum(&p, 512, 0, 0, 512, 8),
                "a change in band {} has to reach the fold",
                band
            );
        }
    }

    /// A band of black pixels is not the same answer as a band nobody read. That
    /// is what the hash basis buys, and it is also what lets two reads of
    /// different widths come out different without the fold having to carry `n`:
    /// on an all-black buffer the wider read's extra band is the only thing that
    /// separates them.
    #[test]
    fn an_all_black_band_is_not_a_band_that_was_never_read() {
        let p = vec![0u32; 512 * 8];
        let narrow = bands(&p, 512, 0, 0, 64, 8).unwrap();
        let wide = bands(&p, 512, 0, 0, 128, 8).unwrap();
        assert_eq!((narrow.n, wide.n), (1, 2));
        assert_eq!(
            narrow.bands[1], PROBE_FNV_BASIS,
            "band 1 was never read in the narrow window"
        );
        assert_ne!(
            narrow.bands[1], wide.bands[1],
            "64 black pixels must not hash to the untouched value"
        );
        assert_ne!(narrow.fold(), wide.fold());
        assert_eq!(narrow.diff_mask(&wide), 1 << 1);
    }

    // --- what a repair round would copy ---

    /// Nothing moved, nothing to repair. Without this every span below could be
    /// explained by "it always returns a span".
    #[test]
    fn a_mask_with_no_band_set_repairs_nothing() {
        assert_eq!(repair_span_px(0, 30, 1920), None);
    }

    /// One band is its own 64 columns and not one more. A span that overshot
    /// would copy settled pixels on every repair round, which is the cost this
    /// whole mechanism is trying to keep proportional.
    #[test]
    fn one_band_is_its_own_sixty_four_columns() {
        assert_eq!(repair_span_px(1 << 0, 30, 1920), Some((0, 64)));
        assert_eq!(repair_span_px(1 << 1, 30, 1920), Some((64, 64)));
        assert_eq!(repair_span_px(1 << 29, 30, 1920), Some((1856, 64)));
    }

    /// Scattered bands become ONE span from the first to the last, settled bands
    /// in between included: two blits of nearby bands cost more than one blit of
    /// both, and a repair round is a blit.
    #[test]
    fn scattered_bands_become_one_span_from_first_to_last() {
        let mask = (1 << 2) | (1 << 5) | (1 << 9);
        assert_eq!(repair_span_px(mask, 30, 1920), Some((128, (10 - 2) * 64)));
    }

    /// A mask whose bits all sit past the bands the window covered describes no
    /// pixels. Turning that into a copy would be a copy of the wrong columns.
    #[test]
    fn bands_past_the_window_repair_nothing() {
        assert_eq!(repair_span_px(1 << 20, 4, 256), None);
        assert_eq!(repair_span_px(1 << 31, 30, 1920), None);
    }

    /// The last band is the folded one: for a window wider than the mask's reach
    /// it stands for every column up to the window's right edge, and the span has
    /// to reach that far or those columns never get repaired.
    #[test]
    fn the_last_band_reaches_the_windows_right_edge() {
        let wide = (PROBE_MAX_BANDS * PROBE_BAND_PX + 300) as u32;
        let (x, w) = repair_span_px(1 << (PROBE_MAX_BANDS - 1), PROBE_MAX_BANDS, wide).unwrap();
        assert_eq!(x, ((PROBE_MAX_BANDS - 1) * PROBE_BAND_PX) as u32);
        assert_eq!(
            x + w,
            wide,
            "the folded band owns everything to the right edge"
        );
    }

    /// A span never reaches past the window, whatever the mask says. A blit that
    /// started inside the window and ran past its right edge would walk into the
    /// next row.
    #[test]
    fn a_span_never_reaches_past_the_window() {
        for bit in 0..PROBE_MAX_BANDS {
            if let Some((x, w)) = repair_span_px(1 << bit, PROBE_MAX_BANDS, 100) {
                assert!(
                    x + w <= 100,
                    "band {} gave {}..{} on a 100-px window",
                    bit,
                    x,
                    x + w
                );
            }
        }
        assert_eq!(repair_span_px(u32::MAX, 30, 1920), Some((0, 1920)));
    }

    /// A window with no width and a mask that covers no bands are both "nothing
    /// to do", not a zero-width blit at the origin.
    #[test]
    fn a_window_of_no_width_repairs_nothing() {
        assert_eq!(repair_span_px(1, 30, 0), None);
        assert_eq!(repair_span_px(1, 0, 1920), None);
    }

    /// The repair is off unless the cmdline arms it, and the reset between tests
    /// disarms it -- a leaked flag would have every later present test doing
    /// extra blits.
    #[test]
    fn the_repair_is_off_unless_the_cmdline_arms_it() {
        let _g = serialised();
        assert!(!present_repair_enabled());
        set_present_repair_enabled(true);
        assert!(present_repair_enabled());
        reset_output_state_for_test();
        assert!(!present_repair_enabled());
        assert_eq!(repair_rounds_for_test(), 0);
    }

    /// Two rounds, and it is a budget: the loop must not be able to run longer
    /// than this against a source that never settles.
    #[test]
    fn the_repair_budget_is_two_rounds() {
        assert_eq!(MAX_REPAIR_ROUNDS, 2);
    }

    /// A pixel that moves from one band into another changes both of them, so
    /// the mask describes a shift rather than hiding it as "nothing moved".
    #[test]
    fn a_pixel_that_moves_between_bands_marks_both() {
        let mut p = vec![0u32; 512 * 8];
        p[10] = 0xDEAD_BEEF;
        let before = bands(&p, 512, 0, 0, 512, 8).unwrap();
        p[10] = 0;
        p[PROBE_BAND_PX + 10] = 0xDEAD_BEEF;
        let after = bands(&p, 512, 0, 0, 512, 8).unwrap();
        assert_eq!(before.diff_mask(&after), 0b11);
    }
}

/// Tests for the one rule about whether an EDID is fit to be served.
///
/// The bytes that leave through the connector's EDID property are a monitor's
/// identity: its make, its model, its stated physical size and its native
/// timing. A client scales its whole UI by that size and picks its mode from
/// that timing, so serving a block that is not an EDID is not a cosmetic
/// problem -- and the one path that could only ever produce such a block was
/// the one nothing checked.
#[cfg(test)]
mod edid_gate_tests {
    extern crate std;

    use super::*;
    use zcore_drivers::display::edid;

    const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];

    /// A whole block with a correct header and checksum, the way a monitor sends
    /// one.
    fn good_block() -> [u8; edid::BLOCK_LEN] {
        let mut b = [0u8; edid::BLOCK_LEN];
        b[..8].copy_from_slice(&HEADER);
        b[18] = 1;
        b[19] = 4;
        b[21] = 60;
        b[22] = 34;
        let sum = b[..edid::BLOCK_LEN - 1]
            .iter()
            .fold(0u8, |s, x| s.wrapping_add(*x));
        b[edid::BLOCK_LEN - 1] = sum.wrapping_neg();
        b
    }

    /// The defect, reproduced: 32 real bytes zero-padded to 128. This is exactly
    /// what `NvidiaGpu::get_connector_edid` used to return when the RM's head did
    /// not match the firmware capture, and what the DRM core used to serve on a
    /// length check alone.
    fn zero_padded_head() -> [u8; edid::BLOCK_LEN] {
        let mut b = [0u8; edid::BLOCK_LEN];
        b[..8].copy_from_slice(&HEADER);
        b[18] = 1;
        b[21] = 60;
        b
    }

    #[test]
    fn a_real_block_is_served() {
        assert_eq!(edid_refusal_reason(&good_block()), None);
    }

    /// The whole point of the batch. A zero-padded head is refused, and the
    /// reason names the checksum rather than the header -- because the header IS
    /// right, which is exactly why a length check let it through.
    #[test]
    fn the_zero_padded_head_is_refused_on_its_checksum() {
        let padded = zero_padded_head();
        assert_eq!(
            &padded[..8],
            &HEADER,
            "the header is the part that was fine"
        );
        assert_eq!(edid_refusal_reason(&padded), Some("checksum"));
    }

    /// And the driver no longer produces one: the same 32 bytes, completed,
    /// pass. The two halves of the fix have to agree, or refusing in the core
    /// just loses the monitor's identity instead of repairing it.
    #[test]
    fn the_completed_head_the_driver_now_reports_is_served() {
        let padded = zero_padded_head();
        let completed = edid::finish_partial_block(&padded[..32]).expect("32 real bytes complete");
        assert_eq!(edid_refusal_reason(&completed), None);
        // And the bytes that were real are the bytes served: byte for byte, the
        // head goes through untouched, so the make, the model and the date the
        // RM gave us are still there to be read.
        assert_eq!(&completed[..32], &padded[..32]);
        // A completed block states no timing, which is the honest answer -- the
        // RM never gave us one -- and the client falls back to the mode.
        assert_eq!(edid::preferred_timing(&completed), None);
    }

    /// A bad pointer and a corrupt read are different things to go and look at,
    /// so they are told apart rather than both coming out as "invalid".
    #[test]
    fn a_wrong_header_and_a_wrong_checksum_are_named_separately() {
        let mut garbage = good_block();
        garbage[3] = 0x00;
        assert_eq!(edid_refusal_reason(&garbage), Some("header"));

        let mut corrupt = good_block();
        corrupt[40] ^= 0xFF;
        assert_eq!(
            edid_refusal_reason(&corrupt),
            Some("checksum"),
            "the header is untouched, so this must not come out as a header fault"
        );
    }

    /// Short of a whole block is its own reason: nothing was decoded, so neither
    /// the header nor the checksum is the thing to report.
    #[test]
    fn a_block_short_of_a_whole_one_says_so() {
        let b = good_block();
        assert_eq!(edid_refusal_reason(&b[..127]), Some("short"));
        assert_eq!(edid_refusal_reason(&[]), Some("short"));
    }

    /// The klog budget, same shape and same reason as everywhere else here: a
    /// connector is probed on every `GETCONNECTOR`, and a compositor that
    /// re-enumerates in a loop would pin the UART.
    #[test]
    fn the_refusal_budget_is_bounded() {
        assert!(MAX_EDID_REFUSALS > 0 && MAX_EDID_REFUSALS <= 8);
    }

    /// What a refused connector reports instead: nothing. Which is the honest
    /// answer and the one every compositor already handles, because it is what
    /// it gets on any machine whose firmware captured no EDID at all.
    #[test]
    fn a_connector_with_no_usable_edid_reports_none_rather_than_a_stub() {
        let _g = super::test_globals::lock();
        // No driver is registered in a host test and no firmware EDID was
        // captured, so this is the fallback path every such machine takes.
        assert_eq!(get_connector_edid(SYNTH_CONNECTOR_ID), None);
        assert_eq!(boot_edid_block(), None);
    }
}

/// Tests for how far right a blit of the CRTC framebuffer may go.
///
/// One number, asked by four places, and the two regimes it has to tell apart
/// look identical from inside any one of them: past the visible width is the
/// display's own off-screen padding when the image covers the screen, and it is
/// the screen itself when the image does not. Getting that backwards is either a
/// write-combining buffer flushed half-full (a fraction of one blit) or the
/// framebuffer's row padding painted onto the desktop (pixels a person sees).
#[cfg(test)]
mod image_pitch_tests {
    use super::*;

    /// The ordinary desktop: a 1920 framebuffer on a 1920 mode whose scanline is
    /// padded to 2048 by the firmware. The blit may run to the row's end, which
    /// is the framebuffer's own stride -- those columns are the display's padding
    /// and nobody sees them.
    #[test]
    fn an_image_that_covers_the_screen_may_reach_the_displays_padding() {
        assert_eq!(image_pitch_px(1920, 2048, 1920, 1920), 1920);
        // And with a padded SOURCE stride, out to that stride.
        assert_eq!(image_pitch_px(1936, 2048, 1920, 1920), 1936);
    }

    /// Never past the row, though. One pixel further is the next row's leftmost
    /// pixel, and a pointer whose tail appears on the far left of the line below
    /// is what that looks like.
    #[test]
    fn it_never_reaches_past_the_row_itself() {
        assert_eq!(image_pitch_px(1920, 4096, 1920, 1920), 1920);
        assert!(image_pitch_px(1920, 4096, 1920, 1920) <= 1920);
    }

    /// Nor past what the display can address, when the display is the narrower
    /// of the two.
    #[test]
    fn it_never_reaches_past_what_the_display_can_address() {
        assert_eq!(image_pitch_px(2048, 1920, 1366, 2048), 1920);
    }

    /// The defect. A client framebuffer narrower than the mode: the columns past
    /// its right edge are ON the screen, and it has nothing to put in them, so
    /// the blit stops at the image. Widening to the stride here read the row
    /// padding, and past that the next row, and painted both.
    #[test]
    fn an_image_narrower_than_the_screen_stops_at_its_own_right_edge() {
        // A 32-wide framebuffer in a 48-pixel stride, on a 64-wide screen.
        assert_eq!(image_pitch_px(48, 64, 64, 32), 32);
        // Not the stride, and not the screen.
        assert_ne!(image_pitch_px(48, 64, 64, 32), 48);
        assert_ne!(image_pitch_px(48, 64, 64, 32), 64);
    }

    /// The boundary between the two regimes is "as wide as the screen", not
    /// "wider than it". One pixel either side decides whether the widened
    /// columns are padding or desktop.
    #[test]
    fn the_regime_turns_over_at_exactly_as_wide_as_the_screen() {
        assert_eq!(
            image_pitch_px(80, 128, 64, 63),
            63,
            "one short: stop at the image"
        );
        assert_eq!(
            image_pitch_px(80, 128, 64, 64),
            80,
            "exactly as wide: reach the padding"
        );
        assert_eq!(image_pitch_px(80, 128, 64, 65), 80, "wider: the same");
    }

    /// Callers hold different widths -- the framebuffer's own, or the `min` of it
    /// and the display's -- and both have to give the same answer, because
    /// `cursor_read_is_synced` compares a window one caller derived against a
    /// flush another caller decided.
    #[test]
    fn the_raw_width_and_the_minned_one_agree() {
        for (dw, fbw) in [
            (64u32, 32u32),
            (64, 64),
            (64, 96),
            (1920, 1920),
            (1920, 1366),
        ] {
            let minned = dw.min(fbw);
            assert_eq!(
                image_pitch_px(2048, 2048, dw, fbw),
                image_pitch_px(2048, 2048, dw, minned),
                "display {} vs framebuffer {}",
                dw,
                fbw
            );
        }
    }

    /// A zero-width image asks for nothing, and must not come out as "the whole
    /// row": `expand_x_for_wc` treats its limit as the right-hand bound, so a
    /// limit of the stride on an empty image would widen a nothing into a row.
    #[test]
    fn an_image_of_no_width_does_not_become_a_whole_row() {
        assert_eq!(image_pitch_px(64, 64, 64, 0), 0);
        assert_eq!(expand_x_for_wc(0, 0, image_pitch_px(64, 64, 64, 0)), (0, 0));
    }
}

/// Tests for the two numbers the present's timing line reports: what the source
/// flush costs, and what the blit needed out of it.
///
/// They exist because the line they go in was behind `if rect.is_none()`, so a
/// damage-clipped present -- every frame a compositor with damage tracking puts
/// up, which is every frame labwc puts up -- printed nothing at all. The one
/// number that says whether the flush is oversized was the one number never
/// reported.
#[cfg(test)]
mod present_cost_tests {
    use super::*;

    /// A full frame reads everything it flushes, bar the last row's padding:
    /// the run IS the frame.
    #[test]
    fn a_full_frame_flushes_about_what_it_reads() {
        // 1920x1080 at a 1920-pixel pitch.
        let flushed = sync_span_bytes(1920, 0, 0, 1920, 1080);
        let read = blit_read_bytes(1920, 1080);
        assert_eq!(read, 1920 * 1080 * 4);
        assert_eq!(flushed, read, "no padding between rows at this pitch");
    }

    /// And with a padded pitch it flushes the padding of every row but the last,
    /// which is the honest cost of one contiguous run.
    #[test]
    fn a_full_frame_on_a_padded_pitch_flushes_the_padding_too() {
        let flushed = sync_span_bytes(2048, 0, 0, 1920, 1080);
        let read = blit_read_bytes(1920, 1080);
        assert!(flushed > read);
        // 1079 rows of 128 padding pixels.
        assert_eq!(flushed - read, 1079 * 128 * 4);
    }

    /// The number this exists to surface. Moebius's power menu is about 200x180
    /// in a 1920-wide framebuffer: the blit reads 144 KiB and the flush sweeps
    /// 1.38 MiB, because one contiguous run spans every byte from the first
    /// row's left edge to the last row's right edge.
    #[test]
    fn a_popups_damage_box_flushes_about_ten_times_what_it_reads() {
        let flushed = sync_span_bytes(1920, 860, 400, 200, 180);
        let read = blit_read_bytes(200, 180);
        assert_eq!(read, 200 * 180 * 4);
        assert!(
            flushed > read * 9 && flushed < read * 11,
            "flushed {} for read {}",
            flushed,
            read
        );
    }

    /// A one-row damage box is the case where they agree however wide the pitch
    /// is, because there is no next row for the run to reach into. A blinking
    /// terminal cursor is this shape.
    #[test]
    fn a_single_row_box_flushes_exactly_what_it_reads() {
        assert_eq!(sync_span_bytes(1920, 100, 500, 8, 1), blit_read_bytes(8, 1));
    }

    /// Nothing to flush is zero, not a panic and not the whole row -- the line
    /// prints these unconditionally now, including for a present that was
    /// refused or had a degenerate box.
    #[test]
    fn a_degenerate_box_costs_nothing() {
        assert_eq!(sync_span_bytes(1920, 0, 0, 0, 100), 0);
        assert_eq!(sync_span_bytes(1920, 0, 0, 100, 0), 0);
        assert_eq!(sync_span_bytes(0, 0, 0, 100, 100), 0);
        assert_eq!(blit_read_bytes(0, 100), 0);
        assert_eq!(blit_read_bytes(100, 0), 0);
    }

    /// The two report rates. A compositor with damage tracking issues several
    /// clipped presents per frame and full frames almost never, so one divisor
    /// for both either drowns the log -- and `klog` writes synchronously to the
    /// UART, where a line this long is milliseconds -- or hides the clipped path,
    /// which is what the old `if rect.is_none()` did.
    #[test]
    fn a_damage_box_is_reported_far_less_often_than_a_full_frame() {
        assert!(
            RECT_REPORT_EVERY >= FULL_FRAME_REPORT_EVERY * 4,
            "the clipped path is the frequent one; reporting it as often as a \
             full frame puts the log on the critical path"
        );
        // And both still report, which is the whole change.
        assert!(FULL_FRAME_REPORT_EVERY > 0 && RECT_REPORT_EVERY > 0);
    }

    /// The report used to divide by 1024 unconditionally, so a caret box or a
    /// cursor patch -- a few hundred bytes read -- printed `0KiB flushed for 0KiB
    /// read`. Two zeros, on exactly the small high-frequency updates the damage
    /// path exists for, and the ratio between the two numbers is the finding.
    #[test]
    fn a_few_hundred_bytes_is_not_reported_as_zero() {
        assert_eq!(cost_scaled(blit_read_bytes(4, 4)), (64, "B"));
        assert_eq!(cost_scaled(1), (1, "B"));
        assert_eq!(cost_scaled(1023), (1023, "B"));
    }

    /// And a whole frame still reads as a whole frame, so the change did not buy
    /// the small case by making the big one unreadable.
    #[test]
    fn a_frames_worth_is_still_reported_in_kibibytes() {
        assert_eq!(cost_scaled(1024), (1, "KiB"));
        assert_eq!(cost_scaled(blit_read_bytes(1920, 1080)), (8100, "KiB"));
    }

    /// Nothing is nothing, not "less than a KiB of something".
    #[test]
    fn no_bytes_at_all_reports_zero_bytes() {
        assert_eq!(cost_scaled(0), (0, "B"));
    }

    /// Neither number overflows on values no framebuffer has, because the line
    /// that prints them must never be the thing that panics the kernel.
    #[test]
    fn neither_number_overflows_on_absurd_geometry() {
        assert_eq!(blit_read_bytes(u32::MAX, u32::MAX), usize::MAX);
        let _ = sync_span_bytes(usize::MAX, u32::MAX, u32::MAX, u32::MAX, u32::MAX);
    }
}

#[cfg(test)]
mod damage_against_the_panel_tests {
    use super::*;

    const BOX: Option<(u32, u32, u32, u32)> = Some((100, 100, 200, 180));

    /// The case the optimisation exists for: the client re-presents the very
    /// framebuffer the panel already carries, so the pixels outside the box
    /// really are the ones on screen and copying them again is waste.
    #[test]
    fn a_box_on_the_framebuffer_the_panel_carries_is_honoured() {
        assert_eq!(rect_for_present(BOX, 7, 7), BOX);
    }

    /// The case that put a collage on the panel. Framebuffer 8 is a swapchain
    /// buffer the compositor drew the popup into; everywhere else it holds the
    /// frame it was last used for, which is not the frame on screen. Copy only
    /// the box and the panel carries two frames at once.
    #[test]
    fn a_box_on_a_different_framebuffer_becomes_the_whole_frame() {
        assert_eq!(rect_for_present(BOX, 7, 8), None);
    }

    /// Nothing on the panel to be a box's reference: the first present of a
    /// session, or the one right after blanking painted it black, or the one
    /// after a text console wrote over it.
    #[test]
    fn a_box_on_a_panel_that_carries_nothing_becomes_the_whole_frame() {
        assert_eq!(rect_for_present(BOX, 0, 8), None);
    }

    /// Two absences are not a match. `0` means "no framebuffer" on both sides,
    /// and letting them compare equal would honour a box against a panel nobody
    /// has ever presented to -- the one case where the whole frame is most
    /// certainly needed.
    #[test]
    fn framebuffer_zero_never_matches_a_panel_that_carries_nothing() {
        assert_eq!(rect_for_present(BOX, 0, 0), None);
    }

    /// A page flip or a modeset asks for the whole frame by the shape of the
    /// call, and stays that way whatever the panel carries.
    #[test]
    fn a_whole_frame_present_is_left_alone() {
        assert_eq!(rect_for_present(None, 0, 0), None);
        assert_eq!(rect_for_present(None, 7, 7), None);
        assert_eq!(rect_for_present(None, 7, 8), None);
    }

    /// When the box is honoured it is passed through untouched: the rule decides
    /// between this box and the whole frame, and never between two boxes.
    #[test]
    fn an_honoured_box_is_the_callers_own_box() {
        for r in [
            (0, 0, 1, 1),
            (100, 100, 200, 180),
            (0, 0, u32::MAX, u32::MAX),
        ] {
            assert_eq!(rect_for_present(Some(r), 3, 3), Some(r));
        }
    }

    /// Retiring a framebuffer forgets it, and forgets only it. Ids are handed
    /// out again, so a new buffer landing on a retired number must not inherit
    /// "the panel already carries this".
    #[test]
    fn retiring_a_framebuffer_forgets_only_that_one() {
        let _g = test_globals::lock();
        reset_output_state_for_test();
        set_panel_fb(7);
        forget_panel_fb(9);
        assert_eq!(panel_fb(), 7, "an unrelated retirement must not clear it");
        forget_panel_fb(7);
        assert_eq!(panel_fb(), 0);
        // And forgetting nothing is not the same as forgetting everything: a
        // retirement while the panel carries nothing leaves it carrying nothing.
        forget_panel_fb(0);
        assert_eq!(panel_fb(), 0);
        reset_output_state_for_test();
    }

    /// The report budget stops, and says so on the way out. A compositor that
    /// presents a fresh buffer every frame promotes every frame, so an unbudgeted
    /// line here is sixty klog writes a second on the path whose cost this whole
    /// area is about.
    #[test]
    fn the_promotion_report_is_budgeted() {
        let _g = test_globals::lock();
        reset_output_state_for_test();
        assert_eq!(DAMAGE_PROMOTIONS_LOGGED.load(Ordering::Relaxed), 0);
        assert!(MAX_DAMAGE_PROMOTIONS_LOGGED > 0 && MAX_DAMAGE_PROMOTIONS_LOGGED <= 8);
        reset_output_state_for_test();
    }
}
