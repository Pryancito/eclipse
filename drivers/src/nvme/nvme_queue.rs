use alloc::slice;
use core::marker::PhantomData;
use volatile::Volatile;

use super::NvmeCommonCommand;
use super::NvmeCompletion;
use crate::{DeviceError, DeviceResult};

#[derive(Debug)]
pub struct NvmeQueue<P: Provider> {
    provider: PhantomData<P>,

    pub sq: &'static mut [Volatile<NvmeCommonCommand>],
    pub cq: &'static mut [Volatile<NvmeCompletion>],

    pub qid: usize,

    pub cq_head: usize,

    pub cq_phase: usize,

    pub sq_tail: usize,

    /// Per-queue command identifier counter (NVMe requires unique CIDs among
    /// outstanding commands; we run one command at a time per queue).
    pub cid_counter: u16,

    pub sq_pa: usize,

    pub cq_pa: usize,

    /// Page-rounded byte lengths of the two rings. Kept because giving a DMA
    /// region back needs the size it was taken with, and `q_size * 64` is not
    /// that size once it has been rounded up to a page.
    sq_len: usize,
    cq_len: usize,

    /// DMA bounce buffer backing this queue's data transfers.
    pub data_pa: usize,
    pub data_va: usize,
    pub data_len: usize,

    /// One DMA page used as the command's PRP list when a transfer spans more
    /// than two pages (PRP1 = page 0, PRP2 -> this list with the remaining
    /// page addresses). 512 u64 entries fit; the 32-page bounce needs 31.
    pub prp_list_pa: usize,
    pub prp_list_va: usize,
}

impl<P: Provider> NvmeQueue<P> {
    /// Build a queue and the four DMA regions it runs on.
    ///
    /// Fallible, because every one of those four can fail and the old code
    /// could not say so: it took whatever `alloc_dma` answered, and when the
    /// kernel has no contiguous run of pages that is the physical address `0`.
    /// `phys_to_virt` made a pointer out of it and the zeroing below wrote
    /// through it; the queue then ran with its rings at physical zero.
    pub fn new(qid: usize, q_size: usize) -> DeviceResult<Self> {
        if q_size == 0 {
            // `from_raw_parts_mut` would build two empty slices and every
            // index into them is then out of bounds. `controller_window`
            // refuses a queue this small already; this is the belt to that.
            return Err(DeviceError::InvalidParam);
        }
        // SQ: 64 bytes per entry. CQ: 16 bytes per entry.
        let sq_bytes = q_size * 64;
        let cq_bytes = q_size * 16;

        // Round up to page size
        let sq_pages = sq_bytes.div_ceil(P::PAGE_SIZE);
        let cq_pages = cq_bytes.div_ceil(P::PAGE_SIZE);

        // 32 pages (128 KiB), not 2: with a 2-page bounce every 1 MiB
        // read-ahead window became 128 serialized 8 KiB commands — enough
        // per-command latency to leave NVMe SLOWER than SATA. 128 KiB per
        // command needs a PRP list (below); `io_rw` still clamps each command
        // to the controller's advertised MDTS.
        let data_len = P::PAGE_SIZE * 32;
        let sq_len = sq_pages * P::PAGE_SIZE;
        let cq_len = cq_pages * P::PAGE_SIZE;

        let regions = [
            (P::alloc_dma(data_len), data_len),
            (P::alloc_dma(P::PAGE_SIZE), P::PAGE_SIZE),
            (P::alloc_dma(sq_len), sq_len),
            (P::alloc_dma(cq_len), cq_len),
        ];

        // All four or none. This is a probe, and one that gives up holding
        // three of them makes the retry likelier to fail than the attempt that
        // just did: 128 KiB of CONTIGUOUS pages is the hardest ask in here.
        // Until now nothing in the tree called `dealloc_dma` at all.
        if regions.iter().any(|((_, pa), _)| *pa == 0) {
            for ((va, pa), len) in regions {
                if pa != 0 {
                    P::dealloc_dma(va, len);
                }
            }
            return Err(DeviceError::DmaError);
        }

        let [((data_va, data_pa), _), ((prp_list_va, prp_list_pa), _), ((sq_va, sq_pa), _), ((cq_va, cq_pa), _)] =
            regions;

        trace!(
            "data_va: {:x}, sq_pa: {:x}, cq_pa: {:x}",
            data_va,
            sq_pa,
            cq_pa
        );

        // The zeroing the completion queue needs -- so phase-bit polling does
        // not read stale memory as a valid completion -- happens inside
        // `alloc_dma` now, together with the writeback that makes it visible
        // to the controller. Doing it here with a plain `write_bytes` left
        // those lines DIRTY in cache, and the first `clflush_range` in
        // `wait_cq` then writes them back over a completion the controller has
        // already DMA'd in. `DmaRegion` carries the same warning.

        // Safety: the two slices alias DMA regions this queue owns and gives
        // back in `Drop`, so they do not outlive it despite the `'static`.
        let submit_queue =
            unsafe { slice::from_raw_parts_mut(sq_va as *mut Volatile<NvmeCommonCommand>, q_size) };

        let complete_queue =
            unsafe { slice::from_raw_parts_mut(cq_va as *mut Volatile<NvmeCompletion>, q_size) };

        Ok(NvmeQueue {
            provider: PhantomData,
            sq: submit_queue,
            cq: complete_queue,
            qid,
            cq_head: 0,
            cq_phase: 1, // Phase starts at 1
            sq_tail: 0,
            cid_counter: 0,
            sq_pa,
            cq_pa,
            sq_len,
            cq_len,
            data_pa,
            data_va,
            data_len,
            prp_list_pa,
            prp_list_va,
        })
    }

