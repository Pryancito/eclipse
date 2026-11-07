//! The vDSO's code mapping stays what it is: `syscall_may_come_from`
//! trusts the recorded range, so Zircon lets nothing unmap, protect or
//! map over it, and lets a process map the vDSO's code once.
use super::*;

fn root_with_vdso_code() -> (Arc<VmAddressRegion>, VirtAddr) {
    let root = VmAddressRegion::new_root();
    let vdso = VmObject::new_paged(2);
    let at = root
        .map(
            None,
            vdso,
            0,
            2 * PAGE_SIZE,
            MMUFlags::READ | MMUFlags::EXECUTE,
        )
        .unwrap();
    assert!(!root.has_vdso_code());
    root.set_vdso_code(at, 2 * PAGE_SIZE);
    assert!(root.has_vdso_code());
    (root, at)
}

#[test]
fn the_vdso_code_is_neither_unmapped_nor_protected() {
    let (root, at) = root_with_vdso_code();
    assert_eq!(root.unmap(at, PAGE_SIZE), Err(ZxError::ACCESS_DENIED));
    assert_eq!(
        root.unmap(at + PAGE_SIZE, 2 * PAGE_SIZE),
        Err(ZxError::ACCESS_DENIED),
        "a range that only overlaps the tail is refused too"
    );
    assert_eq!(
        root.protect(at, PAGE_SIZE, MMUFlags::READ),
        Err(ZxError::ACCESS_DENIED)
    );
    assert!(root.syscall_may_come_from(at + 8));
}

#[test]
fn nothing_is_mapped_over_the_vdso_code() {
    let (root, at) = root_with_vdso_code();
    let other = VmObject::new_paged(1);
    assert_eq!(
        root.map_ext(
            Some(at - root.addr()),
            other,
            0,
            PAGE_SIZE,
            MMUFlags::RXW,
            MMUFlags::READ | MMUFlags::EXECUTE,
            true,
            true,
            false,
        ),
        Err(ZxError::ACCESS_DENIED)
    );
}

#[test]
fn the_pages_next_to_the_vdso_code_are_still_free_to_go() {
    let (root, at) = root_with_vdso_code();
    let plain = VmObject::new_paged(1);
    let next = root
        .map_at(
            at + 2 * PAGE_SIZE - root.addr(),
            plain,
            0,
            PAGE_SIZE,
            MMUFlags::READ,
        )
        .unwrap();
    assert_eq!(root.unmap(next, PAGE_SIZE), Ok(()));
}
