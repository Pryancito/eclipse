use super::*;

#[test]
fn read_write() {
    let vmo = VmObject::new_paged(2);
    super::super::tests::read_write(&*vmo);
}

#[test]
fn create_child() {
    let vmo = VmObject::new_paged(1);
    let child_vmo = vmo.create_child(false, 0, PAGE_SIZE).unwrap();

    // write to parent and make sure clone doesn't see it
    vmo.test_write(0, 1);
    assert_eq!(vmo.test_read(0), 1);
    assert_eq!(child_vmo.test_read(0), 0);

    // write to clone and make sure parent doesn't see it
    child_vmo.test_write(0, 2);
    assert_eq!(vmo.test_read(0), 1);
    assert_eq!(child_vmo.test_read(0), 2);
}

/// A backing source that counts how it is read: page by page
/// (`fill_page`) or by range (`fill_range`), and what each range asked
/// for. Page `i` of the source reads as byte `i`.
struct CountingSource {
    pages: usize,
    page_reads: AtomicUsize,
    range_reads: AtomicUsize,
    ranges: Mutex<Vec<(usize, usize)>>,
    /// Only `fill_page`: the default `fill_range` loop then applies.
    pages_only: bool,
}

impl CountingSource {
    fn new(pages: usize, pages_only: bool) -> Arc<Self> {
        Arc::new(Self {
            pages,
            page_reads: AtomicUsize::new(0),
            range_reads: AtomicUsize::new(0),
            ranges: Mutex::new(Vec::new()),
            pages_only,
        })
    }
    fn page_reads(&self) -> usize {
        self.page_reads.load(Ordering::Relaxed)
    }
    fn range_reads(&self) -> usize {
        self.range_reads.load(Ordering::Relaxed)
    }
    fn ranges(&self) -> Vec<(usize, usize)> {
        self.ranges.lock().clone()
    }
}

impl FrameFiller for CountingSource {
    fn source_len(&self) -> usize {
        self.pages * PAGE_SIZE
    }
    fn fill_page(&self, offset: usize, buf: &mut [u8]) {
        self.page_reads.fetch_add(1, Ordering::Relaxed);
        if offset < self.source_len() {
            buf.fill((offset / PAGE_SIZE) as u8);
        }
    }
    fn fill_range(&self, offset: usize, buf: &mut [u8]) {
        if self.pages_only {
            // What a source that only knows `fill_page` gets.
            for (i, chunk) in buf.chunks_mut(PAGE_SIZE).enumerate() {
                self.fill_page(offset + i * PAGE_SIZE, chunk);
            }
            return;
        }
        self.range_reads.fetch_add(1, Ordering::Relaxed);
        self.ranges.lock().push((offset, buf.len()));
        for (i, chunk) in buf.chunks_mut(PAGE_SIZE).enumerate() {
            let off = offset + i * PAGE_SIZE;
            if off < self.source_len() {
                chunk.fill((off / PAGE_SIZE) as u8);
            }
        }
    }
}

fn page_byte(vmo: &VmObject, page: usize) -> u8 {
    let mut b = [0u8; 1];
    vmo.read(page * PAGE_SIZE, &mut b).unwrap();
    b[0]
}

/// The point of `prefill`: a fault's 16-page window costs the source ONE
/// read, and every commit inside the window afterwards is a hit.
#[test]
fn prefill_reads_the_whole_window_with_one_source_read() {
    let src = CountingSource::new(32, false);
    let vmo = VmObject::new_paged_cache(32, src.clone());
    vmo.prefill(0, 16);
    assert_eq!(src.range_reads(), 1);
    assert_eq!(src.ranges(), vec![(0, 16 * PAGE_SIZE)]);
    assert_eq!(src.page_reads(), 0, "no page-by-page reads");
    assert_eq!(vmo.committed_pages_in_range(0, 32), 16);
    for page in 0..16 {
        vmo.commit_page(page, MMUFlags::READ).unwrap();
        assert_eq!(page_byte(&vmo, page), page as u8, "page {page}");
    }
    assert_eq!(
        (src.range_reads(), src.page_reads()),
        (1, 0),
        "the commits inside a prefilled window must not touch the source"
    );
    // Outside the window the source is still read on demand.
    vmo.commit_page(20, MMUFlags::READ).unwrap();
    assert_eq!(page_byte(&vmo, 20), 20);
    assert_eq!(src.page_reads(), 1);
}

