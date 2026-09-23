use super::*;

pub struct VMObjectSlice {
    /// Parent node.
    parent: Arc<dyn VMObjectTrait>,
    /// The offset from parent.
    offset: usize,
    /// The size in bytes.
    size: usize,
}

impl VMObjectSlice {
    pub fn new(parent: Arc<dyn VMObjectTrait>, offset: usize, size: usize) -> Arc<Self> {
        Arc::new(VMObjectSlice {
            parent,
            offset,
            size,
        })
    }

    fn check_range(&self, offset: usize, len: usize) -> ZxResult {
        // `offset + len` is the exclusive end of the range, so it may equal
        // `self.size`. Use checked arithmetic so a wrapping `offset + len`
        // cannot bypass the bound, and `>` so a full-size range is accepted.
        let end = offset.checked_add(len).ok_or(ZxError::OUT_OF_RANGE)?;
        if end > self.size {
            return Err(ZxError::OUT_OF_RANGE);
        }
        Ok(())
    }
}

impl VMObjectTrait for VMObjectSlice {
    fn read(&self, offset: usize, buf: &mut [u8]) -> ZxResult {
        self.check_range(offset, buf.len())?;
        self.parent.read(offset + self.offset, buf)
    }

    fn write(&self, offset: usize, buf: &[u8]) -> ZxResult {
        self.check_range(offset, buf.len())?;
        self.parent.write(offset + self.offset, buf)
    }

    fn zero(&self, offset: usize, len: usize) -> ZxResult {
        self.check_range(offset, len)?;
        self.parent.zero(offset + self.offset, len)
    }

    fn len(&self) -> usize {
        self.size
    }

    fn set_len(&self, _len: usize) -> ZxResult {
        // A window onto the parent's pages has no size of its own to change.
        // Unreachable today (`create_slice` builds a `VmObject` that is not
        // `resizable` and not `unbounded`, and those are the two doors into
        // this method), but the body was `unimplemented!()`, which is a kernel
        // panic waiting for a third caller.
        Err(ZxError::NOT_SUPPORTED)
    }

    fn commit_page(&self, page_idx: usize, flags: MMUFlags) -> ZxResult<usize> {
        // A page index past the end of the window is a question about the
        // parent's pages, not this object's. Unchecked, `page_idx +
        // self.offset / PAGE_SIZE` walked straight out of the window: a
        // one-page slice answered `commit_page(2)` with its parent's fourth
        // frame, and `VmMapping` installs whatever comes back as a user PTE.
        if page_idx >= self.size / PAGE_SIZE {
            return Err(ZxError::OUT_OF_RANGE);
        }
        self.parent
            .commit_page(page_idx + self.offset / PAGE_SIZE, flags)
    }

    fn commit_pages_with(
        &self,
        f: &mut dyn FnMut(&mut dyn FnMut(usize, MMUFlags) -> ZxResult<PhysAddr>) -> ZxResult,
    ) -> ZxResult {
        // The index `f` passes is in THIS object's page space -- `VmMapping`
        // computes it as `vmo_offset / PAGE_SIZE + i` over the VMO it is a
        // mapping of. Handing `f` to the parent unchanged let the parent read
        // those indices as its own, so every eager map of a slice committed
        // the parent's pages starting at index 0 instead of at the slice's
        // offset: a slice at `2 * PAGE_SIZE` mapped the parent's pages 0 and 1
        // where `commit_page` -- the demand-fault path for the same mapping --
        // answers with pages 2 and 3. The eager path is the only path under
        // libos, and the `map_range = true` path (the vDSO, kernel loads) on
        // hardware.
        let page_offset = self.offset / PAGE_SIZE;
        let len_pages = self.size / PAGE_SIZE;
        self.parent.commit_pages_with(&mut |commit| {
            f(&mut |page_idx, flags| {
                if page_idx >= len_pages {
                    return Err(ZxError::OUT_OF_RANGE);
                }
                commit(page_idx + page_offset, flags)
            })
        })
    }

    fn commit(&self, offset: usize, len: usize) -> ZxResult {
        self.check_range(offset, len)?;
        self.parent.commit(offset + self.offset, len)
    }

    fn decommit(&self, offset: usize, len: usize) -> ZxResult {
        // Without this the window was one-way: `zx_vmo_op_range(DECOMMIT)`
        // hands `offset` and `len` through unbounded, so a one-page slice
        // could free every committed page of its parent.
        self.check_range(offset, len)?;
        self.parent.decommit(offset + self.offset, len)
    }

    fn decommit_seq(&self) -> (u64, u64) {
        // A slice's pages ARE the parent's pages, and `decommit` above frees
        // them through the parent, so the parent's generation is the one a
        // fault through this slice has to watch.
        self.parent.decommit_seq()
    }

