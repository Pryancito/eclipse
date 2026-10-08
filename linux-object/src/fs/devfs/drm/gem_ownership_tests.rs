use super::*;

const A: u64 = 91_001;
const B: u64 = 91_002;

fn plant_handle(id: u32, pid: u64) {
    let vmo = VmObject::new_paged(1);
    DRM_STATE.lock().handles.push((
        GemHandle {
            id,
            size: 4096,
            phys_addr: 0x5_0000,
        },
        vmo,
        pid,
    ));
}

fn plant_fb(fb_id: u32, owner: u64) {
    DRM_STATE.lock().framebuffers.push(DrmFramebuffer {
        pixel_format: DRM_FORMAT_XRGB8888,
        id: fb_id,
        driver_fb_id: None,
        gem_handle_id: 0,
        width: 1,
        height: 1,
        pitch: 4,
        phys_addr: 0x5_0000,
        size: 4096,
        layout: ScanoutLayout::Linear,
        owner,
    });
}

fn forget(handle: u32, fb: u32) {
    let mut state = DRM_STATE.lock();
    state.handles.retain(|(h, _, _)| h.id != handle);
    state.framebuffers.retain(|f| f.id != fb);
}

/// `ADDFB2` with someone else's handle. Building a framebuffer over it is
/// how process B gets an id it can `SETCRTC`/`PAGE_FLIP` — A's pixels onto
/// the panel, or blitted somewhere B can read.
#[test]
fn a_framebuffer_cannot_be_built_over_another_process_handle() {
    let _serialised = super::test_globals::lock();
    plant_handle(9801, A);

    assert!(
        resolve_gem_backing_for(9801, A).is_some(),
        "its owner resolves it"
    );
    assert!(
        resolve_gem_backing_for(9801, B).is_none(),
        "another process must not"
    );
    // The kernel's own paths (pid 0) still resolve everything.
    assert!(resolve_gem_backing_for(9801, 0).is_some());
    assert!(
        resolve_gem_backing(9801).is_some(),
        "and the unchecked resolver the present path uses is unchanged"
    );

    forget(9801, 0);
}

/// `RMFB` of someone else's framebuffer. Removing the compositor's
/// scanout fb makes every later SETCRTC and PAGE_FLIP on it fail, and
/// wlroots retries the modeset forever.
#[test]
fn a_framebuffer_cannot_be_removed_by_another_process() {
    let _serialised = super::test_globals::lock();
    plant_fb(9802, A);

    assert!(!rmfb_for(9802, B), "another process must not remove it");
    assert!(
        DRM_STATE.lock().framebuffers.iter().any(|f| f.id == 9802),
        "and it must still be there afterwards"
    );
    // Indistinguishable from "no such framebuffer", so a prober learns
    // nothing about what other clients own.
    assert!(!rmfb_for(9899, B));

    assert!(rmfb_for(9802, A), "its owner removes it");
    assert!(!DRM_STATE.lock().framebuffers.iter().any(|f| f.id == 9802));
}

/// `GETFB`'s handle field: the enumeration half. Linux zeroes it for a
/// non-master caller rather than failing the call, so the geometry still
/// comes back.
#[test]
fn the_backing_handle_goes_only_to_the_framebuffers_creator() {
    let _serialised = super::test_globals::lock();
    plant_fb(9803, A);
    let fb = get_fb(9803).expect("planted");

    assert!(owned_by(fb.owner, A), "its creator sees the handle");
    assert!(!owned_by(fb.owner, B), "another process gets zero");
    assert!(owned_by(fb.owner, 0), "kernel-internal callers still do");

    forget(0, 9803);
}

/// `GETRESOURCES` lists a client's own framebuffers, as Linux lists
/// `file_priv->fbs`, plus the kernel's; never another client's. The
/// whole table used to be listed to everyone, so any process could read
/// the compositor's scanout fb id off the card.
#[test]
fn the_resource_list_carries_only_the_callers_framebuffers() {
    let _serialised = super::test_globals::lock();
    plant_fb(9804, A);
    plant_fb(9805, B);
    plant_fb(9806, 0);

    let a = framebuffer_ids_for(A);
    assert!(a.contains(&9804), "A sees its own");
    assert!(!a.contains(&9805), "A must not see B's");
    assert!(a.contains(&9806), "the kernel's is everyone's");
    let b = framebuffer_ids_for(B);
    assert!(b.contains(&9805) && !b.contains(&9804) && b.contains(&9806));
    let kernel = framebuffer_ids_for(0);
    assert!(
        kernel.contains(&9804) && kernel.contains(&9805) && kernel.contains(&9806),
        "kernel-internal callers see everything"
    );

    forget(0, 9804);
    forget(0, 9805);
    forget(0, 9806);
}
