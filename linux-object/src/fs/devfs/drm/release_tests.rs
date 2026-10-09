use super::*;

/// Hand-build a GEM entry owned by `pid`, bypassing `alloc_buffer` (which
/// would need a current thread and real contiguous frames). What is under
/// test is the bookkeeping: who owns an entry and what releases it.
fn plant(id: u32, size: usize, pid: u64) {
    let vmo = VmObject::new_paged(1);
    let handle = GemHandle {
        id,
        size,
        phys_addr: 0,
    };
    DRM_STATE.lock().handles.push((handle, vmo, pid));
}

fn live_ids() -> Vec<u32> {
    DRM_STATE
        .lock()
        .handles
        .iter()
        .map(|(h, _, _)| h.id)
        .collect()
}

#[test]
fn process_exit_releases_only_that_process_buffers() {
    let _serialised = super::test_globals::lock();
    // Distinct ids/pids so this cannot collide with another test's state.
    plant(9001, 4096, 77_001);
    plant(9002, 8192, 77_001);
    plant(9003, 4096, 77_002);

    // The bug: nothing but an explicit ioctl ever dropped these, so a
    // process that died without one leaked every buffer it had made.
    assert_eq!(release_process(77_001), 2);

    let ids = live_ids();
    assert!(!ids.contains(&9001), "9001 should be gone with its owner");
    assert!(!ids.contains(&9002), "9002 should be gone with its owner");
    assert!(ids.contains(&9003), "another process's buffer must survive");

    // Idempotent: a second teardown for the same pid frees nothing more.
    assert_eq!(release_process(77_001), 0);

    assert_eq!(release_process(77_002), 1);
    assert!(!live_ids().contains(&9003));
}

#[test]
fn process_exit_gives_back_the_syncobjs_it_still_held() {
    let _serialised = super::test_globals::lock();
    use zcore_drivers::scheme::syncobj;
    // Linux frees a dying client's syncobj handles with its drm_file
    // (`drm_syncobj_release`); here nothing did, so a crashed client's
    // stayed in the table for the rest of the boot.
    let mine = syncobj::create_for(77_004, false);
    let shared = syncobj::create_for(77_005, false);
    assert!(syncobj::add_ref_for(77_004, shared), "imported by 77_004");
    let theirs = syncobj::create_for(77_005, true);
    assert_eq!(release_process(77_004), 0, "no buffers to give back");
    assert!(!syncobj::exists(mine), "freed with its owner");
    assert!(syncobj::exists(shared), "77_005 still holds it");
    assert!(!syncobj::held_by(77_004, shared));
    assert!(syncobj::exists(theirs), "another process's survives");
    assert_eq!(release_process(77_004), 0, "idempotent");
    assert!(syncobj::destroy_for(77_005, shared));
    assert!(syncobj::destroy_for(77_005, theirs));
    assert!(!syncobj::exists(shared) && !syncobj::exists(theirs));
}

#[test]
fn unowned_buffers_are_never_reclaimed() {
    let _serialised = super::test_globals::lock();
    // pid 0 means "allocated with no current thread" (boot-time). A process
    // exit must never take those: pid 0 is not a real owner.
    plant(9101, 4096, 0);
    assert_eq!(release_process(0), 0);
    assert!(live_ids().contains(&9101));
    DRM_STATE.lock().handles.retain(|(h, _, _)| h.id != 9101);
}

#[test]
fn releasing_a_buffer_drops_the_framebuffer_built_on_it() {
    let _serialised = super::test_globals::lock();
    plant(9201, 4096, 77_003);
    {
        let mut state = DRM_STATE.lock();
        state.framebuffers.push(DrmFramebuffer {
            pixel_format: DRM_FORMAT_XRGB8888,
            id: 9299,
            driver_fb_id: None,
            gem_handle_id: 9201,
            width: 1,
            height: 1,
            pitch: 4,
            phys_addr: 0,
            size: 4096,
            layout: ScanoutLayout::Linear,
            owner: 77_003,
        });
        state.crtc_fb = 9299;
    }
    assert_eq!(release_process(77_003), 1);
    let state = DRM_STATE.lock();
    assert!(
        !state.framebuffers.iter().any(|fb| fb.id == 9299),
        "a framebuffer over a freed handle would scan out released memory"
    );
    assert_eq!(state.crtc_fb, 0, "the CRTC must not point at a dropped fb");
}

/// Linux semantics: closing the handle drops the *handle*, not the
/// object. A framebuffer built on it keeps the memory (its own `Arc` on
/// the VMO) and stays scannable until RMFB, which then releases it.
#[test]
fn gem_close_keeps_a_framebuffer_and_its_memory_alive() {
    let _serialised = super::test_globals::lock();
    plant(9301, 4096, 77_004);
    let vmo = handle_vmo(9301).expect("planted handle resolves to its VMO");
    {
        let mut state = DRM_STATE.lock();
        state.framebuffers.push(DrmFramebuffer {
            pixel_format: DRM_FORMAT_XRGB8888,
            id: 9399,
            driver_fb_id: None,
            gem_handle_id: 9301,
            width: 1,
            height: 1,
            pitch: 4,
            phys_addr: 0,
            size: 4096,
            layout: ScanoutLayout::Linear,
            owner: 77_004,
        });
        state.fb_backing.push((9399, vmo.clone()));
        state.crtc_fb = 9399;
    }
    // Two owners besides the handle table: the test's `vmo` and the fb.
    assert_eq!(Arc::strong_count(&vmo), 3);

    assert!(gem_close(9301), "the handle existed");
    assert!(handle_vmo(9301).is_none(), "the handle is gone");
    {
        let state = DRM_STATE.lock();
        assert!(
            state.framebuffers.iter().any(|fb| fb.id == 9399),
            "the fb outlives its handle"
        );
        assert_eq!(state.crtc_fb, 9399, "and the CRTC still scans it out");
    }
    // Only the fb's reference dropped away with the handle table entry.
    assert_eq!(Arc::strong_count(&vmo), 2);

    assert!(rmfb(9399));
    assert_eq!(
        Arc::strong_count(&vmo),
        1,
        "RMFB released the fb's reference"
    );
    let state = DRM_STATE.lock();
    assert!(!state.framebuffers.iter().any(|fb| fb.id == 9399));
    assert!(!state.fb_backing.iter().any(|(id, _)| *id == 9399));
    assert_eq!(state.crtc_fb, 0, "RMFB of the CRTC fb unbinds it");
}
