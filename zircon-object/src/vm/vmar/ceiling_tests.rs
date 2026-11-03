//! The ceiling: a mapping has to end at or below `max_offset`, whoever chose
//! the address.
//!
//! This is what a 32-bit process needs, and it is a property of placement
//! rather than of any one call, so there are four doors to hold shut and each
//! one is its own test: a kernel-chosen `mmap`, an explicit `MAP_FIXED`
//! placement (which takes a different branch again depending on `overwrite`),
//! a `mremap` that moves, and a `mremap` that grows in place without choosing
//! an address at all.
//!
//! Everything here is measured in **offsets** from `vmar.addr()`. The
//! absolute number that matters --- `TASK_SIZE_32`, `0xffffe000`, measured
//! from a 32-bit task on this container's Linux --- lives with the caller, in
//! `linux_syscall::vm`; what belongs here is only that a ceiling is obeyed,
//! and the root VMAR of these tests is nowhere near 4 GiB wide in every
//! configuration.
//!
//! The two errnos are part of the contract and not an accident of the code:
//! above the ceiling is `NO_MEMORY` (Linux's `ENOMEM`, which is what a 32-bit
//! `MAP_FIXED` at `0xffffe000` actually answers) and outside the region or
//! misaligned stays `INVALID_ARGS` (`EINVAL`). A caller probing for a usable
//! address has to be able to tell "not here" from "never".

use super::*;

const RW: MMUFlags = MMUFlags::from_bits_truncate(
    MMUFlags::READ.bits() | MMUFlags::WRITE.bits() | MMUFlags::USER.bits(),
);

/// The ceiling these tests use: 16 KiB into the region.
const CEIL: usize = 0x4000;

/// A root VMAR with one page mapped at offset 0, so that `find_free_area`'s
/// "just above the highest mapping" fast path has something to be just above.
fn a_root() -> Arc<VmAddressRegion> {
    let vmar = VmAddressRegion::new_root();
    let vmo = VmObject::new_paged(1);
    vmar.map_at(0, vmo, 0, PAGE_SIZE, RW).unwrap();
    vmar
}

/// `map_ext_within` for a `len`-byte anonymous mapping, with the usual Linux
/// mmap arguments and `CEIL` as the ceiling.
fn map_within(
    vmar: &Arc<VmAddressRegion>,
    at: Option<usize>,
    len: usize,
    overwrite: bool,
) -> ZxResult<VirtAddr> {
    vmar.map_ext_within(
        at,
        VmObject::new_paged(pages(len)),
        0,
        len,
        RW,
        RW,
        overwrite,
        false,
        false,
        0,
        CEIL,
    )
}

/// The same request with no ceiling at all, which is what every non-Linux
/// caller and every 64-bit process gets.
fn map_min(
    vmar: &Arc<VmAddressRegion>,
    at: Option<usize>,
    len: usize,
    overwrite: bool,
) -> ZxResult<VirtAddr> {
    vmar.map_ext_min(
        at,
        VmObject::new_paged(pages(len)),
        0,
        len,
        RW,
        RW,
        overwrite,
        false,
        false,
        0,
    )
}

#[test]
fn a_kernel_chosen_placement_fits_under_the_ceiling_and_then_runs_out() {
    let vmar = a_root();
    let base = vmar.addr();

    // 0x1000..0x3000: the fast path's candidate, and it fits.
    let first = map_within(&vmar, None, 0x2000, false).unwrap();
    assert_eq!(first, base + 0x1000);

    // 0x3000..0x4000 is the only hole left below the ceiling, so one page
    // fits and two do not.
    let second = map_within(&vmar, None, PAGE_SIZE, false).unwrap();
    assert_eq!(second, base + 0x3000);
    assert_eq!(
        map_within(&vmar, None, PAGE_SIZE, false),
        Err(ZxError::NO_MEMORY),
        "a kernel-chosen placement went above the ceiling instead of failing"
    );
}

#[test]
fn a_kernel_chosen_placement_does_not_step_over_the_ceiling_to_find_room() {
    let vmar = a_root();

    // The region is wide open above `CEIL`, and the hole below it is one page
    // short, so a search that ignored the ceiling would succeed here. That is
    // the mutation this test exists for.
    assert_eq!(
        map_within(&vmar, None, CEIL, false),
        Err(ZxError::NO_MEMORY),
        "the search left the ceiling to find a hole that fit"
    );
    assert!(
        map_min(&vmar, None, CEIL, false).is_ok(),
        "and without a ceiling the very same request has room"
    );
}

#[test]
fn an_explicit_placement_above_the_ceiling_is_no_memory() {
    // Both branches: `overwrite` validates the explicit offset itself, and
    // without it the check is `determine_offset`'s.
    for overwrite in [false, true] {
        let vmar = a_root();
        assert_eq!(
            map_within(&vmar, Some(CEIL), PAGE_SIZE, overwrite),
            Err(ZxError::NO_MEMORY),
            "MAP_FIXED at the ceiling was accepted (overwrite = {})",
            overwrite
        );
        // Straddling it is the same answer: the ceiling is about the whole
        // range, not about where it starts.
        assert_eq!(
            map_within(&vmar, Some(CEIL - PAGE_SIZE), 0x2000, overwrite),
            Err(ZxError::NO_MEMORY),
            "MAP_FIXED straddling the ceiling was accepted (overwrite = {})",
            overwrite
        );
        // And the last page below it is accepted, so the refusals above are
        // the ceiling and not something else refusing everything.
        assert_eq!(
            map_within(&vmar, Some(CEIL - PAGE_SIZE), PAGE_SIZE, overwrite),
            Ok(vmar.addr() + CEIL - PAGE_SIZE),
            "the page ending exactly at the ceiling was refused (overwrite = {})",
            overwrite
        );
    }
}

