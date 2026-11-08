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
use kernel_hal::PhysAddr;
use lock::UnfairMutex;

static TOTAL_MEMORY: AtomicUsize = AtomicUsize::new(0);
static USED_MEMORY: AtomicUsize = AtomicUsize::new(0);

/// 堆分配器。
///
/// 27 + 6 + 3 = 36 -> 64 GiB
/// Behind an UNFAIR lock (test-and-set, `lock::UnfairMutex`), not the ticket
/// lock the rest of the kernel uses, and on purpose.
///
/// A ticket lock hands the lock to ONE named waiter, the next ticket. When
/// that waiter is a vCPU the host has descheduled, every other CPU spins
/// until the host runs it again, however free the lock is. This allocator
/// is the kernel's hottest lock on riscv64 and aarch64 (every `Box`, every
/// `Vec` growth, every frame of an eager `fork` copy comes through here with
/// no per-CPU front cache, unlike `memory_x86_64.rs`), so once the CPUs
/// queue on it the convoy feeds itself.
///
/// Measured on `Linux Libc Test Baremetal (aarch64)`'s own configuration,
/// `-smp 6` on a 4-core host, reproduced here on an idle machine: with the
/// ticket lock the lock was FREE in 85% of samples with all 6 CPUs holding a
/// ticket, 93% of all CPU time went to spins of 16k+ turns averaging 9 ms
/// per acquisition, `fork` copied memory at 6 MB/s and `eclipse-init` took
/// 236-261 s to settle, past the harness's 300 s prompt budget once the
/// kernel boot is added. With 1 or 2 CPUs (no oversubscription) the same
/// kernel copied at 1.3 GB/s and settled in 8 s, so the work was never the
/// problem, the handoff was. With this lock, `-smp 6`: 255 MB/s, settled
/// in 19 s, 12k idle naps where there had been 1.7k -- the riscv64 figures.
///
/// Linux reaches the same conclusion for the same reason: on a hypervisor
/// without paravirt spinlock support it drops its fair qspinlock for a plain
/// test-and-set one, "because fair locks have terrible lock 'holder'
/// preemption issues" (`virt_spin_lock`, arch/x86/include/asm/qspinlock.h).
/// riscv64 and aarch64 Eclipse run under QEMU; x86_64 has its own allocator
/// and keeps its ticket lock.
///
/// Unfair means a CPU can lose the race repeatedly, never for ever: the
/// critical sections here are short (a buddy allocate or free) and the
/// contending CPUs all go on to do other work between acquisitions. The
/// `SpinMutex` keeps the deadlock forensics (holder file:line, the 8 s
/// stuck report) and the shootdown pump the ticket lock has.
struct LockedHeap(UnfairMutex<BuddyAllocator<27, UsizeBuddy, LinkedListBuddy>>);

