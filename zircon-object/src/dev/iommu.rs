use {crate::object::*, crate::vm::*, alloc::sync::Arc, bitflags::bitflags};

/// Iommu refers to DummyIommu in zircon.
///
/// A dummy implementation, do not take it serious.
pub struct Iommu {
    base: KObjectBase,
}

impl_kobject!(Iommu);

/// The device's permissions as the page-commit flags they ask for. Both
/// `map` and `map_contiguous` commit through this: a write-fault on a
/// copy-on-write page has to happen before the device is told an address,
/// or the device writes the shared parent's frame.
fn commit_flags(perms: IommuPerms) -> MMUFlags {
    let mut flags = MMUFlags::empty();
    if perms.contains(IommuPerms::PERM_READ) {
        flags |= MMUFlags::READ;
    }
    if perms.contains(IommuPerms::PERM_WRITE) {
        flags |= MMUFlags::WRITE;
    }
    if perms.contains(IommuPerms::PERM_EXECUTE) {
        flags |= MMUFlags::EXECUTE;
    }
    flags
}

/// `[offset, offset + size)` as a range the VMO covers, or `INVALID_ARGS`:
/// an empty range, a window that wraps, or one that leaves the object.
fn check_window(vmo: &VmObject, offset: usize, size: usize) -> ZxResult {
    if size == 0 {
        return Err(ZxError::INVALID_ARGS);
    }
    match offset.checked_add(size) {
        Some(end) if end <= vmo.len() => Ok(()),
        _ => Err(ZxError::INVALID_ARGS),
    }
}

impl Iommu {
    /// Create a new `IOMMU`.
    pub fn create() -> Arc<Self> {
        Arc::new(Iommu {
            base: KObjectBase::new(),
        })
    }

    /// Check if a `bus_txn_id` is valid for this IOMMU.
    pub fn is_valid_bus_txn_id(&self) -> bool {
        true
    }

    /// Returns the number of bytes that Map() can guarantee, upon success, to find
    /// a contiguous address range for.
    pub fn minimum_contiguity(&self) -> usize {
        PAGE_SIZE
    }

    /// The number of bytes in the address space (UINT64_MAX if 2^64).
    pub fn aspace_size(&self) -> usize {
        usize::MAX
    }

    /// Grant a device access to the range of pages given by [offset, offset + size) in `vmo`.
    ///
    /// Answers the device address of the first page and **the mapped length
    /// in bytes**, which for a paged object is one page (the next call maps
    /// the next one) and for a physical window is the whole range.
    pub fn map(
        &self,
        vmo: Arc<VmObject>,
        offset: usize,
        size: usize,
        perms: IommuPerms,
    ) -> ZxResult<(DevVAddr, usize)> {
        if perms == IommuPerms::empty() {
            return Err(ZxError::INVALID_ARGS);
        }
        check_window(&vmo, offset, size)?;
        let p_addr = vmo.commit_page(offset / PAGE_SIZE, commit_flags(perms))?;
        if vmo.is_paged() {
            Ok((p_addr, PAGE_SIZE))
        } else {
            Ok((p_addr, pages(size) * PAGE_SIZE))
        }
    }

    /// Same as `map`, but with additional guarantee that this will never return a
    /// partial mapping.  It will either return a single contiguous mapping or
    /// return a failure.
    ///
    /// `commit_page` takes a page index; this used to hand it the byte
    /// offset, so a pin at any offset but zero committed page number
    /// `offset` -- out of range for every object smaller than `offset` pages,
    /// and the wrong page for a bigger one, whose address the device then
    /// used for DMA.
    pub fn map_contiguous(
        &self,
        vmo: Arc<VmObject>,
        offset: usize,
        size: usize,
        perms: IommuPerms,
    ) -> ZxResult<(DevVAddr, usize)> {
        if perms == IommuPerms::empty() {
            return Err(ZxError::INVALID_ARGS);
        }
        check_window(&vmo, offset, size)?;
        let p_addr = vmo.commit_page(offset / PAGE_SIZE, commit_flags(perms))?;
        Ok((p_addr, pages(size) * PAGE_SIZE))
    }
}

bitflags! {
    /// IOMMU permission flags.
    pub struct IommuPerms: u32 {
        #[allow(clippy::identity_op)]
        /// Read Permission.
        const PERM_READ             = 1 << 0;
        /// Write Permission.
        const PERM_WRITE            = 1 << 1;
        /// Execute Permission.
        const PERM_EXECUTE          = 1 << 2;
    }
}

#[cfg(test)]
mod tests {
    //! What the IOMMU tells a device about a buffer: the address of the page
    //! it asked for, and a length in the unit the caller (`PinnedMemoryToken`)
    //! walks in, bytes.
    use super::*;
    use kernel_hal::mem::PhysFrame;

    /// A four-page physical window, its frames held for the body of the test
    /// so the range is genuinely exclusive.
    fn with_window(f: impl FnOnce(Arc<VmObject>, PhysAddr)) {
        let frames = PhysFrame::new_contiguous(4, 0);
        assert_eq!(frames.len(), 4, "could not reserve four contiguous frames");
        let paddr = frames[0].paddr();
        f(VmObject::new_physical(paddr, 4), paddr);
    }

