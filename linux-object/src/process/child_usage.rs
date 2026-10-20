//! What a reaped child used, as `wait4` hands it out and the reaper adds it
//! up.
//!
//! It was the child's own CPU time and nothing else. Linux adds the
//! children the child reaped in turn (`wait_task_zombie`: `psig->cutime +=
//! tgutime + sig->cutime`, and `getrusage(p, RUSAGE_BOTH)` for the rusage),
//! so `time sh -c make` here showed the shell's few milliseconds and none of
//! the compilers; and the context switches were not counted at all.

use super::*;

/// What a child had used by the time it exited: what `wait4(2)` reports
/// through its rusage out-parameter and what the parent adds to its
/// `RUSAGE_CHILDREN` totals when it reaps the child. Its own reaped
/// children's are in it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ChildCpu {
    /// User-mode nanoseconds.
    pub utime_ns: u64,
    /// Kernel-mode nanoseconds.
    pub stime_ns: u64,
    /// Context switches made waiting (`ru_nvcsw`).
    pub voluntary_switches: u64,
    /// Context switches made runnable: preempted, or yielding (`ru_nivcsw`).
    pub involuntary_switches: u64,
}

impl ChildCpu {
    /// Add `other` in, as reaping it does.
    pub(super) fn add(&mut self, other: ChildCpu) {
        self.utime_ns += other.utime_ns;
        self.stime_ns += other.stime_ns;
        self.voluntary_switches += other.voluntary_switches;
        self.involuntary_switches += other.involuntary_switches;
    }
}

/// Read an exited child's final usage off its still-live object: its
/// threads' measured time, live ones included, since an `exit_group` makes
/// the child waitable before they are gone (it was the exited threads' user
/// time alone, which was nothing for a child that called `exit`), and what
/// the children it reaped itself had used.
pub(super) fn child_cpu(child: &Arc<Process>) -> ChildCpu {
    let (utime_ns, stime_ns) = child.cpu_time_ns();
    let (voluntary_switches, involuntary_switches) = child.context_switches();
    let mut cpu = ChildCpu {
        utime_ns,
        stime_ns,
        voluntary_switches,
        involuntary_switches,
    };
    if let Some(lp) = child.try_linux() {
        cpu.add(lp.children_usage());
    }
    cpu
}
