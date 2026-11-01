//! One rule, and until now one of four operations kept it: when a VMO
//! hands frames back to the allocator, no PTE may go on pointing at them.
//!
//! `decommit` says so in as many words and does two unmap passes.
//! `set_cache_policy` sidesteps it by refusing while anything is mapped.
//! The other two -- `zx_vmo_set_size` shrinking an object and
//! `zx_vmo_op_range(ZX_VMO_OP_ZERO)` over a whole page -- dropped the
//! frames and told nobody, so a process kept a live PTE onto memory the
//! allocator had already given away, and the zero never reached the one
//! address space that had asked for it.
use super::*;

/// A two-page object with page 1 written and faulted in through a mapping
/// of both pages: `(vmar, vmo, address of page 1, its mapping)`.
fn mapped_pair(
    resizable: bool,
) -> (
    Arc<VmAddressRegion>,
    Arc<VmObject>,
    VirtAddr,
    Arc<VmMapping>,
) {
    let vmar = VmAddressRegion::new_root_zircon();
    let vmo = VmObject::new_paged_with_resizable(resizable, 2);
    vmo.test_write(1, 7);
    vmar.map_at(
        0,
        vmo.clone(),
        0,
        2 * PAGE_SIZE,
        MMUFlags::READ | MMUFlags::WRITE,
    )
    .unwrap();
    let addr = vmar.addr() + PAGE_SIZE;
    vmar.handle_page_fault(addr, MMUFlags::READ).unwrap();
    let mapping = vmar.find_mapping(addr).unwrap();
    assert!(
        mapping.query_vaddr(addr).is_ok(),
        "the fault installed no PTE, so this test would pass for the wrong reason"
    );
    (vmar, vmo, addr, mapping)
}

#[test]
/// `zx_vmo_set_size` down is `decommit` with a different name: the frames
/// of every page past the new end go back to the allocator. The PTEs that
/// pointed at them stayed, readable and writable, which is the same
/// use-after-free `decommit`'s unmap pass exists to prevent -- reached
/// through a syscall that never had one.
fn shrinking_an_object_drops_the_ptes_of_the_pages_it_released() {
    let (_vmar, vmo, addr, mapping) = mapped_pair(true);
    vmo.set_len(PAGE_SIZE).unwrap();
    assert!(
        vmo.committed_paddr(1).is_none(),
        "the frame is still held, so nothing was released"
    );
    assert!(
        mapping.query_vaddr(addr).is_err(),
        "a PTE still points at a frame the allocator has taken back"
    );
}

#[test]
/// And only those pages: page 0 is still inside the object and its
/// mapping is untouched.
fn shrinking_leaves_the_pages_it_kept_mapped() {
    let vmar = VmAddressRegion::new_root_zircon();
    let vmo = VmObject::new_paged_with_resizable(true, 2);
    vmar.map_at(0, vmo.clone(), 0, 2 * PAGE_SIZE, MMUFlags::READ)
        .unwrap();
    let kept = vmar.addr();
    vmar.handle_page_fault(kept, MMUFlags::READ).unwrap();
    vmar.handle_page_fault(kept + PAGE_SIZE, MMUFlags::READ)
        .unwrap();
    let mapping = vmar.find_mapping(kept).unwrap();
    vmo.set_len(PAGE_SIZE).unwrap();
    assert!(
        mapping.query_vaddr(kept).is_ok(),
        "the page that stayed in the object lost its PTE"
    );
}

#[test]
/// Growing releases nothing, so it unmaps nothing. Worth pinning: the
/// released range is computed from the old length, and a shrink-shaped
/// subtraction that underflows or a pass that runs unconditionally would
/// tear down a mapping that is still entirely valid.
fn growing_an_object_leaves_every_pte_where_it_was() {
    let (_vmar, vmo, addr, mapping) = mapped_pair(true);
    vmo.set_len(4 * PAGE_SIZE).unwrap();
    assert!(
        mapping.query_vaddr(addr).is_ok(),
        "growing an object tore down a mapping of a page it kept"
    );
}

