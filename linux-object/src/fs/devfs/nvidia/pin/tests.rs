extern crate std;

use super::*;
use zircon_object::ZxError;

/// `pages` pages of a fresh object mapped at a host address `vmar` covers,
/// with `flags`, none of them committed: the object, and the address.
fn mapped(vmar: &Arc<VmAddressRegion>, pages: usize, flags: MMUFlags) -> (Arc<VmObject>, usize) {
    let vmo = VmObject::new_paged(pages);
    let layout = core::alloc::Layout::from_size_align(pages * PAGE_SIZE, PAGE_SIZE).unwrap();
    let user = unsafe { std::alloc::alloc_zeroed(layout) } as usize;
    vmar.map_at(user - vmar.addr(), vmo.clone(), 0, pages * PAGE_SIZE, flags)
        .unwrap();
    (vmo, user)
}

/// [`mapped`], but as an anonymous mmap is: nothing in the page table until
/// the first fault.
fn lazily_mapped(vmar: &Arc<VmAddressRegion>, pages: usize) -> usize {
    let vmo = VmObject::new_paged(pages);
    let layout = core::alloc::Layout::from_size_align(pages * PAGE_SIZE, PAGE_SIZE).unwrap();
    let user = unsafe { std::alloc::alloc_zeroed(layout) } as usize;
    let (map_range, allow_faults) = (false, false);
    vmar.map_ext(
        Some(user - vmar.addr()),
        vmo,
        0,
        pages * PAGE_SIZE,
        MMUFlags::RXW,
        rw(),
        false,
        map_range,
        allow_faults,
    )
    .unwrap();
    user
}

fn rw() -> MMUFlags {
    MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER
}

fn ro() -> MMUFlags {
    MMUFlags::READ | MMUFlags::USER
}

#[test]
fn pinned_pages_stay_until_the_rm_lets_them_go() {
    let vmar = VmAddressRegion::new_root();
    let (vmo, user) = mapped(&vmar, 3, rw());
    let pages = pin_user_pages(&vmar, user, 3, true).unwrap();
    assert_eq!(pages.va, user);
    assert!(pages.writable);
    let committed: Vec<u64> = (0..3)
        .map(|i| vmo.committed_paddr(i).unwrap() as u64)
        .collect();
    assert_eq!(pages.phys, committed, "each page's own frame, in order");
    assert_eq!(vmo.decommit(0, 3 * PAGE_SIZE), Err(ZxError::BAD_STATE));

    let runs = pages.keep.downcast_ref::<Pins>().unwrap();
    assert_eq!(
        runs.0.len(),
        1,
        "one run for consecutive pages of one object"
    );
    assert_eq!((runs.0[0].1, runs.0[0].2), (0, 3 * PAGE_SIZE));

    drop(pages);
    vmo.decommit(0, 3 * PAGE_SIZE).unwrap();
    core::mem::forget(vmar);
}

#[test]
fn a_page_pinned_for_writing_is_mapped_for_writing() {
    let vmar = VmAddressRegion::new_root();
    let user = lazily_mapped(&vmar, 1);
    assert!(vmar.get_vaddr_flags(user).is_err(), "not touched yet");
    let pages = pin_user_pages(&vmar, user, 1, true).unwrap();
    // The process writes the very frame the GPU was handed.
    let mapping = vmar.find_mapping(user).unwrap();
    let (paddr, flags, _) = mapping.query_vaddr(user).expect("faulted in");
    assert_eq!(paddr as u64, pages.phys[0]);
    assert!(flags.contains(MMUFlags::WRITE));
    core::mem::forget(vmar);
}

#[test]
fn read_only_memory_is_pinned_for_reading_only() {
    let vmar = VmAddressRegion::new_root();
    let (vmo, user) = mapped(&vmar, 2, ro());
    assert_eq!(
        pin_user_pages(&vmar, user, 2, true).err(),
        Some(LxError::EFAULT)
    );
    vmo.decommit(0, 2 * PAGE_SIZE).unwrap();
    let pages = pin_user_pages(&vmar, user, 2, false).unwrap();
    assert!(!pages.writable);
    assert_eq!(vmo.decommit(0, PAGE_SIZE), Err(ZxError::BAD_STATE));
    core::mem::forget(vmar);
}

#[test]
fn inaccessible_memory_is_not_pinned() {
    let vmar = VmAddressRegion::new_root();
    // PROT_NONE.
    let (_vmo, user) = mapped(&vmar, 1, MMUFlags::USER);
    assert_eq!(
        pin_user_pages(&vmar, user, 1, false).err(),
        Some(LxError::EFAULT)
    );
    core::mem::forget(vmar);
}

#[test]
fn a_range_running_off_its_mapping_pins_nothing() {
    let vmar = VmAddressRegion::new_root();
    let (vmo, user) = mapped(&vmar, 2, rw());
    assert_eq!(
        pin_user_pages(&vmar, user, 3, true).err(),
        Some(LxError::EFAULT)
    );
    // The two pages that were pinned on the way are not any more.
    vmo.decommit(0, 2 * PAGE_SIZE).unwrap();
    core::mem::forget(vmar);
}

#[test]
fn an_unaligned_or_empty_range_is_refused() {
    let vmar = VmAddressRegion::new_root();
    // Two pages, so that a page's worth from the unaligned address is
    // still inside the object.
    let (_vmo, user) = mapped(&vmar, 2, rw());
    assert_eq!(
        pin_user_pages(&vmar, user + 8, 1, false).err(),
        Some(LxError::EINVAL)
    );
    assert_eq!(
        pin_user_pages(&vmar, user, 0, false).err(),
        Some(LxError::EINVAL)
    );
    core::mem::forget(vmar);
}

#[test]
fn device_memory_is_not_pinned() {
    let vmar = VmAddressRegion::new_root();
    // A window onto a frame that exists, so the host can map it.
    let backing = VmObject::new_paged(1);
    backing.commit(0, PAGE_SIZE).unwrap();
    let vmo = VmObject::new_physical(backing.committed_paddr(0).unwrap(), 1);
    let layout = core::alloc::Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap();
    let user = unsafe { std::alloc::alloc_zeroed(layout) } as usize;
    vmar.map_at(user - vmar.addr(), vmo, 0, PAGE_SIZE, rw())
        .unwrap();
    assert_eq!(
        pin_user_pages(&vmar, user, 1, false).err(),
        Some(LxError::EFAULT)
    );
    core::mem::forget(vmar);
}

#[test]
fn pages_of_two_objects_are_two_runs() {
    let mut pins = Pins(Vec::new());
    let a = VmObject::new_paged(2);
    let b = VmObject::new_paged(2);
    pins.add(&a, 0);
    pins.add(&a, PAGE_SIZE);
    // Where the run left off, but of another object.
    pins.add(&b, 2 * PAGE_SIZE);
    // Of the same object, but not where the run left off.
    pins.add(&b, 0);
    let runs: Vec<_> = pins.0.iter().map(|(_, at, len)| (*at, *len)).collect();
    assert_eq!(
        runs,
        [
            (0, 2 * PAGE_SIZE),
            (2 * PAGE_SIZE, PAGE_SIZE),
            (0, PAGE_SIZE)
        ]
    );
    // Nothing of these was pinned for real.
    pins.0.clear();
}
