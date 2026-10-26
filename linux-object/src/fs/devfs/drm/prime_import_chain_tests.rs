use super::*;

const CLIENT: u64 = 91_101;
const XWAYLAND: u64 = 91_102;
const STRANGER: u64 = 91_103;

fn forget_handles(ids: &[u32]) {
    let mut state = DRM_STATE.lock();
    state.handles.retain(|(h, _, _)| !ids.contains(&h.id));
}

/// `drm_gem_prime_fd_to_handle` asks the file's prime table before it
/// imports (`drm_prime_lookup_buf_handle`), and the export fills that
/// table too ("if we've exported this buffer then cheat and add it to
/// the import list so we get the correct handle back"): re-importing
/// what one exported gives the original handle, and importing the same
/// buffer twice gives the same handle, with nothing new in the table
/// either time. Here every import minted a new handle over the same
/// frames.
#[test]
fn importing_a_buffer_the_process_already_holds_gives_that_handle() {
    let _serialised = super::test_globals::lock();
    let own = 9_911;
    let vmo = VmObject::new_paged(1);
    DRM_STATE.lock().handles.push((
        GemHandle {
            id: own,
            size: 4096,
            phys_addr: 0x8_0000,
        },
        vmo.clone(),
        CLIENT,
    ));
    let count = || DRM_STATE.lock().handles.len();
    let before = count();
    // The exporter importing its own export: its own handle, nothing new.
    assert_eq!(import_dmabuf_for(0x8_0000, 4096, vmo.clone(), CLIENT), own);
    assert_eq!(count(), before);
    // Another process: a handle of its own, minted once.
    let theirs = import_dmabuf_for(0x8_0000, 4096, vmo.clone(), XWAYLAND);
    assert_ne!(theirs, own);
    assert_eq!(count(), before + 1);
    assert_eq!(
        import_dmabuf_for(0x8_0000, 4096, vmo.clone(), XWAYLAND),
        theirs
    );
    assert_eq!(count(), before + 1);
    assert!(resolve_gem_backing_for(theirs, XWAYLAND).is_some());
    // The exporter still gets its own, not the importer's.
    assert_eq!(import_dmabuf_for(0x8_0000, 4096, vmo.clone(), CLIENT), own);
    // Another buffer of the same size at the same address is another
    // handle: the identity is the backing, not what the dma-buf says
    // about it.
    let other = VmObject::new_paged(1);
    let fresh = import_dmabuf_for(0x8_0000, 4096, other, XWAYLAND);
    assert_ne!(fresh, theirs);
    assert_eq!(count(), before + 2);
    forget_handles(&[own, theirs, fresh]);
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

/// `drm_gem_open_ioctl` from ANOTHER process: a handle of that process's
/// own over the same object, minted once (`drm_gem_handle_create_tail`),
/// while FLINK by a process that does not hold the object is ENOENT. The
/// name outlives the first handle as long as any handle stands
/// (`drm_gem_object_handle_free` runs on the LAST one) and goes with the
/// owner's exit (`drm_gem_release`). A nouveau object is named by its
/// global handle and OPEN hands that handle out with a reference of the
/// opener's, as `GETFB` does; once `gem_mmap` no longer tracks it the
/// name is ENOENT and forgotten.
#[test]
fn a_name_opens_from_another_process_and_names_a_nouveau_object_by_its_global_handle() {
    let _serialised = super::test_globals::lock();
    let own = 9_921;
    let vmo = VmObject::new_paged(1);
    DRM_STATE.lock().handles.push((
        GemHandle {
            id: own,
            size: 4096,
            phys_addr: 0x9_0000,
        },
        vmo.clone(),
        CLIENT,
    ));
    assert_eq!(
        gem_names::gem_flink_for(own, STRANGER),
        None,
        "not the stranger's handle"
    );
    let name = gem_names::gem_flink_for(own, CLIENT).expect("the owner names it");
    assert_ne!(name, 0, "0 is never a name (idr_alloc(.., 1, 0, ..))");
    assert_eq!(gem_names::gem_flink_for(own, CLIENT), Some(name));

    // Xwayland opens it: a handle of its own, the same one next time.
    let (theirs, size) = gem_names::gem_open_for(name, XWAYLAND).unwrap();
    assert_ne!(theirs, own);
    assert_eq!(size, 4096);
    assert_eq!(
        gem_names::gem_open_for(name, XWAYLAND),
        Some((theirs, 4096))
    );
    assert_eq!(
        resolve_gem_backing_for(theirs, XWAYLAND),
        Some((0x9_0000, 4096)),
        "over the same frames"
    );
    assert_eq!(
        gem_names::gem_flink_for(theirs, XWAYLAND),
        Some(name),
        "the object's one name, whichever handle"
    );

    // The owner's handle goes first: Xwayland's keeps the object, and the
    // name with it, so a third process can still open it. Closing the
    // last handle takes the name.
    assert!(gem_close(own));
    let (strangers, _) = gem_names::gem_open_for(name, STRANGER).expect("the name still stands");
    assert_ne!(strangers, theirs, "a handle of the stranger's own");
    assert!(gem_close(theirs));
    assert_eq!(
        gem_names::gem_open_for(name, XWAYLAND).map(|(h, _)| h != strangers),
        Some(true),
        "still standing on the stranger's handle"
    );
    let xwl_again = gem_names::gem_open_for(name, XWAYLAND).map(|(h, _)| h);
    assert!(gem_close(strangers));
    assert!(gem_close(xwl_again.unwrap()));
    assert_eq!(
        gem_names::gem_open_for(name, STRANGER),
        None,
        "the name died with the last handle"
    );

    // A process exiting with the only handle takes the name with it.
    let exiting = import_dmabuf_for(0x9_0000, 4096, vmo.clone(), XWAYLAND);
    let name = gem_names::gem_flink_for(exiting, XWAYLAND).unwrap();
    assert!(release_process(XWAYLAND) >= 1);
    assert_eq!(gem_names::gem_open_for(name, CLIENT), None);

    // A nouveau object: the global handle, plus a reference.
    use zcore_drivers::scheme::gem_mmap;
    const H: u32 = 0x8A00_0001;
    gem_mmap::register(H, 0x1000_0000, 8192, CLIENT);
    assert_eq!(gem_names::gem_flink_for(H, STRANGER), None);
    let name = gem_names::gem_flink_for(H, CLIENT).unwrap();
    assert_eq!(gem_names::gem_open_for(name, CLIENT), Some((H, 8192)));
    assert_eq!(
        gem_mmap::ref_count(H),
        Some(1),
        "a holder gets no second reference"
    );
    assert_eq!(gem_names::gem_open_for(name, XWAYLAND), Some((H, 8192)));
    assert!(gem_mmap::holds(H, XWAYLAND), "a new holder gets one");
    assert_eq!(gem_mmap::ref_count(H), Some(2));
    assert!(gem_mmap::unregister(H));
    assert_eq!(gem_names::gem_open_for(name, STRANGER), None, "gone");
    assert!(!forget_gem_name_of_driver_handle(H), "already forgotten");
    // The handle id comes round again on a NEW object: the old name does
    // not come back with it.
    gem_mmap::register(H, 0x2000_0000, 8192, STRANGER);
    assert_eq!(gem_names::gem_open_for(name, STRANGER), None, "forgotten");
    assert!(gem_mmap::unregister(H));
}
