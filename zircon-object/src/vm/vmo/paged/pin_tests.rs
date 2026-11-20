//! Pinning is the promise that a page stays where it is because a device
//! is about to do DMA into it. Three operations take pages away from a
//! VMO; two of them asked whether anything was pinned first and the third
//! did not, so the frame went back to the allocator with the IOMMU still
//! pointing at it.
use super::*;

/// A four-page object with every page committed.
fn committed() -> Arc<VmObject> {
    let vmo = VmObject::new_paged(4);
    vmo.commit(0, 4 * PAGE_SIZE).unwrap();
    assert_eq!(held(&vmo), [true; 4]);
    vmo
}

/// Holds a pin for as long as it is alive.
///
/// `Drop for VMObjectPaged` asserts that no frame is left pinned, so a
/// test that fails an assertion with a pin still held panics a second
/// time on its way out and aborts the whole binary: the suite goes red
/// with no test name anywhere in it, which is most of what a CI failure
/// is for. Nothing reachable from userspace can trip that assert -- the
/// token holds the `Arc` -- but a test can, and letting go from a `Drop`
/// runs on the unwinding path too.
struct Pinned<'a> {
    vmo: &'a Arc<VmObject>,
    offset: usize,
    len: usize,
    held: bool,
}

impl Pinned<'_> {
    /// Let go of the pin, and say whether the kernel agreed.
    fn release(mut self) -> ZxResult {
        self.held = false;
        self.vmo.unpin(self.offset, self.len)
    }
}

impl Drop for Pinned<'_> {
    fn drop(&mut self) {
        if self.held {
            let _ = self.vmo.unpin(self.offset, self.len);
        }
    }
}

/// Pins a range and hands back the handle that lets go of it.
fn pin(vmo: &Arc<VmObject>, offset: usize, len: usize) -> Pinned<'_> {
    vmo.pin(offset, len).unwrap();
    Pinned {
        vmo,
        offset,
        len,
        held: true,
    }
}

/// Which of the four pages the object is still holding a frame for.
fn held(vmo: &Arc<VmObject>) -> [bool; 4] {
    let mut out = [false; 4];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = vmo.committed_pages_in_range(i, i + 1) == 1;
    }
    out
}

#[test]
/// A pinned page is not handed back to the allocator. `zx_vmo_op_range`
/// passes its offset and length straight through from userspace, so this
/// used to hand a frame a device was about to write into back for someone
/// else to use, and answer `Ok`.
fn a_pinned_page_is_not_handed_back_to_the_allocator() {
    let vmo = committed();
    // Two pages, so a pin that only marked the first one shows up here.
    let pinned = pin(&vmo, 0, 2 * PAGE_SIZE);
    assert_eq!(vmo.decommit(0, PAGE_SIZE), Err(ZxError::BAD_STATE));
    assert_eq!(vmo.decommit(PAGE_SIZE, PAGE_SIZE), Err(ZxError::BAD_STATE));
    assert_eq!(held(&vmo), [true; 4], "a page went under the pin");
    // And with the pin released they go as usual.
    pinned.release().unwrap();
    vmo.decommit(0, 2 * PAGE_SIZE).unwrap();
    assert_eq!(held(&vmo), [false, false, true, true]);
}

#[test]
/// The refusal is per page, not per object: the rest of a mostly-pinned
/// VMO can still be decommitted.
fn decommitting_around_a_pinned_page_still_works() {
    let vmo = committed();
    let pinned = pin(&vmo, PAGE_SIZE, PAGE_SIZE);
    vmo.decommit(0, PAGE_SIZE).unwrap();
    vmo.decommit(2 * PAGE_SIZE, 2 * PAGE_SIZE).unwrap();
    assert_eq!(vmo.decommit(PAGE_SIZE, PAGE_SIZE), Err(ZxError::BAD_STATE));
    assert_eq!(held(&vmo), [false, true, false, false]);
    pinned.release().unwrap();
}

#[test]
/// And it is all or nothing: a range with one pinned page in it leaves
/// every page alone, including the ones before the pinned one.
fn a_refused_decommit_leaves_every_page_alone() {
    let vmo = committed();
    let pinned = pin(&vmo, 3 * PAGE_SIZE, PAGE_SIZE);
    assert_eq!(vmo.decommit(0, 4 * PAGE_SIZE), Err(ZxError::BAD_STATE));
    assert_eq!(held(&vmo), [true; 4]);
    pinned.release().unwrap();
}