    /// A pin at page two of a contiguous object maps page two. The byte
    /// offset went into `commit_page` as a page index, so this was
    /// `OUT_OF_RANGE` for any object shorter than 8192 pages, and a page
    /// 8192 frames further on for a longer one.
    #[test]
    fn map_contiguous_at_an_offset_maps_that_page_not_page_number_offset() {
        with_window(|vmo, paddr| {
            let iommu = Iommu::create();
            let (addr, len) = iommu
                .map_contiguous(vmo, 2 * PAGE_SIZE, PAGE_SIZE, IommuPerms::PERM_READ)
                .unwrap();
            assert_eq!(addr, paddr + 2 * PAGE_SIZE);
            assert_eq!(len, PAGE_SIZE);
        });
    }

    /// The same on a paged contiguous object: the address is that of its
    /// third page, whatever frame the allocator gave it.
    #[test]
    fn map_contiguous_on_a_paged_contiguous_object_commits_the_page_asked_for() {
        let vmo = VmObject::new_contiguous(4, PAGE_SIZE_LOG2).unwrap();
        let iommu = Iommu::create();
        let (addr, len) = iommu
            .map_contiguous(
                vmo.clone(),
                2 * PAGE_SIZE,
                2 * PAGE_SIZE,
                IommuPerms::PERM_READ,
            )
            .unwrap();
        assert_eq!(Some(addr), vmo.committed_paddr(2));
        assert_eq!(
            len,
            2 * PAGE_SIZE,
            "a contiguous object maps as one range, whatever backs it"
        );
    }

    /// The length comes back in bytes, the unit `map_into_iommu` subtracts
    /// from what remains and asserts a multiple of `minimum_contiguity`. For
    /// a physical window `map` answered a page *count*: two, for two pages.
    #[test]
    fn the_mapped_length_is_in_bytes_for_both_calls() {
        with_window(|vmo, paddr| {
            let iommu = Iommu::create();
            let (addr, len) = iommu
                .map(vmo.clone(), 0, 2 * PAGE_SIZE, IommuPerms::PERM_READ)
                .unwrap();
            assert_eq!((addr, len), (paddr, 2 * PAGE_SIZE));
            let (addr, len) = iommu
                .map_contiguous(vmo, PAGE_SIZE, 2 * PAGE_SIZE + 1, IommuPerms::PERM_WRITE)
                .unwrap();
            assert_eq!(addr, paddr + PAGE_SIZE);
            assert_eq!(len, 3 * PAGE_SIZE, "rounded up to whole pages");
        });
        let paged = VmObject::new_paged(4);
        let (_, len) = Iommu::create()
            .map(paged, 0, 3 * PAGE_SIZE, IommuPerms::PERM_READ)
            .unwrap();
        assert_eq!(len, PAGE_SIZE, "a paged object goes one page per call");
    }

    /// A window that wraps `usize` is refused, not added: `offset + size`
    /// used to overflow, and in release wrapped to a small end that passed
    /// the bound.
    #[test]
    fn a_window_that_wraps_or_leaves_the_object_is_invalid_args() {
        let vmo = VmObject::new_paged(4);
        let iommu = Iommu::create();
        for (offset, size) in [
            (usize::MAX, 2),
            (usize::MAX - PAGE_SIZE + 1, 2 * PAGE_SIZE),
            (3 * PAGE_SIZE, 2 * PAGE_SIZE),
            (0, 0),
        ] {
            assert_eq!(
                iommu
                    .map(vmo.clone(), offset, size, IommuPerms::PERM_READ)
                    .err(),
                Some(ZxError::INVALID_ARGS),
                "map({offset:#x}, {size:#x})"
            );
            assert_eq!(
                iommu
                    .map_contiguous(vmo.clone(), offset, size, IommuPerms::PERM_READ)
                    .err(),
                Some(ZxError::INVALID_ARGS),
                "map_contiguous({offset:#x}, {size:#x})"
            );
        }
    }

    /// The device's write permission reaches the commit as a write fault, so
    /// a copy-on-write child gets its own frame before the device is told
    /// where to write: `map_contiguous` used to commit with no flags at all.
    #[test]
    fn a_writable_pin_of_a_cow_child_gets_the_child_its_own_frame() {
        let parent = VmObject::new_paged(1);
        parent.commit(0, PAGE_SIZE).unwrap();
        let child = parent.create_child(false, 0, PAGE_SIZE).unwrap();
        let iommu = Iommu::create();
        let (addr, _) = iommu
            .map_contiguous(child.clone(), 0, PAGE_SIZE, IommuPerms::PERM_WRITE)
            .unwrap();
        assert_ne!(
            Some(addr),
            parent.committed_paddr(0),
            "the device must not be pointed at the parent's frame"
        );
        assert_eq!(Some(addr), child.committed_paddr(0));
    }
}
