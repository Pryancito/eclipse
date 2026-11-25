//! Coroutine stacks mapped from scattered frames: Linux's `CONFIG_VMAP_STACK`.
//!
//! A coroutine stack used to be a heap block, and the heap hands out a 2.6 MiB
//! request as a 4 MiB buddy block, which is 4 MiB of *physically contiguous*,
//! 4 MiB-aligned memory. On a machine that has been up for a while that is the
//! one thing there may be none of: in QEMU, with a few programs holding RAM,
//! `Executor::new` failed with 1.7 GiB free and 254 MiB free in the heap
//! itself, because no free run of frames was 4 MiB long and aligned, and the
//! machine stopped with `Alloction Stack Failed` (real hardware, 2026-11-21,
//! three times).
//!
//! Here a stack is a slot of virtual window ([`VMAP_STACK_LO`]) with one
//! frame per page, wherever the frame allocator finds them. The window has a
//! top-level page-table entry of its own, created at boot before any address
//! space is cloned, so every address space walks into the same tables and a
//! stack mapped later is visible to all of them.
//!
//! Everything else about a stack is unchanged: the guard bands are still
//! installed by [`crate::stack_guard`], which finds 4 KiB entries here and so
//! never has to split a huge one (the split-frame reserve is not touched); the
//! scheduler still pools retired blocks; and only the periodic trim gives one
//! back, through [`free`].

use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

#[cfg(test)]
use self::tests::PageTable;
use crate::common::kaddr::{VMAP_STACK_LO, VMAP_STACK_SLOT, VMAP_STACK_SLOTS};
use crate::mem::PhysFrame;
#[cfg(not(test))]
use crate::vm::PageTable;
use crate::vm::{GenericPageTable, Page, PageSize};
use crate::MMUFlags;

const PAGE_SIZE: usize = 4096;
const SLOT: usize = VMAP_STACK_SLOT as usize;
const SLOTS: usize = VMAP_STACK_SLOTS as usize;
const SLOT_WORDS: usize = SLOTS.div_ceil(64);

/// One bit per slot: set while a stack holds it.
static SLOT_USED: [AtomicU64; SLOT_WORDS] = [const { AtomicU64::new(0) }; SLOT_WORDS];
/// Whether [`init`] made the window; until then [`alloc`] answers `None` and
/// the scheduler keeps carving stacks from the heap.
static READY: AtomicBool = AtomicBool::new(false);
/// Serializes edits of the window's tables. Taken by spinning with the
/// shootdown queue pumped, like `stack_guard`'s split lock: [`alloc`] runs
/// from `Executor::new` with interrupts off, and the holder of the lock may be
/// flushing every CPU.
static LOCK: spin::Mutex<()> = spin::Mutex::new(());

static LIVE: AtomicUsize = AtomicUsize::new(0);
static FAILED: AtomicUsize = AtomicUsize::new(0);
/// The failures split by cause, for the OOM report: every one of them is a
/// stack that went to the kernel heap as a 4 MiB block instead.
static FAILED_NOT_READY: AtomicUsize = AtomicUsize::new(0);
static FAILED_NO_SLOT: AtomicUsize = AtomicUsize::new(0);
static FAILED_NO_FRAME: AtomicUsize = AtomicUsize::new(0);
/// Why [`init`] left the window off, as one of the `INIT_*` codes.
static INIT_STATE: AtomicUsize = AtomicUsize::new(INIT_NOT_RUN);
const INIT_NOT_RUN: usize = 0;
const INIT_OK: usize = 1;
const INIT_NO_SLOTS: usize = 2;
const INIT_WRONG_TABLE: usize = 3;
const INIT_ALREADY_MAPPED: usize = 4;
const INIT_NO_FRAME: usize = 5;
const INIT_MAP_FAILED: usize = 6;

/// `(stacks mapped now, allocations that found no frame or no slot)`.
pub fn stats() -> (usize, usize) {
    (LIVE.load(Ordering::Relaxed), FAILED.load(Ordering::Relaxed))
}

