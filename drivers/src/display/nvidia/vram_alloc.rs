//! Bitmap VRAM allocator over the BAR1 aperture (4 KiB pages). A child of
//! `nvidia` with no logic change: the parent is at its size ceiling
//! (`xtask tamano`). `pub(super)` keeps it visible to the parent and to
//! every sibling module.

use alloc::vec::Vec;

/// Simple bitmap-based VRAM allocator for BAR1 aperture (4KB page granularity)
pub(super) struct NvidiaVramAllocator {
    base_phys: u64,
    total_size: u64,
    bitmap: Vec<u64>,
}

impl NvidiaVramAllocator {
    pub(super) fn new(base_phys: u64, total_size: u64) -> Self {
        let num_pages = (total_size / 4096) as usize;
        let num_u64s = num_pages.div_ceil(64);
        Self {
            base_phys,
            total_size,
            bitmap: alloc::vec![0; num_u64s],
        }
    }

    /// Unused (kept for a future purely-Rust-side allocation need): the
    /// nouveau-uAPI `GEM_NEW` handler (`nvidia.rs` `ioctl`) allocates
    /// through the real RM heap instead (`nvidia_rm_sys::rm_init::
    /// gem_alloc`) so it shares RM's own VRAM bookkeeping rather than
    /// carving up the same physical range out-of-band with a second,
    /// independent allocator.
    #[allow(dead_code)]
    fn _alloc(&mut self, size: usize, align: usize) -> Option<u64> {
        let num_pages = size.div_ceil(4096);
        let align_pages = (align.max(4096) / 4096).max(1);
        let total_bits = (self.total_size / 4096) as usize;

        let mut count = 0;
        let mut start_bit = 0;

        for bit in 0..total_bits {
            let uidx = bit / 64;
            let ubit = bit % 64;
            let is_free = (self.bitmap[uidx] & (1 << ubit)) == 0;

            if is_free {
                if count == 0 {
                    if bit % align_pages != 0 {
                        continue;
                    }
                    start_bit = bit;
                }
                count += 1;
                if count >= num_pages {
                    for i in 0..num_pages {
                        let b = start_bit + i;
                        self.bitmap[b / 64] |= 1 << (b % 64);
                    }
                    return Some(self.base_phys + (start_bit as u64 * 4096));
                }
            } else {
                count = 0;
            }
        }
        None
    }

    pub(super) fn free(&mut self, phys_addr: u64, size: usize) {
        let offset = phys_addr.saturating_sub(self.base_phys);
        if offset >= self.total_size {
            return;
        }
        let start_bit = (offset / 4096) as usize;
        let num_pages = size.div_ceil(4096);
        for i in 0..num_pages {
            let b = start_bit + i;
            if b / 64 < self.bitmap.len() {
                self.bitmap[b / 64] &= !(1 << (b % 64));
            }
        }
    }
}
