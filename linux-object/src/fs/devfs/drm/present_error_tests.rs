use super::*;

/// Plant a framebuffer directly in the table, bypassing `create_fb` (which
/// refuses a fb with no backing — the point here is to build the state a
/// live system can reach anyway, e.g. a driver fb whose GEM went away).
fn plant_fb(fb_id: u32, phys_addr: u64, size: usize) {
    let mut state = DRM_STATE.lock();
    state.framebuffers.retain(|fb| fb.id != fb_id);
    state.framebuffers.push(DrmFramebuffer {
        pixel_format: DRM_FORMAT_XRGB8888,
        id: fb_id,
        driver_fb_id: None,
        gem_handle_id: 0,
        width: 1,
        height: 1,
        pitch: 4,
        phys_addr,
        size,
        layout: ScanoutLayout::Linear,
        owner: 0,
    });
}

fn drop_fb(fb_id: u32) {
    DRM_STATE.lock().framebuffers.retain(|fb| fb.id != fb_id);
}

/// An fb id nothing answers to is the one failure that IS the caller's
/// fault, and it has to be distinguishable from the rest: it is the only
/// reason the ioctl arms still fail on, and they answer `ENOENT` for it
/// (Linux's "Unknown FB ID"), not `EIO`.
///
/// This is not a hypothetical id: `retire_framebuffers_for_handle` drops a
/// nouveau-backed fb the instant its GEM handle closes, so a compositor
/// that still holds the id from `ADDFB2` lands here through no fault of
/// its scanout path.
#[test]
fn an_unknown_fb_id_is_reported_as_no_such_fb() {
    let _serialised = super::test_globals::lock();
    drop_fb(9601);
    assert_eq!(
        scanout_region_checked(9601, None),
        Err(PresentError::NoSuchFb)
    );
    assert_eq!(
        present_now_checked(9601, 1, None),
        Err(PresentError::NoSuchFb),
        "the reason must survive the page-flip/scanout fallback chain"
    );
}

/// A framebuffer that describes no memory is the framebuffer's own defect,
/// so it is reported as such whether or not a display is attached — the
/// backing check runs first for exactly this reason. Getting `NoDisplay`
/// here would send the reader looking at the wrong half of the system.
#[test]
fn a_framebuffer_with_no_backing_is_reported_as_no_backing() {
    let _serialised = super::test_globals::lock();
    plant_fb(9602, 0, 4096);
    assert_eq!(
        scanout_region_checked(9602, None),
        Err(PresentError::NoBacking)
    );
    // Zero size, same verdict: there is nothing to copy either way.
    plant_fb(9602, 0x1_0000, 0);
    assert_eq!(
        scanout_region_checked(9602, None),
        Err(PresentError::NoBacking)
    );
    drop_fb(9602);
}

/// The `bool` wrappers the rest of the tree still calls must keep behaving
/// exactly as they did — the reason is additive, not a change of contract.
#[test]
fn the_bool_wrappers_still_report_failure_the_old_way() {
    let _serialised = super::test_globals::lock();
    drop_fb(9603);
    assert!(!scanout_region(9603, None));
    assert!(!present_now(9603, 1));
    assert!(!present_now_region(9603, 1, Some((0, 0, 1, 1))));
}

/// The fork a `NoSuchFb` on the console cannot resolve on its own: a
/// framebuffer the client removed itself, versus one the kernel took out
/// from under it when the nouveau GEM handle closed. On Linux only the
/// first can happen -- a `drm_framebuffer` there holds its own reference
/// on the GEM object -- so the second is our bug to fix, and the log line
/// has to say which one the compositor hit.
#[test]
fn a_retired_fb_id_remembers_what_took_it() {
    let _serialised = super::test_globals::lock();
    let handle = zcore_drivers::scheme::gem_mmap::DRIVER_HANDLE_BASE + 0x71;
    zcore_drivers::scheme::gem_mmap::register(handle, 0x2_0000, 4096, 0);
    {
        let mut state = DRM_STATE.lock();
        state.framebuffers.push(DrmFramebuffer {
            pixel_format: DRM_FORMAT_XRGB8888,
            id: 9604,
            driver_fb_id: None,
            gem_handle_id: handle,
            width: 1,
            height: 1,
            pitch: 4,
            phys_addr: 0x2_0000,
            size: 4096,
            layout: ScanoutLayout::Linear,
            owner: 0,
        });
    }
    // The object is really gone (last reference dropped) -- the only
    // condition under which a framebuffer is retired behind its owner.
    zcore_drivers::scheme::gem_mmap::unregister(handle);
    assert_eq!(retire_framebuffers_for_handle(handle), 1);
    assert_eq!(fb_retired_reason(9604), Some(FbRetired::HandleClosed));

    // The client's own RMFB reads differently, because it is a different
    // answer: nothing was taken from anyone.
    plant_fb(9605, 0x3_0000, 4096);
    assert!(rmfb(9605));
    assert_eq!(fb_retired_reason(9605), Some(FbRetired::Removed));

    // An id that was never a framebuffer of ours has no story to tell.
    assert_eq!(fb_retired_reason(9699), None);
}

/// The history is a fixed cost: a compositor that recreates its swapchain
/// all session long retires framebuffers forever, and this must not grow
/// with it.
#[test]
fn the_retirement_history_is_bounded() {
    let _serialised = super::test_globals::lock();
    for i in 0..(FB_RETIRE_HISTORY as u32 * 4) {
        plant_fb(9700 + i, 0x3_0000, 4096);
        assert!(rmfb(9700 + i));
    }
    assert!(DRM_STATE.lock().fb_retirements.len() <= FB_RETIRE_HISTORY);
    // And it is the NEWEST that are kept -- the id a stuck compositor is
    // still re-presenting is the one that has to be explainable.
    let newest = 9700 + (FB_RETIRE_HISTORY as u32 * 4) - 1;
    assert_eq!(fb_retired_reason(newest), Some(FbRetired::Removed));
}

/// Each reason prints as itself: these strings are what a boot log carries
/// and what a bug report gets grepped for.
#[test]
fn every_reason_has_its_own_console_text() {
    let _serialised = super::test_globals::lock();
    let all = [
        PresentError::NoSuchFb,
        PresentError::NoDisplay,
        PresentError::NoBacking,
    ];
    for (i, a) in all.iter().enumerate() {
        assert!(!a.as_str().is_empty());
        for b in &all[i + 1..] {
            assert_ne!(a.as_str(), b.as_str(), "{:?} and {:?} read alike", a, b);
        }
    }
}