/// Supplied pages are resident without the source being read, and a
/// page that was already there keeps what it held.
#[test]
fn supplied_pages_are_resident_and_the_source_is_not_read() {
    let src = CountingSource::new(32, false);
    let vmo = VmObject::new_paged_cache(32, src.clone());
    vmo.commit_page(1, MMUFlags::READ).unwrap();
    let reads = (src.range_reads(), src.page_reads());
    vmo.supply(0, &vec![0xAAu8; 3 * PAGE_SIZE]);
    assert_eq!(vmo.committed_pages_in_range(0, 32), 3);
    assert_eq!(page_byte(&vmo, 0), 0xAA);
    assert_eq!(page_byte(&vmo, 1), 1, "the resident page is kept");
    assert_eq!(page_byte(&vmo, 2), 0xAA);
    assert_eq!((src.range_reads(), src.page_reads()), reads);
    // Past the end of the object nothing is taken.
    vmo.supply(31, &vec![0xBBu8; 2 * PAGE_SIZE]);
    assert_eq!(vmo.committed_pages_in_range(0, 32), 4);
}

/// Pages already resident are neither re-read nor replaced: the read
/// spans the missing pages only, and a page that was written keeps what
/// was written.
#[test]
fn prefill_reads_only_the_span_of_missing_pages_and_keeps_the_resident_ones() {
    let src = CountingSource::new(32, false);
    let vmo = VmObject::new_paged_cache(32, src.clone());
    vmo.commit_page(0, MMUFlags::WRITE).unwrap();
    vmo.write(0, &[0xee]).unwrap();
    vmo.commit_page(15, MMUFlags::READ).unwrap();
    assert_eq!(src.page_reads(), 2);
    vmo.prefill(0, 16);
    assert_eq!(src.ranges(), vec![(PAGE_SIZE, 14 * PAGE_SIZE)]);
    assert_eq!(vmo.committed_pages_in_range(0, 16), 16);
    assert_eq!(page_byte(&vmo, 0), 0xee, "a resident page was replaced");
    assert_eq!(page_byte(&vmo, 1), 1);
    assert_eq!(page_byte(&vmo, 14), 14);
    // One missing page is left to the commit that follows: there is no
    // batch to gain, and a range read would cost an allocation.
    vmo.commit_page(16, MMUFlags::READ).unwrap();
    vmo.commit_page(18, MMUFlags::READ).unwrap();
    let before = src.range_reads();
    vmo.prefill(16, 3);
    assert_eq!(src.range_reads(), before);
    assert_eq!(vmo.committed_pages_in_range(17, 18), 0);
}

/// The window is clipped to the object and to the source: nothing is
/// read past either, and the pages past the source stay uncommitted
/// (they are the zero tail a commit hands out on its own).
#[test]
fn prefill_stops_at_the_end_of_the_source_and_of_the_object() {
    let src = CountingSource::new(5, false);
    let vmo = VmObject::new_paged_cache(8, src.clone());
    vmo.prefill(2, 16);
    assert_eq!(src.ranges(), vec![(2 * PAGE_SIZE, 3 * PAGE_SIZE)]);
    assert_eq!(vmo.committed_pages_in_range(0, 8), 3);
    assert_eq!(vmo.committed_pages_in_range(2, 5), 3);
    // Entirely past the source, or past the object: nothing at all.
    vmo.prefill(5, 16);
    vmo.prefill(8, 16);
    vmo.prefill(usize::MAX / PAGE_SIZE, 16);
    assert_eq!(src.range_reads(), 1);
    assert_eq!(vmo.committed_pages_in_range(0, 8), 3);
}

