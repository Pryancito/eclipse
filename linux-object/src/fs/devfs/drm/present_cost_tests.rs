use super::*;

/// A full frame reads everything it flushes, bar the last row's padding:
/// the run IS the frame.
#[test]
fn a_full_frame_flushes_about_what_it_reads() {
    // 1920x1080 at a 1920-pixel pitch.
    let flushed = sync_span_bytes(1920, 0, 0, 1920, 1080);
    let read = blit_read_bytes(1920, 1080);
    assert_eq!(read, 1920 * 1080 * 4);
    assert_eq!(flushed, read, "no padding between rows at this pitch");
}

/// And with a padded pitch it flushes the padding of every row but the last,
/// which is the honest cost of one contiguous run.
#[test]
fn a_full_frame_on_a_padded_pitch_flushes_the_padding_too() {
    let flushed = sync_span_bytes(2048, 0, 0, 1920, 1080);
    let read = blit_read_bytes(1920, 1080);
    assert!(flushed > read);
    // 1079 rows of 128 padding pixels.
    assert_eq!(flushed - read, 1079 * 128 * 4);
}

/// The number this exists to surface. Moebius's power menu is about 200x180
/// in a 1920-wide framebuffer: the blit reads 144 KiB and the flush sweeps
/// 1.38 MiB, because one contiguous run spans every byte from the first
/// row's left edge to the last row's right edge.
#[test]
fn a_popups_damage_box_flushes_about_ten_times_what_it_reads() {
    let flushed = sync_span_bytes(1920, 860, 400, 200, 180);
    let read = blit_read_bytes(200, 180);
    assert_eq!(read, 200 * 180 * 4);
    assert!(
        flushed > read * 9 && flushed < read * 11,
        "flushed {} for read {}",
        flushed,
        read
    );
}

/// A one-row damage box is the case where they agree however wide the pitch
/// is, because there is no next row for the run to reach into. A blinking
/// terminal cursor is this shape.
#[test]
fn a_single_row_box_flushes_exactly_what_it_reads() {
    assert_eq!(sync_span_bytes(1920, 100, 500, 8, 1), blit_read_bytes(8, 1));
}

/// Nothing to flush is zero, not a panic and not the whole row -- the line
/// prints these unconditionally now, including for a present that was
/// refused or had a degenerate box.
#[test]
fn a_degenerate_box_costs_nothing() {
    assert_eq!(sync_span_bytes(1920, 0, 0, 0, 100), 0);
    assert_eq!(sync_span_bytes(1920, 0, 0, 100, 0), 0);
    assert_eq!(sync_span_bytes(0, 0, 0, 100, 100), 0);
    assert_eq!(blit_read_bytes(0, 100), 0);
    assert_eq!(blit_read_bytes(100, 0), 0);
}

/// The two report rates. A compositor with damage tracking issues several
/// clipped presents per frame and full frames almost never, so one divisor
/// for both either drowns the log -- and `klog` writes synchronously to the
/// UART, where a line this long is milliseconds -- or hides the clipped path,
/// which is what the old `if rect.is_none()` did.
#[test]
fn a_damage_box_is_reported_far_less_often_than_a_full_frame() {
    assert!(
        RECT_REPORT_EVERY >= FULL_FRAME_REPORT_EVERY * 4,
        "the clipped path is the frequent one; reporting it as often as a \
         full frame puts the log on the critical path"
    );
    // And both still report, which is the whole change.
    assert!(FULL_FRAME_REPORT_EVERY > 0 && RECT_REPORT_EVERY > 0);
}

/// The report used to divide by 1024 unconditionally, so a caret box or a
/// cursor patch -- a few hundred bytes read -- printed `0KiB flushed for 0KiB
/// read`. Two zeros, on exactly the small high-frequency updates the damage
/// path exists for, and the ratio between the two numbers is the finding.
#[test]
fn a_few_hundred_bytes_is_not_reported_as_zero() {
    assert_eq!(cost_scaled(blit_read_bytes(4, 4)), (64, "B"));
    assert_eq!(cost_scaled(1), (1, "B"));
    assert_eq!(cost_scaled(1023), (1023, "B"));
}

/// And a whole frame still reads as a whole frame, so the change did not buy
/// the small case by making the big one unreadable.
#[test]
fn a_frames_worth_is_still_reported_in_kibibytes() {
    assert_eq!(cost_scaled(1024), (1, "KiB"));
    assert_eq!(cost_scaled(blit_read_bytes(1920, 1080)), (8100, "KiB"));
}

/// Nothing is nothing, not "less than a KiB of something".
#[test]
fn no_bytes_at_all_reports_zero_bytes() {
    assert_eq!(cost_scaled(0), (0, "B"));
}

/// Neither number overflows on values no framebuffer has, because the line
/// that prints them must never be the thing that panics the kernel.
#[test]
fn neither_number_overflows_on_absurd_geometry() {
    assert_eq!(blit_read_bytes(u32::MAX, u32::MAX), usize::MAX);
    let _ = sync_span_bytes(usize::MAX, u32::MAX, u32::MAX, u32::MAX, u32::MAX);
}
