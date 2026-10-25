//! How often a thread's access to its own memory has faulted and been
//! resolved without a disk: a page of its address space touched for the
//! first time, or written while it was still shared copy-on-write.
//!
//! Nothing counted them: `ru_minflt` of `getrusage` and `wait4`, and the
//! `minflt` and `cminflt` fields of `/proc/<pid>/stat`, were always 0, so
//! `/usr/bin/time -v` and `ps -o min_flt` read nothing (`fault-survey`, 14
//! checks, 14/14 on Linux 6.18 with glibc and musl, 4/14 here).
//!
//! Every fault the address space resolves is minor here. Linux counts a
//! fault that had to wait for a read from a disk as major, and none of
//! these waits for one that it can tell apart, so `ru_majflt` stays 0.

use core::sync::atomic::{AtomicU64, Ordering};

/// The minor faults of one thread, or of a process's exited threads.
#[derive(Debug, Default)]
pub(crate) struct MinorFaults(AtomicU64);

impl MinorFaults {
    /// One fault resolved.
    pub(crate) fn note(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    /// A thread's count, moved to its process as it leaves.
    pub(crate) fn add(&self, faults: u64) {
        self.0.fetch_add(faults, Ordering::Relaxed);
    }

    /// The count so far.
    pub(crate) fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}