/// A `MAP_PRIVATE` file mapping is a borrower with no source of its own;
/// its prefill lands in the page cache it borrows from, at the borrowed
/// offset, and the borrower's commits then resolve to those frames.
#[test]
fn a_borrower_prefills_the_page_cache_it_borrows_from() {
    let src = CountingSource::new(32, false);
    let cache = VmObject::new_paged_cache(32, src.clone());
    let borrower = VmObject::new_paged_borrowing(8, cache.clone(), 4 * PAGE_SIZE);
    borrower.prefill(0, 16);
    assert_eq!(src.ranges(), vec![(4 * PAGE_SIZE, 16 * PAGE_SIZE)]);
    assert_eq!(cache.committed_pages_in_range(4, 20), 16);
    assert_eq!(
        borrower.committed_pages_in_range(0, 8),
        0,
        "a borrower owns no clean pages"
    );
    let paddr = borrower.commit_page(0, MMUFlags::READ).unwrap();
    assert_eq!(Some(paddr), cache.committed_paddr(4));
    assert_eq!(page_byte(&borrower, 0), 4);
    assert_eq!((src.range_reads(), src.page_reads()), (1, 0));
    // Past the cache: nothing.
    let long = VmObject::new_paged_borrowing(8, cache.clone(), 30 * PAGE_SIZE);
    long.prefill(2, 16);
    assert_eq!(src.range_reads(), 1);
}

/// After a fork the source lives in the hidden parent and the leaf
/// resolves through it page by page as before: prefill does nothing there
/// rather than reaching into a node it does not own.
#[test]
fn prefill_is_a_no_op_below_a_hidden_parent() {
    let src = CountingSource::new(32, false);
    let vmo = VmObject::new_paged_with_source(32, src.clone());
    let child = vmo.create_child(false, 0, 32 * PAGE_SIZE).unwrap();
    child.prefill(0, 16);
    vmo.prefill(0, 16);
    assert_eq!(src.range_reads(), 0);
    child.commit_page(3, MMUFlags::READ).unwrap();
    assert_eq!(page_byte(&child, 3), 3);
    assert_eq!(src.page_reads(), 1);
}

/// A source that only implements `fill_page` is still batched by the
/// trait's default loop: same pages resident, same contents.
#[test]
fn a_source_without_a_range_read_is_prefilled_page_by_page() {
    let src = CountingSource::new(32, true);
    let vmo = VmObject::new_paged_cache(32, src.clone());
    vmo.prefill(0, 16);
    assert_eq!(src.page_reads(), 16);
    assert_eq!(vmo.committed_pages_in_range(0, 16), 16);
    assert_eq!(page_byte(&vmo, 9), 9);
    // And an object with no source at all has nothing to prefill.
    let anon = VmObject::new_paged(32);
    anon.prefill(0, 16);
    assert_eq!(anon.committed_pages_in_range(0, 32), 0);
}

/// A source that, while it is read, uses the objects it is told to --
/// what a read that waits for a served disk lets another task of the CPU
/// do. Each use takes that object's family lock, so with the lock still
/// held by the read the re-entrancy tripwire fires (or the test hangs).
/// Page `i` reads as byte `i + 1`, and `bump` makes the next read return
/// a different byte after reporting a change to `watch`, as a `write(2)`
/// landing in the middle of the read would.
struct ProbeSource {
    pages: usize,
    reads: AtomicUsize,
    touch: Mutex<Vec<alloc::sync::Weak<VmObject>>>,
    add: core::sync::atomic::AtomicU8,
    bump: Mutex<Option<alloc::sync::Weak<VmObject>>>,
}

impl ProbeSource {
    fn new(pages: usize) -> Arc<Self> {
        Arc::new(Self {
            pages,
            reads: AtomicUsize::new(0),
            touch: Mutex::new(Vec::new()),
            add: core::sync::atomic::AtomicU8::new(1),
            bump: Mutex::new(None),
        })
    }
    fn touch(&self, vmo: &Arc<VmObject>) {
        self.touch.lock().push(Arc::downgrade(vmo));
    }
    fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }
}

