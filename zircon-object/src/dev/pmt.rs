use {
    super::*,
    crate::object::*,
    crate::vm::*,
    alloc::{
        sync::{Arc, Weak},
        vec,
        vec::Vec,
    },
};

/// Pinned Memory Token.
///
/// It will pin memory on construction and unpin on drop.
pub struct PinnedMemoryToken {
    base: KObjectBase,
    bti: Weak<BusTransactionInitiator>,
    vmo: Arc<VmObject>,
    offset: usize,
    size: usize,
    mapped_addrs: Vec<DevVAddr>,
}

impl_kobject!(PinnedMemoryToken);

impl Drop for PinnedMemoryToken {
    fn drop(&mut self) {
        if self.vmo.is_paged() {
            // Closing a handle has nobody to report to. What used to reach this
            // was a `zx_vmo_op_range` decommit of the pinned range, which the
            // VMO now refuses; a kernel panic is still not how a hole here
            // should be announced, so it goes to the log loudly instead.
            if let Err(err) = self.vmo.unpin(self.offset, self.size) {
                error!(
                    "unpinning a dropped memory token answered {:?} (off={:#x}, len={:#x})",
                    err, self.offset, self.size,
                );
            }
        }
    }
}

impl PinnedMemoryToken {
    /// Create a `PinnedMemoryToken` by `BusTransactionInitiator`.
    pub(crate) fn create(
        bti: &Arc<BusTransactionInitiator>,
        vmo: Arc<VmObject>,
        perms: IommuPerms,
        offset: usize,
        size: usize,
    ) -> ZxResult<Arc<Self>> {
        // A pin starts and ends on a page. `map_into_iommu` does not check
        // that, it *asserts* it -- a size that is not a whole number of the
        // runs of contiguity the IOMMU guarantees -- and an assert in a kernel
        // object reached from a syscall is a kernel panic. The only guard for
        // it lived in `sys_bti_pin`, two crates away, which is where the
        // alignment guard for `zx_vmo_create_contiguous` lived too until an
        // unguarded path turned out to reach the panic. `vmo.pin` answers
        // `BAD_STATE` for an unaligned range whose last page is not committed,
        // which hides this most of the time; a buffer that is already
        // committed walks past it and into the assert.
        if !page_aligned(offset) || !page_aligned(size) {
            return Err(ZxError::INVALID_ARGS);
        }
        if vmo.is_paged() {
            vmo.commit(offset, size)?;
            vmo.pin(offset, size)?;
        }
        // No token exists yet to undo the pin from `Drop`, so a mapping the
        // IOMMU refuses (no permissions, a window it cannot commit) has to
        // give the pages back here. It used to leave them pinned for the
        // life of the object: no decommit or resize ever succeeded again.
        let mapped_addrs = Self::map_into_iommu(&bti.iommu(), vmo.clone(), offset, size, perms)
            .inspect_err(|_| {
                if vmo.is_paged() {
                    vmo.unpin(offset, size).ok();
                }
            })?;
        Ok(Arc::new(PinnedMemoryToken {
            base: KObjectBase::new(),
            bti: Arc::downgrade(bti),
            vmo,
            offset,
            size,
            mapped_addrs,
        }))
    }

    /// Used during initialization to set up the IOMMU state for this PMT.
    fn map_into_iommu(
        iommu: &Arc<Iommu>,
        vmo: Arc<VmObject>,
        offset: usize,
        size: usize,
        perms: IommuPerms,
    ) -> ZxResult<Vec<DevVAddr>> {
        if vmo.is_contiguous() {
            let (vaddr, _mapped_len) = iommu.map_contiguous(vmo, offset, size, perms)?;
            Ok(vec![vaddr])
        } else {
            assert_eq!(size % iommu.minimum_contiguity(), 0);
            let mut mapped_addrs: Vec<DevVAddr> = Vec::new();
            let mut remaining = size;
            let mut cur_offset = offset;
            while remaining > 0 {
                let (mut vaddr, mapped_len) =
                    iommu.map(vmo.clone(), cur_offset, remaining, perms)?;
                assert_eq!(mapped_len % iommu.minimum_contiguity(), 0);
                for _ in 0..mapped_len / iommu.minimum_contiguity() {
                    mapped_addrs.push(vaddr);
                    vaddr += iommu.minimum_contiguity();
                }
                remaining -= mapped_len;
                cur_offset += mapped_len;
            }
            Ok(mapped_addrs)
        }
    }

