//! The mapped-size meter of one address space — Linux's `mm->total_vm` and
//! `mm->hiwater_vm`, the live virtual size and its high-water mark (`VmPeak:`).
//!
//! A process address space is a VMAR *tree*: the ELF loader puts the executable
//! and the vDSO in their own child VMARs (see [`VmAddressRegion::protect`]'s
//! note on PIE + vDSO layouts), so `VmSize` is the sum over the whole tree, not
//! one region. One `VmSizeMeter`, shared by `Arc` between a root VMAR and every
//! sub-region created under it, counts the whole address space: every map adds
//! and every unmap subtracts, wherever in the tree it happens, so the running
//! total can never drift from the tree the way a per-region counter did (it
//! missed the children and read the peak ~840 kB short).
//!
//! Like Linux's `total_vm`/`hiwater_vm` it is maintained incrementally — O(1)
//! per `mmap`, never an O(n) walk of the address space — so a process that maps
//! thousands of regions does not pay O(n) each time to keep its peak.

use core::sync::atomic::{AtomicUsize, Ordering};

/// Live mapped bytes of one address space and the largest that total has ever
/// been. Shared across a VMAR tree by `Arc`; see the module docs.
#[derive(Debug, Default)]
pub(super) struct VmSizeMeter {
    /// Bytes currently mapped across the whole tree — Linux's `total_vm`.
    live: AtomicUsize,
    /// High-water mark of `live` — Linux's `hiwater_vm`, reported as `VmPeak:`.
    /// Only rises on growth; a `munmap` leaves it at the peak. `execve`
    /// (`VmAddressRegion::clear`) is the one thing that lowers it, back to the
    /// reused space's new baseline, because Linux gives the new image a fresh
    /// `mm` whose peak starts over.
    peak: AtomicUsize,
}

impl VmSizeMeter {
    /// The address space grew by `delta` bytes: raise the live total and the
    /// high-water if the total is now the largest it has been.
    ///
    /// `Relaxed` is sufficient. The only correctness requirement is that the
    /// peak end up at least the maximum the live total ever reached; `live`
    /// changes only here and in [`shrank`](Self::shrank), and `shrank` never
    /// creates a new high, so every time the total reaches a new maximum it is
    /// this call's own `fetch_add` result that observes it and feeds it to the
    /// `fetch_max`. No ordering against other memory is needed.
    pub(super) fn grew(&self, delta: usize) {
        let now = self.live.fetch_add(delta, Ordering::Relaxed) + delta;
        self.peak.fetch_max(now, Ordering::Relaxed);
    }

    /// The address space shrank by `delta` bytes (a `munmap`, a region torn
    /// down). The live total falls; the peak is deliberately left where it is.
    pub(super) fn shrank(&self, delta: usize) {
        self.live.fetch_sub(delta, Ordering::Relaxed);
    }

    /// The current high-water mark — `VmPeak:`.
    pub(super) fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }

    /// The live mapped total — `VmSize`. Only the tests read this directly; in
    /// production the live size comes from the authoritative walk
    /// (`get_task_stats`), and this meter exists for the peak.
    #[cfg(test)]
    pub(super) fn live(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    /// Reset the high-water to the live total. `execve` calls this after
    /// `clear()` has emptied the reused address space, so the new image's
    /// `VmPeak:` starts from its own baseline rather than inheriting the old
    /// program's high-water.
    pub(super) fn reset_peak_to_live(&self) {
        self.peak
            .store(self.live.load(Ordering::Relaxed), Ordering::Relaxed);
    }
}