// Not the global allocator in the test binary: the harness allocates long
// before anything calls `init()`, so installing this over the 2 MiB static
// would hand the test runner an empty heap. The allocator itself is still
// compiled and still reachable -- the tests call `init()` and then drive
// `HEAP` directly, which is what the kernel's own `GlobalAlloc` impl does.
// Not the global allocator under libos: there the kernel's heap is the host's
// (`std`), as in `memory_x86_64.rs`, and this buddy serves FRAMES only. Making
// it the heap as well put the 2 MiB boot pool -- a static of this binary,
// nowhere near the hosted "physical" window -- in the same pool as the window,
// so `frame_alloc` could answer a request with a block of the static and the
// paddr it computed (`base - window`) wrapped: `assertion failed:
// paddr + len <= PMEM_SIZE` in `pmem_zero`, from `PhysFrame::new_zero` while
// loading userboot, measured in an arm64 VM.
#[cfg_attr(all(not(test), not(feature = "libos")), global_allocator)]
static HEAP: LockedHeap = LockedHeap(UnfairMutex::new(BuddyAllocator::new()));

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
#[cfg(not(feature = "libos"))]
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
#[cfg(not(feature = "libos"))]
#[repr(align(4096))]
// The field is never read *through the type*: `init()` takes the static's
// address with `addr_of_mut!` (a `&mut` to a `static mut` is what that macro
// exists to avoid) and hands it to the allocator as bytes. The array is here to
// reserve the space and to carry the alignment, which is the whole point.
struct BootPool(#[allow(dead_code)] [u8; MEMORY_SIZE]);

#[cfg(not(feature = "libos"))]
static mut MEMORY: BootPool = BootPool([0u8; MEMORY_SIZE]);

unsafe impl GlobalAlloc for LockedHeap {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // See `redzone`: with `mem-debug` every block carries a canary after
        // its payload, checked when the block is freed.
        #[cfg(feature = "mem-debug")]
        let Some(padded) = redzone::grow(layout) else {
            handle_alloc_error(layout)
        };
        #[cfg(not(feature = "mem-debug"))]
        let padded = layout;
        // Bind the allocation to a local so the heap lock (a temporary
        // `MutexGuard`) is dropped at the end of THIS statement, before the
        // alias check runs. `heap_alias_check` may panic into the BSOD painter,
        // which must never re-enter this lock.
        // Small blocks come from this CPU's front cache when it has one of
        // the size, and otherwise from the buddy at the cache's size class,
        // so that every small block the buddy hands out can be cached on its
        // way back. See `front`.
        #[cfg(not(feature = "mem-debug"))]
        let padded = front::class_layout(padded).unwrap_or(padded);
        #[cfg(not(feature = "mem-debug"))]
        let cached = front::try_alloc(padded);
        #[cfg(feature = "mem-debug")]
        let cached: Option<(NonNull<u8>, usize)> = None;
        let alloc = match cached {
            Some(block) => Ok(block),
            None => {
                let first = {
                    let mut heap = self.0.lock();
                    #[cfg(feature = "mem-debug")]
                    let _inside = lockcheck::Inside::enter();
                    heap.allocate_layout(padded)
                };
                // Out of memory with blocks parked in the caches: give them
                // back to the buddy, where they can merge, and ask once more.
                #[cfg(not(feature = "mem-debug"))]
                let first = match first {
                    Err(_)
                        if front::drain(|block, layout| {
                            self.0.lock().deallocate_layout(block, layout)
                        }) > 0 =>
                    {
                        self.0.lock().allocate_layout(padded)
                    }
                    other => other,
                };
                first
            }
        };
        match alloc {
            Ok((ptr, size)) => {
                #[cfg(feature = "mem-debug")]
                {
                    quarantine::assert_not_parked(ptr.as_ptr() as usize, size);
                    redzone::arm(ptr.as_ptr(), layout.size());
                }
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
        // The canary first (it reads the bytes the poison below overwrites),
        // then the padded layout, which is what the buddy was asked for.
        #[cfg(feature = "mem-debug")]
        let layout = {
            redzone::check(ptr, layout);
            // The block was handed out with this padded layout, so it exists.
            redzone::grow(layout).expect("a freed block was allocated with its redzone")
        };
        // See `quarantine`: with `mem-debug` the block is poisoned and parked,
        // and what reaches the buddy is whatever the parking evicted.
        #[cfg(feature = "mem-debug")]
        quarantine::park_and_drain(
            quarantine::Parked {
                addr: ptr as usize,
                size: layout.size(),
                align: layout.align(),
            },
            give_back_to_buddy,
        );
        #[cfg(not(feature = "mem-debug"))]
        {
            let layout = front::class_layout(layout).unwrap_or(layout);
            let block = NonNull::new(ptr).unwrap();
            if !front::try_free(block, layout) {
                self.0.lock().deallocate_layout(block, layout)
            }
        }
    }
}

/// Per-CPU caches of small free blocks in front of the buddy.
///
/// Every `Box`, `Arc` and small `Vec` on aarch64 and riscv64 used to take the
/// one heap lock, and a free in the buddy is a sorted insert into its free
/// list, linear in the list's length. Two CPUs exchanging channel messages
/// spent about 60% of their time there (QMP PC sampling of `zx-ipc-bench`
/// with two processes on `-smp 2`), half of it spinning on the lock the other
/// CPU held for its walk. `memory_x86_64.rs` has had a front cache for the
/// same reason; this is the plain version of it.
///
/// A block from 8 bytes to 4 KiB is allocated at its size class (the next
/// power of two of its size and alignment) on both the alloc and the free
/// side, so any block of a class can serve any request of that class and
/// goes back to the buddy with the layout it came out with. A cache is a
/// LIFO of free blocks per class, linked through the blocks' first word,
/// capped per class so a CPU parks at most a few tens of KiB.
///
/// Each CPU's cache is taken with `try_lock`: an interrupt that allocates
/// while its CPU is inside its own cache finds it busy and goes to the
/// buddy, and so does a CPU that lands on another's slot after a migration
/// or during a drain. Nothing waits on a cache.
#[cfg(not(feature = "mem-debug"))]
mod front {
    use core::alloc::Layout;
    use core::ptr::NonNull;

    /// Classes 2^3 (8 B, a link's size and the buddy's smallest block) to
    /// 2^12 (4 KiB).
    const MIN_CLASS: usize = 3;
    const MAX_CLASS: usize = 12;
    const CLASSES: usize = MAX_CLASS - MIN_CLASS + 1;
    /// Bytes a CPU may park in one class; the count is this over the size,
    /// between [`MIN_CAP`] and [`MAX_CAP`].
    const CLASS_BUDGET: usize = 16 * 1024;
    const MIN_CAP: usize = 4;
    const MAX_CAP: usize = 64;

    const fn cap(class: usize) -> usize {
        let n = CLASS_BUDGET >> (class + MIN_CLASS);
        if n < MIN_CAP {
            MIN_CAP
        } else if n > MAX_CAP {
            MAX_CAP
        } else {
            n
        }
    }

    struct Cache {
        head: [usize; CLASSES],
        count: [usize; CLASSES],
    }

    impl Cache {
        const fn new() -> Self {
            Cache {
                head: [0; CLASSES],
                count: [0; CLASSES],
            }
        }

        fn pop(&mut self, class: usize) -> Option<NonNull<u8>> {
            let block = NonNull::new(self.head[class] as *mut u8)?;
            // SAFETY: a block is in the list only between its free and its
            // next alloc, its first word is the link `push` wrote, and the
            // class is at least 8 bytes aligned to its size.
            self.head[class] = unsafe { block.as_ptr().cast::<usize>().read() };
            self.count[class] -= 1;
            Some(block)
        }

        fn push(&mut self, class: usize, block: NonNull<u8>) -> bool {
            if self.count[class] >= cap(class) {
                return false;
            }
            // SAFETY: the caller frees the block, so its bytes are ours.
            unsafe { block.as_ptr().cast::<usize>().write(self.head[class]) };
            self.head[class] = block.as_ptr() as usize;
            self.count[class] += 1;
            true
        }
    }

    static CACHES: [spin::Mutex<Cache>; lock::MAX_CORE_NUM] =
        [const { spin::Mutex::new(Cache::new()) }; lock::MAX_CORE_NUM];

    fn class_of(layout: Layout) -> Option<usize> {
        let size = layout.size().max(layout.align()).max(1 << MIN_CLASS);
        if size > 1 << MAX_CLASS {
            return None;
        }
        Some(size.next_power_of_two().trailing_zeros() as usize - MIN_CLASS)
    }

    /// The layout a small block is allocated and freed with: its class's
    /// size, aligned to that size. `None` for a block too big to cache.
    pub fn class_layout(layout: Layout) -> Option<Layout> {
        let size = 1usize << (class_of(layout)? + MIN_CLASS);
        Layout::from_size_align(size, size).ok()
    }

    fn mine() -> Option<spin::MutexGuard<'static, Cache>> {
        CACHES.get(kernel_hal::cpu::cpu_id() as usize)?.try_lock()
    }

    /// A cached block for `layout` (already a class layout) and its size.
    pub fn try_alloc(layout: Layout) -> Option<(NonNull<u8>, usize)> {
        let class = class_of(layout)?;
        let block = mine()?.pop(class)?;
        Some((block, layout.size()))
    }

    /// Keep a freed block of `layout` (already a class layout); `false`
    /// means the caller must give it to the buddy.
    pub fn try_free(block: NonNull<u8>, layout: Layout) -> bool {
        match (class_of(layout), mine()) {
            (Some(class), Some(mut cache)) => cache.push(class, block),
            _ => false,
        }
    }

    /// Hand every cached block to `give` with its layout, and answer how
    /// many there were. A cache busy right now is skipped.
    pub fn drain(mut give: impl FnMut(NonNull<u8>, Layout)) -> usize {
        let mut given = 0;
        for cache in CACHES.iter() {
            let Some(mut cache) = cache.try_lock() else {
                continue;
            };
            for class in 0..CLASSES {
                let size = 1usize << (class + MIN_CLASS);
                let layout = Layout::from_size_align(size, size).unwrap();
                while let Some(block) = cache.pop(class) {
                    give(block, layout);
                    given += 1;
                }
            }
        }
        given
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_layout_goes_to_the_class_of_its_size_and_alignment() {
            let class = |s, a| class_layout(Layout::from_size_align(s, a).unwrap());
            assert_eq!(class(1, 1), Layout::from_size_align(8, 8).ok());
            assert_eq!(class(24, 8), Layout::from_size_align(32, 32).ok());
            assert_eq!(class(8, 64), Layout::from_size_align(64, 64).ok());
            assert_eq!(class(4096, 8), Layout::from_size_align(4096, 4096).ok());
            assert_eq!(class(4097, 8), None);
        }

        #[test]
        fn a_cache_hands_back_what_it_kept_last_first_up_to_its_cap() {
            let mut cache = Cache::new();
            let class = 9; // 4 KiB: the smallest cap
            let mut blocks = alloc::vec::Vec::new();
            for _ in 0..cap(class) + 1 {
                let b = alloc::boxed::Box::leak(alloc::boxed::Box::new([0u64; 512]));
                blocks.push(NonNull::new(b.as_mut_ptr().cast::<u8>()).unwrap());
            }
            for b in &blocks[..cap(class)] {
                assert!(cache.push(class, *b));
            }
            assert!(!cache.push(class, blocks[cap(class)]), "past the cap");
            for b in blocks[..cap(class)].iter().rev() {
                assert_eq!(cache.pop(class), Some(*b));
            }
            assert_eq!(cache.pop(class), None);
        }
    }
}

/// 初始化分配器，并将一个小的内存块注册到分配器中，用于启动需要的动态内存。
pub fn init() {
    // Under libos the buddy keeps no boot pool: the heap is the host's, and
    // the window `insert_regions` hands over later is the only memory it will
    // ever own, so its base is the window's. See `HEAP`.
    #[cfg(feature = "libos")]
    unsafe {
        let base = phys_to_virt_offset() as *mut u8;
        HEAP.0.lock().init(
            core::mem::size_of::<usize>().trailing_zeros() as _,
            NonNull::new_unchecked(base),
        );
    }
    #[cfg(not(feature = "libos"))]
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
    // Before the lock: nothing that could allocate runs under the heap's own
    // lock, and a platform's offset is allowed to be lazily initialised.
    let offset = phys_to_virt_offset();
    let mut heap = HEAP.0.lock();
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
    #[cfg(feature = "mem-debug")]
    quarantine::enable();
    #[cfg(feature = "mem-debug")]
    heapcheck::verify_layout();
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
    let (ptr, size) = {
        let mut heap = HEAP.0.lock();
        #[cfg(feature = "mem-debug")]
        let _inside = lockcheck::Inside::enter();
        heap.allocate::<u8>(align_order, bytes).ok()?
    };
    assert_eq!(size, bytes.get());
    let base = ptr.as_ptr() as usize;
    #[cfg(feature = "mem-debug")]
    quarantine::assert_not_parked(base, size);
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
    let vaddr = target + phys_to_virt_offset();
    // With `mem-debug` the frame is poisoned and parked first, and what the
    // buddy gets is the block the parking evicted -- after a check that nobody
    // wrote to it while it was out of circulation. See `quarantine`.
    // What comes back may be a heap block of any size (a one-byte `Box` among
    // them), so it goes through `deallocate_layout`, which rounds a size up
    // to the buddy's minimum order; plain `deallocate` does not, and a size
    // below the minimum underflows its order arithmetic.
    #[cfg(feature = "mem-debug")]
    quarantine::park_and_drain(
        quarantine::Parked {
            addr: vaddr,
            size: 1 << PAGE_BITS,
            align: 1 << PAGE_BITS,
        },
        give_back_to_buddy,
    );
    #[cfg(not(feature = "mem-debug"))]
    HEAP.0.lock().deallocate(
        unsafe { NonNull::new_unchecked(vaddr as *mut u8) },
        1 << PAGE_BITS,
    );
}

/// Hand a block the quarantine let go of back to the buddy.
#[cfg(feature = "mem-debug")]
fn give_back_to_buddy(block: quarantine::Parked) {
    let mut heap = HEAP.0.lock();
    let _inside = lockcheck::Inside::enter();
    // SAFETY: the block was handed to `park` by its owner with the size and
    // alignment it was allocated with, and the ring has held it since.
    unsafe {
        heap.deallocate_layout(
            NonNull::new_unchecked(block.addr as *mut u8),
            Layout::from_size_align_unchecked(block.size, block.align),
        );
    }
}

/// [diag] Is the heap lock actually exclusive? The riscv64 corruption shows
/// up with six harts and never with one, and the values on the free lists
/// are those of LIVE objects (`Arc` counts, sizes, constants): what a block
/// handed out twice would show. A lock that let two harts into the buddy at
/// once would do exactly that. Every holder of the heap lock announces
/// itself here; a second arrival while the first is still inside is reported
/// with both cpu ids, and stops the machine.
#[cfg(feature = "mem-debug")]
mod lockcheck {
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// `cpu + 1` of the hart inside the heap lock; 0 when nobody is.
    static INSIDE: AtomicUsize = AtomicUsize::new(0);

    pub struct Inside;

    impl Inside {
        pub fn enter() -> Self {
            let me = kernel_hal::cpu::cpu_id() as usize + 1;
            let previous = INSIDE.swap(me, Ordering::AcqRel);
            if previous != 0 {
                kernel_hal::console::serial_write_fmt_spin(format_args!(
                    "\n[heap-lock] TWO CPUS INSIDE THE HEAP LOCK: cpu {} was in, cpu {} just entered\n",
                    previous - 1,
                    me - 1,
                ));
                panic!(
                    "heap lock not exclusive: cpu {} inside, cpu {} entered",
                    previous - 1,
                    me - 1
                );
            }
            super::heapcheck::validate("enter");
            Inside
        }
    }

    impl Drop for Inside {
        fn drop(&mut self) {
            super::heapcheck::validate("exit");
            INSIDE.store(0, Ordering::Release);
        }
    }
}

/// [diag] Check the buddy's free lists on every heap operation, at the lock.
/// The riscv64 fault was always `ld s2, 0(s2)` in `Node::insert`, after the
/// register that held the node with the bad `next` had been overwritten, so
/// the fault report never named the block that was written. This walks the
/// lists from the heads inside `HEAP` (the small layers deep, the rest
/// shallow, everything every 1024th operation) and names the node, its layer
/// and the word at the first heap operation after the write -- and says
/// whether the heads themselves (a static in .bss, between `TOPOLOGY` and the
/// executor's per-CPU arrays) are what got written.
#[cfg(feature = "mem-debug")]
mod heapcheck {
    use core::sync::atomic::{AtomicBool, Ordering};

    const LAYERS: usize = 27;
    /// `&HEAP` + this = `buddies[0]`: `{ free_list: Node, order: Order }`,
    /// 16 bytes each. Read off the fault registers (a5 = HEAP + 0x38 was
    /// the layer being walked) and checked by `verify_layout` at boot.
    const BUDDIES_OFF: usize = 0x38;
    const LO: usize = kernel_hal::kaddr::KERNEL_LO as usize;
    const HI: usize = kernel_hal::kaddr::KERNEL_HI as usize;
    /// Cap on a full walk, so a cycle cannot hang the check. Walking every
    /// list on every operation did not boot in 300 s; at depth 8 the fault
    /// still came before the check (the 8-byte list is long).
    const DEPTH: usize = 1 << 16;
    const SMALL_LAYERS: usize = 5;
    const SMALL_DEPTH: usize = 512;
    const SHALLOW_DEPTH: usize = 16;
    const FULL_EVERY: usize = 1024;

    static LAYOUT_OK: AtomicBool = AtomicBool::new(false);

    fn base() -> usize {
        &super::HEAP as *const _ as usize
    }

    fn word(addr: usize) -> usize {
        // SAFETY: kernel-mapped static or a free-list node already range-checked.
        unsafe { core::ptr::read_volatile(addr as *const usize) }
    }

    /// Print the layer table once and accept it if the orders are small and
    /// strictly ascending, which is what the buddy builds.
    pub fn verify_layout() {
        let mut ok = true;
        let mut last = 0;
        for i in 0..LAYERS {
            let e = base() + BUDDIES_OFF + 16 * i;
            let (head, order) = (word(e), word(e + 8));
            kernel_hal::console::serial_write_fmt_spin(format_args!(
                "[heapcheck] layer {:2} @ {:#x}: order={} head={:#x}\n",
                i, e, order, head
            ));
            if order >= 64 || (i > 0 && order <= last) {
                ok = false;
            }
            last = order;
        }
        LAYOUT_OK.store(ok, Ordering::Release);
        kernel_hal::console::serial_write_fmt_spin(format_args!(
            "[heapcheck] layout {} (HEAP at {:#x})\n",
            if ok {
                "accepted"
            } else {
                "REJECTED, checks off"
            },
            base()
        ));
    }

    static CALLS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

    /// The stack pointer each CPU last entered the heap with: a cheap way to
    /// know, when a parked block turns out to hold stack frames, whose stack
    /// is near it.
    pub static LAST_SP: [core::sync::atomic::AtomicUsize; 64] =
        [const { core::sync::atomic::AtomicUsize::new(0) }; 64];

    pub fn current_sp() -> usize {
        let sp: usize;
        // SAFETY: reads the stack pointer only.
        unsafe {
            cfg_if::cfg_if! {
                if #[cfg(target_arch = "riscv64")] {
                    core::arch::asm!("mv {}, sp", out(reg) sp, options(nomem, nostack, preserves_flags));
                } else if #[cfg(target_arch = "aarch64")] {
                    core::arch::asm!("mov {}, sp", out(reg) sp, options(nomem, nostack, preserves_flags));
                } else {
                    core::arch::asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack, preserves_flags));
                }
            }
        }
        sp
    }

    /// Print every CPU's last-seen sp and how far it is from `addr`.
    pub fn report_stacks(addr: usize) {
        let me = kernel_hal::cpu::cpu_id() as usize;
        for (cpu, slot) in LAST_SP.iter().enumerate() {
            let sp = if cpu == me {
                current_sp()
            } else {
                slot.load(Ordering::Relaxed)
            };
            if sp == 0 {
                continue;
            }
            let dist = sp as isize - addr as isize;
            kernel_hal::console::serial_write_fmt_spin(format_args!(
                "[heapcheck]   cpu {} sp={:#x} ({}{:#x} from the block){}\n",
                cpu,
                sp,
                if dist < 0 { "-" } else { "+" },
                dist.unsigned_abs(),
                if cpu == me { " <- this cpu, now" } else { "" },
            ));
        }
        #[cfg(not(feature = "libos"))]
        kernel_hal::console::serial_write_fmt_spin(format_args!(
            "[heapcheck]   registered live stack over the block: {:#x?}; untracked live stacks: {}\n",
            executor::overlapping_live_stack(addr, 4096),
            executor::untracked_live_stacks(),
        ));
    }

    pub fn validate(when: &str) {
        if !LAYOUT_OK.load(Ordering::Acquire) {
            return;
        }
        // Only the walk before the operation: the one after it doubles the
        // cost and a write in between would be caught at the next entry.
        if when != "enter" {
            return;
        }
        let calls = CALLS.fetch_add(1, Ordering::Relaxed);
        if let Some(slot) = LAST_SP.get(kernel_hal::cpu::cpu_id() as usize) {
            slot.store(current_sp(), Ordering::Relaxed);
        }
        if calls == 200_000 {
            // Boot has long passed and the console is up: say the check is on.
            let mut lens = [0usize; LAYERS];
            for (i, len) in lens.iter_mut().enumerate() {
                let mut at = base() + BUDDIES_OFF + 16 * i;
                while word(at) != 0 && *len < DEPTH {
                    at = word(at);
                    *len += 1;
                }
            }
            kernel_hal::console::serial_write_fmt_spin(format_args!(
                "[heapcheck] on; list lengths by layer: {:?}\n",
                lens
            ));
        }
        let full = calls.is_multiple_of(FULL_EVERY);
        for i in 0..LAYERS {
            let e = base() + BUDDIES_OFF + 16 * i;
            let order = word(e + 8);
            let mask = (1usize << order) - 1;
            let mut at = e;
            let depth_cap = if full {
                DEPTH
            } else if i < SMALL_LAYERS {
                SMALL_DEPTH
            } else {
                SHALLOW_DEPTH
            };
            for depth in 0..depth_cap {
                let next = word(at);
                if next == 0 {
                    break;
                }
                if !(LO..HI).contains(&next) || next & mask != 0 {
                    report(i, order, depth, e, at, next, when);
                }
                at = next;
            }
        }
    }

    fn report(
        layer: usize,
        order: usize,
        depth: usize,
        head: usize,
        at: usize,
        next: usize,
        when: &str,
    ) {
        let where_ = if at == head {
            "THE HEAD IN .bss"
        } else {
            "a free block"
        };
        kernel_hal::console::serial_write_fmt_spin(format_args!(
            "\n[heapcheck] BAD FREE-LIST LINK ({}): layer={} order={} depth={} node={:#x} ({}) next={:#x} cpu={} parked_frame={}\n",
            when,
            layer,
            order,
            depth,
            at,
            where_,
            next,
            kernel_hal::cpu::cpu_id(),
            super::quarantine::page_parked(at & !4095),
        ));
        if at != head {
            let lo = at.saturating_sub(32);
            for row in 0..6 {
                let a = lo + row * 16;
                kernel_hal::console::serial_write_fmt_spin(format_args!(
                    "[heapcheck]   {:#x}: {:#018x} {:#018x}{}\n",
                    a,
                    word(a),
                    word(a + 8),
                    if a == at { "  <- node" } else { "" }
                ));
            }
        }
        panic!("heap free list corrupt at {:#x} (next={:#x})", at, next);
    }
}

