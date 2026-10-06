//! Lightweight kernel runtime statistics, surfaced at `/proc/perf/kernel`.
//!
//! Aimed at debugging "why is the machine warm / busy": the headline numbers
//! are **how much wall-clock the CPUs actually spent halted (idle)** vs running,
//! and **the per-vector interrupt counts** (an IRQ storm is the usual culprit
//! behind unexpected heat). Everything here is lock-free atomic counters bumped
//! from the bare-metal idle / IRQ / timer paths; on libos they stay zero.

use crate::config::MAX_CORE_NUM;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

/// Number of interrupt vectors tracked individually (x86 IDT is 256 wide).
const NVEC: usize = 256;

static IDLE_NS: AtomicU64 = AtomicU64::new(0);
static IDLE_ENTRIES: AtomicU64 = AtomicU64::new(0);

/// [diag] Per-CPU idle nap accounting (ns halted, and nap count), indexed by the
/// dense logical CPU id. Shows whether a *specific* core keeps a short idle cap
/// (frequent short naps → it is the one driving the background HID poll) or
/// sleeps long (deep idle). Used to debug input-responsiveness-vs-heat.
static IDLE_NS_PERCPU: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];
static IDLE_ENTRIES_PERCPU: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];

/// [diag] xHCI HID poll invocations split by the path that issued them: the
/// timer tick (`timer`) vs an I/O-wait loop (`iowait`). Keyboard/mouse input is
/// delivered from these polls, so their combined rate IS the input
/// responsiveness. When the CPUs halt and the net busy-spin is gone, `iowait`
/// drops to ~0 and only `timer` keeps input alive — its rate then says whether
/// idle HID polling is fast enough.
static HID_POLL_TIMER: AtomicU64 = AtomicU64::new(0);
static HID_POLL_IOWAIT: AtomicU64 = AtomicU64::new(0);

/// [diag] Monotonic ns of the previous tick on each CPU, for gap measurement.
static TICK_LAST_NS_PERCPU: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];
/// [diag] Longest gap between two consecutive ticks on a *busy* CPU, the
/// count of such gaps beyond three nominal periods, and the last one's size
/// and time. A busy CPU that did not run for tens of ms — a KVM vCPU the
/// host descheduled, an interrupts-off section — shows here, and every wait
/// parked on that CPU's re-scan (audio feeders among them) was served that
/// late. A tick that interrupts the idle halt is counted apart: a halted
/// vCPU the host wakes late harms nobody, and on a lightly loaded machine
/// that is most of them.
static TICK_GAP_MAX_NS: AtomicU64 = AtomicU64::new(0);
static TICK_GAPS_LATE: AtomicU64 = AtomicU64::new(0);
static TICK_GAPS_LATE_IDLE: AtomicU64 = AtomicU64::new(0);
static TICK_GAP_LAST_LATE_NS: AtomicU64 = AtomicU64::new(0);
static TICK_GAP_LAST_LATE_AT_NS: AtomicU64 = AtomicU64::new(0);
/// [diag] The last gap on each CPU, counted only when that CPU was *busy*.
///
/// Read by the slice accounting, which has to tell "this thread was off the
/// CPU" from "the tick that measures it did not fire". Both look identical from
/// the thread: a long gap between two of its own tick observations. Only the
/// busy case is recorded, and a halt stores 0, because a CPU that was halted
/// was running nobody -- a thread whose deadline burned across that really was
/// off the CPU and is owed the time.
static TICK_GAP_LAST_BUSY_NS_PERCPU: [AtomicU64; MAX_CORE_NUM] =
    [const { AtomicU64::new(0) }; MAX_CORE_NUM];

/// [diag] Each CPU's own longest busy gap, and the RIP the tick that set it
/// interrupted.
///
/// The gap says a busy CPU went that long without a tick; the RIP says *where
/// it was* when the tick finally landed. On this kernel the lock that protects
/// almost everything disables interrupts for its whole critical section
/// (`kernel-sync`'s `push_off`), so a multi-second gap on a busy CPU is a
/// multi-second critical section, and the only thing missing to name it is an
/// address.
///
/// Per-CPU, and the maximum taken at *read* time, because the pair has to
/// describe one tick. Two globals updated separately cannot: a CPU that wins
/// `fetch_max`, is overtaken by a bigger gap on another CPU, and only then
/// stores its RIP leaves the number and the address describing different
/// events -- and the address is the whole point. Each slot here is written
/// only by its owning CPU, from the tick interrupt with interrupts already
/// off, so the two halves of a slot cannot disagree.
static TICK_GAP_MAX_NS_PERCPU: [AtomicU64; MAX_CORE_NUM] =
    [const { AtomicU64::new(0) }; MAX_CORE_NUM];
static TICK_GAP_MAX_RIP_PERCPU: [AtomicU64; MAX_CORE_NUM] =
    [const { AtomicU64::new(0) }; MAX_CORE_NUM];

/// [diag] How long this CPU went without a tick, last time it took one while
/// busy; 0 when the last tick interrupted a halt or there is nothing recorded.
///
/// See [`TICK_GAP_LAST_BUSY_NS_PERCPU`]. Called from the slice accounting in
/// the user-trap handler, which runs after `handle_irq` has already accounted
/// this tick, so the value is this tick's own gap.
pub fn last_busy_tick_gap_ns() -> u64 {
    let cpu = crate::cpu::cpu_id() as usize;
    if cpu >= MAX_CORE_NUM {
        return 0;
    }
    TICK_GAP_LAST_BUSY_NS_PERCPU[cpu].load(Relaxed)
}

/// [diag] Where the CPU was when the longest busy tick gap finally ended, and
/// how long that gap was. `(0, 0)` when no busy gap has been recorded.
///
/// The maximum is taken here, over the per-CPU slots, so the gap and the
/// address always come from the same tick. See [`TICK_GAP_MAX_NS_PERCPU`].
pub fn tick_gap_max_rip() -> (u64, u64) {
    let mut best = 0;
    let mut rip = 0;
    for (g, r) in TICK_GAP_MAX_NS_PERCPU
        .iter()
        .zip(TICK_GAP_MAX_RIP_PERCPU.iter())
    {
        let gap = g.load(Relaxed);
        if gap > best {
            best = gap;
            rip = r.load(Relaxed);
        }
    }
    (best, rip)
}

/// [diag] Account the gap since this CPU's previous tick. `nominal_ns` is
/// the tick period the timer was programmed for. Called from the tick
/// interrupt, so the idle flag says whether the tick interrupted a halt.
pub fn note_tick_gap(now_ns: u64, nominal_ns: u64) {
    let cpu = crate::cpu::cpu_id() as usize;
    if cpu >= MAX_CORE_NUM {
        return;
    }
    let last = TICK_LAST_NS_PERCPU[cpu].swap(now_ns, Relaxed);
    if last == 0 || now_ns <= last {
        return;
    }
    let gap = now_ns - last;
    let late = gap > nominal_ns.saturating_mul(3);
    if CPU_IN_IDLE[cpu].load(Relaxed) {
        if late {
            TICK_GAPS_LATE_IDLE.fetch_add(1, Relaxed);
        }
        // A halted CPU was running nobody, so this gap explains no thread's
        // missing time: leave the slice accounting nothing to subtract.
        TICK_GAP_LAST_BUSY_NS_PERCPU[cpu].store(0, Relaxed);
        return;
    }
    TICK_GAP_LAST_BUSY_NS_PERCPU[cpu].store(gap, Relaxed);
    TICK_GAP_MAX_NS.fetch_max(gap, Relaxed);
    if TICK_GAP_MAX_NS_PERCPU[cpu].load(Relaxed) < gap {
        // Our own slot, so nobody else is writing it and the pair stays
        // together. `note_tick_context` ran earlier in the same trap (see
        // `trap_handler`), so the RIP slot already holds this tick's address.
        TICK_GAP_MAX_NS_PERCPU[cpu].store(gap, Relaxed);
        TICK_GAP_MAX_RIP_PERCPU[cpu].store(TICK_LAST_RIP_PERCPU[cpu].load(Relaxed), Relaxed);
    }
    if late {
        TICK_GAPS_LATE.fetch_add(1, Relaxed);
        TICK_GAP_LAST_LATE_NS.store(gap, Relaxed);
        TICK_GAP_LAST_LATE_AT_NS.store(now_ns, Relaxed);
    }
}

/// [diag] Account one xHCI HID poll issued from the timer tick.
pub fn note_hid_poll_timer() {
    HID_POLL_TIMER.fetch_add(1, Relaxed);
}

/// [diag] Account one xHCI HID poll issued from an I/O-wait loop.
pub fn note_hid_poll_iowait() {
    HID_POLL_IOWAIT.fetch_add(1, Relaxed);
}
static TIMER_TICKS: AtomicU64 = AtomicU64::new(0);
/// LAPIC deadline re-arms: how often the timer was reprogrammed to fire sooner
/// than the scheduler tick would have. Compared against `timer_ticks` it says
/// whether deadline programming is buying precision (a handful of re-arms per
/// tick) or has degenerated into an interrupt storm (re-arms >> ticks).
static TIMER_REARMS: AtomicU64 = AtomicU64::new(0);
static IRQ_TOTAL: AtomicU64 = AtomicU64::new(0);
static IRQ_COUNTS: [AtomicU64; NVEC] = [const { AtomicU64::new(0) }; NVEC];
/// Idle-callback invocations and how many found deferred work pending. The
/// scheduler only halts when the callback finds nothing, so a high "busy" ratio
/// here means the idle path keeps finding work and the CPUs never sleep — the
/// signature of a busy-spin (and the heat that comes with it).
static IDLE_CB_TOTAL: AtomicU64 = AtomicU64::new(0);
static IDLE_CB_BUSY: AtomicU64 = AtomicU64::new(0);

/// Per-CPU user-/kernel-mode nanoseconds, attributed by `ThreadSwitchFuture::poll`
/// (zircon-object) from the wall-clock span of one `Future::poll` call — the
/// only span that can never include time a thread spent blocked, since `poll`
/// is synchronous and cannot itself await. These back the `user`/`system`
/// columns of `/proc/stat` and (summed with `IDLE_NS_PERCPU`) let `idle` there
/// agree with the halt-time-based figure `/proc/perf/kernel` already reports.
static USER_NS_PERCPU: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];
static SYS_NS_PERCPU: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];

