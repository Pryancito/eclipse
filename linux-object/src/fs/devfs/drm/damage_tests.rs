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
