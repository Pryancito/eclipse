//! The pre-present fence wait: which ioctls take it, and its clock.
//!
//! Both halves are pure, and both are the part that can be got wrong
//! silently. The router decides whether a frame waits at all -- miss
//! `PAGE_FLIP` and the whole implicit-sync path is dead code on a running
//! desktop, with nothing in any log to say so, because the present still
//! happens and merely shows a half-drawn buffer. The clock decides whether
//! a wait that times out ends: `sleep_until` takes an ABSOLUTE instant, so
//! a deadline already in the past must end the wait rather than park the
//! compositor on it.
//!
//! What is not covered here, because it needs a live device and a driver:
//! the fb lookup, the driver's fence answer, the timeout warning, the text of
//! the two report lines, and the `+ 1` that makes the count one-based (a
//! zero-based count would print `#0` for the first present and open with
//! three lines instead of two -- visible only in a klog no test reads). The
//! report rhythm IS covered, because it is the only thing that will ever say
//! whether the wait found a fence at all, and a rhythm that reports nothing
//! is a diagnostic that lies by omission.

use super::*;

/// The first two presents of a boot always report. Those two are the whole
/// point: they answer "does this find a fence at all" before anything else
/// has had a chance to go wrong, and a rhythm that started at 64 would
/// answer it a second into the session at the earliest.
#[test]
fn the_first_two_presents_of_a_boot_always_say_what_they_found() {
    assert!(fence_report_decision(1, FENCE_REPORT_EVERY));
    assert!(fence_report_decision(2, FENCE_REPORT_EVERY));
}

/// And the third does not, or the klog is a line per frame -- which it
/// writes synchronously to the UART, so it would be a stutter of its own.
#[test]
fn the_third_present_is_quiet() {
    assert!(!fence_report_decision(3, FENCE_REPORT_EVERY));
    for n in [4u64, 5, 63, 65, 127] {
        assert!(!fence_report_decision(n, FENCE_REPORT_EVERY), "n = {}", n);
    }
}

/// After that, one line every `FENCE_REPORT_EVERY` presents, counted from
/// the first: `#64`, `#128`, and so on.
#[test]
fn then_one_line_every_sixty_four_presents() {
    for k in 1..=8u64 {
        let n = k * FENCE_REPORT_EVERY;
        assert!(fence_report_decision(n, FENCE_REPORT_EVERY), "n = {}", n);
    }
}

/// `n = 0` cannot happen -- the counter is read after its increment -- but
/// the multiple-of test would call every rhythm true for it, so the answer
/// is pinned rather than left to `0 % 64 == 0`.
#[test]
fn the_count_is_one_based_and_zero_is_not_a_present() {
    assert!(fence_report_decision(0, FENCE_REPORT_EVERY));
}

/// A rhythm of zero reports the first two and then goes quiet. Nothing
/// divides by zero on the way there: `is_multiple_of(0)` answers `false` for
/// a non-zero count, which is exactly "not on the rhythm". The constant is
/// not configurable today, so this is about the function staying safe if it
/// ever becomes so.
#[test]
fn a_rhythm_of_zero_reports_the_opening_and_nothing_else() {
    assert!(fence_report_decision(1, 0));
    assert!(fence_report_decision(2, 0));
    for n in [3u64, 64, 128, u64::MAX] {
        assert!(!fence_report_decision(n, 0), "n = {}", n);
    }
}

/// The fence line and the present cost line describe the SAME present, so
/// they share one rhythm and land together in the klog: `waited 0us for 0
/// fence(s)` next to `cpu blit 12000us` is the pair that says whether the
/// wait is what costs the frame. Two independent numbers would drift apart
/// and leave a reader counting frames between them.
#[test]
fn the_fence_line_keeps_the_cost_lines_rhythm() {
    assert_eq!(FENCE_REPORT_EVERY, drm::FULL_FRAME_REPORT_EVERY);
}

/// The last present a 64-bit counter can reach still reports on its rhythm
/// rather than panicking or wrapping: the counter itself saturates.
#[test]
fn the_rhythm_holds_at_the_top_of_the_counter() {
    let top = u64::MAX - (u64::MAX % FENCE_REPORT_EVERY);
    assert!(fence_report_decision(top, FENCE_REPORT_EVERY));
    assert!(!fence_report_decision(u64::MAX, FENCE_REPORT_EVERY));
}