/// Attribute `ns` nanoseconds of user-mode execution to `cpu`.
#[inline(always)]
pub fn note_user_time(cpu: usize, ns: u64) {
    if cpu < MAX_CORE_NUM {
        USER_NS_PERCPU[cpu].fetch_add(ns, Relaxed);
    }
}

/// Attribute `ns` nanoseconds of kernel-mode execution to `cpu`.
#[inline(always)]
pub fn note_sys_time(cpu: usize, ns: u64) {
    if cpu < MAX_CORE_NUM {
        SYS_NS_PERCPU[cpu].fetch_add(ns, Relaxed);
    }
}

/// `(user, system, idle)` for one logical CPU, in USER_HZ (100 Hz) jiffies —
/// the unit `/proc/stat` reports. Reads zero for a CPU that never came online
/// (or past `MAX_CORE_NUM`), matching Linux's own "absent means zero" reading.
pub fn cpu_times_jiffies(cpu: usize) -> (u64, u64, u64) {
    const NS_PER_JIFFY: u64 = 10_000_000; // 1e7 ns = 10 ms = 1 / USER_HZ(100)
    if cpu >= MAX_CORE_NUM {
        return (0, 0, 0);
    }
    let user = USER_NS_PERCPU[cpu].load(Relaxed) / NS_PER_JIFFY;
    let sys = SYS_NS_PERCPU[cpu].load(Relaxed) / NS_PER_JIFFY;
    let idle = IDLE_NS_PERCPU[cpu].load(Relaxed) / NS_PER_JIFFY;
    (user, sys, idle)
}

/// `(tasks polled, weak-executor yields)` from the scheduler loop — to attribute
/// a busy-spin: a high `polled` rate means a task keeps re-readying itself; a
/// high `weak_yield` rate means the CPUs spin on an outstanding weak executor.
/// (The scheduler only exists on bare metal; libos reports zeros.)
#[cfg(target_os = "none")]
pub fn sched_stats() -> (u64, u64, u64) {
    executor::sched_stats()
}

#[cfg(not(target_os = "none"))]
pub fn sched_stats() -> (u64, u64, u64) {
    (0, 0, 0)
}

/// `(deadline timer enabled, wake-up preemption enabled)`.
///
/// Both are boot switches (`TIMERDEADLINE=0` / `WAKEPREEMPT=0`). Reporting them
/// means a captured `/proc/perf/kernel` says which mode produced it — an A/B
/// pair of logs that does not record its own configuration is two numbers with
/// nothing tying them to a cause.
#[cfg(target_os = "none")]
pub fn sched_switches() -> (bool, bool) {
    (
        crate::timer::deadline_timer_enabled(),
        executor::wakeup_preempt_enabled(),
    )
}

#[cfg(not(target_os = "none"))]
pub fn sched_switches() -> (bool, bool) {
    (false, false)
}

/// `(timers pending across every CPU's heap, timers adopted from a CPU that
/// had stopped taking ticks)`.
///
/// Timer heaps are per-CPU, so no core is woken for another core's deadline.
/// The second number is the safety net for that split and should stay 0: it
/// only moves when some CPU went so long without a tick that another had to
/// serve its callbacks.
#[cfg(target_os = "none")]
pub fn timer_heap_stats() -> (usize, u64) {
    (
        crate::timer::timer_pending_count(),
        crate::timer::timer_stray_count(),
    )
}

#[cfg(not(target_os = "none"))]
pub fn timer_heap_stats() -> (usize, u64) {
    (0, 0)
}

/// `(wake-up preemption requests, requests honoured)`.
///
/// A request is raised when a task becomes runnable on a CPU that is busy with
/// a different task; it is honoured when that CPU's user-trap path yields in
/// response. `honoured / requested` is the share of wakes that actually cut
/// short someone else's timeslice instead of waiting it out — the interactive
/// latency knob. A large shortfall means the requests are landing on CPUs that
/// stay in kernel mode, where the trap path never sees them.
#[cfg(target_os = "none")]
pub fn wakeup_preempt_stats() -> (u64, u64, u64) {
    executor::wakeup_preempt_stats()
}

#[cfg(not(target_os = "none"))]
pub fn wakeup_preempt_stats() -> (u64, u64, u64) {
    (0, 0, 0)
}

/// `(steal scans, victims probed, steals ok, affinity-empty victims skipped,
/// rebalance pulls, scans skipped by the stealable hint, rescue pulls)`.
#[cfg(target_os = "none")]
pub fn sched_steal_stats() -> (u64, u64, u64, u64, u64, u64, u64) {
    executor::sched_steal_stats()
}

#[cfg(not(target_os = "none"))]
pub fn sched_steal_stats() -> (u64, u64, u64, u64, u64, u64, u64) {
    (0, 0, 0, 0, 0, 0, 0)
}

/// `(weak executors created, peak live weaks on any CPU, soft-cap hits)`.
#[cfg(target_os = "none")]
pub fn sched_weak_stats() -> (u64, u64, u64) {
    executor::sched_weak_stats()
}

#[cfg(not(target_os = "none"))]
pub fn sched_weak_stats() -> (u64, u64, u64) {
    (0, 0, 0)
}

/// `(stack-pool occupied slots, overflow-list length)`.
#[cfg(target_os = "none")]
pub fn stack_pool_stats() -> (usize, usize) {
    executor::stack_pool_stats()
}

#[cfg(not(target_os = "none"))]
pub fn stack_pool_stats() -> (usize, usize) {
    (0, 0)
}

/// `(deepest coroutine-stack use observed at a park, STACK_SIZE)` in bytes.
/// The evidence for (or against) shrinking executor stacks.
#[cfg(target_os = "none")]
pub fn stack_high_water() -> (usize, usize) {
    executor::stack_high_water()
}

#[cfg(not(target_os = "none"))]
pub fn stack_high_water() -> (usize, usize) {
    (0, 0)
}

/// `(alloc-registry drops, live-registry drops)` for the coroutine-stack
/// hand-out guard.
///
/// The scheduler records every live executor stack so the buddy allocator can
/// refuse to hand that memory to a `Vec`/`Box`/VMO frame (the `[double-alloc]`
/// that zeroes a live stack and, via the shared buddy arena, a userspace page —
/// the lunarbar/lunarbg/labwc `wl_list` NULL-deref crash class). Both counters
/// are monotonic and rise ONLY when a registry table filled and an insert was
/// dropped: a non-zero value means the guard has blind spots and the
/// double-alloc protection is not complete. Zero means the registry never
/// overflowed, so that path is ruled out and the corruption is elsewhere.
#[cfg(target_os = "none")]
pub fn coroutine_stack_registry_drops() -> (usize, usize) {
    (
        executor::untracked_alloc_stacks(),
        executor::untracked_live_stacks(),
    )
}

#[cfg(not(target_os = "none"))]
pub fn coroutine_stack_registry_drops() -> (usize, usize) {
    (0, 0)
}

/// Account one idle-callback invocation; `had_work` is whether it found deferred
/// jobs (and so kept the CPU from halting).
pub fn note_idle_callback(had_work: bool) {
    IDLE_CB_TOTAL.fetch_add(1, Relaxed);
    if had_work {
        IDLE_CB_BUSY.fetch_add(1, Relaxed);
    }
}

/// Account `ns` of wall-clock spent halted in one idle nap. Called by the
/// per-CPU idle routine around its `hlt`/`mwait`.
pub fn note_idle(ns: u64) {
    IDLE_NS.fetch_add(ns, Relaxed);
    IDLE_ENTRIES.fetch_add(1, Relaxed);
    // [diag] per-CPU breakdown (cpu_id is 0 on libos, real on bare).
    let cpu = crate::cpu::cpu_id() as usize;
    if cpu < MAX_CORE_NUM {
        IDLE_NS_PERCPU[cpu].fetch_add(ns, Relaxed);
        IDLE_ENTRIES_PERCPU[cpu].fetch_add(1, Relaxed);
    }
}

/// [diag] Whether each logical CPU is *currently* parked in its idle `hlt`/
/// `mwait`. Set immediately before the halt and cleared right after it wakes, so
/// a reader can tell a genuinely-idle core (halted now) from a busy-spinning one
/// — the distinction the lifetime busy% average cannot make. This is the robust,
/// build-independent version of "is the captured RIP the post-`hlt` instruction".
static CPU_IN_IDLE: [AtomicBool; MAX_CORE_NUM] = [const { AtomicBool::new(false) }; MAX_CORE_NUM];

/// Mark the calling CPU as entering (`true`) or leaving (`false`) idle halt.
pub fn set_cpu_idle(in_idle: bool) {
    let cpu = crate::cpu::cpu_id() as usize;
    if cpu < MAX_CORE_NUM {
        CPU_IN_IDLE[cpu].store(in_idle, Relaxed);
    }
}

/// Number of logical CPUs currently parked in idle halt (best-effort snapshot).
pub fn cpus_idle_now() -> usize {
    CPU_IN_IDLE.iter().filter(|c| c.load(Relaxed)).count()
}

/// Whether the *calling* CPU is currently marked idle-halted. Meaningful from
/// an interrupt handler: the flag is set by the idle path around its `hlt`, so
/// an IRQ that reads `true` interrupted the halt itself.
pub fn current_cpu_in_idle() -> bool {
    let cpu = crate::cpu::cpu_id() as usize;
    cpu < MAX_CORE_NUM && CPU_IN_IDLE[cpu].load(Relaxed)
}

/// The `{data, vtable}` words of the timer callback this CPU is currently
/// invoking (0,0 = none). Published by `timer_tick` around each dispatch so a
/// fault *inside* a callback can name the exact closure — the vtable
/// symbolizes to the closure type of the `timer_set` call site.
static TIMER_CB_DATA: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];
static TIMER_CB_VTABLE: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];

/// Record the callback about to run on this CPU (0,0 clears).
pub fn note_timer_cb(data: u64, vtable: u64) {
    let cpu = crate::cpu::cpu_id() as usize;
    if cpu < MAX_CORE_NUM {
        TIMER_CB_DATA[cpu].store(data, Relaxed);
        TIMER_CB_VTABLE[cpu].store(vtable, Relaxed);
    }
}

/// Clear the abandoned callback's diagnostic record without consulting GS.
pub fn clear_timer_cb_after_abandon() {
    if let Some(cpu) = fault_slot() {
        TIMER_CB_DATA[cpu].store(0, Relaxed);
        TIMER_CB_VTABLE[cpu].store(0, Relaxed);
    }
}