#[test]
/// A refused shrink released nothing either. `set_len` answers
/// `BAD_STATE` while a page is pinned -- a device may be doing DMA into
/// it -- and that path must leave the address space exactly as it was.
fn a_refused_shrink_leaves_every_pte_where_it_was() {
    let (_vmar, vmo, addr, mapping) = mapped_pair(true);
    vmo.commit(0, 2 * PAGE_SIZE).unwrap();
    vmo.pin(0, PAGE_SIZE).unwrap();
    assert_eq!(vmo.set_len(PAGE_SIZE), Err(ZxError::BAD_STATE));
    assert!(
        mapping.query_vaddr(addr).is_ok(),
        "a refused shrink unmapped a page it did not release"
    );
    vmo.unpin(0, PAGE_SIZE).unwrap();
}

#[test]
/// A borrower's PTEs point at the CACHE's frames -- that is what
/// borrowing is -- and the borrower has no way of hearing that they went.
/// `decommit` walks `borrower_maps` for exactly this reason; a shrink has
/// to walk it too, or a `ftruncate` under a `MAP_PRIVATE` file mapping
/// leaves the borrower reading freed memory.
fn shrinking_a_cache_drops_the_ptes_of_everything_borrowing_it() {
    let vmar = VmAddressRegion::new_root_zircon();
    let cache = VmObject::new_paged_with_resizable(true, 2);
    cache.test_write(1, 7);
    let borrower = VmObject::new_paged_borrowing(2, cache.clone(), 0);
    vmar.map_at(0, borrower.clone(), 0, 2 * PAGE_SIZE, MMUFlags::READ)
        .unwrap();
    let addr = vmar.addr() + PAGE_SIZE;
    vmar.handle_page_fault(addr, MMUFlags::READ).unwrap();
    let mapping = vmar.find_mapping(addr).unwrap();
    assert!(mapping.query_vaddr(addr).is_ok());

    cache.set_len(PAGE_SIZE).unwrap();
    assert!(
        mapping.query_vaddr(addr).is_err(),
        "a borrower kept a PTE onto a frame its cache had released"
    );
}

#[test]
/// `ZX_VMO_OP_ZERO` over a whole page does not overwrite the frame, it
/// drops it, so the next fault demand-fills a fresh zero page. Without
/// the unmap the process that had it mapped went on reading the bytes it
/// had just asked to have zeroed -- off a frame that was by then back in
/// the allocator. The call reported success and did nothing the caller
/// could observe.
fn zeroing_a_whole_page_reaches_the_address_space_that_has_it_mapped() {
    let vmar = VmAddressRegion::new_root_zircon();
    let vmo = VmObject::new_paged(1);
    vmo.test_write(0, 7);
    vmar.map_at(0, vmo.clone(), 0, PAGE_SIZE, MMUFlags::READ)
        .unwrap();
    let addr = vmar.addr();
    vmar.handle_page_fault(addr, MMUFlags::READ).unwrap();
    assert_eq!(unsafe { (addr as *const u8).read() }, 7);

    vmo.zero(0, PAGE_SIZE).unwrap();
    assert!(
        vmar.find_mapping(addr).unwrap().query_vaddr(addr).is_err(),
        "a PTE still points at the frame the zero released"
    );
    vmar.handle_page_fault(addr, MMUFlags::READ).unwrap();
    assert_eq!(
        unsafe { (addr as *const u8).read() },
        0,
        "the mapping still reads the bytes that were zeroed"
    );
}

#[test]
/// And over several pages every one of them, not all-but-the-last. The
/// run of dropped pages is accumulated as `(first, count)` while the
/// blocks are walked, which is one of the two places in this file where
/// an off-by-one leaves a live PTE onto a freed frame behind -- on the
/// last page, the one a single-page test never reaches.
fn zeroing_several_pages_reaches_the_last_one_too() {
    let vmar = VmAddressRegion::new_root_zircon();
    let vmo = VmObject::new_paged(3);
    for page in 0..3 {
        vmo.test_write(page, 7);
    }
    vmar.map_at(0, vmo.clone(), 0, 3 * PAGE_SIZE, MMUFlags::READ)
        .unwrap();
    let base = vmar.addr();
    for page in 0..3 {
        vmar.handle_page_fault(base + page * PAGE_SIZE, MMUFlags::READ)
            .unwrap();
    }

    vmo.zero(0, 3 * PAGE_SIZE).unwrap();
    // Every PTE first, and only then the faults: a fault maps its
    // neighbours too (`fault_around`), so one re-fault in the middle of
    // this loop would answer for the pages after it.
    for page in 0..3 {
        let at = base + page * PAGE_SIZE;
        assert!(
            vmar.find_mapping(at).unwrap().query_vaddr(at).is_err(),
            "page {} kept a PTE onto the frame the zero released",
            page
        );
    }
    for page in 0..3 {
        let at = base + page * PAGE_SIZE;
        vmar.handle_page_fault(at, MMUFlags::READ).unwrap();
        assert_eq!(unsafe { (at as *const u8).read() }, 0, "page {}", page);
    }
}

