use super::*;
use crate::fs::LxError;

/// The lock, plus the CRTC binding `present_failed` leaves behind.
///
/// `present_failed("SETCRTC", 7, ..)` does what the production path does on a
/// present failure that is not fatal: the modeset stands, so fb 7 ends up
/// bound to the panel's CRTC. Here fb 7 is a bare id that was never in the
/// framebuffer table, and `crtc_fb` is process-wide -- so left bound, the
/// NEXT test's atomic commit inherits it (`atomic_commit` takes
/// `upd.plane_fb_id.unwrap_or(crtc_fb())` when the request names no `FB_ID`),
/// looks it up, and answers `NotFound`. That is a refusal the test that gets
/// it did nothing to earn: `out_fence_tests` fell 7 runs in 8 at 16 threads
/// on it and passed every time it ran alone.
///
/// Put back here rather than at the end of each test because a panicking
/// assertion would skip the cleanup and take the rest of the suite with it.
struct Bound(#[allow(dead_code)] drm::test_globals::Held);

impl Bound {
    fn take() -> Bound {
        Bound(drm::test_globals::lock())
    }
}

impl Drop for Bound {
    fn drop(&mut self) {
        drm::set_crtc_fb(1, 0);
    }
}

/// Linux fails `drm_mode_setcrtc` for a fb id it cannot look up, and does
/// it with `ENOENT` ("Unknown FB ID"). Keep failing that one — it is a real
/// client error — but with the errno that points at fb lifetime instead of
/// at the bus.
#[test]
fn an_unknown_fb_id_still_fails_the_ioctl_but_as_enoent() {
    let _serialised = Bound::take();
    assert_eq!(
        present_failed("SETCRTC", 7, 1, drm::PresentError::NoSuchFb),
        Err(FsError::EntryNotFound),
    );
    assert_eq!(
        LxError::from(FsError::EntryNotFound),
        LxError::ENOENT,
        "EntryNotFound is the arm's way of spelling ENOENT",
    );
}

/// Everything else is a frame that could not be copied, not a modeset that
/// could not be programmed. Report it and carry on: the CRTC keeps its
/// binding, the compositor keeps running, and the next present gets another
/// chance — instead of the output being written off for good.
#[test]
fn a_frame_that_could_not_be_copied_does_not_fail_the_modeset() {
    let _serialised = Bound::take();
    for reason in [drm::PresentError::NoDisplay, drm::PresentError::NoBacking] {
        drm::set_crtc_fb(1, 0);
        assert_eq!(
            present_failed("SETCRTC", 7, 1, reason),
            Ok(()),
            "{:?} must not take the whole output down",
            reason,
        );
        // Answering 0 means the modeset happened, so the readback has to
        // agree: a caller that asks GETCRTC which fb is on the CRTC must
        // be told the one it just set, not the one before it.
        assert_eq!(
            drm::crtc_fb(),
            7,
            "{:?} left GETCRTC naming a different fb than SETCRTC accepted",
            reason,
        );
    }
}

/// The console trace is bounded. A per-frame line on this path is exactly
/// the flood that has wedged spinlocks on a slow serial console before, and
/// the failure it reports is a steady state, not a one-off.
#[test]
fn the_console_trace_is_bounded_however_long_the_storm_runs() {
    let _serialised = Bound::take();
    PRESENT_FAIL_TRACED.store(0, Ordering::Relaxed);
    for _ in 0..10_000 {
        let _ = present_failed("SETCRTC", 7, 1, drm::PresentError::NoDisplay);
    }
    let traced = PRESENT_FAIL_TRACED
        .load(Ordering::Relaxed)
        .min(PRESENT_FAIL_TRACE_BUDGET);
    assert_eq!(
        traced, PRESENT_FAIL_TRACE_BUDGET,
        "the budget is spent exactly once, not per call",
    );
}
