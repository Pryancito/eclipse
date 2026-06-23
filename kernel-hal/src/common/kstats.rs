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
static IDLE_ENTRIES_PERCPU: [AtomicU64; MAX_CORE_NUM] =
    [const { AtomicU64::new(0) }; MAX_CORE_NUM];

/// [diag] xHCI HID poll invocations split by the path that issued them: the
/// timer tick (`timer`) vs an I/O-wait loop (`iowait`). Keyboard/mouse input is
/// delivered from these polls, so their combined rate IS the input
/// responsiveness. When the CPUs halt and the net busy-spin is gone, `iowait`
/// drops to ~0 and only `timer` keeps input alive — its rate then says whether
/// idle HID polling is fast enough.
static HID_POLL_TIMER: AtomicU64 = AtomicU64::new(0);
static HID_POLL_IOWAIT: AtomicU64 = AtomicU64::new(0);

/// [diag] Account one xHCI HID poll issued from the timer tick.
pub fn note_hid_poll_timer() {
    HID_POLL_TIMER.fetch_add(1, Relaxed);
}

/// [diag] Account one xHCI HID poll issued from an I/O-wait loop.
pub fn note_hid_poll_iowait() {
    HID_POLL_IOWAIT.fetch_add(1, Relaxed);
}
static TIMER_TICKS: AtomicU64 = AtomicU64::new(0);
static IRQ_TOTAL: AtomicU64 = AtomicU64::new(0);
static IRQ_COUNTS: [AtomicU64; NVEC] = [const { AtomicU64::new(0) }; NVEC];
/// Idle-callback invocations and how many found deferred work pending. The
/// scheduler only halts when the callback finds nothing, so a high "busy" ratio
/// here means the idle path keeps finding work and the CPUs never sleep — the
/// signature of a busy-spin (and the heat that comes with it).
static IDLE_CB_TOTAL: AtomicU64 = AtomicU64::new(0);
static IDLE_CB_BUSY: AtomicU64 = AtomicU64::new(0);

/// `(tasks polled, weak-executor yields)` from the scheduler loop — to attribute
/// a busy-spin: a high `polled` rate means a task keeps re-readying itself; a
/// high `weak_yield` rate means the CPUs spin on an outstanding weak executor.
/// (The scheduler only exists on bare metal; libos reports zeros.)
#[cfg(target_os = "none")]
pub fn sched_stats() -> (u64, u64) {
    executor::sched_stats()
}

#[cfg(not(target_os = "none"))]
pub fn sched_stats() -> (u64, u64) {
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

/// Account one timer tick.
pub fn note_timer_tick() {
    TIMER_TICKS.fetch_add(1, Relaxed);
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
static TICK_LAST_RIP_PERCPU: [AtomicU64; MAX_CORE_NUM] = [const { AtomicU64::new(0) }; MAX_CORE_NUM];

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

/// [diag] Broadcast an NMI to all other CPUs and busy-wait briefly so their NMI
/// handlers record their current RIP via `note_nmi_rip`. Call immediately before
/// `nmi_rips()`. No-op off bare x86_64.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn capture_cpu_rips() {
    zcore_drivers::irq::x86::Apic::send_nmi_all_others();
    let start = crate::timer::timer_now();
    while crate::timer::timer_now() < start + core::time::Duration::from_millis(2) {
        core::hint::spin_loop();
    }
}

/// [diag] No-op stub for non-bare / non-x86_64 targets.
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
pub fn capture_cpu_rips() {}

/// [diag] Read the per-CPU RIPs captured by the last NMI broadcast.
pub fn nmi_rips() -> Vec<(u16, u64)> {
    let mut v = Vec::new();
    for cpu in 0..MAX_CORE_NUM {
        let rip = NMI_RIP_PERCPU[cpu].load(Relaxed);
        if rip != 0 {
            v.push((cpu as u16, rip));
        }
    }
    v
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
        irq_total: IRQ_TOTAL.load(Relaxed),
        idle_cb_total: IDLE_CB_TOTAL.load(Relaxed),
        idle_cb_busy: IDLE_CB_BUSY.load(Relaxed),
        irqs,
        idle_percpu,
        hid_poll_timer: HID_POLL_TIMER.load(Relaxed),
        hid_poll_iowait: HID_POLL_IOWAIT.load(Relaxed),
        tick_percpu,
    }
}
