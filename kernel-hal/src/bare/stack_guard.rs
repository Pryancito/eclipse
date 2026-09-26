//! Unmapped guard bands around the scheduler's coroutine stacks.
//!
//! A coroutine stack is a plain heap allocation (`PreemptiveScheduler`'s
//! `Executor::new`), laid out as
//! `[bottom guard][usable STACK_SIZE][top guard]`, and the kernel heap is a
//! 512 MiB `static mut` array in `.bss`. So a stack that runs off either end
//! does not fault — it quietly overwrites whichever heap object happens to sit
//! next to it. That silent overwrite is the root of the whole corruption hunt
//! in `docs/README-crash-repro.md`: return addresses with a mangled top byte,
//! `Arc` vtables replaced by `0x87`, a `memset` handed a pointer with one byte
//! flipped, and indirect calls landing on `rip=0x0`/`0x3`.
//!
//! This module makes those two bands **unmapped**, so the overflow takes a
//! clean page fault that `zcore::handler` reports as
//! `[stack-guard] COROUTINE STACK OVERFLOW` — naming the bug instead of
//! scattering it across unrelated memory. The scheduler falls back to filling
//! the bands with canary words if [`install`] refuses, which still *detects*
//! an overflow (at the next poll or timer tick) but only after the heap is
//! already corrupt.
//!
//! # How a page is taken away
//!
//! Not by unmapping it: by **clearing every permission bit while leaving the
//! physical address in the entry**. On all three architectures an empty
//! `MMUFlags` converts to an entry with no present/valid bit, so the CPU faults
//! on any access, while `PageTableEntry::addr()` still holds the frame. That
//! matters because these frames are `.bss` — they belong to the kernel image,
//! not to the frame allocator — and this way the page table itself remembers
//! them: there is no side table of physical addresses to keep in sync, nothing
//! to allocate (so no heap re-entry from inside `Executor::new`), and no way
//! for a bookkeeping slip to hand a `.bss` frame to the frame allocator.
//! [`remove`] just writes the original flags back.
//!
//! Everything here is all-or-nothing. Any surprise — a huge PTE covering the
//! band, a page that is not mapped, flags that differ across the band, a
//! verification read that does not show the page gone — rolls the whole band
//! back and returns `false`, which is exactly the soft-canary fallback the
//! scheduler already handles. The failure mode of this module is "no better
//! than before", never "worse".

use core::sync::atomic::Ordering;

use crate::common::guard_band::{self, BandRefusal, Take};
use crate::fault_slots::SlotTable;
use crate::mem::PhysFrame;
use crate::phys_watch::{FramePool, FrameSet, FreedRing};
use crate::vm::{GenericPageTable, PageTable};
use crate::{MMUFlags, PhysAddr};

const PAGE_SIZE: usize = 4096;

// ── Physical-frame registry: catch a physmap write that aliases a stack ───────
//
// The leading theory for the recurring null-range zeroing crash: a physical
// frame backing a VMO page physically aliases a live coroutine stack, and
// `VMObjectPaged::zero`'s `pmem_zero(paddr, ...)` (a physmap write) zeros the
// stack. The VA-based double-alloc tripwire cannot see this — the write lands
// through the physmap VA (`0xffff_8000 + paddr`), a *different* virtual address
// than the stack's kernel-image VA that maps the *same* physical frame.
//
// So track the PHYSICAL frames each live coroutine stack occupies, and let
// `pmem_zero`/`pmem_write` check their target against them. A hit is the
// smoking gun: a physmap write about to zero a live stack, caught with the
// writer's own call chain.
//
// A bitset over frame numbers, covering up to 16 GiB of RAM (4 Mi frames =
// 512 KiB static). Frames past that are not tracked (reported once): a machine
// with the kernel heap above 16 GiB is not a configuration this hunt targets.
const TRACKED_FRAMES: usize = 4 * 1024 * 1024; // 16 GiB / 4 KiB
const FRAME_WORDS: usize = TRACKED_FRAMES / 64;
/// The bitset itself, and the walk over it, live in `crate::phys_watch`, where
/// every build compiles them.
static STACK_FRAMES: FrameSet<FRAME_WORDS> = FrameSet::new();

