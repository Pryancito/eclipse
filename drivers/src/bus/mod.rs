pub mod brcmstb;
pub mod klog;
#[cfg(any(
    target_arch = "x86_64",
    target_arch = "riscv64",
    target_arch = "aarch64"
))]
pub mod pci;
pub mod pci_drivers;

pub fn phys_to_virt(paddr: PhysAddr) -> VirtAddr {
    unsafe { drivers_phys_to_virt(paddr) }
}

pub fn virt_to_phys(vaddr: VirtAddr) -> PhysAddr {
    unsafe { drivers_virt_to_phys(vaddr) }
}

/// Turn a device-physical window into the virtual address a driver may use.
///
/// Goes through the base the mapper **returns**, not through [`phys_to_virt`]:
/// on riscv a BAR above 512 GiB is mapped at `paddr | (0x1ffffff << 39)`, which
/// is not the linear map, so `phys_to_virt` would hand back an address nothing
/// is mapped at and every MMIO access would fault. On x86 the mapper returns
/// the linear map, so this is `phys_to_virt(pa + off)` either way; with no
/// mapper at all (aarch64 installs none) it is that as well.
///
/// `query_or_map` queries an existing mapping before making one, so calling it
/// once per window is safe even when several windows share a BAR.
///
/// Every PCI driver in the tree needs this, and each one wrote its own: seven
/// of them called `query_or_map`, **threw away the base it returned** and used
/// `phys_to_virt` anyway. One of them --- the modern virtio path --- had the
/// paragraph above written out above the call, sixty lines from a copy that did
/// it wrong.
pub fn resolve_window(
    mapper: &Option<alloc::sync::Arc<dyn crate::builder::IoMapper>>,
    pa: PhysAddr,
    len: usize,
    off: usize,
) -> VirtAddr {
    match mapper {
        Some(m) => m
            .query_or_map(pa, len)
            .map(|base| base + off)
            .unwrap_or_else(|| phys_to_virt(pa + off)),
        None => phys_to_virt(pa + off),
    }
}

/// Return `pages` DMA pages starting at `paddr` to the kernel allocator.
///
/// # Safety
/// The caller must guarantee no device is still reading from or writing to
/// this memory, and that nothing else holds a pointer into it.
pub unsafe fn dma_dealloc(paddr: PhysAddr, pages: usize) -> i32 {
    unsafe { drivers_dma_dealloc(paddr, pages) }
}

#[allow(unused)]
unsafe extern "C" {
    pub fn drivers_dma_alloc(pages: usize) -> PhysAddr;
    fn drivers_dma_dealloc(paddr: PhysAddr, pages: usize) -> i32;
    fn drivers_phys_to_virt(paddr: PhysAddr) -> VirtAddr;
    fn drivers_virt_to_phys(vaddr: VirtAddr) -> PhysAddr;
    pub fn drivers_timer_now_as_micros() -> u64;
}

pub const PAGE_SIZE: usize = 4096;

type VirtAddr = usize;
type PhysAddr = usize;

use core::ptr::{read_volatile, write_volatile};
#[inline(always)]
pub fn write<T>(addr: usize, content: T) {
    let cell = (addr) as *mut T;
    unsafe {
        write_volatile(cell, content);
    }
}
#[inline(always)]
pub fn read<T>(addr: usize) -> T {
    let cell = (addr) as *const T;
    unsafe { read_volatile(cell) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::IoMapper;
    use alloc::boxed::Box;
    use alloc::sync::Arc;

    /// A mapper that hands out a base of its own, the way riscv's does for a
    /// BAR above 512 GiB, and refuses anything it was not given.
    struct Sv39Like {
        pa: PhysAddr,
        base: VirtAddr,
    }

    impl IoMapper for Sv39Like {
        fn query_or_map(&self, paddr: PhysAddr, _size: usize) -> Option<VirtAddr> {
            if paddr == self.pa {
                Some(self.base)
            } else {
                None
            }
        }
    }

    fn sv39_like() -> Option<Arc<dyn IoMapper>> {
        Some(Arc::new(Sv39Like {
            pa: 0xfe00_0000,
            base: 0xffff_ffc0_0000_0000,
        }))
    }

    #[test]
    fn the_address_a_window_resolves_to_is_the_one_the_mapper_gives() {
        assert_eq!(
            resolve_window(&sv39_like(), 0xfe00_0000, 0x4000, 0x1000),
            0xffff_ffc0_0000_1000,
            "the offset goes on the mapped base, not on the physical address"
        );
    }

    #[test]
    fn a_window_the_mapper_cannot_map_falls_back_to_the_linear_map() {
        // `phys_to_virt` is the identity in the host build.
        assert_eq!(
            resolve_window(&sv39_like(), 0xfd00_0000, 0x1000, 0x40),
            phys_to_virt(0xfd00_0040)
        );
    }

    #[test]
    fn without_a_mapper_a_window_is_the_linear_map() {
        assert_eq!(
            resolve_window(&None, 0xfd00_0000, 0x1000, 0x40),
            phys_to_virt(0xfd00_0040)
        );
    }

    // --- the two primitives every non-x86 MMIO path is built on ---------------

    /// `read` and `write` are the whole of how the mips, riscv64 and aarch64
    /// `PortOps` reach config space, and on x86_64 nothing instantiates them,
    /// so they had never run. The address arithmetic is the caller's; what
    /// these two owe is to touch the address they were handed and no other.
    #[test]
    fn a_word_written_to_a_register_reads_back_from_that_same_register() {
        let block: &'static mut [u32; 4] = Box::leak(Box::new([0u32; 4]));
        let base = block as *mut [u32; 4] as usize;

        write::<u32>(base + 4, 0xDEAD_BEEF);

        assert_eq!(read::<u32>(base + 4), 0xDEAD_BEEF);
        assert_eq!(read::<u32>(base), 0, "the register below was written to");
        assert_eq!(
            read::<u32>(base + 8),
            0,
            "the register above was written to"
        );
    }

    /// The width is the type, not the pointer. A `write::<u16>` that stored a
    /// whole word would clobber the register next to the one asked for -- and
    /// on a peripheral whose registers are packed two to a word, that is the
    /// neighbouring register, in silence.
    #[test]
    fn the_width_of_an_access_is_the_type_it_is_asked_for() {
        let block: &'static mut [u32; 2] = Box::leak(Box::new([0xFFFF_FFFFu32; 2]));
        let base = block as *mut [u32; 2] as usize;

        write::<u16>(base, 0);

        assert_eq!(
            read::<u32>(base),
            0xFFFF_0000,
            "a 16-bit write reached outside its own half of the word"
        );
        assert_eq!(
            read::<u32>(base + 4),
            0xFFFF_FFFF,
            "the next word was touched"
        );
    }

    /// `dma_dealloc` hands the kernel a PAGE count, and the unit is the part
    /// worth pinning: a driver that passed bytes here would ask the allocator
    /// to free four thousand times the memory it owns.
    #[test]
    fn freeing_dma_pages_tells_the_kernel_how_many() {
        use crate::utils::host_hooks::{alone_with_the_allocator, DEALLOC_CALLS, DEALLOC_PAGES};
        use core::sync::atomic::Ordering;

        alone_with_the_allocator(|| {
            let rc = unsafe { dma_dealloc(0x1000, 3) };

            assert_eq!(rc, 0);
            assert_eq!(
                DEALLOC_CALLS.load(Ordering::SeqCst),
                1,
                "the kernel was never told"
            );
            assert_eq!(DEALLOC_PAGES.load(Ordering::SeqCst), 3, "pages, not bytes");
        });
    }
}
