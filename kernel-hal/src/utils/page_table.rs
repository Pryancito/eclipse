use crate::sync::Mutex;
use crate::{common::vm::*, mem::PhysFrame, MMUFlags, PhysAddr, VirtAddr};
use alloc::vec::Vec;
use core::{fmt::Debug, marker::PhantomData, slice};

pub trait PageTableLevel: Sync + Send {
    const LEVEL: usize;
}

pub struct PageTableLevel3;
pub struct PageTableLevel4;

impl PageTableLevel for PageTableLevel3 {
    const LEVEL: usize = 3;
}

impl PageTableLevel for PageTableLevel4 {
    const LEVEL: usize = 4;
}

pub trait GenericPTE: Debug + Clone + Copy + Sync + Send {
    /// Returns the physical address mapped by this entry.
    fn addr(&self) -> PhysAddr;
    /// Returns the flags of this entry.
    fn flags(&self) -> MMUFlags;
    /// Returns whether this entry is zero.
    fn is_unused(&self) -> bool;
    /// Returns whether this entry flag indicates present.
    fn is_present(&self) -> bool;
    /// Returns whether this entry maps to a huge frame (or it's a terminal entry).
    fn is_leaf(&self) -> bool;

    /// Set flags for all types of entries.
    fn set_flags(&mut self, flags: MMUFlags, is_huge: bool);
    /// Set physical address for terminal entries.
    fn set_addr(&mut self, paddr: PhysAddr);
    /// Set physical address and flags for intermediate table entries.
    fn set_table(&mut self, paddr: PhysAddr);
    /// Set this entry to zero.
    fn clear(&mut self);
}

pub struct PageTableImpl<L: PageTableLevel, PTE: GenericPTE> {
    /// Root table frame.
    root: PhysFrame,
    /// Intermediate level table frames.
    intrm_tables: Vec<PhysFrame>,
    /// Depth of the open mmu-gather window, and whether a shootdown is owed.
    ///
    /// A counter rather than a flag so nesting cannot have one level close a
    /// window another level still needs. See `GenericPageTable::set_gather`.
    gather: usize,
    gather_pending: bool,
    /// Phantom data.
    _phantom: PhantomData<(L, PTE)>,
}

/// Private implementation.
impl<L: PageTableLevel, PTE: GenericPTE> PageTableImpl<L, PTE> {
    unsafe fn from_root(root_paddr: PhysAddr) -> Self {
        Self {
            root: unsafe { PhysFrame::from_paddr(root_paddr) },
            intrm_tables: Vec::new(),
            gather: 0,
            gather_pending: false,
            _phantom: PhantomData,
        }
    }

    fn alloc_intrm_table(&mut self) -> Option<PhysAddr> {
        let frame = PhysFrame::new_zero()?;
        let paddr = frame.paddr();
        self.intrm_tables.push(frame);
        Some(paddr)
    }

    fn get_entry_mut(&self, vaddr: VirtAddr) -> PagingResult<(&mut PTE, PageSize)> {
        let p3 = if L::LEVEL == 3 {
            table_of_mut::<PTE>(self.table_phys())
        } else if L::LEVEL == 4 {
            let p4 = table_of_mut::<PTE>(self.table_phys());
            let p4e = &mut p4[p4_index(vaddr)];
            next_table_mut(p4e)?
        } else {
            unreachable!()
        };

        let p3e = &mut p3[p3_index(vaddr)];
        if p3e.is_leaf() {
            return Ok((p3e, PageSize::Size1G));
        }

        let p2 = next_table_mut(p3e)?;
        let p2e = &mut p2[p2_index(vaddr)];
        if p2e.is_leaf() {
            return Ok((p2e, PageSize::Size2M));
        }

        let p1 = next_table_mut(p2e)?;
        let p1e = &mut p1[p1_index(vaddr)];
        Ok((p1e, PageSize::Size4K))
    }

    /// Replace the huge leaf that describes `vaddr` with a table of
    /// next-smaller leaves covering exactly the same physical range with the
    /// same flags, repeating until `vaddr` is described by a 4 KiB entry.
    /// A no-op when it already is.
    ///
    /// This exists for [`crate::stack_guard`], which has to take a single page
    /// away from a region the kernel mapped with huge pages, and cannot
    /// allocate: it runs inside `Executor::new` with the scheduler's runtime
    /// lock held. So the frames for the new tables come from `alloc`, which
    /// must hand out zeroed frames that are **never freed** — a table reached
    /// from the page-table root outlives any `PageTableImpl` value, and this
    /// method deliberately does not push them onto `intrm_tables` (`self` is
    /// typically a borrowed `from_current()` view whose vector dies at the end
    /// of the statement, which would free a live page table).
    ///
    /// The caller flushes the TLB — every CPU's, for a kernel mapping — before
    /// relying on the finer entries.
    ///
    /// On a 3-level page table (Sv39) the 1 GiB leaves *are* top-level entries,
    /// and `pt_clone_kernel_space` copies those by value: splitting one after
    /// an address space has been cloned would be invisible to that clone. The
    /// architecture's kernel page table is responsible for not leaving a
    /// top-level leaf anywhere this is used — see the kernel heap window in
    /// `bare::arch::riscv::vm::init_kernel_page_table`.
    pub fn split_huge_page(
        &mut self,
        vaddr: VirtAddr,
        mut alloc: impl FnMut() -> Option<PhysAddr>,
    ) -> PagingResult {
        // 1 GiB -> 2 MiB -> 4 KiB: two steps at most, and each one strictly
        // shrinks the entry, so the bound is a fact rather than a guess.
        for _ in 0..2 {
            let (entry, size) = self.get_entry_mut(vaddr)?;
            let child = match size {
                PageSize::Size4K => return Ok(()),
                PageSize::Size2M => PageSize::Size4K,
                PageSize::Size1G => PageSize::Size2M,
            };
            if entry.is_unused() {
                return Err(PagingError::NotMapped);
            }
            let base = entry.addr();
            let flags = entry.flags();
            // Allocated before the parent is touched: running out of frames
            // must leave the mapping exactly as it was.
            let table_paddr = alloc().ok_or(PagingError::NoMemory)?;
            let table = table_of_mut::<PTE>(table_paddr);
            for (i, e) in table.iter_mut().enumerate() {
                // Built from zero rather than copied from the parent: on
                // x86_64 a leaf carries its level in the entry itself (the PS
                // bit, and the PAT bit that moves with it), so only
                // `set_flags` knows how to spell "same flags, one level down".
                e.clear();
                e.set_addr(base + i * child as usize);
                e.set_flags(flags, child != PageSize::Size4K);
            }
            // The 512 stores above have to reach memory before the pointer to
            // them does: the frame was zeroed when it was reserved, so another
            // CPU's page-table walker that saw the pointer early would read
            // "not present" for memory that is mapped.
            core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
            // The one store that publishes the split. Until it lands every CPU
            // walks the old leaf, which describes the same memory.
            entry.set_table(table_paddr);
        }
        Ok(())
    }

