use {super::*, alloc::sync::Arc, lock::Mutex};

/// VMO representing a physical range of memory.
pub struct VMObjectPhysical {
    paddr: PhysAddr,
    pages: usize,
    /// Lock this when access physical memory.
    data_lock: Mutex<()>,
    inner: Mutex<VMObjectPhysicalInner>,
}

struct VMObjectPhysicalInner {
    cache_policy: CachePolicy,
}

impl VMObjectPhysicalInner {
    pub fn new() -> VMObjectPhysicalInner {
        VMObjectPhysicalInner {
            cache_policy: CachePolicy::Uncached,
        }
    }
}

impl VMObjectPhysical {
    /// Create a new VMO representing a piece of contiguous physical memory.
    /// You must ensure nobody has the ownership of this piece of memory yet.
    pub fn new(paddr: PhysAddr, pages: usize) -> Arc<Self> {
        assert!(page_aligned(paddr));
        Arc::new(VMObjectPhysical {
            paddr,
            pages,
            data_lock: Mutex::default(),
            inner: Mutex::new(VMObjectPhysicalInner::new()),
        })
    }

    /// The bound every range operation on this window shares, spelled the same
    /// way [`VMObjectSlice::check_range`] spells it.
    ///
    /// `offset + len` is the exclusive end of the range, so it may equal
    /// `self.len()`, and the addition is checked because both numbers come
    /// from the caller. The three operations below used to assert this
    /// instead, which is two separate problems in a kernel: an ordinary
    /// out-of-range request panicked, and in a release build -- where the
    /// `+` wraps rather than trapping -- a large `offset` with a small `len`
    /// passed the assertion and then reached `pmem_read`/`pmem_write`/
    /// `pmem_zero` at `self.paddr + offset`, memory this window does not own.
    ///
    /// Not hypothetical: `zx_vmo_op_range(ZX_VMO_OP_ZERO)` passes `offset`
    /// and `len` from userspace to [`VMObjectTrait::zero`] with no bound of
    /// its own, unlike `zx_vmo_read`/`zx_vmo_write`, which check theirs.
    fn check_range(&self, offset: usize, len: usize) -> ZxResult {
        let end = offset.checked_add(len).ok_or(ZxError::OUT_OF_RANGE)?;
        if end > self.len() {
            return Err(ZxError::OUT_OF_RANGE);
        }
        Ok(())
    }
}

impl VMObjectTrait for VMObjectPhysical {
    fn read(&self, offset: usize, buf: &mut [u8]) -> ZxResult {
        self.check_range(offset, buf.len())?;
        let _ = self.data_lock.lock();
        kernel_hal::mem::pmem_read(self.paddr + offset, buf);
        Ok(())
    }

    fn write(&self, offset: usize, buf: &[u8]) -> ZxResult {
        self.check_range(offset, buf.len())?;
        let _ = self.data_lock.lock();
        kernel_hal::mem::pmem_write(self.paddr + offset, buf);
        Ok(())
    }

    fn zero(&self, offset: usize, len: usize) -> ZxResult {
        self.check_range(offset, len)?;
        let _ = self.data_lock.lock();
        kernel_hal::mem::pmem_zero(self.paddr + offset, len);
        Ok(())
    }

    fn len(&self) -> usize {
        self.pages * PAGE_SIZE
    }

    fn set_len(&self, _len: usize) -> ZxResult {
        // A window onto memory somebody else owns has no size of its own to
        // change. No caller reaches this today -- `VmObject::set_len` answers
        // `UNAVAILABLE` first because `new_physical` is not `resizable`, and
        // the only other caller, `grow_unbounded_backing`, needs `unbounded`,
        // which it is not either -- but the body was `unimplemented!()`, and
        // an `unimplemented!()` reachable only by accident is still a kernel
        // panic when the accident happens.
        Err(ZxError::NOT_SUPPORTED)
    }

    fn commit_page(&self, page_idx: usize, _flags: MMUFlags) -> ZxResult<PhysAddr> {
        // Every page of the window is always committed, so there is nothing to
        // do -- but an index past the end is a question about memory this
        // object does not describe, and the answer used to be
        // `self.paddr + page_idx * PAGE_SIZE`: an arbitrary physical address,
        // handed to `VmMapping` to install as a user PTE. `committed_paddr`
        // below has always bounded the same lookup; these two accessors now
        // agree.
        self.committed_paddr(page_idx).ok_or(ZxError::OUT_OF_RANGE)
    }

