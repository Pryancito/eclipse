//! Virtual memory operations.

use alloc::collections::BTreeMap;

use super::mem::{MOCK_PHYS_MEM, PMEM_MAP_VADDR, PMEM_SIZE};
use crate::sync::Mutex;
use crate::{addr::is_aligned, MMUFlags, PhysAddr, VirtAddr, PAGE_SIZE};

lazy_static! {
    /// Every guest page this mock page table has mapped: `vaddr -> (paddr, flags)`.
    ///
    /// The real page tables can be asked what a PTE points at, and
    /// [`VmMapping::protect`] depends on that answer: a page whose frame the
    /// VMO does not OWN -- the one global `ZERO_FRAME` behind demand-zero
    /// memory, or a page-cache frame borrowed by a `MAP_PRIVATE` file mapping
    /// -- has to be unmapped instead of being made writable in place, so the
    /// next store re-faults and copies.
    ///
    /// `query` here used to answer `NotMapped` for every user address, so that
    /// check passed for all of them and an `mprotect(PROT_READ|PROT_WRITE)`
    /// over demand-zero memory turned every page of the range into a WRITABLE
    /// ALIAS OF THE SAME ZERO FRAME. musl's `pthread_create` does exactly that
    /// -- `mmap(PROT_NONE)` for stack + guard + TLS, then `mprotect` the usable
    /// part -- so a new thread's whole 168 KiB allocation was one 4 KiB page
    /// repeated forty-two times: its TLS image, its dtv, its pthread struct and
    /// its stack all sat on top of each other. `functional/tls_local_exec.exe`
    /// read zeros where the main thread read 11, 22 and 33, and every later
    /// demand-zero page in the process came back holding that debris.
    ///
    /// One global table is enough because libos processes all live in the one
    /// host address space, so no two of them can hold the same virtual address.
    static ref MAPPED_PAGES: Mutex<BTreeMap<VirtAddr, (PhysAddr, MMUFlags)>> =
        Mutex::new(BTreeMap::new());
}

hal_fn_impl! {
    impl mod crate::hal_fn::vm {
        fn current_vmtoken() -> PhysAddr { 0 }
        fn activate_paging(_vmtoken: PhysAddr) {}
        fn pin_kernel_vmtoken() {}
        fn activate_kernel_paging() {}
    }
}

/// Dummy page table implemented by `mmap`, `munmap`, and `mprotect`.
pub struct PageTable;

impl PageTable {
    pub fn new() -> Self {
        Self
    }

    pub fn from_current() -> Self {
        Self
    }

    pub fn clone_kernel(&self) -> Self {
        Self::new()
    }
}

impl Default for PageTable {
    fn default() -> Self {
        Self::new()
    }
}

impl GenericPageTable for PageTable {
    fn table_phys(&self) -> PhysAddr {
        0
    }

    fn map(&mut self, page: Page, paddr: PhysAddr, flags: MMUFlags) -> PagingResult {
        debug_assert!(page.size as usize == PAGE_SIZE);
        debug_assert!(is_aligned(paddr));
        if paddr < PMEM_SIZE {
            MOCK_PHYS_MEM.mmap(page.vaddr, PAGE_SIZE, paddr, flags);
            MAPPED_PAGES.lock().insert(page.vaddr, (paddr, flags));
            Ok(())
        } else {
            Err(PagingError::NoMemory)
        }
    }

    fn unmap(&mut self, vaddr: VirtAddr) -> PagingResult<(PhysAddr, PageSize)> {
        self.unmap_cont(vaddr, PAGE_SIZE)?;
        Ok((0, crate::vm::BASE_PAGE_SIZE))
    }

    fn update(
        &mut self,
        vaddr: VirtAddr,
        _paddr: Option<PhysAddr>,
        flags: Option<MMUFlags>,
    ) -> PagingResult<PageSize> {
        debug_assert!(is_aligned(vaddr));
        if let Some(flags) = flags {
            // A page that was reserved but never faulted in has no host
            // mapping: report it as a not-present PTE, exactly like the bare
            // metal page table does, so `VmMapping::protect`'s `.ignore()`
            // skips it instead of the mock aborting the whole process.
            if !MOCK_PHYS_MEM.mprotect(vaddr as _, PAGE_SIZE, flags) {
                return Err(PagingError::NotMapped);
            }
            if let Some(entry) = MAPPED_PAGES.lock().get_mut(&vaddr) {
                entry.1 = flags;
            }
        }
        Ok(crate::vm::BASE_PAGE_SIZE)
    }

