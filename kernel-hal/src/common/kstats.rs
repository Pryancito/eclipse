//! Lightweight kernel runtime statistics, surfaced at `/proc/perf/kernel`.
//!
//! Aimed at debugging "why is the machine warm / busy": the headline numbers
//! are **how much wall-clock the CPUs actually spent halted (idle)** vs running,
//! and **the per-vector interrupt counts** (an IRQ storm is the usual culprit
//! behind unexpected heat). Everything here is lock-free atomic counters bumped
//! from the bare-metal idle / IRQ / timer paths; on libos they stay zero.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Number of interrupt vectors tracked individually (x86 IDT is 256 wide).
const NVEC: usize = 256;

static IDLE_NS: AtomicU64 = AtomicU64::new(0);
static IDLE_ENTRIES: AtomicU64 = AtomicU64::new(0);
static TIMER_TICKS: AtomicU64 = AtomicU64::new(0);
static IRQ_TOTAL: AtomicU64 = AtomicU64::new(0);
static IRQ_COUNTS: [AtomicU64; NVEC] = [const { AtomicU64::new(0) }; NVEC];

/// Account `ns` of wall-clock spent halted in one idle nap. Called by the
/// per-CPU idle routine around its `hlt`/`mwait`.
pub fn note_idle(ns: u64) {
    IDLE_NS.fetch_add(ns, Relaxed);
    IDLE_ENTRIES.fetch_add(1, Relaxed);
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
    /// `(vector, count)` for every vector that fired at least once.
    pub irqs: Vec<(u16, u64)>,
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
    KStats {
        idle_ns: IDLE_NS.load(Relaxed),
        idle_entries: IDLE_ENTRIES.load(Relaxed),
        timer_ticks: TIMER_TICKS.load(Relaxed),
        irq_total: IRQ_TOTAL.load(Relaxed),
        irqs,
    }
}