/// [diag] A canary after every heap block, for the writer the quarantine
/// cannot see: one that runs off the end of its own live allocation into the
/// next block -- which, when that block is free, is where the buddy keeps its
/// free-list node.
///
/// `alloc` asks the buddy for `REDZONE` bytes more than the caller wanted and
/// fills them with `CANARY`; `dealloc` reads them back before the block is
/// poisoned, and a byte that changed names the block that overflowed, with
/// the size and alignment that identify its type. The x86_64 allocator has
/// had this for a while (`memory_x86_64::REDZONE`); this is its twin for the
/// buddy that riscv64 and aarch64 use. Not in the default build.
#[cfg(feature = "mem-debug")]
mod redzone {
    use core::alloc::Layout;

    const REDZONE: usize = 16;
    const CANARY: u8 = 0xAB;

    /// The layout the buddy is asked for: the caller's, plus the canary.
    /// `None` when the padded size is not a layout any more (a request
    /// within 16 bytes of `isize::MAX`): the caller takes the
    /// allocation-failure path, as the buddy would have.
    pub fn grow(layout: Layout) -> Option<Layout> {
        let size = layout.size().checked_add(REDZONE)?;
        Layout::from_size_align(size, layout.align()).ok()
    }

    /// Write the canary after `size` payload bytes at `ptr`.
    pub fn arm(ptr: *mut u8, size: usize) {
        // SAFETY: the block was allocated with `grow(layout)`, so these bytes
        // are inside it.
        unsafe { core::ptr::write_bytes(ptr.add(size), CANARY, REDZONE) };
    }

