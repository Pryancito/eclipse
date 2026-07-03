use kernel_hal::{KernelHandler, MMUFlags};
use zircon_object::task::Thread;

use super::memory;

pub struct ZcoreKernelHandler;

impl KernelHandler for ZcoreKernelHandler {
    fn frame_alloc(&self) -> Option<usize> {
        memory::frame_alloc(1, 0)
    }

    fn frame_alloc_contiguous(&self, frame_count: usize, align_log2: usize) -> Option<usize> {
        memory::frame_alloc(frame_count, align_log2)
    }

    fn frame_dealloc(&self, paddr: usize) {
        memory::frame_dealloc(paddr)
    }

    fn handle_page_fault(&self, fault_vaddr: usize, access_flags: MMUFlags) {
        if let Some(thread) = kernel_hal::thread::get_current_thread() {
            let thread = thread.downcast::<Thread>().unwrap();
            let vmar = thread.proc().vmar();
            if let Err(err) = vmar.handle_page_fault(fault_vaddr, access_flags) {
                // Loud on the *graphic* console (panic prints to serial only,
                // invisible on a headless-but-monitor'd bring-up box). A kernel
                // fault here during a syscall is almost always a driver
                // dereferencing an address the user vmar can't resolve -- e.g.
                // the vendored NVIDIA RM touching an unmapped/mismapped MMIO or
                // heap pointer. The fault address + flags name the culprit
                // directly instead of leaving a silent frozen `cat`.
                kernel_hal::console::console_write_fmt(format_args!(
                    "\n[KERNEL PAGE FAULT] vaddr={:#x} flags={:?} err={:?} \
                     (unresolved against the faulting thread's user vmar -- \
                     likely a kernel-side driver bug, not a userspace fault)\n",
                    fault_vaddr, access_flags, err
                ));
                panic!(
                    "handle kernel page fault error: {:?} vaddr(0x{:x}) flags({:?})",
                    err, fault_vaddr, access_flags
                );
            }
        } else {
            kernel_hal::console::console_write_fmt(format_args!(
                "\n[KERNEL PAGE FAULT] vaddr={:#x} flags={:?} \
                 (no current thread -- fault in kernel-private context)\n",
                fault_vaddr, access_flags
            ));
            panic!(
                "page fault from kernel private address 0x{:x}, flags = {:?}",
                fault_vaddr, access_flags
            );
        }
    }

    fn memory_usage(&self) -> (usize, usize) {
        memory::stats()
    }
}
