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

use crate::common::kaddr::{VMAP_STACK_LO, VMAP_STACK_SLOT, VMAP_STACK_SLOTS};
use crate::mem::PhysFrame;
use crate::vm::{GenericPageTable, Page, PageSize, PageTable};
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

/// `(stacks mapped now, allocations that found no frame or no slot)`.
pub fn stats() -> (usize, usize) {
    (LIVE.load(Ordering::Relaxed), FAILED.load(Ordering::Relaxed))
}

/// Whether `base` is a block [`alloc`] handed out (a slot's first byte).
pub fn owns(base: usize) -> bool {
    let a = base as u64;
    (VMAP_STACK_LO..VMAP_STACK_LO + VMAP_STACK_SLOT * VMAP_STACK_SLOTS).contains(&a)
        && (a - VMAP_STACK_LO) % VMAP_STACK_SLOT == 0
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
fn tables() -> ManuallyDrop<PageTable> {
    ManuallyDrop::new(PageTable::from_current())
}

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
        return;
    }
    if crate::vm::current_vmtoken() != crate::vm::kernel_vmtoken() {
        warn!("vmap_stack: not on the kernel's own page table, stacks stay in the heap");
        return;
    }
    let mut pt = tables();
    // The entry must be ours alone: anything already mapped in the window
    // means another user of this top-level slot.
    if pt.query(VMAP_STACK_LO as usize).is_ok() {
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
            warn!("vmap_stack: could not build the window, stacks stay in the heap");
            flush_everywhere();
            return;
        }
    }
    flush_everywhere();
    drop(frame);
    READY.store(true, Ordering::Release);
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
    if !READY.load(Ordering::Acquire) || size == 0 || size > SLOT {
        return None;
    }
    let Some(slot) = take_slot() else {
        FAILED.fetch_add(1, Ordering::Relaxed);
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
