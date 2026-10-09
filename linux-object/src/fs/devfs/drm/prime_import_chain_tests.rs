use super::*;

const CLIENT: u64 = 91_101;
const XWAYLAND: u64 = 91_102;
const STRANGER: u64 = 91_103;

fn forget_handles(ids: &[u32]) {
    let mut state = DRM_STATE.lock();
    state.handles.retain(|(h, _, _)| !ids.contains(&h.id));
}

/// An imported dma-buf belongs to the process that imported it, so that
/// process can use it and hand it on. Without this an importer would hold
/// a handle it is not allowed to resolve — a buffer it can name and
/// nothing else — which is how the middle of the chain breaks while both
/// ends look fine.
#[test]
fn an_imported_dmabuf_belongs_to_the_importer() {
    let _serialised = super::test_globals::lock();
    // The client's own buffer, exported and then imported by Xwayland.
    // `import_dmabuf` reads the caller from the current thread, which a
    // host test does not have (pid 0), so the entry it makes is planted
    // here with the importer's pid, exactly as it would be made on the
    // target.
    let imported = 9_901;
    let vmo = VmObject::new_paged(1);
    DRM_STATE.lock().handles.push((
        GemHandle {
            id: imported,
            size: 4096,
            phys_addr: 0x7_0000,
        },
        vmo,
        XWAYLAND,
    ));

    assert!(
        resolve_gem_backing_for(imported, XWAYLAND).is_some(),
        "the importer can resolve what it imported, and so re-export it"
    );
    assert!(
        resolve_gem_backing_for(imported, STRANGER).is_none(),
        "a process that imported nothing still gets nothing"
    );
    assert!(
        resolve_gem_backing_for(imported, CLIENT).is_none(),
        "not even the process that exported it in the first place: its own \
         handle is a separate entry, with its own lifetime"
    );

    forget_handles(&[imported]);
}

/// Two processes importing the same dma-buf get two handles, each its
/// own. Closing one must not disturb the other — the compositor releasing
/// a frame cannot invalidate Xwayland's handle on the same memory.
#[test]
fn two_importers_of_one_buffer_hold_independent_handles() {
    let _serialised = super::test_globals::lock();
    let phys = 0x7_1000;
    let (xwl_handle, comp_handle) = (9_902, 9_903);
    for (id, pid) in [(xwl_handle, XWAYLAND), (comp_handle, STRANGER)] {
        let vmo = VmObject::new_paged(1);
        DRM_STATE.lock().handles.push((
            GemHandle {
                id,
                size: 4096,
                phys_addr: phys,
            },
            vmo,
            pid,
        ));
    }

    assert!(resolve_gem_backing_for(xwl_handle, XWAYLAND).is_some());
    assert!(resolve_gem_backing_for(comp_handle, STRANGER).is_some());
    assert!(
        resolve_gem_backing_for(comp_handle, XWAYLAND).is_none(),
        "neither importer can name the other's handle"
    );

    forget_handles(&[xwl_handle]);
    assert!(
        resolve_gem_backing_for(comp_handle, STRANGER).is_some(),
        "one importer letting go leaves the other's handle intact"
    );

    forget_handles(&[comp_handle]);
}
