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
