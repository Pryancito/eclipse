//! Nothing

/// Offset between a "physical" address and its kernel virtual mapping.
///
/// Under libos the physical memory is a window the HAL maps into this very
/// process (`kernel_hal::libos::mem::PMEM_MAP_VADDR`, see `mock_mem.rs`), and
/// a "physical" address is an offset into that window. The two address spaces
/// do NOT coincide, which is what this used to say and return (zero): the
/// generic allocator in `zCore/src/memory.rs` then handed `heap.transfer` the
/// region `0x1000..4 GiB` at host virtual address `0x1000`, and the buddy's
/// first free-list node write was a SIGSEGV -- `str x3, [x18]` with
/// `x18 = 0x1000` in `Node::insert`, from `insert_regions`, before a single
/// line of the guest ran. That is the `exit=-11` at 0.1 s that
/// `Zircon Core Test Libos (aarch64 Linux)` has died of on every run, and it
/// had nothing to do with the guest: no Linux-hosted build other than x86_64
/// (which takes `memory_x86_64.rs`, a bitmap frame allocator over the host
/// heap) ever got this far.
///
/// `frame_alloc` subtracts this offset from the pointer it returns and
/// `frame_dealloc` adds it back, so it has to be the HAL's own mapping and
/// not a number beside it. And it has to be the CONSTANT, not
/// `kernel_hal::mem::phys_to_virt(0)`: `insert_regions` asks with the heap
/// lock held, and that call's first use creates the mock memory's temp file,
/// which allocates, which takes the heap lock again -- measured in the arm64
/// VM as two threads spinning on the heap's ticket lock for ever, with
/// nothing printed.
///
/// `#[allow(dead_code)]`: only `zCore/src/memory.rs` calls this, and
/// `zCore/src/main.rs` routes x86_64 to `memory_x86_64.rs` instead, so on an
/// x86_64 libos build nothing references it -- and `#![deny(warnings)]` turns
/// that into a hard error.
#[allow(dead_code)]
#[inline]
pub fn phys_to_virt_offset() -> usize {
    // Linear: `MockMemory::phys_to_virt` is `PMEM_MAP_VADDR + paddr`.
    kernel_hal::libos::PMEM_MAP_VADDR
}