/// `(data, vtable)` of the timer callback the calling CPU is inside, or (0,0).
pub fn current_timer_cb() -> (u64, u64) {
    let cpu = crate::cpu::cpu_id() as usize;
    if cpu < MAX_CORE_NUM {
        (
            TIMER_CB_DATA[cpu].load(Relaxed),
            TIMER_CB_VTABLE[cpu].load(Relaxed),
        )
    } else {
        (0, 0)
    }
}

/// Bitmask of logical CPUs currently parked in idle halt (bit `i` = cpu `i`).
/// Used by the TLB-shootdown initiator to avoid synchronously waiting on a core
/// that is halted: a halted core is not executing, and the shootdown IPI it was
/// sent will flush its TLB when it wakes (before it runs any user instruction),
/// exactly as the existing budget-exhaustion fire-and-forget fallback relies on.
pub fn cpu_idle_mask() -> u64 {
    let mut mask = 0u64;
    for (i, c) in CPU_IN_IDLE.iter().enumerate() {
        if i < 64 && c.load(Relaxed) {
            mask |= 1u64 << i;
        }
    }
    mask
}

/// Account one timer tick.
pub fn note_timer_tick() {
    TIMER_TICKS.fetch_add(1, Relaxed);
}

/// Account one LAPIC deadline re-arm.
pub fn note_timer_rearm() {
    TIMER_REARMS.fetch_add(1, Relaxed);
}

// ---------------------------------------------------------------------------
// Kernel heap profiling
// ---------------------------------------------------------------------------
//
// `fork` was measured at 471.7 us per mapping with 512 mappings against 55.8 us
// with 32 — quadratic in the mapping count, degrading `create_child` and the
// rest of `clone_map` together, and resetting when the process is replaced. That
// pattern points beneath both to something every one of them does, and the
// suspect is the buddy allocator: `buddy_system_allocator` 0.8.0's `dealloc`
// finds a block's buddy by scanning the whole free list of its size class,
// repeating for each class it merges up through. A fork of n mappings creates
// and destroys about 3n objects of the same size, so the list grows to ~n and
// each free costs O(n).
//
// Timing that is the confirmation, and it has to be paid for carefully: this is
// the hottest path in the kernel. Hence a switch (`HEAPPROF=1`), off by default,
// and raw `rdtsc` rather than `timer_now()` — which on a machine whose TSC is
// not invariant does a `fetch_max` on a globally shared cacheline and would
// swamp the very cost being measured. Cycles, not nanoseconds: only the ratio
// between configurations matters here, and converting would need the very clock
// this avoids.
static HEAP_PROF: AtomicBool = AtomicBool::new(false);
static HEAP_ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
static HEAP_ALLOC_CYCLES: AtomicU64 = AtomicU64::new(0);
static HEAP_DEALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
static HEAP_DEALLOC_CYCLES: AtomicU64 = AtomicU64::new(0);

/// Enable heap profiling (`HEAPPROF=1` on the kernel command line).
pub fn set_heap_prof(on: bool) {
    HEAP_PROF.store(on, Relaxed);
}

/// Whether the allocator should time itself. Read once per allocation, so it is
/// a single relaxed load on the hot path when off.
#[inline(always)]
pub fn heap_prof_enabled() -> bool {
    HEAP_PROF.load(Relaxed)
}

/// Account one allocation taking `cycles` (0 when profiling is off).
///
/// The *count* is kept unconditionally — one relaxed add, alongside the two the
/// allocator already does — because it is what attributes allocations to a
/// caller: sampling it around a region of code gives that region's allocation
/// count without any per-caller plumbing. The cycle total is only meaningful
/// with `HEAPPROF=1`.
#[inline(always)]
pub fn note_heap_alloc(cycles: u64) {
    HEAP_ALLOC_CALLS.fetch_add(1, Relaxed);
    if cycles != 0 {
        HEAP_ALLOC_CYCLES.fetch_add(cycles, Relaxed);
    }
}

/// Allocations performed since boot. Sample around a region to count its own.
#[inline(always)]
pub fn heap_alloc_calls() -> u64 {
    HEAP_ALLOC_CALLS.load(Relaxed)
}

/// Account one deallocation taking `cycles`.
#[inline(always)]
pub fn note_heap_dealloc(cycles: u64) {
    HEAP_DEALLOC_CALLS.fetch_add(1, Relaxed);
    if cycles != 0 {
        HEAP_DEALLOC_CYCLES.fetch_add(cycles, Relaxed);
    }
}

/// `(alloc calls, alloc cycles, dealloc calls, dealloc cycles)`.
///
/// A `dealloc` average that climbs with the number of live same-sized objects,
/// while `alloc` stays flat, is the free-list scan.
pub fn heap_prof_stats() -> (u64, u64, u64, u64) {
    (
        HEAP_ALLOC_CALLS.load(Relaxed),
        HEAP_ALLOC_CYCLES.load(Relaxed),
        HEAP_DEALLOC_CALLS.load(Relaxed),
        HEAP_DEALLOC_CYCLES.load(Relaxed),
    )
}

/// [diag] Per-CPU timer ticks, split by whether the tick interrupted user mode
/// (a thread burning CPU in ring 3) or kernel mode (idle `hlt`, a syscall, or a
/// kernel busy-spin). A core that is pegged with mostly *user* ticks is running
/// a CPU-bound user thread; mostly *kernel* ticks on a pegged core points at a
/// kernel-side spin (lock / poll loop). Used to locate the source of idle heat.
static TICK_TOTAL_PERCPU: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];
static TICK_USER_PERCPU: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];
/// [diag] Most recent RIP observed when a tick interrupted this CPU. For a core
/// wedged in an interrupts-off spin (no more ticks) this stays frozen at the RIP
/// it had on its last tick — i.e. near where it entered the spin — so it can be
/// resolved to a symbol with addr2line.
static TICK_LAST_RIP_PERCPU: [AtomicU64; MAX_CORE_NUM] =
    [const { AtomicU64::new(0) }; MAX_CORE_NUM];

/// [diag] Account one timer tick by the context it interrupted, recording the
/// interrupted instruction pointer.
pub fn note_tick_context(from_user: bool, rip: u64) {
    let cpu = crate::cpu::cpu_id() as usize;
    if cpu < MAX_CORE_NUM {
        TICK_TOTAL_PERCPU[cpu].fetch_add(1, Relaxed);
        if from_user {
            TICK_USER_PERCPU[cpu].fetch_add(1, Relaxed);
        }
        TICK_LAST_RIP_PERCPU[cpu].store(rip, Relaxed);
    }
}

/// [diag] The RIP a timer tick last observed interrupting THIS cpu — a
/// best-effort "what was running here" for the isolation heuristic. A low value
/// (`< 0xffff_8000_0000_0000`) is a userspace RIP (the interrupted thread was in
/// user code); a high one is kernel code. Symbolize with addr2line.
pub fn current_cpu_tick_rip() -> u64 {
    let cpu = crate::cpu::cpu_id() as usize;
    if cpu < MAX_CORE_NUM {
        TICK_LAST_RIP_PERCPU[cpu].load(Relaxed)
    } else {
        0
    }
}

/// [diag] The RIP a timer tick last observed interrupting `cpu` (0 if never, or
/// `cpu` out of range). For a core wedged in an interrupts-off spin this stays
/// frozen where it entered the spin — so the deadlock banner can name where a
/// non-acking shootdown target actually is. Symbolize with addr2line.
pub fn cpu_tick_rip(cpu: usize) -> u64 {
    if cpu < MAX_CORE_NUM {
        TICK_LAST_RIP_PERCPU[cpu].load(Relaxed)
    } else {
        0
    }
}

/// [diag] Print every CPU's last-tick RIP with the IRQ-safe spin serial
/// writer. Used by corruption reports ([spine-smash]) to name what each core
/// was doing at most one tick before the detection — the writer is on one of
/// them. Symbolize with addr2line.
pub fn dump_last_tick_rips() {
    for (cpu, slot) in TICK_LAST_RIP_PERCPU.iter().enumerate() {
        let rip = slot.load(Relaxed);
        if rip != 0 {
            crate::console::serial_write_fmt_spin(format_args!(
                "[tick-rips]   cpu{cpu} last_tick_rip={rip:#x}\n"
            ));
        }
    }
}

/// [diag] Per-CPU RIP captured by the NMI handler. An NMI is delivered even to a
/// core spinning with interrupts disabled, so broadcasting one and reading these
/// slots gives the *current* instruction pointer of an otherwise-wedged core —
/// unlike the last-tick RIP, which freezes one tick *before* the spin begins.
static NMI_RIP_PERCPU: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];

/// [diag] Record the interrupted RIP from the NMI handler (current CPU).
pub fn note_nmi_rip(rip: u64) {
    let cpu = crate::cpu::cpu_id() as usize;
    if cpu < MAX_CORE_NUM {
        NMI_RIP_PERCPU[cpu].store(rip, Relaxed);
    }
}

/// [diag] Most-recent page-fault instruction pointer, stashed by the trap
/// handler (which has the trap frame) so the kernel page-fault handler --
/// which only receives vaddr+flags -- can name the exact faulting code in
/// its panic message.
///
/// Per-CPU, not global. The single-global version assumed "faults are handled
/// to completion before the next, so there's no cross-fault race that matters",
/// which is false with more than one core: EVERY page fault on EVERY cpu stores
/// here, so an ordinary fault elsewhere overwrites the slot between a panicking
/// cpu's store and `oops`'s read. That is not only a wrong diagnostic line --
/// `oops` feeds `current_fault_rsp()` to `fault_sp_abandonable` to decide whether
/// a fault can be isolated, so a clobbered value turns a containable panic into
/// "cannot isolate -- halting". Observed exactly that: a #DF stored
/// 0xffffff002127fa30 and `oops` read back 0xffffff0020e7f1f0, a stack
/// belonging to another cpu.
static FAULT_RIP: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];
static FAULT_RBP: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];
static FAULT_RSP: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];
static FAULT_CS: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];
static ACTIVE_FAULT_RSP: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];