    /// Check the canary of a block about to be freed, and report a clobber.
    pub fn check(ptr: *mut u8, layout: Layout) {
        for i in 0..REDZONE {
            // SAFETY: as in `arm`.
            let byte = unsafe { core::ptr::read_volatile(ptr.add(layout.size() + i)) };
            if byte != CANARY {
                kernel_hal::console::serial_write_fmt_spin(format_args!(
                    "\n[redzone] HEAP OVERFLOW: block={:#x} size={} align={} clobbered at +{} (byte={:#x}, canary {:#x})\n",
                    ptr as usize,
                    layout.size(),
                    layout.align(),
                    layout.size() + i,
                    byte,
                    CANARY,
                ));
                return;
            }
        }
    }
}

/// [diag] A holding pen for freed memory, so a write to a block after its
/// free has a chance of being seen.
///
/// On this arena a freed block goes straight back to the buddy, whose first
/// act is to write a free-list node into its first word -- and whose next act,
/// on a later `insert`, is to follow that word. A block written after its free
/// (a kernel object used after its `Box` died, a user frame freed while a
/// stale PTE still pointed at it) is therefore unreadable as such: the stray
/// store lands on the free list, and what the kernel sees is a `#PF` in
/// `Node::insert` with the stray value in the register, on whichever
/// allocation happened next. That is this job's `rip=0xffffffc0802003ca`,
/// with `0x3b9aca00` (a billion: a nanosecond count), `0x1` and ASCII in it.
///
/// So, with `mem-debug`, a freed block is filled with `POISON` and parked in
/// a ring of `SLOTS` entries instead; the block the parking evicts is scanned
/// before it goes to the buddy, and a word that is not `POISON` any more is
/// reported with the block, its size and the offset -- the block itself stays
/// usable. Blocks over `MAX_PARKED` bytes go to the buddy directly, and so
/// does any block while the ring already holds `BUDGET` bytes.
///
/// Costs a memset per free and a scan per eviction. Not in the default build.
#[cfg(feature = "mem-debug")]
mod quarantine {
    use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
    use kernel_hal::sync::Mutex;

