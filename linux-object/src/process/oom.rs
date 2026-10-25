//! What a process asks of an OOM killer: `signal->oom_score_adj`, and the
//! floor `oom_score_adj_min` below which only `CAP_SYS_RESOURCE` may take
//! it. `/proc/<pid>/oom_score_adj` and the older `oom_adj` read and write
//! them (`fs/procfs/oom.rs`).
//!
//! There is no OOM killer here to act on it. The value is still what
//! programs set and read: systemd's `OOMScoreAdjust=`, `choom`, a browser
//! ranking its renderers, Android's `lmkd`. It belongs to the process as
//! `signal_struct` does: one value for all its threads, copied by a fork
//! with its floor, and kept across an `execve`.

use super::*;

/// `OOM_SCORE_ADJ_MIN`: a process its OOM killer never picks.
pub const OOM_SCORE_ADJ_MIN: i16 = -1000;
/// `OOM_SCORE_ADJ_MAX`: the one it picks first.
pub const OOM_SCORE_ADJ_MAX: i16 = 1000;

/// A process's adjustment and its floor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OomAdj {
    /// `oom_score_adj`, in `OOM_SCORE_ADJ_MIN..=OOM_SCORE_ADJ_MAX`.
    pub adj: i16,
    /// `oom_score_adj_min`: the lowest a writer without `CAP_SYS_RESOURCE`
    /// may set it to.
    pub min: i16,
}

impl OomAdj {
    /// `__set_oom_adj`: move the adjustment to `adj`, already in range, or
    /// `false` (`EACCES`) when the writer may not lower it that far.
    ///
    /// A write to `oom_score_adj` may go down to the floor without
    /// `CAP_SYS_RESOURCE`, and a writer with it sets the floor too. The
    /// legacy `oom_adj` (`legacy`) may not lower the value at all without
    /// it, and leaves the floor alone.
    pub fn set(&mut self, adj: i16, legacy: bool, capable: bool) -> bool {
        let floor = if legacy { self.adj } else { self.min };
        if adj < floor && !capable {
            return false;
        }
        self.adj = adj;
        if !legacy && capable {
            self.min = adj;
        }
        true
    }
}

impl LinuxProcess {
    /// Its OOM adjustment and the floor.
    pub fn oom_adj(&self) -> OomAdj {
        self.inner.lock().oom
    }

    /// [`OomAdj::set`] on its own, under the lock Linux's `oom_adj_mutex`
    /// stands for.
    pub fn set_oom_adj(&self, adj: i16, legacy: bool, capable: bool) -> bool {
        self.inner.lock().oom.set(adj, legacy, capable)
    }
}

#[cfg(test)]
mod tests;
