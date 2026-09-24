use crate::context::{Context as ExecuterContext, ContextData};
use alloc::alloc::{Allocator, Global, Layout};
use core::pin::Pin;
use {
    alloc::boxed::Box,
    alloc::sync::Arc,
    core::ptr::NonNull,
    core::task::{Context, Poll},
};

use crate::arch::executor_entry;
use crate::task_collection::{Task, TaskCollection};
use crate::waker_page::WakerRef;
use core::sync::atomic::AtomicBool;

#[derive(Debug, PartialEq, Eq)]
enum ExecutorState {
    STRONG,
    WEAK, // 执行完一次future后就需要被drop
    KILLED,
    UNUSED,
}

pub struct Executor {
    id: usize,
    task_collection: Arc<TaskCollection>,
    stack_base: usize,
    /// Bottom soft-guard region unmapped via [`set_stack_guard_hooks`].
    hard_guard_bottom: bool,
    /// Top guard (above usable stack) unmapped — catches a neighbour growing down.
    hard_guard_top: bool,
    pub context: ExecuterContext,
    #[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
    context_data: ContextData,
    task_id: usize,
    state: ExecutorState,
    /// The task checked out for the poll currently in flight, with its waker.
    /// The panic-containment path needs to name — and retire — the future that
    /// was running when a fault hit this executor's stack, and `task_id` alone
    /// cannot do that.
    ///
    /// Raw pointers rather than `Arc` clones on purpose: this is the hot poll
    /// path, where the surrounding code goes out of its way to avoid refcount
    /// round-trips (see `Task::waker`'s doc). Both are set immediately before
    /// `Task::poll` and cleared immediately after, and the `Arc`s they borrow
    /// from are live locals of `run` across that whole window — including while
    /// the poll is parked by preemption. The only reader is
    /// [`Executor::abandon_current_task`], reached from a fault taken *inside*
    /// that poll on this same CPU, which is exactly when they are valid.
    current_task: *const Task,
    current_waker: *const WakerRef,
    /// Set when this executor's coroutine stack was abandoned mid-poll (see
    /// [`Executor::abandon_current_task`]). Separate from `state` because it is
    /// written through a shared `&Executor` while the runtime still holds its
    /// `Arc`, and because it must survive the `WEAK` transition that
    /// `downgrade_strong_executor` performs afterwards.
    abandoned: AtomicBool,
    /// Forces the runtime to REPLACE this executor instead of resuming it.
    ///
    /// Set by [`abandon_idle_executor`](Self::abandon_idle_executor): a fault
    /// taken between polls has no task to blame, so `task_id` is already 0 and
    /// `is_running_future()` would say "nothing in flight, just resume it" —
    /// straight back onto the faulting instruction on a corrupt stack. This
    /// flag makes the runtime take the replace path instead.
    force_replace: AtomicBool,
    /// [null-exec guard] Resume-ownership latch: 0 = parked/idle, `cpu+1` =
    /// the CPU currently standing on this executor's stack. The runtime CAS-es
    /// it before every `switch` INTO the executor and releases it only after
    /// control is back on the runtime stack (the executor's frame saved and
    /// parked). A second resumer — the double-consume that pops a dead frame's
    /// zeros as `ret`/`cr3` — fails the CAS and is reported instead of run.
    resume_owner: core::sync::atomic::AtomicUsize,
}

/// Idle-loop iterations since any task was last polled (hang detector; see the
/// idle branch in `run`). Global because all executors on a CPU share progress.
static IDLE_STREAK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

// Scheduler-loop branch counters, surfaced via [`sched_stats`] for
// `/proc/perf/kernel` so a busy-spin can be attributed: `polled` = a task was
// available to run, `weak_yield` = no task but a weak executor outstanding so we
// spun via `sched_yield` instead of halting.
static SCHED_POLLED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static SCHED_WEAK_YIELD: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// `(tasks polled, weak-executor yields)` since boot.
pub fn sched_stats() -> (u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    (SCHED_POLLED.load(Relaxed), SCHED_WEAK_YIELD.load(Relaxed))
}

const PAGE_SIZE: usize = 4096;
/// Soft / hard guard **below** the usable stack. Soft path fills canary words;
/// hard path (when hooks are registered) unmaps these pages so overflow #PFs
/// instead of smashing neighbouring heap (`rip=0x3` / `[rsp0]=0x13486`).
///
/// 512 KiB (was 128 KiB / 16 KiB). Even with `"stack-probes": {"kind": "inline"}`
/// in `zCore/x86_64.json`, keep a wide bottom hole so a missed probe / IRQ nest
/// still #PFs before neighbour heap smash. Proven: hard_guard_executors=64 still
/// soft-smashed with 128 KiB (hole jumped or neighbour smash) before probes
/// were enabled on the custom target.
pub const GUARD_SIZE: usize = PAGE_SIZE * 128; // 512 KiB
/// Unmapped region **above** the usable stack. A neighbour stack growing down
/// hits this before smashing this stack's high return-address slots.
pub const TOP_GUARD_SIZE: usize = PAGE_SIZE * 16; // 64 KiB
/// 2 MiB usable coroutine stack. History: 256 KiB and 512 KiB overflowed into
/// neighbouring heap (`rip=0x0` / `[rsp0]=0x13446`); 1 MiB still saw near-
/// overflow at labwc/lunarbar desktop start (timer smash → `rip=0x3`). Extra
/// headroom is cheap relative to a guard-page-less smash.
///
/// Per-executor footprint: `STACK_SIZE + GUARD_SIZE + TOP_GUARD_SIZE`
/// (= 2 MiB + 512 KiB + 64 KiB). 64 executors ≈ 160 MiB — fine in a 512 MiB heap.
pub const STACK_SIZE: usize = 4096 * 512;
const ALLOC_SIZE: usize = STACK_SIZE + GUARD_SIZE + TOP_GUARD_SIZE;
// Page-aligned so the hard-guard path can `unmap` 4K PTEs in the BSS heap.
const ALLOC_LAYOUT: Layout = match Layout::from_size_align(ALLOC_SIZE, PAGE_SIZE) {
    Ok(l) => l,
    Err(_) => unreachable!(),
};

/// Magic written across the soft-guard region below every coroutine stack.
/// Layout: `[BOTTOM_GUARD GUARD_SIZE][usable STACK_SIZE][TOP_GUARD TOP_GUARD_SIZE]`.
/// Usable grows down from `stack_base + STACK_SIZE` toward `stack_base`; bottom
/// guard is `[stack_base - GUARD_SIZE, stack_base)`; top guard is
/// `[stack_base + STACK_SIZE, stack_base + STACK_SIZE + TOP_GUARD_SIZE)`.
const STACK_CANARY: u64 = 0x5354_4143_4b5f_4f56; // "STACK_OV"
const GUARD_WORDS: usize = GUARD_SIZE / core::mem::size_of::<u64>();

// ── Live coroutine-stack registry (double-alloc tripwire) ────────────────────
//
// Every crash capture zeroes a run of a *transient* executor's stack (exec
// ids 2,3,4 — the downgraded ones whose stacks are freed and reused — never
// the immortal 0/1), and the fill is zeros, not the `0xDEADBEEF` poison
// `Executor::new` writes. That is the signature of a plain zero-initialised
// heap buffer (`vec![0; n]`, a zeroed Box) handed out by the buddy allocator
// over memory that is STILL a live coroutine stack — a heap double-allocation
// / use-after-free of a stack.
//
// This registry records every live stack allocation; the kernel's global
// allocator calls [`alloc_overlaps_live_stack`] on each block it hands out, so
// the double-alloc is caught at the moment it is dispensed, with the
// allocating call chain (the writer's own path) on the stack. Lock-free by
// construction (atomics only): it is consulted from inside the allocator lock.
const STACK_REG_SLOTS: usize = 512;
static STACK_REG_BASE: [core::sync::atomic::AtomicUsize; STACK_REG_SLOTS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; STACK_REG_SLOTS];

