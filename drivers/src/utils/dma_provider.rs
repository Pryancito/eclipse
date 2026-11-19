//! The DMA memory, the physical/virtual translation and the microsecond
//! clock a PCI driver in the kernel gets from the kernel: [`Provider`] and
//! its one implementation, [`ProviderImpl`], over the `drivers_*` hooks.
//!
//! It was `nvme::nvme_queue`, next to the kernel's NVMe driver, and the HDA
//! driver and the test clock of half the crate reached it there. The NVMe
//! driver is gone (the controller is left to its driver in userspace,
//! `crate::pci_disks`); this stayed, and `crate::nvme::nvme_queue` is now
//! this module under its old name.

/// External functions that drivers must use
pub trait Provider {
    /// Page size (usually 4K)
    const PAGE_SIZE: usize;

    /// Allocate consequent physical memory for DMA, zeroed and pushed out of
    /// the cache. Returns (`virtual address`, `physical address`), page
    /// aligned.
    ///
    /// **`(0, 0)` means it could not.** An `Option` would say that better, but
    /// this trait has consumers (`audio::hda`) whose call sites are not ours
    /// to change, and the physical address `0` is already how the kernel hook
    /// underneath reports the same thing. What matters is that callers check
    /// it: the NVMe queue that used to live here did not, and built a pointer
    /// out of `phys_to_virt(0)`.
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
mod provider_tests {
    //! What a region of DMA memory costs, and what happens when the kernel
    //! cannot pay it.

    use super::*;
    use crate::utils::host_hooks as shim;
    use core::sync::atomic::Ordering;

    /// One turnstile for every module in the crate, next to the counters it
    /// guards: see `host_hooks::alone_with_the_allocator`.
    use shim::alone_with_the_allocator;

    fn pages_asked() -> usize {
        shim::ALLOC_PAGES.load(Ordering::SeqCst)
    }

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
    fn the_whole_page_a_caller_is_given_comes_back_clean() {
        alone_with_the_allocator(|| {
            // Half a page costs a whole page, so what the caller is handed is
            // a page -- and the zeroing has to cover the page, not the
            // request. The only test that poisons a region until now asked for
            // two pages exactly, where the two lengths are the same number.
            // A ring that ends mid-page leaves the rest of that page readable
            // by the device, and a completion queue is read by phase bit: one
            // stale byte there is a completion the controller never posted.
            shim::POISON_ALLOC.store(true, Ordering::SeqCst);
            let (va, pa) = ProviderImpl::alloc_dma(PAGE_SIZE / 2);
            assert_ne!(pa, 0, "a page is available");
            let page = unsafe { core::slice::from_raw_parts(va as *const u8, PAGE_SIZE) };
            assert!(
                page.iter().all(|b| *b == 0),
                "the tail of the page kept what its last owner left there"
            );
        });
    }
}