    /// Frees the ring remembers. The riscv64 writer turned out to be a trap
    /// handler running on a `UserContext` as its stack; it was caught with a
    /// ring of 262144 slots in a 4 GiB guest, where the first hit was 11767
    /// parks old. `SLOTS` and `BUDGET` are a `const` away from that again.
    const SLOTS: usize = 4096;
    /// Slots scanned on every park (blocks up to a page: a 64 KiB block per
    /// slot made every libc-test case take twenty seconds), so a write is
    /// reported ~SLOTS/SWEEP parks after it happens instead of at eviction.
    const SWEEP: usize = 8;
    const SWEEP_MAX_SIZE: usize = 4096;
    /// Recent slots the double-free / handed-out scans cover: all of them.
    const WINDOW: usize = SLOTS;
    /// Blocks above this go to the buddy directly. Coroutine stacks (2 MiB)
    /// are excluded on purpose: they would eat the byte budget in a few
    /// frees and push the small blocks out before anybody wrote to them.
    const MAX_PARKED: usize = 64 * 1024;
    /// Bytes the ring may hold back at once; past this the oldest blocks go
    /// back to the buddy. QEMU riscv64 runs with 1 GiB.
    const BUDGET: usize = 64 * 1024 * 1024;
    const POISON: u8 = 0xA5;
    const POISON_WORD: usize = usize::from_ne_bytes([POISON; core::mem::size_of::<usize>()]);

