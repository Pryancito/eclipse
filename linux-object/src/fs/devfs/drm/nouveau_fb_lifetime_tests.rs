use super::*;
use zcore_drivers::scheme::gem_mmap;

/// Plant a framebuffer over a driver-private handle, the way `create_fb`
/// does for a nouveau `GEM_NEW` object: a bare `phys_addr`/`size` resolved
/// through `gem_mmap`, and **no** `fb_backing` reference, because there is
/// no `VmObject` to take one on.
fn plant_nouveau_fb(fb_id: u32, handle: u32, pid: u64) {
    gem_mmap::register(handle, 0x1_0000, 4096, pid);
    let mut state = DRM_STATE.lock();
    state.framebuffers.push(DrmFramebuffer {
        pixel_format: DRM_FORMAT_XRGB8888,
        id: fb_id,
        driver_fb_id: None,
        gem_handle_id: handle,
        width: 1,
        height: 1,
        pitch: 4,
        phys_addr: 0x1_0000,
        size: 4096,
        layout: ScanoutLayout::Linear,
        owner: pid,
    });
    state.crtc_fb = fb_id;
}

fn fb_exists(fb_id: u32) -> bool {
    DRM_STATE
        .lock()
        .framebuffers
        .iter()
        .any(|fb| fb.id == fb_id)
}

/// The regression this guards. `nouveau_gem_close` returns the object's
/// memory to the RM, and the framebuffer over it holds no reference that
/// could stop that -- so the framebuffer has to go with it. It did not:
/// `gem_close` only ever looked at `state.handles`, where a nouveau handle
/// never appears, so the fb (and `crtc_fb` pointing at it) survived the
/// close and the next repaint blitted freed GEM memory. Persistent garbage
/// on the panel, because the scanout keeps reading that address.
///
/// "Freed" is now the precondition, not merely "somebody called close":
/// `gem_mmap` drops its entry when the last reference goes and before the
/// RM free, so an absent entry is what "the memory is gone" means here.
#[test]
fn closing_a_nouveau_handle_retires_the_framebuffer_over_it() {
    let _serialised = super::test_globals::lock();
    let handle = gem_mmap::DRIVER_HANDLE_BASE + 0x55;
    plant_nouveau_fb(9501, handle, 0);
    assert!(fb_exists(9501));

    // The last reference is gone and the object with it.
    gem_mmap::unregister(handle);
    assert_eq!(retire_framebuffers_for_handle(handle), 1);
    assert!(!fb_exists(9501), "the fb outlived the memory it points at");
    assert_eq!(
        DRM_STATE.lock().crtc_fb,
        0,
        "the CRTC must not keep scanning out a retired fb"
    );
    // Idempotent: a second close finds nothing left to retire.
    assert_eq!(retire_framebuffers_for_handle(handle), 0);
}

/// The other half, and the one that cost a desktop. A `GEM_CLOSE` that is
/// NOT the last reference must leave the framebuffer alone.
///
/// `nouveau_gem_close` answers `true` for "this close was handled", which
/// includes one holder letting go of a buffer others still reference -- so
/// the `GEM_CLOSE` arm calls `retire_framebuffers_for_handle` on every
/// close, not just the final one. wlroots closes its buffer handle
/// immediately after `ADDFB2`, because on Linux the framebuffer holds its
/// own reference; here that close retired the scanout buffer seconds after
/// it was created, and every `SETCRTC` on it answered `ENOENT` for the rest
/// of the session -- the "Failed to set CRTC: No such file or directory"
/// storm, at frame rate.
#[test]
fn a_close_that_is_not_the_last_reference_leaves_the_framebuffer_alone() {
    let _serialised = super::test_globals::lock();
    let handle = gem_mmap::DRIVER_HANDLE_BASE + 0x56;
    plant_nouveau_fb(9505, handle, 0);
    // Still registered: other references remain, so the memory is alive.
    assert!(gem_mmap::lookup(handle).is_some());

    assert_eq!(
        retire_framebuffers_for_handle(handle),
        0,
        "a live GEM object's framebuffer must survive a close"
    );
    assert!(fb_exists(9505), "the scanout buffer was destroyed under it");
    assert_eq!(
        DRM_STATE.lock().crtc_fb,
        9505,
        "and the CRTC still scans it out"
    );

    DRM_STATE.lock().framebuffers.retain(|fb| fb.id != 9505);
    DRM_STATE.lock().crtc_fb = 0;
    gem_mmap::unregister(handle);
}