    pub fn next_cid(&mut self) -> u16 {
        self.cid_counter = self.cid_counter.wrapping_add(1);
        self.cid_counter
    }
}

/// Give the four regions back.
///
/// There was no `Drop` at all, so `dealloc_dma` -- the other half of the
/// `Provider` trait -- had no caller anywhere in the tree, and a queue that
/// went away took 128 KiB of contiguous DMA plus its rings and its PRP list
/// with it. `utils::dma::DmaRegion` next door has had this since it was
/// written; this file allocates by hand and so has none of what it does.
impl<P: Provider> Drop for NvmeQueue<P> {
    fn drop(&mut self) {
        P::dealloc_dma(self.data_va, self.data_len);
        P::dealloc_dma(self.prp_list_va, P::PAGE_SIZE);
        P::dealloc_dma(self.sq.as_ptr() as usize, self.sq_len);
        P::dealloc_dma(self.cq.as_ptr() as usize, self.cq_len);
    }
}

/// External functions that drivers must use
pub trait Provider {
    /// Page size (usually 4K)
    const PAGE_SIZE: usize;

    /// Allocate consequent physical memory for DMA, zeroed and pushed out of
    /// the cache. Returns (`virtual address`, `physical address`), page
    /// aligned.
    ///
    /// **`(0, 0)` means it could not.** An `Option` would say that better, but
    /// this trait has a second consumer outside this module (`audio::hda`)
    /// whose call sites are not ours to change, and the physical address `0`
    /// is already how the kernel hook underneath reports the same thing. What
    /// matters is that callers check it: the one in here did not, and built a
    /// pointer out of `phys_to_virt(0)`.
    fn alloc_dma(size: usize) -> (usize, usize);

    /// Deallocate DMA. `size` is the byte length that was asked for, not a
    /// page count.
    fn dealloc_dma(vaddr: usize, size: usize);
}

pub struct ProviderImpl;

impl Provider for ProviderImpl {
    const PAGE_SIZE: usize = PAGE_SIZE;