    /// A parked block: kernel-virtual address, size, alignment. `addr == 0`
    /// is an empty slot.
    #[derive(Clone, Copy)]
    pub struct Parked {
        pub addr: usize,
        pub size: usize,
        pub align: usize,
    }

    struct Ring {
        slots: [Parked; SLOTS],
        next: usize,
        sweep: usize,
        parks: usize,
    }

    const EMPTY: Parked = Parked {
        addr: 0,
        size: 0,
        align: 0,
    };

    static RING: Mutex<Ring> = Mutex::new(Ring {
        slots: [EMPTY; SLOTS],
        next: 0,
        sweep: 0,
        parks: 0,
    });

    /// One bit per 4 KiB page of the kernel window, set while a page-sized,
    /// page-aligned block (a frame) is parked. Exact whatever `WINDOW` is.
    const PAGE_BASE: usize = kernel_hal::kaddr::KERNEL_LO as usize;
    const PAGE_WORDS: usize = (8usize << 30) / 4096 / 64;
    static PAGES: [AtomicUsize; PAGE_WORDS] = [const { AtomicUsize::new(0) }; PAGE_WORDS];

    fn page_bit(addr: usize) -> Option<(usize, usize)> {
        let page = addr.checked_sub(PAGE_BASE)? / 4096;
        if page / 64 >= PAGE_WORDS {
            return None;
        }
        Some((page / 64, 1usize << (page % 64)))
    }

    fn is_frame(block: &Parked) -> bool {
        block.size == 4096 && block.align == 4096
    }

    pub(super) fn page_parked(addr: usize) -> bool {
        page_bit(addr).is_some_and(|(w, b)| PAGES[w].load(Ordering::Relaxed) & b != 0)
    }

    fn mark_page(addr: usize, on: bool) {
        if let Some((w, b)) = page_bit(addr) {
            if on {
                PAGES[w].fetch_or(b, Ordering::Relaxed);
            } else {
                PAGES[w].fetch_and(!b, Ordering::Relaxed);
            }
        }
    }

    /// Blocks found written after their free.
    pub static WRITTEN_AFTER_FREE: AtomicU32 = AtomicU32::new(0);
    /// Bytes currently parked.
    static HELD: AtomicUsize = AtomicUsize::new(0);
    /// Off until `insert_regions` has run: the boot pool is 2 MiB, and
    /// parking frees out of it starved early boot before the console was up
    /// (the kernel went silent after "hartN is booting").
    static ENABLED: AtomicBool = AtomicBool::new(false);

    /// Start parking. Called once the buddy owns the machine's memory.
    pub fn enable() {
        ENABLED.store(true, Ordering::Release);
    }

    /// Poison the block and park it. Returns what the caller should hand to
    /// the buddy now: the block itself when it is too big to park, the block
    /// this parking evicted (checked) otherwise, or nothing while the ring is
    /// still filling.
    pub fn park(block: Parked) -> Option<Parked> {
        if !ENABLED.load(Ordering::Acquire) || block.size > MAX_PARKED {
            return Some(block);
        }
        // The ring caught the writer: `WRITTEN AFTER FREE: block=... size=4096
        // offset=0x8a0 word=0x3b9aca00`, eight times for the SAME frame, and
        // the next fault had its `sp` inside that frame -- a page that was
        // freed as a frame while a hart was running on it as a STACK. Name
        // the freer, here, where its backtrace still exists. Only for blocks
        // too small to be a stack allocation themselves, so a stack's own
        // retirement is never mistaken for this.
        #[cfg(not(feature = "libos"))]
        if let Some(stack_base) = executor::overlapping_live_stack(block.addr, block.size) {
            kernel_hal::console::serial_write_fmt_spin(format_args!(
                "\n[quarantine] FREED A LIVE KERNEL STACK: block={:#x} size={} align={} is inside coroutine stack base={:#x}\n",
                block.addr, block.size, block.align, stack_base,
            ));
            panic!(
                "freed {:#x} ({} bytes), which is inside live coroutine stack {:#x}",
                block.addr, block.size, stack_base
            );
        }
        // And the freer's own stack pointer: the writes the ring reported
        // came from several harts, so the page may be a stack every hart
        // passes through rather than a coroutine's.
        #[cfg(target_arch = "riscv64")]
        {
            let sp: usize;
            // SAFETY: reads a register.
            unsafe { core::arch::asm!("mv {}, sp", out(reg) sp) };
            if sp >= block.addr && sp < block.addr + block.size {
                kernel_hal::console::serial_write_fmt_spin(format_args!(
                    "\n[quarantine] FREED THE STACK IT RUNS ON: block={:#x} size={} align={} sp={:#x}\n",
                    block.addr, block.size, block.align, sp,
                ));
                panic!(
                    "freed {:#x} ({} bytes) with sp={:#x} inside it",
                    block.addr, block.size, sp
                );
            }
        }
        HELD.fetch_add(block.size, Ordering::Relaxed);
        let evicted = {
            let mut ring = RING.lock();
            // A block that overlaps one already parked is being freed twice,
            // or with a size that is not its own. Poisoning it would overwrite
            // the free-list node the buddy keeps in the FIRST free, and the
            // next walk would then follow `0xa5a5a5a5a5a5a5a5` -- which is the
            // fault this ring turned the riscv64 heap corruption into. Say so,
            // and panic for the backtrace: the second freer is the bug.
            if is_frame(&block) && page_parked(block.addr) {
                kernel_hal::console::serial_write_fmt_spin(format_args!(
                    "\n[quarantine] DOUBLE FREE (page bitmap): frame={:#x} is still parked cpu={}\n",
                    block.addr,
                    kernel_hal::cpu::cpu_id(),
                ));
                drop(ring);
                panic!("double free of frame {:#x}, still parked", block.addr);
            }
            let end = block.addr + block.size;
            let twin =
                recent(&ring).find(|p| p.addr != 0 && p.addr < end && block.addr < p.addr + p.size);
            if let Some(twin) = twin {
                kernel_hal::console::serial_write_fmt_spin(format_args!(
                    "\n[quarantine] DOUBLE FREE: block={:#x} size={} align={} overlaps parked block={:#x} size={} align={}\n",
                    block.addr, block.size, block.align, twin.addr, twin.size, twin.align,
                ));
                drop(ring);
                panic!(
                    "double free of {:#x} ({} bytes), already parked as {:#x}",
                    block.addr, block.size, twin.addr
                );
            }
            // SAFETY: the caller owns the block it is freeing and nothing else
            // (legitimately) references it any more; that is what is being
            // checked.
            unsafe { core::ptr::write_bytes(block.addr as *mut u8, POISON, block.size) };
            let slot = ring.next;
            ring.next = (slot + 1) % SLOTS;
            let evicted = core::mem::replace(&mut ring.slots[slot], block);
            if is_frame(&block) {
                mark_page(block.addr, true);
            }
            if is_frame(&evicted) {
                mark_page(evicted.addr, false);
            }
            ring.parks += 1;
            sweep(&mut ring);
            evicted
        };
        if evicted.addr == 0 {
            return None;
        }
        HELD.fetch_sub(evicted.size, Ordering::Relaxed);
        check(evicted);
        Some(evicted)
    }

