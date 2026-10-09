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