    /// Encode the mapped addresses.
    pub fn encode_addrs(
        &self,
        compress_results: bool,
        contiguous: bool,
    ) -> ZxResult<Vec<DevVAddr>> {
        let iommu = self.bti.upgrade().unwrap().iommu();
        if compress_results {
            if self.vmo.is_contiguous() {
                let num_addrs = ceil(self.size, iommu.minimum_contiguity());
                let min_contig = iommu.minimum_contiguity();
                let base = self.mapped_addrs[0];
                Ok((0..num_addrs).map(|i| base + min_contig * i).collect())
            } else {
                Ok(self.mapped_addrs.clone())
            }
        } else if contiguous {
            if !self.vmo.is_contiguous() {
                Err(ZxError::INVALID_ARGS)
            } else {
                Ok(vec![self.mapped_addrs[0]])
            }
        } else {
            let min_contig = if self.vmo.is_contiguous() {
                self.size
            } else {
                iommu.minimum_contiguity()
            };
            let num_pages = self.size / PAGE_SIZE;
            let mut encoded_addrs: Vec<DevVAddr> = Vec::new();
            for base in &self.mapped_addrs {
                let mut addr = *base;
                while addr < base + min_contig && encoded_addrs.len() < num_pages {
                    encoded_addrs.push(addr);
                    addr += PAGE_SIZE; // not sure ...
                }
            }
            Ok(encoded_addrs)
        }
    }

    /// Unpin pages and revoke device access to them.
    pub fn unpin(&self) {
        if let Some(bti) = self.bti.upgrade() {
            bti.release_pmt(self.base.id);
        }
    }
}

#[cfg(test)]
mod tests {
    //! A pinned memory token is the kernel's promise to a device that a buffer
    //! stays where the IOMMU was told it is, from `zx_bti_pin` until the token
    //! goes. These measure that promise from the token's end.
    use super::*;
    use crate::dev::{BusTransactionInitiator, Iommu, IommuPerms};

    fn pinned_page() -> (Arc<VmObject>, Arc<BusTransactionInitiator>) {
        let vmo = VmObject::new_paged_with_resizable(true, 2);
        vmo.commit(0, 2 * PAGE_SIZE).unwrap();
        let bti = BusTransactionInitiator::create(Iommu::create(), 0);
        (vmo, bti)
    }

    #[test]
    /// The sequence a driver runs: pin a buffer, hand it to the device, let
    /// the token go when the transfer is done. What used to fit in between --
    /// a `zx_vmo_op_range` decommit of the pinned range, which nothing
    /// refused -- gave the frame back to the allocator with the IOMMU still
    /// pointing at it, and then the `unpin` on the way out panicked the kernel
    /// looking for a page that was no longer there.
    fn a_decommit_cannot_pull_the_pages_out_from_under_a_token() {
        let (vmo, bti) = pinned_page();
        let pmt = bti
            .pin(vmo.clone(), 0, PAGE_SIZE, IommuPerms::PERM_READ)
            .unwrap();
        assert_eq!(
            Arc::strong_count(&pmt),
            2,
            "the initiator keeps its own reference to the token",
        );

        assert_eq!(vmo.decommit(0, PAGE_SIZE), Err(ZxError::BAD_STATE));
        assert_eq!(vmo.set_len(PAGE_SIZE), Err(ZxError::BAD_STATE));
        assert_eq!(
            vmo.committed_pages_in_range(0, 1),
            1,
            "the page the device was given is not the page it has",
        );

        // Letting the token go puts the pages back in play, and closing the
        // handle is the quiet half of that.
        pmt.unpin();
        assert_eq!(Arc::strong_count(&pmt), 1, "the initiator let it go");
        drop(pmt);
        vmo.decommit(0, PAGE_SIZE).unwrap();
        assert_eq!(vmo.committed_pages_in_range(0, 1), 0);
        vmo.set_len(PAGE_SIZE).unwrap();
    }

    #[test]
    /// The pin covers the range it was asked for and no more: the page next to
    /// it is free to go while the token is alive, and the one it holds is the
    /// one it was given rather than the first page of the object.
    fn the_pin_ends_where_the_token_says_it_does() {
        let (vmo, bti) = pinned_page();
        let pmt = bti
            .pin(vmo.clone(), PAGE_SIZE, PAGE_SIZE, IommuPerms::PERM_READ)
            .unwrap();
        vmo.decommit(0, PAGE_SIZE).unwrap();
        assert_eq!(vmo.committed_pages_in_range(0, 2), 1);
        assert_eq!(vmo.decommit(PAGE_SIZE, PAGE_SIZE), Err(ZxError::BAD_STATE));

        pmt.unpin();
        drop(pmt);
        vmo.decommit(PAGE_SIZE, PAGE_SIZE).unwrap();
        assert_eq!(vmo.committed_pages_in_range(0, 2), 0);
    }

