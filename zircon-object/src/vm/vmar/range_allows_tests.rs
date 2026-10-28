//! `range_allows`: whether a copy to or from user memory may touch every
//! page of a range. Mapped is not enough: the copy faults like the user
//! would, `VmMapping::handle_page_fault` refuses a page whose flags lack the
//! access, and the kernel has no fixup for a fault it refuses. A `write(2)`
//! from a `PROT_NONE` page and a `clock_gettime` into a `PROT_READ` one each
//! halted the machine that way.

use super::*;

const RW: MMUFlags = MMUFlags::from_bits_truncate(
    MMUFlags::READ.bits() | MMUFlags::WRITE.bits() | MMUFlags::USER.bits(),
);
const RO: MMUFlags = MMUFlags::from_bits_truncate(MMUFlags::READ.bits() | MMUFlags::USER.bits());
/// What `mmap(PROT_NONE)` records: `USER` and no access.
const NONE: MMUFlags = MMUFlags::USER;
/// What `mmap(PROT_WRITE)` records: `WRITE` without `READ`.
const WO: MMUFlags = MMUFlags::from_bits_truncate(MMUFlags::WRITE.bits() | MMUFlags::USER.bits());

/// A root VMAR with one mapping of `flags`, `pages` long, at its base.
fn a_mapping(flags: MMUFlags, pages: usize) -> (Arc<VmAddressRegion>, VirtAddr) {
    let vmar = VmAddressRegion::new_root();
    let vmo = VmObject::new_paged(pages);
    vmar.map_at(0, vmo, 0, pages * PAGE_SIZE, flags).unwrap();
    let base = vmar.addr();
    (vmar, base)
}

#[test]
fn a_read_only_page_can_be_read_and_not_written() {
    let (vmar, base) = a_mapping(RO, 1);
    assert!(vmar.range_allows(base, 8, MMUFlags::READ));
    assert!(
        !vmar.range_allows(base, 8, MMUFlags::WRITE),
        "a write into PROT_READ was allowed"
    );
    assert!(
        vmar.range_is_mapped(base, 8),
        "and it is mapped all the same"
    );
}

#[test]
fn a_page_with_no_access_can_be_neither_read_nor_written() {
    let (vmar, base) = a_mapping(NONE, 1);
    assert!(
        !vmar.range_allows(base, 1, MMUFlags::READ),
        "a read from PROT_NONE was allowed"
    );
    assert!(!vmar.range_allows(base, 1, MMUFlags::WRITE));
    assert!(vmar.range_is_mapped(base, 1));
}

/// Write implies read, as on Linux: a `PROT_WRITE` page may be read by the
/// program and copied from by `write(2)`. It used to be refused both ways,
/// the program's read a `SIGSEGV` and the kernel's a page fault.
#[test]
fn a_page_that_may_be_written_may_be_read() {
    let (vmar, base) = a_mapping(WO, 1);
    assert!(
        vmar.range_allows(base, 8, MMUFlags::READ),
        "a read from PROT_WRITE was refused"
    );
    assert!(vmar.range_allows(base, 8, MMUFlags::WRITE));
    vmar.handle_page_fault(base, MMUFlags::READ | MMUFlags::USER)
        .unwrap();
    let mapped = vmar.get_vaddr_flags(base).unwrap();
    assert!(
        mapped.contains(MMUFlags::READ),
        "a read fault mapped the page with {:?}: it grants nothing and faults again",
        mapped
    );
    // Not writable until written, as for any page a read faults in: the
    // write fault is what copy-on-write hangs off.
    assert!(!mapped.contains(MMUFlags::WRITE));
    vmar.handle_page_fault(base, MMUFlags::WRITE | MMUFlags::USER)
        .unwrap();
    assert!(vmar
        .get_vaddr_flags(base)
        .unwrap()
        .contains(MMUFlags::WRITE));
}