#[test]
fn outside_the_region_and_misaligned_keep_their_own_errno() {
    for overwrite in [false, true] {
        let vmar = a_root();
        let past_the_region = vmar.size;
        assert_eq!(
            map_within(&vmar, Some(past_the_region), PAGE_SIZE, overwrite),
            Err(ZxError::INVALID_ARGS),
            "a placement outside the region came back as ENOMEM, which a \
             caller reads as 'try elsewhere' (overwrite = {})",
            overwrite
        );
        assert_eq!(
            map_within(&vmar, Some(0x1800), PAGE_SIZE, overwrite),
            Err(ZxError::INVALID_ARGS),
            "a misaligned placement under the ceiling came back as ENOMEM \
             (overwrite = {})",
            overwrite
        );
    }
}

#[test]
fn map_ext_min_has_no_ceiling_of_its_own() {
    let vmar = a_root();
    // The placement the ceiling refused above, through the call every other
    // caller in the tree uses: `map_ext_min` passes `self.size`, and a
    // ceiling that leaked into it would break the ELF loader and the vDSO.
    assert_eq!(
        map_min(&vmar, Some(CEIL), PAGE_SIZE, false),
        Ok(vmar.addr() + CEIL)
    );
}

#[test]
fn a_mremap_that_moves_stays_under_the_ceiling() {
    let vmar = a_root();
    let base = vmar.addr();

    // One page at 0x1000 with the page after it blocked, so growing it has to
    // move.
    let vmo = VmObject::new_paged(1);
    vmar.map_at(0x1000, vmo, 0, PAGE_SIZE, RW).unwrap();
    let blocker = VmObject::new_paged(1);
    vmar.map_at(0x2000, blocker, 0, PAGE_SIZE, RW).unwrap();

    // 0x3000..0x4000 is the one free page below the ceiling, and the move
    // needs two.
    assert_eq!(
        vmar.remap_within(base + 0x1000, PAGE_SIZE, 0x2000, true, None, CEIL),
        Err(ZxError::NO_MEMORY),
        "a moving mremap chose an address above the ceiling"
    );
    // Without the ceiling it moves, so the refusal is the ceiling's.
    assert!(vmar
        .remap_within(base + 0x1000, PAGE_SIZE, 0x2000, true, None, vmar.size)
        .is_ok());
}

#[test]
fn a_fixed_mremap_target_above_the_ceiling_is_no_memory() {
    let vmar = a_root();
    let base = vmar.addr();
    let vmo = VmObject::new_paged(1);
    vmar.map_at(0x1000, vmo, 0, PAGE_SIZE, RW).unwrap();

    assert_eq!(
        vmar.remap_within(
            base + 0x1000,
            PAGE_SIZE,
            PAGE_SIZE,
            true,
            Some(base + CEIL),
            CEIL
        ),
        Err(ZxError::NO_MEMORY),
        "MREMAP_FIXED placed a mapping at the ceiling"
    );
    // The in-region test comes first and keeps its own errno.
    assert_eq!(
        vmar.remap_within(
            base + 0x1000,
            PAGE_SIZE,
            PAGE_SIZE,
            true,
            Some(base + vmar.size),
            CEIL
        ),
        Err(ZxError::INVALID_ARGS)
    );
}

#[test]
fn a_mremap_that_grows_in_place_cannot_grow_through_the_ceiling() {
    let vmar = a_root();
    let base = vmar.addr();

    // A page ending exactly at the ceiling, with nothing after it: the
    // in-place grow path takes no address decision at all, so it is the one
    // door the `find_free_area` ceiling does not cover.
    let vmo = VmObject::new_paged(1);
    vmar.map_at(CEIL - PAGE_SIZE, vmo, 0, PAGE_SIZE, RW)
        .unwrap();

    assert_eq!(
        vmar.remap_within(
            base + CEIL - PAGE_SIZE,
            PAGE_SIZE,
            0x2000,
            false,
            None,
            CEIL
        ),
        Err(ZxError::NO_MEMORY),
        "an in-place mremap grew a mapping straight through the ceiling"
    );
    // With MAYMOVE it is allowed to succeed, but only by moving somewhere
    // that fits: there is room from 0x1000 to the ceiling.
    let moved = vmar
        .remap_within(base + CEIL - PAGE_SIZE, PAGE_SIZE, 0x2000, true, None, CEIL)
        .unwrap();
    assert!(
        moved + 0x2000 <= base + CEIL,
        "MAYMOVE put the grown mapping at {:#x}, which ends above the ceiling",
        moved
    );
    // And without a ceiling the same request grows where it stands.
    let vmar = a_root();
    let vmo = VmObject::new_paged(1);
    vmar.map_at(CEIL - PAGE_SIZE, vmo, 0, PAGE_SIZE, RW)
        .unwrap();
    assert_eq!(
        vmar.remap_within(
            vmar.addr() + CEIL - PAGE_SIZE,
            PAGE_SIZE,
            0x2000,
            false,
            None,
            vmar.size
        ),
        Ok(vmar.addr() + CEIL - PAGE_SIZE)
    );
}
