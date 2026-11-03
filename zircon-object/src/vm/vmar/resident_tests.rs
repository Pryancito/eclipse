//! The resident count behind `VmHWM:`: what the address space's page table
//! counts as pages come and go, read through the VMAR.
//!
//! Each test gets an address space of its own, at a window nobody else maps
//! into: libos keeps every page of every table in one host address space and
//! one map, so two tests mapping the same address would count each other's
//! pages. Faults are taken by hand, which works whether or not the build maps
//! eagerly (`zircon-object/libos` does, the workspace pass does not).
use super::*;

const RW: MMUFlags = MMUFlags::from_bits_truncate(
    MMUFlags::READ.bits() | MMUFlags::WRITE.bits() | MMUFlags::USER.bits(),
);
const RO: MMUFlags = MMUFlags::from_bits_truncate(MMUFlags::READ.bits() | MMUFlags::USER.bits());
const K: usize = PAGE_SIZE;

/// An empty address space at window `n`, 16 MiB each from 4 GiB: under the
/// windows `aspace-separate` hands out and far above the low addresses the
/// other suites' address spaces start at.
fn space(n: usize) -> Arc<VmAddressRegion> {
    VmAddressRegion::new_root_at(0x1_0000_0000 + n * 0x100_0000, 0x100_0000)
}

/// Map `pages` fresh pages at page `at` and write each one, the way a process
/// touches what it allocated.
fn touched(vmar: &Arc<VmAddressRegion>, at: usize, pages: usize) -> VirtAddr {
    let addr = vmar
        .map_at(at * K, VmObject::new_paged(pages), 0, pages * K, RW)
        .unwrap();
    for i in 0..pages {
        vmar.handle_page_fault(addr + i * K, MMUFlags::WRITE | MMUFlags::USER)
            .unwrap();
    }
    addr
}

fn counts(vmar: &Arc<VmAddressRegion>) -> (usize, usize) {
    (vmar.resident_bytes(), vmar.resident_peak_bytes())
}

/// `VmHWM:` is the most that has been resident: it rises with each page a
/// process touches and stays where it was when the memory is given back, as
/// Linux's `hiwater_rss` does after a `munmap`.
#[test]
fn a_touched_page_is_resident_and_the_mark_outlasts_the_unmap() {
    let vmar = space(0);
    assert_eq!(counts(&vmar), (0, 0));
    let big = touched(&vmar, 0, 4);
    touched(&vmar, 8, 2);
    assert_eq!(counts(&vmar), (6 * K, 6 * K));
    vmar.unmap(big, 4 * K).unwrap();
    assert_eq!(counts(&vmar), (2 * K, 6 * K), "the mark stays");
    touched(&vmar, 0, 1);
    assert_eq!(counts(&vmar), (3 * K, 6 * K), "under the mark again");
}

/// A read of memory nobody has written maps the shared zero frame, and that
/// is not memory the process holds: Linux maps its zero page special and
/// leaves it out of RSS. Writing the page puts a page of its own there, and
/// that one counts.
#[test]
fn reading_untouched_memory_holds_nothing_until_it_is_written() {
    let vmar = space(1);
    let addr = vmar
        .map_at(0, VmObject::new_paged(2), 0, 2 * K, RO)
        .unwrap();
    vmar.handle_page_fault(addr, MMUFlags::READ | MMUFlags::USER)
        .unwrap();
    assert!(
        vmar.find_mapping(addr).unwrap().query_vaddr(addr).is_ok(),
        "the read mapped nothing, so this would pass for the wrong reason"
    );
    assert_eq!(counts(&vmar), (0, 0));

    // Whether the other page gets a frame of its own here depends on the
    // build (libos maps eagerly, and makes it one at the `mprotect`); either
    // way the count is the pages with frames of their own, which is what the
    // walk behind `VmRSS:` counts.
    vmar.protect(addr, 2 * K, RW).unwrap();
    vmar.handle_page_fault(addr, MMUFlags::WRITE | MMUFlags::USER)
        .unwrap();
    let stats = vmar.get_task_stats();
    let walked = (stats.private_bytes() + stats.shared_bytes()) as usize;
    assert!(walked >= K, "the written page has no frame of its own");
    assert_eq!(vmar.resident_bytes(), walked);
}

/// A physical VMO is a window onto a device -- a frame buffer, a GPU's
/// memory -- and Linux leaves such a mapping (`remap_pfn_range`) out of RSS.
/// Mapping it, touching it, `mprotect`ing it and taking it away again must
/// count nothing, and must not take away a page beside it that was counted.
#[test]
fn a_device_window_is_never_resident() {
    let vmar = space(2);
    touched(&vmar, 0, 1);
    let window = vmar
        .map_at(8 * K, VmObject::new_physical(0x3000_0000, 4), 0, 4 * K, RW)
        .unwrap();
    for i in 0..4 {
        vmar.handle_page_fault(window + i * K, MMUFlags::WRITE | MMUFlags::USER)
            .unwrap();
    }
    assert!(vmar
        .find_mapping(window)
        .unwrap()
        .query_vaddr(window)
        .is_ok());
    assert_eq!(counts(&vmar), (K, K));
    vmar.protect(window, 4 * K, RO).unwrap();
    vmar.unmap(window, 4 * K).unwrap();
    assert_eq!(counts(&vmar), (K, K), "the counted page is still there");
}

/// The loader puts the executable and the vDSO in child regions; one page
/// table serves the whole tree, so a page touched in a child is resident in
/// the address space, and the root answers for it.
#[test]
fn a_page_in_a_child_region_is_resident_in_the_whole_space() {
    let vmar = space(3);
    let child = vmar
        .allocate_at(16 * K, 8 * K, VmarFlags::CAN_MAP_RXW, K)
        .unwrap();
    touched(&child, 0, 3);
    touched(&vmar, 0, 1);
    assert_eq!(counts(&vmar), (4 * K, 4 * K));
    assert_eq!(counts(&child), (4 * K, 4 * K));
}

/// `execve` empties the address space and keeps it: the new image's mark
/// starts from nothing, as a new `mm`'s does in Linux.
#[test]
fn clearing_the_space_starts_the_mark_over() {
    let vmar = space(4);
    touched(&vmar, 0, 4);
    vmar.clear().unwrap();
    assert_eq!(counts(&vmar), (0, 0));
    touched(&vmar, 0, 1);
    assert_eq!(counts(&vmar), (K, K));
}