#[test]
fn a_writable_page_nothing_has_touched_yet_can_be_written() {
    // Demand paging: nothing is committed, the fault will commit it.
    let (vmar, base) = a_mapping(RW, 2);
    assert!(vmar.range_allows(base, 2 * PAGE_SIZE, MMUFlags::WRITE));
    assert!(vmar.range_allows(base, 2 * PAGE_SIZE, MMUFlags::READ));
}

/// One `mprotect`ed page in the middle of a writable mapping: the flags are
/// a run per protection, and the check is about every page the range
/// covers, not about the first one.
#[test]
fn a_write_that_reaches_a_protected_page_in_the_middle_is_refused() {
    let (vmar, base) = a_mapping(RW, 3);
    vmar.protect(base + PAGE_SIZE, PAGE_SIZE, RO).unwrap();
    assert!(vmar.range_allows(base, PAGE_SIZE, MMUFlags::WRITE));
    assert!(
        !vmar.range_allows(base + PAGE_SIZE - 4, 8, MMUFlags::WRITE),
        "a write running into the read-only page was allowed"
    );
    assert!(!vmar.range_allows(base, 3 * PAGE_SIZE, MMUFlags::WRITE));
    assert!(vmar.range_allows(base + 2 * PAGE_SIZE, PAGE_SIZE, MMUFlags::WRITE));
    assert!(vmar.range_allows(base, 3 * PAGE_SIZE, MMUFlags::READ));
}

/// The same with two mappings side by side: the walk goes on into the
/// second one, and its flags count too.
#[test]
fn a_write_across_two_mappings_needs_both_to_allow_it() {
    let vmar = VmAddressRegion::new_root();
    let base = vmar.addr();
    vmar.map_at(0, VmObject::new_paged(1), 0, PAGE_SIZE, RW)
        .unwrap();
    vmar.map_at(PAGE_SIZE, VmObject::new_paged(1), 0, PAGE_SIZE, RO)
        .unwrap();
    let across = base + PAGE_SIZE - 4;
    assert!(vmar.range_allows(across, 8, MMUFlags::READ));
    assert!(
        !vmar.range_allows(across, 8, MMUFlags::WRITE),
        "the second mapping's flags were not asked"
    );
    assert!(vmar.range_allows(base, PAGE_SIZE, MMUFlags::WRITE));
}

#[test]
fn a_range_with_a_hole_in_it_is_refused_whatever_the_access() {
    let vmar = VmAddressRegion::new_root();
    let base = vmar.addr();
    vmar.map_at(0, VmObject::new_paged(1), 0, PAGE_SIZE, RW)
        .unwrap();
    vmar.map_at(2 * PAGE_SIZE, VmObject::new_paged(1), 0, PAGE_SIZE, RW)
        .unwrap();
    assert!(!vmar.range_allows(base, 3 * PAGE_SIZE, MMUFlags::READ));
    assert!(!vmar.range_allows(base + PAGE_SIZE, 1, MMUFlags::WRITE));
}

#[test]
fn nothing_to_touch_is_allowed_and_a_length_that_wraps_is_not() {
    let (vmar, base) = a_mapping(NONE, 1);
    assert!(vmar.range_allows(base, 0, MMUFlags::WRITE));
    assert!(!vmar.range_allows(base, usize::MAX, MMUFlags::READ));
}

/// What the check allows is what the fault resolves, and what it refuses
/// the fault refuses: the two must ask the same question, or the check
/// either lets through a fault the kernel dies of or turns a copy that
/// would have worked into `EFAULT`.
#[test]
fn it_allows_what_a_fault_resolves_and_nothing_else() {
    for flags in [RW, RO, WO, NONE] {
        for access in [MMUFlags::READ, MMUFlags::WRITE] {
            let (vmar, base) = a_mapping(flags, 1);
            let allowed = vmar.range_allows(base, 1, access);
            let resolved = vmar.handle_page_fault(base, access).is_ok();
            assert_eq!(
                allowed, resolved,
                "{:?} on {:?}: the check says {}, the fault {}",
                access, flags, allowed, resolved
            );
        }
    }
}