    fn alloc_dma(size: usize) -> (usize, usize) {
        if size == 0 {
            return (0, 0);
        }
        // `div_ceil`, not `/`. Truncating division backs a request smaller
        // than a page with ZERO pages and a 5000-byte one with a single
        // 4096-byte page, and the caller then writes past what it was given.
        // The same line in `net::ProviderImpl` -- the other copy of this very
        // trait, a directory away -- was fixed for exactly that, and this one,
        // which also backs the HDA driver's rings, was not.
        let pages = size.div_ceil(PAGE_SIZE);
        let paddr = unsafe { drivers_dma_alloc(pages) };
        if paddr == 0 {
            return (0, 0);
        }
        if paddr & (PAGE_SIZE - 1) != 0 {
            // A submission queue base is read by the controller with its low
            // bits ignored, so a misaligned answer would silently put the ring
            // somewhere other than where we think it is.
            unsafe { drivers_dma_dealloc(paddr, pages) };
            return (0, 0);
        }
        let vaddr = phys_to_virt(paddr);
        let len = pages * PAGE_SIZE;
        // Recycled frames come back dirty, and a completion queue is read by
        // phase bit: one stale byte with that bit set is a completion the
        // controller never posted.
        unsafe { core::ptr::write_bytes(vaddr as *mut u8, 0, len) };
        // And push those zeros to RAM before the device writes there, or the
        // first `clflush` of that line writes them back over what it wrote.
        crate::utils::dma_sync::dma_sync_wb_to_device(vaddr, len);
        (vaddr, paddr)
    }

    fn dealloc_dma(vaddr: usize, size: usize) {
        let paddr = virt_to_phys(vaddr);
        unsafe { drivers_dma_dealloc(paddr, size.div_ceil(PAGE_SIZE)) };
    }
}

pub fn phys_to_virt(paddr: PhysAddr) -> VirtAddr {
    unsafe { drivers_phys_to_virt(paddr) }
}

pub fn virt_to_phys(vaddr: VirtAddr) -> PhysAddr {
    unsafe { drivers_virt_to_phys(vaddr) }
}

pub fn timer_now_as_micros() -> u64 {
    unsafe { drivers_timer_now_as_micros() }
}

/// The clock behind [`timer_now_as_micros`] in the test binary: one per
/// thread, moved only by the test (and by the driver's own waits), so a
/// driver's timing decisions -- a poll throttle, a verb timeout, an idle
/// stop -- can be driven to the microsecond and the tests still run in
/// parallel. The `drivers_timer_now_as_micros` shim in `net::e1000e`
/// reads it; it starts at 0, which is what every test before this one saw.
#[cfg(test)]
pub mod test_clock {
    extern crate std;
    use core::cell::Cell;

    std::thread_local! {
        static NOW_US: Cell<u64> = const { Cell::new(0) };
        /// Microseconds the clock moves on every read, for code that polls
        /// it until a deadline (a fence wait, a syncobj wait): with the
        /// default 0 such a loop on an unsatisfied condition never ends.
        static AUTO_ADVANCE_US: Cell<u64> = const { Cell::new(0) };
    }

    pub fn now() -> u64 {
        let step = AUTO_ADVANCE_US.with(|c| c.get());
        NOW_US.with(|c| {
            let v = c.get();
            c.set(v.wrapping_add(step));
            v
        })
    }

    /// Makes every `now()` advance the clock by `us` (0 stops it again).
    pub fn set_auto_advance(us: u64) {
        AUTO_ADVANCE_US.with(|c| c.set(us));
    }

    pub fn set(us: u64) {
        NOW_US.with(|c| c.set(us));
    }

    pub fn advance(us: u64) {
        NOW_US.with(|c| c.set(c.get().wrapping_add(us)));
    }
}

unsafe extern "C" {
    fn drivers_dma_alloc(pages: usize) -> PhysAddr;
    fn drivers_dma_dealloc(paddr: PhysAddr, pages: usize) -> i32;
    fn drivers_phys_to_virt(paddr: PhysAddr) -> VirtAddr;
    fn drivers_virt_to_phys(vaddr: VirtAddr) -> PhysAddr;
    fn drivers_timer_now_as_micros() -> u64;
}

pub const PAGE_SIZE: usize = 4096;

type VirtAddr = usize;
type PhysAddr = usize;

