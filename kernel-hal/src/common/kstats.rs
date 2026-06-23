//! Lightweight kernel runtime statistics, surfaced at `/proc/perf/kernel`.
//!
//! Aimed at debugging "why is the machine warm / busy": the headline numbers
//! are **how much wall-clock the CPUs actually spent halted (idle)** vs running,
//! and **the per-vector interrupt counts** (an IRQ storm is the usual culprit
//! behind unexpected heat). Everything here is lock-free atomic counters bumped
//! from the bare-metal idle / IRQ / timer paths; on libos they stay zero.

use crate::config::MAX_CORE_NUM;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

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

/// Account one timer tick.
pub fn note_timer_tick() {
    TIMER_TICKS.fetch_add(1, Relaxed);
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
    }
}