    /// The oldest parked block, checked, while the ring holds more than
    /// `BUDGET` bytes. The first version of this ring BYPASSED parking when
    /// the budget was full, and since only a parking evicts, a full ring was
    /// a ring switched off for good: 1024 parked frames reached the 4 MiB
    /// budget early in boot and every free after that went straight to the
    /// buddy unchecked. Evicting instead keeps the ring turning.
    ///
    /// The oldest block is the first occupied slot from `next` onwards: the
    /// slot `next` itself once the ring has wrapped, and slot 0 while it is
    /// still filling, when `next` and everything after it are empty. The
    /// first version took `next` unconditionally, so during the fill it hit
    /// an empty slot, returned `None` and stopped draining: 1025 parked
    /// 64 KiB frames passed the budget long before 4096 parkings, and the
    /// ring kept growing to its full 256 MiB. The slot is emptied in place,
    /// without moving `next`: the parking that lands on that hole later
    /// evicts `EMPTY`, which `park` already treats as nothing.
    pub fn over_budget() -> Option<Parked> {
        if HELD.load(Ordering::Relaxed) <= BUDGET {
            return None;
        }
        let oldest = {
            let mut ring = RING.lock();
            let slot = (0..SLOTS)
                .map(|i| (ring.next + i) % SLOTS)
                .find(|&i| ring.slots[i].addr != 0)?;
            let oldest = core::mem::replace(&mut ring.slots[slot], EMPTY);
            if is_frame(&oldest) {
                mark_page(oldest.addr, false);
            }
            oldest
        };
        if oldest.addr == 0 {
            return None;
        }
        HELD.fetch_sub(oldest.size, Ordering::Relaxed);
        check(oldest);
        Some(oldest)
    }

    /// The buddy just handed out `[addr, addr + size)`: none of it may be a
    /// block this ring still holds. If it is, the buddy's free lists name
    /// memory that was never given back to them -- a block on a list twice,
    /// or a list rewritten under it.
    pub fn assert_not_parked(addr: usize, size: usize) {
        if !ENABLED.load(Ordering::Acquire) {
            return;
        }
        let end = addr + size;
        let mut page = addr & !4095;
        while page < end {
            if page_parked(page) {
                kernel_hal::console::serial_write_fmt_spin(format_args!(
                    "\n[quarantine] HANDED OUT A PARKED FRAME (page bitmap): allocated {:#x}+{:#x} covers parked frame {:#x} cpu={}\n",
                    addr,
                    size,
                    page,
                    kernel_hal::cpu::cpu_id(),
                ));
                panic!(
                    "buddy handed out {:#x} ({} bytes) covering parked frame {:#x}",
                    addr, size, page
                );
            }
            page += 4096;
        }
        let ring = RING.lock();
        let hit = recent(&ring).find(|p| p.addr != 0 && p.addr < end && addr < p.addr + p.size);
        drop(ring);
        if let Some(hit) = hit {
            kernel_hal::console::serial_write_fmt_spin(format_args!(
                "\n[quarantine] HANDED OUT A PARKED BLOCK: allocated {:#x}+{} overlaps parked {:#x} size={} align={}\n",
                addr, size, hit.addr, hit.size, hit.align,
            ));
            panic!(
                "buddy handed out {:#x} ({} bytes) while the quarantine holds {:#x}",
                addr, size, hit.addr
            );
        }
    }

    /// Park `block`, then hand `give` every block the ring lets go of: the
    /// one this parking evicted, and the oldest ones while over budget.
    pub fn park_and_drain(block: Parked, mut give: impl FnMut(Parked)) {
        if let Some(evicted) = park(block) {
            give(evicted);
        }
        while let Some(old) = over_budget() {
            give(old);
        }
    }