    fn get_entry_mut_or_create(&mut self, page: Page) -> PagingResult<&mut PTE> {
        let vaddr = page.vaddr;
        let p3 = if L::LEVEL == 3 {
            table_of_mut::<PTE>(self.table_phys())
        } else if L::LEVEL == 4 {
            let p4 = table_of_mut::<PTE>(self.table_phys());
            let p4e = &mut p4[p4_index(vaddr)];
            next_table_mut_or_create(p4e, || self.alloc_intrm_table())?
        } else {
            unreachable!()
        };

        let p3e = &mut p3[p3_index(vaddr)];
        if page.size == PageSize::Size1G {
            return Ok(p3e);
        }

        let p2 = next_table_mut_or_create(p3e, || self.alloc_intrm_table())?;
        let p2e = &mut p2[p2_index(vaddr)];
        if page.size == PageSize::Size2M {
            return Ok(p2e);
        }

        let p1 = next_table_mut_or_create(p2e, || self.alloc_intrm_table())?;
        let p1e = &mut p1[p1_index(vaddr)];
        Ok(p1e)
    }

    /// The level [`Self::walk`] labels this tree's ROOT with. It names levels
    /// from the top of a 4-level tree, because that is how it turns an index
    /// into an address (`i << (12 + (3 - level) * 9)`). A 3-level table's root
    /// is the P3, so it starts one level down.
    fn root_walk_level() -> usize {
        4 - L::LEVEL
    }

    fn walk(
        &self,
        table: &[PTE],
        level: usize,
        start_vaddr: usize,
        limit: usize,
        func: &impl Fn(usize, usize, usize, &PTE),
    ) {
        let mut n = 0;
        for (i, entry) in table.iter().enumerate() {
            let vaddr = start_vaddr + (i << (12 + (3 - level) * 9));
            if entry.is_present() {
                func(level, i, vaddr, entry);
                if level < 3 && !entry.is_leaf() {
                    let table_entry = next_table_mut(entry).unwrap();
                    self.walk(table_entry, level + 1, vaddr, limit, func);
                }
                n += 1;
                if n >= limit {
                    break;
                }
            }
        }
    }

    #[allow(unused)]
    fn dump(&self, limit: usize, print_fn: impl Fn(core::fmt::Arguments)) {
        static LOCK: Mutex<()> = Mutex::new(());
        let _lock = LOCK.lock();

        print_fn(format_args!("Root: {:x?}\n", self.table_phys()));
        self.walk(
            table_of(self.table_phys()),
            // From the root of a 3-level table this used to pass 0, and
            // printed every address 512 GiB apart instead of 1 GiB.
            Self::root_walk_level(),
            0,
            limit,
            &|level: usize, idx: usize, vaddr: usize, entry: &PTE| {
                for _ in 0..level {
                    print_fn(format_args!("  "));
                }
                print_fn(format_args!(
                    "[{} - {:x}], {:08x?}: {:x?}\n",
                    level, idx, vaddr, entry
                ));
            },
        );
    }

    #[cfg(not(target_arch = "x86_64"))]
    pub(crate) unsafe fn activate(&mut self) {
        crate::vm::activate_paging(self.table_phys());
    }
}

/// Public implementation.
impl<L: PageTableLevel, PTE: GenericPTE> PageTableImpl<L, PTE> {
    /// Address-space filter for this table's shootdowns: its own root, or
    /// `None` (target everyone) when this is the kernel's table — kernel
    /// entries can carry the global bit and survive CR3 switches, so no CPU
    /// may be skipped for them. Unknown kernel token (early boot, an arch
    /// that never publishes one) disables filtering entirely: over-targeting
    /// is a wasted IPI, under-targeting is a missed invalidation.
    fn aspace_filter(&self) -> Option<usize> {
        let root = self.table_phys();
        let kernel = crate::vm::kernel_vmtoken();
        if kernel == 0 || root & !0xfff == kernel & !0xfff {
            None
        } else {
            Some(root)
        }
    }

    pub fn new() -> Self {
        let root = PhysFrame::new_zero().expect("failed to alloc frame");
        Self {
            root,
            intrm_tables: Vec::new(),
            gather: 0,
            gather_pending: false,
            _phantom: PhantomData,
        }
    }

    /// Create a new `PageTable` from current VM token. (e.g. CR3, SATP, ...)
    pub fn from_current() -> Self {
        unsafe { Self::from_root(crate::vm::current_vmtoken()) }
    }

    pub fn clone_kernel(&self) -> Self {
        let pt = Self::new();
        crate::vm::pt_clone_kernel_space(pt.table_phys(), self.table_phys());
        pt
    }
}

impl<L: PageTableLevel, PTE: GenericPTE> Default for PageTableImpl<L, PTE> {
    fn default() -> Self {
        Self::new()
    }
}

impl<L: PageTableLevel, PTE: GenericPTE> GenericPageTable for PageTableImpl<L, PTE> {
    fn table_phys(&self) -> PhysAddr {
        self.root.paddr()
    }

    fn map(&mut self, page: Page, paddr: PhysAddr, flags: MMUFlags) -> PagingResult {
        let entry = self.get_entry_mut_or_create(page)?;
        if !entry.is_unused() {
            return Err(PagingError::AlreadyMapped);
        }
        entry.set_addr(page.size.align_down(paddr));
        entry.set_flags(flags, page.size.is_huge());
        crate::vm::flush_tlb(Some(page.vaddr));
        trace!(
            "PageTable map: {:x?} -> {:x?}, flags={:?} in {:#x?}",
            page,
            paddr,
            flags,
            self.table_phys()
        );
        Ok(())
    }

    fn unmap(&mut self, vaddr: VirtAddr) -> PagingResult<(PhysAddr, PageSize)> {
        let ret = self.unmap_no_shootdown(vaddr)?;
        // Removing a mapping leaves stale TLB entries on the other CPUs that
        // still point at the old frame; once it is freed and reused this
        // corrupts the new owner. Shoot down the entry on every other online
        // CPU. Range operations use `unmap_no_shootdown` in a loop plus one
        // `remote_flush_all` instead — a synchronous shootdown per page is
        // O(pages × ack-wait) and livelocks when a peer can't ack.
        crate::common::ipi::remote_flush_tlb_aspace(Some(vaddr), self.aspace_filter());
        Ok(ret)
    }

    fn unmap_no_shootdown(&mut self, vaddr: VirtAddr) -> PagingResult<(PhysAddr, PageSize)> {
        // Inside a gather window the paired `remote_flush_all` is swallowed, so
        // record here that one is owed. Recorded before the fallible lookup on
        // purpose: an entry that turns out to be absent owes nothing, but an
        // extra flush costs one IPI and a missed one costs silent corruption.
        self.gather_pending |= self.gather > 0;
        let (entry, size) = self.get_entry_mut(vaddr)?;
        if entry.is_unused() {
            return Err(PagingError::NotMapped);
        }
        let paddr = entry.addr();
        entry.clear();
        // Gathered range ops pay one local flush when the window closes
        // (`remote_flush_tlb_aspace`); per-page INVLPG here is O(pages).
        if self.gather == 0 {
            crate::vm::flush_tlb(Some(vaddr));
        }
        trace!("PageTable unmap: {:x?} in {:#x?}", vaddr, self.table_phys());
        Ok((paddr, size))
    }

