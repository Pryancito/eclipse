//! Page-flip completions on the real vblank, when the panel's driver has a
//! hardware frame counter.
//!
//! Without one, a flip's event goes out on the synthetic vblank: a timer at
//! the mode's period, which knows nothing of where the monitor really is in
//! its frame. In Linux the event goes out from the vblank interrupt, after
//! the flip latched (`drm_crtc_send_vblank_event` from `nv_drm`'s
//! flip-occurred callback). Eclipse has no display interrupt, but the
//! driver can read the frame counter the vblank advances
//! ([`DrmScheme::hw_frame_count`]). So a flip notes the counter when it is
//! queued ([`note_flip`]), its completion stays queued until the counter has
//! moved ([`flip_held`]), and while it waits the queue is looked at again
//! every [`POLL_US`] instead of every period ([`poll_soon`]). The event then
//! reaches the compositor within a millisecond of the real vblank, never
//! before it, so the buffer it gives back is really off the screen.
//!
//! A counter that does not move (a head that went to sleep) holds a flip at
//! most [`HOLD_MAX_US`]; then it goes out as before.

use super::*;
use core::sync::atomic::{AtomicBool, AtomicU64};
use core::time::Duration;

/// How often a held flip is looked at again.
const POLL_US: u64 = 1_000;

/// The longest a flip waits for the counter: three frames at 60 Hz.
const HOLD_MAX_US: u64 = 50_000;

/// The frame counter when the flip in flight was queued, `u64::MAX` for
/// none (or no counter).
static FLIP_FRAME: AtomicU64 = AtomicU64::new(u64::MAX);

static ANNOUNCED: AtomicBool = AtomicBool::new(false);
static STUCK_LOGGED: AtomicBool = AtomicBool::new(false);

/// When that flip was queued, in microseconds.
static FLIP_AT_US: AtomicU64 = AtomicU64::new(0);

fn now_us() -> u64 {
    kernel_hal::timer::timer_now().as_micros() as u64
}

fn frame_count() -> Option<u32> {
    get_display_driver()?.hw_frame_count()
}

/// A flip was queued: note where the hardware counter is.
pub(super) fn note_flip() {
    let frame = frame_count().map_or(u64::MAX, u64::from);
    if frame != u64::MAX && !ANNOUNCED.swap(true, Ordering::Relaxed) {
        kernel_hal::klog_info!(
            "[drm] vblank por hardware: los eventos de flip esperan al contador de frames \
             de la cabeza (FRM_CNT={})",
            frame
        );
    }
    FLIP_AT_US.store(now_us(), Ordering::Relaxed);
    FLIP_FRAME.store(frame, Ordering::Release);
}

/// Whether the queued flip's completion has to wait: the panel has a frame
/// counter and it has not moved since the flip, for less than
/// [`HOLD_MAX_US`]. Once it says no, it says no until the next
/// [`note_flip`].
pub(super) fn flip_held() -> bool {
    let base = FLIP_FRAME.load(Ordering::Acquire);
    if base == u64::MAX {
        return false;
    }
    let moved = frame_count().is_none_or(|f| u64::from(f) != base);
    let late = now_us().wrapping_sub(FLIP_AT_US.load(Ordering::Relaxed)) >= HOLD_MAX_US;
    if late && !moved && !STUCK_LOGGED.swap(true, Ordering::Relaxed) {
        kernel_hal::klog_warn!(
            "[drm] vblank por hardware: el contador de frames no avanzo en {} ms; el flip \
             sale sin esperar (la cabeza esta parada?)",
            HOLD_MAX_US / 1000
        );
    }
    if moved || late {
        FLIP_FRAME.store(u64::MAX, Ordering::Release);
        return false;
    }
    true
}

/// Look at the queue again in [`POLL_US`], for a held flip.
pub(super) fn poll_soon() {
    #[cfg(test)]
    let armed_in = test_globals::acquisition_now();
    let deadline = kernel_hal::timer::timer_now() + Duration::from_micros(POLL_US);
    kernel_hal::timer::timer_set(
        deadline,
        Box::new(move |_| {
            // As `arm_coalesced_drm_timer_locked`'s callback: a poll a
            // finished test left behind delivers nothing for the next one.
            #[cfg(test)]
            if test_globals::acquisition_now() != armed_in {
                return;
            }
            deliver_due_drm_timers()
        }),
    );
}

/// One flip's completion, `drm_send_event_helper`: the out-fence signals
/// first, then the event goes on the fd when one was asked for and the fd is
/// still open, and either way the flip leaves the in-flight population.
pub(super) fn complete_flip(
    crtc_id: u32,
    user_data: u64,
    file: Weak<DrmFileState>,
    event: bool,
    out_fence: Option<u32>,
) {
    if let Some(handle) = out_fence {
        // The syncobj is the sync_file's: closed already, there is nobody
        // to tell.
        let _ = zcore_drivers::scheme::syncobj::signal(handle);
    }
    match (event, file.upgrade()) {
        (true, Some(file)) => queue_flip_event(&file, crtc_id, user_data),
        // No event owed, or the drm_file closed before delivery: the event
        // is dropped, and the flip leaves the in-flight population like any
        // other. Clearing the latch alone here left the count one too high
        // for good: from then on no delivery was ever the last one, the
        // latch never cleared after a real flip, and the next PAGE_FLIP spun
        // `settle_outstanding_flip` out into the EBUSY wlroots escalates
        // into an output teardown.
        _ => flip_in_flight_done(),
    }
}
