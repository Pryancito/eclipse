//! `VmPeak:` (peak virtual size) tests, split out of `vmar.rs` for the size
//! ratchet. Child module of `vmar`, so it reaches the region's private `inner`
//! and the shared `vm_size` meter directly.

use super::*;

/// `VmPeak:` is a high-water mark: it rises as mappings are added and does
/// NOT fall when one is removed, the way Linux's `hiwater_vm` leaves it at
/// the peak after a `munmap`. The live `VmSize` (`mapped_bytes`) does fall.
#[test]
fn the_mapped_peak_is_a_high_water_that_outlasts_unmap() {
    let vmar = VmAddressRegion::new_root();
    let flags = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
    assert_eq!(vmar.peak_mapped_bytes(), 0, "nothing mapped yet");

    vmar.map_at(0, VmObject::new_paged(2), 0, 2 * PAGE_SIZE, flags)
        .unwrap();
    assert_eq!(vmar.get_task_stats().mapped_bytes, (2 * PAGE_SIZE) as u64);
    assert_eq!(vmar.peak_mapped_bytes(), (2 * PAGE_SIZE) as u64);

    vmar.map_at(
        4 * PAGE_SIZE,
        VmObject::new_paged(3),
        0,
        3 * PAGE_SIZE,
        flags,
    )
    .unwrap();
    assert_eq!(vmar.get_task_stats().mapped_bytes, (5 * PAGE_SIZE) as u64);
    assert_eq!(
        vmar.peak_mapped_bytes(),
        (5 * PAGE_SIZE) as u64,
        "peak rose to the new total"
    );

    // Drop the 3-page mapping: VmSize falls back to 2 pages, VmPeak stays.
    vmar.unmap(vmar.addr() + 4 * PAGE_SIZE, 3 * PAGE_SIZE)
        .unwrap();
    assert_eq!(
        vmar.get_task_stats().mapped_bytes,
        (2 * PAGE_SIZE) as u64,
        "VmSize fell with the unmap"
    );
    assert_eq!(
        vmar.peak_mapped_bytes(),
        (5 * PAGE_SIZE) as u64,
        "VmPeak stayed at the high-water"
    );
    assert!(
        vmar.peak_mapped_bytes() >= vmar.get_task_stats().mapped_bytes,
        "VmPeak is never below VmSize"
    );
}

/// The regression guard for the bug the QEMU A/B caught: a process address
/// space is a VMAR tree (the ELF loader puts the executable and the vDSO in
/// child VMARs), so a mapping in a child must count toward the one `VmSize`
/// and `VmPeak:` read off the root. A per-region counter missed the children
/// and read the peak short; the shared meter does not.
#[test]
fn a_child_region_s_mappings_count_toward_the_whole_space() {
    let vmar = VmAddressRegion::new_root();
    let flags = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;

    // A sub-region, like the one the loader carves out for the executable —
    // placed past where the root's own mapping goes, so they do not overlap.
    let child = vmar
        .allocate_at(
            16 * PAGE_SIZE,
            8 * PAGE_SIZE,
            VmarFlags::CAN_MAP_RXW,
            PAGE_SIZE,
        )
        .unwrap();

    // The child shares the parent's meter — one meter per address space.
    let root_meter = vmar.inner.lock().as_ref().unwrap().vm_size.clone();
    let child_meter = child.inner.lock().as_ref().unwrap().vm_size.clone();
    assert!(
        Arc::ptr_eq(&root_meter, &child_meter),
        "a child shares its parent's address-space meter"
    );

    // Map two pages into the root and three into the child.
    vmar.map_at(0, VmObject::new_paged(2), 0, 2 * PAGE_SIZE, flags)
        .unwrap();
    child
        .map_at(0, VmObject::new_paged(3), 0, 3 * PAGE_SIZE, flags)
        .unwrap();

    // The root's VmSize and VmPeak both span the whole tree: 2 + 3 pages. A
    // root-only counter would have reported 2 pages and missed the child.
    assert_eq!(
        vmar.get_task_stats().mapped_bytes,
        (5 * PAGE_SIZE) as u64,
        "VmSize spans root + child"
    );
    assert_eq!(
        vmar.peak_mapped_bytes(),
        (5 * PAGE_SIZE) as u64,
        "VmPeak spans root + child"
    );

    // Unmapping the child's pages leaves the peak at the tree's high-water.
    child.unmap(child.addr(), 3 * PAGE_SIZE).unwrap();
    assert_eq!(
        vmar.get_task_stats().mapped_bytes,
        (2 * PAGE_SIZE) as u64,
        "VmSize fell when the child's pages went"
    );
    assert_eq!(
        vmar.peak_mapped_bytes(),
        (5 * PAGE_SIZE) as u64,
        "VmPeak held the tree's high-water across the child's unmap"
    );
}

/// The running mapped total the peak is derived from never drifts from a
/// walk of the live mappings — through a plain map, a split unmap that cuts
/// a hole in the middle of a mapping, and the removal of the pieces.
#[test]
fn the_mapped_total_matches_a_walk_through_map_unmap_and_split() {
    let vmar = VmAddressRegion::new_root();
    let flags = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
    let running = || vmar.inner.lock().as_ref().unwrap().vm_size.live() as u64;
    let walk = || vmar.get_task_stats().mapped_bytes;

    vmar.map_at(0, VmObject::new_paged(6), 0, 6 * PAGE_SIZE, flags)
        .unwrap();
    assert_eq!(running(), walk(), "after a plain map");

    // Punch a 2-page hole in the middle: the mapping splits in two and the
    // total drops by exactly the hole.
    vmar.unmap(vmar.addr() + 2 * PAGE_SIZE, 2 * PAGE_SIZE)
        .unwrap();
    assert_eq!(running(), walk(), "after a split unmap");
    assert_eq!(
        walk(),
        (4 * PAGE_SIZE) as u64,
        "6 pages minus a 2-page hole"
    );

    // Remove the two surviving pieces.
    vmar.unmap(vmar.addr(), 2 * PAGE_SIZE).unwrap();
    vmar.unmap(vmar.addr() + 4 * PAGE_SIZE, 2 * PAGE_SIZE)
        .unwrap();
    assert_eq!(running(), walk(), "after removing the rest");
    assert_eq!(walk(), 0, "nothing left mapped");
    assert_eq!(
        vmar.peak_mapped_bytes(),
        (6 * PAGE_SIZE) as u64,
        "the peak held the original full size throughout"
    );
}

/// `execve` reuses the address space through `vmar.clear()`, and Linux
/// gives the new image a fresh `mm`, so `VmPeak:` must start over from the
/// new image rather than carry the old one's high-water. `clear()` resets
/// the peak to the (empty) live total; a plain `munmap` (the tests above)
/// does not.
#[test]
fn clear_resets_the_mapped_peak_the_way_execve_does() {
    let vmar = VmAddressRegion::new_root();
    let flags = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
    vmar.map_at(0, VmObject::new_paged(4), 0, 4 * PAGE_SIZE, flags)
        .unwrap();
    assert_eq!(vmar.peak_mapped_bytes(), (4 * PAGE_SIZE) as u64);
    vmar.clear().unwrap();
    assert_eq!(
        vmar.peak_mapped_bytes(),
        0,
        "a fresh address space has no peak yet"
    );
    assert_eq!(vmar.get_task_stats().mapped_bytes, 0);
}