    #[test]
    /// A pin the IOMMU refuses is no pin at all: `zx_bti_pin` with no
    /// permission bits answers `INVALID_ARGS`, and the pages it had pinned on
    /// the way in are free again. They used to stay pinned with no token to
    /// let them go, so the buffer could never be decommitted or resized.
    fn a_pin_the_iommu_refuses_leaves_nothing_pinned() {
        let (vmo, bti) = pinned_page();
        assert_eq!(
            bti.pin(vmo.clone(), 0, PAGE_SIZE, IommuPerms::empty())
                .err(),
            Some(ZxError::INVALID_ARGS)
        );
        vmo.decommit(0, PAGE_SIZE).unwrap();
        vmo.set_len(PAGE_SIZE).unwrap();
    }

    #[test]
    /// A pin whose addresses will not fit what the caller has room for is no
    /// pin at all. The bug lived in the syscall layer: `zx_bti_pin` pinned
    /// first and counted after, so a caller that got `addrs_count` wrong
    /// answered `INVALID_ARGS` with the pages still pinned and the token in the
    /// initiator's list with no handle to it. The buffer could then never be
    /// decommitted or resized, and the only way out needed a right on the
    /// initiator that a caller holding `MAP` need not have.
    fn a_pin_the_caller_has_no_room_for_leaves_nothing_pinned() {
        let (vmo, bti) = pinned_page();
        for asked in [0usize, 1, 3, 17] {
            assert_eq!(
                bti.pin_and_encode(
                    vmo.clone(),
                    0,
                    2 * PAGE_SIZE,
                    IommuPerms::PERM_READ,
                    false,
                    false,
                    asked,
                )
                .err(),
                Some(ZxError::INVALID_ARGS),
                "room for {} addresses was accepted for a two-page pin",
                asked
            );
            assert_eq!(
                vmo.decommit(0, 2 * PAGE_SIZE),
                Ok(()),
                "the pages of a refused pin are still pinned"
            );
            vmo.commit(0, 2 * PAGE_SIZE).unwrap();
        }
        // The count the pin really needs is served, and then it holds.
        let (pmt, addrs) = bti
            .pin_and_encode(
                vmo.clone(),
                0,
                2 * PAGE_SIZE,
                IommuPerms::PERM_READ,
                false,
                false,
                2,
            )
            .unwrap();
        assert_eq!(addrs.len(), 2);
        assert_eq!(vmo.decommit(0, PAGE_SIZE), Err(ZxError::BAD_STATE));
        pmt.unpin();
        drop(pmt);
        vmo.decommit(0, 2 * PAGE_SIZE).unwrap();
    }

    #[test]
    /// An encoding the options make impossible undoes its pin too: asking for
    /// one contiguous address from a paged object is the caller's error, and
    /// used to be the caller's error with the pages pinned.
    fn an_encoding_the_options_forbid_leaves_nothing_pinned() {
        let (vmo, bti) = pinned_page();
        assert_eq!(
            bti.pin_and_encode(
                vmo.clone(),
                0,
                2 * PAGE_SIZE,
                IommuPerms::PERM_READ,
                false,
                true,
                1,
            )
            .err(),
            Some(ZxError::INVALID_ARGS)
        );
        assert_eq!(
            vmo.decommit(0, 2 * PAGE_SIZE),
            Ok(()),
            "the pages of a refused pin are still pinned"
        );
        vmo.set_len(PAGE_SIZE).unwrap();
    }

    #[test]
    /// Two tokens over the same page hold it until both are gone.
    fn a_page_stays_pinned_until_the_last_token_lets_go() {
        let (vmo, bti) = pinned_page();
        let first = bti
            .pin(vmo.clone(), 0, PAGE_SIZE, IommuPerms::PERM_READ)
            .unwrap();
        let second = bti
            .pin(vmo.clone(), 0, PAGE_SIZE, IommuPerms::PERM_WRITE)
            .unwrap();
        first.unpin();
        drop(first);
        assert_eq!(vmo.decommit(0, PAGE_SIZE), Err(ZxError::BAD_STATE));
        second.unpin();
        drop(second);
        vmo.decommit(0, PAGE_SIZE).unwrap();
    }

    /// A scattered buffer of `n` pages and an initiator to pin it with.
    fn pinned_pages(n: usize) -> (Arc<VmObject>, Arc<BusTransactionInitiator>) {
        let vmo = VmObject::new_paged_with_resizable(true, n);
        vmo.commit(0, n * PAGE_SIZE).unwrap();
        let bti = BusTransactionInitiator::create(Iommu::create(), 0);
        (vmo, bti)
    }