    fn update(
        &mut self,
        vaddr: VirtAddr,
        paddr: Option<PhysAddr>,
        flags: Option<MMUFlags>,
    ) -> PagingResult<PageSize> {
        let size = self.update_no_shootdown(vaddr, paddr, flags)?;
        // Reducing permissions / repointing a live mapping (e.g. COW write
        // protect) must invalidate the stale entry on the other CPUs too.
        crate::common::ipi::remote_flush_tlb_aspace(Some(vaddr), self.aspace_filter());
        Ok(size)
    }

    fn update_no_shootdown(
        &mut self,
        vaddr: VirtAddr,
        paddr: Option<PhysAddr>,
        flags: Option<MMUFlags>,
    ) -> PagingResult<PageSize> {
        // See `unmap_no_shootdown`: inside a gather window this records the
        // shootdown the swallowed `remote_flush_all` would have performed.
        self.gather_pending |= self.gather > 0;
        let (entry, size) = self.get_entry_mut(vaddr)?;
        // Symmetric with `unmap_no_shootdown`/`query`: an update must never
        // resurrect a decommitted leaf. `get_entry_mut` returns a leaf as long
        // as the intermediate P4..P1 tables are present, even if the leaf
        // itself was cleared (e.g. by a prior `unmap`). Without this guard,
        // `update(va, None, Some(RXW))` on such a cleared leaf stamps
        // PRESENT|WRITABLE onto an entry whose addr() is 0, producing a phantom
        // PTE mapping the VA to physical frame 0 — a store then lands in phys 0
        // with no fault and no VMO commit (the demand-paged anon corruption that
        // aborted mimalloc/apk with "corrupted free list entry" / SIGSEGV).
        // Returning NotMapped lets the `.ignore()` at the mprotect/range_change
        // call sites be the true no-op they assume, so the page stays unmapped
        // and re-faults cleanly into `handle_page_fault`.
        if entry.is_unused() {
            return Err(PagingError::NotMapped);
        }
        if let Some(paddr) = paddr {
            entry.set_addr(paddr);
        }
        if let Some(flags) = flags {
            entry.set_flags(flags, size.is_huge());
        }
        if self.gather == 0 {
            crate::vm::flush_tlb(Some(vaddr));
        }
        trace!(
            "PageTable update: {:x?}, flags={:?} in {:#x?}",
            vaddr,
            flags,
            self.table_phys()
        );
        Ok(size)
    }

    fn remote_flush_all(&self) {
        if self.gather > 0 {
            // A gather window is open: record the debt and let the caller that
            // opened it pay once. `set_gather` is the only way back out, and it
            // reports what is owed.
            //
            // Interior mutability through `&self` would be needed to record it
            // here, so the flag is set by the `&mut self` paths instead — see
            // `note_gathered_flush`, called from `unmap_no_shootdown` and
            // `update_no_shootdown`, which are the only operations that can
            // leave a stale remote entry inside a window.
            return;
        }
        crate::common::ipi::remote_flush_tlb_aspace(None, self.aspace_filter());
    }

    fn set_gather(&mut self, on: bool) -> bool {
        if on {
            self.gather += 1;
            false
        } else {
            self.gather = self.gather.saturating_sub(1);
            if self.gather > 0 {
                return false; // an outer window is still open
            }
            core::mem::take(&mut self.gather_pending)
        }
    }

    fn query(&self, vaddr: VirtAddr) -> PagingResult<(PhysAddr, MMUFlags, PageSize)> {
        let (entry, size) = self.get_entry_mut(vaddr)?;
        if entry.is_unused() {
            return Err(PagingError::NotMapped);
        }
        let off = size.page_offset(vaddr);
        let ret = (entry.addr() + off, entry.flags(), size);
        trace!("PageTable query: {:x?} => {:x?}", vaddr, ret);
        Ok(ret)
    }
}

const ENTRY_COUNT: usize = 512;

const fn p4_index(vaddr: usize) -> usize {
    (vaddr >> (12 + 27)) & (ENTRY_COUNT - 1)
}

const fn p3_index(vaddr: usize) -> usize {
    (vaddr >> (12 + 18)) & (ENTRY_COUNT - 1)
}

const fn p2_index(vaddr: usize) -> usize {
    (vaddr >> (12 + 9)) & (ENTRY_COUNT - 1)
}

const fn p1_index(vaddr: usize) -> usize {
    (vaddr >> 12) & (ENTRY_COUNT - 1)
}

fn table_of<'a, E>(paddr: PhysAddr) -> &'a [E] {
    let ptr = crate::mem::phys_to_virt(paddr) as *const E;
    unsafe { slice::from_raw_parts(ptr, ENTRY_COUNT) }
}

fn table_of_mut<'a, E>(paddr: PhysAddr) -> &'a mut [E] {
    let ptr = crate::mem::phys_to_virt(paddr) as *mut E;
    unsafe { slice::from_raw_parts_mut(ptr, ENTRY_COUNT) }
}

fn next_table_mut<'a, E: GenericPTE>(entry: &E) -> PagingResult<&'a mut [E]> {
    // A leaf is a mapped frame, not a table. Reading 512 entries out of it
    // reinterprets whatever the frame holds as page-table entries, and the
    // caller then WRITES one of them -- so a `map` of a 4 KiB page inside an
    // existing 2 MiB or 1 GiB mapping used to scribble a PTE onto the mapped
    // data and install the data frame as a page table. This was a
    // `debug_assert!`, which a release kernel does not compile, so the check
    // that was supposed to catch it was absent exactly where it mattered.
    //
    // `AlreadyMapped` rather than `NotMapped`: something IS mapped over that
    // address, and `map` -- the only path that can reach a leaf here, through
    // `get_entry_mut_or_create` -- is the only caller that can act on it.
    // Checked before `is_present`, because a PROT_NONE huge leaf (`x86` keeps
    // the PS bit and drops PRESENT) is just as much of a mapping.
    if entry.is_leaf() {
        return Err(PagingError::AlreadyMapped);
    }
    if !entry.is_present() {
        return Err(PagingError::NotMapped);
    }
    Ok(table_of_mut(entry.addr()))
}

fn next_table_mut_or_create<'a, E: GenericPTE>(
    entry: &mut E,
    mut allocator: impl FnMut() -> Option<PhysAddr>,
) -> PagingResult<&'a mut [E]> {
    if entry.is_unused() {
        let paddr = allocator().ok_or(PagingError::NoMemory)?;
        entry.set_table(paddr);
        Ok(table_of_mut(paddr))
    } else {
        next_table_mut(entry)
    }
}

/// The page-table walker every architecture uses, on the host.
///
/// This module was `#[cfg(not(feature = "libos"))]`, so no `cargo test` has
/// ever compiled it, let alone run it: the code that turns a virtual address
/// into a page-table entry for x86_64, aarch64 and riscv64 alike had zero
/// tests. Nothing here needs hardware -- a page table is a tree of 512-entry
/// arrays in physical memory, and `libos` has physical memory -- so the only
/// thing standing in the way was the `cfg`.
///
/// The entry type below is shaped like [`X86PTE`](../../bare/arch/x86_64/vm.rs):
/// physical address in bits 12..52, a present bit, and a page-size bit that
/// tells a mapped frame from a pointer to the next table. The walker only
/// ever asks a `GenericPTE` those questions, so that is enough to exercise it
/// exactly as the real ones do.
#[cfg(test)]
mod walker_tests {
    use super::*;
    use crate::utils::test_frames;
    use crate::PAGE_SIZE;
    use alloc::vec;
    use alloc::vec::Vec;