/// [diag] The general-purpose registers at the faulting instruction.
///
/// `rip`/`rbp`/`rsp` alone answer "where", and the recurring fault this exists
/// for needs "with what". It arrives as a WRITE to a nanosecond reading of this
/// boot's clock from inside
/// `BTreeMap<usize, PageState>::insert+0x1b4` -- so some operand's base was a
/// clock value where a node pointer belongs, and the register file says which
/// operand, and what the *other* registers held. One of them points at the
/// `BTreeMap` itself, which is the heap cell a write-watch would want.
///
/// The `[null-exec]` containment has printed its registers all along; the
/// kernel `#PF` report never had them to print, so three captures of the same
/// fault came back naming the victim and nothing about the pointer it used.
///
/// Order is [`GPR_NAMES`].
static FAULT_GPRS: [[AtomicU64; 16]; MAX_CORE_NUM] =
    [const { [const { AtomicU64::new(0) }; 16] }; MAX_CORE_NUM];

/// The register names, in the order `FAULT_GPRS` stores them.
pub const GPR_NAMES: [&str; 16] = [
    "rax", "rbx", "rcx", "rdx", "rsi", "rdi", "rbp", "rsp", "r8", "r9", "r10", "r11", "r12", "r13",
    "r14", "r15",
];

/// Keeps fault-stack provenance valid only while its trap handler is active.
#[must_use]
pub struct FaultScope {
    cpu: Option<usize>,
    previous_rsp: u64,
    _not_send: core::marker::PhantomData<*mut ()>,
}

impl FaultScope {
    fn enter(cpu: Option<usize>, rsp: u64, cs: u64) -> Self {
        let kernel_rsp = if cs != 0 && cs & 3 == 0 { rsp } else { 0 };
        let previous_rsp = cpu.map_or(0, |cpu| ACTIVE_FAULT_RSP[cpu].swap(kernel_rsp, Relaxed));
        Self {
            cpu,
            previous_rsp,
            _not_send: core::marker::PhantomData,
        }
    }
}

impl Drop for FaultScope {
    fn drop(&mut self) {
        if let Some(cpu) = self.cpu {
            ACTIVE_FAULT_RSP[cpu].store(self.previous_rsp, Relaxed);
        }
    }
}

/// The kernel stack pointer of the currently active fault, never historic regs.
///
/// Independent of the diagnostic exception summary, which the panic reporter
/// consumes before attempting containment. Nested handled faults restore the
/// outer fault's provenance when their scope ends.
pub fn current_fault_rsp() -> Option<u64> {
    let rsp = ACTIVE_FAULT_RSP[fault_slot()?].load(Relaxed);
    (rsp != 0).then_some(rsp)
}

/// Forget all fault scopes when their entire call chain is being abandoned.
///
/// # Safety
/// Interrupts must be disabled and none of this CPU's active fault scopes may
/// subsequently return or run their destructors.
pub unsafe fn abandon_fault_scopes() {
    if let Some(cpu) = fault_slot() {
        ACTIVE_FAULT_RSP[cpu].store(0, Relaxed);
    }
}

/// Index for the per-CPU fault slots; `None` past `MAX_CORE_NUM`, where storing
/// would be out of bounds and reading would be another cpu's data.
///
/// Keyed off the Local APIC id, never GS. `crate::cpu::cpu_id()` resolves the
/// logical id through the GS-backed per-CPU region, and these slots are read by
/// `oops` precisely when something has gone wrong in the trap path -- including
/// the window `lock::current_cpu_id_via_apic` documents, where `syscall_return`
/// has already swapped in the USER gsbase while CS is still ring 0. Indexing by
/// a GS-derived id there lands the record in another cpu's slot, which is the
/// exact cross-cpu mix-up this per-cpu split exists to end.
fn fault_slot() -> Option<usize> {
    let cpu = lock::current_cpu_id_via_apic() as usize;
    (cpu < MAX_CORE_NUM).then_some(cpu)
}

/// [diag] Record the RIP of the instruction that just page-faulted.
pub fn note_fault_rip(rip: u64) {
    if let Some(cpu) = fault_slot() {
        FAULT_RIP[cpu].store(rip, Relaxed);
    }
}

/// [diag] Record the frame/stack pointers at the faulting instruction so the
/// page-fault handler can walk the call chain (e.g. name the caller of a wild
/// `memset`). Stored alongside the RIP by the arch trap entry. `cs` is the
/// hardware CS at the fault: ring 0 vs ring 3 is how a kernel EXECUTE to a
/// userspace RIP is told apart from a real user #PF.
///
/// Keep the returned scope alive until the trap handler returns. Diagnostic
/// registers persist, but containment provenance expires with this scope.
pub fn note_fault_regs(rip: u64, rbp: u64, rsp: u64, cs: u64) -> FaultScope {
    let cpu = fault_slot();
    if let Some(cpu) = cpu {
        FAULT_RIP[cpu].store(rip, Relaxed);
        FAULT_RBP[cpu].store(rbp, Relaxed);
        FAULT_RSP[cpu].store(rsp, Relaxed);
        FAULT_CS[cpu].store(cs, Relaxed);
    }
    FaultScope::enter(cpu, rsp, cs)
}

/// [diag] Record the general-purpose registers at the faulting instruction.
///
/// `gprs` is in [`GPR_NAMES`] order. Sixteen relaxed stores on the fault path,
/// which is already doing far more than that, and they are what turns "a write
/// to a bad address" into "this register held it and that one held the object".
/// See `FAULT_GPRS`.
pub fn note_fault_gprs(gprs: &[u64; 16]) {
    if let Some(cpu) = fault_slot() {
        for (slot, value) in FAULT_GPRS[cpu].iter().zip(gprs.iter()) {
            slot.store(*value, Relaxed);
        }
    }
}

/// [diag] The general-purpose registers of the last fault on this CPU, in
/// [`GPR_NAMES`] order. All zero when nothing has faulted here.
pub fn last_fault_gprs() -> [u64; 16] {
    match fault_slot() {
        Some(cpu) => core::array::from_fn(|i| FAULT_GPRS[cpu][i].load(Relaxed)),
        None => [0; 16],
    }
}

/// [diag] The CPU exception a panic is about to report, so the panic handler
/// can repeat it *after* the backtrace.
///
/// Packed as `vec << 32 | error_code` with bit 63 as "armed", alongside the
/// faulting RIP. Zero means nothing is armed, which is also what an ordinary
/// `panic!()` leaves behind.
///
/// Why a stash and not just a longer panic message: on a real machine the only
/// artifact that comes back is a photo of the framebuffer, and the early
/// framebuffer console does not scroll — it clears the screen and restarts at
/// the top when it fills (`early_fb_console::newline`). The panic handler
/// prints the message and then up to 32 backtrace lines, so on a 25-line
/// display a deep backtrace wipes the message off the glass before the
/// operator ever sees it. Whatever has to survive must therefore be printed
/// last, which is after the backtrace, which is somewhere the trap frame no
/// longer exists.
static EXCEPTION_SUMMARY: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];
static EXCEPTION_RIP: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];

const EXCEPTION_ARMED: u64 = 1 << 63;

/// [diag] Arm the one-line exception summary for the panic that is about to
/// happen on this cpu. Call immediately before `panic!`.
pub fn note_exception(vec: usize, error_code: usize, rip: u64) {
    if let Some(cpu) = fault_slot() {
        EXCEPTION_RIP[cpu].store(rip, Relaxed);
        EXCEPTION_SUMMARY[cpu].store(
            EXCEPTION_ARMED
                | ((vec as u64 & 0x7fff_ffff) << 32)
                | (error_code as u64 & 0xffff_ffff),
            Relaxed,
        );
    }
}

/// [diag] Take the armed exception summary, leaving nothing behind.
///
/// Taking rather than peeking is what keeps a later, unrelated `panic!()` from
/// being decorated with the last exception this cpu happened to survive.
pub fn take_exception() -> Option<(usize, usize, u64)> {
    let cpu = fault_slot()?;
    let packed = EXCEPTION_SUMMARY[cpu].swap(0, Relaxed);
    if packed & EXCEPTION_ARMED == 0 {
        return None;
    }
    Some((
        ((packed >> 32) & 0x7fff_ffff) as usize,
        (packed & 0xffff_ffff) as usize,
        EXCEPTION_RIP[cpu].load(Relaxed),
    ))
}

/// [diag] Read back the last page-fault RIP recorded by `note_fault_rip`.
pub fn last_fault_rip() -> u64 {
    fault_slot().map_or(0, |cpu| FAULT_RIP[cpu].load(Relaxed))
}

/// [diag] Read back the frame pointer at the last page fault.
pub fn last_fault_rbp() -> u64 {
    fault_slot().map_or(0, |cpu| FAULT_RBP[cpu].load(Relaxed))
}

/// [diag] Read back the stack pointer at the last page fault.
pub fn last_fault_rsp() -> u64 {
    fault_slot().map_or(0, |cpu| FAULT_RSP[cpu].load(Relaxed))
}

/// Whether the last stashed fault was taken in ring 0.
///
/// `cs == 0` means nothing was stored (the arrays start at zero): treat that
/// as *not* kernel, so a libos path that never notes CS still demand-pages.
pub fn last_fault_from_kernel() -> bool {
    let cs = fault_slot().map_or(0, |cpu| FAULT_CS[cpu].load(Relaxed));
    cs != 0 && (cs & 0b11) == 0
}

/// [diag] Broadcast an NMI to all other CPUs and busy-wait briefly so their NMI
/// handlers record their current RIP via `note_nmi_rip`. Call immediately before
/// `nmi_rips()`. No-op off bare x86_64.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn capture_cpu_rips() {
    zcore_drivers::irq::x86::Apic::send_nmi_all_others();
    let start = crate::timer::timer_now();
    while crate::timer::timer_now() < start + core::time::Duration::from_millis(2) {
        // Same reason as every other spin a CPU can reach with interrupts
        // off: while we wait for the peers' NMI handlers we may not be
        // acknowledging shootdowns, and a peer waiting on ours is spending
        // its budget. Draining our own queue here is lock-free,
        // allocation-free queue work, and a no-op when it is empty. See
        // `lock::pump`.
        lock::pump();
        core::hint::spin_loop();
    }
}

/// [diag] No-op stub for non-bare / non-x86_64 targets.
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
pub fn capture_cpu_rips() {}

/// [diag] Read the per-CPU RIPs captured by the last NMI broadcast.
pub fn nmi_rips() -> Vec<(u16, u64)> {
    let mut v = Vec::new();
    for (cpu, slot) in NMI_RIP_PERCPU.iter().enumerate() {
        let rip = slot.load(Relaxed);
        if rip != 0 {
            v.push((cpu as u16, rip));
        }
    }
    v
}