    fn create_child(&self, _offset: usize, _len: usize) -> ZxResult<Arc<dyn VMObjectTrait>> {
        Err(ZxError::NOT_SUPPORTED)
    }

    fn complete_info(&self, info: &mut VmoInfo) {
        self.parent.complete_info(info);
    }

    fn cache_policy(&self) -> CachePolicy {
        self.parent.cache_policy()
    }

    fn set_cache_policy(&self, _policy: CachePolicy) -> ZxResult {
        Ok(())
    }

    fn committed_pages_in_range(&self, start_idx: usize, end_idx: usize) -> usize {
        // Clamp to the window before shifting into the parent's index space.
        // `VMObjectPaged`'s own implementation asserts that `end_idx` is
        // within its size -- an assertion a caller already had to work around
        // by hand (`VmMapping::fill_task_stats`, which is what a
        // `/proc/<pid>/status` read runs) -- and a slice that passed a too-far
        // `end_idx` straight through would walk right into it.
        let po = pages(self.offset);
        let end = end_idx.min(self.size / PAGE_SIZE);
        if start_idx >= end {
            // Which is also what keeps `start_idx + po` below from wrapping.
            return 0;
        }
        self.parent
            .committed_pages_in_range(start_idx + po, end + po)
    }

    fn pin(&self, offset: usize, len: usize) -> ZxResult {
        self.check_range(offset, len)?;
        self.parent.pin(offset + self.offset, len)
    }

    fn unpin(&self, offset: usize, len: usize) -> ZxResult {
        self.check_range(offset, len)?;
        self.parent.unpin(offset + self.offset, len)
    }

    fn is_contiguous(&self) -> bool {
        self.parent.is_contiguous()
    }

