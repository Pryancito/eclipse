use super::*;

/// A GEM entry of `size` bytes, owned by `pid`, planted directly so the
/// test does not need a current thread or real contiguous frames.
fn plant(id: u32, size: usize, pid: u64) {
    let vmo = VmObject::new_paged(size.div_ceil(4096));
    DRM_STATE.lock().handles.push((
        GemHandle {
            id,
            size,
            phys_addr: 0,
        },
        vmo,
        pid,
    ));
}

fn unplant(id: u32) {
    let mut state = DRM_STATE.lock();
    state.handles.retain(|(h, _, _)| h.id != id);
    state.framebuffers.retain(|fb| fb.gem_handle_id != id);
}

/// The regression this guards. Every CPU consumer of `fb.pitch` turns it
/// into a row stride in PIXELS with `fb.pitch / 4` (`src_stride` in
/// `scanout_region` and in `repaint_for_cursor`), so a pitch that is not a
/// whole number of XRGB8888 pixels makes each row start a couple of bytes
/// early -- a shear that grows down the screen. The copy engine does not
/// truncate, so the two present paths would not even agree on the image.
/// ADDFB2 used to accept it.
#[test]
fn addfb_rejects_a_pitch_that_is_not_whole_pixels() {
    let _serialised = super::test_globals::lock();
    let (w, h) = (64u32, 4u32);
    // Generous backing so the size guard never decides these cases: what
    // is under test is the pitch alignment, nothing else.
    plant(9401, 64 * 1024, 78_001);

    // The aligned pitch for this width is accepted.
    assert!(create_fb(9401, w, h, w * 4).is_some(), "w*4 must be valid");
    // A padded but still 4-byte-aligned pitch is fine too: padding is
    // legal, a fractional pixel is not.
    assert!(create_fb(9401, w, h, w * 4 + 16).is_some());
    // Two bytes past a whole pixel is not.
    assert!(
        create_fb(9401, w, h, w * 4 + 2).is_none(),
        "a fractional pitch would scan out a sheared image"
    );
    for bad in [1u32, 2, 3] {
        assert!(create_fb(9401, w, h, w * 4 + bad).is_none(), "+{}", bad);
    }

    unplant(9401);
}

/// The pre-existing guards, asserted so the new pitch check cannot be
/// mistaken for the whole of the validation: a framebuffer must fit inside
/// its backing buffer, and must be at least as wide as it claims.
#[test]
fn addfb_still_rejects_a_framebuffer_that_does_not_fit_its_buffer() {
    let _serialised = super::test_globals::lock();
    plant(9402, 4096, 78_002);
    // 64x16 at 4 bytes = 4096, exactly the buffer.
    assert!(create_fb(9402, 64, 16, 256).is_some());
    // One row more does not fit.
    assert!(create_fb(9402, 64, 17, 256).is_none());
    // A pitch too small for the claimed width would make `scanout_region`
    // read the next row's pixels as this row's tail.
    assert!(create_fb(9402, 64, 4, 128).is_none());
    // Degenerate sizes are not framebuffers.
    assert!(create_fb(9402, 64, 0, 256).is_none());
    assert!(create_fb(9402, 0, 4, 0).is_none());
    // An unknown handle has no backing to scan out.
    assert!(create_fb(9499, 64, 4, 256).is_none());
    unplant(9402);
}
