//! A VMO counts how many mappings it has, and until now the count only ever
//! went up: nothing called `VmObject::remove_mapping`, so an object that had
//! been mapped and unmapped reported sharers it no longer had.
use super::*;

fn one_page_vmo() -> Arc<VmObject> {
    VmObject::new_paged(1)
}

#[test]
fn unmapping_brings_the_share_count_back_down() {
    let vmar = VmAddressRegion::new_root_zircon();
    let vmo = one_page_vmo();
    assert_eq!(vmo.share_count(), 0, "a fresh VMO has no mappings");
    vmar.map_at(0, vmo.clone(), 0, PAGE_SIZE, MMUFlags::READ)
        .unwrap();
    assert_eq!(vmo.share_count(), 1);
    vmar.unmap(vmar.addr(), PAGE_SIZE).unwrap();
    assert_eq!(
        vmo.share_count(),
        0,
        "the mapping is gone and the VMO still counts it"
    );
}

#[test]
fn mapping_and_unmapping_over_and_over_does_not_run_the_count_up() {
    let vmar = VmAddressRegion::new_root_zircon();
    let vmo = one_page_vmo();
    for round in 0..8 {
        vmar.map_at(0, vmo.clone(), 0, PAGE_SIZE, MMUFlags::READ)
            .unwrap();
        vmar.unmap(vmar.addr(), PAGE_SIZE).unwrap();
        assert_eq!(
            vmo.share_count(),
            0,
            "round {}: the count kept what the last round mapped",
            round
        );
    }
}

/// The count is what `set_cache_policy` refuses on, so a monotonic count
/// refused for the rest of the object's life.
#[test]
fn a_vmo_that_was_mapped_and_unmapped_accepts_a_cache_policy() {
    let vmar = VmAddressRegion::new_root_zircon();
    let vmo = one_page_vmo();
    vmar.map_at(0, vmo.clone(), 0, PAGE_SIZE, MMUFlags::READ)
        .unwrap();
    assert_eq!(
        vmo.set_cache_policy(CachePolicy::Uncached),
        Err(ZxError::BAD_STATE),
        "a mapped VMO must still refuse a cache policy"
    );
    vmar.unmap(vmar.addr(), PAGE_SIZE).unwrap();
    vmo.set_cache_policy(CachePolicy::Uncached)
        .expect("nothing is mapped any more, so this must be allowed");
}

/// `clear`/`destroy_internal` unmap the range and zero the mapping's size
/// first, so `Drop for VmMapping` returns early -- which is why the count
/// has to come down before that return and not after it.
#[test]
fn destroying_the_region_brings_the_count_down_too() {
    let vmar = VmAddressRegion::new_root_zircon();
    let vmo = one_page_vmo();
    vmar.map_at(0, vmo.clone(), 0, PAGE_SIZE, MMUFlags::READ)
        .unwrap();
    assert_eq!(vmo.share_count(), 1);
    vmar.clear().unwrap();
    assert_eq!(
        vmo.share_count(),
        0,
        "the region let the mapping go and the VMO still counts it"
    );
}

/// Unmapping a hole in the middle splits one mapping into two, and `cut`
/// builds the second one by hand instead of through `VmMapping::new`, so
/// the VMO was never told about it.
#[test]
fn a_mapping_split_in_two_is_counted_twice() {
    let vmar = VmAddressRegion::new_root_zircon();
    let vmo = VmObject::new_paged(3);
    vmar.map_at(0, vmo.clone(), 0, 3 * PAGE_SIZE, MMUFlags::READ)
        .unwrap();
    assert_eq!(vmo.share_count(), 1);
    vmar.unmap(vmar.addr() + PAGE_SIZE, PAGE_SIZE).unwrap();
    assert_eq!(
        vmo.share_count(),
        2,
        "the hole left two mappings and the VMO counts one"
    );
    vmar.clear().unwrap();
    assert_eq!(
        vmo.share_count(),
        0,
        "both halves are gone and the VMO still counts one"
    );
}

/// Two live mappings of one object is what the `share_count() > 1` guards
/// are for, and dropping one of them has to leave the other counted.
#[test]
fn dropping_one_of_two_mappings_leaves_the_other_counted() {
    let vmar = VmAddressRegion::new_root_zircon();
    let vmo = one_page_vmo();
    vmar.map_at(0, vmo.clone(), 0, PAGE_SIZE, MMUFlags::READ)
        .unwrap();
    vmar.map_at(2 * PAGE_SIZE, vmo.clone(), 0, PAGE_SIZE, MMUFlags::READ)
        .unwrap();
    assert_eq!(vmo.share_count(), 2);
    vmar.unmap(vmar.addr(), PAGE_SIZE).unwrap();
    assert_eq!(
        vmo.share_count(),
        1,
        "one mapping left, and the VMO says otherwise"
    );
}

fn meter(vmar: &Arc<VmAddressRegion>) -> Arc<super::vmar_vm_size::VmSizeMeter> {
    vmar.inner.lock().as_ref().unwrap().vm_size.clone()
}

/// `vm.max_map_count`: the address space counts its mappings, the whole
/// tree's, and refuses the one past 65530 with NO_MEMORY (ENOMEM), as
/// `mmap_region` does -- and a hole punched in the middle of a mapping,
/// which leaves two, as `__split_vma` does.
#[test]
fn the_mappings_of_an_address_space_stop_at_max_map_count() {
    use super::vmar_vm_size::MAX_MAP_COUNT;
    let vmar = VmAddressRegion::new_root_zircon();
    let base = vmar.addr();
    let m = meter(&vmar);
    vmar.map_at(0, VmObject::new_paged(3), 0, 3 * PAGE_SIZE, MMUFlags::READ)
        .unwrap();
    let child = vmar
        .allocate_at(
            16 * PAGE_SIZE,
            4 * PAGE_SIZE,
            VmarFlags::CAN_MAP_RXW,
            PAGE_SIZE,
        )
        .unwrap();
    child
        .map_at(0, one_page_vmo(), 0, PAGE_SIZE, MMUFlags::READ)
        .unwrap();
    assert_eq!(m.count(), 2, "a child region's mappings count too");

    // Stand in for 65527 more without making them.
    m.mapped(MAX_MAP_COUNT - 3);
    vmar.map_at(8 * PAGE_SIZE, one_page_vmo(), 0, PAGE_SIZE, MMUFlags::READ)
        .unwrap();
    assert_eq!(
        vmar.map_at(10 * PAGE_SIZE, one_page_vmo(), 0, PAGE_SIZE, MMUFlags::READ)
            .err(),
        Some(ZxError::NO_MEMORY)
    );
    // A hole in the middle needs one more: refused, and nothing is cut.
    assert_eq!(
        vmar.unmap(base + PAGE_SIZE, PAGE_SIZE).err(),
        Some(ZxError::NO_MEMORY)
    );
    assert_eq!(
        vmar.mapped_bytes_in(base, base + 3 * PAGE_SIZE),
        3 * PAGE_SIZE
    );
    // Trimming an edge leaves one, and unmapping gives room back.
    vmar.unmap(base, PAGE_SIZE).unwrap();
    vmar.unmap(base + 8 * PAGE_SIZE, PAGE_SIZE).unwrap();
    assert_eq!(m.count(), MAX_MAP_COUNT - 1);
    vmar.unmap(base + 2 * PAGE_SIZE, PAGE_SIZE).unwrap();
    m.unmapped(MAX_MAP_COUNT - 3);
    assert_eq!(m.count(), 2);
    child.destroy().unwrap();
    assert_eq!(m.count(), 1, "a region torn down takes its mappings off");
}