    /// Every address of the pin, in order, as it comes out of
    /// `pin_and_encode`. A token released on the way out so the pages go back
    /// in play.
    fn addrs_of(
        bti: &Arc<BusTransactionInitiator>,
        vmo: &Arc<VmObject>,
        offset: usize,
        size: usize,
        compress: bool,
        contiguous: bool,
        asked: usize,
    ) -> Vec<DevVAddr> {
        let (pmt, addrs) = bti
            .pin_and_encode(
                vmo.clone(),
                offset,
                size,
                IommuPerms::PERM_READ,
                compress,
                contiguous,
                asked,
            )
            .unwrap();
        pmt.unpin();
        addrs
    }

    #[test]
    /// What the device is handed for a scattered buffer is the frame of each
    /// pinned page, in the order the pages come. The walk that collects them
    /// asks the IOMMU for one page at a time and has to move along as it
    /// goes: reading the offset from the request instead of from the cursor
    /// hands over the first page's frame again and again, and the device then
    /// does the whole transfer into one page.
    fn the_addresses_handed_over_are_the_pinned_pages_in_order() {
        let (vmo, bti) = pinned_pages(3);
        let addrs = addrs_of(&bti, &vmo, 0, 3 * PAGE_SIZE, false, false, 3);
        assert_eq!(
            addrs,
            vec![
                vmo.committed_paddr(0).unwrap(),
                vmo.committed_paddr(1).unwrap(),
                vmo.committed_paddr(2).unwrap(),
            ],
        );
        vmo.decommit(0, 3 * PAGE_SIZE).unwrap();
    }

    #[test]
    /// A pin that does not start at the buffer's first page hands over the
    /// page it pinned, not the first one. The pin itself and the addresses
    /// come from two different walks of the same range, and only this says
    /// they agree about where the range starts.
    fn a_pin_at_an_offset_hands_over_the_page_it_pinned() {
        let (vmo, bti) = pinned_pages(3);
        let addrs = addrs_of(&bti, &vmo, PAGE_SIZE, 2 * PAGE_SIZE, false, false, 2);
        assert_eq!(
            addrs,
            vec![
                vmo.committed_paddr(1).unwrap(),
                vmo.committed_paddr(2).unwrap(),
            ],
        );
        vmo.decommit(0, 3 * PAGE_SIZE).unwrap();
    }

    #[test]
    /// A contiguous buffer is one range to the IOMMU: one call, one base
    /// address, and the device addresses are that base walked a page at a
    /// time. The stride is the whole pinned length for a contiguous buffer
    /// and one page for a scattered one, and reading it from the wrong side
    /// of that choice gives a contiguous pin exactly one address -- which
    /// then does not match the count the caller has room for, so the pin is
    /// refused rather than wrong.
    fn a_contiguous_buffer_is_mapped_once_and_walked_from_its_base() {
        let vmo = VmObject::new_contiguous(4, PAGE_SIZE_LOG2).unwrap();
        let bti = BusTransactionInitiator::create(Iommu::create(), 0);
        let walked = addrs_of(&bti, &vmo, PAGE_SIZE, 2 * PAGE_SIZE, false, false, 2);
        // Read after the pin: the pin is what commits the range.
        let base = vmo.committed_paddr(1).unwrap();
        assert_eq!(walked, vec![base, base + PAGE_SIZE]);
        assert_eq!(Some(base + PAGE_SIZE), vmo.committed_paddr(2));
        // And `ZX_BTI_CONTIGUOUS` asks for that base and nothing else.
        assert_eq!(
            addrs_of(&bti, &vmo, PAGE_SIZE, 2 * PAGE_SIZE, false, true, 1),
            vec![base],
        );
    }

    #[test]
    /// `ZX_BTI_COMPRESS` asks for one address per run of guaranteed
    /// contiguity. This IOMMU guarantees a page, so the compressed form is
    /// the same list as the plain one -- and that is the point: a buffer the
    /// device sees as one range still has to be described page by page, or
    /// the caller is handed fewer addresses than it has room for.
    fn a_compressed_encoding_is_one_address_per_run_of_contiguity() {
        let contiguous = VmObject::new_contiguous(4, PAGE_SIZE_LOG2).unwrap();
        let bti = BusTransactionInitiator::create(Iommu::create(), 0);
        let compressed = addrs_of(&bti, &contiguous, 0, 3 * PAGE_SIZE, true, false, 3);
        let base = contiguous.committed_paddr(0).unwrap();
        assert_eq!(
            compressed,
            vec![base, base + PAGE_SIZE, base + 2 * PAGE_SIZE],
        );

        // A scattered buffer is already one address per page, so compressing
        // it hands the list over as it stands.
        let (scattered, bti) = pinned_pages(3);
        let plain = addrs_of(&bti, &scattered, 0, 3 * PAGE_SIZE, false, false, 3);
        assert_eq!(
            addrs_of(&bti, &scattered, 0, 3 * PAGE_SIZE, true, false, 3),
            plain,
        );
        scattered.decommit(0, 3 * PAGE_SIZE).unwrap();
    }

