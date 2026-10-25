//! What the thread that wore a process's id had for the scheduler, kept
//! after that thread has gone.
//!
//! Linux keeps a thread group's leader until `release_task()`: a zombie is
//! that task, and a pid whose leader has exited while other threads run on
//! still names it. `sched_getaffinity`, `sched_getscheduler`,
//! `sched_getparam`, `sched_getattr`, `getpriority` and `ioprio_get` read
//! its mask, policy and nice, and the setters change them, until the reap.
//! Here the leader is a `Thread` that leaves its process's list when it
//! dies, and those calls answered `ESRCH` for every zombie. What they read
//! is kept here when it leaves, five words with nothing behind them, so the
//! process holds no thread and no thread holds it.

use super::*;
use crate::task::SCHED_NORMAL;

/// A task's scheduling attributes in one piece: what `sched_*` and
/// `setpriority` read and set on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedSnapshot {
    /// One of the `SCHED_*` policies.
    pub policy: u8,
    /// The nice value, -20..=19.
    pub nice: i8,
    /// The static real-time priority: 1..=99 under `SCHED_FIFO` or
    /// `SCHED_RR`, 0 otherwise.
    pub rt_priority: u8,
    /// Whether `SCHED_RESET_ON_FORK` is set.
    pub reset_on_fork: bool,
    /// The CPU mask: bit `i` set means it may run on CPU `i`.
    pub affinity: u64,
}

impl Default for SchedSnapshot {
    /// What a thread starts with: `SCHED_NORMAL` at nice 0 on every CPU.
    fn default() -> Self {
        SchedSnapshot {
            policy: SCHED_NORMAL,
            nice: 0,
            rt_priority: 0,
            reset_on_fork: false,
            affinity: u64::MAX,
        }
    }
}

impl SchedSnapshot {
    /// What `thread` has now.
    pub fn of(thread: &Thread) -> Self {
        SchedSnapshot {
            policy: thread.sched_policy(),
            nice: thread.sched_nice(),
            rt_priority: thread.sched_rt_priority(),
            reset_on_fork: thread.sched_reset_on_fork(),
            affinity: thread.affinity(),
        }
    }
}

impl Process {
    /// What the thread that wore this process's id had when it left the
    /// thread list, or `None` while it is still there or if it never was.
    pub fn leader_sched(&self) -> Option<SchedSnapshot> {
        *self.leader_sched.lock()
    }

    /// Put `sched` on file as what the gone leader has: for a leader object
    /// kept after it left the list and changed since, when it is let go.
    pub fn set_leader_sched(&self, sched: SchedSnapshot) {
        *self.leader_sched.lock() = Some(sched);
    }

    /// Change what the gone leader has, for a setter aimed at it. `false`
    /// when there is none on file and nothing was changed.
    pub fn update_leader_sched(&self, change: impl FnOnce(&mut SchedSnapshot)) -> bool {
        match self.leader_sched.lock().as_mut() {
            Some(sched) => {
                change(sched);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests;
