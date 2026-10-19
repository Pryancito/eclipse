//! `mapped_bytes_in` is what `RLIMIT_AS` is checked against: every mapping
//! in the range counted whole, committed or not, child VMARs included.
use super::*;

fn map(vmar: &Arc<VmAddressRegion>, page: usize, pages: usize) {
    let vmo = VmObject::new_paged(pages);
    vmar.map_at(page * PAGE_SIZE, vmo, 0, pages * PAGE_SIZE, MMUFlags::READ)
        .unwrap();
}

#[test]
fn every_mapping_counts_whole_and_nothing_is_committed() {
    let vmar = VmAddressRegion::new_root_zircon();
    let base = vmar.addr();
    map(&vmar, 0, 3);
    map(&vmar, 10, 2);
    assert_eq!(vmar.mapped_bytes_in(0, usize::MAX), 5 * PAGE_SIZE);
    assert_eq!(
        vmar.mapped_bytes_in(base, base + 12 * PAGE_SIZE),
        5 * PAGE_SIZE
    );
}

#[test]
fn a_range_counts_only_what_falls_inside_it() {
    let vmar = VmAddressRegion::new_root_zircon();
    let base = vmar.addr();
    map(&vmar, 0, 3);
    map(&vmar, 10, 2);
    // The last page of the first mapping and the gap after it.
    let page = |n: usize| base + n * PAGE_SIZE;
    assert_eq!(vmar.mapped_bytes_in(page(2), page(10)), PAGE_SIZE);
    // The gap alone.
    assert_eq!(vmar.mapped_bytes_in(page(3), page(10)), 0);
    // The first page of the second mapping.
    assert_eq!(vmar.mapped_bytes_in(page(5), page(11)), PAGE_SIZE);
    // An empty range.
    assert_eq!(vmar.mapped_bytes_in(page(1), page(1)), 0);
}

#[test]
fn a_child_vmar_s_mappings_count_too() {
    let vmar = VmAddressRegion::new_root_zircon();
    let child = vmar
        .allocate_at(0, 8 * PAGE_SIZE, VmarFlags::CAN_MAP_RXW, PAGE_SIZE)
        .unwrap();
    map(&child, 1, 4);
    map(&vmar, 20, 1);
    assert_eq!(vmar.mapped_bytes_in(0, usize::MAX), 5 * PAGE_SIZE);
}
