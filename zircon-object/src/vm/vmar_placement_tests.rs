//! Where an explicit offset may and may not place a region, split out of
//! `vmar.rs` for the size ratchet.

use super::*;
use crate::vm::VmObject;

/// An explicit offset so large that `offset + len` wraps used to reach
/// `test_map` unchecked, and its `BTreeMap::range(begin..end)` with
/// `end < begin` panicked the kernel. Found by tools/sysfuzz as
/// `shmat(id, 0xfffffffffffffff5, SHM_RND)` (rounded to the last page).
#[test]
fn an_offset_that_wraps_is_refused_and_does_not_panic() {
    let root = VmAddressRegion::new_root();
    // One mapping, so the overlap probe has something to range over.
    root.map(None, VmObject::new_paged(1), 0, PAGE_SIZE, MMUFlags::READ)
        .expect("an ordinary mapping");
    let last_page = usize::MAX & !(PAGE_SIZE - 1);
    for offset in [last_page, last_page - PAGE_SIZE, usize::MAX / 2 + 1] {
        let wrapping = VmObject::new_paged(2);
        assert_eq!(
            root.map(Some(offset), wrapping, 0, 2 * PAGE_SIZE, MMUFlags::READ)
                .err(),
            Some(ZxError::INVALID_ARGS),
            "offset {offset:#x}"
        );
        assert!(!root.range_is_free(root.addr().wrapping_add(offset), 2 * PAGE_SIZE));
    }
    assert_eq!(
        root.allocate(
            Some(last_page),
            2 * PAGE_SIZE,
            VmarFlags::CAN_MAP_RXW,
            PAGE_SIZE
        )
        .err(),
        Some(ZxError::INVALID_ARGS)
    );
}

/// `ZX_VM_ALIGN_*` names where a sub-region STARTS, not how long it is.
/// `determine_offset` required the length to be a multiple of the
/// alignment too, so `zx_vmar_allocate(ZX_VM_ALIGN_1MB, size = one page)`
/// -- a legal request -- was `INVALID_ARGS`.
#[test]
fn a_sub_region_is_aligned_by_its_base_and_not_by_its_length() {
    let root = VmAddressRegion::new_root();
    const ALIGN: usize = 1 << 20;
    let child = root
        .allocate(None, PAGE_SIZE, VmarFlags::CAN_MAP_RXW, ALIGN)
        .expect("a megabyte-aligned page is a legal region");
    assert_eq!(child.addr() % ALIGN, 0, "the base carries the alignment");
    assert_eq!(child.size, PAGE_SIZE, "the length is the one asked for");

    // A length that is not even a whole page is still refused.
    assert_eq!(
        root.allocate(None, PAGE_SIZE - 1, VmarFlags::CAN_MAP_RXW, ALIGN)
            .err(),
        Some(ZxError::INVALID_ARGS)
    );
    // And an explicit offset still has to carry the alignment itself.
    assert_eq!(
        root.allocate(
            Some(child.addr() - root.addr() + PAGE_SIZE),
            PAGE_SIZE,
            VmarFlags::CAN_MAP_RXW,
            ALIGN
        )
        .err(),
        Some(ZxError::INVALID_ARGS)
    );
}
