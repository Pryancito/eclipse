//! Handlers implemented in kernel and called by HAL.

use crate::{utils::init_once::InitOnce, MMUFlags, PhysAddr, VirtAddr};

/// Functions implemented in the kernel and used by HAL funtions.
pub trait KernelHandler: Send + Sync + 'static {
    /// Allocate one physical frame.
    fn frame_alloc(&self) -> Option<PhysAddr> {
        unimplemented!()
    }

    /// Allocate contiguous `frame_count` physical frames.
    fn frame_alloc_contiguous(&self, _frame_count: usize, _align_log2: usize) -> Option<PhysAddr> {
        unimplemented!()
    }

    /// Deallocate a physical frame.
    fn frame_dealloc(&self, _paddr: PhysAddr) {
        unimplemented!()
    }

    /// Handle kernel mode page fault.
    fn handle_page_fault(&self, _fault_vaddr: VirtAddr, _access_flags: MMUFlags) {}

    /// Get memory usage: (used_bytes, total_bytes)
    fn memory_usage(&self) -> (usize, usize) {
        (0, 0)
    }

    /// Whether `[vaddr, vaddr + len)` is covered by a mapping in the CURRENT
    /// process's address space.
    ///
    /// `UserPtr::check` could only ask whether an address is in the user half,
    /// which says nothing about whether anything is mapped there. So every
    /// user-pointer access in every syscall dereferenced straight into the user
    /// address space and relied on the page being resolvable. When it is not —
    /// a wild pointer from a library, a freed mapping — the kernel takes a page
    /// fault it has no fixup for and the machine dies:
    ///
    ///     [KERNEL PAGE FAULT] vaddr=0x7125048a65 rip=<sys_futex...>
    ///         (unresolved by the user vmar)
    ///
    /// which is a kernel DoS reachable from any syscall, by any process.
    ///
    /// This asks the question `check` could not. It is about MAPPINGS, not
    /// present pages: demand paging must keep working, so a mapped-but-uncommitted
    /// range answers `true` and faults in as usual. Only a range with no mapping
    /// at all — the case that is fatal — answers `false`.
    ///
    /// Defaults to `true` so a handler that does not implement it (libos, tests)
    /// behaves exactly as before.
    fn check_user_range(&self, _vaddr: VirtAddr, _len: usize) -> bool {
        true
    }
}

#[allow(dead_code)]
pub(crate) struct DummyKernelHandler;

#[cfg(feature = "libos")]
pub(crate) static KHANDLER: InitOnce<&dyn KernelHandler> =
    InitOnce::new_with_default(&DummyKernelHandler);

#[cfg(not(feature = "libos"))]
pub(crate) static KHANDLER: InitOnce<&dyn KernelHandler> = InitOnce::new();