/// Everything the OOM report needs to say why stacks ended up in the heap.
#[derive(Clone, Copy, Debug, Default)]
pub struct Detail {
    /// What [`init`] made of the window.
    pub window: &'static str,
    /// Stacks mapped in the window now.
    pub live: usize,
    /// Slots in the window.
    pub slots: usize,
    /// Refusals because the window was off.
    pub not_ready: usize,
    /// Refusals because every slot was taken.
    pub no_slot: usize,
    /// Refusals because a frame could not be had or mapped.
    pub no_frame: usize,
}

/// See [`Detail`]. Allocates nothing.
pub fn detail() -> Detail {
    let window = match INIT_STATE.load(Ordering::Relaxed) {
        INIT_OK => "on",
        INIT_NO_SLOTS => "off (no slots on this arch)",
        INIT_WRONG_TABLE => "off (init not on the kernel page table)",
        INIT_ALREADY_MAPPED => "off (its PML4 slot was already mapped)",
        INIT_NO_FRAME => "off (no frame to build it)",
        INIT_MAP_FAILED => "off (could not build its tables)",
        _ => "off (init never ran)",
    };
    Detail {
        window,
        live: LIVE.load(Ordering::Relaxed),
        slots: SLOTS,
        not_ready: FAILED_NOT_READY.load(Ordering::Relaxed),
        no_slot: FAILED_NO_SLOT.load(Ordering::Relaxed),
        no_frame: FAILED_NO_FRAME.load(Ordering::Relaxed),
    }
}

/// Whether `base` is a block [`alloc`] handed out (a slot's first byte).
pub fn owns(base: usize) -> bool {
    let a = base as u64;
    (VMAP_STACK_LO..VMAP_STACK_LO + VMAP_STACK_SLOT * VMAP_STACK_SLOTS).contains(&a)
        && (a - VMAP_STACK_LO).is_multiple_of(VMAP_STACK_SLOT)
}

fn lock() -> spin::MutexGuard<'static, ()> {
    loop {
        if let Some(g) = LOCK.try_lock() {
            return g;
        }
        crate::common::ipi::tlb_shootdown_pump();
        core::hint::spin_loop();
    }
}

/// The kernel half's tables, walked from whichever root is loaded: every
/// address space shares the window's sub-tables, so any root reaches them.
///
/// `ManuallyDrop` because a `PageTable` owns the intermediate tables it
/// creates and frees them when dropped, and the window's tables are for ever.
/// The only thing that leaks is the small vector naming them, on the calls
/// that had to create one.
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

/// Every CPU's TLB, after a change that took pages away.
fn flush_everywhere() {
    crate::vm::flush_tlb(None);
    crate::common::ipi::remote_flush_tlb_aspace(None, None);
}

/// Make the window. Called once at boot, from `stack_guard::init`, while the
/// kernel's own root is loaded and before any address space has been cloned
/// from it: the top-level entry made here is what every later clone copies.
/// Leaves the window off (stacks keep coming from the heap) if the entry is
/// already taken or the tables cannot be made.
pub fn init() {
    if SLOTS == 0 {
        INIT_STATE.store(INIT_NO_SLOTS, Ordering::Relaxed);
        return;
    }
    if !on_kernel_table() {
        INIT_STATE.store(INIT_WRONG_TABLE, Ordering::Relaxed);
        warn!("vmap_stack: not on the kernel's own page table, stacks stay in the heap");
        return;
    }
    let mut pt = tables();
    // The entry must be ours alone: anything already mapped in the window
    // means another user of this top-level slot.
    if pt.query(VMAP_STACK_LO as usize).is_ok() {
        INIT_STATE.store(INIT_ALREADY_MAPPED, Ordering::Relaxed);
        warn!(
            "vmap_stack: {:#x} is already mapped, stacks stay in the heap",
            VMAP_STACK_LO
        );
        return;
    }
    // Map and unmap one page in each gigabyte of the window. That makes the
    // top-level entry and every second-level table now, while nothing has
    // been cloned yet; the tables under them are shared by construction, so
    // what `alloc` adds later needs no copying anywhere.
    let Some(frame) = PhysFrame::new() else {
        INIT_STATE.store(INIT_NO_FRAME, Ordering::Relaxed);
        warn!("vmap_stack: no frame to build the window, stacks stay in the heap");
        return;
    };
    const GIB: usize = 1 << 30;
    let window = SLOT * SLOTS;
    for off in (0..window).step_by(GIB) {
        let va = VMAP_STACK_LO as usize + off;
        let page = Page::new_aligned(va, PageSize::Size4K);
        if pt
            .map(page, frame.paddr(), MMUFlags::READ | MMUFlags::WRITE)
            .is_err()
            || pt.unmap_no_shootdown(va).is_err()
        {
            INIT_STATE.store(INIT_MAP_FAILED, Ordering::Relaxed);
            warn!("vmap_stack: could not build the window, stacks stay in the heap");
            flush_everywhere();
            return;
        }
    }
    flush_everywhere();
    drop(frame);
    READY.store(true, Ordering::Release);
    INIT_STATE.store(INIT_OK, Ordering::Relaxed);
    info!(
        "vmap_stack: {} coroutine-stack slots of {} KiB at {:#x}",
        SLOTS,
        SLOT / 1024,
        VMAP_STACK_LO
    );
}

