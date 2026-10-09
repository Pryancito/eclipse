use super::*;

/// The regression this guards. `DrmFramebuffer` is `Copy`, so the present
/// path took a bare `phys_addr`/`size` out of `DRM_STATE`, dropped the lock,
/// and blitted for up to ~100 ms holding nothing. A concurrent RMFB could
/// drop the last `Arc<VmObject>` in that window and hand the frames back to
/// the allocator while the blit was still reading them.
#[test]
fn a_present_snapshot_keeps_the_framebuffer_memory_alive() {
    let _serialised = super::test_globals::lock();
    let vmo = VmObject::new_paged(1);
    {
        let mut state = DRM_STATE.lock();
        state.framebuffers.push(DrmFramebuffer {
            pixel_format: DRM_FORMAT_XRGB8888,
            id: 9601,
            driver_fb_id: None,
            gem_handle_id: 7,
            width: 1,
            height: 1,
            pitch: 4,
            phys_addr: 0,
            size: 4096,
            layout: ScanoutLayout::Linear,
            owner: current_pid(),
        });
        state.fb_backing.push((9601, vmo.clone()));
    }
    // The test's own reference plus the one in `fb_backing`.
    assert_eq!(Arc::strong_count(&vmo), 2);

    let (fb, backing) = snapshot_fb_for_present(9601).expect("the fb exists");
    assert_eq!(fb.id, 9601);
    let backing = backing.expect("a dumb-buffer fb has a VMO to hold");
    assert_eq!(
        Arc::strong_count(&vmo),
        3,
        "the snapshot must take its own reference"
    );

    // RMFB while the "blit" is in flight: the fb is gone from the table, but
    // the memory is NOT freed, because the snapshot still owns a reference.
    assert!(rmfb(9601));
    assert!(snapshot_fb_for_present(9601).is_none(), "the fb is retired");
    assert_eq!(
        Arc::strong_count(&vmo),
        2,
        "only the table's reference dropped; the blit's is intact"
    );

    // The blit finishes and lets go.
    drop(backing);
    assert_eq!(Arc::strong_count(&vmo), 1);
}

/// A nouveau-backed framebuffer has no `VmObject` to take a reference on, so
/// the snapshot reports `None` rather than pretending. That window is closed
/// from the other end instead, by retiring the framebuffer when the GEM
/// object is freed -- which is what `retire_framebuffers_for_handle` does.
#[test]
fn a_nouveau_backed_framebuffer_has_no_reference_to_take() {
    let _serialised = super::test_globals::lock();
    let handle = zcore_drivers::scheme::gem_mmap::DRIVER_HANDLE_BASE + 0x77;
    {
        let mut state = DRM_STATE.lock();
        state.framebuffers.push(DrmFramebuffer {
            pixel_format: DRM_FORMAT_XRGB8888,
            id: 9602,
            driver_fb_id: None,
            gem_handle_id: handle,
            width: 1,
            height: 1,
            pitch: 4,
            phys_addr: 0x2_0000,
            size: 4096,
            layout: ScanoutLayout::Linear,
            owner: current_pid(),
        });
    }
    let (fb, backing) = snapshot_fb_for_present(9602).expect("the fb exists");
    assert_eq!(fb.gem_handle_id, handle);
    assert!(
        backing.is_none(),
        "there is no VmObject behind a nouveau GEM"
    );
    DRM_STATE.lock().framebuffers.retain(|f| f.id != 9602);
}

/// An unknown id is not a framebuffer.
#[test]
fn an_unknown_framebuffer_cannot_be_snapshotted() {
    let _serialised = super::test_globals::lock();
    assert!(snapshot_fb_for_present(0).is_none());
    assert!(snapshot_fb_for_present(0xDEAD_BEEF).is_none());
}