    const K: usize = PageSize::Size4K as usize;
    const M: usize = PageSize::Size2M as usize;
    const G: usize = PageSize::Size1G as usize;

    const PRESENT: u64 = 1 << 0;
    /// The page-size bit: set in a P3 or P2 entry that maps a frame instead of
    /// pointing at the next table.
    const PS: u64 = 1 << 7;
    const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;
    /// The `MMUFlags` the caller asked for, parked above the address so they
    /// survive a round trip. A real entry re-derives them from the hardware
    /// bits; that translation is the architecture's business, not the
    /// walker's.
    const FLAGS_SHIFT: u64 = 52;

    #[derive(Clone, Copy, Debug)]
    #[repr(transparent)]
    struct TestPTE(u64);

    impl GenericPTE for TestPTE {
        fn addr(&self) -> PhysAddr {
            (self.0 & ADDR_MASK) as usize
        }
        fn flags(&self) -> MMUFlags {
            MMUFlags::from_bits_truncate(((self.0 >> FLAGS_SHIFT) & 0xff) as usize)
        }
        fn is_unused(&self) -> bool {
            self.0 == 0
        }
        fn is_present(&self) -> bool {
            self.0 & PRESENT != 0
        }
        fn is_leaf(&self) -> bool {
            self.0 & PS != 0
        }
        fn set_addr(&mut self, paddr: PhysAddr) {
            self.0 = (self.0 & !ADDR_MASK) | (paddr as u64 & ADDR_MASK);
        }
        fn set_flags(&mut self, flags: MMUFlags, is_huge: bool) {
            // Like x86: a mapping with no access at all (PROT_NONE) keeps its
            // address and its page-size bit but is not present.
            let mut bits = self.0 & ADDR_MASK;
            if flags.intersects(MMUFlags::READ | MMUFlags::WRITE | MMUFlags::EXECUTE) {
                bits |= PRESENT;
            }
            if is_huge {
                bits |= PS;
            }
            self.0 = bits | ((flags.bits() as u64 & 0xff) << FLAGS_SHIFT);
        }
        fn set_table(&mut self, paddr: PhysAddr) {
            self.0 = (paddr as u64 & ADDR_MASK)
                | PRESENT
                | (((MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER).bits() as u64)
                    << FLAGS_SHIFT);
        }
        fn clear(&mut self) {
            self.0 = 0;
        }
    }

    /// The 4-level tree (x86_64, aarch64).
    type Pt4 = PageTableImpl<PageTableLevel4, TestPTE>;
    /// The 3-level tree (riscv64 Sv39), whose root is the P3.
    type Pt3 = PageTableImpl<PageTableLevel3, TestPTE>;

    fn rw() -> MMUFlags {
        MMUFlags::READ | MMUFlags::WRITE
    }

    fn a_table<L: PageTableLevel>() -> PageTableImpl<L, TestPTE> {
        test_frames::install();
        PageTableImpl::new()
    }

    /// A frame of zeros, 2 MiB aligned, to be the data behind a huge page.
    ///
    /// Zeros because that is what a freshly faulted anonymous page holds, and
    /// it is the case that goes silent: a walker that descends into the frame
    /// reads those zeros as unused page-table entries, so `map` believes the
    /// slot is free and FILLS ONE IN -- eight bytes of page-table entry
    /// written over the process's data, with no error returned.
    ///
    /// 2 MiB aligned because `map` aligns a huge page's frame down to the
    /// page's own size: only then is the address a walker would step into the
    /// frame this hands back.
    fn a_zeroed_2m_frame() -> Vec<PhysFrame> {
        let frames = PhysFrame::new_contiguous(1, 21);
        assert!(!frames.is_empty(), "no frame");
        assert!(PageSize::Size2M.is_aligned(frames[0].paddr()));
        crate::mem::pmem_zero(frames[0].paddr(), PAGE_SIZE);
        frames
    }

    fn is_still_zero(frame: &PhysFrame) -> bool {
        let words = unsafe {
            slice::from_raw_parts(
                crate::mem::phys_to_virt(frame.paddr()) as *const u64,
                PAGE_SIZE / 8,
            )
        };
        words.iter().all(|w| *w == 0)
    }

    // ── the address split ──────────────────────────────────────────────────

    /// The four index functions are the whole translation. A wrong shift does
    /// not fail loudly: it maps the page somewhere else and the caller finds
    /// out when a store lands in the wrong process.
    #[test]
    fn an_address_is_split_into_four_nine_bit_indices_and_an_offset() {
        // Each index is nine bits, at 12, 21, 30 and 39.
        let vaddr = (0x1a << 39) | (0x2b << 30) | (0x3c << 21) | (0x4d << 12) | 0x5e;
        assert_eq!(p4_index(vaddr), 0x1a);
        assert_eq!(p3_index(vaddr), 0x2b);
        assert_eq!(p2_index(vaddr), 0x3c);
        assert_eq!(p1_index(vaddr), 0x4d);

        // Each one takes nine bits and no more: the neighbours above and below
        // must not bleed in.
        assert_eq!(p1_index(usize::MAX), 511);
        assert_eq!(p2_index(usize::MAX), 511);
        assert_eq!(p3_index(usize::MAX), 511);
        assert_eq!(p4_index(usize::MAX), 511);
        assert_eq!(p1_index(K - 1), 0);
        assert_eq!(p2_index(M - 1), 0);
        assert_eq!(p3_index(G - 1), 0);

        // A kernel-half address indexes the top of the P4 -- the sign
        // extension above bit 47 must not change which entry it is. This one
        // is the classic -2 GiB kernel window.
        assert_eq!(p4_index(0xffff_ffff_8000_0000), 511);
        assert_eq!(p3_index(0xffff_ffff_8000_0000), 510);
        assert_eq!(p2_index(0xffff_ffff_8000_0000), 0);
        assert_eq!(p1_index(0xffff_ffff_8000_0000), 0);
        assert_eq!(ENTRY_COUNT * core::mem::size_of::<TestPTE>(), PAGE_SIZE);
    }

    // ── map / query / unmap ────────────────────────────────────────────────

    #[test]
    fn a_mapped_page_is_found_again_with_its_frame_its_flags_and_its_size() {
        let mut pt = a_table::<PageTableLevel4>();
        let vaddr = 0x2000_0000_0000 + 7 * K;
        let paddr = 0x9876_5000;
        pt.map(Page::new_aligned(vaddr, PageSize::Size4K), paddr, rw())
            .unwrap();

        let (found, flags, size) = pt.query(vaddr).unwrap();
        assert_eq!(found, paddr);
        assert_eq!(flags, rw());
        assert_eq!(size, PageSize::Size4K);

        // The offset inside the page comes back too: `query` answers for an
        // address, not for a page.
        assert_eq!(pt.query(vaddr + 0x321).unwrap().0, paddr + 0x321);

        // Its neighbours are not mapped by association.
        assert!(matches!(pt.query(vaddr + K), Err(PagingError::NotMapped)));
        assert!(matches!(pt.query(vaddr - K), Err(PagingError::NotMapped)));
    }