fn take_slot() -> Option<usize> {
    for (w, word) in SLOT_USED.iter().enumerate() {
        let mut cur = word.load(Ordering::Relaxed);
        while cur != u64::MAX {
            let bit = (!cur).trailing_zeros() as usize;
            match word.compare_exchange_weak(
                cur,
                cur | (1 << bit),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    let slot = w * 64 + bit;
                    if slot >= SLOTS {
                        word.fetch_and(!(1 << bit), Ordering::Release);
                        return None;
                    }
                    return Some(slot);
                }
                Err(now) => cur = now,
            }
        }
    }
    None
}

fn give_slot(slot: usize) {
    SLOT_USED[slot / 64].fetch_and(!(1u64 << (slot % 64)), Ordering::Release);
}

/// Unmap `[base, base + mapped)` and free its frames. The caller flushes
/// every TLB before the slot can be handed out again.
///
/// A frame goes back before the flush. Nothing reads or writes these pages by
/// now (an allocation being rolled back was never handed out; a stack being
/// freed sat out a grace period in the pool), so a stale entry elsewhere is a
/// translation nobody uses; it is the same trust the heap puts in a freed
/// block.
fn unmap_range(pt: &mut PageTable, base: usize, mapped: usize) {
    for off in (0..mapped).step_by(PAGE_SIZE) {
        if let Ok((paddr, _)) = pt.unmap_no_shootdown(base + off) {
            // SAFETY: every page here was mapped by `alloc` to a frame it took
            // from the frame allocator with `PhysFrame::new` and forgot.
            drop(unsafe { PhysFrame::from_virt_owned(crate::mem::phys_to_virt(paddr)) });
        }
    }
}