#[test]
/// Part of a page is zeroed IN the frame, which every mapping of it can
/// already see, and nothing is released. The unmap pass must not run
/// there: an operation that tears down a PTE it did not have to costs a
/// fault and, on a shared page, a cross-CPU shootdown.
fn zeroing_part_of_a_page_leaves_the_mapping_alone() {
    let vmar = VmAddressRegion::new_root_zircon();
    let vmo = VmObject::new_paged(1);
    vmo.test_write(0, 7);
    vmo.commit(0, PAGE_SIZE).unwrap();
    vmar.map_at(0, vmo.clone(), 0, PAGE_SIZE, MMUFlags::READ)
        .unwrap();
    let addr = vmar.addr();
    vmar.handle_page_fault(addr, MMUFlags::READ).unwrap();
    let mapping = vmar.find_mapping(addr).unwrap();
    let before = mapping.query_vaddr(addr).unwrap().0;

    vmo.zero(0, 8).unwrap();
    let after = mapping
        .query_vaddr(addr)
        .expect("a partial zero released nothing and must unmap nothing")
        .0;
    assert_eq!(after, before, "the page moved under a partial zero");
    assert_eq!(unsafe { (addr as *const u8).read() }, 0);
}

#[test]
/// A mapping outlives the pages it covers. After a shrink the second page
/// of this mapping is no longer part of the object, and a fault there is
/// an exception -- the same answer `read`, `write`, `zero` and `pin` at
/// that offset already gave. It used to succeed: a write fault
/// demand-allocated a real frame at an index past `size`, which
/// `committed_pages_in_range` clamps away, so nothing that reports what a
/// VMO is holding could ever see it again.
fn a_fault_past_the_end_of_a_shrunk_object_is_an_error() {
    let vmar = VmAddressRegion::new_root_zircon();
    let vmo = VmObject::new_paged_with_resizable(true, 2);
    vmar.map_at(
        0,
        vmo.clone(),
        0,
        2 * PAGE_SIZE,
        MMUFlags::READ | MMUFlags::WRITE,
    )
    .unwrap();
    let addr = vmar.addr() + PAGE_SIZE;
    vmo.set_len(PAGE_SIZE).unwrap();

    assert_eq!(
        vmar.handle_page_fault(addr, MMUFlags::WRITE),
        Err(ZxError::OUT_OF_RANGE),
    );
    assert_eq!(
        vmar.handle_page_fault(addr, MMUFlags::READ),
        Err(ZxError::OUT_OF_RANGE),
    );
    assert!(
        vmar.find_mapping(addr).unwrap().query_vaddr(addr).is_err(),
        "the fault installed a PTE for a page the object does not have"
    );
    // The page that is still there faults as it always did.
    vmar.handle_page_fault(vmar.addr(), MMUFlags::WRITE)
        .unwrap();
}

