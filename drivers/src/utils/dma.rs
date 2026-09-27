//! DMA memory region allocator.
//!
//! Provides page-aligned DMA-capable buffers backed by the kernel DMA allocator
//! (`drivers_dma_alloc`).  Physical and virtual addresses are tracked so that
//! hardware registers can be programmed with the physical address while the CPU
//! accesses data through the virtual address.

use crate::bus::PAGE_SIZE;

extern "C" {
    fn drivers_dma_alloc(pages: usize) -> usize;
    fn drivers_dma_dealloc(paddr: usize, pages: usize) -> i32;
    fn drivers_phys_to_virt(paddr: usize) -> usize;
    fn drivers_dma_mark_uncached(paddr: usize, pages: usize) -> i32;
    fn drivers_dma_verify_uncached(paddr: usize, pages: usize) -> i32;
    // NOTE: `drivers_dma_mark_uncached` has no counterpart. Nothing here can put
    // a page back to write-back, which is why `alloc_coherent` leaks instead of
    // freeing when the remap only half-succeeds.
}

/// A contiguous, page-aligned DMA memory region.
pub struct DmaRegion {
    virt: usize,
    phys: usize,
    pages: usize,
}

impl DmaRegion {
    /// Allocate `len` bytes of DMA-capable memory, zero-filled.
    pub fn alloc(len: usize) -> Option<Self> {
        Self::alloc_inner(len, true)
    }

    /// Allocate without zeroing. Use for RX buffers filled by device DMA: zeroing
    /// dirties the cache and breaks coherency on x86 unless mappings are UC.
    pub fn alloc_uninit(len: usize) -> Option<Self> {
        Self::alloc_inner(len, false)
    }

    fn alloc_inner(len: usize, zero: bool) -> Option<Self> {
        if len == 0 {
            return None;
        }
        let pages = len.div_ceil(PAGE_SIZE);
        let phys = unsafe { drivers_dma_alloc(pages) };
        if phys == 0 {
            return None;
        }
        if phys & (PAGE_SIZE - 1) != 0 {
            unsafe { drivers_dma_dealloc(phys, pages) };
            return None;
        }
        let virt = unsafe { drivers_phys_to_virt(phys) };
        if zero {
            unsafe { core::ptr::write_bytes(virt as *mut u8, 0, pages * PAGE_SIZE) };
        }
        // Evict any stale — possibly *dirty* — cache lines this physical memory
        // carried from its previous life (it is recycled from the frame
        // allocator and `alloc_uninit` does not zero it) BEFORE any device DMAs
        // into it. Otherwise a later `dma_sync(FromDevice)` clflush would write
        // such a dirty line back to RAM *over* the bytes the device just DMA'd
        // in — silent RX corruption that scales with the number of buffers
        // touched, so a large transfer (many buffers) fails while a small one
        // (few buffers) slips through. Writing the zeros/garbage back to RAM now
        // is harmless: the device overwrites it on the next receive. This also
        // covers the WB->UC transition in `map_coherent`, which does not flush
        // the cache itself. On non-x86 this is just a fence (those rely on UC
        // mappings); see `dma_sync`.
        crate::utils::dma_sync::dma_sync_wb_to_device(virt, pages * PAGE_SIZE);
        Some(Self { virt, phys, pages })
    }

    /// Virtual (CPU-accessible) base address of the region.
    #[inline]
    pub fn vaddr(&self) -> usize {
        self.virt
    }

    /// Physical (device-accessible) base address of the region.
    #[inline]
    pub fn paddr(&self) -> usize {
        self.phys
    }

    /// Size of the allocation in bytes (always page-rounded up).
    #[inline]
    pub fn byte_len(&self) -> usize {
        self.pages * PAGE_SIZE
    }

    /// Return a raw pointer to the start of the region cast to `*mut T`.
    #[inline]
    pub fn as_ptr<T>(&self) -> *mut T {
        self.virt as *mut T
    }