/// The other half of the contract, so the fix above cannot creep into the
/// dumb-buffer path: a dumb-buffer fb holds an `Arc` on its VMO, so closing
/// the handle drops only the handle and the fb stays scannable until RMFB
/// -- which is what Linux does, and what
/// `gem_close_keeps_a_framebuffer_and_its_memory_alive` asserts end to end.
/// This helper must therefore refuse to touch a low-range handle at all.
#[test]
fn a_dumb_buffer_framebuffer_is_never_retired_by_this_path() {
    let _serialised = super::test_globals::lock();
    let mut state = DRM_STATE.lock();
    state.framebuffers.push(DrmFramebuffer {
        pixel_format: DRM_FORMAT_XRGB8888,
        id: 9502,
        driver_fb_id: None,
        gem_handle_id: 42, // low range: a CREATE_DUMB handle
        width: 1,
        height: 1,
        pitch: 4,
        phys_addr: 0,
        size: 4096,
        layout: ScanoutLayout::Linear,
        owner: 0,
    });
    drop(state);

    assert_eq!(retire_framebuffers_for_handle(42), 0);
    assert!(fb_exists(9502), "a dumb fb outlives its handle by design");
    DRM_STATE.lock().framebuffers.retain(|fb| fb.id != 9502);
}

/// A process exit is the case that actually matters -- a compositor that
/// crashed never sends RMFB or GEM_CLOSE. `release_process` builds its
/// `doomed` list from `state.handles`, where a nouveau handle never
/// appears, so it used to leave every nouveau-backed fb behind while
/// `nouveau_release_process` (which runs immediately after) freed the
/// memory underneath it.
#[test]
fn a_process_exit_retires_the_nouveau_framebuffers_that_process_held() {
    let _serialised = super::test_globals::lock();
    let mine = gem_mmap::DRIVER_HANDLE_BASE + 0x66;
    let theirs = gem_mmap::DRIVER_HANDLE_BASE + 0x67;
    plant_nouveau_fb(9503, mine, 78_101);
    plant_nouveau_fb(9504, theirs, 78_102);

    release_process(78_101);
    assert!(!fb_exists(9503), "the dead process's fb must be retired");
    assert!(fb_exists(9504), "another process's fb must survive");

    // And the survivor goes when its own owner exits.
    release_process(78_102);
    assert!(!fb_exists(9504));
    assert_eq!(DRM_STATE.lock().crtc_fb, 0);
    gem_mmap::unregister(mine);
    gem_mmap::unregister(theirs);
}

/// The same sweep, over the buffer an X11 GL client actually produces.
///
/// A native Wayland client's buffer has one holder, so "the dying pid
/// holds this object" and "the dying pid made this framebuffer" are the
/// same statement and the test above cannot tell them apart. Under
/// Xwayland the buffer has three: the client hands it to Xwayland, which
/// hands it on to the compositor, and the compositor is the one that
/// issues `ADDFB` over it (a direct scanout).
///
/// Keyed on holders, the client exiting -- a tab closing, a GL program
/// ending -- retired the COMPOSITOR's framebuffer and zeroed `crtc_fb`
/// with it. Every `PAGE_FLIP` and `SETCRTC` on that id then answers "no
/// such fb", which leaves wlroots with an output it cannot drive.
#[test]
fn a_client_exiting_does_not_retire_the_compositor_framebuffer_over_its_buffer() {
    let _serialised = super::test_globals::lock();
    const CLIENT: u64 = 78_201;
    const XWAYLAND: u64 = 78_202;
    const COMPOSITOR: u64 = 78_203;
    let shared = gem_mmap::DRIVER_HANDLE_BASE + 0x68;

    // One buffer, three holders, and the framebuffer belongs to the last
    // of them.
    gem_mmap::register(shared, 0x2_0000, 4096, CLIENT);
    gem_mmap::add_ref(shared, XWAYLAND);
    gem_mmap::add_ref(shared, COMPOSITOR);
    {
        let mut state = DRM_STATE.lock();
        state.framebuffers.push(DrmFramebuffer {
            pixel_format: DRM_FORMAT_XRGB8888,
            id: 9505,
            driver_fb_id: None,
            gem_handle_id: shared,
            width: 1,
            height: 1,
            pitch: 4,
            phys_addr: 0x2_0000,
            size: 4096,
            layout: ScanoutLayout::Linear,
            owner: COMPOSITOR,
        });
        state.crtc_fb = 9505;
    }

    release_process(CLIENT);
    assert!(
        fb_exists(9505),
        "a holder exiting must not take a framebuffer somebody else made"
    );
    release_process(XWAYLAND);
    assert!(fb_exists(9505), "nor the hop in the middle exiting");
    assert_eq!(
        DRM_STATE.lock().crtc_fb,
        9505,
        "the scanout framebuffer must still be the one bound to the CRTC"
    );

    // The compositor's own exit is what retires it, as before.
    release_process(COMPOSITOR);
    assert!(!fb_exists(9505));
    assert_eq!(DRM_STATE.lock().crtc_fb, 0);
    gem_mmap::unregister(shared);
}