/// How many frames past the tracked range were offered, for the boot log.
pub fn stack_frames_over_cap() -> usize {
    STACK_FRAMES.over_cap()
}

// ── Recently-freed DMA-block ring: catch the DEVICE/userspace-mapping UAF ─────
//
// The physmap guard above only catches a *CPU* write (`pmem_zero`/`pmem_write`)
// that aliases a live stack. It cannot see the two writers that reach a stack
// frame WITHOUT going through those primitives:
//
//   * a device DMA (a NIC RX ring, a GPU pushbuffer/GEM, an NVMe completion)
//     whose descriptor still points at a physical block after the driver freed
//     it — the device writes long after the CPU moved on;
//   * a userspace `VmObject::new_physical` mapping (the nouveau-uAPI GEM CPU
//     mmap: `gem_map_cpu` publishes `memdescGetPhysAddr`, Mesa mmaps it) that
//     outlives the GEM free.
//
// Both are the SAME shape: a physical block handed to a device / mapped into a
// process, then freed by `drivers_dma_dealloc` straight back to the general
// frame pool (no quarantine — see that function), then re-handed by
// `frame_alloc` to a fresh coroutine stack. `frame_alias_check` passes at that
// realloc (nothing lived there when the block was freed), and the stale
// mapping/descriptor then writes the recycled stack — the "all-zeros usable
// region, no guard hit" signature of the desktop-start crash on real NVIDIA
// hardware, where every zero-VRAM GEM travels this CPU-mmap path.
//
// This ring records the last N freed DMA blocks. The null-execute/soft-smash
// fault path asks `paddr_recently_freed_dma` whether the corrupted stack frame
// was one of them: a hit CONFIRMS the UAF and names it (a canary only ever said
// corruption *happened*, never that a freed DMA buffer was the writer). Pure
// diagnostic — recording is a couple of relaxed stores per DMA free, the lookup
// runs only on the already-fatal fault path.
const DMA_RING_SLOTS: usize = 512;
/// The ring itself, and its publication order, live in `crate::phys_watch`.
static DMA_FREED: FreedRing<DMA_RING_SLOTS> = FreedRing::new();

/// Record that `[paddr, paddr + pages*PAGE)` was just freed by a DMA path.
/// Called from `drivers_dma_dealloc` for every block returned to the pool.
pub fn dma_free_note(paddr: usize, pages: usize) {
    DMA_FREED.note(paddr, pages);
}

/// If `paddr` falls inside a recently-freed DMA block, return how many DMA
/// frees have happened since (0 = the most recent). `None` if not found —
/// either it was never a DMA buffer, or it aged out of the ring.
pub fn paddr_recently_freed_dma(paddr: usize) -> Option<u64> {
    DMA_FREED.since(paddr)
}

// ── Userspace pin of DMA frames (GEM CPU-mmap) ───────────────────────────────
//
// The bookkeeping lives in `crate::dma_pin`, which compiles everywhere and is
// tested; here are the wrappers that pair it with the frame pool. It used to
// sit in this file, which nothing but an x86 bare build compiles.

/// Pin `[paddr, paddr+pages*PAGE)` against returning to the frame pool.
/// Called from `VmObject::new_physical`.
pub fn dma_pin_user(paddr: usize, pages: usize) {
    crate::dma_pin::pin(paddr, pages);
}

/// Drop one pin. When the last pin on a range goes away, any DMA free that was
/// waiting on it is released into the quarantine.
pub fn dma_unpin_user(paddr: usize, pages: usize) {
    // Released outside the registry's lock: `frame_dealloc` and the
    // quarantine's poison scan must not run under it.
    for (base, n) in crate::dma_pin::unpin(paddr, pages) {
        crate::drivers::dma_quarantine_release_held(base, n);
    }
}