    /// Map this region uncacheable in the kernel page tables (bare-metal NIC DMA).
    pub fn mark_uncached(&self) -> bool {
        if self.phys == 0 {
            return false;
        }
        unsafe { drivers_dma_mark_uncached(self.phys, self.pages) == 0 }
    }

    /// Linux `dma_alloc_coherent` / FreeBSD `BUS_DMA_COHERENT`: map UC at alloc time.
    pub fn map_coherent(&self) -> bool {
        self.mark_uncached() && self.verify_uncached()
    }

    /// Allocate and map UC immediately (preferred for NIC rings — no WB fallback).
    pub fn alloc_coherent(len: usize) -> Option<Self> {
        let region = Self::alloc(len)?;
        // The two ways this can fail need OPPOSITE answers, so they are asked
        // separately rather than through `map_coherent`'s `&&`.
        if !region.mark_uncached() {
            // The remap never happened: the pages are still write-back, so
            // handing them back is both safe and right. Leaking here would buy
            // nothing.
            return None;
        }
        if region.verify_uncached() {
            return Some(region);
        }
        // The remap DID happen and the check then failed, so these pages are
        // uncacheable — and there is no FFI to put them back
        // (`drivers_dma_mark_uncached` has no counterpart). So LEAK them.
        //
        // Dropping the region here handed UC frames straight back to the frame
        // allocator, and the next caller to be given them ran ordinary kernel
        // memory with every access going to RAM: correct, invisible, and one to
        // two orders of magnitude slower, on memory with nothing to do with DMA.
        //
        // This runs once per NIC ring at bring-up and only when the remap half
        // succeeded, so the leak is a handful of pages, once. A bounded leak
        // beats poisoning the allocator.
        core::mem::forget(region);
        None
    }

    /// Like [`Self::alloc_uninit`] but returns `(region, coherent)` even if PAT remap fails.
    pub fn alloc_uninit_try_coherent(len: usize) -> Option<(Self, bool)> {
        let region = Self::alloc_uninit(len)?;
        let coherent = region.map_coherent();
        Some((region, coherent))
    }

    /// Returns true when every page in the region is mapped UC/UC- in the PTEs.
    pub fn verify_uncached(&self) -> bool {
        if self.phys == 0 {
            return false;
        }
        unsafe { drivers_dma_verify_uncached(self.phys, self.pages) == 0 }
    }
}

