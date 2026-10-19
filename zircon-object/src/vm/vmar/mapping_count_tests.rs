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
