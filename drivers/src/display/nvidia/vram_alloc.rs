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

    /// Frees the pages `[phys_addr, phys_addr + size)` names; an address
    /// outside the aperture frees nothing. (Below `base_phys` it used to
    /// saturate to offset 0 and free the first pages.)
    pub(super) fn free(&mut self, phys_addr: u64, size: usize) {
        if phys_addr < self.base_phys {
            return;
        }
        let offset = phys_addr - self.base_phys;
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

#[cfg(test)]
mod tests {
    //! The allocator on its own: nothing exercised it (`_alloc` has no
    //! caller and `free` only runs on a real GPU), so none of its lines
    //! had a test. Pages are 4 KiB; the aperture here is tiny and the
    //! numbers are the bitmap's own.

    use super::*;

    const BASE: u64 = 0x1_0000_0000;
    const PAGE: u64 = 4096;

    fn used(a: &NvidiaVramAllocator) -> Vec<usize> {
        let pages = (a.total_size / PAGE) as usize;
        (0..pages)
            .filter(|&b| a.bitmap[b / 64] & (1 << (b % 64)) != 0)
            .collect()
    }

    #[test]
    fn the_bitmap_has_one_bit_per_page_rounded_up_to_a_word() {
        assert_eq!(NvidiaVramAllocator::new(BASE, 0).bitmap.len(), 0);
        assert_eq!(NvidiaVramAllocator::new(BASE, PAGE).bitmap.len(), 1);
        assert_eq!(NvidiaVramAllocator::new(BASE, 64 * PAGE).bitmap.len(), 1);
        assert_eq!(NvidiaVramAllocator::new(BASE, 65 * PAGE).bitmap.len(), 2);
        // A size that is not whole pages loses the tail.
        let a = NvidiaVramAllocator::new(BASE, 3 * PAGE + 100);
        assert_eq!(a.total_size, 3 * PAGE + 100);
        assert_eq!(used(&a), Vec::<usize>::new());
    }

    #[test]
    fn allocations_are_first_fit_from_the_base_and_round_up_to_pages() {
        let mut a = NvidiaVramAllocator::new(BASE, 8 * PAGE);
        assert_eq!(a._alloc(1, 0), Some(BASE), "a byte takes the first page");
        assert_eq!(
            a._alloc(PAGE as usize + 1, 0),
            Some(BASE + PAGE),
            "two pages"
        );
        assert_eq!(used(&a), alloc::vec![0, 1, 2]);
        assert_eq!(a._alloc(5 * PAGE as usize, 0), Some(BASE + 3 * PAGE));
        assert_eq!(a._alloc(1, 0), None, "full");
        assert_eq!(used(&a), (0..8).collect::<Vec<_>>());
    }

    #[test]
    fn a_run_broken_by_a_used_page_starts_over_after_it() {
        let mut a = NvidiaVramAllocator::new(BASE, 8 * PAGE);
        // Pages 0..8 free; take page 2 so the free run before it is 2 long.
        a.bitmap[0] |= 1 << 2;
        assert_eq!(
            a._alloc(3 * PAGE as usize, 0),
            Some(BASE + 3 * PAGE),
            "three pages do not fit in 0..2, so they go after page 2"
        );
        assert_eq!(used(&a), alloc::vec![2, 3, 4, 5]);
        assert_eq!(
            a._alloc(2 * PAGE as usize, 0),
            Some(BASE),
            "two pages still fit in 0..2"
        );
    }

    #[test]
    fn alignment_is_honoured_in_pages_and_anything_under_a_page_is_a_page() {
        let mut a = NvidiaVramAllocator::new(BASE, 16 * PAGE);
        assert_eq!(a._alloc(1, 0), Some(BASE));
        assert_eq!(
            a._alloc(1, 4 * PAGE as usize),
            Some(BASE + 4 * PAGE),
            "the next 4-page boundary, pages 1..4 skipped"
        );
        assert_eq!(
            a._alloc(1, 1),
            Some(BASE + PAGE),
            "an alignment under a page is a page"
        );
        assert_eq!(
            a._alloc(2 * PAGE as usize, 8 * PAGE as usize),
            Some(BASE + 8 * PAGE)
        );
        assert_eq!(used(&a), alloc::vec![0, 1, 4, 8, 9]);
        assert_eq!(
            a._alloc(1, 16 * PAGE as usize),
            None,
            "no free 16-page boundary but 0, which is taken"
        );
        assert_eq!(
            a._alloc(8 * PAGE as usize, 8 * PAGE as usize),
            None,
            "8..16 has page 8 and 9 taken"
        );
    }

    #[test]
    fn free_clears_exactly_the_pages_it_names_and_they_come_back() {
        let mut a = NvidiaVramAllocator::new(BASE, 8 * PAGE);
        let first = a._alloc(3 * PAGE as usize, 0).unwrap();
        let second = a._alloc(2 * PAGE as usize, 0).unwrap();
        assert_eq!((first, second), (BASE, BASE + 3 * PAGE));
        a.free(first, 3 * PAGE as usize);
        assert_eq!(used(&a), alloc::vec![3, 4]);
        a.free(second + PAGE, 1);
        assert_eq!(
            used(&a),
            alloc::vec![3],
            "a byte frees the one page it is in"
        );
        assert_eq!(
            a._alloc(3 * PAGE as usize, 0),
            Some(BASE),
            "freed pages are reused"
        );
        assert_eq!(used(&a), alloc::vec![0, 1, 2, 3]);
    }

    #[test]
    fn free_outside_the_aperture_touches_nothing() {
        let mut a = NvidiaVramAllocator::new(BASE, 4 * PAGE);
        assert_eq!(a._alloc(4 * PAGE as usize, 0), Some(BASE));
        a.free(BASE - PAGE, PAGE as usize);
        assert_eq!(
            used(&a),
            alloc::vec![0, 1, 2, 3],
            "below the base: not page 0"
        );
        a.free(BASE + 4 * PAGE, PAGE as usize);
        assert_eq!(used(&a), alloc::vec![0, 1, 2, 3], "at the end");
        a.free(u64::MAX, PAGE as usize);
        assert_eq!(used(&a), alloc::vec![0, 1, 2, 3]);
        // A range that starts inside and runs off the end frees what is
        // inside and stops at the bitmap.
        a.free(BASE + 3 * PAGE, 100 * PAGE as usize);
        assert_eq!(used(&a), alloc::vec![0, 1, 2]);
    }
}
