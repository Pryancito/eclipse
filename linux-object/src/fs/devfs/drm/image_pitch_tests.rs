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