#[test]
/// Taking pages away is three operations, and all three answer the same
/// thing while something is pinned. Two of them always did.
fn the_three_ways_of_taking_pages_away_all_refuse_a_pinned_object() {
    let vmo = VmObject::new_paged_with_resizable(true, 4);
    vmo.commit(0, 4 * PAGE_SIZE).unwrap();
    let pinned = pin(&vmo, 0, PAGE_SIZE);
    assert_eq!(vmo.set_len(PAGE_SIZE), Err(ZxError::BAD_STATE));
    assert_eq!(
        vmo.set_cache_policy(CachePolicy::Uncached),
        Err(ZxError::BAD_STATE),
    );
    assert_eq!(vmo.decommit(0, PAGE_SIZE), Err(ZxError::BAD_STATE));

    pinned.release().unwrap();
    vmo.decommit(0, PAGE_SIZE).unwrap();
    vmo.set_len(PAGE_SIZE).unwrap();
}

#[test]
/// Pinning a page the object does not have commits it on the spot, as
/// Zircon does. `zx_bti_pin` commits the range and then pins it, and
/// those are two turns of the object's lock: a decommit in between is
/// allowed, because the pages are not pinned yet, and this is where it
/// lands. It used to be a kernel panic, then `BAD_STATE`, which
/// `Bti.DecommitRace` races a decommitting thread to provoke.
fn pinning_a_page_that_is_not_there_commits_it() {
    let vmo = VmObject::new_paged(4);
    vmo.pin(0, PAGE_SIZE).unwrap();
    assert_eq!(held(&vmo), [true, false, false, false]);
    assert_eq!(vmo.decommit(0, PAGE_SIZE), Err(ZxError::BAD_STATE));
    vmo.unpin(0, PAGE_SIZE).unwrap();

    let vmo = committed();
    vmo.decommit(2 * PAGE_SIZE, PAGE_SIZE).unwrap();
    vmo.pin(0, 4 * PAGE_SIZE).unwrap();
    assert_eq!(held(&vmo), [true, true, true, true]);
    assert_eq!(
        vmo.decommit(2 * PAGE_SIZE, PAGE_SIZE),
        Err(ZxError::BAD_STATE)
    );
    vmo.unpin(0, 4 * PAGE_SIZE).unwrap();
    vmo.decommit(0, PAGE_SIZE).unwrap();
    assert_eq!(held(&vmo), [false, true, true, true]);
}

#[test]
/// A pinned page of a snapshot child is the child's own: pinning forks
/// it off the parent instead of pinning the page they share.
fn pinning_a_page_a_child_shares_forks_it() {
    let vmo = committed();
    let child = vmo.create_child(false, 0, 4 * PAGE_SIZE).unwrap();
    child.pin(PAGE_SIZE, PAGE_SIZE).unwrap();
    assert_eq!(child.committed_pages_in_range(1, 2), 1);
    assert_eq!(vmo.test_read(1), child.test_read(1));
    child.unpin(PAGE_SIZE, PAGE_SIZE).unwrap();
}

#[test]
/// Unpinning one is the same answer rather than the same panic.
fn unpinning_a_page_that_is_not_there_is_an_error_not_a_panic() {
    let vmo = VmObject::new_paged(4);
    assert_eq!(vmo.unpin(0, PAGE_SIZE), Err(ZxError::BAD_STATE));

    // A page that is there but was never pinned is a different answer.
    let vmo = committed();
    assert_eq!(vmo.unpin(0, PAGE_SIZE), Err(ZxError::UNAVAILABLE));
}

#[test]
/// A VMO that goes away with a page still pinned is a hole worth saying
/// out loud, and saying it used to be an `assert_eq!` inside a
/// destructor. That costs the machine to report a leak, and in a test
/// binary it costs every test after this one: a panic raised while
/// something else is already unwinding does not unwind, it aborts, so
/// the run ends with no test name in it at all. It is also exactly
/// where a refused `unpin` in `PinnedMemoryToken`'s own `Drop` lands one
/// frame later, so the two have to agree.
fn a_vmo_that_goes_away_still_pinned_is_reported_not_a_panic() {
    let vmo = committed();
    vmo.pin(0, PAGE_SIZE).unwrap();
    drop(vmo);
}

#[test]
/// A zero-length pin or unpin is nothing at all, whatever it names.
fn nothing_is_pinned_by_a_zero_length_range() {
    let vmo = committed();
    vmo.pin(0, 0).unwrap();
    vmo.unpin(0, 0).unwrap();
    vmo.decommit(0, PAGE_SIZE).unwrap();
    assert_eq!(held(&vmo), [false, true, true, true]);
}