    fn commit_pages_with(
        &self,
        f: &mut dyn FnMut(&mut dyn FnMut(usize, MMUFlags) -> ZxResult<PhysAddr>) -> ZxResult,
    ) -> ZxResult {
        f(&mut |page_idx, _flags| self.committed_paddr(page_idx).ok_or(ZxError::OUT_OF_RANGE))
    }

    fn commit(&self, offset: usize, len: usize) -> ZxResult {
        // Nothing to commit -- every page of the window always is -- but a
        // range that leaves the window is still a range this object cannot
        // answer for, and `zx_vmo_op_range(ZX_VMO_OP_COMMIT)` asks with the
        // numbers it was given.
        self.check_range(offset, len)
    }

    fn decommit(&self, offset: usize, len: usize) -> ZxResult {
        // Nothing to decommit: these frames are not the allocator's.
        self.check_range(offset, len)
    }

    fn create_child(&self, _offset: usize, _len: usize) -> ZxResult<Arc<dyn VMObjectTrait>> {
        Err(ZxError::NOT_SUPPORTED)
    }

    fn complete_info(&self, _info: &mut VmoInfo) {
        warn!("VmoInfo for physical is unimplemented");
    }

    fn cache_policy(&self) -> CachePolicy {
        let inner = self.inner.lock();
        inner.cache_policy
    }

    fn set_cache_policy(&self, policy: CachePolicy) -> ZxResult {
        let mut inner = self.inner.lock();
        inner.cache_policy = policy;
        Ok(())
    }

    fn committed_pages_in_range(&self, _start_idx: usize, _end_idx: usize) -> usize {
        0
    }

    fn is_contiguous(&self) -> bool {
        true
    }

    fn is_physical(&self) -> bool {
        true
    }