#[test]
/// `madvise(MADV_DONTNEED)` over such a mapping keeps working: the part
/// that is still inside the object is discarded, and the part that is not
/// is left out rather than failing the whole call. `dontneed` hands
/// `decommit` ONE range for the whole span, so an unclamped range would
/// now take the pages that ARE there down with the ones that are not.
fn dontneed_over_a_mapping_longer_than_its_object_still_discards_what_is_there() {
    struct OnePage;
    impl FrameFiller for OnePage {
        fn source_len(&self) -> usize {
            PAGE_SIZE
        }
        fn fill_page(&self, _offset: usize, buf: &mut [u8]) {
            buf.fill(0xaa);
        }
    }
    let vmar = VmAddressRegion::new_root_zircon();
    // File-backed and not shared, which is what makes DONTNEED discard
    // rather than zero in place; one page long, mapped over two with
    // `ZX_VM_ALLOW_FAULTS`, which is the supported way of mapping past
    // the end of an object.
    let vmo = VmObject::new_paged_with_source(1, Arc::new(OnePage));
    vmar.map_ext(
        Some(0),
        vmo.clone(),
        0,
        2 * PAGE_SIZE,
        MMUFlags::RXW,
        MMUFlags::READ | MMUFlags::WRITE,
        false,
        false,
        true,
    )
    .unwrap();
    let base = vmar.addr();
    vmar.handle_page_fault(base, MMUFlags::WRITE).unwrap();
    assert_eq!(vmo.committed_pages_in_range(0, 1), 1);

    vmar.find_mapping(base)
        .unwrap()
        .dontneed(base, base + 2 * PAGE_SIZE);
    assert_eq!(
        vmo.committed_pages_in_range(0, 1),
        0,
        "the page that was inside the object was not discarded"
    );
}

/// A page cache that counts how it is read, page by page or by range.
/// Page `i` reads as byte `i`.
struct CountingSource {
    pages: usize,
    page_reads: core::sync::atomic::AtomicUsize,
    ranges: lock::Mutex<Vec<(usize, usize)>>,
}
impl FrameFiller for CountingSource {
    fn source_len(&self) -> usize {
        self.pages * PAGE_SIZE
    }
    fn fill_page(&self, offset: usize, buf: &mut [u8]) {
        self.page_reads
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        buf.fill((offset / PAGE_SIZE) as u8);
    }
    fn fill_range(&self, offset: usize, buf: &mut [u8]) {
        self.ranges.lock().push((offset, buf.len()));
        for (i, chunk) in buf.chunks_mut(PAGE_SIZE).enumerate() {
            chunk.fill(((offset + i * PAGE_SIZE) / PAGE_SIZE) as u8);
        }
    }
}

/// A 32-page file mapped `MAP_PRIVATE` (a borrower over its page
/// cache), read-only, over `vmar`.
fn private_file_mapping(
    vmar: &VmAddressRegion,
    perms: MMUFlags,
) -> (Arc<CountingSource>, Arc<VmObject>, Arc<VmObject>, VirtAddr) {
    let src = Arc::new(CountingSource {
        pages: 32,
        page_reads: core::sync::atomic::AtomicUsize::new(0),
        ranges: lock::Mutex::new(Vec::new()),
    });
    let cache = VmObject::new_paged_cache(32, src.clone());
    let borrower = VmObject::new_paged_borrowing(32, cache.clone(), 0);
    let base = vmar
        .map_ext(
            Some(0),
            borrower.clone(),
            0,
            32 * PAGE_SIZE,
            MMUFlags::RXW,
            perms,
            false,
            false,
            false,
        )
        .unwrap();
    // The `libos` build (which is what runs these tests) maps eagerly:
    // `map_ext` commits every page through the source. Hand those back
    // so the mapping is in the state a demand-paged library is in on the
    // kernel: nothing resident, nothing mapped, every touch a fault.
    borrower.decommit(0, 32 * PAGE_SIZE).unwrap();
    cache.decommit(0, 32 * PAGE_SIZE).unwrap();
    assert_eq!(cache.committed_pages_in_range(0, 32), 0);
    assert!(vmar.find_mapping(base).unwrap().query_vaddr(base).is_err());
    src.page_reads
        .store(0, core::sync::atomic::Ordering::Relaxed);
    src.ranges.lock().clear();
    (src, cache, borrower, base)
}

