//! Large kernel-heap blocks mapped from scattered frames: Linux's `kvmalloc`.
//!
//! The kernel heap is a buddy allocator, and the buddy hands out a block as a
//! power of two of physically contiguous memory aligned to its size. For a
//! large block that is the worst possible shape: a 33 MiB firmware copy takes
//! a 64 MiB block, 4 MiB of socket buffer plus one byte takes 8 MiB, and on a
//! machine that has been up for a while there may be no free run of frames of
//! that size and alignment at all (the same failure that moved coroutine
//! stacks to [`crate::vmap_stack`]). None of the large blocks needs physical
//! contiguity: the drivers that give heap memory to a device translate it page
//! by page (`drivers_virt_to_phys` walks the page table), and NVIDIA's own
//! `os_alloc_mem` hands out `vmalloc` memory on Linux above a size.
//!
//! So a block the buddy would make 1 MiB or larger comes from here instead: a
//! slot of a virtual window ([`VMALLOC_LO`]) with one frame per page, wherever
//! the frame allocator finds them, and only as many pages as were asked for.
//!
//! **Freeing never unmaps.** A free can come from anywhere -- under a spin
//! lock, with interrupts off, from inside the frame allocator's own reclaim --
//! and taking pages away needs every CPU's TLB flushed, which means waiting on
//! CPUs that may be waiting on this one. So a freed block stays mapped, as a
//! cached slot the next large allocation of a similar size takes over as it
//! is (no mapping, no flush, no zeroing beyond what the caller asks), and
//! [`purge`] gives cached slots back from `reclaimd`, a context that may wait.
//! Linux's `vfree` defers its purge to a worker for the same reason.
//!
//! The window has a top-level page-table entry of its own, made at boot
//! before any address space is cloned, so every address space walks into the
//! same tables and a block mapped later is visible to all of them.

use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};

use crate::mem::PhysFrame;
#[cfg(not(test))]
use crate::vm::PageTable;
use crate::vm::{GenericPageTable, Page, PageSize};
use crate::MMUFlags;

/// The window: PML4 slot `0x1fd`, between the stack window (`0x1fc`) and the
/// kernel image (`0x1fe`).
///
/// Declared in [`crate::kaddr`], not here, because the fault path has to know
/// it too: a coroutine stack is a ~2.6 MiB heap block, so it is served from
/// this window whenever `vmap_stack` declines, and a probe that does not know
/// the window answers "not a kernel stack" about a live stack pointer.
pub const VMALLOC_LO: usize = crate::kaddr::VMALLOC_LO as usize;
/// Bytes of window per block: the largest block served from here.
pub const SLOT: usize = crate::kaddr::VMALLOC_SLOT as usize;
/// Blocks in the window: 64 GiB of address space.
pub const SLOTS: usize = crate::kaddr::VMALLOC_SLOTS as usize;
pub const VMALLOC_HI: usize = crate::kaddr::VMALLOC_HI as usize;

const PAGE: usize = 4096;
const SLOT_PAGES: usize = SLOT / PAGE;

const FREE: u8 = 0;
/// Being mapped, or being purged: owned by the CPU that set it.
const BUSY: u8 = 1;
const LIVE: u8 = 2;
/// Freed and still mapped: reusable as it is, or purged by `reclaimd`.
const CACHED: u8 = 3;

static STATE: [AtomicU8; SLOTS] = [const { AtomicU8::new(FREE) }; SLOTS];
/// Pages mapped in each slot.
static MAPPED: [AtomicU32; SLOTS] = [const { AtomicU32::new(0) }; SLOTS];

static READY: AtomicBool = AtomicBool::new(false);
/// Serialises changes to the window's page tables. Never held across a frame
/// allocation, and never taken by [`free`].
static LOCK: spin::Mutex<()> = spin::Mutex::new(());
/// `cpu + 1` of the CPU holding [`LOCK`], `0` for none: an interrupt that
/// allocates a large block on that CPU goes to the buddy instead of waiting
/// on itself.
static LOCK_OWNER: AtomicUsize = AtomicUsize::new(0);

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static CACHED_BYTES: AtomicUsize = AtomicUsize::new(0);
static REUSED: AtomicUsize = AtomicUsize::new(0);
static FAILED: AtomicUsize = AtomicUsize::new(0);
static PURGED_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Whether `a` is inside the window, mapped or not.
#[inline]
pub fn owns(a: usize) -> bool {
    (VMALLOC_LO..VMALLOC_HI).contains(&a)
}