impl Drop for DmaRegion {
    fn drop(&mut self) {
        if self.phys != 0 {
            unsafe { drivers_dma_dealloc(self.phys, self.pages) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::host_hooks as shim;
    use core::sync::atomic::Ordering;

    /// One turnstile for every module in the crate, next to the counters it
    /// guards: see `host_hooks::alone_with_the_allocator`. A copy per module --
    /// which is what this was -- is three different locks over one set of
    /// process-wide counters, so none of them guards anything.
    use shim::alone_with_the_allocator;

    #[test]
    fn a_region_is_page_rounded_and_both_addresses_point_at_it() {
        alone_with_the_allocator(|| {
            let region = DmaRegion::alloc(1).expect("one byte is one page");
            assert_eq!(region.byte_len(), PAGE_SIZE, "a byte still costs a page");
            assert_eq!(region.paddr() & (PAGE_SIZE - 1), 0, "must be page-aligned");
            assert_eq!(region.vaddr(), region.paddr(), "identity phys_to_virt here");
            assert_eq!(region.as_ptr::<u8>() as usize, region.vaddr());

            let bigger = DmaRegion::alloc(PAGE_SIZE + 1).unwrap();
            assert_eq!(bigger.byte_len(), 2 * PAGE_SIZE, "rounds UP, never down");
        });
    }

    /// A zero-length allocation is refused rather than turned into an empty
    /// region: a device handed a zero-length ring has no ring.
    #[test]
    fn a_zero_length_allocation_is_refused_and_costs_nothing() {
        alone_with_the_allocator(|| {
            assert!(DmaRegion::alloc(0).is_none());
            assert!(DmaRegion::alloc_uninit(0).is_none());
            assert_eq!(
                shim::ALLOC_CALLS.load(Ordering::SeqCst),
                0,
                "a refusal must not reach the allocator"
            );
        });
    }

    #[test]
    fn alloc_zeroes_the_region_and_alloc_uninit_does_not() {
        alone_with_the_allocator(|| {
            // Recycled frames come back DIRTY, which is the whole point: with a
            // pre-zeroed allocator the zeroing below is unobservable and the test
            // proves nothing (a mutant that deletes it survives).
            shim::POISON_ALLOC.store(true, Ordering::SeqCst);

            let zeroed = DmaRegion::alloc(64).unwrap();
            let bytes =
                unsafe { core::slice::from_raw_parts(zeroed.as_ptr::<u8>(), zeroed.byte_len()) };
            assert!(
                bytes.iter().all(|b| *b == 0),
                "alloc must zero the region even when the frame comes back dirty"
            );

            // And `alloc_uninit` exists precisely not to pay for that.
            let raw = DmaRegion::alloc_uninit(64).unwrap();
            let bytes = unsafe { core::slice::from_raw_parts(raw.as_ptr::<u8>(), raw.byte_len()) };
            assert!(
                bytes.iter().any(|b| *b != 0),
                "alloc_uninit must NOT pay to zero a buffer the device overwrites"
            );
        });
    }

    #[test]
    fn an_allocator_that_says_no_is_not_second_guessed() {
        alone_with_the_allocator(|| {
            shim::FAIL_ALLOC.store(true, Ordering::SeqCst);
            assert!(DmaRegion::alloc(4096).is_none());
            assert_eq!(
                shim::DEALLOC_CALLS.load(Ordering::SeqCst),
                0,
                "nothing was allocated, so nothing may be freed"
            );
        });
    }

    /// A misaligned answer is refused AND given back. Every device in the tree
    /// programs this address into a ring-base register whose low bits are
    /// reserved, so an unaligned base is not a slow ring, it is a wrong one.
    #[test]
    fn a_misaligned_allocation_is_refused_and_handed_straight_back() {
        alone_with_the_allocator(|| {
            shim::MISALIGN_ALLOC.store(true, Ordering::SeqCst);
            assert!(DmaRegion::alloc(4096).is_none());
            assert_eq!(
                shim::DEALLOC_CALLS.load(Ordering::SeqCst),
                1,
                "a refused allocation must not be leaked"
            );
        });
    }

    #[test]
    fn a_region_is_handed_back_when_it_is_dropped() {
        alone_with_the_allocator(|| {
            {
                let _region = DmaRegion::alloc(3 * PAGE_SIZE).unwrap();
                assert_eq!(shim::DEALLOC_CALLS.load(Ordering::SeqCst), 0);
            }
            assert_eq!(shim::DEALLOC_CALLS.load(Ordering::SeqCst), 1);
            assert_eq!(
                shim::DEALLOC_PAGES.load(Ordering::SeqCst),
                3,
                "it must give back exactly the pages it took"
            );
        });
    }

    #[test]
    fn coherent_means_remapped_and_then_checked() {
        alone_with_the_allocator(|| {
            let region = DmaRegion::alloc_coherent(PAGE_SIZE).expect("both shims succeed");
            assert_eq!(shim::MARK_CALLS.load(Ordering::SeqCst), 1);
            assert_eq!(
                shim::VERIFY_CALLS.load(Ordering::SeqCst),
                1,
                "marking without verifying is how a WB ring gets mistaken for UC"
            );
            drop(region);
        });
    }

    /// The bug. `map_coherent` is `mark_uncached() && verify_uncached()`, so it
    /// can fail with the PTEs ALREADY flipped to UC -- and there is no FFI to
    /// flip them back. Freeing here returned uncacheable frames to the frame
    /// allocator, and the next caller to be handed them ran ordinary kernel
    /// memory with every access going to RAM: correct, invisible, and orders of
    /// magnitude slower, on memory with nothing to do with DMA.
    #[test]
    fn a_half_succeeded_remap_leaks_the_pages_instead_of_poisoning_the_allocator() {
        alone_with_the_allocator(|| {
            shim::FAIL_VERIFY.store(true, Ordering::SeqCst);
            assert!(DmaRegion::alloc_coherent(PAGE_SIZE).is_none());
            assert_eq!(
                shim::MARK_CALLS.load(Ordering::SeqCst),
                1,
                "the pages WERE remapped, which is the whole problem"
            );
            assert_eq!(
                shim::DEALLOC_CALLS.load(Ordering::SeqCst),
                0,
                "UC frames must NOT go back to the allocator"
            );
        });
    }

    /// The other half of the same decision: when the remap never happened, the
    /// pages are still write-back and freeing them is right. Leaking here would
    /// be a plain leak with nothing bought by it.
    #[test]
    fn a_remap_that_never_happened_still_gives_the_pages_back() {
        alone_with_the_allocator(|| {
            shim::FAIL_MARK.store(true, Ordering::SeqCst);
            assert!(DmaRegion::alloc_coherent(PAGE_SIZE).is_none());
            assert_eq!(shim::MARK_CALLS.load(Ordering::SeqCst), 1);
            assert_eq!(
                shim::VERIFY_CALLS.load(Ordering::SeqCst),
                0,
                "&& must short-circuit: no point verifying a remap that failed"
            );
            assert_eq!(
                shim::DEALLOC_CALLS.load(Ordering::SeqCst),
                1,
                "still write-back, so still safe to hand back"
            );
        });
    }

    /// `alloc_uninit_try_coherent` is the deliberate opposite: it keeps the
    /// region either way and TELLS the caller, who then chooses between UC
    /// accesses and explicit `dma_sync` calls. So a failed remap here must not
    /// throw the ring away.
    #[test]
    fn try_coherent_keeps_the_region_and_reports_what_it_got() {
        alone_with_the_allocator(|| {
            let (region, coherent) = DmaRegion::alloc_uninit_try_coherent(PAGE_SIZE).unwrap();
            assert!(coherent);
            drop(region);

            shim::reset();
            shim::FAIL_VERIFY.store(true, Ordering::SeqCst);
            let (region, coherent) = DmaRegion::alloc_uninit_try_coherent(PAGE_SIZE)
                .expect("a failed remap must not cost the caller its ring");
            assert!(!coherent, "and it must say so, so the caller can dma_sync");
            drop(region);
        });
    }

    #[test]
    fn verify_and_mark_refuse_outright_on_a_region_with_no_physical_address() {
        alone_with_the_allocator(|| {
            // The `phys == 0` guard: a region that never got memory must answer
            // no instead of asking the kernel about address zero.
            let region = DmaRegion {
                virt: 0,
                phys: 0,
                pages: 1,
            };
            assert!(!region.mark_uncached());
            assert!(!region.verify_uncached());
            assert_eq!(shim::MARK_CALLS.load(Ordering::SeqCst), 0);
            assert_eq!(shim::VERIFY_CALLS.load(Ordering::SeqCst), 0);
            core::mem::forget(region);
        });
    }

    /// And such a region must not try to free address zero when it drops.
    #[test]
    fn a_region_with_no_physical_address_frees_nothing() {
        alone_with_the_allocator(|| {
            drop(DmaRegion {
                virt: 0,
                phys: 0,
                pages: 1,
            });
            assert_eq!(shim::DEALLOC_CALLS.load(Ordering::SeqCst), 0);
        });
    }
}