/// True if any userspace pin overlaps `[paddr, paddr+pages*PAGE)`.
pub fn dma_user_pinned(paddr: usize, pages: usize) -> bool {
    crate::dma_pin::pinned(paddr, pages)
}

/// Park a DMA free until every overlapping userspace pin is gone, and say
/// whether it was parked. `false` means nothing holds these frames.
pub fn dma_hold_if_pinned(paddr: usize, pages: usize) -> bool {
    crate::dma_pin::hold_if_pinned(paddr, pages)
}

/// Mark or clear every physical frame backing the usable stack `[usable_base,
/// usable_base + STACK_SIZE)` in the stack-frame bitset, by querying the live
/// page table for each page's physical address.
fn mark_stack_frames(usable_base: usize, set: bool) {
    let pt = PageTable::from_current();
    let stack_size = executor::STACK_SIZE;
    for off in (0..stack_size).step_by(PAGE_SIZE) {
        let Ok((paddr, _, _)) = pt.query(usable_base + off) else {
            continue;
        };
        let frame = paddr / PAGE_SIZE;
        if set {
            STACK_FRAMES.mark(frame);
        } else {
            STACK_FRAMES.clear(frame);
        }
    }
}

/// Physmap-write guard: report (once) if `who` is about to write over a live
/// coroutine stack through the physmap, and name the writing call chain.
///
/// This is THE check for the leading root-cause theory. A hit means a physical
/// frame backing a VMO page (or a DMA buffer, or a `pmem_copy` destination)
/// physically aliases a live executor stack — the wild zero-writer, caught at
/// the instant of the write with its own backtrace. Latches the smash flag so
/// the timer path stops running on the (about-to-be) corrupted stack.
pub fn check_physmap_write(who: &str, paddr: usize, len: usize) {
    if !paddr_aliases_stack(paddr, len) {
        return;
    }
    ::executor::note_heap_smash_suspected();
    use core::sync::atomic::AtomicBool;
    static REPORTED: AtomicBool = AtomicBool::new(false);
    if REPORTED.swap(true, Ordering::SeqCst) {
        return;
    }
    crate::console::serial_write_fmt_spin(format_args!(
        "\n[physmap-smash] {} paddr={:#x} len={:#x} ALIASES A LIVE COROUTINE STACK — \
         this physmap write is the wild zero-writer (diag rev 5). Call chain:\n",
        who, paddr, len,
    ));
    // Frame-pointer backtrace of the writing path, bounded and guarded.
    #[cfg(target_arch = "x86_64")]
    {
        let mut rbp: usize;
        unsafe { core::arch::asm!("mov {}, rbp", out(reg) rbp) };
        for _ in 0..24 {
            if rbp == 0 || rbp & 0x7 != 0 || rbp < 0xffff_ff00_0000_0000 {
                break;
            }
            let ret = unsafe { core::ptr::read_volatile((rbp + 8) as *const usize) };
            let next = unsafe { core::ptr::read_volatile(rbp as *const usize) };
            if ret == 0 {
                break;
            }
            crate::console::serial_write_fmt_spin(format_args!(
                "[physmap-smash]   ret={:#x}\n",
                ret
            ));
            if next <= rbp {
                break;
            }
            rbp = next;
        }
    }
    crate::console::serial_write_str(
        "[physmap-smash] symbolize: llvm-addr2line -e <zcore.elf> -fCi <ret ...>\n",
    );
}

/// Whether any physical frame in `[paddr, paddr + len)` currently backs a live
/// coroutine stack. Called by the physmap write primitives (`pmem_zero`,
/// `pmem_write`) before they scribble — a `true` means the write would corrupt
/// a live executor stack through the physmap alias.
pub fn paddr_aliases_stack(paddr: usize, len: usize) -> bool {
    STACK_FRAMES.aliases(paddr, len)
}