/// [diag] Non-allocating single-CPU read of the RIP captured by the last NMI
/// broadcast. Unlike [`nmi_rips`] this touches no heap, so it is safe to call
/// from the deadlock/panic painter (where the allocator itself may be one of
/// the wedged locks). Returns 0 if that CPU never took the NMI (or is out of
/// range). Pair with [`capture_cpu_rips`], which must run first.
pub fn nmi_rip(cpu: usize) -> u64 {
    if cpu < MAX_CORE_NUM {
        NMI_RIP_PERCPU[cpu].load(Relaxed)
    } else {
        0
    }
}

/// Account one hardware interrupt on `vector`.
pub fn note_irq(vector: usize) {
    IRQ_TOTAL.fetch_add(1, Relaxed);
    if vector < NVEC {
        IRQ_COUNTS[vector].fetch_add(1, Relaxed);
    }
}

/// A consistent-enough snapshot of the counters for rendering.
pub struct KStats {
    /// Total wall-clock all CPUs spent halted (summed across CPUs).
    pub idle_ns: u64,
    /// Number of idle naps entered.
    pub idle_entries: u64,
    /// Timer ticks handled.
    pub timer_ticks: u64,
    /// LAPIC deadline re-arms (timer pulled in ahead of the scheduler tick).
    pub timer_rearms: u64,
    /// Total interrupts handled.
    pub irq_total: u64,
    /// Idle-callback invocations.
    pub idle_cb_total: u64,
    /// Idle-callback invocations that found deferred work (kept the CPU awake).
    pub idle_cb_busy: u64,
    /// `(vector, count)` for every vector that fired at least once.
    pub irqs: Vec<(u16, u64)>,
    /// [diag] Per-CPU `(nap_count, total_nap_ns)` for cores that napped at least
    /// once, indexed by dense logical CPU id.
    pub idle_percpu: Vec<(u16, u64, u64)>,
    /// [diag] xHCI HID polls issued from the timer tick.
    pub hid_poll_timer: u64,
    /// [diag] xHCI HID polls issued from I/O-wait loops.
    pub hid_poll_iowait: u64,
    /// [diag] Longest gap between two consecutive ticks on a busy CPU (ns).
    pub tick_gap_max_ns: u64,
    /// [diag] Tick gaps beyond three nominal periods on a busy CPU, and the
    /// last one's size (ns) and monotonic time (ns).
    pub tick_gaps_late: u64,
    pub tick_gap_last_late_ns: u64,
    pub tick_gap_last_late_at_ns: u64,
    /// [diag] Late gaps whose tick interrupted the idle halt (informational).
    pub tick_gaps_late_idle: u64,
    /// [diag] Per-CPU `(total_ticks, user_ticks, last_rip)` for cores that took
    /// at least one tick, indexed by dense logical CPU id. `user/total` localises
    /// a pegged core's busy time to ring 3 (user thread) vs ring 0 (kernel);
    /// `last_rip` is frozen at the spin entry for a wedged (no-tick) core.
    pub tick_percpu: Vec<(u16, u64, u64, u64)>,
}

/// Read the current counters.
pub fn snapshot() -> KStats {
    let mut irqs = Vec::new();
    for (v, c) in IRQ_COUNTS.iter().enumerate() {
        let n = c.load(Relaxed);
        if n != 0 {
            irqs.push((v as u16, n));
        }
    }
    let mut idle_percpu = Vec::new();
    for cpu in 0..MAX_CORE_NUM {
        let n = IDLE_ENTRIES_PERCPU[cpu].load(Relaxed);
        if n != 0 {
            idle_percpu.push((cpu as u16, n, IDLE_NS_PERCPU[cpu].load(Relaxed)));
        }
    }
    let mut tick_percpu = Vec::new();
    for cpu in 0..MAX_CORE_NUM {
        let t = TICK_TOTAL_PERCPU[cpu].load(Relaxed);
        if t != 0 {
            tick_percpu.push((
                cpu as u16,
                t,
                TICK_USER_PERCPU[cpu].load(Relaxed),
                TICK_LAST_RIP_PERCPU[cpu].load(Relaxed),
            ));
        }
    }
    KStats {
        idle_ns: IDLE_NS.load(Relaxed),
        idle_entries: IDLE_ENTRIES.load(Relaxed),
        timer_ticks: TIMER_TICKS.load(Relaxed),
        timer_rearms: TIMER_REARMS.load(Relaxed),
        irq_total: IRQ_TOTAL.load(Relaxed),
        idle_cb_total: IDLE_CB_TOTAL.load(Relaxed),
        idle_cb_busy: IDLE_CB_BUSY.load(Relaxed),
        irqs,
        idle_percpu,
        hid_poll_timer: HID_POLL_TIMER.load(Relaxed),
        hid_poll_iowait: HID_POLL_IOWAIT.load(Relaxed),
        tick_gap_max_ns: TICK_GAP_MAX_NS.load(Relaxed),
        tick_gaps_late: TICK_GAPS_LATE.load(Relaxed),
        tick_gap_last_late_ns: TICK_GAP_LAST_LATE_NS.load(Relaxed),
        tick_gap_last_late_at_ns: TICK_GAP_LAST_LATE_AT_NS.load(Relaxed),
        tick_gaps_late_idle: TICK_GAPS_LATE_IDLE.load(Relaxed),
        tick_percpu,
    }
}

#[cfg(test)]
mod tests {
    //! Host tests for the CPU runtime-statistics counters.
    //!
    //! **Which slot a `note_*` lands in is not knowable here.** These tests
    //! used to assume `cpu::cpu_id()` is 0 on the host and assert on
    //! `*_percpu` entry 0; it is not, in either host configuration, so both
    //! per-CPU tests failed the moment the suite was actually run. Under
    //! `libos` the id is the host *thread* id truncated to `u8`
    //! (`libos/cpu.rs`), which differs per test thread; on a bare-metal build
    //! compiled for the host it is `lock::current_cpu_id()`, i.e. whichever
    //! physical core the OS scheduler happened to pick — and the thread may
    //! migrate between two consecutive calls. So every per-CPU assertion here
    //! sums over all slots, which is migration-proof, and the slot-indexed
    //! reads go through [`current_slot`].
    //!
    //! All counters here are process-global monotonic atomics and the test
    //! runner executes tests in parallel, so the assertions are written to be
    //! interference-proof: monotonic counters use `>=` deltas, per-vector IRQ
    //! checks each pick a vector no other test touches (exact delta), and the
    //! few tests that read non-monotonic shared state (the per-CPU idle flag,
    //! the last-RIP slot) serialise through `SERIAL`.
    use super::*;
    use spin::Mutex;

    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn handled_fault_registers_are_not_current_fault_provenance() {
        let _guard = SERIAL.lock();
        assert_eq!(current_fault_rsp(), None);
        {
            let _fault = note_fault_regs(0x1000, 0x2000, 0x3000, 8);
            assert_eq!(current_fault_rsp(), Some(0x3000));
        }
        assert_eq!(last_fault_rsp(), 0x3000);
        assert_eq!(current_fault_rsp(), None);
    }

    #[test]
    fn nested_faults_restore_provenance_and_user_faults_do_not_authorize_it() {
        let _guard = SERIAL.lock();
        let outer = note_fault_regs(0x1000, 0x2000, 0x3000, 8);
        {
            let _inner = note_fault_regs(0x4000, 0x5000, 0x6000, 8);
            assert_eq!(current_fault_rsp(), Some(0x6000));
        }
        assert_eq!(current_fault_rsp(), Some(0x3000));
        {
            let _user = note_fault_regs(0x4000, 0x5000, 0x6000, 0x23);
            assert_eq!(current_fault_rsp(), None);
        }
        assert_eq!(current_fault_rsp(), Some(0x3000));
        drop(outer);
        assert_eq!(current_fault_rsp(), None);
    }

    #[test]
    fn consuming_exception_summary_does_not_consume_active_trap_provenance() {
        let _guard = SERIAL.lock();
        let _fault = note_fault_regs(0x1000, 0x2000, 0x3000, 8);
        note_exception(13, 0, 0x1000);
        assert_eq!(take_exception(), Some((13, 0, 0x1000)));
        assert_eq!(take_exception(), None);
        assert_eq!(current_fault_rsp(), Some(0x3000));
    }

    #[test]
    fn abandoning_the_chain_discards_all_fault_provenance() {
        let _guard = SERIAL.lock();
        let outer = note_fault_regs(0x1000, 0x2000, 0x3000, 8);
        let inner = note_fault_regs(0x4000, 0x5000, 0x6000, 8);
        // Model the nonreturning switch: neither scope's destructor will run.
        core::mem::forget(inner);
        core::mem::forget(outer);
        unsafe { abandon_fault_scopes() };
        assert_eq!(current_fault_rsp(), None);
        let subsequent = note_fault_regs(0x7000, 0x8000, 0x9000, 8);
        assert_eq!(current_fault_rsp(), Some(0x9000));
        drop(subsequent);
        assert_eq!(current_fault_rsp(), None);
    }

    /// The per-CPU slot the calling thread's `note_*` calls land in, or `None`
    /// when this host's id is past the table (the `note_*` helpers check the
    /// bound instead of indexing blindly, so those updates are dropped).
    fn current_slot() -> Option<usize> {
        let cpu = crate::cpu::cpu_id() as usize;
        (cpu < MAX_CORE_NUM).then_some(cpu)
    }

    /// Total per-CPU idle entries summed over every slot.
    fn idle_entries_all(s: &KStats) -> u64 {
        s.idle_percpu.iter().map(|(_, n, _)| *n).sum()
    }

    /// `(total ticks, user ticks)` summed over every per-CPU slot.
    fn ticks_all(s: &KStats) -> (u64, u64) {
        s.tick_percpu
            .iter()
            .fold((0, 0), |(t, u), (_, tt, uu, _)| (t + tt, u + uu))
    }

    fn irq_count(snap: &KStats, vector: u16) -> u64 {
        snap.irqs
            .iter()
            .find(|(v, _)| *v == vector)
            .map(|(_, c)| *c)
            .unwrap_or(0)
    }

