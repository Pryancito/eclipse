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