    #[test]
    /// The pin commits the range it is about to pin, and that is the range it
    /// was asked for. `zx_bti_pin` works on a buffer nobody has touched yet --
    /// the pages are demand-allocated by the commit on the way in -- so
    /// committing the wrong range leaves `vmo.pin` looking for a frame that is
    /// not there. Every other test here commits the whole buffer first, which
    /// hides it, and it also hides the commit spilling outside the pin.
    fn a_pin_commits_the_range_it_pins_even_when_nothing_touched_it() {
        let vmo = VmObject::new_paged_with_resizable(true, 3);
        let bti = BusTransactionInitiator::create(Iommu::create(), 0);
        assert_eq!(
            vmo.committed_pages_in_range(0, 3),
            0,
            "nothing is committed yet",
        );

        let (pmt, addrs) = bti
            .pin_and_encode(
                vmo.clone(),
                PAGE_SIZE,
                2 * PAGE_SIZE,
                IommuPerms::PERM_READ,
                false,
                false,
                2,
            )
            .unwrap();
        assert_eq!(
            addrs,
            vec![
                vmo.committed_paddr(1).unwrap(),
                vmo.committed_paddr(2).unwrap(),
            ],
        );
        assert_eq!(
            vmo.committed_pages_in_range(0, 1),
            0,
            "the page outside the pin was left alone",
        );

        pmt.unpin();
        drop(pmt);
        vmo.decommit(PAGE_SIZE, 2 * PAGE_SIZE).unwrap();
    }

    #[test]
    /// A pin has to start and end on a page. `map_into_iommu` does not check
    /// that, it *asserts* it, and an assert in a kernel object reached from a
    /// syscall is a kernel panic. `zx_bti_pin` checks the alignment itself,
    /// two crates away, and `vmo.pin` answers `BAD_STATE` for an unaligned
    /// range whose last page is not committed -- but a buffer that is already
    /// committed, which is every buffer a driver pins twice, walks past both
    /// and into the assert.
    fn a_pin_that_does_not_start_and_end_on_a_page_is_invalid_args() {
        let (vmo, bti) = pinned_page();
        for (offset, size) in [
            (0, PAGE_SIZE + 1),
            (0, PAGE_SIZE - 1),
            (1, PAGE_SIZE),
            (PAGE_SIZE - 1, PAGE_SIZE),
        ] {
            assert_eq!(
                bti.pin(vmo.clone(), offset, size, IommuPerms::PERM_READ)
                    .err(),
                Some(ZxError::INVALID_ARGS),
                "pin(offset={:#x}, size={:#x})",
                offset,
                size,
            );
        }
        // And nothing of a refused pin is left behind.
        vmo.decommit(0, 2 * PAGE_SIZE).unwrap();
        vmo.set_len(PAGE_SIZE).unwrap();
    }

    #[test]
    /// A page can be pinned thirty-one times and no more, and the pin that
    /// does not fit is refused outright rather than half-taken. The count
    /// lives in the frame, so what saturates it is tokens over the same page,
    /// and the error `vmo.pin` answers is the only thing standing between a
    /// saturated count and a token that believes it holds a pin it does not.
    fn the_pin_that_does_not_fit_is_refused_and_leaves_the_count_alone() {
        let (vmo, bti) = pinned_page();
        let mut held = Vec::new();
        for n in 0..31 {
            held.push(
                bti.pin(vmo.clone(), 0, PAGE_SIZE, IommuPerms::PERM_READ)
                    .unwrap_or_else(|err| panic!("pin number {} answered {:?}", n + 1, err)),
            );
        }
        assert_eq!(
            bti.pin(vmo.clone(), 0, PAGE_SIZE, IommuPerms::PERM_READ)
                .err(),
            Some(ZxError::UNAVAILABLE),
        );

        // And the page comes back once the thirty-one that did fit let go,
        // which it would not if the refused one had left a count behind.
        for pmt in held.drain(..) {
            pmt.unpin();
        }
        vmo.decommit(0, PAGE_SIZE).unwrap();
    }
}
