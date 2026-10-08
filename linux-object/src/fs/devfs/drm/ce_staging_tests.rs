use super::*;

/// Read `len` bytes of the staging buffer back through the physmap, the
/// same way the copy engine reaches them.
fn staging_bytes(pa: u64, len: usize) -> alloc::vec::Vec<u8> {
    let va = phys_to_virt(pa as usize);
    // SAFETY: `pa` is the start of the contiguous staging VMO, which the
    // repack just reported as holding at least `len` bytes, and it stays
    // alive in `CE_STAGING` for the rest of the process.
    unsafe { core::slice::from_raw_parts(va as *const u8, len).to_vec() }
}

/// Source pixels numbered from `base` so each one is identifiable.
fn numbered(base: u32, stride: usize, rows: usize) -> alloc::vec::Vec<u32> {
    (0..stride * rows).map(|n| base + n as u32).collect()
}

/// The regression this guards. The repack writes `w * 4` bytes per row, but
/// the caller then asks the copy engine for a FLAT copy of
/// `dst_pitch * h` bytes -- so the `row_bytes..dst_pitch` tail of every row
/// is carried to the screen without anyone having written it. A first,
/// wider frame leaves its pixels there; a later, narrower frame does not
/// overwrite them, and the copy engine paints them. When that tail is
/// off-screen padding it is invisible, so the repack now defines it;
/// when it would start inside the visible width the repack DECLINES (see
/// the next test) rather than clobbering real columns.
#[test]
fn the_row_tail_the_copy_engine_carries_is_never_left_stale() {
    let _serialised = super::test_globals::lock();
    const PITCH: usize = 64; // 16 pixels per destination row
    const H: u32 = 4;
    // Frame 1 fills whole rows: 16 pixels of 4 bytes = the full pitch.
    let wide = numbered(0xAAA0_0000, 16, H as usize);
    let (pa, size) = ce_repack_to_staging(&wide, 0, 16, PITCH, PITCH, 16, H)
        .expect("a whole-row repack must be accepted");
    assert_eq!(size, (PITCH * H as usize) as u64);
    let before = staging_bytes(pa, PITCH * H as usize);
    assert_eq!(
        &before[60..64],
        &0xAAA0_000Fu32.to_ne_bytes(),
        "frame 1 wrote the end of row 0"
    );

    // Frame 2 is narrower -- 8 pixels -- with a destination whose visible
    // part is only those 8 pixels, so the remaining 32 bytes of each row
    // are off-screen padding and the repack is allowed to proceed.
    let narrow = numbered(0xBBB0_0000, 8, H as usize);
    let (pa2, size2) = ce_repack_to_staging(&narrow, 0, 8, PITCH, 32, 8, H)
        .expect("an off-screen tail must still be accepted");
    assert_eq!(size2, (PITCH * H as usize) as u64);
    let after = staging_bytes(pa2, PITCH * H as usize);
    for r in 0..H as usize {
        let row = &after[r * PITCH..(r + 1) * PITCH];
        // The 8 pixels the frame actually has.
        assert_eq!(
            u32::from_ne_bytes([row[0], row[1], row[2], row[3]]),
            0xBBB0_0000 + (r * 8) as u32,
            "row {} first pixel",
            r
        );
        // And the tail the copy engine will carry regardless: defined, not
        // frame 1's leftovers. This is the assertion that failed before.
        assert!(
            row[32..].iter().all(|&b| b == 0),
            "row {} tail still holds a previous frame: {:02x?}",
            r,
            &row[32..]
        );
    }
}

/// A tail that would START inside the visible width must make the repack
/// decline, so the caller falls back to the CPU blit -- which writes only
/// the columns it has pixels for and leaves the rest of the screen alone.
/// Filling that tail (with zeros or with anything else) would paint over
/// on-screen columns the frame says nothing about.
#[test]
fn a_repack_that_cannot_cover_the_visible_width_is_declined() {
    let _serialised = super::test_globals::lock();
    const PITCH: usize = 64;
    let src = numbered(0xCCC0_0000, 8, 4);
    // 8 pixels of source, but 12 pixels (48 bytes) of the row are visible.
    assert!(
        ce_repack_to_staging(&src, 0, 8, PITCH, 48, 8, 4).is_none(),
        "would have clobbered visible columns 8..12"
    );
    // Exactly covering the visible width is fine.
    assert!(ce_repack_to_staging(&src, 0, 8, PITCH, 32, 8, 4).is_some());
    // A row wider than the destination pitch is refused as before: it
    // would spill each row into the next.
    assert!(ce_repack_to_staging(&src, 0, 8, PITCH, 32, 17, 4).is_none());
    // Degenerate geometry.
    assert!(ce_repack_to_staging(&src, 0, 8, PITCH, 32, 0, 4).is_none());
}
