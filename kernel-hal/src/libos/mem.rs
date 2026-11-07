//! Physical memory operations.

use alloc::vec::Vec;
use core::ops::Range;

use crate::sync::Mutex;
use bitmap_allocator::BitAlloc;

use super::mock_mem::MockMemory;
use crate::{PhysAddr, VirtAddr, PAGE_SIZE};

type FrameAlloc = bitmap_allocator::BitAlloc1M;

/// Map physical memory from here.
///
/// `pub` (re-exported as `kernel_hal::libos::PMEM_MAP_VADDR`) because the
/// generic kernel allocator needs the "physical"-to-virtual offset as a plain
/// constant: it reads it with its heap lock held, and anything that could
/// allocate there -- the lazy `MOCK_PHYS_MEM`, whose first use creates a temp
/// file -- re-enters the allocator and deadlocks on its own lock.
pub const PMEM_MAP_VADDR: VirtAddr = 0x8_0000_0000;
/// Physical memory size = 4 GiB, the most `BitAlloc1M` can keep track of.
///
/// A sparse file, so only the frames a guest touches cost the host anything.
/// It was 1 GiB, and that is what `RseqTest.ManyConcurrent` ran out of: a
/// guest page that is accessible is committed as soon as it is mapped here
/// (there is no demand paging in libos), so each of its 2048 threads cost its
/// whole 256 KiB stack and 256 KiB unsafe stack up front, and the 1888th
/// thread's `zx_vmar_map` came back NO_MEMORY with 1023 MiB used. Zircon
/// commits a stack page when it is first touched, and the test assumes it.
pub(super) const PMEM_SIZE: usize = 0x1_0000_0000;

lazy_static! {
    pub(super) static ref FRAME_ALLOCATOR: Mutex<FrameAlloc> = {
        let mut allocator = FrameAlloc::DEFAULT;
        allocator.insert(1..PMEM_SIZE / PAGE_SIZE);
        Mutex::new(allocator)
    };
    pub(super) static ref MOCK_PHYS_MEM: MockMemory = MockMemory::new(PMEM_SIZE);
}

pub(super) static USED_PAGES: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

hal_fn_impl! {
    impl mod crate::hal_fn::mem {
        fn phys_to_virt(paddr: PhysAddr) -> VirtAddr {
            MOCK_PHYS_MEM.phys_to_virt(paddr)
        }

        fn virt_to_phys(vaddr: VirtAddr) -> PhysAddr {
            assert!((PMEM_MAP_VADDR..PMEM_MAP_VADDR + PMEM_SIZE).contains(&vaddr));
            vaddr - PMEM_MAP_VADDR
        }

        #[allow(clippy::single_range_in_vec_init)]
        fn free_pmem_regions() -> Vec<Range<PhysAddr>> {
            // The window has to exist before anyone acts on the answer: the
            // generic kernel allocator (`zCore/src/memory.rs`, aarch64 and
            // riscv64) writes its free-list nodes into the region it is
            // handed, and the window is a lazy static that nothing else
            // touched by then -- so on an aarch64 libos the first node write
            // went to `PMEM_MAP_VADDR + 0x1000` with nothing mapped there
            // (SIGSEGV in `Node::insert` from `insert_regions`, measured in
            // an arm64 VM). Forcing it HERE, outside any lock, is also what
            // keeps the allocator from initialising it later from under its
            // own heap lock, which allocates (a temp file) and deadlocks.
            lazy_static::initialize(&MOCK_PHYS_MEM);
            vec![PAGE_SIZE..PMEM_SIZE]
        }

        fn pmem_read(paddr: PhysAddr, buf: &mut [u8]) {
            trace!("pmem read: paddr={:#x}, len={:#x}", paddr, buf.len());
            assert!(paddr + buf.len() <= PMEM_SIZE);
            let src = MOCK_PHYS_MEM.as_ptr(paddr);
            unsafe { buf.as_mut_ptr().copy_from_nonoverlapping(src, buf.len()) };
        }

        fn pmem_write(paddr: PhysAddr, buf: &[u8]) {
            trace!("pmem write: paddr={:#x}, len={:#x}", paddr, buf.len());
            assert!(paddr + buf.len() <= PMEM_SIZE);
            let dst = MOCK_PHYS_MEM.as_mut_ptr::<u8>(paddr);
            unsafe { dst.copy_from_nonoverlapping(buf.as_ptr(), buf.len()) };
        }

        fn pmem_zero(paddr: PhysAddr, len: usize) {
            trace!("pmem_zero: addr={:#x}, len={:#x}", paddr, len);
            assert!(paddr + len <= PMEM_SIZE);
            unsafe { core::ptr::write_bytes(MOCK_PHYS_MEM.as_mut_ptr::<u8>(paddr), 0, len) };
        }

        fn pmem_copy(dst: PhysAddr, src: PhysAddr, len: usize) {
            trace!("pmem_copy: {:#x} <- {:#x}, len={:#x}", dst, src, len);
            assert!(src + len <= PMEM_SIZE && dst + len <= PMEM_SIZE);
            let dst = MOCK_PHYS_MEM.as_mut_ptr::<u8>(dst);
            let src = MOCK_PHYS_MEM.as_ptr::<u8>(src);
            unsafe { dst.copy_from_nonoverlapping(src, len) };
        }

        fn frame_flush(_target: PhysAddr) {
            // do nothing
        }

        fn memory_usage() -> (usize, usize) {
            let used = USED_PAGES.load(core::sync::atomic::Ordering::Relaxed) * PAGE_SIZE;
            (used, PMEM_SIZE)
        }
    }
}
