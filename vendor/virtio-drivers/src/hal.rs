use super::*;

type VirtAddr = usize;
type PhysAddr = usize;

pub struct DMA {
    /// Physical address of the block, full width.
    ///
    /// This used to be a `u32`, and `DMA::new` wrote `paddr as u32` into it.
    /// The allocator behind `virtio_dma_alloc` is the kernel frame allocator
    /// and hands out whatever physical frame it has, so on a machine with more
    /// than 4 GiB of RAM the address was **truncated**: `paddr()` and `pfn()`
    /// then named a different, live frame, and the device DMA'd into it. The
    /// legacy MMIO `QueuePFN` register really is 32 bits wide -- that is a
    /// 44-bit address, not a 32-bit one -- so the narrowing belongs in `pfn`,
    /// where it is checked, and nowhere else.
    paddr: usize,
    pages: usize,
}

impl DMA {
    pub fn new(pages: usize) -> Result<Self> {
        if pages == 0 {
            return Err(Error::InvalidParam);
        }
        let paddr = unsafe { virtio_dma_alloc(pages) };
        if paddr == 0 {
            return Err(Error::DmaError);
        }
        Ok(DMA { paddr, pages })
    }

    pub fn paddr(&self) -> usize {
        self.paddr
    }

    pub fn vaddr(&self) -> usize {
        phys_to_virt(self.paddr)
    }

    /// Page frame number, for the legacy `QueuePFN` register.
    ///
    /// # Panics
    ///
    /// If the block sits above the 44 bits that register can name. A silent
    /// truncation here points the device at the wrong frame, which is a memory
    /// corruption the driver never sees; refusing is the only honest answer,
    /// and the allocator has to be asked for a low block instead.
    pub fn pfn(&self) -> u32 {
        let pfn = self.paddr >> 12;
        assert!(
            pfn <= u32::MAX as usize,
            "DMA block at {:#x} is above the 44 bits QueuePFN can name",
            self.paddr
        );
        pfn as u32
    }

    /// Convert to a buffer
    pub unsafe fn as_buf(&self) -> &'static mut [u8] {
        core::slice::from_raw_parts_mut(self.vaddr() as _, PAGE_SIZE * self.pages)
    }
}

impl Drop for DMA {
    fn drop(&mut self) {
        let err = unsafe { virtio_dma_dealloc(self.paddr, self.pages) };
        assert_eq!(err, 0, "failed to deallocate DMA");
    }
}

pub fn phys_to_virt(paddr: PhysAddr) -> VirtAddr {
    unsafe { virtio_phys_to_virt(paddr) }
}

pub fn virt_to_phys(vaddr: VirtAddr) -> PhysAddr {
    unsafe { virtio_virt_to_phys(vaddr) }
}

extern "C" {
    fn virtio_dma_alloc(pages: usize) -> PhysAddr;
    fn virtio_dma_dealloc(paddr: PhysAddr, pages: usize) -> i32;
    fn virtio_phys_to_virt(paddr: PhysAddr) -> VirtAddr;
    fn virtio_virt_to_phys(vaddr: VirtAddr) -> PhysAddr;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_of_no_pages_is_refused() {
        // `pages(0)` is 0, and an allocator asked for nothing has nothing
        // sensible to answer; it used to reach the allocator and come back as
        // a `DmaError`, which reads as "out of memory" rather than "you asked
        // for nothing".
        assert_eq!(DMA::new(0).err(), Some(Error::InvalidParam));
    }

    #[test]
    fn a_block_is_a_whole_number_of_pages_and_page_aligned() {
        let dma = DMA::new(2).unwrap();
        assert_eq!(dma.paddr() % PAGE_SIZE, 0);
        assert_eq!(unsafe { dma.as_buf() }.len(), 2 * PAGE_SIZE);
    }

    #[test]
    fn a_block_keeps_its_whole_physical_address() {
        // `paddr` was a `u32`. The kernel frame allocator hands out whatever
        // frame the machine has, so on anything with more than 4 GiB of RAM the
        // address was cut down to 32 bits and named a different, live frame --
        // which the device then wrote into. The test arena sits above 4 GiB for
        // exactly this reason.
        let dma = DMA::new(1).unwrap();
        assert!(
            dma.paddr() > u32::MAX as usize,
            "the block came back at {:#x}: either the address was truncated to \
             32 bits, or the test arena moved below 4 GiB and stopped looking",
            dma.paddr()
        );
    }

    #[test]
    fn the_page_frame_number_is_the_address_shifted() {
        let dma = DMA::new(1).unwrap();
        assert_eq!(dma.pfn() as usize, dma.paddr() >> 12);
    }

    #[test]
    fn the_two_translations_are_each_other() {
        let dma = DMA::new(1).unwrap();
        assert_eq!(virt_to_phys(dma.vaddr()), dma.paddr());
        assert_eq!(phys_to_virt(dma.paddr()), dma.vaddr());
    }

    #[test]
    fn an_address_the_page_frame_number_cannot_name_is_refused_not_truncated() {
        // `paddr` was a `u32`, so a frame above 4 GiB -- which the kernel frame
        // allocator hands out on any machine with more than that -- came back
        // as a different, live frame, and the device wrote into it. The field
        // is full width now, and the narrowing that is genuinely forced by the
        // 32-bit `QueuePFN` register says so instead of quietly happening.
        let high = DMA {
            paddr: 1usize << 45,
            pages: 1,
        };
        let refused = std::panic::catch_unwind(|| high.pfn());
        assert!(
            refused.is_err(),
            "a frame above 2^44 was given a page number"
        );
        core::mem::forget(high);
    }
}
