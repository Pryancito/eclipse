//! Define dynamic memory allocation.

use crate::platform::phys_to_virt_offset;
use alloc::alloc::handle_alloc_error;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::{
    alloc::{GlobalAlloc, Layout},
    num::NonZeroUsize,
    ops::Range,
    ptr::NonNull,
};
use customizable_buddy::{BuddyAllocator, LinkedListBuddy, UsizeBuddy};
use kernel_hal::sync::Mutex;
use kernel_hal::PhysAddr;

static TOTAL_MEMORY: AtomicUsize = AtomicUsize::new(0);
static USED_MEMORY: AtomicUsize = AtomicUsize::new(0);

/// 堆分配器。
///
/// 27 + 6 + 3 = 36 -> 64 GiB
struct LockedHeap(Mutex<BuddyAllocator<27, UsizeBuddy, LinkedListBuddy>>);

// Not the global allocator in the test binary: the harness allocates long
// before anything calls `init()`, so installing this over the 2 MiB static
// would hand the test runner an empty heap. The allocator itself is still
// compiled and still reachable -- the tests call `init()` and then drive
// `HEAP` directly, which is what the kernel's own `GlobalAlloc` impl does.
#[cfg_attr(not(test), global_allocator)]
static HEAP: LockedHeap = LockedHeap(Mutex::new(BuddyAllocator::new()));

/// 单页地址位数。
const PAGE_BITS: usize = 12;

/// 为启动准备的初始内存。
///
/// 经测试，不同硬件的需求：
///
/// | machine         | memory
/// | --------------- | -
/// | qemu,virt SMP 1 |  16 KiB
/// | qemu,virt SMP 4 |  32 KiB
/// | allwinner,nezha | 256 KiB
const MEMORY_SIZE: usize = 2 * 1024 * 1024;