    fn query(&self, vaddr: VirtAddr) -> PagingResult<(PhysAddr, MMUFlags, PageSize)> {
        debug_assert!(is_aligned(vaddr));
        if (PMEM_MAP_VADDR..PMEM_MAP_VADDR + PMEM_SIZE).contains(&vaddr) {
            return Ok((
                vaddr - PMEM_MAP_VADDR,
                MMUFlags::READ | MMUFlags::WRITE,
                crate::vm::BASE_PAGE_SIZE,
            ));
        }
        MAPPED_PAGES
            .lock()
            .get(&vaddr)
            .map(|&(paddr, flags)| (paddr, flags, crate::vm::BASE_PAGE_SIZE))
            .ok_or(PagingError::NotMapped)
    }

    fn unmap_cont(&mut self, vaddr: VirtAddr, size: usize) -> PagingResult {
        if size == 0 {
            return Ok(());
        }
        debug_assert!(is_aligned(vaddr));
        MOCK_PHYS_MEM.munmap(vaddr as _, size);
        let mut mapped = MAPPED_PAGES.lock();
        for page in (vaddr..vaddr + size).step_by(PAGE_SIZE) {
            mapped.remove(&page);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid virtual address base to mmap.
    const VBASE: VirtAddr = 0x0002_0000_0000;

    #[test]
    fn map_unmap() {
        let mut pt = PageTable::new();
        let flags = MMUFlags::READ | MMUFlags::WRITE;
        // map 2 pages to 1 frame
        pt.map(
            Page::new_aligned(VBASE, crate::vm::BASE_PAGE_SIZE),
            PAGE_SIZE,
            flags,
        )
        .unwrap();
        pt.map(
            Page::new_aligned(VBASE + PAGE_SIZE, crate::vm::BASE_PAGE_SIZE),
            PAGE_SIZE,
            flags,
        )
        .unwrap();

        unsafe {
            const MAGIC: usize = 0xdead_beaf;
            (VBASE as *mut usize).write(MAGIC);
            assert_eq!(((VBASE + PAGE_SIZE) as *mut usize).read(), MAGIC);
        }

        pt.unmap(VBASE + PAGE_SIZE).unwrap();
    }

    /// A valid virtual address base to mmap, distinct from `VBASE` because the
    /// mock page table is global to the test binary.
    const VBASE_QUERY: VirtAddr = 0x0002_0100_0000;

    /// `query` has to name the frame a page points at.
    ///
    /// `VmMapping::protect` asks exactly this before raising WRITE on a page:
    /// when the frame in the PTE is not the one the VMO owns at that index --
    /// the shared zero frame behind demand-zero memory, a page-cache frame
    /// borrowed by a MAP_PRIVATE file mapping -- the page must be dropped so
    /// the next store re-faults and copies, not updated in place. This mock
    /// used to answer `NotMapped` for every user address, so that check never
    /// fired: one `mprotect(PROT_READ|PROT_WRITE)` over a demand-zero range
    /// left every page of it a writable alias of the same frame.
    #[test]
    fn query_names_the_frame_and_flags_behind_a_page() {
        let mut pt = PageTable::new();
        let paddr = 4 * PAGE_SIZE;
        pt.map(
            Page::new_aligned(VBASE_QUERY, crate::vm::BASE_PAGE_SIZE),
            paddr,
            MMUFlags::READ,
        )
        .unwrap();

        let (found, flags, _) = pt.query(VBASE_QUERY).unwrap();
        assert_eq!(found, paddr);
        assert_eq!(flags, MMUFlags::READ);

        pt.update(VBASE_QUERY, None, Some(MMUFlags::READ | MMUFlags::WRITE))
            .unwrap();
        assert_eq!(
            pt.query(VBASE_QUERY).unwrap().1,
            MMUFlags::READ | MMUFlags::WRITE
        );

        // A page that was never faulted in has no frame to name, and neither
        // has one that has been unmapped again.
        assert!(matches!(
            pt.query(VBASE_QUERY + PAGE_SIZE),
            Err(PagingError::NotMapped)
        ));
        pt.unmap(VBASE_QUERY).unwrap();
        assert!(matches!(pt.query(VBASE_QUERY), Err(PagingError::NotMapped)));
    }
}
