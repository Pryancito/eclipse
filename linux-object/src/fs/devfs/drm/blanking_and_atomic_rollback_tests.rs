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
