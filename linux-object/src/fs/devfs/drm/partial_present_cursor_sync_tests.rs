use super::{cursor_read_is_synced, expand_x_for_wc};

const STRIDE: usize = 1920;
const FB_H: u32 = 1080;
const CURSOR: u32 = 64;

/// The pointer window as the present computes it: x widened to the
/// write-combining boundary, then handed in as the read rect.
fn ptr(x: u32, y: u32) -> (i32, u32, u32, u32) {
    let (ex, ew) = expand_x_for_wc(x, CURSOR, STRIDE as u32);
    (ex as i32, y, ew, CURSOR)
}

fn synced_for(blit: (u32, u32, u32, u32), at: (u32, u32)) -> bool {
    let (ex, py, ew, ph) = ptr(at.0, at.1);
    cursor_read_is_synced(
        STRIDE,
        blit,
        true,
        (ex, py as i32, ew, ph),
        STRIDE as u32,
        FB_H,
    )
}

/// A full-frame present invalidates the whole buffer in one run, so the
/// pointer is covered wherever it is and the present must not pay for a
/// second flush. This is the case the old guard was written for, and it
/// has to keep behaving exactly as it did.
#[test]
fn a_full_frame_present_covers_the_pointer_anywhere() {
    let full = (0, 0, STRIDE as u32, FB_H);
    for x in [0u32, 1, 15, 700, 1855, 1919] {
        for y in [0u32, 1, 540, 1015, 1079] {
            assert!(
                synced_for(full, (x, y)),
                "full frame left the pointer at ({}, {}) unsynced",
                x,
                y
            );
        }
    }
}

/// The bug: a popup's damage box does not reach the pointer's rows, and
/// nothing else in the present invalidates them. A menu or a calendar is
/// exactly this box.
#[test]
fn a_popup_damage_box_does_not_cover_a_pointer_outside_its_rows() {
    // A 320x240 menu near the top left.
    let menu = (48, 64, 320, 240);
    // The pointer well below the menu's last row (64 + 240 = 304).
    for y in [320u32, 500, 900] {
        for x in [64u32, 900, 1600] {
            assert!(
                !synced_for(menu, (x, y)),
                "the pointer at ({}, {}) is outside the damage box's rows, so the \
                 present did not invalidate what the blend reads -- it must flush",
                x,
                y
            );
        }
    }
}

/// And when the box does span the pointer's rows *and* its columns, the
/// run already covers it: no second flush, so the common case of a popup
/// opening under the pointer stays as cheap as it was.
#[test]
fn a_damage_box_around_the_pointer_needs_no_second_flush() {
    // The box starts left of the widened window and ends right of it, on
    // every row the pointer occupies.
    let wide = (0, 100, STRIDE as u32, 400);
    for y in [100u32, 200, 435] {
        for x in [0u32, 33, 900, 1855] {
            assert!(
                synced_for(wide, (x, y)),
                "pointer at ({}, {}) is inside the damage run and was flushed twice",
                x,
                y
            );
        }
    }
}

/// A box that spans the pointer's ROWS but stops short of its COLUMNS is
/// still covered, because the invalidate is one contiguous run and not a
/// set of rows -- the columns in between are flushed on the way past. This
/// is what makes the cheap containment test correct rather than merely
/// conservative; getting it wrong the other way would flush on every
/// frame.
#[test]
fn the_run_between_the_first_and_last_row_counts_as_covered() {
    // Columns [0, 200) only, rows [100, 500) -- the pointer at x = 1600 is
    // far to its right, but between row 100's left edge and row 499's
    // right edge in linear order.
    let narrow = (0, 100, 200, 400);
    assert!(
        synced_for(narrow, (1600, 300)),
        "a row strictly inside the run is covered whatever its column"
    );
    // The pointer's LAST row must still be inside the run: at row 436 the
    // window ends on row 499, the run's last row, past its right edge.
    assert!(
        !synced_for(narrow, (1600, 436)),
        "the pointer's last row runs past the end of the run"
    );
}

/// Every unknown falls towards "flush". A needless flush of a 64x64 window
/// costs microseconds; a skipped one puts stale pixels on the screen, so
/// there is no input for which "I cannot tell" may answer "covered".
#[test]
fn what_cannot_be_decided_is_flushed() {
    let full = (0, 0, STRIDE as u32, FB_H);
    let (ex, py, ew, ph) = ptr(700, 500);
    // A stride of zero cannot be linearised at all.
    assert!(
        !cursor_read_is_synced(0, full, true, (ex, py as i32, ew, ph), STRIDE as u32, FB_H),
        "a stride nothing can be linearised against must flush"
    );
    // The present ran no FromDevice of its own.
    assert!(
        !cursor_read_is_synced(
            STRIDE,
            full,
            false,
            (ex, py as i32, ew, ph),
            STRIDE as u32,
            FB_H
        ),
        "no sync ran, so nothing is covered"
    );
    // A degenerate blit rect invalidated nothing.
    assert!(
        !cursor_read_is_synced(
            STRIDE,
            (0, 0, 0, 0),
            true,
            (ex, py as i32, ew, ph),
            STRIDE as u32,
            FB_H
        ),
        "an empty blit rect covers nothing"
    );
    // A width past `i32::MAX` used to wrap negative through an `as i32`
    // and read as an empty rect -- "nothing to read", which skips the
    // flush. It has to clip to the buffer and be treated as a real read.
    assert!(
        !cursor_read_is_synced(
            STRIDE,
            (0, 900, 16, 8),
            true,
            (0, 0, u32::MAX, u32::MAX),
            STRIDE as u32,
            FB_H
        ),
        "an out-of-range read window must clip, not vanish"
    );
}