/// What a fault on `libxul.so` costs the disk now: the faulting page and
/// the fifteen after it are read from the file in ONE read, the cache
/// holds all sixteen, and fault-around maps them. Before, the sixteen
/// were sixteen reads, each a filesystem walk and, on the polled disk
/// drivers, its own synchronous command.
#[test]
fn a_read_fault_on_a_file_mapping_reads_its_whole_window_once() {
    let vmar = VmAddressRegion::new_root_zircon();
    let (src, cache, _borrower, base) =
        private_file_mapping(&vmar, MMUFlags::READ | MMUFlags::EXECUTE);
    vmar.handle_page_fault(base + 3 * PAGE_SIZE, MMUFlags::READ)
        .unwrap();
    assert_eq!(
        src.ranges.lock().clone(),
        vec![(3 * PAGE_SIZE, 16 * PAGE_SIZE)],
        "one range read of the fault-around window, from the faulting page"
    );
    assert_eq!(
        src.page_reads.load(core::sync::atomic::Ordering::Relaxed),
        0,
        "no page-by-page reads"
    );
    assert_eq!(cache.committed_pages_in_range(0, 32), 16);
    let mapping = vmar.find_mapping(base).unwrap();
    for page in 3..19 {
        let (paddr, _, _) = mapping.query_vaddr(base + page * PAGE_SIZE).unwrap();
        assert_eq!(Some(paddr), cache.committed_paddr(page), "page {page}");
    }
    assert!(mapping.query_vaddr(base + 19 * PAGE_SIZE).is_err());
    // The next fault, past the window, is another single read.
    vmar.handle_page_fault(base + 19 * PAGE_SIZE, MMUFlags::READ)
        .unwrap();
    assert_eq!(src.ranges.lock().len(), 2);
    assert_eq!(
        src.ranges.lock()[1],
        (19 * PAGE_SIZE, 13 * PAGE_SIZE),
        "clipped to the end of the file"
    );
}

/// A write fault on a private file mapping copies one page up, and it
/// still costs the file one read for the window: the relocations that
/// walk a library's data pages one write fault at a time were one 4 KiB
/// command each.
#[test]
fn a_write_fault_on_a_private_file_mapping_prefills_the_cache_too() {
    let vmar = VmAddressRegion::new_root_zircon();
    let (src, cache, borrower, base) =
        private_file_mapping(&vmar, MMUFlags::READ | MMUFlags::WRITE);
    vmar.handle_page_fault(base + 5 * PAGE_SIZE, MMUFlags::WRITE)
        .unwrap();
    assert_eq!(
        src.ranges.lock().clone(),
        vec![(5 * PAGE_SIZE, 16 * PAGE_SIZE)]
    );
    assert_eq!(cache.committed_pages_in_range(5, 21), 16);
    // The written page is the borrower's own copy; the cache's stays clean.
    let own = borrower.committed_paddr(5).expect("copied up");
    assert_ne!(Some(own), cache.committed_paddr(5));
    let mut b = [0u8; 1];
    borrower.read(5 * PAGE_SIZE, &mut b).unwrap();
    assert_eq!(b[0], 5, "the copy carries the file's bytes");
    // A later read fault inside the window is a hit for the window: no
    // second range read. Its own fault-around reaches one page past the
    // first window (21), and a lone missing page is read by itself, as
    // before, not with a window of its own.
    vmar.handle_page_fault(base + 6 * PAGE_SIZE, MMUFlags::READ)
        .unwrap();
    assert_eq!(src.ranges.lock().len(), 1);
    assert_eq!(
        src.page_reads.load(core::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(cache.committed_pages_in_range(0, 32), 17);
}

/// An anonymous mapping has no source: the fault path must not go
/// looking for one (and must not start reading zero pages in batches).
#[test]
fn a_fault_on_an_anonymous_mapping_prefills_nothing() {
    let vmar = VmAddressRegion::new_root_zircon();
    let vmo = VmObject::new_paged(32);
    let base = vmar
        .map_ext(
            Some(0),
            vmo.clone(),
            0,
            32 * PAGE_SIZE,
            MMUFlags::RXW,
            MMUFlags::READ | MMUFlags::WRITE,
            false,
            false,
            false,
        )
        .unwrap();
    // Undo the eager `libos` map (see `private_file_mapping`).
    vmo.decommit(0, 32 * PAGE_SIZE).unwrap();
    assert_eq!(vmo.committed_pages_in_range(0, 32), 0);
    vmar.handle_page_fault(base + 2 * PAGE_SIZE, MMUFlags::WRITE)
        .unwrap();
    assert_eq!(vmo.committed_pages_in_range(0, 32), 1);
}