    /// The intermediate tables are allocated once and reused. If they were
    /// re-allocated per page, a page table would grow a frame per mapped page
    /// and the old tables would be unreachable -- a leak that only shows up as
    /// "the machine ran out of memory".
    #[test]
    fn the_intermediate_tables_are_built_once_and_then_shared() {
        let mut pt = a_table::<PageTableLevel4>();
        let vaddr = 0x2100_0000_0000;

        let before = test_frames::live_frames();
        pt.map(Page::new_aligned(vaddr, PageSize::Size4K), 0x1000, rw())
            .unwrap();
        // P3, P2 and P1: three tables between the root and the leaf.
        assert_eq!(test_frames::live_frames() - before, 3);

        // Another page in the same 2 MiB region shares all three.
        pt.map(Page::new_aligned(vaddr + K, PageSize::Size4K), 0x2000, rw())
            .unwrap();
        assert_eq!(test_frames::live_frames() - before, 3);

        // A page 2 MiB away needs a new P1 only.
        pt.map(Page::new_aligned(vaddr + M, PageSize::Size4K), 0x3000, rw())
            .unwrap();
        assert_eq!(test_frames::live_frames() - before, 4);

        // And one 1 GiB away needs a P2 and a P1.
        pt.map(Page::new_aligned(vaddr + G, PageSize::Size4K), 0x4000, rw())
            .unwrap();
        assert_eq!(test_frames::live_frames() - before, 6);
    }

    /// Dropping an address space gives its frames back. A page table that
    /// leaked its own tables would burn memory on every `fork`.
    #[test]
    fn dropping_a_page_table_returns_every_frame_it_allocated() {
        test_frames::install();
        let before = test_frames::live_frames();
        {
            let mut pt = Pt4::new();
            for i in 0..4 {
                pt.map(
                    Page::new_aligned(0x2200_0000_0000 + i * G, PageSize::Size4K),
                    0x1000 * (i + 1),
                    rw(),
                )
                .unwrap();
            }
            assert!(test_frames::live_frames() > before);
        }
        assert_eq!(test_frames::live_frames(), before);
    }

    #[test]
    fn mapping_over_a_live_page_is_refused_and_leaves_it_alone() {
        let mut pt = a_table::<PageTableLevel4>();
        let vaddr = 0x2300_0000_0000;
        pt.map(Page::new_aligned(vaddr, PageSize::Size4K), 0x1000, rw())
            .unwrap();
        assert!(matches!(
            pt.map(
                Page::new_aligned(vaddr, PageSize::Size4K),
                0x2000,
                MMUFlags::READ
            ),
            Err(PagingError::AlreadyMapped)
        ));
        assert_eq!(pt.query(vaddr).unwrap(), (0x1000, rw(), PageSize::Size4K));
    }

    /// `unmap` names the frame it released so the caller can free it, and the
    /// page stops resolving. The tables above it stay, which is what makes a
    /// re-fault cheap.
    #[test]
    fn unmapping_releases_the_frame_and_keeps_the_tables() {
        let mut pt = a_table::<PageTableLevel4>();
        let vaddr = 0x2400_0000_0000;
        pt.map(Page::new_aligned(vaddr, PageSize::Size4K), 0x7000, rw())
            .unwrap();

        let after_map = test_frames::live_frames();
        assert_eq!(pt.unmap(vaddr).unwrap(), (0x7000, PageSize::Size4K));
        assert!(matches!(pt.query(vaddr), Err(PagingError::NotMapped)));
        assert_eq!(test_frames::live_frames(), after_map);

        // Unmapping it again finds nothing.
        assert!(matches!(pt.unmap(vaddr), Err(PagingError::NotMapped)));

        // And mapping it back needs no new table.
        pt.map(Page::new_aligned(vaddr, PageSize::Size4K), 0x8000, rw())
            .unwrap();
        assert_eq!(test_frames::live_frames(), after_map);
    }

    /// The phantom-PTE guard, which is the one that corrupted anonymous
    /// memory: `get_entry_mut` hands back a leaf whenever the tables above it
    /// exist, even after `unmap` cleared it. Without the `is_unused` check an
    /// `update` on that address stamps PRESENT|WRITABLE onto an entry whose
    /// address is zero, and the next store lands in physical frame 0 with no
    /// fault and no page ever committed.
    #[test]
    fn updating_an_unmapped_page_never_resurrects_it_onto_frame_zero() {
        let mut pt = a_table::<PageTableLevel4>();
        let vaddr = 0x2500_0000_0000;
        pt.map(Page::new_aligned(vaddr, PageSize::Size4K), 0x9000, rw())
            .unwrap();
        pt.unmap(vaddr).unwrap();

        assert!(matches!(
            pt.update(vaddr, None, Some(MMUFlags::RXW)),
            Err(PagingError::NotMapped)
        ));
        assert!(matches!(pt.query(vaddr), Err(PagingError::NotMapped)));

        // The same for an address whose tables were never built at all, and
        // for one whose leaf was never touched.
        assert!(matches!(
            pt.update(vaddr + G, None, Some(MMUFlags::RXW)),
            Err(PagingError::NotMapped)
        ));
        assert!(matches!(
            pt.update(vaddr + K, None, Some(MMUFlags::RXW)),
            Err(PagingError::NotMapped)
        ));
    }

    /// Copy-on-write write-protects a live page and repoints it at the copy.
    #[test]
    fn update_can_change_the_flags_the_frame_or_both() {
        let mut pt = a_table::<PageTableLevel4>();
        let vaddr = 0x2600_0000_0000;
        pt.map(Page::new_aligned(vaddr, PageSize::Size4K), 0x1_0000, rw())
            .unwrap();

        assert_eq!(
            pt.update(vaddr, None, Some(MMUFlags::READ)).unwrap(),
            PageSize::Size4K
        );
        assert_eq!(
            pt.query(vaddr).unwrap(),
            (0x1_0000, MMUFlags::READ, PageSize::Size4K)
        );

        pt.update(vaddr, Some(0x2_0000), None).unwrap();
        assert_eq!(
            pt.query(vaddr).unwrap(),
            (0x2_0000, MMUFlags::READ, PageSize::Size4K)
        );

        pt.update(vaddr, Some(0x3_0000), Some(rw())).unwrap();
        assert_eq!(pt.query(vaddr).unwrap(), (0x3_0000, rw(), PageSize::Size4K));
    }

    /// PROT_NONE is a mapping with no access, not the absence of a mapping.
    /// It must stay claimed, so the next `map` over it is still refused.
    #[test]
    fn a_page_with_no_access_is_still_a_mapping() {
        let mut pt = a_table::<PageTableLevel4>();
        let vaddr = 0x2700_0000_0000;
        pt.map(
            Page::new_aligned(vaddr, PageSize::Size4K),
            0x1000,
            MMUFlags::empty(),
        )
        .unwrap();
        assert!(matches!(
            pt.map(Page::new_aligned(vaddr, PageSize::Size4K), 0x2000, rw()),
            Err(PagingError::AlreadyMapped)
        ));
        assert_eq!(pt.unmap(vaddr).unwrap().0, 0x1000);
    }