    #[test]
    fn note_irq_counts_specific_vector() {
        // Vector 0xF1 is used by no other test, so the delta is exactly ours.
        const V: u16 = 0xF1;
        let before = irq_count(&snapshot(), V);
        let total_before = snapshot().irq_total;
        for _ in 0..5 {
            note_irq(V as usize);
        }
        assert_eq!(irq_count(&snapshot(), V) - before, 5);
        assert!(snapshot().irq_total >= total_before + 5);
    }

    #[test]
    fn note_irq_out_of_range_still_counts_total() {
        // A vector beyond the tracked table bumps the grand total but no slot.
        let total_before = snapshot().irq_total;
        note_irq(NVEC + 10);
        assert!(snapshot().irq_total >= total_before + 1);
        // It must not have created a per-vector entry.
        assert!(snapshot().irqs.iter().all(|(v, _)| (*v as usize) < NVEC));
    }

    #[test]
    fn idle_accounting_is_monotonic() {
        let in_range = current_slot().is_some();
        let before = snapshot();
        for _ in 0..4 {
            note_idle(1000);
        }
        let after = snapshot();
        assert!(after.idle_ns >= before.idle_ns + 4000);
        assert!(after.idle_entries >= before.idle_entries + 4);
        // The per-CPU breakdown must have grown by the same four entries --
        // summed over all slots, because the calling thread may migrate
        // between two `note_idle` calls and land in two different ones.
        if in_range {
            assert!(idle_entries_all(&after) >= idle_entries_all(&before) + 4);
        }
        // Whatever the slot, the per-CPU breakdown can never claim more idle
        // naps than the global counter did.
        assert!(idle_entries_all(&after) <= after.idle_entries);
    }

    #[test]
    fn timer_ticks_monotonic() {
        let before = snapshot().timer_ticks;
        for _ in 0..7 {
            note_timer_tick();
        }
        assert!(snapshot().timer_ticks >= before + 7);
    }

    #[test]
    fn hid_poll_counters_monotonic() {
        let before = snapshot();
        note_hid_poll_timer();
        note_hid_poll_timer();
        note_hid_poll_iowait();
        let after = snapshot();
        assert!(after.hid_poll_timer >= before.hid_poll_timer + 2);
        assert!(after.hid_poll_iowait >= before.hid_poll_iowait + 1);
    }

    #[test]
    fn idle_callback_busy_never_exceeds_total() {
        let before = snapshot();
        note_idle_callback(true); // found work
        note_idle_callback(false); // went to sleep
        note_idle_callback(false);
        let after = snapshot();
        assert!(after.idle_cb_total >= before.idle_cb_total + 3);
        assert!(after.idle_cb_busy >= before.idle_cb_busy + 1);
        // Structural invariant: busy is a subset of total, always.
        assert!(after.idle_cb_busy <= after.idle_cb_total);
    }

    #[test]
    fn snapshot_structural_invariants() {
        // Make sure there is some data to inspect.
        note_irq(0x42);
        note_idle(500);
        let s = snapshot();
        // Every reported vector fired at least once.
        assert!(s.irqs.iter().all(|(_, c)| *c > 0));
        // The grand total is at least the sum of the per-vector counts (the
        // total also includes out-of-range vectors).
        let per_vec_sum: u64 = s.irqs.iter().map(|(_, c)| *c).sum();
        assert!(s.irq_total >= per_vec_sum);
        // Idle naps imply idle time and vice-versa are both accounted.
        assert!(s.idle_entries > 0 && s.idle_ns > 0);
        // Per-CPU idle entries never exceed the global count.
        let percpu_entries: u64 = s.idle_percpu.iter().map(|(_, n, _)| *n).sum();
        assert!(percpu_entries <= s.idle_entries);
    }

    #[test]
    fn cpu_time_jiffies_reflect_noted_ns() {
        // CPU 9 is touched by no other test in this file, so the delta below
        // is exact rather than merely a lower bound.
        const CPU: usize = 9;
        const NS_PER_JIFFY: u64 = 10_000_000;
        let (u0, s0, _) = cpu_times_jiffies(CPU);
        note_user_time(CPU, 3 * NS_PER_JIFFY);
        note_sys_time(CPU, 5 * NS_PER_JIFFY);
        let (u1, s1, _) = cpu_times_jiffies(CPU);
        assert_eq!(u1 - u0, 3);
        assert_eq!(s1 - s0, 5);
    }

    #[test]
    fn cpu_time_jiffies_out_of_range_is_zero() {
        assert_eq!(cpu_times_jiffies(MAX_CORE_NUM + 1), (0, 0, 0));
        // Out-of-range notes must not panic (checked, not indexed blindly).
        note_user_time(MAX_CORE_NUM + 1, 1);
        note_sys_time(MAX_CORE_NUM + 1, 1);
    }

    #[test]
    fn cpu_idle_flag_roundtrip() {
        let _g = SERIAL.lock();
        // SERIAL keeps the other flag-touching test out, but the slot this
        // thread marks is whichever `cpu_id()` names right now, so assert on
        // the delta rather than on an absolute count of one. And under libos
        // that id is the host thread id truncated to a byte, so whether it
        // names a slot at all depends on how many threads the harness has
        // started before this one -- that is, on how many tests the crate
        // happens to have. Guarded the same way as `tick_context_records_*`,
        // which this one was missing.
        let Some(_slot) = current_slot() else {
            // Out-of-range ids are checked, not indexed: nothing to observe.
            let before = cpus_idle_now();
            set_cpu_idle(true);
            assert_eq!(cpus_idle_now(), before);
            return;
        };
        set_cpu_idle(false);
        let base = cpus_idle_now();
        set_cpu_idle(true);
        assert_eq!(cpus_idle_now(), base + 1);
        set_cpu_idle(false);
        assert_eq!(cpus_idle_now(), base);
    }

    #[test]
    fn tick_context_records_user_and_rip() {
        let _g = SERIAL.lock();
        let Some(_slot) = current_slot() else {
            // Out-of-range ids are checked, not indexed: nothing to observe.
            let before = snapshot();
            note_tick_context(true, 0xdead_beef);
            assert_eq!(ticks_all(&snapshot()), ticks_all(&before));
            return;
        };
        let before = snapshot();
        note_tick_context(true, 0xdead_beef); // interrupted user mode
        note_tick_context(false, 0xc0ff_ee00); // interrupted kernel mode
        let after = snapshot();
        // Two more ticks, exactly one of them in user mode. Summed over all
        // slots: the two calls may have run on two different CPUs.
        let (t0, u0) = ticks_all(&before);
        let (t1, u1) = ticks_all(&after);
        assert!(t1 >= t0 + 2);
        assert!(u1 >= u0 + 1);
        // User ticks are a subset of total ticks, in every slot.
        assert!(after.tick_percpu.iter().all(|(_, t, u, _)| u <= t));
        // The last recorded RIP is the most recent call's (kernel-mode one),
        // in whichever slot that call landed.
        assert!(after
            .tick_percpu
            .iter()
            .any(|(_, _, _, rip)| *rip == 0xc0ff_ee00));
    }

    // ── the gap between two ticks ────────────────────────────────────────

    /// The tick period this kernel programs, in ns.
    const NOMINAL: u64 = 1_000_000;

    /// What one `note_tick_gap` recorded.
    #[derive(Debug, PartialEq, Eq, Default)]
    struct Recorded {
        late: u64,
        late_idle: u64,
        max_ns: u64,
        last_late_ns: u64,
        last_late_at: u64,
    }

    /// Put every slot in the same state, so which one `cpu_id()` names --
    /// and whether the thread moves between two calls -- cannot change the
    /// answer. Callers hold `SERIAL`.
    fn prime(last: u64, idle: bool) {
        for (l, c) in TICK_LAST_NS_PERCPU.iter().zip(CPU_IN_IDLE.iter()) {
            l.store(last, Relaxed);
            c.store(idle, Relaxed);
        }
        for g in TICK_GAP_LAST_BUSY_NS_PERCPU.iter() {
            g.store(u64::MAX, Relaxed);
        }
        TICK_GAP_MAX_NS.store(0, Relaxed);
        TICK_GAP_LAST_LATE_NS.store(0, Relaxed);
        TICK_GAP_LAST_LATE_AT_NS.store(0, Relaxed);
        for (g, r) in TICK_GAP_MAX_NS_PERCPU
            .iter()
            .zip(TICK_GAP_MAX_RIP_PERCPU.iter())
        {
            g.store(0, Relaxed);
            r.store(0, Relaxed);
        }
    }

    /// What `last_busy_tick_gap_ns` reports after one `note_tick_gap`, read
    /// from every slot so a thread that migrated cannot change the answer.
    /// `prime` seeds `u64::MAX`, which no gap here produces, so an untouched
    /// slot is distinguishable from one that was written with 0.
    fn busy_gap_written() -> u64 {
        let seen: alloc::vec::Vec<u64> = TICK_GAP_LAST_BUSY_NS_PERCPU
            .iter()
            .map(|g| g.load(Relaxed))
            .filter(|g| *g != u64::MAX)
            .collect();
        assert_eq!(seen.len(), 1, "exactamente un slot tuvo que escribirse");
        seen[0]
    }

