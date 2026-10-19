//! `MAP_FIXED_NOREPLACE` asks for one exact range and must be told
//! whether it is taken, because the answer is `EEXIST` and not "here is
//! somewhere else". `map_ext_min` already refuses an occupied explicit
//! offset, but it says `INVALID_ARGS` for that, for a misaligned offset
//! and for one past the end alike, and userspace needs those apart.

use super::*;

/// A root VMAR with one page mapped at offset `PAGE_SIZE`, so there is a
/// free page on either side of it.
fn vmar_with_a_mapped_page() -> (Arc<VmAddressRegion>, VirtAddr) {
    let vmar = VmAddressRegion::new_root_zircon();
    let vmo = VmObject::new_paged(1);
    let addr = vmar.addr() + PAGE_SIZE;
    vmar.map_at(PAGE_SIZE, vmo, 0, PAGE_SIZE, MMUFlags::READ)
        .unwrap();
    (vmar, addr)
}

#[test]
fn a_page_with_a_mapping_on_it_is_not_free() {
    let (vmar, addr) = vmar_with_a_mapped_page();
    assert!(!vmar.range_is_free(addr, PAGE_SIZE));
    assert!(vmar.range_is_free(addr + PAGE_SIZE, PAGE_SIZE));
    assert!(vmar.range_is_free(addr - PAGE_SIZE, PAGE_SIZE));
}

/// The question is about the whole range, not about its first page: a
/// request that starts in a hole and runs into a mapping is not free.
/// Answering from the start address alone is what a `find_mapping(addr)`
/// check would do, and it would let `MAP_FIXED_NOREPLACE` land on top of
/// the mapping it was told to leave alone.
#[test]
fn a_range_that_runs_into_a_mapping_is_not_free_either() {
    let (vmar, addr) = vmar_with_a_mapped_page();
    assert!(!vmar.range_is_free(addr - PAGE_SIZE, 2 * PAGE_SIZE));
    assert!(!vmar.range_is_free(addr, 2 * PAGE_SIZE));
    assert!(!vmar.range_is_free(addr - PAGE_SIZE, 3 * PAGE_SIZE));
}

/// A sub-region occupies its range as much as a mapping does: nothing may
/// be placed inside one from the parent.
#[test]
fn a_sub_region_occupies_its_range_too() {
    let vmar = VmAddressRegion::new_root_zircon();
    let child = vmar.allocate_at(0, PAGE_SIZE, VmarFlags::CAN_MAP_RXW, PAGE_SIZE);
    let child = child.unwrap();
    assert!(!vmar.range_is_free(vmar.addr(), PAGE_SIZE));
    assert!(vmar.range_is_free(vmar.addr() + PAGE_SIZE, PAGE_SIZE));
    drop(child);
}

/// Outside this VMAR there is no address to give out, so nothing there is
/// free. `MAP_FIXED_NOREPLACE` leans on this: the caller checks the range
/// is inside first, because a range that is not is a placement failure
/// and not `EEXIST`.
#[test]
fn a_range_outside_the_vmar_is_never_free() {
    let vmar = VmAddressRegion::new_root_zircon();
    assert!(!vmar.range_is_free(vmar.end_addr(), PAGE_SIZE));
    assert!(!vmar.range_is_free(vmar.end_addr() - PAGE_SIZE, 2 * PAGE_SIZE));
    // Below the base, which underflows the offset rather than exceeding
    // the size.
    if vmar.addr() >= PAGE_SIZE {
        assert!(!vmar.range_is_free(vmar.addr() - PAGE_SIZE, PAGE_SIZE));
    }
    // And a range whose end wraps the address space.
    assert!(!vmar.range_is_free(vmar.addr(), usize::MAX - vmar.addr() + 1));
}

/// An empty or misaligned range is not a question this can answer, and
/// `test_map` debug-asserts its alignment, so the guards have to come
/// first. `mmap` rejects a misaligned fixed address before it gets here;
/// this is what keeps a future caller from finding out the hard way.
#[test]
fn an_empty_or_misaligned_range_is_not_free() {
    let vmar = VmAddressRegion::new_root_zircon();
    let free = vmar.addr() + PAGE_SIZE;
    assert!(vmar.range_is_free(free, PAGE_SIZE));
    assert!(!vmar.range_is_free(free, 0));
    assert!(!vmar.range_is_free(free + 1, PAGE_SIZE));
    assert!(!vmar.range_is_free(free, PAGE_SIZE - 1));
}

/// A destroyed VMAR hands out nothing.
#[test]
fn a_destroyed_vmar_has_no_free_range() {
    let vmar = VmAddressRegion::new_root_zircon();
    let addr = vmar.addr() + PAGE_SIZE;
    assert!(vmar.range_is_free(addr, PAGE_SIZE));
    vmar.destroy().unwrap();
    assert!(!vmar.range_is_free(addr, PAGE_SIZE));
}
