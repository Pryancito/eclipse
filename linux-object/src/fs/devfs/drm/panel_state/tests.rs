//! What the panel state may be told about a CRTC that is not the one it
//! describes.
//!
//! Every test here takes [`crate::fs::devfs::drm::test_globals::lock`]:
//! `PANEL_CRTC` and the panel state it guards are process-wide, and cargo runs
//! a crate's tests in threads, so without it one test's CRTC is another's.

use super::super::test_globals;
use super::super::{panel_fb_for_test, set_panel_fb};
use super::{claim_panel_for, forget_panel_crtc, panel_crtc, PanelState};

/// Leave the state as a fresh boot would, so a test does not inherit the CRTC
/// of whatever ran before it inside the same lock.
fn from_scratch() {
    forget_panel_crtc();
    set_panel_fb(0);
}

#[test]
fn the_same_crtc_twice_keeps_what_the_panel_carries() {
    let _g = test_globals::lock();
    from_scratch();

    assert_eq!(claim_panel_for(2001), PanelState::TakenOver);
    set_panel_fb(42);
    // The second present to the same pipe is the common case, and it is the
    // one that must NOT throw the damage box away.
    assert_eq!(claim_panel_for(2001), PanelState::Describes);
    assert_eq!(panel_fb_for_test(), 42);
}

#[test]
fn another_crtc_does_not_inherit_the_first_ones_frame() {
    let _g = test_globals::lock();
    from_scratch();

    claim_panel_for(2001);
    set_panel_fb(42);
    // This is the bug: without the takeover, CRTC 5001 would read "the panel
    // already carries 42" and honour a damage box against another monitor.
    assert_eq!(claim_panel_for(5001), PanelState::TakenOver);
    assert_eq!(panel_fb_for_test(), 0);
}

/// And back again: the first CRTC does not get its own frame back either. The
/// state describes one panel, and 5001 has overwritten what it said.
#[test]
fn going_back_to_the_first_crtc_also_starts_over() {
    let _g = test_globals::lock();
    from_scratch();

    claim_panel_for(2001);
    set_panel_fb(42);
    claim_panel_for(5001);
    set_panel_fb(77);
    assert_eq!(claim_panel_for(2001), PanelState::TakenOver);
    assert_eq!(panel_fb_for_test(), 0);
}

/// A present that names no pipe reads the state as it is. That is the
/// pre-26-oct behaviour, and it is what the software-KMS path wants: there is
/// one synthetic CRTC for the whole machine there, so there is nothing for a
/// second pipe to take the state away from.
#[test]
fn a_present_that_names_no_crtc_takes_nothing_over() {
    let _g = test_globals::lock();
    from_scratch();

    claim_panel_for(2001);
    set_panel_fb(42);
    assert_eq!(claim_panel_for(0), PanelState::Describes);
    assert_eq!(panel_fb_for_test(), 42);
    // And it does not become the owner either, so the real pipe still reads
    // its own state back.
    assert_eq!(panel_crtc(), 2001);
    assert_eq!(claim_panel_for(2001), PanelState::Describes);
    assert_eq!(panel_fb_for_test(), 42);
}

/// Forgetting the CRTC is what a blank or a console VT does, and the next
/// present -- to that same pipe -- has to start over, not read `Describes`
/// over state somebody else cleared.
#[test]
fn forgetting_the_crtc_makes_the_same_pipe_start_over() {
    let _g = test_globals::lock();
    from_scratch();

    claim_panel_for(2001);
    set_panel_fb(42);
    forget_panel_crtc();
    assert_eq!(panel_crtc(), 0);
    assert_eq!(claim_panel_for(2001), PanelState::TakenOver);
    assert_eq!(panel_fb_for_test(), 0);
}

/// The takeover clears the framebuffer id AND the band hashes, because they
/// describe the same pixels. `set_panel_fb(0)` is what couples them, and a
/// takeover that cleared only the id would leave the band skip declining to
/// copy bands it hashed off the other panel.
#[test]
fn a_takeover_clears_the_bands_too_not_just_the_framebuffer() {
    let _g = test_globals::lock();
    from_scratch();

    claim_panel_for(2001);
    set_panel_fb(42);
    super::super::note_panel_bands_for_test(1920, 1080);
    assert!(super::super::panel_bands_known_for_test());

    claim_panel_for(5001);
    assert!(!super::super::panel_bands_known_for_test());
}

/// A blank, or a console VT taking the panel, clears the state through
/// `set_panel_fb(0)` -- and that has to drop the CRTC too, or the next present
/// to that same pipe reads `Describes` over state somebody else cleared.
#[test]
fn clearing_the_panel_makes_the_same_pipe_start_over() {
    let _g = test_globals::lock();
    from_scratch();

    claim_panel_for(2001);
    set_panel_fb(42);
    // What a blank or a foreground text VT does.
    set_panel_fb(0);
    assert_eq!(panel_crtc(), 0);
    assert_eq!(claim_panel_for(2001), PanelState::TakenOver);
}