    /// The address travels with the maximum, not with the latest tick. A gap
    /// that did not win the maximum must not overwrite the address of the one
    /// that did, or the number and the place stop describing the same event --
    /// and the place is the whole point: on this kernel a multi-second gap on a
    /// busy CPU is a multi-second interrupts-off critical section.
    #[test]
    fn the_address_belongs_to_the_worst_gap_not_the_last_one() {
        let _g = SERIAL.lock();
        prime(1_000_000, false);
        let slot = current_slot().expect("este host tiene slot");

        // El hueco gordo, con su direccion.
        TICK_LAST_RIP_PERCPU[slot].store(0xffff_8000_dead_beef, Relaxed);
        note_tick_gap(1_000_000 + 50 * NOMINAL, NOMINAL);
        assert_eq!(tick_gap_max_rip().1, 0xffff_8000_dead_beef);

        // Uno mas corto despues: ni toca el maximo ni toca la direccion.
        TICK_LAST_RIP_PERCPU[slot].store(0xffff_8000_0000_0001, Relaxed);
        note_tick_gap(1_000_000 + 50 * NOMINAL + 10 * NOMINAL, NOMINAL);
        assert_eq!(
            tick_gap_max_rip().1,
            0xffff_8000_dead_beef,
            "un hueco menor se llevo la direccion del mayor"
        );
        // Y el par sale junto: el hueco devuelto es el del mismo tick.
        assert_eq!(
            tick_gap_max_rip().0,
            50 * NOMINAL,
            "el hueco y la direccion tienen que ser del mismo tick"
        );

        // Entre CPUs: el par sale de la ranura del maximo, no de la ultima que
        // tenga algo escrito. El señuelo va DESPUES del ganador a proposito --
        // con el ganador el ultimo, cualquier lectura que arrastre la direccion
        // por su cuenta daria la respuesta correcta por casualidad.
        for (g, r) in TICK_GAP_MAX_NS_PERCPU
            .iter()
            .zip(TICK_GAP_MAX_RIP_PERCPU.iter())
        {
            g.store(0, Relaxed);
            r.store(0, Relaxed);
        }
        TICK_GAP_MAX_NS_PERCPU[1].store(900 * NOMINAL, Relaxed);
        TICK_GAP_MAX_RIP_PERCPU[1].store(0xffff_8000_cafe_0000, Relaxed);
        TICK_GAP_MAX_NS_PERCPU[2].store(5 * NOMINAL, Relaxed);
        TICK_GAP_MAX_RIP_PERCPU[2].store(0xffff_8000_0bad_0bad, Relaxed);
        assert_eq!(
            tick_gap_max_rip(),
            (900 * NOMINAL, 0xffff_8000_cafe_0000),
            "la direccion tiene que venir de la ranura del maximo"
        );

        prime(1_000_000, false);
        for r in TICK_LAST_RIP_PERCPU.iter() {
            r.store(0, Relaxed);
        }
        for (g, r) in TICK_GAP_MAX_NS_PERCPU
            .iter()
            .zip(TICK_GAP_MAX_RIP_PERCPU.iter())
        {
            g.store(0, Relaxed);
            r.store(0, Relaxed);
        }
    }

    /// A busy CPU records the gap, so the slice accounting can tell a tick that
    /// did not fire from a thread that did not run. Without this it credits a
    /// thread that held the CPU the whole time, which is the opposite of the
    /// preemption its slice had earned.
    #[test]
    fn a_busy_cpu_records_its_gap_and_a_halted_one_records_none() {
        let _g = SERIAL.lock();
        let gap = 15 * NOMINAL;
        tick_gap(1_000_000, 1_000_000 + gap, NOMINAL, false);
        assert_eq!(busy_gap_written(), gap, "una CPU ocupada apunta su hueco");
        // Una CPU parada no corria a nadie, asi que su hueco no explica la
        // ausencia de ningun hilo: no hay nada que restar.
        tick_gap(1_000_000, 1_000_000 + gap, NOMINAL, true);
        assert_eq!(
            busy_gap_written(),
            0,
            "una CPU parada no puede explicar el tiempo que un hilo no corrio"
        );
    }

    /// Drive `note_tick_gap` once from a known state and report what it wrote.
    fn tick_gap(last: u64, now_ns: u64, nominal_ns: u64, idle: bool) -> Recorded {
        prime(last, idle);
        let b = snapshot();
        note_tick_gap(now_ns, nominal_ns);
        let a = snapshot();
        for c in CPU_IN_IDLE.iter() {
            c.store(false, Relaxed);
        }
        Recorded {
            late: a.tick_gaps_late - b.tick_gaps_late,
            late_idle: a.tick_gaps_late_idle - b.tick_gaps_late_idle,
            max_ns: a.tick_gap_max_ns,
            last_late_ns: a.tick_gap_last_late_ns,
            last_late_at: a.tick_gap_last_late_at_ns,
        }
    }

    #[test]
    fn the_first_tick_on_a_cpu_has_no_gap_to_report() {
        let _g = SERIAL.lock();
        // A zero stamp means this CPU has not ticked yet. Subtracted from all
        // the same, it reports the whole monotonic clock as one gap: every
        // core would look stalled since boot on its very first tick, and the
        // longest-gap figure would never recover.
        assert_eq!(
            tick_gap(0, 900 * NOMINAL, NOMINAL, false),
            Recorded::default(),
            "el primer tic de una cpu se conto como un hueco"
        );
    }

    #[test]
    fn a_clock_that_did_not_move_forward_reports_no_gap() {
        let _g = SERIAL.lock();
        // Two ticks stamped with the same nanosecond, and a reading that came
        // back behind the last one -- which is what a TSC that boot never
        // synchronised hands this path. `now - last` would wrap and report
        // some eighteen quintillion nanoseconds, the largest number the
        // diagnostic can hold and not a stall.
        assert_eq!(tick_gap(5_000, 5_000, NOMINAL, false), Recorded::default());
        assert_eq!(tick_gap(5_000, 4_999, NOMINAL, false), Recorded::default());
    }

    #[test]
    fn a_tick_leaves_its_own_stamp_for_the_next_one_to_measure_from() {
        let _g = SERIAL.lock();
        // Each gap is measured against the previous tick, so every tick has
        // to leave its own time behind. Measured against a stamp that never
        // moves, the gaps grow without anything stalling and the diagnostic
        // reports a machine that is getting worse by the minute.
        prime(1_000, false);
        note_tick_gap(2_000, NOMINAL);
        TICK_GAP_MAX_NS.store(0, Relaxed);
        note_tick_gap(3_000, NOMINAL);
        assert_eq!(
            TICK_GAP_MAX_NS.load(Relaxed),
            1_000,
            "el hueco se midio desde una marca que ya no valia"
        );
    }

    #[test]
    fn three_nominal_periods_is_not_late_yet_and_one_nanosecond_more_is() {
        let _g = SERIAL.lock();
        let base = 1_000_000_000u64;
        // Three periods is the threshold the module names, and a threshold is
        // a threshold.
        let r = tick_gap(base, base + 3 * NOMINAL, NOMINAL, false);
        assert_eq!(r.late, 0, "tres periodos justos se contaron como tarde");
        assert_eq!(r.max_ns, 3 * NOMINAL, "y el hueco no llego al maximo");
        // One nanosecond past it.
        assert_eq!(
            tick_gap(base, base + 3 * NOMINAL + 1, NOMINAL, false).late,
            1
        );
        // Two periods, where a stricter threshold would fire: every ordinary
        // scheduling hiccup lands there, and a counter that counts those
        // stops pointing at anything.
        assert_eq!(tick_gap(base, base + 2 * NOMINAL, NOMINAL, false).late, 0);
    }

    #[test]
    fn a_nominal_period_near_the_top_of_the_range_does_not_wrap_into_lateness() {
        let _g = SERIAL.lock();
        // Three times a period that is already a third of the range does not
        // fit in one. Wrapped instead of saturated the threshold comes out at
        // two nanoseconds, so every tick is late from the first one on and
        // the stall counter is pinned for the rest of the boot.
        let huge = u64::MAX / 3 + 1;
        assert_eq!(
            tick_gap(1, 1 + 4 * NOMINAL, huge, false).late,
            0,
            "un periodo enorme hizo tarde a todos los tics"
        );
    }

    #[test]
    fn a_late_tick_that_interrupted_the_halt_is_counted_apart() {
        let _g = SERIAL.lock();
        // A halted vCPU the host wakes late harms nobody: nothing was waiting
        // on it, and on a lightly loaded machine that is most late ticks.
        // Counted together with the stalls that did hurt, they bury them.
        let base = 1_000_000_000u64;
        let r = tick_gap(base, base + 100 * NOMINAL, NOMINAL, true);
        assert_eq!(
            r.late_idle, 1,
            "el tic tarde sobre el halt no se conto aparte"
        );
        assert_eq!(r.late, 0, "y ademas se conto como un atasco de verdad");
        assert_eq!(r.max_ns, 0, "el hueco del halt subio el peor hueco");
        // The same gap on a busy CPU is the real thing.
        let r = tick_gap(base, base + 100 * NOMINAL, NOMINAL, false);
        assert_eq!((r.late, r.late_idle), (1, 0));
        assert_eq!(r.max_ns, 100 * NOMINAL);
    }

    #[test]
    fn the_longest_gap_is_the_longest_one_and_not_the_last_one() {
        let _g = SERIAL.lock();
        // It is the headline of the stall diagnostic: the worst this machine
        // went without a tick. Overwritten rather than kept, it reports
        // whatever the most recent tick happened to be, and a machine that
        // stalled once and then behaved reports nothing at all.
        let base = 1_000_000_000u64;
        prime(base, false);
        note_tick_gap(base + 50 * NOMINAL, NOMINAL);
        note_tick_gap(base + 50 * NOMINAL + 1, NOMINAL);
        assert_eq!(
            TICK_GAP_MAX_NS.load(Relaxed),
            50 * NOMINAL,
            "el peor hueco lo piso el tic siguiente"
        );
    }

    #[test]
    fn the_last_late_gap_records_its_size_and_its_time_the_right_way_round() {
        let _g = SERIAL.lock();
        // The pair reads as "a gap of N ns, at time T". The other way round
        // it says the machine stalled for as long as it has been up, a few
        // microseconds after boot.
        let base = 1_000_000_000u64;
        let now = base + 100 * NOMINAL;
        let r = tick_gap(base, now, NOMINAL, false);
        assert_eq!(r.last_late_ns, 100 * NOMINAL, "el tamano del ultimo atasco");
        assert_eq!(r.last_late_at, now, "y el momento en que paso");
    }

    // ── who is halted, and where each core was ───────────────────────────

    #[test]
    fn the_idle_mask_puts_each_cpu_in_its_own_bit() {
        let _g = SERIAL.lock();
        // The TLB-shootdown initiator reads this to decide which cores it may
        // skip waiting on. A bit in the wrong place lets it skip a core that
        // is *running*, whose shootdown then becomes fire-and-forget while it
        // keeps executing against a stale TLB entry.
        for c in CPU_IN_IDLE.iter() {
            c.store(false, Relaxed);
        }
        assert_eq!(cpu_idle_mask(), 0);
        for cpu in [0usize, 1, 7, MAX_CORE_NUM - 1] {
            for c in CPU_IN_IDLE.iter() {
                c.store(false, Relaxed);
            }
            CPU_IN_IDLE[cpu].store(true, Relaxed);
            assert_eq!(cpu_idle_mask(), 1u64 << cpu, "la cpu {} no es su bit", cpu);
            assert_eq!(cpus_idle_now(), 1);
        }
        for c in CPU_IN_IDLE.iter() {
            c.store(false, Relaxed);
        }
    }