/// Live guard bands. Two per executor (bottom and top); the scheduler's own
/// history shows a few dozen executors alive at once during a desktop session,
/// so this is sized well past that. Exhausting it is not fatal — [`install`]
/// refuses and that executor falls back to the soft canary.
const MAX_GUARDS: usize = 1024;

/// Registry of installed bands, with each band's original flags. Lock-free
/// and allocation-free rather than a `Mutex<Vec<_>>` because
/// [`is_guard_fault`] is called from the page-fault handler, where taking a
/// lock — or allocating — risks turning a diagnosable fault into a deadlock.
/// The table lives in `crate::fault_slots`, shared with the quarantine
/// registry below, which had a second copy of every line of it.
static GUARDS: SlotTable<MAX_GUARDS> = SlotTable::new();

/// The bottom band is told from the top one by its size, which only works
/// while the two differ. If they ever stopped differing, `install` would mark
/// the frames *above* the top guard as a live coroutine stack, and the next
/// legitimate physmap write to them would report a smash that did not happen —
/// which latches the flag that stops the timer path dispatching at all.
const _: () = assert!(executor::GUARD_SIZE != executor::TOP_GUARD_SIZE);

// ── Page-table frames reserved for splitting huge mappings ───────────────────
//
// On riscv64 and aarch64 the kernel heap is a window of the physmap, and the
// kernel page table covers the physmap with 2 MiB pages. A guard band inside
// one of those cannot be taken away a page at a time: the smallest thing the
// entry can describe is the whole 2 MiB. So `install` splits the covering
// entry into 4 KiB ones first — same physical range, same flags, finer
// granularity — which needs a frame for the new table.
//
// It cannot allocate one. `install` runs inside `Executor::new`, which the
// scheduler calls with the runtime lock held and interrupts off, and the
// coroutine stack it is guarding came out of the very heap a `frame_alloc`
// would lock. Hence this pool: frames taken at boot, from `init`, where
// allocating is ordinary, and never given back — each one becomes a live page
// table reachable from the kernel root for the rest of the boot.
//
// A split is permanent and shared by every address space, so the pool is only
// ever drawn on the FIRST time a guard band lands in a given 2 MiB region;
// every later stack in that region finds 4 KiB entries already there. The
// scheduler recycles its stacks through a pool of its own, so the set of
// regions that ever host one settles quickly. Running out is not a failure:
// `install` refuses that band and the scheduler keeps its soft canary, which
// is exactly what happened for every band before this existed.
const SPLIT_POOL_FRAMES: usize = 128;
/// The reserve itself lives in `crate::phys_watch`, where every build compiles
/// it.
static SPLIT_POOL: FramePool<SPLIT_POOL_FRAMES> = FramePool::new();

/// Serialises the split phase of [`install`].
///
/// Two CPUs creating an executor at the same time can hold guard bands in the
/// same 2 MiB region. Without this, both would see the huge entry, both would
/// build a table, and the second `set_table` would orphan the first table —
/// including any 4 KiB split the first CPU had already made inside it, leaving
/// that CPU convinced the band was 4 KiB-mapped while the live entry still
/// covered 2 MiB. Clearing the permissions of *that* entry would take 2 MiB of
/// live heap away instead of one guard band.
///
/// A raw spin lock rather than `crate::sync::Mutex`: this is held with
/// interrupts already off, over a few hundred stores and no allocation.
static SPLIT_LOCK: spin::Mutex<()> = spin::Mutex::new(());

/// Reserve the page-table frames [`ensure_4k`] will need. Called from `init`.
fn fill_split_pool() {
    for _ in 0..SPLIT_POOL_FRAMES {
        let Some(frame) = PhysFrame::new_zero() else {
            break;
        };
        if !SPLIT_POOL.push(frame.paddr()) {
            break;
        }
        // Leaked deliberately. Dropping a `PhysFrame` returns it to the frame
        // allocator, and these become live page tables: the allocator would
        // hand a table that the MMU is walking to the next VMO that asks for a
        // page.
        core::mem::forget(frame);
    }
}