/// A block of `size` bytes, page-aligned and mapped read-write, in a slot of
/// the window, or `None` (no window, no slot, or no frames).
pub fn alloc(size: usize) -> Option<usize> {
    if size == 0 || size > SLOT {
        return None;
    }
    if !READY.load(Ordering::Acquire) {
        FAILED.fetch_add(1, Ordering::Relaxed);
        FAILED_NOT_READY.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let Some(slot) = take_slot() else {
        FAILED.fetch_add(1, Ordering::Relaxed);
        if FAILED_NO_SLOT.fetch_add(1, Ordering::Relaxed) == 0 {
            // Once: from here every new stack is a 4 MiB heap block, and the
            // heap has a ceiling. 2048 slots is far past any live peak, so
            // reaching it means stacks are being leaked, not used.
            warn!(
                "vmap_stack: all {} slots taken ({} live) -- new coroutine stacks fall back \
                 to the kernel heap; stacks are probably leaking (see /proc/perf/kernel)",
                SLOTS,
                LIVE.load(Ordering::Relaxed),
            );
        }
        return None;
    };
    let base = VMAP_STACK_LO as usize + slot * SLOT;
    let size = size.next_multiple_of(PAGE_SIZE);
    let guard = lock();
    let mut pt = tables();
    let mut mapped = 0;
    while mapped < size {
        let Some(frame) = PhysFrame::new() else {
            break;
        };
        let page = Page::new_aligned(base + mapped, PageSize::Size4K);
        if pt
            .map(page, frame.paddr(), MMUFlags::READ | MMUFlags::WRITE)
            .is_err()
        {
            break;
        }
        core::mem::forget(frame);
        mapped += PAGE_SIZE;
    }
    if mapped < size {
        unmap_range(&mut pt, base, mapped);
        drop(guard);
        flush_everywhere();
        give_slot(slot);
        FAILED.fetch_add(1, Ordering::Relaxed);
        FAILED_NO_FRAME.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    drop(guard);
    LIVE.fetch_add(1, Ordering::Relaxed);
    Some(base)
}

/// Give back a block [`alloc`] handed out: its frames to the frame allocator,
/// its slot to the window. `false`, touching nothing, if `base` is not one.
///
/// The caller must have removed whatever it changed in the mapping (the guard
/// bands) first: the pages are unmapped by reading which frame each holds,
/// and a band `stack_guard` installed keeps its frame in the entry, so it is
/// freed with the rest either way.
pub fn free(base: usize, size: usize) -> bool {
    if !owns(base) {
        return false;
    }
    let slot = (base - VMAP_STACK_LO as usize) / SLOT;
    let size = size.next_multiple_of(PAGE_SIZE).min(SLOT);
    let guard = lock();
    let mut pt = tables();
    unmap_range(&mut pt, base, size);
    drop(guard);
    flush_everywhere();
    give_slot(slot);
    LIVE.fetch_sub(1, Ordering::Relaxed);
    true
}

/// The window on a host, on the walker and entry format the machine uses.
///
/// Everything above runs as written except two things a host has no register
/// for: the loaded root ([`tables`]) is a table this module builds, and
/// whether it is the kernel's own ([`on_kernel_table`]) is a flag. What these
/// pin is what the real-hardware panics of 2026-11-21 depended on and no
/// emulator run exercises: that a stack mapped after an address space was
/// cloned is visible through the clone, that a stack which gets half its
/// frames gives every one of them back and its slot too, and that a full
/// window says no instead of handing out a taken slot.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::page_table::{PageTableImpl, PageTableLevel4};
    use crate::utils::pte::x86_64::X86PTE;
    use crate::utils::test_frames;
    use core::sync::atomic::AtomicBool;

    pub(super) type PageTable = PageTableImpl<PageTableLevel4, X86PTE>;

    static ROOT: AtomicUsize = AtomicUsize::new(0);
    static ON_KERNEL: AtomicBool = AtomicBool::new(true);
    /// The module's state is global, as on the machine: one test at a time.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub(super) fn tables() -> ManuallyDrop<PageTable> {
        // SAFETY: `ROOT` is a zeroed frame `fresh` took and never frees.
        ManuallyDrop::new(unsafe { PageTable::from_root(ROOT.load(Ordering::Relaxed)) })
    }

    pub(super) fn on_kernel_table() -> bool {
        ON_KERNEL.load(Ordering::Relaxed)
    }

    /// A boot: no window, no slot taken, nothing counted, an empty root.
    fn fresh() -> std::sync::MutexGuard<'static, ()> {
        let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        test_frames::install();
        test_frames::frame_budget(None);
        for w in &SLOT_USED {
            w.store(0, Ordering::Relaxed);
        }
        READY.store(false, Ordering::Relaxed);
        for c in [
            &LIVE,
            &FAILED,
            &FAILED_NOT_READY,
            &FAILED_NO_SLOT,
            &FAILED_NO_FRAME,
        ] {
            c.store(0, Ordering::Relaxed);
        }
        INIT_STATE.store(INIT_NOT_RUN, Ordering::Relaxed);
        ON_KERNEL.store(true, Ordering::Relaxed);
        let root = PhysFrame::new_zero().expect("root frame");
        ROOT.store(root.paddr(), Ordering::Relaxed);
        core::mem::forget(root);
        g
    }

    fn mapped(va: usize) -> Option<(usize, MMUFlags)> {
        tables().query(va).ok().map(|(pa, f, _)| (pa, f))
    }

    /// The top-level entry of the window, read straight out of the root.
    fn pml4_entry() -> u64 {
        let idx = (VMAP_STACK_LO as usize >> 39) & 0x1ff;
        let root = crate::mem::phys_to_virt(ROOT.load(Ordering::Relaxed)) as *const u64;
        // SAFETY: the root is a whole frame of 512 entries in mock memory.
        unsafe { root.add(idx).read() }
    }

    const STACK: usize = 2 * 1024 * 1024 + 512 * 1024 + 64 * 1024;

    #[test]
    fn before_init_every_stack_is_refused_and_counted() {
        let _g = fresh();
        assert_eq!(alloc(STACK), None);
        let d = detail();
        assert_eq!(d.window, "off (init never ran)");
        assert_eq!((d.not_ready, d.live, d.slots), (1, 0, SLOTS));
        assert_eq!(stats(), (0, 1));
    }

    #[test]
    fn init_off_the_kernel_table_leaves_the_window_off() {
        let _g = fresh();
        ON_KERNEL.store(false, Ordering::Relaxed);
        init();
        assert_eq!(detail().window, "off (init not on the kernel page table)");
        assert_eq!(pml4_entry(), 0, "nothing built");
        assert_eq!(alloc(STACK), None);
    }

    #[test]
    fn init_refuses_a_window_someone_already_mapped() {
        let _g = fresh();
        let f = PhysFrame::new().unwrap();
        let page = Page::new_aligned(VMAP_STACK_LO as usize, PageSize::Size4K);
        tables().map(page, f.paddr(), MMUFlags::READ).unwrap();
        init();
        assert_eq!(detail().window, "off (its PML4 slot was already mapped)");
        assert_eq!(
            mapped(VMAP_STACK_LO as usize).map(|m| m.0),
            Some(f.paddr()),
            "the other user's page is left alone"
        );
        assert_eq!(alloc(STACK), None);
    }

    #[test]
    fn init_without_a_frame_leaves_the_window_off() {
        let _g = fresh();
        test_frames::frame_budget(Some(0));
        init();
        test_frames::frame_budget(None);
        assert_eq!(detail().window, "off (no frame to build it)");
        assert_eq!(alloc(STACK), None);
    }

    #[test]
    fn init_that_cannot_build_its_tables_says_so() {
        let _g = fresh();
        // The probe frame, and two of the tables under the top-level entry:
        // the first gigabyte needs three (PDPT, PD, PT), so it fails there.
        test_frames::frame_budget(Some(3));
        init();
        test_frames::frame_budget(None);
        assert_eq!(detail().window, "off (could not build its tables)");
        assert_eq!(alloc(STACK), None);
    }

    #[test]
    fn init_builds_the_top_level_entry_and_maps_nothing() {
        let _g = fresh();
        init();
        assert_eq!(detail().window, "on");
        assert_ne!(pml4_entry() & 1, 0, "top-level entry present");
        for gib in 0..(SLOT * SLOTS) >> 30 {
            assert_eq!(mapped(VMAP_STACK_LO as usize + (gib << 30)), None);
        }
    }

    #[test]
    fn a_stack_is_every_page_rw_on_its_own_frame_and_no_page_more() {
        let _g = fresh();
        init();
        let size = STACK + 1;
        let base = alloc(size).expect("stack");
        assert_eq!(base, VMAP_STACK_LO as usize);
        assert!(owns(base));
        assert!(!owns(base + PAGE_SIZE));
        let pages = size.div_ceil(PAGE_SIZE);
        let mut frames = std::collections::BTreeSet::new();
        for p in 0..pages {
            let (pa, f) = mapped(base + p * PAGE_SIZE).expect("page mapped");
            assert!(f.contains(MMUFlags::READ | MMUFlags::WRITE), "{:?}", f);
            assert!(!f.contains(MMUFlags::USER), "kernel only: {:?}", f);
            assert!(frames.insert(pa), "frame {:#x} mapped twice", pa);
        }
        assert_eq!(
            mapped(base + pages * PAGE_SIZE),
            None,
            "one page past the end"
        );
        assert_eq!(stats(), (1, 0));
        // The next stack is the next slot, not an overlap.
        let second = alloc(STACK).unwrap();
        assert_eq!(second, base + SLOT);
    }

    #[test]
    fn sizes_a_slot_cannot_hold_are_refused_without_counting() {
        let _g = fresh();
        init();
        assert_eq!(alloc(0), None);
        assert_eq!(alloc(SLOT + 1), None);
        assert!(alloc(SLOT).is_some(), "a whole slot fits");
        assert_eq!(stats(), (1, 0));
    }

    /// The window is made before any address space is cloned, so a clone
    /// shares its tables: a stack mapped afterwards must be reachable from the
    /// clone too, or a CPU running that process faults on its own stack.
    #[test]
    fn a_stack_mapped_after_a_clone_is_seen_through_the_clone() {
        let _g = fresh();
        init();
        // What x86_64's `pt_clone_kernel_space` does for every new process:
        // the kernel half's top-level entries, copied by value.
        let new_root = PhysFrame::new_zero().unwrap();
        let src = crate::mem::phys_to_virt(ROOT.load(Ordering::Relaxed)) as *const u64;
        let dst = crate::mem::phys_to_virt(new_root.paddr()) as *mut u64;
        for i in 0x100..0x200 {
            // SAFETY: both are whole frames of 512 entries in mock memory.
            unsafe { dst.add(i).write(src.add(i).read()) };
        }
        // SAFETY: the clone's root outlives it; its sub-tables are shared and
        // must not be freed, so it is never dropped.
        let clone = ManuallyDrop::new(unsafe { PageTable::from_root(new_root.paddr()) });
        let base = alloc(STACK).unwrap();
        for off in [0, STACK - PAGE_SIZE] {
            let want = mapped(base + off).unwrap().0;
            assert_eq!(clone.query(base + off).ok().map(|q| q.0), Some(want));
        }
        assert!(free(base, STACK));
        assert!(clone.query(base).is_err(), "and its unmap too");
    }

    #[test]
    fn free_gives_back_every_frame_and_the_slot() {
        let _g = fresh();
        init();
        // Warm the slot so its page tables exist before counting.
        let base = alloc(STACK).unwrap();
        assert!(free(base, STACK));
        let before = test_frames::live_frames();
        let again = alloc(STACK).unwrap();
        assert_eq!(again, base, "lowest free slot again");
        assert_eq!(
            test_frames::live_frames() - before,
            STACK.div_ceil(PAGE_SIZE)
        );
        assert!(free(again, STACK));
        assert_eq!(test_frames::live_frames(), before);
        assert_eq!(mapped(base), None);
        assert_eq!(stats(), (0, 0));
    }

    #[test]
    fn free_refuses_what_is_not_a_slot_base() {
        let _g = fresh();
        init();
        let base = alloc(STACK).unwrap();
        assert!(!free(base + PAGE_SIZE, STACK));
        assert!(!free(0x1000, STACK));
        assert!(!free(crate::common::kaddr::VMAP_STACK_HI as usize, STACK));
        assert!(mapped(base).is_some(), "untouched");
        assert_eq!(stats().0, 1);
    }

    #[test]
    fn a_stack_that_runs_out_of_frames_rolls_back_whole() {
        let _g = fresh();
        init();
        let base = alloc(STACK).unwrap();
        assert!(free(base, STACK));
        let before = test_frames::live_frames();
        test_frames::frame_budget(Some(10));
        assert_eq!(alloc(STACK), None);
        test_frames::frame_budget(None);
        assert_eq!(
            test_frames::live_frames(),
            before,
            "the 10 frames came back"
        );
        for p in 0..11 {
            assert_eq!(mapped(base + p * PAGE_SIZE), None);
        }
        let d = detail();
        assert_eq!((d.no_frame, d.live), (1, 0));
        assert_eq!(alloc(STACK), Some(base), "and the slot");
    }

    #[test]
    fn a_full_window_says_no_and_takes_a_freed_slot_back() {
        let _g = fresh();
        init();
        let bases: Vec<usize> = (0..SLOTS).map(|_| alloc(PAGE_SIZE).unwrap()).collect();
        assert_eq!(
            bases[SLOTS - 1],
            VMAP_STACK_LO as usize + (SLOTS - 1) * SLOT
        );
        assert_eq!(alloc(PAGE_SIZE), None);
        assert_eq!(alloc(PAGE_SIZE), None);
        assert_eq!(detail().no_slot, 2);
        assert!(SLOT_USED
            .iter()
            .all(|w| w.load(Ordering::Relaxed) == u64::MAX));
        assert!(free(bases[700], PAGE_SIZE));
        assert_eq!(alloc(PAGE_SIZE), Some(bases[700]));
        assert_eq!(stats().0, SLOTS);
    }
}