    #[test]
    fn the_nmi_rips_name_the_cpus_that_answered_and_leave_out_the_rest() {
        let _g = SERIAL.lock();
        // The deadlock banner reads these to say where each wedged core
        // actually is, and the index is the only thing tying a line to a
        // core. A CPU that never took the NMI has a zero slot and no line;
        // listed anyway it reads as "cpu 5 is at address 0".
        for s in NMI_RIP_PERCPU.iter() {
            s.store(0, Relaxed);
        }
        assert!(nmi_rips().is_empty());
        NMI_RIP_PERCPU[3].store(0xffff_8000_0001_0000, Relaxed);
        NMI_RIP_PERCPU[11].store(0x40_4142, Relaxed);
        assert_eq!(
            nmi_rips(),
            [(3u16, 0xffff_8000_0001_0000u64), (11, 0x40_4142)],
            "la lista no nombra a las cpus que contestaron"
        );
        // The non-allocating single-CPU read, which the panic painter uses
        // because the allocator may be one of the wedged locks, agrees.
        assert_eq!(nmi_rip(3), 0xffff_8000_0001_0000);
        assert_eq!(nmi_rip(4), 0);
        for s in NMI_RIP_PERCPU.iter() {
            s.store(0, Relaxed);
        }
    }

    #[test]
    fn a_tick_that_interrupted_the_kernel_is_not_a_user_tick() {
        let _g = SERIAL.lock();
        // `user/total` is what localises a pegged core to ring 3 or ring 0.
        // With one tick of each kind there is exactly one user tick whichever
        // way round the question is asked, so this takes two of one kind.
        let Some(slot) = current_slot() else {
            return;
        };
        let before = ticks_all(&snapshot());
        note_tick_context(true, 0x1111);
        note_tick_context(true, 0x2222);
        note_tick_context(false, 0x3333);
        let after = ticks_all(&snapshot());
        assert_eq!(after.0 - before.0, 3, "los tres tics");
        assert_eq!(after.1 - before.1, 2, "de los que dos son de usuario");
        // The last one owns the slot, by its number and for this CPU alike.
        assert_eq!(cpu_tick_rip(slot), 0x3333);
        assert_eq!(current_cpu_tick_rip(), 0x3333);
    }

    #[test]
    fn the_cpu_one_past_the_last_is_out_of_range_like_any_other() {
        // `MAX_CORE_NUM` is the number of slots, so the last one is
        // `MAX_CORE_NUM - 1` and every bound here is written `<`. The id
        // exactly at the bound is the one a `<=` would let through, and the
        // tests that were here only tried `MAX_CORE_NUM + 1`, which `<=`
        // turns away too. Reading another core's slot from the deadlock
        // banner is the mix-up the per-CPU split exists to end; indexing off
        // the end of it is a panic inside the panic.
        note_user_time(MAX_CORE_NUM, 1);
        note_sys_time(MAX_CORE_NUM, 1);
        assert_eq!(cpu_times_jiffies(MAX_CORE_NUM), (0, 0, 0));
        assert_eq!(cpu_tick_rip(MAX_CORE_NUM), 0);
        assert_eq!(nmi_rip(MAX_CORE_NUM), 0);
        assert_eq!(nmi_rip(usize::MAX), 0);
    }

    // ── the counters read as rates ───────────────────────────────────────

    #[test]
    fn an_allocation_is_counted_even_when_nobody_is_timing_it() {
        let _g = SERIAL.lock();
        // The count is what attributes allocations to a region of code:
        // sample it either side and the difference is that region's own, with
        // no per-caller plumbing. Kept only while `HEAPPROF=1`, every
        // ordinary boot reports zero allocations.
        let (a0, ac0, d0, dc0) = heap_prof_stats();
        note_heap_alloc(0);
        note_heap_alloc(0);
        note_heap_dealloc(0);
        let (a1, ac1, d1, dc1) = heap_prof_stats();
        assert_eq!(a1 - a0, 2, "las reservas sin cronometrar no se contaron");
        assert_eq!(d1 - d0, 1, "ni las liberaciones");
        assert_eq!((ac1, dc1), (ac0, dc0), "y una medida de cero sumo ciclos");
        // With the profile on, the cycles land where they are named.
        note_heap_alloc(700);
        note_heap_dealloc(11);
        let (_, ac2, _, dc2) = heap_prof_stats();
        assert_eq!(ac2 - ac1, 700, "los ciclos de reservar");
        assert_eq!(dc2 - dc1, 11, "los ciclos de liberar");
    }

    #[test]
    fn idle_callbacks_that_found_no_work_do_not_count_as_busy() {
        // `busy/total` is the rate that says deferred work is what keeps the
        // CPUs awake. Counted the other way round it is the exact inverse,
        // and both tests that were here are `>=` lower bounds that pass
        // either way.
        let before = snapshot();
        for _ in 0..16 {
            note_idle_callback(false);
        }
        let after = snapshot();
        assert!(after.idle_cb_total >= before.idle_cb_total + 16);
        assert!(
            after.idle_cb_busy - before.idle_cb_busy < 16,
            "dieciseis llamadas sin trabajo contaron como ocupadas"
        );
    }

    #[test]
    fn a_nap_lands_in_its_own_cpus_idle_time_and_not_only_in_its_count() {
        // The idle column of `/proc/stat` for one core comes from here. With
        // the nap counted and its nanoseconds dropped, a core that sleeps all
        // day reports a thousand naps and no idle time at all, which reads as
        // a core that is pegged.
        let ns_all = |s: &KStats| -> u64 { s.idle_percpu.iter().map(|(_, _, ns)| *ns).sum() };
        let before = snapshot();
        note_idle(7_000_000);
        let after = snapshot();
        assert!(
            ns_all(&after) >= ns_all(&before) + 7_000_000,
            "la siesta no sumo al tiempo de su cpu"
        );
        assert!(ns_all(&after) <= after.idle_ns);
    }

    #[test]
    fn a_faults_register_file_reads_back_whole_and_the_next_one_replaces_it() {
        let _guard = SERIAL.lock();
        // Every slot distinct, so a swapped pair or a short copy cannot pass.
        let regs: [u64; 16] = core::array::from_fn(|i| 0xffff_ff00_0000_1000 + 8 * i as u64);
        note_fault_gprs(&regs);
        assert_eq!(last_fault_gprs(), regs);
        // Replaced whole by the next fault on this CPU: a register left over
        // from an earlier fault would be read as this one's operand.
        let next: [u64; 16] = core::array::from_fn(|i| i as u64 + 1);
        note_fault_gprs(&next);
        assert_eq!(last_fault_gprs(), next);
    }
}

// ---------------------------------------------------------------------------
// Futex path
// ---------------------------------------------------------------------------
//
// One cross-CPU wake costs 47-60 us here against Linux's 26 us under the same
// emulator (`eclipse-bench --only psched`, the `emision de 1 despertar` row),
// and a burst is *cheaper* per wake than a single one, so the cost is not in
// the scheduler's hand-off -- it is in what one `FUTEX_WAKE` syscall does
// before it reaches the queue. Reading the code gives candidates and no
// weights, and the only machine that can weigh them is Moebius's, so the
// candidates are counted rather than guessed at.
//
// The one that reading points at: a futex without `FUTEX_PRIVATE_FLAG` makes
// `sys_futex` go looking for a word two processes share, and that search walks
// the VMAR -- a second descent after the one the bounds check already did, and
// a third lock to read the mapping's VMO -- only to give up on a private
// mapping, which is what almost every futex word is. musl's own pthreads do
// set the private flag, so the bench's raw `op = 1` pays a cost most real
// programs do not: these counters are what will say whether that is the whole
// gap, part of it, or none of it.
//
// Per-CPU and summed on read, like the idle and tick counters above: a shared
// counter on a path this hot is itself a cache line bouncing between cores on
// every `pthread_mutex_unlock`.

/// Every `sys_futex` whose operation was understood.
static FUTEX_OPS: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];
/// …of those, the ones that went looking for a cross-process word.
static FUTEX_SHARED_PROBES: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];
/// …of those probes, the ones where the word really was shared.
static FUTEX_SHARED_HITS: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];
/// VMAR descents the futex path performed, bounds check included.
static FUTEX_VMAR_WALKS: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];

/// Add `n` to this CPU's slot of a per-CPU counter, or drop it if the CPU id is
/// outside the array.
///
/// Dropping rather than folding into slot 0 is deliberate: slot 0 is a real
/// CPU's slot, and a CPU the array cannot hold is a machine wider than
/// `MAX_CORE_NUM`, where a counter that quietly attributes its work to CPU 0
/// would be worse than one that is short by it.
#[inline]
fn bump_percpu(c: &[AtomicU64; MAX_CORE_NUM], n: u64) {
    let cpu = crate::cpu::cpu_id() as usize;
    if cpu < MAX_CORE_NUM {
        c[cpu].fetch_add(n, Relaxed);
    }
}

/// Sum a per-CPU counter.
fn sum_percpu(c: &[AtomicU64; MAX_CORE_NUM]) -> u64 {
    c.iter().map(|v| v.load(Relaxed)).sum()
}

/// Account one `sys_futex` whose operation was understood.
pub fn note_futex_op() {
    bump_percpu(&FUTEX_OPS, 1);
}

/// Account one search for a cross-process futex word, and how many VMAR
/// descents it cost, and whether the word really was shared.
pub fn note_futex_shared_probe(walks: u64, hit: bool) {
    bump_percpu(&FUTEX_SHARED_PROBES, 1);
    bump_percpu(&FUTEX_VMAR_WALKS, walks);
    if hit {
        bump_percpu(&FUTEX_SHARED_HITS, 1);
    }
}

/// Account VMAR descents the futex path performed outside a shared-word search.
pub fn note_futex_vmar_walks(walks: u64) {
    bump_percpu(&FUTEX_VMAR_WALKS, walks);
}

/// `(ops, shared probes, shared hits, VMAR descents)` across all CPUs.
pub fn futex_stats() -> (u64, u64, u64, u64) {
    (
        sum_percpu(&FUTEX_OPS),
        sum_percpu(&FUTEX_SHARED_PROBES),
        sum_percpu(&FUTEX_SHARED_HITS),
        sum_percpu(&FUTEX_VMAR_WALKS),
    )
}
