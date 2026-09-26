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
}
