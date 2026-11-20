//! One question -- is this page inside the object? -- asked by six
//! operations that all take an offset and a length straight from
//! userspace, and answered by four of them.
//!
//! `zx_vmo_op_range` checks that both numbers are page-aligned and hands
//! them through; `VMObjectSlice` wrote the bound down for its own
//! `commit`/`decommit` ("hands `offset` and `len` through unbounded"), and
//! `zero` carries it too. The object underneath did not, so a commit past
//! the end demand-allocated real frames at page indices the object does
//! not have -- and `committed_pages_in_range` clamps to `size`, so no
//! counter that reports what a VMO is holding could see them afterwards.
use super::*;

/// A one-page object, so page 1 is the first one outside it.
fn one_page() -> Arc<VmObject> {
    VmObject::new_paged(1)
}

#[test]
/// All six give the same answer for the same range. Two of them did not.
fn every_range_operation_answers_the_same_bound() {
    let vmo = one_page();
    let mut buf = [0u8; 1];
    assert_eq!(vmo.read(PAGE_SIZE, &mut buf), Err(ZxError::OUT_OF_RANGE));
    assert_eq!(vmo.write(PAGE_SIZE, &[9]), Err(ZxError::OUT_OF_RANGE));
    assert_eq!(vmo.zero(PAGE_SIZE, PAGE_SIZE), Err(ZxError::OUT_OF_RANGE));
    assert_eq!(vmo.pin(PAGE_SIZE, PAGE_SIZE), Err(ZxError::OUT_OF_RANGE));
    assert_eq!(vmo.commit(PAGE_SIZE, PAGE_SIZE), Err(ZxError::OUT_OF_RANGE));
    assert_eq!(
        vmo.decommit(PAGE_SIZE, PAGE_SIZE),
        Err(ZxError::OUT_OF_RANGE)
    );
}

#[test]
/// And the frames are the point: `zx_vmo_op_range(ZX_VMO_OP_COMMIT)` over
/// sixty-four pages of a one-page object really allocated sixty-four
/// frames, while `zx_object_get_info` went on reporting one page
/// committed. Every accounting and memory-pressure figure in the kernel
/// reads those clamped counters, so an unprivileged process could hold
/// physical memory that nothing could account for.
///
/// Counted on the object's own frame map, which means going one level below
/// [`VmObject`] to the [`VMObjectPaged`] under it. Two reasons, and both
/// rule out the obvious alternatives:
///
/// * `vmo_page_bytes()`, which this test used to compare before and after,
///   is `VMO_PAGE_ALLOC - VMO_PAGE_DEALLOC` for the WHOLE PROCESS. Any
///   other test in the binary that commits or drops a page moves it under
///   this one, which it did about twice in sixty parallel runs of the
///   suite. It is not a witness a parallel test can read.
/// * `committed_pages_in_range` -- and so `get_info().committed_bytes` --
///   clamps both indices to `size`, so it cannot see a frame at an index
///   past the end. That is exactly the frame the bug left behind, which is
///   why the lying counter cannot be the one that catches it.
///
/// `frames.len()` is the honest witness restricted to this object: nothing
/// else can touch it, and it counts frames at any index.
fn a_commit_past_the_end_allocates_nothing() {
    let vmo = VMObjectPaged::new(1);
    assert_eq!(
        vmo.commit(0, 64 * PAGE_SIZE),
        Err(ZxError::OUT_OF_RANGE),
        "a one-page object accepted a sixty-four-page commit"
    );
    assert_eq!(
        vmo.get_inner().frames.len(),
        0,
        "the refused commit still allocated frames"
    );
    assert_eq!(vmo.committed_pages_in_range(0, 64), 0);
}

#[test]
/// All or nothing, like the pinned-page refusal next door: a range with
/// one page outside the object leaves the pages inside it alone, rather
/// than committing as far as it can and then failing.
fn a_partly_out_of_range_commit_leaves_the_object_alone() {
    let vmo = one_page();
    assert_eq!(
        vmo.commit(0, 2 * PAGE_SIZE),
        Err(ZxError::OUT_OF_RANGE),
        "a two-page commit on a one-page object"
    );
    assert_eq!(
        vmo.committed_pages_in_range(0, 1),
        0,
        "the page inside the object was committed by a refused call"
    );
}

#[test]
/// A sum that wraps is out of range, not a short range that happens to
/// fit. `zx_vmo_op_range` page-aligns both numbers and nothing else, so
/// the pair below arrives exactly as written.
fn a_range_that_wraps_is_out_of_range() {
    let vmo = one_page();
    let high = usize::MAX - PAGE_SIZE + 1;
    assert_eq!(vmo.commit(high, PAGE_SIZE * 2), Err(ZxError::OUT_OF_RANGE));
    assert_eq!(
        vmo.decommit(high, PAGE_SIZE * 2),
        Err(ZxError::OUT_OF_RANGE)
    );
}

#[test]
/// The bound is on the end of the range, so an empty range that starts
/// exactly at the end of the object is inside it -- the same answer a
/// zero-length pin already gave.
fn an_empty_range_at_the_very_end_is_allowed() {
    let vmo = one_page();
    vmo.commit(PAGE_SIZE, 0).unwrap();
    vmo.decommit(PAGE_SIZE, 0).unwrap();
    vmo.pin(PAGE_SIZE, 0).unwrap();
    vmo.zero(PAGE_SIZE, 0).unwrap();
    // One past it is not.
    assert_eq!(
        vmo.commit(PAGE_SIZE + PAGE_SIZE, 0),
        Err(ZxError::OUT_OF_RANGE)
    );
}

#[test]
/// The whole object still commits, which is what the bound must not cost.
fn committing_the_whole_object_still_works() {
    let vmo = VmObject::new_paged(4);
    vmo.commit(0, 4 * PAGE_SIZE).unwrap();
    assert_eq!(vmo.committed_pages_in_range(0, 4), 4);
    vmo.decommit(0, 4 * PAGE_SIZE).unwrap();
    assert_eq!(vmo.committed_pages_in_range(0, 4), 0);
}