    // ── huge pages ─────────────────────────────────────────────────────────

    #[test]
    fn a_2m_mapping_stops_at_the_p2_and_a_1g_mapping_at_the_p3() {
        let mut pt = a_table::<PageTableLevel4>();

        let two_meg = 0x2800_0000_0000;
        let before = test_frames::live_frames();
        pt.map(Page::new_aligned(two_meg, PageSize::Size2M), 4 * M, rw())
            .unwrap();
        // P3 and P2 only: a huge leaf has no page table under it.
        assert_eq!(test_frames::live_frames() - before, 2);
        assert_eq!(
            pt.query(two_meg + 0x1234).unwrap(),
            (4 * M + 0x1234, rw(), PageSize::Size2M)
        );

        let one_gig = 0x2900_0000_0000;
        let before = test_frames::live_frames();
        pt.map(Page::new_aligned(one_gig, PageSize::Size1G), 8 * G, rw())
            .unwrap();
        // The P3 alone.
        assert_eq!(test_frames::live_frames() - before, 1);
        assert_eq!(
            pt.query(one_gig + M + 0x30).unwrap(),
            (8 * G + M + 0x30, rw(), PageSize::Size1G)
        );

        assert_eq!(pt.unmap(two_meg).unwrap(), (4 * M, PageSize::Size2M));
        assert_eq!(pt.unmap(one_gig).unwrap(), (8 * G, PageSize::Size1G));
    }

    /// A huge page can only describe a frame aligned to its own size -- the
    /// low bits of the entry are not address bits at all. `map` aligns the
    /// physical address down for the caller; the mapping then covers a
    /// different range than a naive reader of the arguments would think, so
    /// it has to be the range `query` reports.
    #[test]
    fn a_huge_mapping_aligns_its_frame_down_to_its_own_size() {
        let mut pt = a_table::<PageTableLevel4>();

        let vaddr = 0x3700_0000_0000;
        pt.map(
            Page::new_aligned(vaddr, PageSize::Size2M),
            6 * M + 5 * K,
            rw(),
        )
        .unwrap();
        assert_eq!(pt.query(vaddr).unwrap().0, 6 * M);
        assert_eq!(pt.unmap(vaddr).unwrap(), (6 * M, PageSize::Size2M));

        let gig = 0x3800_0000_0000;
        pt.map(
            Page::new_aligned(gig, PageSize::Size1G),
            2 * G + 7 * M,
            rw(),
        )
        .unwrap();
        assert_eq!(pt.query(gig).unwrap().0, 2 * G);

        // A 4 KiB page is already as fine as the grid goes, so its frame is
        // only rounded to a page.
        let small = 0x3900_0000_0000;
        pt.map(
            Page::new_aligned(small, PageSize::Size4K),
            0x1234_5678,
            rw(),
        )
        .unwrap();
        assert_eq!(pt.query(small).unwrap().0, 0x1234_5000);
    }

    /// The hole this batch closes. A huge leaf is a mapped frame; walking
    /// into it reads 512 words of somebody's data as page-table entries and
    /// then writes one of them. The check that was supposed to stop it was a
    /// `debug_assert!`, which a release kernel does not compile.
    #[test]
    fn mapping_a_small_page_inside_a_huge_one_never_walks_into_the_data() {
        let mut pt = a_table::<PageTableLevel4>();
        let vaddr = 0x2a00_0000_0000;
        let frames = a_zeroed_2m_frame();

        // A 2 MiB mapping whose frame we can inspect afterwards. The walker
        // only ever reaches it by mistake, so any change to it is the bug.
        pt.map(
            Page::new_aligned(vaddr, PageSize::Size2M),
            frames[0].paddr(),
            rw(),
        )
        .unwrap();

        for offset in [0, K, M - K] {
            let result = pt.map(
                Page::new_aligned(vaddr + offset, PageSize::Size4K),
                0x4000,
                rw(),
            );
            // The frame first: a walker that descended into it has already
            // written a page-table entry over somebody's data, which is the
            // damage, whatever it went on to return.
            assert!(
                is_still_zero(&frames[0]),
                "a 4 KiB map at +{:#x} wrote a page-table entry into the frame the 2 MiB page maps",
                offset
            );
            assert!(
                matches!(result, Err(PagingError::AlreadyMapped)),
                "a 4 KiB map at +{:#x} inside a 2 MiB page was not refused",
                offset
            );
        }

        // The same one level up: a 2 MiB and a 4 KiB map inside a live 1 GiB
        // page. Nothing to paint here -- a 1 GiB-aligned frame does not fit
        // in the mock physical memory -- but the refusal is the same one.
        let gig = 0x2b00_0000_0000;
        pt.map(Page::new_aligned(gig, PageSize::Size1G), 0, rw())
            .unwrap();
        assert!(matches!(
            pt.map(
                Page::new_aligned(gig + 2 * M, PageSize::Size2M),
                0x4000,
                rw()
            ),
            Err(PagingError::AlreadyMapped)
        ));
        assert!(matches!(
            pt.map(Page::new_aligned(gig + K, PageSize::Size4K), 0x4000, rw()),
            Err(PagingError::AlreadyMapped)
        ));

        // The huge mappings are untouched by the refusals.
        assert_eq!(pt.query(vaddr).unwrap().2, PageSize::Size2M);
        assert_eq!(pt.query(gig).unwrap().2, PageSize::Size1G);
    }

    /// A PROT_NONE huge page is not present, so the leaf check has to come
    /// first or the walker reports "nothing mapped here" for a frame that is.
    #[test]
    fn a_huge_page_with_no_access_is_still_in_the_way() {
        let mut pt = a_table::<PageTableLevel4>();
        let vaddr = 0x2c00_0000_0000;
        let frames = a_zeroed_2m_frame();
        pt.map(
            Page::new_aligned(vaddr, PageSize::Size2M),
            frames[0].paddr(),
            MMUFlags::empty(),
        )
        .unwrap();
        let result = pt.map(Page::new_aligned(vaddr, PageSize::Size4K), 0x4000, rw());
        assert!(is_still_zero(&frames[0]), "the walker wrote into the frame");
        assert!(matches!(result, Err(PagingError::AlreadyMapped)));
    }

    // ── splitting ──────────────────────────────────────────────────────────