/// Page-aligned on purpose, and the alignment is load-bearing.
///
/// `init()` hands this block to the buddy after telling it the minimum order is
/// `size_of::<usize>()`, and the buddy splits a transferred block by the
/// alignment of its **address**: a block less aligned than that minimum makes
/// it evaluate `order - min_order` on a `usize` with `order` smaller, which
/// underflows. That is a panic in a debug build and an out-of-bounds index into
/// the layer array in a release one, and both happen inside `init()`, before
/// there is a heap to format a message with.
///
/// Nothing but this attribute makes the address aligned: `align_of::<[u8; N]>()`
/// is **1**, whatever N is. The kernel links have been getting away with it on
/// the linker's goodwill -- opening this file to the host suite put the static
/// on an odd address and `init()` died on the first transfer.
///
/// Page alignment rather than word alignment because the same pool backs
/// `frame_alloc`, and its callers are handed addresses they take to be pages.
#[repr(align(4096))]
// The field is never read *through the type*: `init()` takes the static's
// address with `addr_of_mut!` (a `&mut` to a `static mut` is what that macro
// exists to avoid) and hands it to the allocator as bytes. The array is here to
// reserve the space and to carry the alignment, which is the whole point.
struct BootPool(#[allow(dead_code)] [u8; MEMORY_SIZE]);

static mut MEMORY: BootPool = BootPool([0u8; MEMORY_SIZE]);

unsafe impl GlobalAlloc for LockedHeap {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Bind the allocation to a local so the heap lock (a temporary
        // `MutexGuard`) is dropped at the end of THIS statement, before the
        // alias check runs. `heap_alias_check` may panic into the BSOD painter,
        // which must never re-enter this lock.
        let alloc = self.0.lock().allocate_layout(layout);
        match alloc {
            Ok((ptr, size)) => {
                // The REQUESTED size, not the block the buddy rounded it up to,
                // because `dealloc` below subtracts `layout.size()` and the two
                // have to be the same quantity. Counting `size` here and
                // `layout.size()` there left the difference in the counter on
                // every allocation that was not already a multiple of the
                // buddy's minimum block: `heap_used()` climbed for ever, and
                // once it passed `heap_total()` every reader of it -- the stats
                // syscall, the OOM report -- was quoting a number larger than
                // the heap. `memory_x86_64.rs` counts the requested size on
                // both sides, and says so.
                USED_MEMORY.fetch_add(layout.size(), Ordering::Relaxed);
                // [diag] The kernel heap (Box/Vec/Arc/String, and every
                // `*_zeroed` allocation) is carved from the SAME buddy arena as
                // the coroutine stacks. `frame_alloc` already alias-checks the
                // VMO consumer of this arena; this is the OTHER consumer and had
                // no check. A zeroed heap object handed a block that overlaps a
                // live kernel stack blanks it with zeros — the exact recurring
                // `[null-exec]` signature (`ret` pops 0, `rip=0x0`). Catch it at
                // hand-out, before the zero-fill corrupts anything.
                heap_alias_check(ptr.as_ptr() as usize, size);
                ptr.as_ptr()
            }
            Err(_) => handle_alloc_error(layout),
        }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        USED_MEMORY.fetch_sub(layout.size(), Ordering::Relaxed);
        self.0
            .lock()
            .deallocate_layout(NonNull::new(ptr).unwrap(), layout)
    }
}

/// 初始化分配器，并将一个小的内存块注册到分配器中，用于启动需要的动态内存。
pub fn init() {
    unsafe {
        let start = core::ptr::addr_of_mut!(MEMORY).cast::<u8>();
        // `MEMORY_SIZE`, not the size written out a second time: the constant
        // above is a per-board tuning knob (its own comment tabulates what
        // different machines need), and a hand-written length beside it only
        // agrees until someone turns the knob. Too small loses the rest of the
        // pool; too large hands the buddy memory the static does not own.
        let len = MEMORY_SIZE;
        log::info!("MEMORY = {:#?}", start..start.add(len));
        let mut heap = HEAP.0.lock();
        let ptr = NonNull::new_unchecked(start);
        heap.init(core::mem::size_of::<usize>().trailing_zeros() as _, ptr);
        heap.transfer(ptr, len);
        // This block is memory the heap manages, so it belongs in the total the
        // heap reports -- `insert_regions` is the only other place that adds to
        // it, and it runs later in boot. Without this line `heap_total()` was
        // **zero** for the whole early-boot window and short by `MEMORY_SIZE`
        // for ever after, and the reader that matters is the OOM banner in
        // `lang.rs`: an allocation failure before `insert_regions` printed
        // "used N / total 0 MiB", which reads as a broken counter rather than
        // as the heap genuinely being the 2 MiB boot pool. `stats()` quoted the
        // same short number to userspace.
        TOTAL_MEMORY.fetch_add(len, Ordering::Relaxed);
    }
}

/// 将一些内存区域注册到分配器。
pub fn insert_regions(regions: &[Range<PhysAddr>]) {
    let mut heap = HEAP.0.lock();
    let offset = phys_to_virt_offset();
    regions
        .iter()
        // Not just tidiness: an empty range's `len()` is 0, so the `transfer`
        // below would be a no-op, but `NonNull::new_unchecked` still runs on
        // `region.start + offset` -- and a firmware table reporting a
        // zero-length range at address 0 makes that a null `NonNull`, which is
        // undefined behaviour whatever the length beside it says. A reversed
        // range (start > end) reads as empty here too, and is the shape a
        // mis-parsed table produces.
        .filter(|region| !region.is_empty())
        .for_each(|region| unsafe {
            heap.transfer(
                NonNull::new_unchecked((region.start + offset) as *mut u8),
                region.len(),
            );
            TOTAL_MEMORY.fetch_add(region.len(), Ordering::Relaxed);
        });
}

pub fn frame_alloc(frame_count: usize, align_log2: usize) -> Option<PhysAddr> {
    // No frames is no allocation. Said here because the `NonZeroUsize` below
    // used to be `new_unchecked`, so a count of zero was undefined behaviour
    // rather than a `None`, and because the give-back further down would
    // otherwise hand the whole block straight back and return a pointer into
    // freed memory.
    if frame_count == 0 {
        return None;
    }
    // `checked_shl` is **not** an overflow guard: it refuses a shift amount of
    // `usize::BITS` or more and nothing else, while `<<` silently discards the
    // high bits. So a large `frame_count` came out of here as a *smaller*
    // `want`, and the rest of the function then handed out a run of `want`
    // bytes while the caller -- `PhysFrame::new_contiguous`, which builds
    // `frame_count` frames from the base it gets -- treated `frame_count` pages
    // as its own. Those pages past the end of the run are other allocations'
    // memory. `checked_mul` is the question that was meant.
    let want = frame_count.checked_mul(1 << PAGE_BITS)?;
    // And the request has to be something the allocator can express: it rounds
    // the size up to a power of two, and that round-up overflows above
    // `1 << (usize::BITS - 1)` -- `attempt to add with overflow` inside the
    // buddy in a debug kernel, and a wrap to zero in a release one, for a
    // request that should simply have been refused.
    want.checked_next_power_of_two()?;
    // `align_log2` arrives in FRAMES -- `VMObjectPaged::new_contiguous` hands
    // on `align_log2 - PAGE_SIZE_LOG2` -- and `allocate` wants an ORDER in
    // bytes: `allocate_layout` passes it `layout.align().trailing_zeros()`. The
    // two differ by `PAGE_BITS`, so the conversion is an ADDITION.
    //
    // It used to be `align_log2 << PAGE_BITS`, which shifts a log2 as if it
    // were a count: `align_log2 = 1` asked for an alignment order of 4096, that
    // is 2^4096 bytes. No layer of the buddy will honour an alignment that
    // large, so every aligned request fell through to the oligarchy -- the
    // max-order free list -- and took a max-order block to hold a couple of
    // pages, or answered `NO_MEMORY` with the block it wanted lying free. Only
    // `align_log2 == 0` came out right, which is every caller that is not
    // asking for alignment, which is why nothing noticed.
    let align_order = align_log2.checked_add(PAGE_BITS)?;
    let align = 1usize.checked_shl(align_order as u32)?;
    // And ask for at least `align` bytes, because that is the only alignment
    // this allocator can actually promise: it hands out a block at its own
    // order, `idx << size_order`, so what comes back is aligned to
    // `next_power_of_two(size)` and no more. Its `align_order` argument is
    // turned into a per-layer index alignment by a shift, which comes out zero
    // for every alignment smaller than a layer's block, so asking for a coarse
    // alignment on a small block is answered by a block that does not have it.
    // Sizing the request for the alignment is what makes the answer true, and
    // it is what the caller needs: these frames back `zx_vmo_create_contiguous`
    // and DMA buffers, where an unaligned buffer is worse than a refusal.
    let bytes = NonZeroUsize::new(want.max(align))?;
    let (ptr, size) = HEAP.0.lock().allocate::<u8>(align_order, bytes).ok()?;
    assert_eq!(size, bytes.get());
    let base = ptr.as_ptr() as usize;
    // Hand back the pages the alignment asked for and the caller did not, or
    // they are lost until reboot: `frame_alloc` reports only the base, and the
    // caller frees `frame_count` pages and no more.
    if size > want {
        HEAP.0.lock().deallocate(
            unsafe { NonNull::new_unchecked((base + want) as *mut u8) },
            size - want,
        );
    }
    // [diag] These frames back userspace VMOs, and they come out of the SAME
    // buddy arena as the kernel's coroutine stacks (the `GlobalAlloc` impl
    // above locks this very heap). Handing out a block that is already a live
    // stack would reproduce docs/README-crash-repro.md exactly: the VMO is
    // zero-filled on creation, blanking a live kernel stack (`rip=0x0`,
    // `[rsp0..3]=0`, `region=usable` — an overflow would have faulted in the
    // unmapped guard instead), and userspace writing its buffer afterwards
    // sprays arbitrary bytes over kernel stacks and heap objects.
    //
    // Checked here rather than via a canary or watchpoint because this fires
    // at hand-out — naming the aliasing BEFORE anything is corrupted. The
    // registry is plain atomics, so the cost is a short scan of relaxed loads
    // (the heap lock above is already released by this point).
    frame_alias_check(base, want);
    // What the caller keeps, which is what `frame_dealloc` will subtract again,
    // one page at a time.
    USED_MEMORY.fetch_add(want, Ordering::Relaxed);
    Some(base as PhysAddr - phys_to_virt_offset())
}

/// [diag] Panic loudly if a just-allocated frame range overlaps a live kernel
/// coroutine stack. A no-op on libos, where the scheduler's stacks are not
/// carved out of this heap.
#[cfg(not(feature = "libos"))]
fn frame_alias_check(vaddr: usize, size: usize) {
    if let Some(stack_base) = executor::overlapping_live_stack(vaddr, size) {
        kernel_hal::console::serial_write_fmt_spin(format_args!(
            "\n[frame-alias] ALLOCATOR HANDED OUT A LIVE KERNEL STACK\n\
             [frame-alias]   frames  {:#x}..{:#x} ({} bytes)\n\
             [frame-alias]   overlaps coroutine stack alloc_base={:#x}\n\
             [frame-alias] this block is about to back a userspace VMO: the\n\
             [frame-alias] zero-fill alone will blank a live kernel stack.\n",
            vaddr,
            vaddr + size,
            size,
            stack_base,
        ));
        panic!(
            "frame_alloc aliased a live coroutine stack: frames {:#x}..{:#x} vs stack {:#x}",
            vaddr,
            vaddr + size,
            stack_base
        );
    }
}

#[cfg(feature = "libos")]
fn frame_alias_check(_vaddr: usize, _size: usize) {}

/// [diag] Panic loudly if a just-allocated kernel-heap block (`Box`/`Vec`/…)
/// overlaps a live coroutine stack. The `GlobalAlloc` twin of
/// [`frame_alias_check`], on the buddy arena's other consumer — the one that
/// zeroing allocations reach without ever touching `frame_alloc`.
///
/// Uses `alloc_overlaps_live_stack` (not `overlapping_live_stack`) because
/// that is the helper reading `STACK_REG_BASE`, the registry kept for this
/// hook. It carries no exemption for a fresh stack flagging itself, and needs
/// none: coroutine stacks are allocated through this very `GlobalAlloc`, so
/// this check runs *as part of* that allocation and the registry inserts
/// happen on the line after it returns. This comment used to claim such an
/// exemption existed; it never did, and a promised exemption in front of a
/// `panic!` is worse than no comment at all.
/// A no-op on libos, where scheduler stacks are not carved out of this heap.
#[cfg(not(feature = "libos"))]
fn heap_alias_check(vaddr: usize, size: usize) {
    if let Some(stack_base) = executor::alloc_overlaps_live_stack(vaddr, size) {
        kernel_hal::console::serial_write_fmt_spin(format_args!(
            "\n[heap-alias] ALLOCATOR HANDED OUT A LIVE KERNEL STACK\n\
             [heap-alias]   block   {:#x}..{:#x} ({} bytes)\n\
             [heap-alias]   overlaps coroutine stack alloc_base={:#x}\n\
             [heap-alias] this block is about to back a kernel heap object: a\n\
             [heap-alias] zeroed allocation alone will blank a live kernel stack.\n",
            vaddr,
            vaddr + size,
            size,
            stack_base,
        ));
        panic!(
            "GlobalAlloc aliased a live coroutine stack: block {:#x}..{:#x} vs stack {:#x}",
            vaddr,
            vaddr + size,
            stack_base
        );
    }
}

#[cfg(feature = "libos")]
fn heap_alias_check(_vaddr: usize, _size: usize) {}

pub fn frame_dealloc(target: PhysAddr) {
    USED_MEMORY.fetch_sub(1 << PAGE_BITS, Ordering::Relaxed);
    HEAP.0.lock().deallocate(
        unsafe { NonNull::new_unchecked((target + phys_to_virt_offset()) as *mut u8) },
        1 << PAGE_BITS,
    );
}

pub fn stats() -> (usize, usize) {
    (
        USED_MEMORY.load(Ordering::Relaxed),
        TOTAL_MEMORY.load(Ordering::Relaxed),
    )
}

/// Bytes of heap currently in use (mirrors `memory_x86_64::heap_used`).
#[allow(dead_code)]
pub fn heap_used() -> usize {
    USED_MEMORY.load(Ordering::Relaxed)
}

/// Whether this CPU may allocate right now — fault/panic paths only.
///
/// A real `try_lock`, like its x86_64 twin. The comment that used to sit here
/// said this build "does not own the global allocator's lock", so there was no
/// self-deadlock to avoid and the answer could always be yes. That was false:
/// `HEAP` above *is* this build's `#[global_allocator]`, and `Mutex` is a
/// spinlock that is not re-entrant, so a fault taken inside `alloc`/`dealloc`
/// on a path that then allocates waits, with interrupts off, for a release only
/// this CPU could perform. Answering `true` there is the one answer that hangs
/// the machine, and it is exactly the answer a fault path asks this function to
/// avoid.
#[allow(dead_code)]
pub fn heap_available() -> bool {
    match HEAP.0.try_lock() {
        Some(guard) => {
            drop(guard);
            true
        }
        None => false,
    }
}

/// Whether THIS cpu is already inside the heap's critical section (mirrors
/// `memory_x86_64::heap_held_by_current_cpu`).
#[allow(dead_code)]
pub fn heap_held_by_current_cpu() -> bool {
    use lock::HeldByCurrentCpu;
    // Via the APIC, for the reason given on the x86_64 twin: the only caller
    // is the fault reporter, and GS is one of the things a fault may have
    // smashed.
    HEAP.0.held_by_current_cpu_via_apic()
}

/// Total bytes managed by the heap (mirrors `memory_x86_64::heap_total`).
#[allow(dead_code)]
pub fn heap_total() -> usize {
    TOTAL_MEMORY.load(Ordering::Relaxed)
}

/// Allocations refused by the heap re-entrancy guard (mirrors
/// `memory_x86_64::heap_reentrancy_events`). This build has no such guard.
#[allow(dead_code)]
pub fn heap_reentrancy_events() -> u32 {
    0
}

/// Wild blocks refused by the allocator (mirrors
/// `memory_x86_64::heap_wild_blocks`). This build does not check.
#[allow(dead_code)]
pub fn heap_wild_blocks() -> u32 {
    0
}

/// Free blocks found written after their free (mirrors
/// `memory_x86_64::heap_written_after_free`). This build does not check.
#[allow(dead_code)]
pub fn heap_written_after_free() -> u32 {
    0
}

/// The allocator this file is, driven on the host.
///
/// Everything here is a process global: one `HEAP`, two counters and one 2 MiB
/// static, and `init()` is not idempotent -- calling it twice re-initialises
/// the buddy and hands it the same static a second time, which is two owners
/// for one block. So every test runs behind one turnstile, and `init()` runs
/// exactly once inside it.
///
/// The turnstile is also what makes the counter assertions exact: this build
/// does not install `HEAP` as the `#[global_allocator]` (see the `cfg_attr`
/// above), so nothing but these tests moves `USED_MEMORY` and `TOTAL_MEMORY`.
#[cfg(test)]
mod heap_tests {
    use super::*;
    use core::alloc::GlobalAlloc;

    const PAGE: usize = 1 << PAGE_BITS;

    /// A page-aligned run of real memory handed to the allocator once, so the
    /// frame tests have something a frame can come out of.
    ///
    /// The 2 MiB boot static is **not** enough on its own: it is a `[u8; N]`,
    /// so its alignment is 1, and the buddy splits a transferred block by the
    /// alignment of its address -- with the static landing on an odd address
    /// the largest block inside it is a byte. On a real board `init()` is
    /// followed by `insert_regions` with page-aligned firmware ranges, and that
    /// is where frames come from; this is that step.
    const REGION_ALIGN: usize = 2 * 1024 * 1024;
    const REGION_SIZE: usize = 4 * 1024 * 1024;

    /// What the heap held the instant `init()` returned, before this fixture
    /// hands it anything else. Recorded because the region below hides the boot
    /// pool in every later reading: a total of 4 MiB is "at least 2 MiB"
    /// whether or not `init()` counted its own block, and a mutation test said
    /// so -- both the missing `fetch_add` and a half-sized transfer survived
    /// until these two numbers were kept.
    static TOTAL_AFTER_INIT: AtomicUsize = AtomicUsize::new(usize::MAX);
    static CAPACITY_AFTER_INIT: AtomicUsize = AtomicUsize::new(usize::MAX);

    #[must_use = "bind it to `_alone`: a bare `_` releases the turnstile at once"]
    fn alone_with_the_heap() -> spin::MutexGuard<'static, ()> {
        static GUARD: spin::Mutex<()> = spin::Mutex::new(());
        static READY: spin::Once<()> = spin::Once::new();
        let guard = GUARD.lock();
        READY.call_once(|| {
            init();
            TOTAL_AFTER_INIT.store(heap_total(), Ordering::Relaxed);
            CAPACITY_AFTER_INIT.store(HEAP.0.lock().capacity(), Ordering::Relaxed);
            let layout = Layout::from_size_align(REGION_SIZE, REGION_ALIGN).unwrap();
            // Leaked on purpose: the allocator keeps blocks out of it for the
            // rest of the process, so it must outlive every test.
            let base = unsafe { alloc::alloc::alloc(layout) } as usize;
            assert!(
                base != 0,
                "the host allocator would not give us a test region"
            );
            insert_regions(&[base..base + REGION_SIZE]);
        });
        guard
    }

    /// Frees a run `frame_alloc` handed out, the way the kernel does it: one
    /// page at a time (`kernel-hal`'s `frame_dealloc` loop), which is also what
    /// makes the used counter come back to where it started.
    fn free_run(base: PhysAddr, frame_count: usize) {
        for i in 0..frame_count {
            frame_dealloc(base + i * PAGE);
        }
    }

    // --- the boot pool itself ---------------------------------------------

    /// The bug, and the one this file could never have found on its own: the
    /// 2 MiB boot pool is a `[u8; N]`, whose `align_of` is **1**, and `init()`
    /// hands it to a buddy whose minimum order is `size_of::<usize>()`. The
    /// buddy splits a transferred block by the alignment of its address, so an
    /// address less aligned than that minimum makes it compute
    /// `order - min_order` with `order` smaller -- an underflow, inside
    /// `init()`, before there is a heap to format a panic with. The kernel
    /// links were getting away with it on the linker's goodwill; on the host
    /// the static landed on an odd address and `init()` died.
    #[test]
    fn the_boot_pool_is_aligned_for_the_allocator_that_receives_it() {
        let base = core::ptr::addr_of!(MEMORY) as usize;
        let min_order = core::mem::size_of::<usize>().trailing_zeros();
        assert!(
            base.trailing_zeros() >= min_order,
            "the boot pool is at {:#x}, aligned to 2^{}, and the buddy is told its \
             minimum order is {}: transferring it underflows `order - min_order`",
            base,
            base.trailing_zeros(),
            min_order
        );
        assert_eq!(
            base % PAGE,
            0,
            "the boot pool backs frame_alloc too, so it has to start on a page"
        );
    }

    /// And the whole pool has to reach the allocator. `init()` used to write
    /// the length out by hand beside `MEMORY_SIZE`; the buddy's own `capacity`
    /// is the answer that does not go through this file's counters, so it says
    /// what was really transferred.
    #[test]
    fn the_whole_boot_pool_reaches_the_allocator() {
        let _alone = alone_with_the_heap();
        assert_eq!(
            core::mem::size_of::<BootPool>(),
            MEMORY_SIZE,
            "aligning the pool changed its size"
        );
        assert_eq!(
            CAPACITY_AFTER_INIT.load(Ordering::Relaxed),
            MEMORY_SIZE,
            "init() handed the allocator something other than the whole pool: the \
             length it transfers and the size of the static have to be the same \
             number, and they were written out separately"
        );
    }

    // --- what the heap says it manages -----------------------------------

    /// The bug. `init()` transferred the 2 MiB boot pool to the buddy and did
    /// not add it to `TOTAL_MEMORY`, and `insert_regions` -- the only other
    /// writer -- runs later in boot. So `heap_total()` was **0** for the whole
    /// early window, and the reader is the OOM banner in `lang.rs`: a failed
    /// allocation there printed "used N / total 0 MiB".
    #[test]
    fn the_boot_pool_counts_as_memory_the_heap_manages() {
        let _alone = alone_with_the_heap();
        assert_eq!(
            TOTAL_AFTER_INIT.load(Ordering::Relaxed),
            MEMORY_SIZE,
            "the instant init() returned, the heap held the 2 MiB boot pool and \
             reported a total of that much instead: a total smaller than what the \
             heap holds makes every reader of it -- the OOM banner in lang.rs, the \
             stats syscall -- quote a number that cannot be true"
        );
        // And it stays true as regions arrive: what the heap reports as its
        // total is what the allocator was actually handed, which is the
        // invariant, and it does not depend on this fixture's own region.
        assert_eq!(
            heap_total(),
            HEAP.0.lock().capacity(),
            "the reported total and what the allocator was given have drifted"
        );
    }

    #[test]
    fn the_regions_the_firmware_reports_are_added_to_what_the_heap_manages() {
        let _alone = alone_with_the_heap();
        let before = heap_total();
        let layout = Layout::from_size_align(2 * PAGE, PAGE).unwrap();
        let base = unsafe { alloc::alloc::alloc(layout) } as usize;
        assert!(base != 0);
        insert_regions(&[base..base + 2 * PAGE]);
        assert_eq!(heap_total(), before + 2 * PAGE);
    }

    /// A firmware table that reports a range backwards, or a zero-length one,
    /// must not reach `transfer`: its `len()` is 0, so it would add nothing and
    /// hand the buddy a pointer to memory the range does not describe.
    #[test]
    fn an_empty_or_reversed_region_is_not_handed_to_the_allocator() {
        let _alone = alone_with_the_heap();
        let before = heap_total();
        let somewhere = core::ptr::addr_of!(MEMORY) as usize;
        insert_regions(&[somewhere..somewhere, (somewhere + PAGE)..somewhere]);
        assert_eq!(
            heap_total(),
            before,
            "an empty or reversed range was counted as memory"
        );
    }

    #[test]
    fn stats_says_the_same_two_numbers_as_the_readers_beside_it() {
        let _alone = alone_with_the_heap();
        assert_eq!(stats(), (heap_used(), heap_total()));
    }

    // --- frame_alloc: the two refusals -----------------------------------

    /// No frames is no allocation. It has to be an early `None`: the
    /// `NonZeroUsize` below used to be `new_unchecked`, and the give-back
    /// further down would otherwise hand the whole block straight back and
    /// return a pointer into freed memory.
    #[test]
    fn no_frames_is_no_allocation_and_costs_nothing() {
        let _alone = alone_with_the_heap();
        let used = heap_used();
        assert_eq!(frame_alloc(0, 0), None);
        assert_eq!(heap_used(), used, "a refused allocation moved the counter");
    }

    /// The bug the guard was supposed to catch and did not. `checked_shl` only
    /// refuses a shift amount of `usize::BITS` or more, and `<<` discards the
    /// high bits, so a frame count whose byte count does not fit came out as a
    /// **smaller** one -- and the caller
    /// (`PhysFrame::new_contiguous` -> `zx_vmo_create_contiguous`) builds
    /// `frame_count` frames from the base it is handed, so the pages past the
    /// end of the short run are somebody else's memory. The exact multiple of
    /// `2^52` truncated all the way to zero and asked for a single page.
    #[test]
    fn a_frame_count_whose_byte_count_does_not_fit_is_refused() {
        let _alone = alone_with_the_heap();
        let used = heap_used();
        for frame_count in [
            usize::MAX,
            // (usize::MAX >> PAGE_BITS) + 1: the byte count is exactly 2^64, so
            // the shift truncated it to 0 and the request became one page.
            1 + (usize::MAX >> PAGE_BITS),
            // Fits in a usize but its round-up to a power of two does not, which
            // is what the allocator does to it first.
            1 + (1usize << (usize::BITS as usize - 1 - PAGE_BITS)),
        ] {
            assert_eq!(
                frame_alloc(frame_count, 0),
                None,
                "a run of {} frames was not refused",
                frame_count
            );
        }
        // And an alignment that cannot be named in a usize.
        assert_eq!(frame_alloc(1, usize::BITS as usize), None);
        assert_eq!(frame_alloc(1, usize::MAX), None);
        assert_eq!(heap_used(), used, "a refused allocation moved the counter");
    }

    // --- frame_alloc: the alignment ---------------------------------------

    /// The bug that hid for a long time: the conversion from an alignment in
    /// FRAMES to the allocator's alignment ORDER in bytes is an **addition**,
    /// and it was written `align_log2 << PAGE_BITS`, which shifts a log2 as if
    /// it were a count. `align_log2 = 1` then asked for an alignment order of
    /// 4096, that is 2^4096 bytes. Only `align_log2 == 0` came out right, which
    /// is every caller that is not asking for alignment -- which is why nothing
    /// noticed.
    #[test]
    fn a_run_is_aligned_to_the_number_of_frames_the_caller_asked_for() {
        let _alone = alone_with_the_heap();
        for align_log2 in 0..=6 {
            let base = frame_alloc(1, align_log2)
                .unwrap_or_else(|| panic!("no frame for align_log2={}", align_log2));
            assert_eq!(
                base % (PAGE << align_log2),
                0,
                "align_log2={} asked for {}-byte alignment and got {:#x}",
                align_log2,
                PAGE << align_log2,
                base
            );
            free_run(base, 1);
        }
    }

    #[test]
    fn a_multi_frame_run_is_aligned_too() {
        let _alone = alone_with_the_heap();
        let base = frame_alloc(3, 4).expect("no 3-frame run aligned to 16 frames");
        assert_eq!(base % (PAGE << 4), 0);
        free_run(base, 3);
    }

    /// The give-back. `frame_alloc` inflates the request to the alignment,
    /// because a block aligned to `2^order` is the only thing this allocator
    /// can promise, and then hands the surplus back -- it reports only the
    /// base, and the caller frees `frame_count` pages and no more, so anything
    /// not given back is lost until reboot.
    #[test]
    fn the_pages_the_alignment_asked_for_and_the_caller_did_not_go_back() {
        let _alone = alone_with_the_heap();
        let used = heap_used();
        // 1 frame aligned to 64: the request is inflated to 64 frames, so 63
        // of them are surplus.
        let base = frame_alloc(1, 6).expect("no frame aligned to 64 frames");
        assert_eq!(
            heap_used(),
            used + PAGE,
            "the counter must charge the caller ONE page, not the whole aligned block"
        );
        free_run(base, 1);
        assert_eq!(heap_used(), used);
        // And the surplus really went back to the heap rather than into a hole.
        // Asked of the buddy's own free count, not of a second allocation
        // succeeding: with 6 MiB of pool a leak of 252 KiB per call takes a
        // couple of dozen calls to show, so "it still works" is not an answer.
        let free_before = HEAP.0.lock().free();
        let base = frame_alloc(1, 6).expect("no frame aligned to 64 frames");
        free_run(base, 1);
        assert_eq!(
            HEAP.0.lock().free(),
            free_before,
            "taking one frame aligned to 64 and giving it back cost the heap \
             memory: the 63 frames the alignment asked for and the caller did \
             not are lost until reboot"
        );
    }

    // --- the counters ------------------------------------------------------

    #[test]
    fn what_frame_alloc_charges_is_what_frame_dealloc_gives_back() {
        let _alone = alone_with_the_heap();
        for frame_count in [1usize, 2, 3, 5, 8] {
            let used = heap_used();
            let base = frame_alloc(frame_count, 0).expect("no run");
            assert_eq!(
                heap_used(),
                used + frame_count * PAGE,
                "a {}-frame run charged the wrong number of bytes",
                frame_count
            );
            free_run(base, frame_count);
            assert_eq!(
                heap_used(),
                used,
                "freeing a {}-frame run one page at a time left bytes on the counter",
                frame_count
            );
        }
    }

    /// The documented bug on the heap side: `alloc` counted the block the buddy
    /// rounded the request up to, and `dealloc` subtracts `layout.size()`. The
    /// difference stayed on the counter at every allocation that was not
    /// already a multiple of the minimum block, so `heap_used()` climbed for
    /// ever and eventually passed `heap_total()`. Both sides must count the
    /// **requested** size.
    #[test]
    fn the_heap_counts_the_size_the_caller_asked_for_on_both_sides() {
        let _alone = alone_with_the_heap();
        // Deliberately not a multiple of the minimum block, and not a power of
        // two: this is the shape the old code leaked on.
        for size in [1usize, 3, 7, 17, 100, 1000, 4095] {
            let used = heap_used();
            let layout = Layout::from_size_align(size, 1).unwrap();
            let ptr = unsafe { HEAP.alloc(layout) };
            assert!(!ptr.is_null(), "the heap refused {} bytes", size);
            assert_eq!(
                heap_used(),
                used + size,
                "allocating {} bytes charged something other than {} bytes",
                size,
                size
            );
            unsafe { HEAP.dealloc(ptr, layout) };
            assert_eq!(
                heap_used(),
                used,
                "an allocation of {} bytes left bytes on the counter when freed",
                size
            );
        }
    }

    /// And what it hands back is usable memory of at least the size asked for,
    /// aligned as asked. Worth pinning separately from the counter: a counter
    /// can be right about a block that is wrong.
    #[test]
    fn the_heap_hands_back_a_block_that_is_aligned_and_big_enough() {
        let _alone = alone_with_the_heap();
        for (size, align) in [(1usize, 1usize), (24, 8), (100, 64), (4096, 4096)] {
            let layout = Layout::from_size_align(size, align).unwrap();
            let ptr = unsafe { HEAP.alloc(layout) };
            assert!(!ptr.is_null(), "the heap refused {} bytes", size);
            assert_eq!(
                ptr as usize % align,
                0,
                "{} bytes aligned to {} came back at {:p}",
                size,
                align,
                ptr
            );
            // Writable end to end, and distinct from anything else live.
            unsafe { core::ptr::write_bytes(ptr, 0xa5, size) };
            assert!(unsafe { core::slice::from_raw_parts(ptr, size) }
                .iter()
                .all(|b| *b == 0xa5));
            unsafe { HEAP.dealloc(ptr, layout) };
        }
    }

    /// Two live blocks never overlap, which is the one thing an allocator is
    /// for. Checked with the frame path, because that is the one with the
    /// give-back arithmetic in it.
    #[test]
    fn two_live_runs_do_not_overlap() {
        let _alone = alone_with_the_heap();
        let mut runs = alloc::vec::Vec::new();
        for _ in 0..16 {
            let base = frame_alloc(2, 1).expect("no run");
            runs.push(base);
        }
        for (i, a) in runs.iter().enumerate() {
            for b in runs.iter().skip(i + 1) {
                assert!(
                    a + 2 * PAGE <= *b || b + 2 * PAGE <= *a,
                    "two live 2-frame runs overlap: {:#x} and {:#x}",
                    a,
                    b
                );
            }
        }
        for base in runs {
            free_run(base, 2);
        }
    }

    // --- the fault-path question -----------------------------------------

    /// The other bug. This used to answer `true` unconditionally, on the
    /// grounds -- written in its own comment -- that this build "does not own
    /// the global allocator's lock". It does: `HEAP` is this build's
    /// `#[global_allocator]`. The answer a fault path needs is whether taking
    /// the lock would block, and with a non-re-entrant spinlock held by this
    /// very CPU the honest answer is no.
    #[test]
    fn the_heap_is_not_available_while_this_cpu_holds_its_lock() {
        let _alone = alone_with_the_heap();
        assert!(heap_available(), "an idle heap should be available");
        let held = HEAP.0.lock();
        assert!(
            !heap_available(),
            "heap_available() said yes with the heap lock held by this very CPU: a \
             fault path that believes it allocates into a lock only it can release"
        );
        drop(held);
        assert!(
            heap_available(),
            "the heap stayed unavailable after the release"
        );
    }

    /// This build has no re-entrancy guard, so the count is zero. Pinned
    /// because `lang.rs` prints a line only when it is non-zero, and a stub
    /// that started returning something else would put a false "the heap may
    /// not be out of memory" note in every OOM report.
    #[test]
    fn this_build_reports_no_re_entrancy_refusals() {
        let _alone = alone_with_the_heap();
        assert_eq!(heap_reentrancy_events(), 0);
    }
}
