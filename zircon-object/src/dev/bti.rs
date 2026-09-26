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
    pub fn get_info(&self) -> BtiInfo {
        BtiInfo {
            minimum_contiguity: self.iommu.minimum_contiguity() as u64,
            aspace_size: self.iommu.aspace_size() as u64,
            pmo_count: self.pmo_count() as u64,
            quarantine_count: self.quarantine_count() as u64,
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

    fn pmo_count(&self) -> usize {
        self.inner.lock().pmts.len()
    }

    fn quarantine_count(&self) -> usize {
        self.inner
            .lock()
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