    /// `split_huge_page` is how `stack_guard` takes one page away from a
    /// region the kernel mapped huge. The replacement has to describe exactly
    /// the same memory with exactly the same permissions, or the split itself
    /// changes what the kernel can reach.
    #[test]
    fn splitting_a_2m_page_describes_the_same_memory_in_4k_entries() {
        let mut pt = a_table::<PageTableLevel4>();
        let vaddr = 0x2d00_0000_0000;
        let base = 16 * M;
        let flags = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
        pt.map(Page::new_aligned(vaddr, PageSize::Size2M), base, flags)
            .unwrap();

        let mut frames = vec![];
        pt.split_huge_page(vaddr + 3 * K, || {
            let f = PhysFrame::new_zero()?;
            let paddr = f.paddr();
            frames.push(f);
            Some(paddr)
        })
        .unwrap();
        assert_eq!(frames.len(), 1, "one table for one level");

        for i in [0usize, 1, 3, 255, 511] {
            assert_eq!(
                pt.query(vaddr + i * K + 0x11).unwrap(),
                (base + i * K + 0x11, flags, PageSize::Size4K),
                "page {} of the split",
                i
            );
        }
        // The children are ordinary pages, not huge leaves. The page-size
        // bit means something different at every level -- in a P1 entry x86
        // uses bit 7 for the PAT, not for PS -- so a child that inherited
        // "huge" would come out of the split with a different cache policy
        // than the page it replaced.
        for i in [0usize, 3, 511] {
            let (entry, size) = pt.get_entry_mut(vaddr + i * K).unwrap();
            assert_eq!(size, PageSize::Size4K);
            assert!(!entry.is_leaf(), "child {} still marked huge", i);
        }
        // Nothing outside the split page moved.
        assert!(matches!(pt.query(vaddr + M), Err(PagingError::NotMapped)));

        // And now a 4 KiB page of it can be taken away on its own -- which is
        // the entire point.
        assert_eq!(
            pt.unmap(vaddr + 3 * K).unwrap(),
            (base + 3 * K, PageSize::Size4K)
        );
        assert_eq!(pt.query(vaddr).unwrap().0, base);
    }

    #[test]
    fn splitting_a_1g_page_walks_it_down_two_levels_and_leaves_the_rest_huge() {
        let mut pt = a_table::<PageTableLevel4>();
        let vaddr = 0x2e00_0000_0000;
        let base = 4 * G;
        pt.map(Page::new_aligned(vaddr, PageSize::Size1G), base, rw())
            .unwrap();

        let mut frames = vec![];
        pt.split_huge_page(vaddr, || {
            let f = PhysFrame::new_zero()?;
            let paddr = f.paddr();
            frames.push(f);
            Some(paddr)
        })
        .unwrap();
        assert_eq!(frames.len(), 2, "a P2 and a P1");

        // The address asked for is now a 4 KiB page ...
        assert_eq!(pt.query(vaddr).unwrap(), (base, rw(), PageSize::Size4K));
        // ... its own 2 MiB neighbourhood is 4 KiB pages ...
        assert_eq!(
            pt.query(vaddr + 511 * K).unwrap(),
            (base + 511 * K, rw(), PageSize::Size4K)
        );
        // ... and the remaining 511 halves of the gigabyte are still 2 MiB
        // leaves, pointing where they always did.
        assert_eq!(
            pt.query(vaddr + 5 * M + 0x40).unwrap(),
            (base + 5 * M + 0x40, rw(), PageSize::Size2M)
        );
        assert_eq!(
            pt.query(vaddr + G - M).unwrap(),
            (base + G - M, rw(), PageSize::Size2M)
        );
    }

    /// Running out of frames mid-split must leave the mapping exactly as it
    /// was: `stack_guard` runs with the scheduler's lock held and cannot
    /// recover from a half-split kernel mapping.
    #[test]
    fn a_split_that_cannot_allocate_leaves_the_huge_page_untouched() {
        let mut pt = a_table::<PageTableLevel4>();
        let vaddr = 0x2f00_0000_0000;
        let base = 32 * M;
        pt.map(Page::new_aligned(vaddr, PageSize::Size2M), base, rw())
            .unwrap();

        assert!(matches!(
            pt.split_huge_page(vaddr, || None),
            Err(PagingError::NoMemory)
        ));
        assert_eq!(pt.query(vaddr).unwrap(), (base, rw(), PageSize::Size2M));

        // The second level failing is the same story: the 1 GiB leaf is gone
        // but the 2 MiB ones that replaced it describe the same gigabyte.
        let gig = 0x3000_0000_0000;
        pt.map(Page::new_aligned(gig, PageSize::Size1G), 8 * G, rw())
            .unwrap();
        let mut frames = vec![];
        let mut budget = 1;
        assert!(matches!(
            pt.split_huge_page(gig, || {
                if budget == 0 {
                    return None;
                }
                budget -= 1;
                let f = PhysFrame::new_zero()?;
                let paddr = f.paddr();
                frames.push(f);
                Some(paddr)
            }),
            Err(PagingError::NoMemory)
        ));
        assert_eq!(
            pt.query(gig + 3 * M + 8).unwrap(),
            (8 * G + 3 * M + 8, rw(), PageSize::Size2M)
        );
    }

    #[test]
    fn splitting_what_is_already_a_small_page_or_nothing_at_all() {
        let mut pt = a_table::<PageTableLevel4>();
        let vaddr = 0x3100_0000_0000;
        pt.map(Page::new_aligned(vaddr, PageSize::Size4K), 0x5000, rw())
            .unwrap();
        // Already 4 KiB: nothing to do, and no frame is consumed saying so.
        pt.split_huge_page(vaddr, || panic!("must not allocate"))
            .unwrap();
        assert_eq!(pt.query(vaddr).unwrap().2, PageSize::Size4K);

        // A hole inside a built-out table is "already split" as well.
        pt.split_huge_page(vaddr + K, || panic!("must not allocate"))
            .unwrap();

        // An address with no tables above it at all has nothing to split.
        assert!(matches!(
            pt.split_huge_page(vaddr + G, || panic!("must not allocate")),
            Err(PagingError::NotMapped)
        ));
    }

    // ── the three-level tree ───────────────────────────────────────────────

    /// Sv39's root IS the P3, so the walker must not look for a P4 above it.
    /// Reading one would walk off the root table into whatever follows it in
    /// physical memory.
    #[test]
    fn a_three_level_tree_maps_its_gigabytes_in_the_root_table() {
        let mut pt = a_table::<PageTableLevel3>();
        let vaddr = 0x20_0000_0000 + 3 * G; // inside Sv39's 512 GiB

        let before = test_frames::live_frames();
        pt.map(Page::new_aligned(vaddr, PageSize::Size1G), 12 * G, rw())
            .unwrap();
        // The root already exists, so a 1 GiB page costs nothing.
        assert_eq!(test_frames::live_frames(), before);
        assert_eq!(
            pt.query(vaddr + 0x777).unwrap(),
            (12 * G + 0x777, rw(), PageSize::Size1G)
        );

        // And a 4 KiB page needs a P2 and a P1, one fewer than on a 4-level
        // tree.
        let small = 0x20_0000_0000 + 9 * G;
        let before = test_frames::live_frames();
        pt.map(Page::new_aligned(small, PageSize::Size4K), 0x6000, rw())
            .unwrap();
        assert_eq!(test_frames::live_frames() - before, 2);
        assert_eq!(pt.query(small).unwrap(), (0x6000, rw(), PageSize::Size4K));
    }