/// Hand out one reserved frame, or `None` once the pool is spent.
fn split_pool_take() -> Option<PhysAddr> {
    SPLIT_POOL.take()
}

/// `(frames reserved, frames spent on splits, splits refused for want of one)`.
pub fn split_pool_stats() -> (usize, usize, usize) {
    SPLIT_POOL.stats()
}

/// Make every page of `[base, base + size)` an ordinary 4 KiB entry, splitting
/// the huge entries that cover it. Idempotent, and never rolled back: a split
/// leaves the mapping describing exactly the same memory, so a band that is
/// refused afterwards costs nothing but the frames.
fn ensure_4k(pt: &mut PageTable, base: usize, size: usize) -> Result<(), BandRefusal> {
    let _guard = SPLIT_LOCK.lock();
    let mut split_any = false;
    for off in (0..size).step_by(PAGE_SIZE) {
        let vaddr = base + off;
        match pt.query(vaddr) {
            Ok((_, _, crate::vm::PageSize::Size4K)) => continue,
            // Anything bigger: split it, and the rest of the pages it covered
            // come back as 4 KiB on their own turn through this loop.
            Ok(_) => {}
            Err(_) => return Err(BandRefusal::NotMapped),
        }
        if pt.split_huge_page(vaddr, split_pool_take).is_err() {
            return Err(BandRefusal::NoSplitFrames);
        }
        split_any = true;
    }
    if split_any {
        // Every CPU may hold a TLB entry for the huge page this band was part
        // of, and a kernel mapping is reachable from every address space, so
        // the shootdown deliberately targets all of them.
        crate::vm::flush_tlb(None);
        crate::common::ipi::remote_flush_tlb_aspace(None, None);
    }
    Ok(())
}

/// Register this module's hooks with the scheduler.
///
/// Must run after the kernel page tables are pinned and *before* the first
/// `Executor::new`, which is why `primary_init` calls it immediately before
/// `executor::warm_runtimes()`. Boot asserts that the hooks got registered.
pub fn init() {
    // Before the hooks, so the first `Executor::new` already finds the frames
    // its guard bands need (see `SPLIT_POOL`).
    fill_split_pool();
    // SAFETY: the contract is that `remove` restores any mapping `install`
    // took away before the VA goes back to the heap, and that no `.bss`-owned
    // frame is handed to the frame allocator. `install` never takes a frame out
    // of its page table entry, so the second obligation cannot be violated at
    // all, and `remove` restores from that same entry.
    unsafe { executor::set_stack_guard_hooks(install, remove) };
    // SAFETY: same contract — `quarantine_unprotect` restores the original
    // flags (kept in the registry) before the scheduler frees the VA, and no
    // frame ever leaves its page-table entry. Only used when STACKQUARANTINE=1
    // arms the scheduler side.
    unsafe { executor::set_stack_quarantine_hooks(quarantine_protect, quarantine_unprotect) };
}

/// `(bands installed, install requests refused)` since boot.
pub fn stats() -> (usize, usize) {
    GUARDS.stats()
}