#[cfg(test)]
mod queue_tests {
    //! What a queue costs, and what happens when the kernel cannot pay it.
    //!
    //! Four DMA regions come out of `NvmeQueue::new` -- a 128 KiB bounce
    //! buffer, a PRP list page and the two rings -- and every one of them can
    //! fail. None of that was checked and none of it was ever given back:
    //! `dealloc_dma` had no caller in the tree. `utils::dma::DmaRegion`, one
    //! directory away, does all of it; this file allocates by hand.

    use super::*;
    use crate::utils::host_hooks as shim;
    use core::sync::atomic::Ordering;

    /// The shims keep process-wide state, so a test that touches them must be
    /// the only one doing so. CI runs this suite with `--test-threads=1`,
    /// which would hide a missing turnstile; this does not rely on that.
    fn alone_with_the_allocator<R>(body: impl FnOnce() -> R) -> R {
        static TURNSTILE: crate::sync::Mutex<()> = crate::sync::Mutex::new(());
        let _guard = TURNSTILE.lock();
        shim::reset();
        let out = body();
        shim::reset();
        out
    }

    fn pages_asked() -> usize {
        shim::ALLOC_PAGES.load(Ordering::SeqCst)
    }

    /// Regions a queue of `q_size` entries takes: the bounce, the PRP list,
    /// and the two rings rounded up to whole pages.
    fn pages_for(q_size: usize) -> usize {
        32 + 1 + (q_size * 64).div_ceil(PAGE_SIZE) + (q_size * 16).div_ceil(PAGE_SIZE)
    }

    // ── what a byte length costs ────────────────────────────────────────────

    #[test]
    fn a_request_smaller_than_a_page_still_costs_a_whole_page() {
        alone_with_the_allocator(|| {
            // `size / PAGE_SIZE` asked the kernel for ZERO pages here, and the
            // caller then wrote into whatever came back. The sibling copy of
            // this trait in `net` was fixed for exactly this.
            let (va, pa) = ProviderImpl::alloc_dma(2048);
            assert_ne!(pa, 0, "a page is available");
            assert_ne!(va, 0);
            assert_eq!(pages_asked(), 1, "asked the kernel for one page, not none");
        });
    }

    #[test]
    fn a_request_that_straddles_a_page_boundary_costs_both() {
        alone_with_the_allocator(|| {
            ProviderImpl::alloc_dma(PAGE_SIZE + 1);
            assert_eq!(pages_asked(), 2, "rounds UP, never down");
        });
    }

    #[test]
    fn a_zero_length_request_is_refused_without_reaching_the_kernel() {
        alone_with_the_allocator(|| {
            assert_eq!(ProviderImpl::alloc_dma(0), (0, 0));
            assert_eq!(
                shim::ALLOC_CALLS.load(Ordering::SeqCst),
                0,
                "a refusal must not reach the allocator"
            );
        });
    }

    #[test]
    fn giving_a_region_back_returns_every_page_it_took() {
        alone_with_the_allocator(|| {
            let (va, _) = ProviderImpl::alloc_dma(2048);
            ProviderImpl::dealloc_dma(va, 2048);
            assert_eq!(
                shim::DEALLOC_PAGES.load(Ordering::SeqCst),
                1,
                "the same rounding on the way out, or the page is never freed"
            );
        });
    }

    // ── when the kernel says no ─────────────────────────────────────────────

    #[test]
    fn an_allocation_that_fails_is_reported_and_not_turned_into_a_pointer() {
        alone_with_the_allocator(|| {
            shim::FAIL_ALLOC.store(true, Ordering::SeqCst);
            // The physical address 0 is how the kernel says "no contiguous
            // pages". `phys_to_virt(0)` used to make a pointer out of it.
            assert_eq!(ProviderImpl::alloc_dma(PAGE_SIZE), (0, 0));
        });
    }

    #[test]
    fn a_queue_refuses_to_come_up_when_there_is_no_memory_for_it() {
        alone_with_the_allocator(|| {
            shim::FAIL_ALLOC.store(true, Ordering::SeqCst);
            let queue = NvmeQueue::<ProviderImpl>::new(0, 32);
            assert_eq!(queue.err(), Some(DeviceError::DmaError));
            assert_eq!(
                shim::DEALLOC_CALLS.load(Ordering::SeqCst),
                0,
                "nothing was taken, so nothing to give back"
            );
        });
    }