impl FrameFiller for ProbeSource {
    fn source_len(&self) -> usize {
        self.pages * PAGE_SIZE
    }
    fn fill_page(&self, offset: usize, buf: &mut [u8]) {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let touch: Vec<_> = self
            .touch
            .lock()
            .iter()
            .filter_map(|w| w.upgrade())
            .collect();
        for vmo in &touch {
            let _ = vmo.len();
            let _ = vmo.committed_pages_in_range(0, 1);
        }
        let add = self.add.load(Ordering::Relaxed);
        if let Some(watch) = self.bump.lock().take().and_then(|w| w.upgrade()) {
            // The file changes after this read took its bytes.
            self.add.store(add + 100, Ordering::Relaxed);
            watch.note_source_change();
        }
        if offset < self.source_len() {
            buf.fill((offset / PAGE_SIZE) as u8 + add);
        }
    }
}

/// The bug that stopped a machine whose root is on a served NVMe disk:
/// a page read from its file with its family's lock held. Every way a
/// page reaches its source is tried here with a source that takes the
/// locks it must not find taken.
#[test]
fn a_page_is_read_from_its_source_with_no_lock_of_its_family_held() {
    let src = ProbeSource::new(8);
    let cache = VmObject::new_paged_cache(8, src.clone());
    src.touch(&cache);
    // A fault on the cache (MAP_SHARED) and a read(2) of it.
    cache.commit_page(0, MMUFlags::READ).unwrap();
    assert_eq!(page_byte(&cache, 0), 1);
    assert_eq!(page_byte(&cache, 1), 2);
    // A borrower (MAP_PRIVATE): neither its lock nor the cache's.
    let borrower = VmObject::new_paged_borrowing(4, cache.clone(), 2 * PAGE_SIZE);
    src.touch(&borrower);
    assert_eq!(
        borrower.commit_page(0, MMUFlags::READ).unwrap(),
        cache.committed_paddr(2).unwrap()
    );
    borrower.commit_page(1, MMUFlags::WRITE).unwrap();
    assert_eq!(page_byte(&borrower, 1), 4);
    assert_eq!(page_byte(&cache, 3), 4);
    // The fault-around read.
    cache.prefill(4, 4);
    assert_eq!(cache.committed_pages_in_range(4, 8), 4);
    assert_eq!(page_byte(&cache, 6), 7);
    // Below a hidden parent, which holds the source after a fork.
    let private = VmObject::new_paged_with_source(8, src.clone());
    src.touch(&private);
    let child = private.create_child(false, 0, 8 * PAGE_SIZE).unwrap();
    src.touch(&child);
    child.commit_page(5, MMUFlags::WRITE).unwrap();
    assert_eq!(page_byte(&child, 5), 6);
    assert_eq!(page_byte(&private, 5), 6);
    // A mapping's eager commit, which does not run its closure again.
    let eager = VmObject::new_paged_with_source(8, src.clone());
    src.touch(&eager);
    let mut got = Vec::new();
    eager
        .commit_pages_with(&mut |commit| {
            for i in 0..3 {
                got.push(commit(i, MMUFlags::READ)?);
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(got.len(), 3);
    assert_eq!(Some(got[2]), eager.committed_paddr(2));
    assert_eq!(page_byte(&eager, 2), 3);
}

/// A read that took its bytes before a write to the file finished is
/// not kept: the page is read again, and holds what the write left.
#[test]
fn a_page_read_while_its_source_changes_is_read_again() {
    let src = ProbeSource::new(4);
    let cache = VmObject::new_paged_cache(4, src.clone());
    *src.bump.lock() = Some(Arc::downgrade(&cache));
    cache.commit_page(1, MMUFlags::READ).unwrap();
    assert_eq!(page_byte(&cache, 1), 102);
    assert_eq!(src.reads(), 2);
}

/// `zero` over whole pages of a file-backed object commits them only to
/// drop them: it must not read them, or each run would read again what
/// the one before dropped. The partial pages at the two ends are read
/// and keep their other bytes.
#[test]
fn zeroing_whole_pages_of_a_file_does_not_read_them() {
    let src = ProbeSource::new(4);
    let vmo = VmObject::new_paged_with_source(4, src.clone());
    vmo.zero(100, 3 * PAGE_SIZE).unwrap();
    assert_eq!(src.reads(), 2, "only the two partial pages");
    let mut b = [0u8; 1];
    vmo.read(99, &mut b).unwrap();
    assert_eq!(b[0], 1);
    vmo.read(100, &mut b).unwrap();
    assert_eq!(b[0], 0);
    vmo.read(3 * PAGE_SIZE + 99, &mut b).unwrap();
    assert_eq!(b[0], 0);
    vmo.read(3 * PAGE_SIZE + 100, &mut b).unwrap();
    assert_eq!(b[0], 4);
}

#[test]
fn fork_copy_lazy() {
    struct Filler;
    impl FrameFiller for Filler {
        fn source_len(&self) -> usize {
            2 * PAGE_SIZE
        }
        fn fill_page(&self, offset: usize, buf: &mut [u8]) {
            buf.fill(if offset == 0 { 0xaa } else { 0xbb });
        }
    }
    let vmo = VmObject::new_paged_with_source(2, Arc::new(Filler));
    // Touch (and modify) only page 0; page 1 stays uncommitted.
    vmo.commit_page(0, MMUFlags::WRITE).unwrap();
    vmo.test_write(0, 1);

    let child = vmo.fork_copy().unwrap();
    // The committed page was copied with the parent's modification…
    assert_eq!(child.test_read(0), 1);
    // …and further parent writes don't leak into the child (independent frame).
    vmo.test_write(0, 7);
    assert_eq!(child.test_read(0), 1);
    // The untouched page still demand-fills from the shared source.
    let mut buf = [0u8; 1];
    child.read(PAGE_SIZE, &mut buf).unwrap();
    assert_eq!(buf[0], 0xbb);
}

#[test]
#[ignore] // FIXME
fn zero_page_write() {
    let vmo0 = VmObject::new_paged(1);
    let vmo1 = vmo0.create_child(false, 0, PAGE_SIZE).unwrap();
    let vmo2 = vmo0.create_child(false, 0, PAGE_SIZE).unwrap();
    let vmos = [vmo0, vmo1, vmo2];
    let origin = vmo_page_bytes();

    // no committed pages
    for vmo in &vmos {
        assert_eq!(vmo.get_info().committed_bytes, 0);
    }

    // copy-on-write
    for i in 0..3 {
        vmos[i].test_write(0, i as u8);
        for j in 0..3 {
            assert_eq!(vmos[j].test_read(0), if j <= i { j as u8 } else { 0 });
            assert_eq!(
                vmos[j].get_info().committed_bytes as usize,
                if j <= i { PAGE_SIZE } else { 0 }
            );
        }
        assert_eq!(vmo_page_bytes() - origin, (i + 1) * PAGE_SIZE);
    }
}

/// `VmoClone2TestCase.ObjMemAccounting` and `OffsetTest2`, on the
/// scaled figure: a shared page is half each side's, a forked page is
/// its forker's, and a snapshot's snapshots share their grandparent's
/// pages with the original by halves once the middle one is gone. And
/// the cousin's data: the snapshot's snapshot still reads what it saw.
/// A page read three ways is a third of a page each, and the third of
/// a byte left over is reported in the fraction field, in the units the
/// core tests read it in (`VmoClone2TestCase.Overflow`): a whole byte is
/// `1 << 63`.
#[test]
fn a_third_of_a_byte_is_reported_in_units_of_two_to_the_minus_63() {
    let vmo = VmObject::new_paged(1);
    vmo.test_write(0, 1);
    let clone = vmo.create_child(false, 0, PAGE_SIZE).unwrap();
    let clone2 = clone.create_child(false, 0, PAGE_SIZE).unwrap();
    for v in [&vmo, &clone, &clone2] {
        let info = v.get_info();
        assert_eq!(info.populated_scaled_bytes as usize, PAGE_SIZE / 3);
        assert_eq!(
            info.populated_fractional_scaled_bytes,
            0x8000_0000_0000_0000u64 / 3
        );
        assert_eq!(
            info.committed_fractional_scaled_bytes,
            0x8000_0000_0000_0000u64 / 3
        );
    }
    drop(clone2);
    for v in [&vmo, &clone] {
        let info = v.get_info();
        assert_eq!(info.populated_scaled_bytes as usize, PAGE_SIZE / 2);
        assert_eq!(info.populated_fractional_scaled_bytes, 0);
    }
}

/// A child's offset is counted from the root of its tree, and a chain
/// of offsets whose sum wraps around is refused where the wrap happens,
/// at `create_child` or at `set_size` (`VmoClone2TestCase.NoAccumulatedOverflow`).
#[test]
fn an_offset_that_wraps_counted_from_the_root_is_invalid() {
    let vmo = VmObject::new_paged(0);
    let child1 = vmo.create_child(false, 0xffff_ffff_ffff_8000, 0).unwrap();
    assert_eq!(
        child1.create_child(false, 0x8000, 0).err(),
        Some(ZxError::INVALID_ARGS)
    );
    let child2 = child1.create_child(true, 0x4000, 0).unwrap();
    assert_eq!(child2.set_len(0x8000).err(), Some(ZxError::INVALID_ARGS));
    assert!(child2.set_len(0x2000).is_ok());
}

#[test]
fn a_shared_page_is_split_equally_between_the_vmos_that_read_it() {
    let scaled = |vmo: &Arc<VmObject>| vmo.get_info().populated_scaled_bytes as usize;
    let vmo = VmObject::new_paged(2);
    vmo.test_write(0, 1);
    vmo.test_write(1, 1);
    assert_eq!(scaled(&vmo), 2 * PAGE_SIZE);
    let clone = vmo.create_child(false, 0, 2 * PAGE_SIZE).unwrap();
    assert_eq!((scaled(&vmo), scaled(&clone)), (PAGE_SIZE, PAGE_SIZE));
    // The original forks page 0: that one is its own, the old one is the
    // clone's alone, and page 1 stays shared.
    vmo.test_write(0, 2);
    assert_eq!(
        (scaled(&vmo), scaled(&clone)),
        (3 * PAGE_SIZE / 2, 3 * PAGE_SIZE / 2)
    );
    clone.test_write(1, 2);
    assert_eq!(
        (scaled(&vmo), scaled(&clone)),
        (2 * PAGE_SIZE, 2 * PAGE_SIZE)
    );

    // OffsetTest2: four pages; a clone of pages 1..4; two clones of that
    // one covering its pages 0..2 and 2..3; the middle clone writes its
    // page 1 and is closed.
    let vmo = VmObject::new_paged(4);
    for i in 0..4 {
        vmo.test_write(i, i as u8 + 1);
    }
    let offset_clone = vmo.create_child(false, PAGE_SIZE, 3 * PAGE_SIZE).unwrap();
    let clone1 = offset_clone.create_child(false, 0, 2 * PAGE_SIZE).unwrap();
    let clone2 = offset_clone
        .create_child(false, 2 * PAGE_SIZE, PAGE_SIZE)
        .unwrap();
    offset_clone.test_write(1, 9);
    drop(offset_clone);
    assert_eq!((clone1.test_read(0), clone1.test_read(1)), (2, 3));
    assert_eq!(clone2.test_read(0), 4);
    assert_eq!(
        vmo.test_read(2),
        3,
        "the original lost the page its grandchild forked"
    );
    assert_eq!(scaled(&vmo), 5 * PAGE_SIZE / 2, "1 + 1/2 + 1/2 + 1/2");
    assert_eq!(scaled(&clone1), PAGE_SIZE, "1/2 + 1/2");
    assert_eq!(scaled(&clone2), PAGE_SIZE / 2);
    drop(clone1);
    assert_eq!(scaled(&vmo), 7 * PAGE_SIZE / 2, "1 + 1 + 1 + 1/2");
    assert_eq!(scaled(&clone2), PAGE_SIZE / 2);
    assert_eq!(clone2.test_read(0), 4);
    drop(clone2);
    assert_eq!(scaled(&vmo), 4 * PAGE_SIZE);
    assert_eq!(vmo.test_read(3), 4);
    assert_ne!(vmo.get_info().populated_fractional_scaled_bytes, u64::MAX);
}

#[test]
fn overflow() {
    let vmo0 = VmObject::new_paged(2);
    vmo0.test_write(0, 1);
    let vmo1 = vmo0.create_child(false, 0, 2 * PAGE_SIZE).unwrap();
    vmo1.test_write(1, 2);
    let vmo2 = vmo1.create_child(false, 0, 3 * PAGE_SIZE).unwrap();
    vmo2.test_write(2, 3);
    assert_eq!(vmo0.get_info().committed_bytes as usize, PAGE_SIZE);
    assert_eq!(vmo1.get_info().committed_bytes as usize, PAGE_SIZE);
    assert_eq!(vmo2.get_info().committed_bytes as usize, PAGE_SIZE);
}

#[test]
fn many_clones() {
    const N: usize = 4;
    let old: u8 = 0xa;
    let new: u8 = 0xb;
    let permutations = [
        [0, 1, 2, 3],
        [0, 1, 3, 2],
        [0, 2, 1, 3],
        [0, 2, 3, 1],
        [0, 3, 1, 2],
        [0, 3, 2, 1],
        [1, 0, 2, 3],
        [1, 0, 3, 2],
        [1, 2, 0, 3],
        [1, 2, 3, 0],
        [1, 3, 0, 2],
        [1, 3, 2, 0],
        [2, 1, 0, 3],
        [2, 1, 3, 0],
        [2, 0, 1, 3],
        [2, 0, 3, 1],
        [2, 3, 1, 0],
        [2, 3, 0, 1],
        [3, 1, 2, 0],
        [3, 1, 0, 2],
        [3, 2, 1, 0],
        [3, 2, 0, 1],
        [3, 0, 1, 2],
        [3, 0, 2, 1],
    ];
    for i in 0..24 {
        let vmo0 = VmObject::new_paged(1);
        vmo0.write(0, &[old]).unwrap();
        let vmo1 = vmo0.create_child(false, 0, PAGE_SIZE).unwrap();
        let vmo2 = vmo0.create_child(false, 0, PAGE_SIZE).unwrap();
        let vmo3 = vmo1.create_child(false, 0, PAGE_SIZE).unwrap();
        let vmos: [Arc<VmObject>; 4] = [vmo0.clone(), vmo1.clone(), vmo2.clone(), vmo3.clone()];
        let mut write: [bool; 4] = [false; 4];
        let perm = permutations[i];
        println!("{:?}", perm);
        for j in 0..N {
            println!("j = {}, write = {}", j, perm[j]);
            vmos[perm[j]].write(0, &[new]).unwrap();
            write[perm[j]] = true;
            let mut buf: [u8; 1] = [0];
            for k in 0..N {
                vmos[k].read(0, &mut buf).unwrap();
                println!("vmo[{}] = {:x}", k, buf[0]);
                if write[k] {
                    assert!(buf[0] == new);
                } else {
                    assert!(buf[0] == old);
                }
                buf[0] = 0;
            }
        }
    }
}

impl VmObject {
    pub fn test_write(&self, page: usize, value: u8) {
        self.write(page * PAGE_SIZE, &[value]).unwrap();
    }

    pub fn test_read(&self, page: usize) -> u8 {
        let mut buf = [0; 1];
        self.read(page * PAGE_SIZE, &mut buf).unwrap();
        buf[0]
    }
}

/// `read` and `write` had no bound at all: a read four pages into a
/// one-page object answered `Ok(())` after resolving page 4 through the
/// demand-zero path, and the matching write would have allocated a frame
/// at an index `len()` says does not exist, where nothing ever frees it.
#[test]
fn a_range_outside_the_object_is_refused_by_the_object() {
    let vmo = VmObject::new_paged(1);
    let mut buf = [0u8; 8];
    assert_eq!(vmo.len(), PAGE_SIZE);
    assert_eq!(vmo.read(PAGE_SIZE - 8, &mut buf), Ok(()));

    assert_eq!(
        vmo.read(4 * PAGE_SIZE, &mut buf),
        Err(ZxError::OUT_OF_RANGE)
    );
    assert_eq!(
        vmo.read(PAGE_SIZE - 7, &mut buf),
        Err(ZxError::OUT_OF_RANGE)
    );
    assert_eq!(vmo.write(PAGE_SIZE, &[1u8; 1]), Err(ZxError::OUT_OF_RANGE));
    assert_eq!(
        vmo.committed_pages_in_range(0, 1),
        0,
        "a refused read must not have committed anything"
    );
}

/// The three range operations share one shape of bound, and all three of
/// them have to survive a sum that wraps -- `zx_vmo_op_range(ZX_VMO_OP_
/// ZERO)` passes `offset` and `len` through from userspace untouched.
#[test]
fn a_range_whose_end_wraps_is_refused_too() {
    let vmo = VmObject::new_paged(2);
    let mut buf = [0u8; 8];
    assert_eq!(
        vmo.read(usize::MAX - 3, &mut buf),
        Err(ZxError::OUT_OF_RANGE)
    );
    assert_eq!(
        vmo.write(usize::MAX - 3, &[0u8; 8]),
        Err(ZxError::OUT_OF_RANGE)
    );
    assert_eq!(vmo.zero(usize::MAX - 3, 8), Err(ZxError::OUT_OF_RANGE));
    assert_eq!(
        vmo.zero(usize::MAX - PAGE_SIZE + 1, 2 * PAGE_SIZE),
        Err(ZxError::OUT_OF_RANGE)
    );
    // The whole object is still in range.
    assert_eq!(vmo.zero(0, 2 * PAGE_SIZE), Ok(()));
}

/// `align_log2` is an absolute log2, so 0 is not "page alignment": the
/// frame allocator is handed `align_log2 - PAGE_SIZE_LOG2`, and anything
/// below the page shift made that underflow. `sys_vmo_create_contiguous`
/// maps 0 to `PAGE_SIZE_LOG2` and rejects the rest, but it is the only
/// caller that knows, and it is two crates away.
#[test]
fn a_contiguous_vmo_takes_an_absolute_log2_alignment() {
    assert!(VmObject::new_contiguous(1, PAGE_SIZE_LOG2).is_ok());
    assert!(VmObject::new_contiguous(1, PAGE_SIZE_LOG2 + 1).is_ok());
    assert_eq!(
        VmObject::new_contiguous(1, 0).err(),
        Some(ZxError::INVALID_ARGS)
    );
    assert_eq!(
        VmObject::new_contiguous(1, PAGE_SIZE_LOG2 - 1).err(),
        Some(ZxError::INVALID_ARGS)
    );
}

/// Counting pages is a question, not a command: asking about pages the
/// object does not have answers zero. It used to assert, and the caller
/// that runs on every `/proc/<pid>/status` read carries a hand-written
/// clamp and an early return purely to stay out of that assertion.
#[test]
fn counting_pages_past_the_end_answers_zero_instead_of_panicking() {
    let vmo = VmObject::new_paged(2);
    vmo.commit_page(0, MMUFlags::WRITE).unwrap();
    assert_eq!(vmo.committed_pages_in_range(0, 2), 1);
    assert_eq!(vmo.committed_pages_in_range(0, 64), 1);
    assert_eq!(vmo.committed_pages_in_range(2, 64), 0);
    assert_eq!(vmo.committed_pages_in_range(64, 65), 0);
    assert_eq!(vmo.committed_pages_in_range(64, 8), 0);
}