    fn committed_paddr(&self, page_idx: usize) -> Option<PhysAddr> {
        // A physical window is a fixed range: every page is always "committed".
        // The lazy fork map therefore installs all its PTEs eagerly — correct
        // for device memory (framebuffer/dumb buffers) that userspace expects
        // to be mapped, and costs no allocation.
        if page_idx < self.pages {
            Some(self.paddr + page_idx * PAGE_SIZE)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel_hal::mem::PhysFrame;
    use kernel_hal::CachePolicy;

    #[test]
    fn read_write() {
        // Own the window instead of naming a fixed address. Under libos the
        // frame allocator hands out every frame from 0x1000 up, so `0x1000`
        // was memory another VMO could be committed to at the same time --
        // this test then wrote its four bytes straight through that VMO's
        // page. It only bit when the suite ran under enough load for the
        // allocator to reach frame 1 while another test held it, which is why
        // it surfaced as an occasional failure elsewhere (the concurrent
        // append test, losing exactly four bytes).
        //
        // Holding the frames for the body of the test makes the range
        // genuinely exclusive, which is what a physical window models.
        let frames = PhysFrame::new_contiguous(2, 0);
        assert_eq!(frames.len(), 2, "could not reserve two contiguous frames");
        let vmo = VmObject::new_physical(frames[0].paddr(), 2);
        assert_eq!(vmo.cache_policy(), CachePolicy::Uncached);
        super::super::tests::read_write(&vmo);
    }

    /// Reserve a two-page window and hand it to `f` as a VMO. The frames stay
    /// owned for the body of the test, so the range really is exclusive --
    /// see the note on `read_write` above.
    fn with_window(f: impl FnOnce(&VmObject, usize)) {
        let frames = PhysFrame::new_contiguous(2, 0);
        assert_eq!(frames.len(), 2, "could not reserve two contiguous frames");
        let vmo = VmObject::new_physical(frames[0].paddr(), 2);
        let len = vmo.len();
        assert_eq!(len, 2 * PAGE_SIZE);
        f(&vmo, len);
    }

    /// A range that ends outside the window is refused, and refused with an
    /// errno rather than by taking the kernel down.
    #[test]
    fn a_range_that_leaves_the_window_is_an_error_not_a_panic() {
        with_window(|vmo, len| {
            let mut buf = [0u8; 8];
            // The whole window, and the last byte of it, are in range.
            assert_eq!(vmo.read(0, &mut [0u8; 2 * PAGE_SIZE]), Ok(()));
            assert_eq!(vmo.read(len - 8, &mut buf), Ok(()));
            assert_eq!(vmo.zero(0, len), Ok(()));

            // One byte past is not.
            assert_eq!(vmo.read(len - 7, &mut buf), Err(ZxError::OUT_OF_RANGE));
            assert_eq!(vmo.write(len, &[0u8; 1]), Err(ZxError::OUT_OF_RANGE));
            assert_eq!(vmo.zero(0, len + 1), Err(ZxError::OUT_OF_RANGE));
            // `zx_vmo_op_range` passes these two straight through for ZERO,
            // COMMIT and DECOMMIT alike, so this is the shape a process can
            // ask for. The last two have nothing to do either way -- every
            // page of a physical window always is committed, and its frames
            // are not the allocator's -- but a range that leaves the window
            // is still a range this object cannot answer for.
            assert_eq!(vmo.zero(len, PAGE_SIZE), Err(ZxError::OUT_OF_RANGE));
            assert_eq!(vmo.commit(0, len), Ok(()));
            assert_eq!(vmo.decommit(0, len), Ok(()));
            assert_eq!(vmo.commit(0, len + PAGE_SIZE), Err(ZxError::OUT_OF_RANGE));
            assert_eq!(vmo.decommit(len, PAGE_SIZE), Err(ZxError::OUT_OF_RANGE));
        });
    }

    /// The bound is on `offset + len`, so it has to survive an `offset` that
    /// makes that sum wrap: `usize::MAX - 3 + 8` is `4`, which a release build
    /// computes without complaint and which is comfortably "inside" a two-page
    /// window. What follows the check is `pmem_read(self.paddr + offset, ..)`.
    #[test]
    fn an_offset_that_wraps_does_not_land_back_inside_the_window() {
        with_window(|vmo, _| {
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
            // And the plain `usize::MAX` case, where nothing wraps.
            assert_eq!(vmo.read(usize::MAX, &mut buf), Err(ZxError::OUT_OF_RANGE));
        });
    }

    /// `commit_page` and `committed_paddr` answer the same question about the
    /// same page, so they have to answer it the same way -- including at an
    /// index the window does not have, where one of them used to return an
    /// address `paddr + page_idx * PAGE_SIZE` pages away from anything this
    /// object owns. `VmMapping` installs what `commit_page` returns as a user
    /// PTE.
    #[test]
    fn the_two_ways_of_naming_a_page_agree_inside_the_window_and_outside_it() {
        with_window(|vmo, _| {
            for idx in 0..2 {
                let committed = vmo.committed_paddr(idx).expect("page is in the window");
                assert_eq!(vmo.commit_page(idx, MMUFlags::READ), Ok(committed));
            }
            assert_eq!(vmo.committed_paddr(2), None);
            assert_eq!(
                vmo.commit_page(2, MMUFlags::READ),
                Err(ZxError::OUT_OF_RANGE)
            );
            assert_eq!(
                vmo.commit_page(99, MMUFlags::WRITE),
                Err(ZxError::OUT_OF_RANGE)
            );
            // The eager path (`VmMapping::map`) asks through this closure
            // instead, and gets the same answers.
            let mut eager = alloc::vec::Vec::new();
            vmo.commit_pages_with(&mut |commit| {
                for idx in 0..2 {
                    eager.push(commit(idx, MMUFlags::READ)?);
                }
                Ok(())
            })
            .unwrap();
            assert_eq!(
                eager,
                [
                    vmo.committed_paddr(0).unwrap(),
                    vmo.committed_paddr(1).unwrap()
                ]
            );
            let out = vmo.commit_pages_with(&mut |commit| {
                commit(2, MMUFlags::READ)?;
                Ok(())
            });
            assert_eq!(out, Err(ZxError::OUT_OF_RANGE));
        });
    }

    /// A window onto memory somebody else owns has no size of its own. The
    /// object answers `NOT_SUPPORTED`; the `VmObject` around it never gets
    /// that far, because a physical VMO is not resizable.
    #[test]
    fn a_physical_window_has_no_size_to_change() {
        with_window(|vmo, len| {
            assert_eq!(
                VMObjectTrait::set_len(&***vmo, len + PAGE_SIZE),
                Err(ZxError::NOT_SUPPORTED)
            );
            assert_eq!(vmo.set_len(len + PAGE_SIZE), Err(ZxError::UNAVAILABLE));
        });
    }
}