    #[test]
    fn a_queue_that_gets_three_of_its_four_regions_keeps_none_of_them() {
        alone_with_the_allocator(|| {
            // The bounce buffer is 128 KiB of CONTIGUOUS pages, the hardest
            // ask in this file. A probe that gives up holding three regions
            // makes the retry likelier to fail than the attempt that just did.
            shim::FAIL_ALLOC_AFTER.store(3, Ordering::SeqCst);
            let queue = NvmeQueue::<ProviderImpl>::new(0, 32);
            assert_eq!(queue.err(), Some(DeviceError::DmaError));
            assert_eq!(
                shim::DEALLOC_CALLS.load(Ordering::SeqCst),
                3,
                "the three that succeeded go back"
            );
        });
    }

    #[test]
    fn a_misaligned_answer_is_refused_and_handed_straight_back() {
        alone_with_the_allocator(|| {
            // A controller reads a ring base with its low bits ignored, so a
            // misaligned region would silently put the ring elsewhere.
            shim::MISALIGN_ALLOC.store(true, Ordering::SeqCst);
            assert_eq!(ProviderImpl::alloc_dma(PAGE_SIZE), (0, 0));
            assert_eq!(
                shim::DEALLOC_CALLS.load(Ordering::SeqCst),
                1,
                "refusing it is not the same as leaking it"
            );
        });
    }

    #[test]
    fn a_queue_of_no_entries_is_refused_before_anything_is_allocated() {
        alone_with_the_allocator(|| {
            // `from_raw_parts_mut(.., 0)` builds a ring every index into which
            // is out of bounds.
            let queue = NvmeQueue::<ProviderImpl>::new(0, 0);
            assert_eq!(queue.err(), Some(DeviceError::InvalidParam));
            assert_eq!(shim::ALLOC_CALLS.load(Ordering::SeqCst), 0);
        });
    }

    // ── what the controller reads ───────────────────────────────────────────

    #[test]
    fn a_region_comes_back_zeroed_even_when_the_frame_was_dirty() {
        alone_with_the_allocator(|| {
            // Recycled frames come back dirty, and the completion queue is
            // read by phase bit: one stale byte with that bit set is a
            // completion the controller never posted.
            shim::POISON_ALLOC.store(true, Ordering::SeqCst);
            let (va, pa) = ProviderImpl::alloc_dma(PAGE_SIZE * 2);
            assert_ne!(pa, 0);
            let bytes = unsafe { core::slice::from_raw_parts(va as *const u8, PAGE_SIZE * 2) };
            assert!(bytes.iter().all(|b| *b == 0), "every byte of both pages");
        });
    }

    #[test]
    fn a_fresh_completion_queue_holds_no_completion() {
        alone_with_the_allocator(|| {
            shim::POISON_ALLOC.store(true, Ordering::SeqCst);
            let queue = NvmeQueue::<ProviderImpl>::new(0, 32).expect("a queue");
            // Phase starts at 1 and an entry matches when its status bit 0
            // equals the phase, so a dirty ring answers `wait_cq` immediately
            // with a completion for a command nobody submitted.
            assert!(
                queue
                    .cq
                    .iter()
                    .all(|e| (e.read().status & 1) as usize != queue.cq_phase),
                "not one entry may look like a completion"
            );
        });
    }

    // ── the rings a queue asks for ──────────────────────────────────────────

    #[test]
    fn a_queue_takes_its_bounce_its_list_and_its_two_rings_and_no_more() {
        alone_with_the_allocator(|| {
            let queue = NvmeQueue::<ProviderImpl>::new(1, 128).expect("a queue");
            assert_eq!(shim::ALLOC_CALLS.load(Ordering::SeqCst), 4);
            assert_eq!(pages_asked(), pages_for(128));
            assert_eq!(queue.data_len, PAGE_SIZE * 32, "128 KiB, not 8 KiB");
            assert_eq!(queue.sq.len(), 128);
            assert_eq!(queue.cq.len(), 128);
            assert_eq!(queue.qid, 1);
        });
    }