/// Make `[guard_base, guard_base + guard_size)` fault on any access.
///
/// Returns `false` — having left the mapping exactly as it found it — if the
/// band is not a run of ordinary 4 KiB pages with uniform flags, if the
/// registry is full, or if the result does not verify. The scheduler then keeps
/// its soft canary.
fn install(guard_base: usize, guard_size: usize) -> bool {
    let refuse = |reason: BandRefusal| {
        let n = GUARDS.note_refused();
        // Logged only the first few times. This runs inside `Executor::new`,
        // which the scheduler calls with the runtime lock held and interrupts
        // off on every preemption-mid-poll — a per-executor log line from a
        // permanent condition (a full registry, say) would be a storm in
        // exactly the context that can least afford one. The scheduler prints
        // its own one-shot soft-canary fallback banner regardless, and
        // [`stats`] keeps the running count.
        if n < 4 {
            warn!(
                "stack_guard: refusing hard guard at {:#x}+{:#x} (band {})",
                guard_base,
                guard_size,
                reason.reason(Take::Everything)
            );
        }
        false
    };
    if let Err(reason) = guard_band::check_alignment(guard_base, guard_size) {
        return refuse(reason);
    }

    // The kernel half is shared by every address space (`pt_clone_kernel_space`
    // copies the top-level entries by value, so all of them walk into the same
    // sub-tables), which is why editing through whatever page table happens to
    // be loaded is enough — and necessary, since this can run under a user CR3
    // left behind by lazy TLB.
    let mut pt = PageTable::from_current();

    // Huge mappings first. On riscv64 and aarch64 the kernel heap is part of
    // the physmap, which the kernel page table covers with 2 MiB pages, so
    // every band landed here as "band is covered by a 1 GiB PTE" / "2 MiB PTE"
    // and every coroutine stack on those architectures ran with a soft canary
    // and no hard guard. Splitting the covering entries into 4 KiB ones
    // changes nothing about what is mapped where — see `SPLIT_POOL`.
    if let Err(reason) = ensure_4k(&mut pt, guard_base, guard_size) {
        return refuse(reason);
    }

    // Survey first, touch nothing: alignment, then every page an ordinary
    // 4 KiB mapping and all of them agreeing on their flags, so `remove` can
    // restore the band from a single recorded value.
    let expect = match guard_band::survey(&pt, guard_base, guard_size, Take::Everything) {
        Ok(flags) => flags,
        Err(reason) => return refuse(reason),
    };

    // Publish before editing: a fault inside the band from here on is a guard
    // hit and should be reported as one.
    let Some(slot) = GUARDS.claim(guard_base, guard_size, expect.bits()) else {
        return refuse(BandRefusal::RegistryFull);
    };

    let rollback = |pt: &mut PageTable| {
        // Best effort by construction: every entry still holds its own frame,
        // so restoring is one flag write per page and cannot fail for any
        // reason the survey above did not already rule out.
        let _ = guard_band::set_band_flags(pt, guard_base, guard_size, expect);
        crate::vm::flush_tlb(None);
        crate::common::ipi::remote_flush_tlb_aspace(None, None);
    };

    let taken = Take::Everything.applied_to(expect);
    if guard_band::set_band_flags(&mut pt, guard_base, guard_size, taken).is_err() {
        rollback(&mut pt);
        GUARDS.free(slot);
        return refuse(BandRefusal::EditFailed);
    }

    if !guard_band::took_effect(&pt, guard_base, guard_size, Take::Everything) {
        rollback(&mut pt);
        GUARDS.free(slot);
        return refuse(BandRefusal::NoEffect);
    }

    // One flush for the whole band. Other CPUs may hold TLB entries from the
    // heap's previous tenant at these addresses; without this the guard would
    // silently not exist on those cores. `aspace = None` deliberately targets
    // every CPU: a kernel mapping is reachable from every address space, so
    // filtering by page-table root would under-target.
    crate::common::ipi::remote_flush_tlb_aspace(None, None);
    // The bottom guard install is the one point that knows this executor's
    // full layout: record the usable stack's physical frames so a physmap
    // write that aliases them is caught. `guard_base` is `alloc_base`, so the
    // usable region starts one bottom-guard above it.
    if guard_size == executor::GUARD_SIZE {
        mark_stack_frames(guard_base + executor::GUARD_SIZE, true);
    }
    GUARDS.note_accepted();
    true
}