/// `SETCRTC` and `PAGE_FLIP` both present, and both must wait.
#[test]
fn the_two_legacy_presents_are_recognised_at_their_real_sizes() {
    assert_eq!(
        legacy_present_kind(DRM_IOCTL_MODE_SETCRTC),
        Some(LegacyPresent::SetCrtc)
    );
    assert_eq!(
        legacy_present_kind(DRM_IOCTL_MODE_PAGE_FLIP),
        Some(LegacyPresent::PageFlip)
    );
}

/// The struct sizes the `nr` table claims are the ones the dispatch arms
/// actually read, so a struct that changes shape breaks here first.
#[test]
fn the_size_floors_match_the_structs_the_wait_parses() {
    assert_eq!(nr::MODE_SETCRTC.1, core::mem::size_of::<DrmModeGetCrtc>());
    assert_eq!(
        nr::MODE_PAGE_FLIP.1,
        core::mem::size_of::<DrmModeCrtcPageFlip>()
    );
}

/// A struct that grows a trailing field keeps waiting. This is the bug
/// `is_drm_ioctl_nr` exists for, and pinning the 32-bit command instead
/// would have made a wider `drm_mode_crtc` skip the wait in silence.
#[test]
fn a_larger_encoded_struct_is_still_the_same_present() {
    for extra in [8usize, 16, 64] {
        assert_eq!(
            legacy_present_kind(drm_iowr_core(
                nr::MODE_SETCRTC.0,
                nr::MODE_SETCRTC.1 + extra
            )),
            Some(LegacyPresent::SetCrtc)
        );
        assert_eq!(
            legacy_present_kind(drm_iowr_core(
                nr::MODE_PAGE_FLIP.0,
                nr::MODE_PAGE_FLIP.1 + extra
            )),
            Some(LegacyPresent::PageFlip)
        );
    }
}

/// Below the floor the helper would read past the request, so it must fall
/// through to the sync arm, which zero-pads it the way Linux does.
#[test]
fn a_short_request_does_not_take_the_wait() {
    assert_eq!(
        legacy_present_kind(drm_iowr_core(nr::MODE_SETCRTC.0, nr::MODE_SETCRTC.1 - 1)),
        None
    );
    assert_eq!(
        legacy_present_kind(drm_iowr_core(nr::MODE_PAGE_FLIP.0, 8)),
        None
    );
}

/// Everything else, including the ioctls with their own waits: a present
/// fence wait on an atomic commit would wait twice, and on a syncobj wait
/// it would wait for the wrong thing entirely.
#[test]
fn no_other_ioctl_takes_the_present_wait() {
    for cmd in [
        ATOMIC_IOCTL,
        WAIT_VBLANK_IOCTL,
        DRM_IOCTL_SYNCOBJ_WAIT,
        DRM_IOCTL_MODE_DIRTYFB,
        DRM_IOCTL_MODE_SETPLANE,
    ] {
        assert_eq!(legacy_present_kind(cmd), None, "cmd {:#x} waits", cmd);
    }
    // Right NR, wrong ioctl type byte: another subsystem's 0xA2.
    let foreign = drm_iowr_core(nr::MODE_SETCRTC.0, nr::MODE_SETCRTC.1) & !(0xff << 8);
    assert_eq!(legacy_present_kind(foreign), None);
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// The ordinary tick, while the deadline is far away.
#[test]
fn a_poll_far_from_its_deadline_sleeps_one_tick() {
    assert_eq!(next_poll_wake(ms(10), ms(110), ms(1)), Some(ms(11)));
}

/// Clamped to the deadline, or a 100 ms cap becomes 101 ms on every frame
/// that times out.
#[test]
fn the_last_poll_never_sleeps_past_the_deadline() {
    assert_eq!(
        next_poll_wake(ms(10), ms(10) + ms(1) / 2, ms(1)),
        Some(ms(10) + ms(1) / 2)
    );
}

/// At or past the deadline the wait ends. Parking on an instant already
/// behind `timer_now` is what would hang the compositor instead of
/// presenting a possibly-torn frame.
#[test]
fn an_expired_deadline_ends_the_wait_instead_of_sleeping() {
    assert_eq!(next_poll_wake(ms(10), ms(10), ms(1)), None);
    assert_eq!(next_poll_wake(ms(10), ms(9), ms(1)), None);
    assert_eq!(next_poll_wake(ms(10), Duration::ZERO, ms(1)), None);
}

/// A zero tick is a busy loop, not a sleep: `wake == now` ends the wait
/// rather than yielding forever at the same instant.
#[test]
fn a_zero_tick_ends_the_wait_rather_than_spinning() {
    assert_eq!(next_poll_wake(ms(10), ms(110), Duration::ZERO), None);
}