    #[test]
    fn every_ring_entry_is_inside_the_memory_that_was_taken_for_it() {
        alone_with_the_allocator(|| {
            // A submission entry is 64 bytes, so 2 entries is 128 bytes and one
            // page; asking the kernel for the byte count rather than the page
            // count is how a ring ends up longer than its allocation.
            let queue = NvmeQueue::<ProviderImpl>::new(0, 2).expect("the smallest queue");
            let sq_end = queue.sq.as_ptr() as usize + queue.sq.len() * 64;
            let cq_end = queue.cq.as_ptr() as usize + queue.cq.len() * 16;
            assert!(sq_end <= queue.sq.as_ptr() as usize + PAGE_SIZE);
            assert!(cq_end <= queue.cq.as_ptr() as usize + PAGE_SIZE);
            assert_eq!(pages_asked(), pages_for(2));
        });
    }

    #[test]
    fn the_addresses_the_controller_is_given_are_the_rings_themselves() {
        alone_with_the_allocator(|| {
            // `phys_to_virt` is the identity in this binary, so these are the
            // same number -- which is exactly why a mix-up would go unseen on
            // the host and put the controller's DMA somewhere else on a board.
            let queue = NvmeQueue::<ProviderImpl>::new(0, 32).expect("a queue");
            assert_eq!(virt_to_phys(queue.sq.as_ptr() as usize), queue.sq_pa);
            assert_eq!(virt_to_phys(queue.cq.as_ptr() as usize), queue.cq_pa);
            assert_eq!(virt_to_phys(queue.data_va), queue.data_pa);
            assert_eq!(virt_to_phys(queue.prp_list_va), queue.prp_list_pa);
            for pa in [queue.sq_pa, queue.cq_pa, queue.data_pa, queue.prp_list_pa] {
                assert_eq!(pa & (PAGE_SIZE - 1), 0, "page aligned, as the trait says");
            }
        });
    }

    // ── giving it back ──────────────────────────────────────────────────────

    #[test]
    fn a_queue_that_goes_away_gives_all_four_regions_back() {
        alone_with_the_allocator(|| {
            // There was no `Drop` at all, so `dealloc_dma` had no caller
            // anywhere and a queue took 128 KiB of contiguous DMA with it.
            let queue = NvmeQueue::<ProviderImpl>::new(0, 32).expect("a queue");
            assert_eq!(
                shim::DEALLOC_CALLS.load(Ordering::SeqCst),
                0,
                "not while alive"
            );
            drop(queue);
            assert_eq!(shim::DEALLOC_CALLS.load(Ordering::SeqCst), 4);
            assert_eq!(
                shim::DEALLOC_PAGES.load(Ordering::SeqCst),
                pages_for(32),
                "every page it took, and the rings are rounded up on the way out too"
            );
        });
    }

    #[test]
    fn what_goes_back_is_what_came_out() {
        alone_with_the_allocator(|| {
            // Freeing by `q_size * 64` rather than by the page-rounded length
            // hands back fewer pages than were taken, which leaks the rest.
            let queue = NvmeQueue::<ProviderImpl>::new(0, 2).expect("a queue");
            let took = pages_asked();
            drop(queue);
            assert_eq!(shim::DEALLOC_PAGES.load(Ordering::SeqCst), took);
        });
    }

    // ── command identifiers ─────────────────────────────────────────────────

    #[test]
    fn each_command_gets_an_identifier_of_its_own() {
        alone_with_the_allocator(|| {
            let mut queue = NvmeQueue::<ProviderImpl>::new(0, 32).expect("a queue");
            let first = queue.next_cid();
            assert_eq!(queue.next_cid(), first.wrapping_add(1));
            // It wraps rather than overflowing: a panic here would come from
            // the completion path of a machine that had been up a while.
            queue.cid_counter = u16::MAX;
            assert_eq!(queue.next_cid(), 0);
        });
    }
}