/// Put a band installed by [`install`] back the way it was.
///
/// Called from `Executor::drop`, immediately before the allocation goes back to
/// the heap — so failing to restore would hand out memory with a hole in it,
/// and the next owner would fault on an address nothing explains. That is worth
/// a loud panic rather than a silent return.
fn remove(guard_base: usize, guard_size: usize) {
    let Some(slot) = GUARDS.slot_of(guard_base) else {
        // Never installed (the scheduler only calls this for bands it recorded
        // as hard, so this means the registry and the scheduler disagree).
        return;
    };
    // Stop tracking this stack's physical frames before the memory is freed:
    // the frames are about to be legitimately reused, and a stale bit would
    // make the next VMO zeroing of them a false positive.
    if guard_size == executor::GUARD_SIZE {
        mark_stack_frames(guard_base + executor::GUARD_SIZE, false);
    }
    // A slot that went free between the lookup and here would hand back `None`
    // and leave the band unmapped; the registry is only ever released by this
    // function, for a band the scheduler is holding, so it cannot.
    let Some(bits) = GUARDS.flags(slot) else {
        return;
    };
    let flags = MMUFlags::from_bits_truncate(bits);
    let mut pt = PageTable::from_current();
    if guard_band::set_band_flags(&mut pt, guard_base, guard_size, flags).is_err() {
        panic!(
            "stack_guard: could not restore guard band {:#x}+{:#x} — refusing to \
             return unmapped memory to the heap",
            guard_base, guard_size
        );
    }
    // Other CPUs may have cached the not-present entry; make them re-walk.
    crate::vm::flush_tlb(None);
    crate::common::ipi::remote_flush_tlb_aspace(None, None);
    GUARDS.free(slot);
}

/// Whether `fault_vaddr` fell inside an installed guard band.
///
/// The page-fault handler asks this before trying to resolve the fault against
/// the faulting thread's VMAR: a guard hit is a kernel stack overflow, and no
/// amount of VMAR work will explain it. Lock-free and allocation-free — it runs
/// on the fault path, where blocking would turn a reportable overflow into a
/// hang.
pub fn is_guard_fault(fault_vaddr: usize) -> bool {
    GUARDS.slot_of(fault_vaddr).is_some()
}

// ── Freed-stack quarantine: catch the use-after-free WRITER red-handed ────────
//
// The recurring coroutine-stack smash (rev 7 run: a transient executor's return
// slot zeroed, then a `ret` into `rip=0x0`) is a heap use-after-free that no
// dispatch gate can catch — it is a raw write, not a `dyn` call. The only way to
// name the culprit is to catch the write itself.
//
// So when a transient executor's stack is freed, the scheduler does not return
// it to the heap immediately: it hands the usable region here to be
// **write-protected** (present + readable, but not writable) and held in a
// bounded ring. A dangling pointer that still writes into the freed stack then
// takes a clean WRITE #PF *at the writer's own rip*, which `zcore::handler`
// reports as `[stack-uaf]` with the writing call chain — the exact instruction
// doing the UAF, instead of the damage discovered later on the victim's stack.
//
// Write-protect (not unmap) on purpose: a benign stale *read* of freed memory
// is not the corruptor and should not fault, but every stale *write* — the zero
// writer — does. Same PTE machinery, TLB flush and lock-free registry as the
// guards; a separate table so a hit is reported as a UAF, not an overflow.

/// Same real ceiling as the guard registry — a handful of executors churn at
/// once, and the scheduler's ring holds only the most-recently-freed stacks.
const MAX_QUAR: usize = 128;
/// The same registry as the guards, a second instance of it — not a second
/// copy of the code, which is what this was.
static QUARANTINE: SlotTable<MAX_QUAR> = SlotTable::new();

