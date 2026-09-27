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

/// A legacy present waits for the writes a client imported onto the
/// framebuffer's buffer (`DMA_BUF_IOCTL_IMPORT_SYNC_FILE`), found by the
/// buffer's physical address: the writes, not the reads
/// (`drm_gem_plane_helper_prepare_fb` takes `DMA_RESV_USAGE_WRITE`), and
/// nothing once they have landed or for a framebuffer that does not
/// exist. There was nowhere to import such a fence before.
#[test]
fn a_scanout_waits_for_the_writes_imported_onto_its_buffer() {
    use zcore_drivers::scheme::{resv, syncobj};
    let _serialised = super::test_globals::lock();
    let phys = 0x9_0000_0000u64;
    plant(9402, 64 * 1024, 78_002);
    DRM_STATE
        .lock()
        .handles
        .iter_mut()
        .find(|(h, _, _)| h.id == 9402)
        .unwrap()
        .0
        .phys_addr = phys;
    let fb = create_fb(9402, 64, 4, 256).unwrap();
    assert_eq!(scanout_resv_fences(fb), [], "nothing imported");
    let read = syncobj::create(false);
    let write = syncobj::create(false);
    assert!(resv::add_fence(phys, read, 1, false));
    assert_eq!(scanout_resv_fences(fb), [], "a read is not waited for");
    assert!(resv::add_fence(phys, write, 1, true));
    let deps = scanout_resv_fences(fb);
    assert_eq!(deps.len(), 1, "the write is");
    assert_eq!(deps[0].1, 1);
    assert_eq!(syncobj::query(deps[0].0), Some(0), "still in flight");
    assert_eq!(scanout_resv_fences(fb + 1000), [], "not a framebuffer");
    assert!(syncobj::signal(write));
    assert_eq!(scanout_resv_fences(fb), [], "landed");
    resv::forget(phys);
    syncobj::destroy(read);
    syncobj::destroy(write);
    unplant(9402);
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
