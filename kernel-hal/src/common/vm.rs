use crate::{addr::is_aligned, MMUFlags, PhysAddr, VirtAddr};

/// Errors may occur during address translation.
#[derive(Debug)]
pub enum PagingError {
    NoMemory,
    NotMapped,
    AlreadyMapped,
}

/// Address translation result.
pub type PagingResult<T = ()> = Result<T, PagingError>;

/// The [`PagingError::NotMapped`] can be ignored.
pub trait IgnoreNotMappedErr {
    /// If self is `Err(PagingError::NotMapped`, ignores the error and returns
    /// `Ok(())`, otherwise remain unchanged.
    fn ignore(self) -> PagingResult;
}

impl<T> IgnoreNotMappedErr for PagingResult<T> {
    fn ignore(self) -> PagingResult {
        match self {
            Ok(_) | Err(PagingError::NotMapped) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Possible page size (4K, 2M, 1G).
#[repr(usize)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum PageSize {
    Size4K = 0x1000,
    Size2M = 0x20_0000,
    Size1G = 0x4000_0000,
}

pub const BASE_PAGE_SIZE: PageSize = PageSize::Size4K;

/// A 4K, 2M or 1G size page.
#[derive(Debug, Copy, Clone)]
pub struct Page {
    pub vaddr: VirtAddr,
    pub size: PageSize,
}

impl PageSize {
    pub const fn is_aligned(self, addr: usize) -> bool {
        self.page_offset(addr) == 0
    }

    pub const fn align_down(self, addr: usize) -> usize {
        addr & !(self as usize - 1)
    }

    pub const fn page_offset(self, addr: usize) -> usize {
        addr & (self as usize - 1)
    }

    pub const fn is_huge(self) -> bool {
        matches!(self, Self::Size1G | Self::Size2M)
    }
}

impl Page {
    pub fn new_aligned(vaddr: VirtAddr, size: PageSize) -> Self {
        debug_assert!(size.is_aligned(vaddr));
        Self { vaddr, size }
    }
}

/// A generic page table abstraction.
pub trait GenericPageTable: Sync + Send {
    /// Get the physical address of root page table.
    fn table_phys(&self) -> PhysAddr;

    /// Map the `page` to the frame of `paddr` with `flags`.
    fn map(&mut self, page: Page, paddr: PhysAddr, flags: MMUFlags) -> PagingResult;

    /// Unmap the page of `vaddr`.
    fn unmap(&mut self, vaddr: VirtAddr) -> PagingResult<(PhysAddr, PageSize)>;

    /// Change the `flags` of the page of `vaddr`.
    fn update(
        &mut self,
        vaddr: VirtAddr,
        paddr: Option<PhysAddr>,
        flags: Option<MMUFlags>,
    ) -> PagingResult<PageSize>;

    /// Query the physical address which the page of `vaddr` maps to.
    fn query(&self, vaddr: VirtAddr) -> PagingResult<(PhysAddr, MMUFlags, PageSize)>;

    /// Like [`unmap`](Self::unmap), but the implementation may DEFER the
    /// cross-CPU TLB shootdown, leaving only the local flush. Callers looping
    /// over a range use this and then issue ONE
    /// [`remote_flush_all`](Self::remote_flush_all) at the end — the mmu-gather
    /// pattern. A per-page synchronous shootdown is O(pages × ack-wait); with a
    /// peer CPU that cannot ack promptly (spinning on a lock with IRQs off,
    /// e.g. a page fault contending for the same address space) each page burns
    /// the full spin budget and a large `munmap` turns into an hours-long
    /// livelock. Seen in practice: glibc's malloc arena setup
    /// (mmap 128 MiB, munmap the unaligned head) from a fresh labwc thread
    /// wedged the whole desktop.
    fn unmap_no_shootdown(&mut self, vaddr: VirtAddr) -> PagingResult<(PhysAddr, PageSize)> {
        self.unmap(vaddr)
    }

    /// Like [`update`](Self::update), but may defer the cross-CPU TLB
    /// shootdown (see [`unmap_no_shootdown`](Self::unmap_no_shootdown)).
    fn update_no_shootdown(
        &mut self,
        vaddr: VirtAddr,
        paddr: Option<PhysAddr>,
        flags: Option<MMUFlags>,
    ) -> PagingResult<PageSize> {
        self.update(vaddr, paddr, flags)
    }

    /// Flush the TLB of every other CPU once, synchronously. Pairs with the
    /// `*_no_shootdown` methods above. Default is a no-op for implementations
    /// (libos, tests) whose `unmap`/`update` don't defer anything.
    fn remote_flush_all(&self) {}

    /// Begin or end a *gather window* on this address space: while one is open,
    /// [`remote_flush_all`](Self::remote_flush_all) only records that a flush is
    /// owed instead of performing it, and closing the window performs at most
    /// one.
    ///
    /// This exists for `fork`, which write-protects every mapping of the parent
    /// one at a time. Each mapping costs *two* full cross-CPU shootdowns today —
    /// one inside `VmObject::create_child`'s `range_change`, one in
    /// `VmMapping::protect_for_cow` — and each shootdown is an IPI round trip
    /// to every other CPU with an ack spin-wait. Measured, that made a
    /// copy-on-write `fork` of a *small* process 3x more expensive than the
    /// eager copy it replaced (10 956 us against 3 602 us), because a process
    /// with many small mappings pays per mapping while saving only per page.
    ///
    /// Returns whether a flush was owed at the moment the window closed, so the
    /// caller can issue exactly one.
    ///
    /// **Opening a window widens a real race** and callers must know it is not
    /// possible for them. Between write-protecting a page and flushing, another
    /// CPU can still write through a stale writable TLB entry — onto a frame the
    /// child now shares. Today that window is the few instructions inside
    /// `protect_for_cow`; gathered, it is the whole `fork`. The only caller
    /// opens one when the forking process has a single thread, which is the case
    /// where no other CPU can be executing in that address space at all.
    fn set_gather(&mut self, _on: bool) -> bool {
        false
    }

    fn map_cont(
        &mut self,
        start_vaddr: VirtAddr,
        size: usize,
        start_paddr: PhysAddr,
        flags: MMUFlags,
    ) -> PagingResult {
        assert!(is_aligned(start_vaddr));
        assert!(is_aligned(start_paddr));
        assert!(is_aligned(size));
        debug!(
            "map_cont: {:#x?} => {:#x}, flags={:?}",
            start_vaddr..start_vaddr + size,
            start_paddr,
            flags
        );
        let mut vaddr = start_vaddr;
        let mut paddr = start_paddr;
        let end_vaddr = vaddr + size;
        if flags.contains(MMUFlags::HUGE_PAGE) {
            while vaddr < end_vaddr {
                let remains = end_vaddr - vaddr;
                let page_size = if remains >= PageSize::Size1G as usize
                    && PageSize::Size1G.is_aligned(vaddr)
                    && PageSize::Size1G.is_aligned(paddr)
                {
                    PageSize::Size1G
                } else if remains >= PageSize::Size2M as usize
                    && PageSize::Size2M.is_aligned(vaddr)
                    && PageSize::Size2M.is_aligned(paddr)
                {
                    PageSize::Size2M
                } else {
                    PageSize::Size4K
                };
                let page = Page::new_aligned(vaddr, page_size);
                self.map(page, paddr, flags)?;
                vaddr += page_size as usize;
                paddr += page_size as usize;
            }
        } else {
            while vaddr < end_vaddr {
                let page_size = PageSize::Size4K;
                let page = Page::new_aligned(vaddr, page_size);
                self.map(page, paddr, flags)?;
                vaddr += page_size as usize;
                paddr += page_size as usize;
            }
        }
        Ok(())
    }

    fn unmap_cont(&mut self, start_vaddr: VirtAddr, size: usize) -> PagingResult {
        assert!(is_aligned(start_vaddr));
        assert!(is_aligned(size));
        debug!(
            "{:#x?} unmap_cont: {:#x?}",
            self.table_phys(),
            start_vaddr..start_vaddr + size
        );
        let mut vaddr = start_vaddr;
        let end_vaddr = vaddr + size;
        // mmu-gather: clear every PTE first (local flush only), then shoot the
        // whole range down on the other CPUs with ONE synchronous IPI round.
        // The frames are not freed until after this function returns, so the
        // single flush at the end still closes the stale-TLB window before any
        // freed frame can be reused.
        let mut any_unmapped = false;
        while vaddr < end_vaddr {
            let page_size = match self.unmap_no_shootdown(vaddr) {
                Ok((_, s)) => {
                    assert!(s.is_aligned(vaddr));
                    any_unmapped = true;
                    s as usize
                }
                Err(PagingError::NotMapped) => PageSize::Size4K as usize,
                Err(e) => return Err(e),
            };
            vaddr += page_size;
            assert!(vaddr <= end_vaddr);
        }
        if any_unmapped {
            self.remote_flush_all();
        }
        Ok(())
    }
}

/// Which translation base a page-table root belongs in, where an architecture
/// has more than one — aarch64, whose kernel half is walked through `TTBR1_EL1`
/// and whose user half is walked through `TTBR0_EL1`.
///
/// `flagged_user` is what the caller said, by OR-ing `USER_TABLE_FLAG` into the
/// token it passed to `activate_paging`. It is the **last** thing consulted,
/// because a caller that forgets it is not a caller that gets a slower kernel:
/// on aarch64 the unflagged path writes `TTBR1_EL1`, so a user root arriving
/// without the flag replaces the base register the kernel's own text, stack and
/// page tables are translated through — with a table that maps none of them.
/// The next instruction fetch faults, at a point where the vectors it would
/// need are themselves unreachable. `zircon-object`'s IRELATIVE resolver
/// (`elf_loader.rs`, the ifunc batch) passes the root of a user VMAR with no
/// flag, through the same one-argument entry point the flagged call sites use.
///
/// So the root decides first, and it can: a root that **is** the kernel's own
/// is the kernel table, and any other root is a user table. Only while no
/// kernel root has been published — early boot, before `pin_kernel_vmtoken` —
/// is there nothing to compare against, and there the caller's word is all
/// there is.
///
/// Compared on the frame base for the reason
/// [`crate::common::ipi::aspace_filter`] gives.
pub fn is_user_table_root(root: PhysAddr, kernel_root: PhysAddr, flagged_user: bool) -> bool {
    if kernel_root == 0 {
        return flagged_user;
    }
    root & !0xfff != kernel_root & !0xfff
}

/// Whether this CPU still holds a user page table that the lazy-TLB restore
/// point has to drop.
///
/// The kernel keeps the process page table loaded after a poll instead of
/// reloading the kernel root every time, because that reload is a TLB flush
/// and it dominated syscall- and yield-heavy workloads. What makes that safe
/// is the promise that the user root is dropped before the CPU can idle, so
/// it cannot sit holding translations for an address space a concurrent
/// process exit is about to free. `activate_kernel_paging`, called from the
/// executor's idle callback, is where that promise is kept.
///
/// `loaded_user_root` is whatever this CPU's hardware says the user half is
/// being translated through: `CR3` on x86_64 and `satp`'s root on riscv64,
/// where one register holds both roots and loading the kernel root evicts the
/// user one; `TTBR0_EL1` on aarch64, where it does not. **That difference is
/// the whole reason this is a shared function.** aarch64 asked the question of
/// `current_vmtoken()`, which reads `TTBR1_EL1` — the register the kernel's
/// own half is translated through, which nothing but the kernel root is ever
/// written to. The answer was therefore "no" on every call after boot, and the
/// restore point had been doing nothing at all: an idle aarch64 core kept
/// `TTBR0_EL1` pointing at a page table any other core was free to free.
///
/// A CPU with nothing loaded has nothing to drop, and while no kernel root has
/// been published there is nothing to restore *to*, so neither is a restore.
/// Compared on the frame base for the reason [`crate::common::ipi::aspace_filter`]
/// gives.
pub fn should_restore_kernel_table(loaded_user_root: PhysAddr, kernel_root: PhysAddr) -> bool {
    kernel_root != 0
        && loaded_user_root & !0xfff != 0
        && loaded_user_root & !0xfff != kernel_root & !0xfff
}

/// Every [`PageSize`] is used as a mask: `align_down` and `page_offset` do
/// `addr & !(size - 1)` and `addr & (size - 1)`, which is only the intended
/// arithmetic while each value is a power of two. A new variant that is not
/// would not fail here — it would silently align to the wrong boundary and map
/// a page over its neighbour. Checked at compile time rather than in a test,
/// because it is a property of the enum.
const _: () = {
    assert!((PageSize::Size4K as usize).is_power_of_two());
    assert!((PageSize::Size2M as usize).is_power_of_two());
    assert!((PageSize::Size1G as usize).is_power_of_two());
    assert!((PageSize::Size4K as usize) == 4096);
};

/// `map_cont` decides, per step, which of the three page sizes to use, and a
/// wrong answer there is not a crash: it maps a larger page than the caller
/// asked for, silently covering the next mapping's range. It had no tests.
///
/// The fake table below records what it was asked to map instead of touching
/// hardware, which is all these rules need — the whole decision is alignment
/// and how much is left.
#[cfg(test)]
mod page_size_tests {
    use super::*;
    use alloc::vec::Vec;

    const K: usize = PageSize::Size4K as usize;
    const M: usize = PageSize::Size2M as usize;
    const G: usize = PageSize::Size1G as usize;

    // ── the mask arithmetic ────────────────────────────────────────────────

    #[test]
    fn align_down_and_page_offset_split_an_address_in_two() {
        for size in [PageSize::Size4K, PageSize::Size2M, PageSize::Size1G] {
            let s = size as usize;
            for addr in [0, 1, s - 1, s, s + 1, 3 * s + 7, usize::MAX - 4096] {
                assert_eq!(
                    size.align_down(addr) + size.page_offset(addr),
                    addr,
                    "{:?} split {:#x}",
                    size,
                    addr
                );
                assert!(size.is_aligned(size.align_down(addr)));
                assert_eq!(size.is_aligned(addr), size.page_offset(addr) == 0);
            }
        }
    }

    #[test]
    fn only_2m_and_1g_are_huge() {
        assert!(!PageSize::Size4K.is_huge());
        assert!(PageSize::Size2M.is_huge());
        assert!(PageSize::Size1G.is_huge());
        assert_eq!(BASE_PAGE_SIZE, PageSize::Size4K);
    }

    #[test]
    fn a_2m_aligned_address_is_not_necessarily_1g_aligned() {
        // The three alignments are nested one way only, and `map_cont` relies
        // on asking about the largest first.
        assert!(PageSize::Size4K.is_aligned(M));
        assert!(PageSize::Size2M.is_aligned(M));
        assert!(!PageSize::Size1G.is_aligned(M));
        assert!(PageSize::Size4K.is_aligned(G));
        assert!(PageSize::Size2M.is_aligned(G));
        assert!(PageSize::Size1G.is_aligned(G));
    }

    // ── a table that only remembers ────────────────────────────────────────

    #[derive(Default)]
    struct RecordingTable {
        mapped: Vec<(VirtAddr, PhysAddr, PageSize)>,
        /// Sizes `unmap` should report, popped in order; `Size4K` once empty.
        unmap_sizes: Vec<PageSize>,
        unmapped: Vec<VirtAddr>,
        flushes: usize,
    }

    impl GenericPageTable for RecordingTable {
        fn table_phys(&self) -> PhysAddr {
            0x1000
        }
        fn map(&mut self, page: Page, paddr: PhysAddr, _flags: MMUFlags) -> PagingResult {
            self.mapped.push((page.vaddr, paddr, page.size));
            Ok(())
        }
        fn unmap(&mut self, vaddr: VirtAddr) -> PagingResult<(PhysAddr, PageSize)> {
            self.unmapped.push(vaddr);
            let size = if self.unmap_sizes.is_empty() {
                PageSize::Size4K
            } else {
                self.unmap_sizes.remove(0)
            };
            Ok((vaddr, size))
        }
        fn update(
            &mut self,
            _vaddr: VirtAddr,
            _paddr: Option<PhysAddr>,
            _flags: Option<MMUFlags>,
        ) -> PagingResult<PageSize> {
            Ok(PageSize::Size4K)
        }
        fn query(&self, _vaddr: VirtAddr) -> PagingResult<(PhysAddr, MMUFlags, PageSize)> {
            Err(PagingError::NotMapped)
        }
        fn remote_flush_all(&self) {
            // `&self`, so count through a cell the test reads back.
            FLUSHES.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        }
    }

    static FLUSHES: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

    fn flushes_during(f: impl FnOnce()) -> usize {
        use core::sync::atomic::Ordering::SeqCst;
        let before = FLUSHES.load(SeqCst);
        f();
        FLUSHES.load(SeqCst) - before
    }

    fn sizes(t: &RecordingTable) -> Vec<usize> {
        t.mapped.iter().map(|(_, _, s)| *s as usize).collect()
    }

    // ── choosing a page size ───────────────────────────────────────────────

    #[test]
    fn without_the_huge_flag_everything_is_a_4k_page() {
        let mut t = RecordingTable::default();
        t.map_cont(G, 2 * M, G, MMUFlags::READ).unwrap();
        assert_eq!(t.mapped.len(), 2 * M / K);
        assert!(sizes(&t).iter().all(|&s| s == K), "no huge page may appear");
    }

    #[test]
    fn a_huge_mapping_takes_the_largest_page_that_fits_and_is_aligned() {
        let mut t = RecordingTable::default();
        t.map_cont(G, G + 2 * M, G, MMUFlags::HUGE_PAGE).unwrap();
        assert_eq!(sizes(&t), alloc::vec![G, M, M]);
        // And it does not reach for a gigabyte it has not got: one byte less
        // than 1 GiB of range, however well aligned, is 2 MiB pages.
        let mut t = RecordingTable::default();
        t.map_cont(G, G - M, G, MMUFlags::HUGE_PAGE).unwrap();
        assert_eq!(t.mapped.len(), (G - M) / M);
        assert!(sizes(&t).iter().all(|&s| s == M));
    }

    #[test]
    fn a_range_that_starts_unaligned_climbs_up_to_the_big_pages() {
        // 4 KiB short of a 2 MiB boundary, then a full gigabyte: the head must
        // be 4 KiB pages until the address is aligned, and only then grow.
        let start = G - K;
        let mut t = RecordingTable::default();
        t.map_cont(start, K + G, start, MMUFlags::HUGE_PAGE)
            .unwrap();
        assert_eq!(sizes(&t), alloc::vec![K, G]);
    }

    #[test]
    fn a_tail_too_short_for_a_huge_page_is_mapped_in_4k() {
        let mut t = RecordingTable::default();
        t.map_cont(M, M + 3 * K, M, MMUFlags::HUGE_PAGE).unwrap();
        assert_eq!(sizes(&t), alloc::vec![M, K, K, K]);
    }

    #[test]
    fn the_physical_side_has_a_vote_too() {
        // A 2 MiB-aligned virtual address whose frame is not 2 MiB-aligned
        // cannot be a huge page: one PTE cannot describe that pairing, and
        // taking it anyway would map the wrong memory.
        let mut t = RecordingTable::default();
        t.map_cont(M, M, M + K, MMUFlags::HUGE_PAGE).unwrap();
        assert_eq!(t.mapped.len(), M / K);
        assert!(sizes(&t).iter().all(|&s| s == K));
    }

    #[test]
    fn every_page_is_mapped_once_and_covers_the_range_exactly() {
        let mut t = RecordingTable::default();
        let start = 2 * G - M;
        t.map_cont(start, M + G + 4 * K, start, MMUFlags::HUGE_PAGE)
            .unwrap();
        let mut expect = start;
        for (vaddr, paddr, size) in &t.mapped {
            assert_eq!(*vaddr, expect, "a gap or an overlap at {:#x}", expect);
            assert_eq!(*paddr, expect, "physical side drifted from virtual");
            assert!(size.is_aligned(*vaddr), "{:?} page at {:#x}", size, vaddr);
            expect += *size as usize;
        }
        assert_eq!(expect, start + M + G + 4 * K, "the range was not covered");
    }

    #[test]
    fn an_empty_range_maps_nothing() {
        let mut t = RecordingTable::default();
        t.map_cont(G, 0, G, MMUFlags::HUGE_PAGE).unwrap();
        assert!(t.mapped.is_empty());
    }

    // ── unmapping a range ──────────────────────────────────────────────────

    #[test]
    fn unmapping_a_range_shoots_the_other_cpus_down_once_not_per_page() {
        // The mmu-gather point: a synchronous shootdown per page turned a
        // large `munmap` into a livelock against a peer that cannot ack
        // promptly. One round for the whole range.
        let mut t = RecordingTable::default();
        let n = flushes_during(|| t.unmap_cont(0, 64 * K).unwrap());
        assert_eq!(t.unmapped.len(), 64);
        assert_eq!(n, 1, "exactly one cross-CPU flush for the whole range");
    }

    #[test]
    fn a_range_that_was_never_mapped_shoots_nobody_down() {
        struct Unmapped;
        impl GenericPageTable for Unmapped {
            fn table_phys(&self) -> PhysAddr {
                0
            }
            fn map(&mut self, _: Page, _: PhysAddr, _: MMUFlags) -> PagingResult {
                Ok(())
            }
            fn unmap(&mut self, _: VirtAddr) -> PagingResult<(PhysAddr, PageSize)> {
                Err(PagingError::NotMapped)
            }
            fn update(
                &mut self,
                _: VirtAddr,
                _: Option<PhysAddr>,
                _: Option<MMUFlags>,
            ) -> PagingResult<PageSize> {
                Err(PagingError::NotMapped)
            }
            fn query(&self, _: VirtAddr) -> PagingResult<(PhysAddr, MMUFlags, PageSize)> {
                Err(PagingError::NotMapped)
            }
            fn remote_flush_all(&self) {
                FLUSHES.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
            }
        }
        let mut t = Unmapped;
        let n = flushes_during(|| t.unmap_cont(0, 16 * K).unwrap());
        assert_eq!(n, 0, "nothing was unmapped, so no TLB can be stale");
    }

    #[test]
    fn unmapping_advances_by_the_size_the_table_reports() {
        // A 2 MiB mapping must consume 2 MiB of the walk, not 4 KiB, or the
        // loop visits the same huge page 512 times.
        let mut t = RecordingTable {
            unmap_sizes: alloc::vec![PageSize::Size2M],
            ..Default::default()
        };
        t.unmap_cont(0, M + 2 * K).unwrap();
        assert_eq!(t.unmapped, alloc::vec![0, M, M + K]);
    }

    #[test]
    fn a_failure_that_is_not_not_mapped_stops_the_walk() {
        struct NoMemory(usize);
        impl GenericPageTable for NoMemory {
            fn table_phys(&self) -> PhysAddr {
                0
            }
            fn map(&mut self, _: Page, _: PhysAddr, _: MMUFlags) -> PagingResult {
                Ok(())
            }
            fn unmap(&mut self, _: VirtAddr) -> PagingResult<(PhysAddr, PageSize)> {
                self.0 += 1;
                Err(PagingError::NoMemory)
            }
            fn update(
                &mut self,
                _: VirtAddr,
                _: Option<PhysAddr>,
                _: Option<MMUFlags>,
            ) -> PagingResult<PageSize> {
                Err(PagingError::NotMapped)
            }
            fn query(&self, _: VirtAddr) -> PagingResult<(PhysAddr, MMUFlags, PageSize)> {
                Err(PagingError::NotMapped)
            }
        }
        let mut t = NoMemory(0);
        assert!(t.unmap_cont(0, 16 * K).is_err());
        assert_eq!(t.0, 1, "the walk stops at the first real error");
    }

    // ── ignoring the one error that is not one ─────────────────────────────

    #[test]
    fn ignore_swallows_not_mapped_and_nothing_else() {
        let ok: PagingResult<u32> = Ok(7);
        assert!(ok.ignore().is_ok());
        let missing: PagingResult<u32> = Err(PagingError::NotMapped);
        assert!(missing.ignore().is_ok());
        for e in [PagingError::NoMemory, PagingError::AlreadyMapped] {
            let err: PagingResult<u32> = Err(e);
            assert!(err.ignore().is_err(), "only NotMapped is ignorable");
        }
    }
}

/// Which of aarch64's two translation bases a page-table root belongs in. The
/// answer used to be a flag the caller OR-ed into the token, and one caller
/// does not.
#[cfg(test)]
mod translation_base_tests {
    use super::*;

    const KERNEL: usize = 0x4_1000;
    const USER: usize = 0x9_2000;

    #[test]
    fn the_kernel_s_own_root_is_the_kernel_table_however_it_was_labelled() {
        assert!(!is_user_table_root(KERNEL, KERNEL, false));
        // And a caller that labelled it user does not get the kernel's own
        // tables moved into TTBR0.
        assert!(!is_user_table_root(KERNEL, KERNEL, true));
    }

    #[test]
    fn any_other_root_is_a_user_table_even_when_the_caller_forgot_to_say_so() {
        // This is the whole point: the unflagged call in the IRELATIVE
        // resolver used to put a user root into the register the kernel's own
        // half is translated through.
        assert!(is_user_table_root(USER, KERNEL, false));
        assert!(is_user_table_root(USER, KERNEL, true));
    }

    #[test]
    fn with_no_kernel_root_published_the_caller_s_word_is_all_there_is() {
        // Early boot, before `pin_kernel_vmtoken`: nothing to compare against.
        assert!(!is_user_table_root(KERNEL, 0, false));
        assert!(!is_user_table_root(USER, 0, false));
        assert!(is_user_table_root(USER, 0, true));
    }

    #[test]
    fn the_low_twelve_bits_do_not_make_a_root_another_table() {
        assert!(!is_user_table_root(KERNEL | 0xfff, KERNEL, true));
        assert!(!is_user_table_root(KERNEL, KERNEL | 0xabc, true));
        // One frame along is a different table, flag or no flag.
        assert!(is_user_table_root(KERNEL + 0x1000, KERNEL, false));
    }
}

/// The lazy-TLB restore point: a CPU that ran userspace must drop the user
/// page table before it can idle, and the three architectures were asking
/// three different registers whether it still had one.
#[cfg(test)]
mod lazy_tlb_restore_tests {
    use super::*;

    const KERNEL: usize = 0x4_1000;
    const USER: usize = 0x9_2000;

    #[test]
    fn a_cpu_that_still_holds_a_user_table_has_to_drop_it() {
        assert!(should_restore_kernel_table(USER, KERNEL));
    }

    #[test]
    fn a_cpu_already_back_on_the_kernel_table_pays_nothing() {
        // The idle callback runs on EVERY idle iteration, so the second one
        // and all the rest must cost a register read and no flush.
        assert!(!should_restore_kernel_table(KERNEL, KERNEL));
    }

    #[test]
    fn a_cpu_with_no_user_table_loaded_has_nothing_to_drop() {
        // aarch64 after the drop, and after `vm::init`, leaves TTBR0_EL1 at
        // zero. Asking again must not flush again.
        assert!(!should_restore_kernel_table(0, KERNEL));
    }

    #[test]
    fn nothing_is_restored_before_a_kernel_root_has_been_published() {
        // Early boot, before `pin_kernel_vmtoken`: there is no root to go
        // back to, so dropping the one that is loaded would leave the CPU
        // translating through nothing.
        assert!(!should_restore_kernel_table(USER, 0));
        assert!(!should_restore_kernel_table(0, 0));
    }

    #[test]
    fn the_low_twelve_bits_do_not_make_a_restore_necessary() {
        // The registers carry more than the frame base -- an ASID, table
        // walk attributes -- and none of it names another address space.
        assert!(!should_restore_kernel_table(KERNEL | 0xfff, KERNEL));
        assert!(!should_restore_kernel_table(KERNEL, KERNEL | 0xabc));
        assert!(!should_restore_kernel_table(0xfff, KERNEL));
        // One frame along really is another table.
        assert!(should_restore_kernel_table(KERNEL + 0x1000, KERNEL));
    }

    #[test]
    fn a_bring_up_table_that_is_neither_root_is_dropped_like_any_other() {
        // aarch64's secondaries reach compiled Rust with the identity table
        // the trampoline used to keep the PC valid across the MMU-enable step
        // still in TTBR0_EL1. It belongs to no process and it is not the
        // kernel's, and nothing had ever dropped it.
        const TRAMPOLINE: usize = 0x2_3000;
        assert!(should_restore_kernel_table(TRAMPOLINE, KERNEL));
    }

    #[test]
    fn the_answer_agrees_with_which_base_register_the_root_belongs_in() {
        // The two questions are asked of the same pair of roots on opposite
        // sides of a thread's life: there is a user table to drop exactly
        // when the root loaded is one `activate_paging` would have put in the
        // user base register.
        for root in [KERNEL, USER, KERNEL + 0x1000, 0x1000] {
            assert_eq!(
                should_restore_kernel_table(root, KERNEL),
                is_user_table_root(root, KERNEL, false),
                "root {root:#x}",
            );
        }
    }
}
