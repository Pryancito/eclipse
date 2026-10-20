//! How often a thread has given its CPU up, the two ways Linux counts it.
//!
//! Nothing counted them: `ru_nvcsw` and `ru_nivcsw` of `getrusage` and
//! `wait4` were always 0, and `/proc/<pid>/status` had no
//! `voluntary_ctxt_switches` or `nonvoluntary_ctxt_switches` line, so
//! `/usr/bin/time -v`, `pidstat -w` and a profiler that tells a thread that
//! waits from one that is crowded off its CPU read nothing.

use core::sync::atomic::{AtomicU64, Ordering};

/// The context switches of one thread, or of a process's exited threads.
///
/// Every poll of a thread that returns `Pending` is one: the thread leaves
/// the CPU and the executor picks again. It is nonvoluntary when the thread
/// left it runnable -- preempted at the end of its slice or by a wake-up, or
/// in `sched_yield` -- and voluntary when it went to wait for something,
/// which is `__schedule`'s line between `nvcsw` and `nivcsw` (a task that
/// leaves in `TASK_RUNNING` did not give the CPU up).
///
/// One difference stays: Linux switches only when another task is waiting
/// for the CPU, and a slice that ends here with nobody else waiting still
/// goes back through the executor, so it counts.
#[derive(Debug, Default)]
pub(crate) struct ContextSwitches {
    voluntary: AtomicU64,
    involuntary: AtomicU64,
}

impl ContextSwitches {
    /// One poll left the CPU, `runnable` or waiting.
    pub(crate) fn note(&self, runnable: bool) {
        let count = if runnable {
            &self.involuntary
        } else {
            &self.voluntary
        };
        count.fetch_add(1, Ordering::Relaxed);
    }

    /// A thread's counts, moved to its process as it leaves.
    pub(crate) fn add(&self, (voluntary, involuntary): (u64, u64)) {
        self.voluntary.fetch_add(voluntary, Ordering::Relaxed);
        self.involuntary.fetch_add(involuntary, Ordering::Relaxed);
    }

    /// `(voluntary, nonvoluntary)`.
    pub(crate) fn get(&self) -> (u64, u64) {
        (
            self.voluntary.load(Ordering::Relaxed),
            self.involuntary.load(Ordering::Relaxed),
        )
    }
}
