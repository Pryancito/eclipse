use {
    super::*,
    crate::object::*,
    crate::vm::*,
    alloc::{sync::Arc, vec::Vec},
    dev::Iommu,
    kernel_hal::sync::Mutex,
    kernel_hal::DevVAddr,
};

/// Bus Transaction Initiator.
///
/// Bus Transaction Initiators (BTIs) represent the bus mastering/DMA capability
/// of a device, and can be used for granting a device access to memory.
pub struct BusTransactionInitiator {
    base: KObjectBase,
    iommu: Arc<Iommu>,
    #[allow(dead_code)]
    bti_id: u64,
    inner: Mutex<BtiInner>,
}

#[derive(Default)]
struct BtiInner {
    /// A BTI manages a list of quarantined PMTs.
    pmts: Vec<Arc<PinnedMemoryToken>>,
}

impl_kobject!(BusTransactionInitiator);

impl BusTransactionInitiator {
    /// Create a new bus transaction initiator.
    pub fn create(iommu: Arc<Iommu>, bti_id: u64) -> Arc<Self> {
        Arc::new(BusTransactionInitiator {
            base: KObjectBase::new(),
            iommu,
            bti_id,
            inner: Mutex::new(BtiInner::default()),
        })
    }

    /// Get information of BTI.
    ///
    /// Both counts come out of ONE lock. They used to be two calls that each
    /// took it, so another thread pinning or unpinning in between could leave a
    /// caller of `zx_object_get_info(ZX_INFO_BTI)` holding a quarantine count
    /// larger than the total it is a subset of.
    pub fn get_info(&self) -> BtiInfo {
        let inner = self.inner.lock();
        BtiInfo {
            minimum_contiguity: self.iommu.minimum_contiguity() as u64,
            aspace_size: self.iommu.aspace_size() as u64,
            pmo_count: inner.pmts.len() as u64,
            quarantine_count: Self::quarantined(&inner) as u64,
        }
    }

    /// Pin memory and grant access to it to the BTI.
    pub fn pin(
        self: &Arc<Self>,
        vmo: Arc<VmObject>,
        offset: usize,
        size: usize,
        perms: IommuPerms,
    ) -> ZxResult<Arc<PinnedMemoryToken>> {
        if size == 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        let pmt = PinnedMemoryToken::create(self, vmo, perms, offset, size)?;
        self.inner.lock().pmts.push(pmt.clone());
        Ok(pmt)
    }

    /// Pin a buffer and encode the device addresses a caller expecting
    /// `addrs_count` of them will be handed, or leave nothing pinned.
    ///
    /// `zx_bti_pin` used to pin first and encode after, in the syscall layer:
    /// an encoding the options made impossible, or a count the caller got
    /// wrong, answered an error with the pages **still pinned** and the token
    /// sitting in this initiator's list with no handle to it. From there the
    /// buffer could never be decommitted or resized again, and the only way
    /// out was `zx_bti_release_quarantine`, which needs a right on the
    /// initiator that a caller holding only `MAP` need not have. Doing both
    /// steps here means a failure undoes the pin.
    #[allow(clippy::too_many_arguments)]
    pub fn pin_and_encode(
        self: &Arc<Self>,
        vmo: Arc<VmObject>,
        offset: usize,
        size: usize,
        perms: IommuPerms,
        compress_results: bool,
        contiguous: bool,
        addrs_count: usize,
    ) -> ZxResult<(Arc<PinnedMemoryToken>, Vec<DevVAddr>)> {
        let pmt = self.pin(vmo, offset, size, perms)?;
        let encoded = match pmt.encode_addrs(compress_results, contiguous) {
            Ok(encoded) if encoded.len() == addrs_count => encoded,
            Ok(encoded) => {
                warn!(
                    "bti.pin: the caller has room for {} addresses and the pin needs {}",
                    addrs_count,
                    encoded.len(),
                );
                pmt.unpin();
                return Err(ZxError::INVALID_ARGS);
            }
            Err(err) => {
                pmt.unpin();
                return Err(err);
            }
        };
        Ok((pmt, encoded))
    }

    /// Releases all quarantined PMTs.
    pub fn release_quarantine(&self) {
        let mut inner = self.inner.lock();
        // remove no handle, the only Arc is from self.pmts
        inner.pmts.retain(|pmt| Arc::strong_count(pmt) > 1);
    }

    /// Release a PMT by KoID.
    pub(super) fn release_pmt(&self, id: KoID) {
        let mut inner = self.inner.lock();
        inner.pmts.retain(|pmt| pmt.id() != id);
    }

    pub(super) fn iommu(&self) -> Arc<Iommu> {
        self.iommu.clone()
    }

    /// The tokens this initiator holds that nothing else names any more: the
    /// pin outlived every handle to it, which is what quarantine means here.
    /// Takes the guard rather than the lock so a caller can read it together
    /// with the total.
    fn quarantined(inner: &BtiInner) -> usize {
        inner
            .pmts
            .iter()
            .filter(|pmt| Arc::strong_count(pmt) == 1)
            .count()
    }
}

/// Information of BTI.
#[repr(C)]
#[derive(Default)]
pub struct BtiInfo {
    minimum_contiguity: u64,
    aspace_size: u64,
    pmo_count: u64,
    quarantine_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::Iommu;
    use crate::vm::PAGE_SIZE;

    /// The two counts of `ZX_INFO_BTI` are a total and a subset of it, so they
    /// have to come out of one snapshot. They used to be two separate locks.
    #[test]
    fn the_quarantine_count_is_a_subset_of_the_pin_count() {
        let vmo = VmObject::new_paged_with_resizable(true, 2);
        vmo.commit(0, 2 * PAGE_SIZE).unwrap();
        let bti = BusTransactionInitiator::create(Iommu::create(), 0);

        let info = bti.get_info();
        assert_eq!((info.pmo_count, info.quarantine_count), (0, 0));

        let pmt = bti
            .pin(vmo.clone(), 0, PAGE_SIZE, IommuPerms::PERM_READ)
            .unwrap();
        let info = bti.get_info();
        assert_eq!(
            (info.pmo_count, info.quarantine_count),
            (1, 0),
            "a token somebody still holds is pinned, not quarantined",
        );

        // Dropping the caller's `Arc` without unpinning is what quarantine is:
        // the initiator's own reference is the last one left.
        drop(pmt);
        let info = bti.get_info();
        assert_eq!(
            (info.pmo_count, info.quarantine_count),
            (1, 1),
            "a token nothing else names is quarantined, and still pinned",
        );
        assert!(info.quarantine_count <= info.pmo_count);

        bti.release_quarantine();
        let info = bti.get_info();
        assert_eq!((info.pmo_count, info.quarantine_count), (0, 0));
    }
}