    /// The walker labels levels from the top of a 4-level tree, because that
    /// is how it turns an index into an address. A 3-level root starts one
    /// level down; from the root it named every entry 512 GiB apart.
    #[test]
    fn walking_a_tree_names_the_address_each_entry_covers() {
        use alloc::vec::Vec;
        use core::cell::RefCell;

        fn seen<L: PageTableLevel>(pt: &PageTableImpl<L, TestPTE>) -> Vec<(usize, usize)> {
            let out = RefCell::new(Vec::new());
            pt.walk(
                table_of(pt.table_phys()),
                PageTableImpl::<L, TestPTE>::root_walk_level(),
                0,
                usize::MAX,
                &|level: usize, _idx: usize, vaddr: usize, entry: &TestPTE| {
                    if entry.is_leaf() {
                        out.borrow_mut().push((level, vaddr));
                    }
                },
            );
            out.into_inner()
        }

        let mut four = a_table::<PageTableLevel4>();
        four.map(Page::new_aligned(5 * G, PageSize::Size1G), 0, rw())
            .unwrap();
        four.map(Page::new_aligned(7 * G + 4 * M, PageSize::Size2M), 0, rw())
            .unwrap();
        assert_eq!(seen(&four), vec![(1, 5 * G), (2, 7 * G + 4 * M)]);

        let mut three = a_table::<PageTableLevel3>();
        three
            .map(Page::new_aligned(5 * G, PageSize::Size1G), 0, rw())
            .unwrap();
        three
            .map(Page::new_aligned(7 * G + 4 * M, PageSize::Size2M), 0, rw())
            .unwrap();
        assert_eq!(seen(&three), vec![(1, 5 * G), (2, 7 * G + 4 * M)]);
    }

    // ── the gather window ──────────────────────────────────────────────────

    /// `fork` write-protects every mapping one at a time and pays for one
    /// shootdown at the end instead of two per mapping. Losing the debt is
    /// silent corruption: another CPU keeps writing through a stale writable
    /// entry onto a frame the child now shares.
    #[test]
    fn a_gather_window_owes_exactly_one_shootdown_when_it_closes() {
        let mut pt = a_table::<PageTableLevel4>();
        let vaddr = 0x3200_0000_0000;
        for i in 0..4 {
            pt.map(
                Page::new_aligned(vaddr + i * K, PageSize::Size4K),
                0x1000 * (i + 1),
                rw(),
            )
            .unwrap();
        }

        // Outside a window nothing is owed: the operation shot down as it went.
        pt.unmap_no_shootdown(vaddr).unwrap();
        assert!(!pt.set_gather(true));
        assert!(!pt.set_gather(false), "a debt from before the window");

        // Inside one, an unmap and an update each leave a debt, and closing
        // reports it once.
        assert!(!pt.set_gather(true));
        pt.unmap_no_shootdown(vaddr + K).unwrap();
        pt.update_no_shootdown(vaddr + 2 * K, None, Some(MMUFlags::READ))
            .unwrap();
        assert!(pt.set_gather(false));
        // And the debt is cleared by being reported.
        assert!(!pt.set_gather(true));
        assert!(!pt.set_gather(false));

        // Write-protecting a page is the `fork` case, and it owes a shootdown
        // on its own: another CPU can still write through a stale writable
        // entry onto a frame the child now shares.
        assert!(!pt.set_gather(true));
        pt.update_no_shootdown(vaddr + 3 * K, None, Some(MMUFlags::READ))
            .unwrap();
        assert!(pt.set_gather(false));
    }

    /// Nested windows: an inner close must not end a window an outer caller
    /// still needs, or the shootdown happens while the outer one is still
    /// write-protecting pages.
    #[test]
    fn only_the_outermost_close_of_a_nested_window_reports_the_debt() {
        let mut pt = a_table::<PageTableLevel4>();
        let vaddr = 0x3300_0000_0000;
        pt.map(Page::new_aligned(vaddr, PageSize::Size4K), 0x1000, rw())
            .unwrap();

        assert!(!pt.set_gather(true));
        assert!(!pt.set_gather(true));
        pt.unmap_no_shootdown(vaddr).unwrap();
        assert!(!pt.set_gather(false), "the outer window is still open");
        assert!(pt.set_gather(false));
    }

    /// The debt is recorded before the lookup, on purpose: an entry that
    /// turns out to be absent owes nothing, but one extra flush costs an IPI
    /// and a missed one costs silent corruption.
    #[test]
    fn a_window_that_unmapped_nothing_still_pays_once() {
        let mut pt = a_table::<PageTableLevel4>();
        assert!(!pt.set_gather(true));
        assert!(matches!(
            pt.unmap_no_shootdown(0x3400_0000_0000),
            Err(PagingError::NotMapped)
        ));
        assert!(pt.set_gather(false));
    }

    // ── the range helpers, over a real tree ────────────────────────────────

    /// `map_cont` picks a page size per step; `common::vm` tests that choice
    /// against a table that only records it. Here the same range goes into
    /// the real walker, so the sizes it chose have to be sizes the tree can
    /// actually hold.
    #[test]
    fn a_huge_range_lands_as_the_pages_map_cont_chose() {
        let mut pt = a_table::<PageTableLevel4>();
        // 1 GiB-aligned, and long enough for a gigabyte, a couple of 2 MiB
        // pages and a 4 KiB tail.
        let vaddr = 0x3500_0000_0000;
        let len = G + 2 * M + 3 * K;
        pt.map_cont(vaddr, len, vaddr, MMUFlags::HUGE_PAGE | MMUFlags::READ)
            .unwrap();

        assert_eq!(pt.query(vaddr).unwrap().2, PageSize::Size1G);
        assert_eq!(pt.query(vaddr + G).unwrap().2, PageSize::Size2M);
        assert_eq!(pt.query(vaddr + G + M).unwrap().2, PageSize::Size2M);
        assert_eq!(pt.query(vaddr + G + 2 * M).unwrap().2, PageSize::Size4K);
        assert_eq!(pt.query(vaddr + len - K).unwrap().2, PageSize::Size4K);
        assert!(matches!(pt.query(vaddr + len), Err(PagingError::NotMapped)));
        // Identity mapped, so every address answers for itself.
        assert_eq!(
            pt.query(vaddr + G + M + 0x99).unwrap().0,
            vaddr + G + M + 0x99
        );

        pt.unmap_cont(vaddr, len).unwrap();
        for probe in [0, G, G + M, G + 2 * M, len - K] {
            assert!(
                matches!(pt.query(vaddr + probe), Err(PagingError::NotMapped)),
                "+{:#x} survived unmap_cont",
                probe
            );
        }
    }

    /// `unmap_cont` walks the range in whatever page size it finds, so a hole
    /// in the middle must not stop it or shift it off the page grid.
    #[test]
    fn unmapping_a_range_steps_over_the_holes_in_it() {
        let mut pt = a_table::<PageTableLevel4>();
        let vaddr = 0x3600_0000_0000;
        for i in [0usize, 1, 5, 6] {
            pt.map(
                Page::new_aligned(vaddr + i * K, PageSize::Size4K),
                0x1000 * (i + 1),
                rw(),
            )
            .unwrap();
        }
        pt.unmap_cont(vaddr, 8 * K).unwrap();
        for i in 0..8 {
            assert!(matches!(
                pt.query(vaddr + i * K),
                Err(PagingError::NotMapped)
            ));
        }
    }
}
