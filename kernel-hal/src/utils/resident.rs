//! The resident count an address space's page table keeps: Linux's
//! `rss_stat` and `hiwater_rss` (`include/linux/mm_types.h`), behind `VmHWM:`
//! in `/proc/<pid>/status`.

/// Bytes of ordinary memory an address space maps, and the most it has had.
///
/// Kept by the page table because every entry that appears or goes away goes
/// through it, so no path that maps or unmaps can forget to count. The VM above
/// maps from a fault, from fault-around, from a fork's copy, from an eager map
/// and from libos's remap, and tears down from as many places again; Linux
/// counts by hand at each of its own (`inc_mm_counter`, `zap_pte_range`), and
/// a site missed there is a counter that drifts forever.
///
/// An entry marked [`crate::MMUFlags::SPECIAL`] is never counted, in or out,
/// as `vm_normal_page` leaves a device page or the zero page out in Linux.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Resident {
    now: usize,
    peak: usize,
}

impl Resident {
    /// An entry of `bytes` appeared.
    pub(crate) fn add(&mut self, bytes: usize) {
        self.now += bytes;
        self.peak = self.peak.max(self.now);
    }

    /// An entry of `bytes` went away.
    ///
    /// Saturating for libos, whose page tables all share the one host address
    /// space: two of them mapping the same address replace each other's page,
    /// and the second one then takes away a page the first one counted. A real
    /// table can only count out what it counted in.
    pub(crate) fn sub(&mut self, bytes: usize) {
        self.now = self.now.saturating_sub(bytes);
    }

    /// Bytes resident now.
    pub(crate) fn now(&self) -> usize {
        self.now
    }

    /// The most that has been resident since the start or the last
    /// [`Self::restart_peak`].
    pub(crate) fn peak(&self) -> usize {
        self.peak
    }

    /// Start the high-water mark over from what is resident now.
    pub(crate) fn restart_peak(&mut self) {
        self.peak = self.now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_peak_is_the_most_there_has_been_and_stays_after_a_drop() {
        let mut r = Resident::default();
        r.add(3 * 4096);
        r.add(4096);
        r.sub(2 * 4096);
        assert_eq!((r.now(), r.peak()), (2 * 4096, 4 * 4096));
        r.add(4096);
        assert_eq!(r.peak(), 4 * 4096, "back under the mark, which stays");
    }

    #[test]
    fn a_restart_starts_the_mark_from_what_is_resident_now() {
        let mut r = Resident::default();
        r.add(8 * 4096);
        r.sub(6 * 4096);
        r.restart_peak();
        assert_eq!((r.now(), r.peak()), (2 * 4096, 2 * 4096));
        r.add(4096);
        assert_eq!(r.peak(), 3 * 4096);
    }

    #[test]
    fn taking_away_more_than_was_counted_stops_at_nothing() {
        let mut r = Resident::default();
        r.add(4096);
        r.sub(2 * 4096);
        assert_eq!((r.now(), r.peak()), (0, 4096));
    }
}
