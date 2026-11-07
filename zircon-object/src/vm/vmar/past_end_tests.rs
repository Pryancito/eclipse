//! A mapping window may reach past the end of its VMO, as in Zircon: only an
//! offset+len that overflows is refused, the pages that exist are mapped and
//! the ones past the end are left for a fault. `VmoTestCase.Commit` maps a
//! 4-page VMO at offsets page_size and 2*size, len=size, and expects both OK.
use super::*;

const PAGES: usize = 4;

fn map_window(vmar: &Arc<VmAddressRegion>, vmo: &Arc<VmObject>, vmo_offset: usize) -> ZxResult {
    vmar.map_ext(
        None,
        vmo.clone(),
        vmo_offset,
        PAGES * PAGE_SIZE,
        MMUFlags::RXW,
        MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER,
        false,
        true,
        false,
    )
    .map(|_| ())
}

#[test]
fn a_window_that_overhangs_the_end_is_mapped() {
    let vmar = VmAddressRegion::new_root_zircon();
    let vmo = VmObject::new_paged(PAGES);
    assert_eq!(map_window(&vmar, &vmo, 0), Ok(()));
    assert_eq!(map_window(&vmar, &vmo, PAGE_SIZE), Ok(()));
}

#[test]
fn a_window_entirely_past_the_end_is_mapped() {
    let vmar = VmAddressRegion::new_root_zircon();
    let vmo = VmObject::new_paged(PAGES);
    assert_eq!(map_window(&vmar, &vmo, 2 * PAGES * PAGE_SIZE), Ok(()));
    // Nothing was committed for it.
    assert_eq!(vmo.committed_pages_in_range(0, PAGES), 0);
}

#[test]
fn only_the_pages_that_exist_are_committed_eagerly() {
    let vmar = VmAddressRegion::new_root_zircon();
    let vmo = VmObject::new_paged(PAGES);
    assert_eq!(map_window(&vmar, &vmo, PAGE_SIZE), Ok(()));
    if cfg!(feature = "libos") {
        // The eager (libos) map commits pages 1..4 and skips the one past the end.
        assert_eq!(vmo.committed_pages_in_range(0, PAGES), PAGES - 1);
        assert!(vmo.committed_paddr(0).is_none());
    }
}

#[test]
fn an_offset_plus_len_that_overflows_is_still_invalid() {
    let vmar = VmAddressRegion::new_root_zircon();
    let vmo = VmObject::new_paged(PAGES);
    assert_eq!(
        map_window(&vmar, &vmo, usize::MAX - PAGE_SIZE + 1),
        Err(ZxError::INVALID_ARGS)
    );
}