    fn is_paged(&self) -> bool {
        self.parent.is_paged()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A four-page parent with a distinct frame per page, and a two-page
    /// window onto its second half. Every page is committed for WRITE first,
    /// because a read-only demand-zero page resolves to the ONE shared zero
    /// frame and four equal addresses prove nothing about which page was
    /// meant.
    fn parent_and_slice() -> (Arc<VmObject>, Arc<VmObject>, [PhysAddr; 4]) {
        let parent = VmObject::new_paged(4);
        let frames = [
            parent.commit_page(0, MMUFlags::WRITE).unwrap(),
            parent.commit_page(1, MMUFlags::WRITE).unwrap(),
            parent.commit_page(2, MMUFlags::WRITE).unwrap(),
            parent.commit_page(3, MMUFlags::WRITE).unwrap(),
        ];
        let slice = parent.create_slice(2 * PAGE_SIZE, 2 * PAGE_SIZE).unwrap();
        (parent, slice, frames)
    }

    /// The window's own two pages, by both names, and nothing beyond them.
    #[test]
    fn a_slice_reads_and_writes_only_the_pages_it_covers() {
        let (parent, slice, _) = parent_and_slice();
        parent.write(2 * PAGE_SIZE, &[0xA5; 4]).unwrap();
        let mut buf = [0u8; 4];
        slice.read(0, &mut buf).unwrap();
        assert_eq!(
            buf, [0xA5; 4],
            "offset 0 of the slice is page 2 of the parent"
        );

        assert_eq!(slice.len(), 2 * PAGE_SIZE);
        assert_eq!(
            slice.read(2 * PAGE_SIZE, &mut buf),
            Err(ZxError::OUT_OF_RANGE)
        );
        assert_eq!(
            slice.write(2 * PAGE_SIZE, &[0; 4]),
            Err(ZxError::OUT_OF_RANGE)
        );
        assert_eq!(slice.zero(0, 2 * PAGE_SIZE + 1), Err(ZxError::OUT_OF_RANGE));
        // And a sum that wraps does not come back inside the window: the
        // parent would take `usize::MAX - 3 + self.offset` from here.
        assert_eq!(
            slice.read(usize::MAX - 3, &mut buf),
            Err(ZxError::OUT_OF_RANGE)
        );
        assert_eq!(
            slice.write(usize::MAX - 3, &[0u8; 4]),
            Err(ZxError::OUT_OF_RANGE)
        );
        assert_eq!(
            slice.decommit(usize::MAX - 3, 8),
            Err(ZxError::OUT_OF_RANGE)
        );
    }

    /// The demand-fault path (`commit_page`) and the eager path
    /// (`commit_pages_with`, which is `VmMapping::map` -- the ONLY path under
    /// libos and the `map_range = true` path on hardware) have to name the
    /// same frame for the same page of the same object.
    ///
    /// They did not: the eager one handed the caller's closure to the parent
    /// unchanged, so the parent read indices meant for the window as its own
    /// and a slice at `2 * PAGE_SIZE` mapped the parent's pages 0 and 1.
    #[test]
    fn the_eager_and_the_faulting_commit_name_the_same_frame() {
        let (_parent, slice, frames) = parent_and_slice();
        let by_fault: Vec<PhysAddr> = (0..2)
            .map(|i| slice.commit_page(i, MMUFlags::WRITE).unwrap())
            .collect();
        assert_eq!(
            by_fault,
            [frames[2], frames[3]],
            "the window is pages 2 and 3"
        );

        let mut eager = Vec::new();
        slice
            .commit_pages_with(&mut |commit| {
                for i in 0..2 {
                    eager.push(commit(i, MMUFlags::WRITE)?);
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(eager, by_fault);
    }

    /// A page index past the window is a question about the parent's pages,
    /// and a slice is not the place to ask it. Unchecked, `page_idx +
    /// self.offset / PAGE_SIZE` simply kept walking.
    #[test]
    fn a_page_index_past_the_window_is_not_the_parents_page() {
        let (parent, slice, _) = parent_and_slice();
        // The parent has a page there; the slice still must not hand it out.
        assert!(parent.commit_page(3, MMUFlags::WRITE).is_ok());
        assert_eq!(
            slice.commit_page(2, MMUFlags::WRITE),
            Err(ZxError::OUT_OF_RANGE)
        );
        assert_eq!(
            slice.commit_page(usize::MAX, MMUFlags::READ),
            Err(ZxError::OUT_OF_RANGE)
        );
        let eager = slice.commit_pages_with(&mut |commit| {
            commit(2, MMUFlags::WRITE)?;
            Ok(())
        });
        assert_eq!(eager, Err(ZxError::OUT_OF_RANGE));
    }

    /// `zx_vmo_op_range(COMMIT / DECOMMIT)` passes `offset` and `len` through
    /// with no bound of its own, so before this a one-page window could free
    /// every committed page of its parent.
    #[test]
    fn a_slice_cannot_decommit_its_parents_other_pages() {
        let (parent, slice, _) = parent_and_slice();
        assert_eq!(parent.committed_pages_in_range(0, 4), 4);

        assert_eq!(slice.decommit(0, 4 * PAGE_SIZE), Err(ZxError::OUT_OF_RANGE));
        assert_eq!(slice.commit(0, 4 * PAGE_SIZE), Err(ZxError::OUT_OF_RANGE));
        assert_eq!(
            parent.committed_pages_in_range(0, 4),
            4,
            "the refusal must leave the parent alone"
        );

        // Its own window it may still decommit.
        slice.decommit(0, 2 * PAGE_SIZE).unwrap();
        assert_eq!(parent.committed_pages_in_range(0, 2), 2);
        assert_eq!(parent.committed_pages_in_range(2, 4), 0);
    }

    /// Counting stops at the window's edge. `VMObjectPaged` asserts that the
    /// index it is handed is inside itself, so a range shifted by the slice's
    /// offset and not clamped first walks into that assertion -- and the
    /// counting path is `/proc/<pid>/status`.
    #[test]
    fn counting_committed_pages_stops_at_the_windows_edge() {
        let (_parent, slice, _) = parent_and_slice();
        assert_eq!(slice.committed_pages_in_range(0, 2), 2);
        assert_eq!(slice.committed_pages_in_range(0, 64), 2);
        assert_eq!(slice.committed_pages_in_range(2, 64), 0);
        assert_eq!(slice.committed_pages_in_range(64, 65), 0);
        assert_eq!(slice.committed_pages_in_range(usize::MAX, 65), 0);

        // The window above sits at the END of its parent, where an unclamped
        // end lands inside the parent anyway and counts the same pages. A
        // window at the FRONT is where the clamp shows: the parent has four
        // committed pages, and two of them are none of this object's business.
        let parent = VmObject::new_paged(4);
        for i in 0..4 {
            parent.commit_page(i, MMUFlags::WRITE).unwrap();
        }
        let front = parent.create_slice(0, 2 * PAGE_SIZE).unwrap();
        assert_eq!(parent.committed_pages_in_range(0, 4), 4);
        assert_eq!(front.committed_pages_in_range(0, 64), 2);
    }

    /// A window onto somebody else's pages has no size of its own.
    #[test]
    fn a_slice_has_no_size_to_change() {
        let (_parent, slice, _) = parent_and_slice();
        assert_eq!(
            VMObjectTrait::set_len(&***slice, PAGE_SIZE),
            Err(ZxError::NOT_SUPPORTED)
        );
        assert_eq!(slice.set_len(PAGE_SIZE), Err(ZxError::UNAVAILABLE));
    }
}
