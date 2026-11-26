use {
    crate::object::*,
    crate::vm::*,
    alloc::{collections::BTreeMap, sync::Arc, vec::Vec},
    bitflags::bitflags,
    kernel_hal::{iommu::DmaDomain, sync::Mutex},
};

/// What a device behind a bus transaction initiator may reach.
///
/// Without a domain this is Zircon's dummy IOMMU: a device address is the
/// physical one and nothing is enforced, which is all a machine with no
/// IOMMU (or a boot with `iommu=off`) can do. With one, every range this
/// maps is written into the device's [`DmaDomain`] first and taken out of it
/// when the pin goes ([`Iommu::unmap`]), and nothing else is in there. The
/// addresses are the same either way: a domain is a sparse identity map.
pub struct Iommu {
    base: KObjectBase,
    domain: Option<Arc<dyn DmaDomain>>,
    /// Each page in the domain: how many grants name it, and the accesses
    /// they add up to. The map is an identity, so two pins of one page are
    /// one entry in the hardware, and the first unpin must not take the
    /// page from under the second.
    granted: Mutex<BTreeMap<PhysAddr, (usize, MMUFlags)>>,
}

/// Consecutive pages that want the same thing done, as one call.
fn runs(pages: impl Iterator<Item = (PhysAddr, MMUFlags)>) -> Vec<(PhysAddr, usize, MMUFlags)> {
    let mut runs: Vec<(PhysAddr, usize, MMUFlags)> = Vec::new();
    for (page, flags) in pages {
        match runs.last_mut() {
            Some((start, len, f)) if *start + *len == page && *f == flags => *len += PAGE_SIZE,
            _ => runs.push((page, PAGE_SIZE, flags)),
        }
    }
    runs
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
    /// Create a new `IOMMU` that enforces nothing: the device stays in
    /// passthrough.
    pub fn create() -> Arc<Self> {
        Arc::new(Iommu {
            base: KObjectBase::new(),
            domain: None,
            granted: Mutex::new(BTreeMap::new()),
        })
    }

    /// An `IOMMU` that grants through `domain`: the device reaches what its
    /// pins map and nothing else.
    pub fn with_domain(domain: Arc<dyn DmaDomain>) -> Arc<Self> {
        Arc::new(Iommu {
            base: KObjectBase::new(),
            domain: Some(domain),
            granted: Mutex::new(BTreeMap::new()),
        })
    }

    /// Whether the hardware enforces what this grants.
    pub fn is_translating(&self) -> bool {
        self.domain.is_some()
    }

    /// Put `len` bytes at device address `addr` in the domain, answering
    /// the address and length the caller asked to hand on.
    fn grant(&self, addr: DevVAddr, len: usize, perms: IommuPerms) -> ZxResult<(DevVAddr, usize)> {
        let Some(domain) = &self.domain else {
            return Ok((addr, len));
        };
        let flags = commit_flags(perms);
        let mut granted = self.granted.lock();
        // What the hardware has to be told: pages new to the domain, and
        // pages already there that this grant lets the device do more to.
        let wanted =
            (addr..addr + len)
                .step_by(PAGE_SIZE)
                .filter_map(|page| match granted.get(&page) {
                    Some(&(_, had)) if had.contains(flags) => None,
                    Some(&(_, had)) => Some((page, had | flags)),
                    None => Some((page, flags)),
                });
        let wanted = runs(wanted);
        for (i, &(start, run, run_flags)) in wanted.iter().enumerate() {
            if domain.map(start, run, run_flags).is_err() {
                // Take back what this grant put in, and only that: a page
                // somebody else holds stays, with the accesses it had.
                for &(start, run, _) in &wanted[..i] {
                    for page in (start..start + run).step_by(PAGE_SIZE) {
                        if !granted.contains_key(&page) {
                            domain.unmap(page, PAGE_SIZE);
                        }
                    }
                }
                return Err(ZxError::NO_RESOURCES);
            }
        }
        for page in (addr..addr + len).step_by(PAGE_SIZE) {
            let entry = granted.entry(page).or_insert((0, MMUFlags::empty()));
            entry.0 += 1;
            entry.1 |= flags;
        }
        Ok((addr, len))
    }

    /// Take back what [`map`](Self::map) or
    /// [`map_contiguous`](Self::map_contiguous) granted: `len` bytes at
    /// `addr`, exactly as answered. When this returns the device can no
    /// longer reach them, so the pages may be unpinned.
    pub(super) fn unmap(&self, addr: DevVAddr, len: usize) {
        let Some(domain) = &self.domain else {
            return;
        };
        let mut granted = self.granted.lock();
        let mut gone = Vec::new();
        for page in (addr..addr + len).step_by(PAGE_SIZE) {
            match granted.get_mut(&page) {
                Some(entry) if entry.0 > 1 => entry.0 -= 1,
                Some(_) => {
                    granted.remove(&page);
                    gone.push((page, MMUFlags::empty()));
                }
                None => warn!("iommu: unmap of {page:#x}, which no grant names"),
            }
        }
        for (start, run, _) in runs(gone.into_iter()) {
            domain.unmap(start, run);
        }
    }

    /// How many pages are in the domain now (0 without one).
    pub fn granted_pages(&self) -> usize {
        self.granted.lock().len()
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
            self.grant(p_addr, PAGE_SIZE, perms)
        } else {
            self.grant(p_addr, pages(size) * PAGE_SIZE, perms)
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
        self.grant(p_addr, pages(size) * PAGE_SIZE, perms)
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
        // Read before the child exists. `create_child` moves the frames into a
        // hidden parent node and `committed_paddr` then answers `None` on both
        // sides, so an `assert_ne!` against it afterwards compares with `None`
        // and holds whatever address the device was given.
        let forked_from = parent
            .committed_paddr(0)
            .expect("the parent's page is committed");
        let child = parent.create_child(false, 0, PAGE_SIZE).unwrap();
        let iommu = Iommu::create();
        let (addr, _) = iommu
            .map_contiguous(child.clone(), 0, PAGE_SIZE, IommuPerms::PERM_WRITE)
            .unwrap();
        assert_ne!(
            addr, forked_from,
            "the device must not be pointed at the frame the child forked from"
        );
        assert_eq!(Some(addr), child.committed_paddr(0));
    }

    /// Each permission the device asked for reaches the commit as its own
    /// fault flag and no other. `commit_page` is the only thing that breaks a
    /// copy-on-write page and it breaks it on a WRITE fault alone, so a write
    /// permission arriving as a read one is the difference between the device
    /// writing a frame of its own and writing the frame its parent still
    /// shares.
    #[test]
    fn each_permission_asks_the_commit_for_its_own_fault_flag() {
        assert_eq!(commit_flags(IommuPerms::empty()), MMUFlags::empty());
        assert_eq!(commit_flags(IommuPerms::PERM_READ), MMUFlags::READ);
        assert_eq!(commit_flags(IommuPerms::PERM_WRITE), MMUFlags::WRITE);
        assert_eq!(commit_flags(IommuPerms::PERM_EXECUTE), MMUFlags::EXECUTE);
        assert_eq!(
            commit_flags(IommuPerms::PERM_READ | IommuPerms::PERM_EXECUTE),
            MMUFlags::READ | MMUFlags::EXECUTE,
        );
        assert_eq!(
            commit_flags(IommuPerms::all()),
            MMUFlags::READ | MMUFlags::WRITE | MMUFlags::EXECUTE,
        );
    }

    /// The permission bits are the ones `ZX_BTI_PERM_*` names. Nothing links
    /// them to the options word of `zx_bti_pin` but these three numbers:
    /// `BtiOptions::to_iommu_perms` copies the bits over one at a time, in
    /// another crate, by name on both sides.
    #[test]
    fn the_permission_bits_are_the_ones_the_abi_names() {
        assert_eq!(IommuPerms::PERM_READ.bits(), 1 << 0);
        assert_eq!(IommuPerms::PERM_WRITE.bits(), 1 << 1);
        assert_eq!(IommuPerms::PERM_EXECUTE.bits(), 1 << 2);
        assert_eq!(IommuPerms::all().bits(), 0b111);
    }

    /// What this dummy IOMMU says about itself: the unit `map_into_iommu`
    /// walks a scattered pin in, and the two numbers `ZX_INFO_BTI` reports.
    #[test]
    fn the_dummy_iommu_answers_one_page_of_contiguity_and_every_address() {
        let iommu = Iommu::create();
        assert!(iommu.is_valid_bus_txn_id());
        assert_eq!(iommu.minimum_contiguity(), PAGE_SIZE);
        assert_eq!(iommu.aspace_size(), usize::MAX);
    }

    /// A mapping with no permission bits at all is refused by both calls. A
    /// device address the device may neither read nor write is the caller's
    /// mistake, and `zx_bti_pin` lets an options word through with no
    /// permissions in it.
    #[test]
    fn a_map_with_no_permissions_is_invalid_args_from_either_call() {
        let vmo = VmObject::new_paged(1);
        let iommu = Iommu::create();
        assert_eq!(
            iommu
                .map(vmo.clone(), 0, PAGE_SIZE, IommuPerms::empty())
                .err(),
            Some(ZxError::INVALID_ARGS),
        );
        assert_eq!(
            iommu
                .map_contiguous(vmo, 0, PAGE_SIZE, IommuPerms::empty())
                .err(),
            Some(ZxError::INVALID_ARGS),
        );
    }

    /// A window that ends on the object's last byte is inside it.
    /// `check_window` is the only bound either call has, and the comparison
    /// that decides "inside" is the one an off-by-one turns into "the last
    /// page of every buffer cannot be pinned".
    #[test]
    fn a_window_that_ends_on_the_last_byte_is_inside_the_object() {
        let vmo = VmObject::new_paged(4);
        let iommu = Iommu::create();
        assert!(iommu
            .map(vmo.clone(), 3 * PAGE_SIZE, PAGE_SIZE, IommuPerms::PERM_READ)
            .is_ok());
        assert!(iommu
            .map_contiguous(vmo.clone(), 0, 4 * PAGE_SIZE, IommuPerms::PERM_READ)
            .is_ok());
        // And one byte further out is not.
        assert_eq!(
            iommu
                .map(vmo, 3 * PAGE_SIZE, PAGE_SIZE + 1, IommuPerms::PERM_READ)
                .err(),
            Some(ZxError::INVALID_ARGS),
        );
    }

    /// `map` answers the frame of the page the offset falls in, and a length
    /// in bytes rounded up to whole pages. The offset goes into
    /// `commit_page`, which takes a page *index*: handing it the byte offset
    /// answers for a page that far along in page units, which is the bug
    /// `map_contiguous` had.
    #[test]
    fn map_at_an_offset_answers_that_page_and_a_length_in_bytes() {
        let vmo = VmObject::new_paged(4);
        // Committed first, which is what the pin does before it maps anything:
        // a read fault on an untouched page answers a shared zero page that
        // the object does not own, and comparing that with `committed_paddr`
        // compares with `None`.
        vmo.commit(0, 4 * PAGE_SIZE).unwrap();
        let (addr, len) = Iommu::create()
            .map(vmo.clone(), 2 * PAGE_SIZE, PAGE_SIZE, IommuPerms::PERM_READ)
            .unwrap();
        assert_eq!(Some(addr), vmo.committed_paddr(2));
        assert_ne!(
            Some(addr),
            vmo.committed_paddr(0),
            "the page the offset falls in, not the object's first one",
        );
        assert_eq!(len, PAGE_SIZE, "a paged object goes one page per call");

        with_window(|window, paddr| {
            // A physical window is mapped in one go, so its length is the
            // whole request rounded up -- in bytes, not in pages.
            let (addr, len) = Iommu::create()
                .map(window, PAGE_SIZE, PAGE_SIZE + 1, IommuPerms::PERM_READ)
                .unwrap();
            assert_eq!(addr, paddr + PAGE_SIZE);
            assert_eq!(len, 2 * PAGE_SIZE);
        });
    }

    /// A write permission reaches the commit as a write fault and a read
    /// permission does not, and that is the whole difference between the two:
    /// a read fault on an untouched page answers a shared zero page and
    /// commits nothing, so the address the device is handed is not a frame the
    /// object owns -- and the next read fault answers a different one again. A
    /// write fault is what makes the page real. This is why the pin commits
    /// the range before it maps it, and why the permissions have to arrive
    /// here rather than being dropped on the way.
    #[test]
    fn a_writable_mapping_commits_the_page_and_a_read_only_one_does_not() {
        let vmo = VmObject::new_paged(2);
        let read_only = Iommu::create()
            .map(vmo.clone(), 0, PAGE_SIZE, IommuPerms::PERM_READ)
            .unwrap()
            .0;
        assert_eq!(
            vmo.committed_paddr(0),
            None,
            "a read fault left the page untouched",
        );

        let writable = Iommu::create()
            .map(vmo.clone(), 0, PAGE_SIZE, IommuPerms::PERM_WRITE)
            .unwrap()
            .0;
        assert_eq!(Some(writable), vmo.committed_paddr(0));
        assert_ne!(
            read_only, writable,
            "the read-only address was not the frame the object ended up with",
        );

        // `map_contiguous` commits through the same permissions -- it used to
        // pass none at all.
        let other = VmObject::new_paged(2);
        let writable = Iommu::create()
            .map_contiguous(other.clone(), PAGE_SIZE, PAGE_SIZE, IommuPerms::PERM_WRITE)
            .unwrap()
            .0;
        assert_eq!(Some(writable), other.committed_paddr(1));
        assert_eq!(other.committed_paddr(0), None, "and only that page");
    }
}
