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
        let pages = (len + PAGE_SIZE - 1) / PAGE_SIZE;
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