    /// The last `WINDOW` parked blocks, newest first.
    fn recent(ring: &Ring) -> impl Iterator<Item = Parked> + '_ {
        (1..=WINDOW).map(move |back| ring.slots[(ring.next + SLOTS - back) % SLOTS])
    }

    /// Blocks found written while still parked (by the rolling sweep).
    pub static WRITTEN_WHILE_PARKED: AtomicU32 = AtomicU32::new(0);

    /// Scan the next `SWEEP` slots for writes. A hit is reported with the
    /// block, the first bad word and its offset, how many parks ago the
    /// block was freed, and the CPU that noticed; the block is re-poisoned so
    /// a second write to it is reported again.
    fn sweep(ring: &mut Ring) {
        let word_size = core::mem::size_of::<usize>();
        for _ in 0..SWEEP {
            let slot = ring.sweep;
            ring.sweep = (slot + 1) % SLOTS;
            let block = ring.slots[slot];
            if block.addr == 0 || block.size > SWEEP_MAX_SIZE {
                continue;
            }
            let words = block.size / word_size;
            let mut bad = None;
            for i in 0..words {
                // SAFETY: a parked block is kernel-mapped and owned by the ring.
                let word = unsafe { core::ptr::read_volatile((block.addr as *const usize).add(i)) };
                if word != POISON_WORD {
                    bad = Some((i, word));
                    break;
                }
            }
            let Some((i, word)) = bad else { continue };
            let age = (ring.next + SLOTS - slot) % SLOTS;
            let n = WRITTEN_WHILE_PARKED.fetch_add(1, Ordering::Relaxed) + 1;
            // Up to eight more bad words, to see the shape of what was written.
            let mut tail = [0usize; 8];
            let mut t = 0;
            let mut j = i + 1;
            while j < words && t < 8 {
                let w = unsafe { core::ptr::read_volatile((block.addr as *const usize).add(j)) };
                if w != POISON_WORD {
                    tail[t] = w;
                    t += 1;
                }
                j += 1;
            }
            kernel_hal::console::serial_write_fmt_spin(format_args!(
                "\n[quarantine] WRITTEN WHILE PARKED: block={:#x} size={} align={} offset={:#x} word={:#x} next={:#x?} age={} parks cpu={} #{}\n",
                block.addr, block.size, block.align, i * word_size, word, &tail[..t], age,
                kernel_hal::cpu::cpu_id(), n,
            ));
            // The whole written stretch, 4 words a row, to read the frames.
            let mut j = i;
            let mut rows = 0;
            while j < words && rows < 16 {
                let a = block.addr + j * word_size;
                let w = |k: usize| unsafe {
                    core::ptr::read_volatile((block.addr as *const usize).add(j + k))
                };
                if j + 3 < words
                    && (w(0) != POISON_WORD
                        || w(1) != POISON_WORD
                        || w(2) != POISON_WORD
                        || w(3) != POISON_WORD)
                {
                    kernel_hal::console::serial_write_fmt_spin(format_args!(
                        "[quarantine]   {:#x}: {:#018x} {:#018x} {:#018x} {:#018x}\n",
                        a,
                        w(0),
                        w(1),
                        w(2),
                        w(3)
                    ));
                    rows += 1;
                }
                j += 4;
            }
            super::heapcheck::report_stacks(block.addr);
            unsafe { core::ptr::write_bytes(block.addr as *mut u8, POISON, block.size) };
        }
    }

    fn check(block: Parked) {
        let word_size = core::mem::size_of::<usize>();
        let words = block.size / word_size;
        for i in 0..words {
            // SAFETY: a parked block is kernel-mapped and owned by the ring.
            let word = unsafe { core::ptr::read_volatile((block.addr as *const usize).add(i)) };
            if word != POISON_WORD {
                WRITTEN_AFTER_FREE.fetch_add(1, Ordering::Relaxed);
                error!(
                    "[quarantine] WRITTEN AFTER FREE: block={:#x} size={} align={} offset={:#x} word={:#x} (poison {:#x}), #{}",
                    block.addr,
                    block.size,
                    block.align,
                    i * word_size,
                    word,
                    POISON_WORD,
                    WRITTEN_AFTER_FREE.load(Ordering::Relaxed),
                );
                return;
            }
        }
    }
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

/// The x86_64 heap grows out of physical RAM and gives that growth back
/// here; this one is a fixed pool with nothing grown to return. Only
/// `reclaimd` calls it, and a zircon-only kernel has no `reclaimd`.
#[allow(dead_code)]
pub fn release_idle_heap(_want: usize) -> usize {
    0
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

/// Bytes those refusals leaked (mirrors `memory_x86_64::heap_wild_bytes`).
/// This build does not check.
#[allow(dead_code)]
pub fn heap_wild_bytes() -> usize {
    0
}

/// Freed frames found written after their free (mirrors
/// `memory_x86_64::heap_written_after_free`). Counted by the `mem-debug`
/// frame quarantine, both ways it can notice: at eviction
/// (`WRITTEN_AFTER_FREE`) and by the rolling sweep (`WRITTEN_WHILE_PARKED`),
/// which re-poisons the block so the eviction will not count that write
/// again. The default build does not check.
#[allow(dead_code)]
pub fn heap_written_after_free() -> u32 {
    #[cfg(feature = "mem-debug")]
    {
        quarantine::WRITTEN_AFTER_FREE
            .load(Ordering::Relaxed)
            .saturating_add(quarantine::WRITTEN_WHILE_PARKED.load(Ordering::Relaxed))
    }
    #[cfg(not(feature = "mem-debug"))]
    {
        0
    }
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
            insert_regions(&[physical(base)..physical(base) + REGION_SIZE]);
        });
        guard
    }

    /// The physical address a host block would have: `insert_regions` maps
    /// what it is given through `phys_to_virt_offset`, which is zero on the
    /// bare-metal arms this suite stands in for and the mock-memory window's
    /// base under libos. The host's addresses sit far above that base, so
    /// the subtraction is exact and the allocator gets the block back at the
    /// address it really has.
    fn physical(host_vaddr: usize) -> PhysAddr {
        host_vaddr - phys_to_virt_offset()
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
    #[cfg(not(feature = "libos"))]
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
    #[cfg(not(feature = "libos"))]
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

    /// Under libos there is no boot pool: the heap is the host's until
    /// `insert_regions` hands over the mock-memory window, so `init()` must
    /// leave the allocator empty -- a pool here was the `pmem_zero`
    /// assertion on the aarch64 libos boot, a frame handed out from a static
    /// that is not inside the window -- and its base must be the window's,
    /// which `phys_to_virt_offset` is. Measured in an arm64 VM: an offset of
    /// zero was `str x3, [x18]` at `0x1000`, the first of the six causes of
    /// that job's red.
    #[cfg(feature = "libos")]
    #[test]
    fn under_libos_init_keeps_no_boot_pool_and_the_offset_is_the_window() {
        let _alone = alone_with_the_heap();
        assert_eq!(TOTAL_AFTER_INIT.load(Ordering::Relaxed), 0);
        assert_eq!(CAPACITY_AFTER_INIT.load(Ordering::Relaxed), 0);
        assert_eq!(phys_to_virt_offset(), kernel_hal::libos::PMEM_MAP_VADDR);
        assert_ne!(phys_to_virt_offset(), 0);
    }

    // --- what the heap says it manages -----------------------------------

    /// The bug. `init()` transferred the 2 MiB boot pool to the buddy and did
    /// not add it to `TOTAL_MEMORY`, and `insert_regions` -- the only other
    /// writer -- runs later in boot. So `heap_total()` was **0** for the whole
    /// early window, and the reader is the OOM banner in `lang.rs`: a failed
    /// allocation there printed "used N / total 0 MiB".
    #[cfg(not(feature = "libos"))]
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
        insert_regions(&[physical(base)..physical(base) + 2 * PAGE]);
        assert_eq!(heap_total(), before + 2 * PAGE);
    }

    /// A firmware table that reports a range backwards, or a zero-length one,
    /// must not reach `transfer`: its `len()` is 0, so it would add nothing and
    /// hand the buddy a pointer to memory the range does not describe.
    #[test]
    fn an_empty_or_reversed_region_is_not_handed_to_the_allocator() {
        let _alone = alone_with_the_heap();
        let before = heap_total();
        // Any address at all: an empty range's pointer must never be read.
        let somewhere = physical(core::ptr::addr_of!(TOTAL_MEMORY) as usize);
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