/// Stacks [`stack_reg_insert`] could not record because the table was full.
///
/// Monotonic on purpose: a dropped insert may later be balanced by that stack
/// being freed, but nothing here can tell, so the count never goes down. It
/// therefore only ever over-reports incompleteness — the safe direction for a
/// check whose whole value is knowing when a clean result means nothing.
static STACK_REG_BASE_OVERFLOW: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// Record `[alloc_base, alloc_base + ALLOC_SIZE)` as a live stack. `base == 0`
/// slots are free. Counts, rather than silently drops, an insert that does not
/// fit: an unrecorded stack makes [`alloc_overlaps_live_stack`] return false
/// negatives, and a caller that cannot see that would read "no overlap" as
/// "no aliasing".
fn stack_reg_insert(alloc_base: usize) {
    use core::sync::atomic::Ordering::AcqRel;
    for slot in STACK_REG_BASE.iter() {
        if slot
            .compare_exchange(0, alloc_base, AcqRel, core::sync::atomic::Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
    }
    STACK_REG_BASE_OVERFLOW.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// [diag] Live stacks missing from the hand-out registry; non-zero means
/// [`alloc_overlaps_live_stack`] can return false negatives.
pub fn untracked_alloc_stacks() -> usize {
    STACK_REG_BASE_OVERFLOW.load(core::sync::atomic::Ordering::Relaxed)
}

/// Remove a stack recorded by [`stack_reg_insert`].
fn stack_reg_remove(alloc_base: usize) {
    use core::sync::atomic::Ordering::AcqRel;
    for slot in STACK_REG_BASE.iter() {
        if slot
            .compare_exchange(alloc_base, 0, AcqRel, core::sync::atomic::Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
    }
}

/// Whether `[ptr, ptr + len)` overlaps any live coroutine-stack allocation.
///
/// Returns the overlapping `alloc_base` if so. The global allocator calls this
/// on every block it dispenses: a hit means the buddy handed out memory that is
/// still a live executor stack — the double-alloc these crashes are chasing.
///
/// There is no exclusion for `Executor::new` allocating its own stack, and
/// none is needed: that allocation is what *calls* this, and the two registry
/// inserts happen on the line after it returns. Both this comment and the
/// caller's used to promise one ("excludes an exact `alloc_base` match with
/// `len >= ALLOC_SIZE`"), which the body has never implemented — a stated
/// guarantee in front of a `panic!` in the global allocator, describing an
/// exemption nothing has ever needed.
///
/// An end that overflows the address space saturates rather than wrapping:
/// nonsense input must not come back as "no overlap" from a check whose whole
/// job is to refuse.
pub fn alloc_overlaps_live_stack(ptr: usize, len: usize) -> Option<usize> {
    use core::sync::atomic::Ordering::Acquire;
    let a_end = ptr.saturating_add(len);
    for slot in STACK_REG_BASE.iter() {
        let base = slot.load(Acquire);
        if base == 0 {
            continue;
        }
        let b_end = base.saturating_add(ALLOC_SIZE);
        if ptr < b_end && base < a_end {
            return Some(base);
        }
    }
    None
}

/// Optional hard-guard install/remove. Registered by kernel-hal once page
/// tables are ready. `install` returns false to keep the soft canary.
type StackGuardHook = fn(guard_base: usize, guard_size: usize) -> bool;
type StackGuardRemove = fn(guard_base: usize, guard_size: usize);

static STACK_GUARD_INSTALL: spin::Mutex<Option<StackGuardHook>> = spin::Mutex::new(None);
static STACK_GUARD_REMOVE: spin::Mutex<Option<StackGuardRemove>> = spin::Mutex::new(None);

/// Executors created with an unmapped (hard) guard vs soft canary.
static HARD_GUARD_EXECUTORS: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
static SOFT_GUARD_EXECUTORS: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// `(hard_guard count, soft_guard count)` of executors created this boot.
pub fn hard_guard_executor_counts() -> (usize, usize) {
    use core::sync::atomic::Ordering::Relaxed;
    (
        HARD_GUARD_EXECUTORS.load(Relaxed),
        SOFT_GUARD_EXECUTORS.load(Relaxed),
    )
}

/// Register unmapped-guard hooks. Safe to call once at boot; later calls replace.
///
/// # Safety
/// Hooks must remap frames on remove before the VA is returned to the heap,
/// and must not free BSS-owned physical frames to the frame allocator.
pub unsafe fn set_stack_guard_hooks(install: StackGuardHook, remove: StackGuardRemove) {
    *STACK_GUARD_INSTALL.lock() = Some(install);
    *STACK_GUARD_REMOVE.lock() = Some(remove);
}

/// True once [`set_stack_guard_hooks`] has registered install/remove.
pub fn stack_guard_hooks_registered() -> bool {
    STACK_GUARD_INSTALL.lock().is_some()
}

// ── Freed-stack quarantine (diagnostic; armed by STACKQUARANTINE=1) ───────────
//
// Instead of returning a freed coroutine stack straight to the heap, hold it
// write-protected in a bounded ring so a dangling pointer that writes into it
// faults at the writer's rip (see `kernel_hal::stack_guard`'s quarantine). This
// pins the use-after-free that smashes transient-executor stacks. Off by default
// because it holds `QUAR_RING` allocations back and adds a TLB shootdown per
// free — both fine for a diagnostic run, not for normal operation.

/// Protect a freed usable stack; returns false if it could not (caller frees
/// normally). Unprotect restores it just before the real free.
type StackQuarProtect = fn(usable_base: usize, size: usize) -> bool;
type StackQuarUnprotect = fn(usable_base: usize, size: usize);

static STACK_QUAR_PROTECT: spin::Mutex<Option<StackQuarProtect>> = spin::Mutex::new(None);
static STACK_QUAR_UNPROTECT: spin::Mutex<Option<StackQuarUnprotect>> = spin::Mutex::new(None);

static QUARANTINE_ENABLED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// How many freed stacks to hold write-protected at once. Each is
/// `ALLOC_SIZE` (~2.6 MiB); the ring bounds the memory held out of the heap.
const QUAR_RING: usize = 24;
static QUAR_RING_SLOTS: [core::sync::atomic::AtomicUsize; QUAR_RING] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; QUAR_RING];
static QUAR_RING_IDX: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Freed stacks awaiting an SMP grace period before they may be reused or
/// returned to the heap.
///
/// A stack stops being "owned" by its executor before every CPU has
/// necessarily passed through a scheduler point that proves it is no longer
/// still executing, or about to resume, on that saved frame. Returning the
/// block to the pool/heap in that window is the exact `[double-alloc]` /
/// `[null-exec]` corruption: the next zero-init consumer blanks a frame that
/// was merely late, not dead. Keep the block off the allocator until every
/// online CPU has reached the runtime stack once after retirement; only then is
/// it quiescent enough to recycle.
const RETIRED_STACKS_CAP: usize = 256;

#[derive(Copy, Clone)]
struct RetiredStack {
    alloc_base: usize,
    hard_guard_bottom: bool,
    hard_guard_top: bool,
    protected: bool,
    pending_cpus: u64,
}

impl RetiredStack {
    const EMPTY: Self = Self {
        alloc_base: 0,
        hard_guard_bottom: false,
        hard_guard_top: false,
        protected: false,
        pending_cpus: 0,
    };
}

static RETIRED_STACKS: spin::Mutex<[RetiredStack; RETIRED_STACKS_CAP]> =
    spin::Mutex::new([RetiredStack::EMPTY; RETIRED_STACKS_CAP]);

/// Register the quarantine protect/unprotect hooks (kernel-hal, bare-metal).
///
/// # Safety
/// `unprotect` must restore the exact flags `protect` cleared before the VA is
/// freed, and neither may hand a `.bss` frame to the frame allocator.
pub unsafe fn set_stack_quarantine_hooks(protect: StackQuarProtect, unprotect: StackQuarUnprotect) {
    *STACK_QUAR_PROTECT.lock() = Some(protect);
    *STACK_QUAR_UNPROTECT.lock() = Some(unprotect);
}

/// Arm/disarm the freed-stack quarantine (STACKQUARANTINE=1 at boot).
pub fn set_stack_quarantine_enabled(on: bool) {
    QUARANTINE_ENABLED.store(on, core::sync::atomic::Ordering::SeqCst);
}

/// Push a just-freed `alloc_base` into the ring; returns the `alloc_base` it
/// evicted (`0` if the slot was empty).
///
/// Eviction no longer implies "safe to unprotect + free": a stack that has aged
/// out of the diagnostic ring may still be the one a stale parked frame resumes
/// on under SMP. Bare-metal therefore treats the evicted block as permanently
/// retired instead of returning it to the shared buddy arena.
fn quar_ring_push(alloc_base: usize) -> usize {
    use core::sync::atomic::Ordering;
    let idx = QUAR_RING_IDX.fetch_add(1, Ordering::Relaxed) % QUAR_RING;
    QUAR_RING_SLOTS[idx].swap(alloc_base, Ordering::AcqRel)
}

fn reclaim_retired_stack(slot: &mut RetiredStack) {
    let retired = *slot;
    if retired.alloc_base == 0 {
        return;
    }
    if retired.protected {
        if let Some(unprotect) = *STACK_QUAR_UNPROTECT.lock() {
            unprotect(retired.alloc_base + GUARD_SIZE, STACK_SIZE);
        }
    }
    // Never return a coroutine stack block to the shared buddy arena (`Global`):
    // it also backs userspace frames, and a retirement that turned out to be one
    // grace period early — a parked frame a frozen SMP sibling still resumes
    // onto — would let a stale kernel write corrupt whatever userspace page
    // reused the block, a silent fault the live-stack registry cannot catch (the
    // block left the registry at `Drop`). Keep every hard-guarded block in
    // coroutine-stack land: the fixed pool first, then the overflow retention
    // list, both with guards installed so reuse is a cheap re-poison.
    if retired.hard_guard_bottom && retired.hard_guard_top {
        if !stack_pool_push(retired.alloc_base) {
            STACK_OVERFLOW.lock().push(retired.alloc_base);
        }
        *slot = RetiredStack::EMPTY;
        return;
    }
    // A soft-guarded block cannot be pooled — `Executor::new`'s reuse path
    // assumes hard guards are already installed — and returning it to `Global` is
    // the exact aliasing above, so leak it instead. This only happens before the
    // guard hooks are registered (early boot) or when a huge PTE refused the
    // unmap, never in the steady-state desktop workload, so the leak is bounded
    // in practice.
    static SOFT_LEAKED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    let n = SOFT_LEAKED.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
    warn!(
        "[stack-retire] leaking soft-guarded retired stack {:#x} rather than returning it to the \
         shared arena (leaked {} so far)",
        retired.alloc_base, n,
    );
    *slot = RetiredStack::EMPTY;
}

fn observe_cpu_quiescent_locked(retired: &mut [RetiredStack; RETIRED_STACKS_CAP], cpu: usize) {
    if cpu >= 64 {
        return;
    }
    let bit = 1u64 << cpu;
    for slot in retired.iter_mut() {
        if slot.alloc_base == 0 || slot.pending_cpus & bit == 0 {
            continue;
        }
        slot.pending_cpus &= !bit;
        if slot.pending_cpus == 0 {
            reclaim_retired_stack(slot);
        }
    }
}

pub(crate) fn note_cpu_quiescent(cpu: usize) {
    let mut retired = RETIRED_STACKS.lock();
    observe_cpu_quiescent_locked(&mut retired, cpu);
}

fn retire_stack_after_grace(
    alloc_base: usize,
    hard_guard_bottom: bool,
    hard_guard_top: bool,
    protected: bool,
) {
    use core::sync::atomic::{AtomicUsize, Ordering};
    static RETIRED_OVERFLOW: AtomicUsize = AtomicUsize::new(0);

    let cpu = crate::arch::cpu_id() as usize;
    let mut pending = crate::runtime::executor_ready_mask();
    if cpu < 64 {
        pending &= !(1u64 << cpu);
    }

    let mut retired = RETIRED_STACKS.lock();
    observe_cpu_quiescent_locked(&mut retired, cpu);

    if pending == 0 {
        let mut slot = RetiredStack {
            alloc_base,
            hard_guard_bottom,
            hard_guard_top,
            protected,
            pending_cpus: 0,
        };
        reclaim_retired_stack(&mut slot);
        return;
    }

    for slot in retired.iter_mut() {
        if slot.alloc_base == 0 {
            *slot = RetiredStack {
                alloc_base,
                hard_guard_bottom,
                hard_guard_top,
                protected,
                pending_cpus: pending,
            };
            return;
        }
    }

    let n = RETIRED_OVERFLOW.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 16 {
        error!(
            "[stack-retire] retired-stack table full while parking {:#x} (report {}/16) — \
             leaking the block instead of returning it to the allocator before quiescence",
            alloc_base, n
        );
    }
}

fn executor_alloc_id() -> usize {
    use core::sync::atomic::{AtomicUsize, Ordering};
    static EXECUTOR_ID: AtomicUsize = AtomicUsize::new(1);
    EXECUTOR_ID.fetch_add(1, Ordering::SeqCst)
}

fn note_first_executor_created(_hard_guard_bottom: bool, _hard_guard_top: bool) {
    use core::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if LOGGED.swap(true, Ordering::SeqCst) {
        return;
    }
}

fn note_soft_guard_fallback(reason: &'static str) {
    use core::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if LOGGED.swap(true, Ordering::SeqCst) {
        return;
    }
    // Soft canary still detects overflow, but later than an unmapped guard —
    // desktop bring-up previously reached rip=0 / [rsp0]=0x13446 first.
    error!(
        "stack_guard: HARD GUARD UNAVAILABLE ({}) — falling back to soft canary; \
         overflow may smash neighbouring heap before the next IRQ tripwire \
         (rip=0 / [rsp0]=0x13446 class)",
        reason
    );
}

/// [diag] Lock-free registry of live coroutine-stack allocations.
///
/// Kernel coroutine stacks and userspace VMO frames come out of the SAME
/// buddy arena (`zCore::memory`: `frame_alloc` and the `GlobalAlloc` impl both
/// call `HEAP.0.lock().allocate`). If that allocator ever hands the same block
/// to both, the consequences match the crashes in
/// `docs/README-crash-repro.md` exactly: a freshly created VMO is zero-filled,
/// which would blank a live kernel stack (`rip=0x0`, `[rsp0..3]=0`,
/// `region=usable` — an overflow would have faulted in the unmapped guard
/// instead), and userspace then writing its buffer sprays arbitrary bytes over
/// kernel stacks and heap objects (the mangled return addresses and the
/// clobbered `KObjectBase` name `String`).
///
/// Walking `GLOBAL_RUNTIME` to test that costs a `try_lock` per CPU, far too
/// much for an allocator hot path — and heavy enough to shift the timing that
/// reproduces the bug. This registry is plain atomics: registering is one
/// store, and a check is a handful of relaxed loads.
const MAX_TRACKED_STACKS: usize = 512;
static STACK_REG: [core::sync::atomic::AtomicUsize; MAX_TRACKED_STACKS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; MAX_TRACKED_STACKS];
/// Live stacks that did not fit `STACK_REG` (the check is then incomplete;
/// reported so a silent gap is never mistaken for a clean result).
static STACK_REG_OVERFLOW: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// Publish `[alloc_base, alloc_base + ALLOC_SIZE)` as a live stack.
fn register_stack(alloc_base: usize) {
    for slot in STACK_REG.iter() {
        if slot
            .compare_exchange(
                0,
                alloc_base,
                core::sync::atomic::Ordering::AcqRel,
                core::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
        {
            return;
        }
    }
    STACK_REG_OVERFLOW.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Retract a stack registered by [`register_stack`].
fn unregister_stack(alloc_base: usize) {
    for slot in STACK_REG.iter() {
        if slot
            .compare_exchange(
                alloc_base,
                0,
                core::sync::atomic::Ordering::AcqRel,
                core::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
        {
            return;
        }
    }
}

/// Publish a stack in BOTH registries, as one operation.
///
/// They hold the same set and exist for the same check, but they are read by
/// different callers: `STACK_REG` (via [`overlapping_live_stack`]) by the
/// frame allocator, `STACK_REG_BASE` (via [`alloc_overlaps_live_stack`]) by
/// the `GlobalAlloc` hook — and coroutine stacks are allocated through that
/// second one. The two inserts used to sit at opposite ends of
/// `Executor::new`, two guard installs (page-table splits with their TLB
/// shootdowns) and a 2.6 MiB poison loop apart, so for the length of that
/// window a stack was refused to the frame allocator and waved past the hook
/// that actually hands stacks out. One function, one call site, no window.
fn publish_live_stack(alloc_base: usize) {
    register_stack(alloc_base);
    stack_reg_insert(alloc_base);
}

/// Retract a stack from both registries. The twin of [`publish_live_stack`];
/// a stack left in either one is a permanent false positive, and a false
/// positive here is a `panic!` inside the global allocator.
fn retract_live_stack(alloc_base: usize) {
    stack_reg_remove(alloc_base);
    unregister_stack(alloc_base);
}

/// [diag] Does `[start, start + len)` overlap a live coroutine-stack
/// allocation (guards included)? Returns the offending stack's alloc base.
///
/// Intended for the allocator to call on every block it is about to hand out:
/// an overlap means the block is already spoken for, and reporting it *at
/// hand-out* names the aliasing before any corruption happens — unlike a
/// canary or a watchpoint, which can only report damage after the fact.
/// Saturates on an overflowing end for the same reason as its twin: `?` on a
/// `checked_add` returned `None`, which this function spells "no overlap" —
/// the one answer a range it cannot even measure must not give.
pub fn overlapping_live_stack(start: usize, len: usize) -> Option<usize> {
    let end = start.saturating_add(len);
    for slot in STACK_REG.iter() {
        let base = slot.load(core::sync::atomic::Ordering::Acquire);
        if base != 0 && start < base.saturating_add(ALLOC_SIZE) && base < end {
            return Some(base);
        }
    }
    None
}

/// [diag] Live stacks that could not be tracked; non-zero means
/// [`overlapping_live_stack`] can return false negatives.
pub fn untracked_live_stacks() -> usize {
    STACK_REG_OVERFLOW.load(core::sync::atomic::Ordering::Relaxed)
}

// ── Spine-slot registry (null-exec hunt, generation 2) ───────────────────────
//
// Every surviving `[null-exec]` capture is the SAME instruction-level event:
// a `ret` at `stack_top - 0x508` pops zero. Static frame math pins that slot:
// `executor_entry` leaves rsp at `top - 0x8`; `run_executor`'s prologue
// (6 pushes + `sub 0x4C8`) leaves rsp at `top - 0x500`; its `call p.run()`
// therefore pushes the return-into-`run_executor` qword at `top - 0x508`.
// That qword is WRITE-ONCE for the executor's whole life: `Executor::run`
// never returns for a strong executor, no legitimate call chain re-pushes at
// that depth while `run` is live, and IRQ frames only grow downward from the
// interrupted rsp (always below). Yet in six captures it reads 0 with a
// ~0x400-byte all-zero blob above it — a foreign write.
//
// So: publish each live executor's spine slot (address + expected value)
// here. Two consumers:
//  * `kernel-hal`'s watchpoint module arms DR0-DR3 with the first four slots
//    on every CPU — the corruptor's store traps with ITS rip in the frame.
//  * The timer tick calls [`spine_verify`], a ≤16-load sweep that reports the
//    smash (and the zero blob's exact extent) within one tick of the write,
//    even if the debug registers miss it.
//
// Registration brackets exactly the write-once window: register at
// `Executor::run` entry (the slot was just written by the `call`), unregister
// at `run`'s only `return` (before `run_executor`'s post-run calls
// legitimately re-push there) and in `Drop` (covers the abandon path, where
// `run` never returns). A pool re-poison can only touch an UNREGISTERED slot,
// so any hit on a registered slot is the corruptor.

/// Registry capacity. Live executors ≈ CPUs + parked weaks; 16 is generous.
pub const SPINE_SLOTS: usize = 16;
static SPINE_ADDR: [core::sync::atomic::AtomicUsize; SPINE_SLOTS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; SPINE_SLOTS];
static SPINE_VAL: [core::sync::atomic::AtomicU64; SPINE_SLOTS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; SPINE_SLOTS];
static SPINE_EXEC: [core::sync::atomic::AtomicUsize; SPINE_SLOTS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; SPINE_SLOTS];
static SPINE_BASE: [core::sync::atomic::AtomicUsize; SPINE_SLOTS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; SPINE_SLOTS];
/// Bumped on every register/unregister: watchpoint re-sync + sweep seqlock.
static SPINE_GEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// One full smash report is enough; later mismatches only bump this.
static SPINE_SMASHES: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

fn spine_register(slot: usize, val: u64, exec_id: usize, stack_base: usize) {
    use core::sync::atomic::Ordering::{AcqRel, Relaxed, Release};
    for i in 0..SPINE_SLOTS {
        if SPINE_ADDR[i]
            .compare_exchange(0, usize::MAX, AcqRel, Relaxed)
            .is_ok()
        {
            // Payload first, then the real address (Release) so a concurrent
            // sweep never pairs the new address with a stale expected value.
            SPINE_VAL[i].store(val, Relaxed);
            SPINE_EXEC[i].store(exec_id, Relaxed);
            SPINE_BASE[i].store(stack_base, Relaxed);
            SPINE_ADDR[i].store(slot, Release);
            SPINE_GEN.fetch_add(1, Release);
            return;
        }
    }
    // Full: this executor's slot simply goes unwatched (diagnostic best-effort).
}

fn spine_unregister(slot: usize) {
    use core::sync::atomic::Ordering::{AcqRel, Relaxed, Release};
    for i in 0..SPINE_SLOTS {
        if SPINE_ADDR[i]
            .compare_exchange(slot, 0, AcqRel, Relaxed)
            .is_ok()
        {
            SPINE_GEN.fetch_add(1, Release);
            return;
        }
    }
}

/// Drop-path unregister: clear any entry whose slot lies on this stack. Covers
/// abandoned executors (their `run` never returned) right before the stack is
/// pooled/freed — the pool re-poison must not fire the watch.
fn spine_unregister_by_stack(stack_base: usize) {
    use core::sync::atomic::Ordering::{Acquire, Relaxed, Release, SeqCst};
    let lo = stack_base;
    let hi = stack_base + STACK_SIZE;
    for i in 0..SPINE_SLOTS {
        let a = SPINE_ADDR[i].load(Acquire);
        if a != 0 && a != usize::MAX && a >= lo && a < hi {
            if SPINE_ADDR[i]
                .compare_exchange(a, 0, SeqCst, Relaxed)
                .is_ok()
            {
                SPINE_GEN.fetch_add(1, Release);
            }
        }
    }
}

/// Registration generation, for per-CPU debug-register re-sync.
pub fn spine_gen() -> u64 {
    SPINE_GEN.load(core::sync::atomic::Ordering::Acquire)
}

/// First `out.len()` registered spine-slot addresses (0-padded).
pub fn spine_snapshot(out: &mut [usize]) -> usize {
    use core::sync::atomic::Ordering::Acquire;
    let mut n = 0;
    for i in 0..SPINE_SLOTS {
        if n >= out.len() {
            break;
        }
        let a = SPINE_ADDR[i].load(Acquire);
        if a != 0 && a != usize::MAX {
            out[n] = a;
            n += 1;
        }
    }
    for s in out[n..].iter_mut() {
        *s = 0;
    }
    n
}

/// Is `addr` currently a registered spine slot? (`#DB` handler filter: a hit
/// on an unregistered slot is pool-poison/reuse noise, not the corruptor.)
/// Returns the owning executor id when registered.
pub fn spine_owner_of(addr: usize) -> Option<usize> {
    use core::sync::atomic::Ordering::{Acquire, Relaxed};
    for i in 0..SPINE_SLOTS {
        if SPINE_ADDR[i].load(Acquire) == addr {
            return Some(SPINE_EXEC[i].load(Relaxed));
        }
    }
    None
}

/// One detected spine smash, for the caller (kernel-hal's timer tick) to
/// print with its IRQ-safe spin serial writer — no logging happens here, so
/// this is callable from IRQ context without touching the console lock.
#[derive(Clone, Copy)]
pub struct SpineSmash {
    /// How many smashes had been seen before this one (0 = first).
    pub ordinal: usize,
    /// The victim executor's id.
    pub exec_id: usize,
    /// The overwritten spine slot's address.
    pub slot: usize,
    /// The write-once value the slot must hold (return into `run_executor`).
    pub expected: u64,
    /// What the slot holds now.
    pub found: u64,
    /// The victim stack's usable range.
    pub stack_base: usize,
    pub stack_top: usize,
    /// Extent of the contiguous foreign blob around the slot (values equal to
    /// `found` or zero), bounded to ±4 KiB.
    pub blob_lo: usize,
    pub blob_hi: usize,
}

/// Timer-tick sweep: verify every registered spine slot still holds its
/// expected return address. Detection runs within one tick of the write —
/// usually BEFORE the victim unwinds into the fatal `ret` — and returns the
/// first mismatch's facts for the caller to report. A ≤16-load sweep when
/// nothing is wrong.
pub fn spine_verify() -> Option<SpineSmash> {
    use core::sync::atomic::Ordering::{Acquire, Relaxed};
    for i in 0..SPINE_SLOTS {
        let gen0 = SPINE_GEN.load(Acquire);
        let a = SPINE_ADDR[i].load(Acquire);
        if a == 0 || a == usize::MAX {
            continue;
        }
        let expect = SPINE_VAL[i].load(Relaxed);
        let exec_id = SPINE_EXEC[i].load(Relaxed);
        let base = SPINE_BASE[i].load(Relaxed);
        // SAFETY: a registered slot lies on a live (or at worst pooled — the
        // memory stays mapped) executor stack; 8-aligned by construction.
        let now = unsafe { core::ptr::read_volatile(a as *const u64) };
        if now == expect {
            continue;
        }
        // Seqlock-ish: if a register/unregister raced this read, skip — the
        // mismatch may pair a new slot with an old value or a poisoned stack.
        if SPINE_GEN.load(Acquire) != gen0 || SPINE_ADDR[i].load(Acquire) != a {
            continue;
        }
        let ordinal = SPINE_SMASHES.fetch_add(1, Relaxed);
        // Self-heal the expectation so the SAME unrepaired slot is reported
        // once, not at tick rate forever — while a FUTURE write to it (or to
        // any other slot) still re-triggers detection.
        SPINE_VAL[i].store(now, Relaxed);
        // Walk the contiguous foreign blob around the slot (bounded ±4 KiB).
        let top = base + STACK_SIZE;
        let lo_lim = a.saturating_sub(0x1000).max(base);
        let hi_lim = (a + 0x1000).min(top);
        let matches_blob = |v: u64| v == now || v == 0;
        let mut lo = a;
        while lo >= lo_lim + 8 {
            let v = unsafe { core::ptr::read_volatile((lo - 8) as *const u64) };
            if !matches_blob(v) {
                break;
            }
            lo -= 8;
        }
        let mut hi = a + 8;
        while hi + 8 <= hi_lim {
            let v = unsafe { core::ptr::read_volatile(hi as *const u64) };
            if !matches_blob(v) {
                break;
            }
            hi += 8;
        }
        return Some(SpineSmash {
            ordinal,
            exec_id,
            slot: a,
            expected: expect,
            found: now,
            stack_base: base,
            stack_top: top,
            blob_lo: lo,
            blob_hi: hi,
        });
    }
    None
}

// ── Dedicated executor-stack pool (SMP `[null-exec]` fix) ─────────────────────
//
// Executor stacks (`ALLOC_SIZE` each) come out of the SAME buddy heap as every
// `Vec`/`Box`. Returning a freed stack to that heap lets a later
// zero-initialising allocation land on it; under SMP a coroutine can still be
// racing on that block, and the zero-fill blanks its saved return addresses ->
// `RET` to 0 (the multi-core-ONLY `[null-exec]`: single-core soaks of 55 min /
// 400+ respawn cycles never hit it, a `-smp 4` boot triple-faults in ~10 min).
// Keeping freed stacks in a stack-ONLY free list means heap `Box`/`Vec` memory
// and executor-stack memory can never overlap, which removes that corruption
// class at the root. Pooled blocks keep their hard guard pages installed and
// their `[guard|usable|guard]` layout, so reuse is just a re-poison of the
// usable region — no unmap/remap churn and no TLB shootdowns (unlike the
// diagnostic quarantine). Under GL=1 the compositor can spawn and kill 30+
// threads in rapid bursts across 4 CPUs; the old cap of 32 overflowed, letting
// freed stack memory reach the general heap and trigger the SMP [null-exec]
// corruption. 128 covers a ~4×30 peak burst with headroom
// (balanced create/free keeps the pool near-empty in steady state).
const STACK_POOL_CAP: usize = 128;
static STACK_POOL: [core::sync::atomic::AtomicUsize; STACK_POOL_CAP] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; STACK_POOL_CAP];

/// Overflow retention for freed hard-guarded stack blocks when [`STACK_POOL`] is
/// full. A coroutine stack block must NEVER be returned to the shared buddy
/// arena (`Global`): that arena also backs userspace VMO frames, so a stack that
/// reaches it and is later handed to userspace can be corrupted by a stale
/// SMP-late write to a parked frame the retirement grace period reclaimed one
/// period too early. The live-stack registry cannot flag that (the block left
/// the registry at `Drop`), so it surfaces as a silent SIGSEGV in an innocent
/// process — e.g. the wallpaper renderer faulting in bounds-checked code with a
/// float-heavy stack and no `[double-alloc]`. Keeping every freed block in
/// coroutine-stack land (pool first, this list once the pool is full, both with
/// hard guards still installed) removes that aliasing at the root. Retained
/// blocks are re-handed by [`stack_pool_pop`], so the cost is bounded by peak
/// concurrent stacks, not cumulative churn.
static STACK_OVERFLOW: spin::Mutex<alloc::vec::Vec<usize>> =
    spin::Mutex::new(alloc::vec::Vec::new());

/// Take a pooled stack block (its `alloc_base`, hard guards already installed),
/// or `None` if the pool is empty. The fixed array is scanned lock-free (one
/// atomic swap per slot); the overflow retention list is checked last.
fn stack_pool_pop() -> Option<usize> {
    use core::sync::atomic::Ordering::AcqRel;
    for slot in STACK_POOL.iter() {
        let base = slot.swap(0, AcqRel);
        if base != 0 {
            return Some(base);
        }
    }
    STACK_OVERFLOW.lock().pop()
}

/// Return a hard-guarded stack block to the pool. `false` if the pool is full
/// (the caller must free it to the heap instead). Lock-free.
fn stack_pool_push(alloc_base: usize) -> bool {
    use core::sync::atomic::Ordering::{AcqRel, Relaxed};
    for slot in STACK_POOL.iter() {
        if slot
            .compare_exchange(0, alloc_base, AcqRel, Relaxed)
            .is_ok()
        {
            return true;
        }
    }
    false
}

impl Executor {
    pub fn new(task_collection: Arc<TaskCollection>) -> Pin<Box<Self>> {
        // Reuse a pooled stack if one is available (its hard guards are already
        // installed); otherwise carve a fresh block from the heap and install
        // guards below. Pooling keeps freed executor stacks out of the general
        // heap — see `STACK_POOL` — so a zero-init allocation can never blank a
        // live coroutine's stack.
        let pooled = stack_pool_pop();
        let alloc_base = match pooled {
            Some(base) => base,
            None => {
                let raw: NonNull<u8> = Global
                    .allocate(ALLOC_LAYOUT)
                    .expect("Alloction Stack Failed.")
                    .cast();
                raw.as_ptr() as usize
            }
        };
        debug_assert_eq!(alloc_base % PAGE_SIZE, 0);
        // Both registries, together, right here.
        //
        // They hold the same set and exist for the same check, but they are
        // read by different callers: `STACK_REG` (via `overlapping_live_stack`)
        // by the frame allocator, `STACK_REG_BASE` (via
        // `alloc_overlaps_live_stack`) by the `GlobalAlloc` hook — and
        // coroutine stacks are allocated through that second one. The insert
        // for it used to sit at the far end of this function, after two guard
        // installs (page-table splits with their TLB shootdowns) and a 2.6 MiB
        // poison loop, carrying a comment that said it recorded the stack
        // "from the first instant it can be aliased". The first instant is the
        // one above, where `Global.allocate` returned; everything between was
        // a window in which a block aliasing this stack could be handed out on
        // another CPU and the hook would wave it through — which is exactly
        // the `[double-alloc]` these registries were added to catch.
        publish_live_stack(alloc_base);
        let stack_base = alloc_base + GUARD_SIZE;
        let top_guard_base = stack_base + STACK_SIZE;
        // Prefer unmapped guards (hard). Soft canary only if hooks are missing
        // or install refuses (e.g. huge-page leaf). Bare-metal boot must call
        // `stack_guard::init` before the first `Executor::new`
        // (`warm_runtimes` right after hooks).
        //
        // Install bottom and top separately (hook API is one range per call;
        // stack_guard stores each base independently). A POOLED block already
        // has its hard guards installed (they were never removed while pooled),
        // so skip the install and report both hard.
        let (hard_guard_bottom, hard_guard_top) = if pooled.is_some() {
            (true, true)
        } else {
            match *STACK_GUARD_INSTALL.lock() {
                Some(f) => {
                    let ok_bottom = f(alloc_base, GUARD_SIZE);
                    if !ok_bottom {
                        note_soft_guard_fallback(
                            "bottom install refused (huge PTE / unmap failed)",
                        );
                    }
                    let ok_top = f(top_guard_base, TOP_GUARD_SIZE);
                    if !ok_top {
                        note_soft_guard_fallback("top install refused (huge PTE / unmap failed)");
                    }
                    (ok_bottom, ok_top)
                }
                None => {
                    note_soft_guard_fallback("hooks not registered yet");
                    (false, false)
                }
            }
        };
        if !hard_guard_bottom {
            unsafe {
                let p = alloc_base as *mut u64;
                for i in 0..GUARD_WORDS {
                    core::ptr::write_volatile(p.add(i), STACK_CANARY ^ i as u64);
                }
            }
            SOFT_GUARD_EXECUTORS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        } else {
            HARD_GUARD_EXECUTORS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        // Poison usable stack before context is written on top. Fresh buddy
        // pages are BSS-zero; a RET into never-used slots was rip=0 / [rsp]=0.
        // Non-zero poison makes that a distinctive bad-RIP #PF instead.
        unsafe {
            let p = stack_base as *mut u64;
            let words = STACK_SIZE / core::mem::size_of::<u64>();
            const STACK_POISON: u64 = 0xDEAD_BEEF_DEAD_BEEF;
            for i in 0..words {
                core::ptr::write_volatile(p.add(i), STACK_POISON);
            }
        }
        note_first_executor_created(hard_guard_bottom, hard_guard_top);
        let mut pin_executor = Pin::new(Box::new(Executor {
            id: executor_alloc_id(),
            task_collection,
            stack_base,
            hard_guard_bottom,
            hard_guard_top,
            context: ExecuterContext::default(),
            #[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
            context_data: ContextData::default(),
            task_id: 0,
            state: ExecutorState::UNUSED,
            current_task: core::ptr::null(),
            current_waker: core::ptr::null(),
            abandoned: AtomicBool::new(false),
            force_replace: AtomicBool::new(false),
            resume_owner: core::sync::atomic::AtomicUsize::new(0),
        }));

        pin_executor.init_stack_and_context();

        trace!(
            "stack top 0x{:x} executor addr 0x{:x}, pgbr = 0x{:x}",
            pin_executor.context.get_sp(),
            pin_executor.context.get_pc(),
            pin_executor.context.get_pgbr(),
        );
        pin_executor
    }

    // stack layout: [executor_addr | context ]
    fn init_stack_and_context(&mut self) {
        let mut stack_top = self.stack_base + STACK_SIZE;
        let self_addr = self as *const Self as usize;
        stack_top = unsafe { push_stack(stack_top, self_addr) };
        #[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
        {
            self.context_data = ContextData::new(
                executor_entry as *const () as usize,
                stack_top,
                crate::arch::pg_base_register(),
            );
            self.context
                .set_context(&self.context_data as *const _ as usize);
        }
        #[cfg(target_arch = "x86_64")]
        {
            let context_data = ContextData::new(
                executor_entry as *const () as usize,
                stack_top,
                crate::arch::pg_base_register(),
            );
            stack_top = unsafe { push_stack(stack_top, context_data) };
            self.context.set_context(stack_top);
        }
    }

    #[inline(never)]
    pub fn run(&mut self) {
        // Spine-slot capture (see the registry above): at this point our
        // caller `run_executor`'s `call` has just written the
        // return-into-`run_executor` qword at `stack_top - 0x508`, and it must
        // stay untouched until `run` returns. `[rbp + 8]` at function entry IS
        // that slot (frame pointers are enabled build-wide; `run`'s prologue
        // pushed rbp and set rbp = rsp, so rbp + 8 = entry rsp = &return
        // address). Validate geometry + a `.text`-range value before
        // registering so an inlining/FP change degrades to "unwatched", never
        // to a false trap.
        #[cfg(target_arch = "x86_64")]
        let spine_slot: usize = {
            let rbp: usize;
            // SAFETY: reads the frame-pointer register only.
            unsafe {
                core::arch::asm!("mov {}, rbp", out(reg) rbp,
                    options(nomem, nostack, preserves_flags))
            };
            let slot = rbp.wrapping_add(8);
            let top = self.stack_base + STACK_SIZE;
            if slot >= top - 0x800 && slot + 8 <= top && slot & 7 == 0 {
                // SAFETY: within this executor's own live stack.
                let val = unsafe { core::ptr::read_volatile(slot as *const u64) };
                if (0xffff_ff00_0000_0000..0xffff_ff00_0100_0000).contains(&val) {
                    spine_register(slot, val, self.id, self.stack_base);
                    slot
                } else {
                    0
                }
            } else {
                0
            }
        };
        #[cfg(not(target_arch = "x86_64"))]
        let spine_slot: usize = 0;
        // Lazy-TLB safety pin.
        //
        // `ThreadSwitchFuture::poll` leaves this CPU on the polled thread's
        // *process* page table after the poll (lazy-TLB: it skips reloading the
        // kernel CR3 to avoid a TLB flush per poll). The scheduler code below —
        // `take_task` and `steal_task_from_other_cpu` — therefore runs under
        // that user CR3. Those routines only touch kernel-half memory, which is
        // mapped in every process page table, so that is fine *as long as the
        // page table still exists*.
        //
        // The danger is the page table being freed out from under us: if the
        // process whose CR3 we are holding exits and is reaped on another CPU,
        // its `PageTableImpl` drops and the root (PML4) / intermediate frames go
        // back to the frame allocator and get reused. The MMU then walks freed,
        // overwritten page-table memory on the next TLB miss, so our own kernel
        // stack / the iret frame we are about to build reads garbage -> `iretq`
        // to ring-0 junk -> #UD. This is exactly the intermittent SMP crash seen
        // under `apk` (rapid fork/exit) — and ctxcheck never fires because the
        // saved `UserContext` is valid; it is the physical memory behind it that
        // changes under the stale CR3.
        //
        // Hold an `Arc` to the most-recently-polled task across the next
        // `take_task`/`steal` step. That keeps its `Thread` -> `Process` ->
        // `vmar` -> page table alive, so a concurrent exit cannot free the page
        // table while its CR3 is still loaded here. The pin is replaced only
        // after the *next* poll has switched CR3 to another address space (or to
        // the kernel CR3, which `CurrentThread::drop` restores when a thread
        // finishes), so the previous page table is released only once its CR3 is
        // no longer loaded on this CPU.
        let mut _cr3_pin: Option<Arc<Task>> = None;
        loop {
            let mut task_info = self.task_collection.take_task();
            if task_info.is_none() {
                task_info = crate::runtime::steal_task_from_other_cpu();
            }
            if let Some((_key, task, waker_ref)) = task_info {
                // `waker_ref` is the task's one shared waker (built at insert):
                // no per-poll `Arc::new`, and the borrowed bit was already set
                // atomically by the generator under the collection lock —
                // setting it again here was redundant.
                let waker = woke::waker_ref(&waker_ref);
                let mut cx = Context::from_waker(&waker);
                self.task_id = task.id();
                debug!("running future {}:{}", self.id(), task.id());
                // Hang detector: a task is being polled, so clear the idle-loop
                // streak. If the machine then spins the idle loop many times with
                // tasks still present but nothing polled, a wake was lost (see the
                // else-branch dump below).
                IDLE_STREAK.store(0, core::sync::atomic::Ordering::Relaxed);
                SCHED_POLLED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                // Publish who is running before entering the future: if the
                // poll faults, the panic-containment path reads these to retire
                // exactly this task (see `runtime::abandon_current_task`). Both
                // are cleared right after the poll returns, so a fault outside
                // a poll finds no victim and the kernel halts as before.
                self.current_task = Arc::as_ptr(&task);
                self.current_waker = Arc::as_ptr(&waker_ref);
                let ret = task.poll(&mut cx);
                self.current_task = core::ptr::null();
                self.current_waker = core::ptr::null();
                // Did this future overflow the coroutine stack?  The stack is a
                // guard-page-less heap allocation: an overflow silently corrupts
                // the adjacent heap object.  Detect it immediately and panic so
                // the corrupted heap is never used — continuing with a clobbered
                // heap leads to null fn-ptr calls and an unrecoverable crash loop
                // (the labwc/Wayland crash with garbled process names).
                if !self.canary_intact() {
                    panic!(
                        "\n[stackcheck] COROUTINE STACK OVERFLOW: executor id={} task_id={} \
                         stack_base={:#x} size={:#x} — halting before corrupted heap is used\n",
                        self.id(),
                        task.id(),
                        self.stack_base,
                        STACK_SIZE
                    );
                }
                debug!("back from future {}:{}", self.id(), task.id());
                self.task_id = 0;
                // Pin this task's address space for the upcoming take_task/steal
                // (which run under the CR3 this poll just (re)loaded). Replacing
                // the previous pin here is safe: CR3 now points at *this* task's
                // page table (or at the kernel CR3 if the thread just finished —
                // `CurrentThread::drop` restored it), so the page table we drop
                // is no longer the active one. See the comment at the top of
                // `run`.
                _cr3_pin = Some(task.clone());
                // Borrow-release ordering — this is load-bearing for SMP.
                //
                // The OLD order (mark_borrowed(false), then drop_by_ref on
                // Ready) opened a window where a completed task was published
                // as (borrowed=0, dropped=0). A wake that raced with the poll
                // (deferred by take_notified while we were borrowed — routine
                // for IRQ-driven futures) then let ANOTHER executor take and
                // re-poll the SAME completed task. If a timer preemption
                // parked either executor in that window, the late
                // mark_borrowed(false) could land on a slab slot that had
                // been removed and REUSED, wiping the borrow bit of an
                // unrelated live task -> two executors polling one future ->
                // the second spins forever on the future lock
                // (task_collection.rs, Task::poll) while the first sits
                // parked as a weak executor: the >8s DEADLOCK banner.
                //
                // Therefore: on Ready, publish `dropped` FIRST and leave the
                // borrow bit SET. take_notified masks dropped tasks, so the
                // task can never be handed out again; the generator's
                // dropped-branch remove() -> clear() wipes all bits (borrow
                // included) atomically with freeing the slot, BEFORE the slot
                // can be reused by insert(). Only a Pending poll releases the
                // borrow here, and only after Task::poll has returned (future
                // lock already released).
                match ret {
                    Poll::Ready(()) => {
                        debug!("task over id = {}", task.id());
                        waker_ref.drop_by_ref();
                    }
                    Poll::Pending => {
                        waker_ref.mark_borrowed(false);
                    }
                };
                if let ExecutorState::WEAK = self.state {
                    self.state = ExecutorState::KILLED;
                    // Past this return, `run_executor`'s post-run calls
                    // legitimately re-push over the spine slot — stop
                    // watching it first.
                    if spine_slot != 0 {
                        spine_unregister(spine_slot);
                    }
                    return;
                }
            } else {
                // Our run queue is drained (and stealing found nothing), so any
                // pending wake-up preemption request for this CPU has already
                // been satisfied by simply running out of work. Drop it: a stale
                // bit would suppress the coalesced IPI for the next real wake.
                crate::runtime::clear_need_resched(crate::arch::cpu_id() as usize);
                let runtime = crate::runtime::get_current_runtime();
                let task_num = runtime.task_num();
                let weak_executor = runtime.weak_executor_num();
                drop(runtime);
                // TODO: some cores may exit by mistake when we have multi-cores
                if cfg!(feature = "baremetal-test") && task_num == 0 {
                    debug!("all done! exit and reboot");
                    crate::runtime::sched_yield();
                } else if weak_executor != 0 {
                    debug!("return to runtime and run weak executor");
                    SCHED_WEAK_YIELD.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    crate::runtime::sched_yield();
                } else if crate::runtime::run_idle_callback() {
                    // The idle callback made progress (e.g. drained deferred
                    // driver jobs that may have woken tasks): re-check the run
                    // queue instead of halting until the next interrupt.
                    continue;
                } else {
                    // Hang detector (diagnostics only): the run queue is empty so
                    // we are about to halt until the next interrupt. If tasks still
                    // exist, count idle-loop iterations; the 250 Hz timer wakes us
                    // ~every 4 ms, so ~750 iterations with no task polled ≈ 3 s.
                    // `IDLE_STREAK` is global and reset on every real poll, so it
                    // only climbs while *no* CPU makes progress.
                    //
                    // Snapshot the page bits ONLY at report cadence:
                    // `debug_pending` walks every collection under its lock,
                    // far too heavy to run on each of the ~250 idle passes/s.
                    //
                    // Classification: `notified > 0` (affinity-parked) and
                    // `borrowed > 0` (in-flight on another executor) are normal
                    // under SMP; only tasks-exist-but-nothing-pending can never
                    // recover on its own = a genuine lost wake.
                    if task_num > 0 {
                        let s = IDLE_STREAK.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        if s == 750 || (s > 750 && s % 2500 == 0) {
                            let (tn, n, d, b) = self.task_collection.debug_pending();
                            if n == 0 && b == 0 {
                                warn!(
                                    "[sched] possible lost wake: {} task(s) parked \
                                     (notified=0 borrowed=0 dropped={}) after {} idle passes",
                                    tn, d, s
                                );
                            }
                        }
                    }
                    debug!("no other tasks, wait for interrupt");
                    // About to halt with an empty run queue: a quiescent
                    // point for the retired-stack grace period, and the
                    // ONLY one an idle CPU ever reaches.
                    //
                    // `note_cpu_quiescent` was called from three places, all
                    // of them in `run_until_idle`'s loop — and this branch
                    // does not go back there. `sched_yield` below covers the
                    // weak-executor case, but a CPU whose queue is drained,
                    // whose steal scan found nothing and whose runtime holds
                    // no weak executor halts HERE and stays inside
                    // `Executor::run` until work arrives. It is a perfectly
                    // ordinary state on a desktop with more cores than
                    // threads, and while a CPU is in it, its bit in every
                    // retired block's `pending_cpus` never clears. Those
                    // blocks then sit in the 256-slot table for good, and once
                    // it is full every later freed stack is leaked whole
                    // (~2.6 MiB each, `[stack-retire] retired-stack table
                    // full`) — on a workload that frees stacks in bursts
                    // (labwc under GL=1 spawns and kills 30+ threads at a
                    // time, see `STACK_POOL_CAP`).
                    //
                    // Sound, not a relaxation: what the grace period is
                    // protecting against is a CPU still executing on, or
                    // about to resume, a frame parked on the retired block.
                    // This CPU is executing on its OWN executor's stack and
                    // the only frame it will resume is its own, on the
                    // instruction after the halt. A parked executor frame is
                    // only ever resumed by the `switch` in `run_until_idle`,
                    // which this CPU has left. "Reached the runtime stack" is
                    // a sufficient condition for quiescence, not the
                    // necessary one.
                    note_cpu_quiescent(crate::arch::cpu_id() as usize);
                    // Halt protocol vs lost wakes. Publish "sleeping" FIRST,
                    // then re-check the queue with IRQs off, and only then
                    // halt (`wait_for_interrupt` is an atomic sti;hlt — an IPI
                    // arriving after the sti breaks the hlt). A remote waker
                    // does notify -> read sleeping-mask (both SeqCst): either
                    // it sees us sleeping and kicks us with the reschedule
                    // IPI, or its notify is ordered before our recheck and we
                    // see the ready bit and skip the halt. Either way the
                    // wake cannot fall into the check-then-halt window (which
                    // previously cost up to one full 4 ms tick).
                    let cpu = crate::arch::cpu_id() as usize;
                    let intr_was_on = crate::arch::intr_get();
                    crate::arch::intr_off();
                    crate::runtime::set_cpu_sleeping(cpu, true);
                    if !self.task_collection.has_ready() {
                        crate::arch::wait_for_interrupt();
                    }
                    crate::runtime::set_cpu_sleeping(cpu, false);
                    if intr_was_on {
                        crate::arch::intr_on();
                    }
                }
            }
        }
    }

    // 当前是否在运行future
    // 发生supervisor时钟中断时, 若executor在运行future, 则
    // 说明该future超时, 需要切换到另一个executor来执行其他future.
    pub fn is_running_future(&self) -> bool {
        self.task_id != 0
            || self
                .force_replace
                .load(core::sync::atomic::Ordering::Acquire)
    }

    pub fn killed(&self) -> bool {
        self.state == ExecutorState::KILLED
            || self.abandoned.load(core::sync::atomic::Ordering::SeqCst)
    }

    /// Whether this executor is *inside* `Task::poll` right now.
    ///
    /// Stricter than [`is_running_future`](Self::is_running_future), which stays
    /// true for the rest of the loop iteration after the poll returns. Only
    /// during the poll itself is there a future that can be retired, so this is
    /// what the panic-containment path tests: a fault in the scheduler's own
    /// code between polls has no task to blame and must not kill one.
    pub fn is_polling(&self) -> bool {
        !self.current_task.is_null()
    }

    /// Whether `sp` points into this executor's *usable* coroutine stack
    /// (guard bands excluded).
    ///
    /// The panic path uses this to prove that the faulting frames really are on
    /// the stack it is about to abandon, before it switches away from them. A
    /// fault taken on some other stack — the boot/idle stack, an IST stack —
    /// has nothing to do with this executor's task.
    pub fn stack_contains(&self, sp: usize) -> bool {
        (self.stack_base..self.stack_base + STACK_SIZE).contains(&sp)
    }

    /// [null-exec guard] Claim the exclusive right to stand on this executor's
    /// stack. `Err(holder_cpu)` means another CPU already owns it — resuming
    /// now would be the double-consume that pops a dead frame (`ret` to 0 /
    /// `cr3` garbage). The caller must NOT switch in on failure.
    pub fn try_claim_resume(&self, cpu: usize) -> Result<(), usize> {
        use core::sync::atomic::Ordering::{AcqRel, Acquire};
        match self
            .resume_owner
            .compare_exchange(0, cpu + 1, AcqRel, Acquire)
        {
            Ok(_) => Ok(()),
            Err(holder) => Err(holder.wrapping_sub(1)),
        }
    }

    /// Release the resume claim. Only call once control is back OFF this
    /// executor's stack (its context frame saved and parked) — releasing while
    /// still standing on it re-opens the double-resume window this closes.
    pub fn release_resume(&self) {
        self.resume_owner
            .store(0, core::sync::atomic::Ordering::Release);
    }

    /// Retire the task this executor is polling and mark the executor dead.
    ///
    /// After this, [`killed`](Self::killed) reports `true`, so the runtime drops
    /// this executor instead of ever resuming its (abandoned) stack, and the
    /// task is both marked finished and removed from the collection so no other
    /// CPU picks it up. The stack itself is not touched here — the caller is
    /// still standing on it and must switch away before it can be freed.
    ///
    /// Returns `false` when there is nothing to abandon or the future could not
    /// be retired, in which case nothing has been changed.
    ///
    /// # Safety
    ///
    /// May only be called from the CPU running this executor, as the immediate
    /// prelude to switching off its stack for good.
    pub unsafe fn abandon_current_task(&self) -> bool {
        // SAFETY: non-null means `run` is inside `Task::poll`, so the `Arc`s
        // these borrow from are live in that (current, faulted) frame.
        let Some(task) = self.current_task.as_ref() else {
            return false;
        };
        if !task.abandon() {
            return false;
        }
        // Publish the task as dropped so the collection's generator removes the
        // slab slot. The borrow bit is deliberately left set (as on the normal
        // `Ready` path) so nothing can hand this task out in the window before
        // the removal lands.
        if let Some(waker) = self.current_waker.as_ref() {
            waker.drop_by_ref();
        }
        self.abandoned
            .store(true, core::sync::atomic::Ordering::SeqCst);
        true
    }

    /// Abandon this executor after a fault taken **outside** any poll.
    ///
    /// The idle/scheduler half of [`abandon_current_task`](Self::abandon_current_task).
    /// A null-range fault on an executor's stack while it is between tasks (an
    /// IRQ landing on the idle path, a corrupted return slot in the scheduler's
    /// own frames) has NO task to retire, so `abandon_current_task` refuses and
    /// the machine used to halt — even though nothing of value was in flight and
    /// the core was perfectly recoverable.
    ///
    /// Here there is no future to leak and no task to kill: just retire the
    /// executor. `killed()` becomes true so the runtime drops it rather than
    /// resuming its corrupt stack, and `force_replace` makes the runtime build a
    /// fresh strong executor instead of switching straight back into this one.
    ///
    /// Returns `false` if a poll IS in flight — that is the task case, and
    /// killing the task is strictly better than discarding the whole executor.
    ///
    /// # Safety
    ///
    /// May only be called from the CPU running this executor, as the immediate
    /// prelude to switching off its stack for good.
    /// Force the runtime to rebuild this executor without retiring anything.
    /// Last resort for a fault whose task could not be retired: the stack is
    /// dead either way and must not be resumed.
    pub fn force_replace_executor(&self) {
        self.force_replace
            .store(true, core::sync::atomic::Ordering::Release);
        self.abandoned
            .store(true, core::sync::atomic::Ordering::SeqCst);
    }

    pub unsafe fn abandon_idle_executor(&self) -> bool {
        if !self.current_task.is_null() {
            return false;
        }
        self.force_replace
            .store(true, core::sync::atomic::Ordering::Release);
        self.abandoned
            .store(true, core::sync::atomic::Ordering::SeqCst);
        true
    }

    pub fn mark_weak(&mut self) {
        self.state = ExecutorState::WEAK;
    }

    pub fn id(&self) -> usize {
        self.id
    }

    pub fn task_id(&self) -> usize {
        self.task_id
    }

    /// Base address of this executor's coroutine stack (lowest address).
    pub fn stack_base(&self) -> usize {
        self.stack_base
    }

    /// [diag] Whether the soft-guard canary below the usable stack is intact.
    ///
    /// Layout: `[BOTTOM_GUARD][usable STACK_SIZE][TOP_GUARD]`. With a hard
    /// (unmapped) bottom guard, overflow #PFs before touching heap — report
    /// intact. Soft path samples canary words at both ends of the bottom guard.
    pub fn canary_intact(&self) -> bool {
        if self.hard_guard_bottom {
            return true;
        }
        unsafe {
            let p = (self.stack_base - GUARD_SIZE) as *const u64;
            // High end of the guard (first words an overflow smashes).
            let high_ok = (0..8).all(|i| {
                let idx = GUARD_WORDS - 1 - i;
                core::ptr::read_volatile(p.add(idx)) == (STACK_CANARY ^ idx as u64)
            });
            // Low end (catches a large downward jump past the edge samples).
            let low_ok =
                (0..4).all(|i| core::ptr::read_volatile(p.add(i)) == (STACK_CANARY ^ i as u64));
            high_ok && low_ok
        }
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        let alloc_base = self.stack_base - GUARD_SIZE;

        // [null-exec root guard] Never free or reuse a stack a CPU is still
        // standing on. `resume_owner` is 0 only once control is OFF this
        // executor's stack (its frame parked and `release_resume` called). If
        // it is non-zero at Drop, some CPU claimed this executor and is
        // executing on (or switching into) its stack RIGHT NOW — returning that
        // block to the buddy heap is precisely the `[double-alloc]` that let a
        // later `Vec`/`Box` land on a live coroutine stack and zero its saved
        // return slots (the labwc/Wayland `wl_list` NULL-deref crash class).
        //
        // The quarantine and recycle pool both assume quiescence here and so
        // cannot save this case; the only safe action is to LEAK the block —
        // never write-protect it (that would fault the CPU still on it), never
        // free it, never recycle it. One stack (~2.6 MiB) leaked is a bounded
        // cost next to heap corruption and an unrecoverable crash loop. The log
        // names the still-standing CPU so the remaining lifetime race can be
        // traced to where an executor is dropped while claimed.
        let owner = self
            .resume_owner
            .load(core::sync::atomic::Ordering::Acquire);
        if owner != 0 {
            use core::sync::atomic::{AtomicUsize, Ordering};
            static LEAKED: AtomicUsize = AtomicUsize::new(0);
            let n = LEAKED.fetch_add(1, Ordering::Relaxed) + 1;
            // Drop the tracking entries (bounded registries) but NOT the memory.
            retract_live_stack(alloc_base);
            spine_unregister_by_stack(self.stack_base);
            error!(
                "[null-exec root guard] executor id={} dropped while cpu={} still stands on its \
                 stack {:#x}..{:#x} — leaking the block instead of freeing it (leaked {} so far); \
                 this is the free-while-live race, contained",
                self.id,
                owner.wrapping_sub(1),
                self.stack_base,
                self.stack_base + STACK_SIZE,
                n,
            );
            return;
        }

        // Stop tracking this stack BEFORE it goes back to the heap/pool, so a
        // later legitimate reuse of the freed range is not flagged as a
        // double-alloc.
        retract_live_stack(alloc_base);
        // Abandoned executors never return from `run`, so their spine slot is
        // still registered here; clear it before the pool re-poison writes
        // through it (a watched write would misreport the poison loop as the
        // corruptor).
        spine_unregister_by_stack(self.stack_base);
        // [null-exec root fix] A dropped executor may still have an SMP-late
        // parked frame. Neither the pool nor the general heap may see this
        // block until every online CPU has run on its OWN runtime stack since
        // retirement. Quarantine remains the diagnostic fast-path: if the
        // protect hook is available, freeze the usable region now so any stale
        // writer faults as `[stack-uaf]` during that grace period.
        let on_this_stack = {
            #[cfg(target_arch = "x86_64")]
            {
                let rsp: usize;
                // SAFETY: reads RSP only.
                unsafe {
                    core::arch::asm!(
                        "mov {}, rsp",
                        out(reg) rsp,
                        options(nomem, nostack, preserves_flags)
                    );
                }
                rsp >= alloc_base && rsp < alloc_base + ALLOC_SIZE
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                false
            }
        };
        let protect = *STACK_QUAR_PROTECT.lock();
        let quarantine_always = protect.is_some();
        let want_quarantine =
            quarantine_always || QUARANTINE_ENABLED.load(core::sync::atomic::Ordering::Relaxed);
        let mut protected = false;
        if want_quarantine && !on_this_stack {
            if let Some(protect) = protect {
                protected = protect(self.stack_base, STACK_SIZE);
            }
        }
        retire_stack_after_grace(
            alloc_base,
            self.hard_guard_bottom,
            self.hard_guard_top,
            protected,
        );
    }
}

unsafe impl Send for Executor {}
unsafe impl Sync for Executor {}

pub unsafe fn push_stack<T>(stack_top: usize, val: T) -> usize {
    let stack_top = (stack_top as *mut T).sub(1);
    *stack_top = val;
    stack_top as _
}

/// The SMP grace period that decides when a freed coroutine stack may be
/// handed out again — which had no tests.
///
/// Getting it wrong one period early is the `[double-alloc]` / `[null-exec]`
/// corruption this whole file is built around: a block returned while an SMP
/// sibling could still resume a parked frame onto it, and then zero-filled by
/// its next consumer. Getting it wrong the other way leaks 2.6 MiB a time.
///
/// These drive the table directly rather than through `Executor::drop`, which
/// needs a real coroutine stack; the decisions live in
/// [`retire_stack_after_grace`] and [`observe_cpu_quiescent_locked`], and they
/// are what is exercised here. They share the module's globals, so they lock.
#[cfg(test)]
mod grace_period_tests {
    use super::*;
    use core::sync::atomic::Ordering;

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Empty every global these tests read, and hand back the ready mask that
    /// was there. The host's `cpu_id()` is hardwired to 0, so "us" is CPU 0.
    struct Fresh {
        _guard: std::sync::MutexGuard<'static, ()>,
        saved_ready: u64,
    }

    impl Drop for Fresh {
        fn drop(&mut self) {
            crate::runtime::set_executor_ready_mask_for_test(self.saved_ready);
            *RETIRED_STACKS.lock() = [RetiredStack::EMPTY; RETIRED_STACKS_CAP];
            for slot in STACK_POOL.iter() {
                slot.store(0, Ordering::SeqCst);
            }
            STACK_OVERFLOW.lock().clear();
        }
    }

    fn fresh(ready: u64) -> Fresh {
        let guard = test_lock();
        let saved_ready = crate::runtime::set_executor_ready_mask_for_test(ready);
        *RETIRED_STACKS.lock() = [RetiredStack::EMPTY; RETIRED_STACKS_CAP];
        for slot in STACK_POOL.iter() {
            slot.store(0, Ordering::SeqCst);
        }
        STACK_OVERFLOW.lock().clear();
        Fresh {
            _guard: guard,
            saved_ready,
        }
    }

    /// A hard-guarded block reaches the pool; anything else is leaked. So
    /// "was it reclaimed" is "is it in the pool".
    fn pooled(base: usize) -> bool {
        STACK_POOL.iter().any(|s| s.load(Ordering::SeqCst) == base)
            || STACK_OVERFLOW.lock().contains(&base)
    }

    fn parked(base: usize) -> Option<u64> {
        RETIRED_STACKS
            .lock()
            .iter()
            .find(|s| s.alloc_base == base)
            .map(|s| s.pending_cpus)
    }

    fn retire(base: usize) {
        retire_stack_after_grace(base, true, true, false);
    }

    #[test]
    fn on_a_uniprocessor_a_freed_stack_is_reusable_at_once() {
        // Only CPU 0 has an executor, and CPU 0 is the one retiring: there is
        // no sibling that could hold a parked frame, so waiting for one would
        // be waiting forever.
        let _f = fresh(0b1);
        retire(0x1000_0000);
        assert!(pooled(0x1000_0000));
        assert_eq!(
            parked(0x1000_0000),
            None,
            "nothing to wait for, yet it waited"
        );
    }

    #[test]
    fn a_stack_is_held_until_every_other_ready_cpu_has_passed_through() {
        let _f = fresh(0b1011); // CPUs 0, 1 and 3 have executors; we are 0.
        retire(0x2000_0000);
        assert_eq!(
            parked(0x2000_0000),
            Some(0b1010),
            "the retiring CPU waited on itself"
        );
        assert!(!pooled(0x2000_0000));

        note_cpu_quiescent(1);
        assert_eq!(parked(0x2000_0000), Some(0b1000));
        assert!(
            !pooled(0x2000_0000),
            "reclaimed with CPU 3 still unaccounted for"
        );

        note_cpu_quiescent(3);
        assert_eq!(parked(0x2000_0000), None);
        assert!(pooled(0x2000_0000));
    }

    #[test]
    fn a_cpu_reporting_twice_does_not_count_twice() {
        let _f = fresh(0b0111);
        retire(0x3000_0000);
        assert_eq!(parked(0x3000_0000), Some(0b0110));
        note_cpu_quiescent(1);
        note_cpu_quiescent(1);
        note_cpu_quiescent(1);
        assert_eq!(
            parked(0x3000_0000),
            Some(0b0100),
            "a repeat report cleared CPU 2"
        );
        assert!(!pooled(0x3000_0000));
    }

    #[test]
    fn a_cpu_that_appears_after_the_retirement_is_not_waited_on() {
        // The mask is sampled at retirement on purpose: a CPU whose executor
        // did not exist when the block was freed cannot hold a frame parked on
        // it, so making the block wait for it would be a leak, not caution.
        let _f = fresh(0b0011);
        retire(0x4000_0000);
        assert_eq!(parked(0x4000_0000), Some(0b0010));
        crate::runtime::set_executor_ready_mask_for_test(0b1111);
        note_cpu_quiescent(1);
        assert!(pooled(0x4000_0000));
    }

    #[test]
    fn an_id_past_the_mask_reports_for_nobody() {
        let _f = fresh(0b0011);
        // Waited on by the boot CPU and CPU 2 — the shape a block retired by
        // CPU 1 has. `1u64 << 64` is not a no-op: on x86 the shift amount
        // wraps, so id 64 would report for CPU 0 and id 127 for CPU 63. Every
        // other per-CPU accessor in this crate refuses such an id.
        park_as(0x5000_0000, 0b0101);
        note_cpu_quiescent(64);
        note_cpu_quiescent(127);
        note_cpu_quiescent(usize::MAX);
        assert_eq!(
            parked(0x5000_0000),
            Some(0b0101),
            "an id with no bit cleared one"
        );
        assert!(!pooled(0x5000_0000));
        note_cpu_quiescent(0);
        note_cpu_quiescent(2);
        assert!(pooled(0x5000_0000));
    }

    /// Park a block as if a CPU other than this host's CPU 0 had retired it,
    /// so the waited-on set can include bit 0 — which is what a real machine
    /// looks like whenever the retiring CPU is not the boot CPU.
    fn park_as(base: usize, pending_cpus: u64) {
        let mut retired = RETIRED_STACKS.lock();
        let slot = retired
            .iter_mut()
            .find(|s| s.alloc_base == 0)
            .expect("retired table full");
        *slot = RetiredStack {
            alloc_base: base,
            hard_guard_bottom: true,
            hard_guard_top: true,
            protected: false,
            pending_cpus,
        };
    }

    #[test]
    fn each_block_keeps_its_own_count() {
        let _f = fresh(0b0111);
        retire(0x6000_0000);
        note_cpu_quiescent(1);
        retire(0x6100_0000);
        // The second block was retired after CPU 1 reported, so it still waits
        // for both peers; the first waits only for CPU 2.
        assert_eq!(parked(0x6000_0000), Some(0b0100));
        assert_eq!(parked(0x6100_0000), Some(0b0110));
        note_cpu_quiescent(2);
        assert!(pooled(0x6000_0000));
        assert_eq!(
            parked(0x6100_0000),
            Some(0b0010),
            "the second block was let go early"
        );
        note_cpu_quiescent(1);
        assert!(pooled(0x6100_0000));
    }

    #[test]
    fn a_soft_guarded_block_is_leaked_rather_than_pooled() {
        // `Executor::new`'s reuse path assumes hard guards are already
        // installed, and returning the block to the shared buddy arena is the
        // aliasing the pool exists to prevent — so the only remaining option
        // is to leak it. What matters here is that it does not reach the pool.
        let _f = fresh(0b0001);
        retire_stack_after_grace(0x7000_0000, true, false, false);
        retire_stack_after_grace(0x7100_0000, false, true, false);
        assert!(!pooled(0x7000_0000));
        assert!(!pooled(0x7100_0000));
        assert_eq!(
            parked(0x7000_0000),
            None,
            "a leaked block still holds a slot"
        );
        assert_eq!(parked(0x7100_0000), None);
    }

    #[test]
    fn a_quarantined_block_is_unprotected_before_it_is_handed_back() {
        use core::sync::atomic::AtomicUsize;
        static UNPROTECTED: AtomicUsize = AtomicUsize::new(0);
        fn record(usable_base: usize, _size: usize) {
            UNPROTECTED.store(usable_base, Ordering::SeqCst);
        }
        let _f = fresh(0b0011);
        UNPROTECTED.store(0, Ordering::SeqCst);
        // SAFETY: the recorder touches no page tables; this is the host.
        unsafe { set_stack_quarantine_hooks(|_, _| true, record) };

        retire_stack_after_grace(0x8000_0000, true, true, true);
        assert_eq!(
            UNPROTECTED.load(Ordering::SeqCst),
            0,
            "unprotected before quiescence"
        );
        note_cpu_quiescent(1);
        assert_eq!(
            UNPROTECTED.load(Ordering::SeqCst),
            0x8000_0000 + GUARD_SIZE,
            "a write-protected block went back to the pool still read-only"
        );
        assert!(pooled(0x8000_0000));

        *STACK_QUAR_PROTECT.lock() = None;
        *STACK_QUAR_UNPROTECT.lock() = None;
    }

    #[test]
    fn a_full_table_leaks_the_block_rather_than_handing_it_back_early() {
        let _f = fresh(0b0011);
        for i in 0..RETIRED_STACKS_CAP {
            retire(0x9000_0000 + i * ALLOC_SIZE);
        }
        assert_eq!(parked(0x9000_0000), Some(0b0010));
        let overflow = 0x9000_0000 + RETIRED_STACKS_CAP * ALLOC_SIZE;
        retire(overflow);
        // No slot, so no way to know when it is safe: the one thing that must
        // not happen is for it to be reused anyway.
        assert_eq!(parked(overflow), None);
        assert!(
            !pooled(overflow),
            "a block with nowhere to wait was handed out anyway"
        );
    }

    #[test]
    fn one_peer_that_never_reports_holds_every_block_retired_after_it() {
        // Why the call site matters as much as the arithmetic. The mask is
        // sampled once, at retirement, and only a report clears a bit — so a
        // single ready CPU that never reaches a quiescent point pins every
        // block retired from then on, until the table is full and the rest
        // are leaked outright. `Executor::run`'s idle branch is where an idle
        // CPU sits, and it is why that branch reports before it halts.
        let _f = fresh(0b0111);
        for i in 0..8 {
            retire(0xb000_0000 + i * ALLOC_SIZE);
        }
        for _ in 0..4 {
            note_cpu_quiescent(1);
        }
        for i in 0..8 {
            let base = 0xb000_0000 + i * ALLOC_SIZE;
            assert_eq!(parked(base), Some(0b0100), "block {}", i);
            assert!(!pooled(base));
        }
        note_cpu_quiescent(2);
        for i in 0..8 {
            assert!(pooled(0xb000_0000 + i * ALLOC_SIZE), "block {}", i);
        }
    }

    #[test]
    fn the_pool_falls_back_to_the_retention_list_instead_of_the_heap() {
        // A coroutine stack block must never reach the shared buddy arena,
        // which also backs userspace frames. Past `STACK_POOL_CAP` the
        // overflow list is what keeps it in stack land.
        let _f = fresh(0b0001);
        for i in 0..STACK_POOL_CAP + 4 {
            retire(0xa000_0000 + i * ALLOC_SIZE);
        }
        for i in 0..STACK_POOL_CAP + 4 {
            assert!(pooled(0xa000_0000 + i * ALLOC_SIZE), "block {} was lost", i);
        }
        assert_eq!(STACK_OVERFLOW.lock().len(), 4);
        // And they come back out, pool first then the retention list.
        let mut seen = 0;
        while stack_pool_pop().is_some() {
            seen += 1;
        }
        assert_eq!(seen, STACK_POOL_CAP + 4);
    }
}

/// The two live-stack registries and the two overlap checks over them.
///
/// Neither had a test, and a false positive in either one is not a wrong log
/// line: both callers in `zCore/src/memory.rs` end in `panic!`, inside the
/// global allocator, with the heap lock just released. A false negative is the
/// `[double-alloc]` they were written to catch going past unnoticed.
#[cfg(test)]
mod stack_registry_tests {
    use super::*;
    use core::sync::atomic::Ordering;

    /// Both registries are module globals; every test here empties them.
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    struct Clean(std::sync::MutexGuard<'static, ()>);

    impl Drop for Clean {
        fn drop(&mut self) {
            for slot in STACK_REG.iter() {
                slot.store(0, Ordering::SeqCst);
            }
            for slot in STACK_REG_BASE.iter() {
                slot.store(0, Ordering::SeqCst);
            }
            STACK_REG_OVERFLOW.store(0, Ordering::SeqCst);
            STACK_REG_BASE_OVERFLOW.store(0, Ordering::SeqCst);
        }
    }

    fn clean() -> Clean {
        let g = test_lock();
        for slot in STACK_REG.iter() {
            slot.store(0, Ordering::SeqCst);
        }
        for slot in STACK_REG_BASE.iter() {
            slot.store(0, Ordering::SeqCst);
        }
        STACK_REG_OVERFLOW.store(0, Ordering::SeqCst);
        STACK_REG_BASE_OVERFLOW.store(0, Ordering::SeqCst);
        Clean(g)
    }

    /// A plausible stack allocation base: page-aligned and far from both ends
    /// of the address space, so a test that means to overflow has to say so.
    const BASE: usize = 0x1000_0000;

    use super::{publish_live_stack as publish, retract_live_stack as retract};

    #[test]
    fn both_registries_answer_for_a_stack_the_moment_it_is_published() {
        // The frame allocator reads one of them and the `GlobalAlloc` hook the
        // other. A stack visible to one and not the other is a stack half the
        // machine can be handed.
        let _c = clean();
        publish(BASE);
        let inside = BASE + ALLOC_SIZE / 2;
        assert_eq!(overlapping_live_stack(inside, 64), Some(BASE));
        assert_eq!(alloc_overlaps_live_stack(inside, 64), Some(BASE));
    }

    #[test]
    fn a_stack_only_half_published_is_a_hole_in_one_of_the_two_checks() {
        // This is the shape of the bug: `register_stack` ran at the top of
        // `Executor::new` and `stack_reg_insert` at the bottom, two guard
        // installs and a 2.6 MiB poison loop later. In between, the frame
        // allocator would refuse a block over this stack and the `GlobalAlloc`
        // hook — the one stacks are actually allocated through — would not.
        let _c = clean();
        register_stack(BASE);
        let inside = BASE + ALLOC_SIZE / 2;
        assert_eq!(overlapping_live_stack(inside, 64), Some(BASE));
        assert_eq!(
            alloc_overlaps_live_stack(inside, 64),
            None,
            "this is the window; if it has closed, the test above is the one that matters"
        );
    }

    #[test]
    fn a_retracted_stack_stops_being_reported_by_both() {
        // A stale entry is a permanent false positive, and a false positive
        // here panics the kernel out of the global allocator.
        let _c = clean();
        publish(BASE);
        retract(BASE);
        let inside = BASE + ALLOC_SIZE / 2;
        assert_eq!(overlapping_live_stack(inside, 64), None);
        assert_eq!(alloc_overlaps_live_stack(inside, 64), None);
    }

    #[test]
    fn a_block_ending_exactly_where_a_stack_begins_does_not_overlap() {
        // Half-open ranges: the byte at `base` belongs to the stack, the byte
        // before it does not. Getting this off by one costs a live kernel.
        let _c = clean();
        publish(BASE);
        assert_eq!(overlapping_live_stack(BASE - 4096, 4096), None);
        assert_eq!(alloc_overlaps_live_stack(BASE - 4096, 4096), None);
        assert_eq!(overlapping_live_stack(BASE - 4096, 4097), Some(BASE));
        assert_eq!(alloc_overlaps_live_stack(BASE - 4096, 4097), Some(BASE));
    }

    #[test]
    fn a_block_starting_exactly_where_a_stack_ends_does_not_overlap() {
        let _c = clean();
        publish(BASE);
        assert_eq!(overlapping_live_stack(BASE + ALLOC_SIZE, 4096), None);
        assert_eq!(alloc_overlaps_live_stack(BASE + ALLOC_SIZE, 4096), None);
        assert_eq!(overlapping_live_stack(BASE + ALLOC_SIZE - 1, 1), Some(BASE));
        assert_eq!(
            alloc_overlaps_live_stack(BASE + ALLOC_SIZE - 1, 1),
            Some(BASE)
        );
    }

    #[test]
    fn a_stack_is_reported_by_the_guard_bands_too_not_only_its_usable_part() {
        // The registries record `alloc_base`, which is the bottom guard. A
        // block landing in a guard band is still a block landing on this
        // allocation, and the guard is what an overflow is supposed to hit.
        let _c = clean();
        publish(BASE);
        assert_eq!(overlapping_live_stack(BASE, 8), Some(BASE));
        assert_eq!(alloc_overlaps_live_stack(BASE, 8), Some(BASE));
        let top_guard = BASE + GUARD_SIZE + STACK_SIZE;
        assert_eq!(overlapping_live_stack(top_guard, 8), Some(BASE));
        assert_eq!(alloc_overlaps_live_stack(top_guard, 8), Some(BASE));
    }

    #[test]
    fn a_range_whose_end_overflows_the_address_space_is_never_called_clean() {
        // `overlapping_live_stack` used to `?` on a `checked_add`, which this
        // function spells "no overlap" — the one answer a range it cannot even
        // measure must not give, when the caller reads it as permission to
        // hand the block out.
        let _c = clean();
        publish(BASE);
        assert_eq!(overlapping_live_stack(BASE, usize::MAX), Some(BASE));
        assert_eq!(alloc_overlaps_live_stack(BASE, usize::MAX), Some(BASE));
    }

    #[test]
    fn a_fresh_stack_does_not_flag_itself() {
        // Both doc comments used to promise an exemption for exactly this, and
        // neither function has ever had one. It works because the check runs
        // as part of the allocation and the inserts come after it.
        let _c = clean();
        assert_eq!(alloc_overlaps_live_stack(BASE, ALLOC_SIZE), None);
        publish(BASE);
        // And once published it does flag: a second hand-out of the same block
        // is the double-alloc, whatever its size.
        assert_eq!(alloc_overlaps_live_stack(BASE, ALLOC_SIZE), Some(BASE));
    }

    #[test]
    fn two_stacks_side_by_side_are_told_apart() {
        let _c = clean();
        let second = BASE + ALLOC_SIZE;
        publish(BASE);
        publish(second);
        assert_eq!(alloc_overlaps_live_stack(second + 8, 8), Some(second));
        retract(second);
        assert_eq!(alloc_overlaps_live_stack(second + 8, 8), None);
        assert_eq!(
            alloc_overlaps_live_stack(BASE + 8, 8),
            Some(BASE),
            "retracting one stack unregistered its neighbour"
        );
    }

    #[test]
    fn a_stack_that_did_not_fit_is_counted_rather_than_dropped_in_silence() {
        // A full table makes both checks return false negatives. The counters
        // are what stops a clean result being mistaken for a clean machine.
        let _c = clean();
        for i in 0..MAX_TRACKED_STACKS {
            publish(BASE + (i + 1) * ALLOC_SIZE);
        }
        assert_eq!(untracked_live_stacks(), 0);
        assert_eq!(untracked_alloc_stacks(), 0);
        let overflowing = BASE + (MAX_TRACKED_STACKS + 1) * ALLOC_SIZE;
        publish(overflowing);
        assert_eq!(untracked_live_stacks(), 1);
        assert_eq!(untracked_alloc_stacks(), 1);
        assert_eq!(
            alloc_overlaps_live_stack(overflowing + 8, 8),
            None,
            "the false negative is real — which is what the counter is for"
        );
    }

    #[test]
    fn the_overflow_counters_never_go_back_down() {
        // A dropped insert may later be balanced by that stack being freed,
        // and nothing here can tell. Over-reporting incompleteness is the safe
        // direction for a counter whose only job is to say when a clean result
        // means nothing.
        let _c = clean();
        for i in 0..MAX_TRACKED_STACKS {
            publish(BASE + (i + 1) * ALLOC_SIZE);
        }
        let overflowing = BASE + (MAX_TRACKED_STACKS + 1) * ALLOC_SIZE;
        publish(overflowing);
        retract(overflowing);
        assert_eq!(untracked_live_stacks(), 1);
        assert_eq!(untracked_alloc_stacks(), 1);
    }
}