/// Write-protect the freed usable stack `[usable_base, usable_base + size)` and
/// register it, so a stale write into it faults at the writer.
///
/// Returns `false` — having touched nothing — if the region is not a uniform run
/// of writable 4 KiB pages or the registry is full; the scheduler then frees the
/// stack the ordinary way. Same all-or-nothing contract as [`install`].
pub fn quarantine_protect(usable_base: usize, size: usize) -> bool {
    let refuse = |reason: BandRefusal| {
        let n = QUARANTINE.note_refused();
        // Same rate limit, and for the same reason, as `install`'s: this runs
        // on every executor teardown and a permanent condition would otherwise
        // print at that rate. Counted always, so `quarantine_stats` can tell
        // "armed and never hit" from "never armed".
        if n < 4 {
            warn!(
                "stack_guard: refusing to quarantine {:#x}+{:#x} (region {})",
                usable_base,
                size,
                reason.reason(Take::WriteOnly)
            );
        }
        false
    };
    if let Err(reason) = guard_band::check_alignment(usable_base, size) {
        return refuse(reason);
    }
    let mut pt = PageTable::from_current();
    // Split the covering huge entries first, exactly as `install` does. Without
    // this the quarantine was silently unavailable on riscv64 and aarch64,
    // where the kernel heap is a window of a physmap mapped with 2 MiB pages:
    // every region arrived covered by a huge PTE, every call returned `false`,
    // and the only outward sign was that the use-after-free writer was never
    // caught on those architectures.
    if let Err(reason) = ensure_4k(&mut pt, usable_base, size) {
        return refuse(reason);
    }
    let expect = match guard_band::survey(&pt, usable_base, size, Take::WriteOnly) {
        Ok(flags) => flags,
        Err(reason) => return refuse(reason),
    };
    let Some(slot) = QUARANTINE.claim(usable_base, size, expect.bits()) else {
        return refuse(BandRefusal::RegistryFull);
    };
    let rollback = |pt: &mut PageTable| {
        let _ = guard_band::set_band_flags(pt, usable_base, size, expect);
        crate::vm::flush_tlb(None);
        crate::common::ipi::remote_flush_tlb_aspace(None, None);
    };
    // Present + readable, minus WRITE: a stale read stays silent, a stale write
    // faults with the WRITE error bit set.
    let readonly = Take::WriteOnly.applied_to(expect);
    if guard_band::set_band_flags(&mut pt, usable_base, size, readonly).is_err() {
        rollback(&mut pt);
        QUARANTINE.free(slot);
        return refuse(BandRefusal::EditFailed);
    }
    if !guard_band::took_effect(&pt, usable_base, size, Take::WriteOnly) {
        rollback(&mut pt);
        QUARANTINE.free(slot);
        return refuse(BandRefusal::NoEffect);
    }
    crate::vm::flush_tlb(None);
    crate::common::ipi::remote_flush_tlb_aspace(None, None);
    QUARANTINE.note_accepted();
    true
}

/// Undo [`quarantine_protect`] on `[usable_base, usable_base + size)`, restoring
/// the original flags just before the scheduler finally frees the stack. Loud
/// panic on failure: returning write-protected memory to the heap would fault
/// the next owner on an address nothing explains.
pub fn quarantine_unprotect(usable_base: usize, size: usize) {
    let Some(slot) = QUARANTINE.slot_of(usable_base) else {
        return;
    };
    let Some(bits) = QUARANTINE.flags(slot) else {
        return;
    };
    let flags = MMUFlags::from_bits_truncate(bits);
    let mut pt = PageTable::from_current();
    if guard_band::set_band_flags(&mut pt, usable_base, size, flags).is_err() {
        panic!(
            "stack_guard: could not un-protect quarantined stack {:#x}+{:#x} — \
             refusing to return write-protected memory to the heap",
            usable_base, size
        );
    }
    crate::vm::flush_tlb(None);
    crate::common::ipi::remote_flush_tlb_aspace(None, None);
    QUARANTINE.free(slot);
}

/// Whether `fault_vaddr` fell inside a write-protected quarantined stack — i.e.
/// a use-after-free write into freed coroutine-stack memory. Lock-free and
/// allocation-free; safe on the page-fault path.
pub fn is_quarantine_fault(fault_vaddr: usize) -> bool {
    QUARANTINE.slot_of(fault_vaddr).is_some()
}

/// `(stacks protected right now, protected ever, refused)` for the boot log.
///
/// Three numbers because the one this used to return could not tell the two
/// interesting failures apart: a quarantine that is armed and has simply not
/// caught anything reads the same as one that has never once been able to arm.
pub fn quarantine_stats() -> (usize, usize, usize) {
    let (ever, refused) = QUARANTINE.stats();
    (QUARANTINE.live(), ever, refused)
}