/// `(bytes in live blocks, bytes in cached blocks, reuses, failures, bytes
/// purged)`, since boot. Allocates nothing.
pub fn stats() -> (usize, usize, usize, usize, usize) {
    (
        LIVE_BYTES.load(Ordering::Relaxed),
        CACHED_BYTES.load(Ordering::Relaxed),
        REUSED.load(Ordering::Relaxed),
        FAILED.load(Ordering::Relaxed),
        PURGED_BYTES.load(Ordering::Relaxed),
    )
}

fn me() -> usize {
    crate::cpu::cpu_id() as usize + 1
}

struct Locked(#[allow(dead_code)] spin::MutexGuard<'static, ()>);

impl Drop for Locked {
    fn drop(&mut self) {
        LOCK_OWNER.store(0, Ordering::Release);
    }
}

fn lock() -> Locked {
    loop {
        if let Some(g) = LOCK.try_lock() {
            LOCK_OWNER.store(me(), Ordering::Release);
            return Locked(g);
        }
        crate::common::ipi::tlb_shootdown_pump();
        core::hint::spin_loop();
    }
}

/// See `vmap_stack::tables`: every address space shares the window's tables.
#[cfg(not(test))]
fn tables() -> ManuallyDrop<PageTable> {
    ManuallyDrop::new(PageTable::from_current())
}

/// Whether the loaded root is the kernel's own (see [`init`]).
#[cfg(not(test))]
fn on_kernel_table() -> bool {
    crate::vm::current_vmtoken() == crate::vm::kernel_vmtoken()
}

#[cfg(test)]
use self::tests::{on_kernel_table, tables};

fn flush_everywhere() {
    crate::vm::flush_tlb(None);
    crate::common::ipi::remote_flush_tlb_aspace(None, None);
}

/// Make the window. Called once at boot with `vmap_stack::init`, on the
/// kernel's own page table, before any address space is cloned from it.
pub fn init() {
    if !on_kernel_table() {
        warn!("vmalloc: not on the kernel's own page table, large blocks stay in the heap");
        return;
    }
    let mut pt = tables();
    if pt.query(VMALLOC_LO).is_ok() {
        warn!(
            "vmalloc: {:#x} is already mapped, large blocks stay in the heap",
            VMALLOC_LO
        );
        return;
    }
    let Some(frame) = PhysFrame::new() else {
        warn!("vmalloc: no frame to build the window, large blocks stay in the heap");
        return;
    };
    // One page mapped and unmapped per gigabyte makes the top-level entry and
    // every second-level table now, while nothing has been cloned yet.
    const GIB: usize = 1 << 30;
    for off in (0..SLOT * SLOTS).step_by(GIB) {
        let va = VMALLOC_LO + off;
        let page = Page::new_aligned(va, PageSize::Size4K);
        if pt
            .map(page, frame.paddr(), MMUFlags::READ | MMUFlags::WRITE)
            .is_err()
            || pt.unmap_no_shootdown(va).is_err()
        {
            warn!("vmalloc: could not build the window, large blocks stay in the heap");
            flush_everywhere();
            return;
        }
    }
    flush_everywhere();
    drop(frame);
    READY.store(true, Ordering::Release);
    info!(
        "vmalloc: {} large-block slots of {} MiB at {:#x}",
        SLOTS,
        SLOT >> 20,
        VMALLOC_LO
    );
}

fn base_of(slot: usize) -> usize {
    VMALLOC_LO + slot * SLOT
}

/// A cached slot with between `pages` and twice `pages` mapped, taken.
fn take_cached(pages: usize) -> Option<usize> {
    for slot in 0..SLOTS {
        if STATE[slot].load(Ordering::Relaxed) != CACHED {
            continue;
        }
        let mapped = MAPPED[slot].load(Ordering::Relaxed) as usize;
        if mapped < pages || mapped > 2 * pages {
            continue;
        }
        if STATE[slot]
            .compare_exchange(CACHED, LIVE, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            CACHED_BYTES.fetch_sub(mapped * PAGE, Ordering::Relaxed);
            LIVE_BYTES.fetch_add(mapped * PAGE, Ordering::Relaxed);
            REUSED.fetch_add(1, Ordering::Relaxed);
            return Some(slot);
        }
    }
    None
}

fn take_free() -> Option<usize> {
    (0..SLOTS).find(|&slot| {
        STATE[slot]
            .compare_exchange(FREE, BUSY, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    })
}

/// A block of at least `size` bytes, page-aligned and mapped read-write, or
/// `None`: no window, `size` past a slot, the window's lock held by this CPU
/// (an interrupt inside [`alloc`] or [`purge`]), no slot, or no frames. The
/// caller then asks the buddy.
pub fn alloc(size: usize) -> Option<usize> {
    let pages = size.div_ceil(PAGE);
    if pages == 0 || pages > SLOT_PAGES || !READY.load(Ordering::Acquire) {
        return None;
    }
    if LOCK_OWNER.load(Ordering::Acquire) == me() {
        return None;
    }
    if let Some(slot) = take_cached(pages) {
        return Some(base_of(slot));
    }
    let Some(slot) = take_free() else {
        FAILED.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    let base = base_of(slot);
    let mut mapped = 0;
    while mapped < pages {
        // The frame first, off the lock: a frame allocation that fails runs
        // the shrinkers, and they free heap blocks, maybe ours.
        let Some(frame) = PhysFrame::new() else {
            break;
        };
        let page = Page::new_aligned(base + mapped * PAGE, PageSize::Size4K);
        let ok = {
            let _g = lock();
            tables()
                .map(page, frame.paddr(), MMUFlags::READ | MMUFlags::WRITE)
                .is_ok()
        };
        if !ok {
            break;
        }
        core::mem::forget(frame);
        mapped += 1;
    }
    MAPPED[slot].store(mapped as u32, Ordering::Relaxed);
    if mapped < pages {
        // What was mapped stays mapped, as a cached slot for the purge: taking
        // it away here would need the flush this path must not wait on.
        CACHED_BYTES.fetch_add(mapped * PAGE, Ordering::Relaxed);
        STATE[slot].store(if mapped > 0 { CACHED } else { FREE }, Ordering::Release);
        FAILED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    LIVE_BYTES.fetch_add(mapped * PAGE, Ordering::Relaxed);
    STATE[slot].store(LIVE, Ordering::Release);
    Some(base)
}

/// Take back a block [`alloc`] handed out; `false`, touching nothing, if
/// `base` is not the start of a live one. Never unmaps and never waits: the
/// block is cached for reuse and [`purge`].
pub fn free(base: usize) -> bool {
    if !owns(base) || !(base - VMALLOC_LO).is_multiple_of(SLOT) {
        return false;
    }
    let slot = (base - VMALLOC_LO) / SLOT;
    if STATE[slot]
        .compare_exchange(LIVE, CACHED, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return false;
    }
    let bytes = MAPPED[slot].load(Ordering::Relaxed) as usize * PAGE;
    LIVE_BYTES.fetch_sub(bytes, Ordering::Relaxed);
    CACHED_BYTES.fetch_add(bytes, Ordering::Relaxed);
    true
}

/// Bytes in cached slots: what [`purge`] could give back.
pub fn cached_bytes() -> usize {
    CACHED_BYTES.load(Ordering::Relaxed)
}

/// Unmap cached slots and give their frames back until at most `keep` bytes
/// stay cached; answers the bytes given. Waits on every CPU's TLB flush, so
/// only from a context that may wait (`reclaimd`).
pub fn purge(keep: usize) -> usize {
    let mut given = 0;
    for slot in 0..SLOTS {
        if CACHED_BYTES.load(Ordering::Relaxed) <= keep {
            break;
        }
        if STATE[slot]
            .compare_exchange(CACHED, BUSY, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            continue;
        }
        let pages = MAPPED[slot].load(Ordering::Relaxed) as usize;
        let base = base_of(slot);
        // In batches: a batch's frames go back only after every TLB has
        // forgotten them.
        const BATCH: usize = 256;
        let mut done = 0;
        while done < pages {
            let n = (pages - done).min(BATCH);
            let mut frames = [0usize; BATCH];
            {
                let _g = lock();
                let mut pt = tables();
                for (i, f) in frames.iter_mut().enumerate().take(n) {
                    if let Ok((paddr, _)) = pt.unmap_no_shootdown(base + (done + i) * PAGE) {
                        *f = paddr;
                    }
                }
            }
            flush_everywhere();
            for &paddr in frames.iter().take(n) {
                if paddr != 0 {
                    // SAFETY: `alloc` mapped this page to a frame it took with
                    // `PhysFrame::new` and forgot; the flush above was the
                    // last thing that could still reach it.
                    drop(unsafe { PhysFrame::from_virt_owned(crate::mem::phys_to_virt(paddr)) });
                }
            }
            done += n;
        }
        MAPPED[slot].store(0, Ordering::Relaxed);
        CACHED_BYTES.fetch_sub(pages * PAGE, Ordering::Relaxed);
        PURGED_BYTES.fetch_add(pages * PAGE, Ordering::Relaxed);
        STATE[slot].store(FREE, Ordering::Release);
        given += pages * PAGE;
    }
    given
}

/// The purge as a deferred cache, for `reclaimd` under memory pressure.
fn purge_all(_want: usize) -> usize {
    purge(0)
}

static SHRINKER: crate::shrinker::Shrinker = crate::shrinker::Shrinker {
    name: "vmalloc cached blocks",
    count: cached_bytes,
    scan: purge_all,
};

/// Register the purge with the deferred reclaim. After the heap is up.
pub fn register_shrinker() {
    if !crate::shrinker::register_deferred(&SHRINKER) {
        warn!("vmalloc: deferred shrinker registry full, cached blocks go back only on reclaimd's pass");
    }
}

/// The large-block window on a host, on the walker and entry format the
/// machine uses; only the loaded root is a seam (see `vmap_stack`'s tests).
///
/// What these pin is the part of the design that is unusual: a free never
/// unmaps, a cached slot is taken over as it is by a similar size and only by
/// a similar size, an allocation that gets half its frames keeps them mapped
/// for the purge instead of flushing, and the purge gives every frame back.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::page_table::{PageTableImpl, PageTableLevel4};
    use crate::utils::pte::x86_64::X86PTE;
    use crate::utils::test_frames;

    pub(super) type PageTable = PageTableImpl<PageTableLevel4, X86PTE>;

    static ROOT: AtomicUsize = AtomicUsize::new(0);
    static ON_KERNEL: AtomicBool = AtomicBool::new(true);
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub(super) fn tables() -> ManuallyDrop<PageTable> {
        // SAFETY: `ROOT` is a zeroed frame `fresh` took and never frees.
        ManuallyDrop::new(unsafe { PageTable::from_root(ROOT.load(Ordering::Relaxed)) })
    }

    pub(super) fn on_kernel_table() -> bool {
        ON_KERNEL.load(Ordering::Relaxed)
    }

    fn fresh() -> std::sync::MutexGuard<'static, ()> {
        let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        test_frames::install();
        test_frames::frame_budget(None);
        for s in &STATE {
            s.store(FREE, Ordering::Relaxed);
        }
        for m in &MAPPED {
            m.store(0, Ordering::Relaxed);
        }
        READY.store(false, Ordering::Relaxed);
        LOCK_OWNER.store(0, Ordering::Relaxed);
        for c in [&LIVE_BYTES, &CACHED_BYTES, &REUSED, &FAILED, &PURGED_BYTES] {
            c.store(0, Ordering::Relaxed);
        }
        ON_KERNEL.store(true, Ordering::Relaxed);
        let root = PhysFrame::new_zero().expect("root frame");
        ROOT.store(root.paddr(), Ordering::Relaxed);
        core::mem::forget(root);
        g
    }

    fn mapped(va: usize) -> Option<usize> {
        tables().query(va).ok().map(|q| q.0)
    }

    const MIB: usize = 1 << 20;

    #[test]
    fn no_window_no_blocks() {
        let _g = fresh();
        assert_eq!(alloc(MIB), None);
        ON_KERNEL.store(false, Ordering::Relaxed);
        init();
        assert_eq!(alloc(MIB), None, "init off the kernel table builds nothing");
        assert_eq!(stats(), (0, 0, 0, 0, 0), "and none of that is a failure");
    }

    #[test]
    fn init_refuses_a_window_someone_already_mapped() {
        let _g = fresh();
        let f = PhysFrame::new().unwrap();
        let page = Page::new_aligned(VMALLOC_LO, PageSize::Size4K);
        tables().map(page, f.paddr(), MMUFlags::READ).unwrap();
        init();
        assert_eq!(alloc(MIB), None);
        assert_eq!(mapped(VMALLOC_LO), Some(f.paddr()));
    }

    #[test]
    fn init_without_frames_leaves_the_window_off() {
        let _g = fresh();
        test_frames::frame_budget(Some(0));
        init();
        test_frames::frame_budget(Some(2));
        init();
        test_frames::frame_budget(None);
        assert_eq!(alloc(MIB), None);
    }

    #[test]
    fn a_block_is_exactly_the_pages_asked_for() {
        let _g = fresh();
        init();
        assert_eq!(mapped(VMALLOC_LO), None, "init leaves nothing mapped");
        let base = alloc(MIB + 1).expect("block");
        assert_eq!(base, VMALLOC_LO);
        assert!(owns(base) && owns(VMALLOC_HI - 1) && !owns(VMALLOC_HI));
        let pages = (MIB + 1).div_ceil(PAGE);
        let mut seen = std::collections::BTreeSet::new();
        for p in 0..pages {
            assert!(seen.insert(mapped(base + p * PAGE).expect("mapped")));
        }
        assert_eq!(mapped(base + pages * PAGE), None);
        assert_eq!(stats().0, pages * PAGE);
        assert_eq!(alloc(MIB).unwrap(), base + SLOT, "next slot");
    }

    #[test]
    fn sizes_past_a_slot_and_zero_go_to_the_buddy() {
        let _g = fresh();
        init();
        assert_eq!(alloc(0), None);
        assert_eq!(alloc(SLOT + 1), None);
        assert_eq!(stats().3, 0, "not failures: the caller asks the buddy");
    }

    #[test]
    fn free_keeps_the_mapping_and_a_similar_size_takes_it_over() {
        let _g = fresh();
        init();
        let base = alloc(4 * MIB).unwrap();
        let first = mapped(base).unwrap();
        let before = test_frames::live_frames();
        assert!(free(base));
        assert!(!free(base), "a cached block is not freed twice");
        assert_eq!(mapped(base), Some(first), "free never unmaps");
        assert_eq!(test_frames::live_frames(), before, "and keeps its frames");
        assert_eq!(cached_bytes(), 4 * MIB);
        // 3 MiB: the 4 MiB slot is within twice that, so it is reused as is.
        assert_eq!(alloc(3 * MIB), Some(base));
        assert_eq!(mapped(base), Some(first));
        assert_eq!(test_frames::live_frames(), before, "no new frame");
        let (live, cached, reused, _, _) = stats();
        assert_eq!((live, cached, reused), (4 * MIB, 0, 1));
    }

    #[test]
    fn a_cached_slot_is_not_given_to_a_size_it_does_not_fit() {
        let _g = fresh();
        init();
        let base = alloc(4 * MIB).unwrap();
        assert!(free(base));
        // Larger than what is mapped: a new slot.
        assert_eq!(alloc(5 * MIB), Some(base + SLOT));
        // Less than half of it: a new slot too, not 4 MiB for 1 MiB.
        assert_eq!(alloc(MIB), Some(base + 2 * SLOT));
        assert_eq!(stats().2, 0);
    }

    #[test]
    fn free_refuses_what_is_not_a_live_block() {
        let _g = fresh();
        init();
        let base = alloc(MIB).unwrap();
        assert!(!free(base + PAGE));
        assert!(!free(base + SLOT), "a slot never handed out");
        assert!(!free(0x1000));
        assert_eq!(stats().0, MIB);
    }

    #[test]
    fn purge_gives_back_every_frame_down_to_keep() {
        let _g = fresh();
        init();
        let a = alloc(2 * MIB).unwrap();
        let b = alloc(MIB + 3 * PAGE).unwrap();
        let held = test_frames::live_frames();
        assert!(free(a) && free(b));
        // Keep a little: the first slot goes, then the cache is under `keep`.
        assert_eq!(purge(MIB + 4 * PAGE), 2 * MIB);
        assert_eq!(mapped(a), None);
        assert!(mapped(b).is_some(), "second still cached");
        assert_eq!(test_frames::live_frames(), held - 2 * MIB / PAGE);
        assert_eq!(purge(0), MIB + 3 * PAGE);
        assert_eq!(
            test_frames::live_frames(),
            held - (3 * MIB + 3 * PAGE) / PAGE
        );
        assert_eq!(cached_bytes(), 0);
        assert_eq!(stats().4, 3 * MIB + 3 * PAGE);
        // A purged slot is free again, and a live one was never touched.
        let c = alloc(MIB).unwrap();
        assert_eq!(c, a);
        assert_eq!(purge(0), 0, "nothing cached, nothing given");
        assert!(mapped(c).is_some());
    }

    /// More than one purge batch (256 pages) in one slot.
    #[test]
    fn purge_of_a_block_past_one_batch_takes_all_of_it() {
        let _g = fresh();
        init();
        let a = alloc(3 * MIB).unwrap();
        assert!(free(a));
        assert_eq!(purge(0), 3 * MIB);
        for p in (0..3 * MIB / PAGE).step_by(97) {
            assert_eq!(mapped(a + p * PAGE), None, "page {}", p);
        }
    }

    #[test]
    fn running_out_of_frames_keeps_what_was_mapped_for_the_purge() {
        let _g = fresh();
        init();
        let warm = alloc(MIB).unwrap();
        assert!(free(warm));
        assert_eq!(purge(0), MIB);
        let before = test_frames::live_frames();
        test_frames::frame_budget(Some(5));
        assert_eq!(alloc(MIB), None);
        test_frames::frame_budget(None);
        assert_eq!(stats().3, 1);
        assert_eq!(cached_bytes(), 5 * PAGE, "kept, not flushed");
        assert!(mapped(warm + 4 * PAGE).is_some());
        // A cached slot of 5 pages is not a 1 MiB block.
        assert_eq!(alloc(MIB), Some(warm + SLOT));
        assert_eq!(purge(0), 5 * PAGE);
        assert_eq!(mapped(warm), None);
        // The new block's pages, and the page table its slot needed.
        assert_eq!(test_frames::live_frames(), before + MIB / PAGE + 1);
    }

    #[test]
    fn running_out_before_the_first_frame_frees_the_slot() {
        let _g = fresh();
        init();
        test_frames::frame_budget(Some(0));
        assert_eq!(alloc(MIB), None);
        test_frames::frame_budget(None);
        assert_eq!(cached_bytes(), 0);
        assert_eq!(alloc(MIB), Some(VMALLOC_LO), "slot 0 is free again");
    }

    /// An interrupt that allocates while this CPU holds the window's lock
    /// must not spin on it: it is told no and goes to the buddy.
    #[test]
    fn an_allocation_inside_the_lock_holder_is_refused() {
        let _g = fresh();
        init();
        let held = lock();
        assert_eq!(alloc(MIB), None);
        drop(held);
        assert_eq!(LOCK_OWNER.load(Ordering::Relaxed), 0);
        assert!(alloc(MIB).is_some());
    }

    #[test]
    fn a_full_window_is_a_failure_and_a_freed_slot_comes_back() {
        let _g = fresh();
        init();
        let bases: Vec<usize> = (0..SLOTS).map(|_| alloc(PAGE).unwrap()).collect();
        assert_eq!(alloc(PAGE), None);
        assert_eq!(stats().3, 1);
        assert!(free(bases[3]));
        assert_eq!(alloc(PAGE), Some(bases[3]));
    }
}
