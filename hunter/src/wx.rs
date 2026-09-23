//! Per-process "ever-writable region" tracking for write-xor-execute (P3).
//!
//! A single per-call conjunction check (deny only when a mapping requests
//! `WRITE` *and* `EXEC` simultaneously) is trivially defeated by the two-step
//! `mmap(PROT_WRITE)` then `mprotect(PROT_EXEC)` sequence (finding WX-2): each
//! call on its own looks benign. This module remembers, per process, every
//! address range that was *ever* writable, so a later request to make any part
//! of such a range executable is recognised as a W^X violation.
//!
//! State is bounded: each process tracks at most [`MAX_REGIONS`] intervals; on
//! overflow the process is marked *saturated* and conservatively treated as if
//! its whole address space were ever-writable (fail-closed for security, never
//! fail-open). Intervals are dropped on `munmap` of the range and on task exit.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use lock::Mutex;

/// Maximum tracked writable intervals per process before saturation.
const MAX_REGIONS: usize = 1024;
/// Maximum number of processes tracked before the least-recently-touched is
/// dropped, bounding total memory against a spawn-flood DoS.
const MAX_TRACKED_PIDS: usize = 4096;

#[derive(Default)]
struct ProcRegions {
    /// Sorted, non-overlapping `[start, end)` intervals ever mapped writable.
    intervals: Vec<(usize, usize)>,
    /// Once true, every executable request is treated as a W^X violation.
    saturated: bool,
    /// Monotonic touch stamp for LRU eviction under the pid cap.
    touch: u64,
}

lazy_static::lazy_static! {
    static ref REGIONS: Mutex<BTreeMap<u64, ProcRegions>> = Mutex::new(BTreeMap::new());
}

/// Highest pid whose tracked state the LRU cap has dropped, or 0 if none has.
///
/// The module's promise is "fail-closed for security, never fail-open", and
/// the saturation path keeps it — but eviction did not. A dropped process
/// simply became untracked, and an untracked process answers "never writable",
/// so `mmap(PROT_WRITE); <evict>; mprotect(PROT_EXEC)` went through: a flood of
/// [`MAX_TRACKED_PIDS`] live processes cleared the record of whichever process
/// you meant to attack. Distinguishing "never made anything writable" (no) from
/// "we forgot what it made writable" (yes) needs state we cannot afford
/// per-pid, but pids only ever count up, so one watermark does it: a pid at or
/// below it may have been evicted, a pid above it was born after the last
/// eviction and was never tracked. `forget` (task exit) is a legitimate removal
/// and does NOT raise it.
static EVICTED_UP_TO: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Monotonic counter used only to order LRU eviction (independent of wall clock).
static TOUCH: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
fn next_touch() -> u64 {
    TOUCH.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
}

fn ranges_overlap(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

fn end_of(addr: usize, len: usize) -> usize {
    addr.saturating_add(len)
}

/// Records that `[addr, addr+len)` was mapped (or re-protected) writable.
pub fn record_writable(pid: u64, addr: usize, len: usize) {
    if len == 0 {
        return;
    }
    let new = (addr, end_of(addr, len));
    let mut map = REGIONS.lock();
    evict_if_needed(&mut map, pid);
    let pr = map.entry(pid).or_default();
    pr.touch = next_touch();
    if pr.saturated {
        return;
    }
    // Coalesce against every overlapping or adjacent interval.
    //
    // This used to be one `retain` over an unordered list, growing `merged` as
    // it walked and pushing the result on the end. An interval the walk had
    // already passed could touch the *grown* `merged` and was kept anyway, so
    // the list drifted into overlapping, unsorted entries — against what the
    // field documents, and a slow leak of slots towards the `MAX_REGIONS`
    // saturation cliff, which a program can drive by mapping alternate pages
    // and then the gaps between them.
    //
    // Keeping the list sorted is what makes a single pass sufficient: the new
    // interval touches one contiguous run, and `merged` only grows outwards.
    let mut merged = new;
    let at = pr.intervals.partition_point(|iv| iv.1 < merged.0);
    let mut upto = at;
    while upto < pr.intervals.len() && pr.intervals[upto].0 <= merged.1 {
        let iv = pr.intervals[upto];
        merged = (merged.0.min(iv.0), merged.1.max(iv.1));
        upto += 1;
    }
    pr.intervals.drain(at..upto);
    pr.intervals.insert(at, merged);
    if pr.intervals.len() > MAX_REGIONS {
        // Give up on precise tracking and fail closed.
        pr.saturated = true;
        pr.intervals = Vec::new();
    }
}

/// Returns `true` if any part of `[addr, addr+len)` was ever writable for `pid`.
pub fn is_ever_writable(pid: u64, addr: usize, len: usize) -> bool {
    let q = (addr, end_of(addr, len.max(1)));
    let map = REGIONS.lock();
    match map.get(&pid) {
        Some(pr) if pr.saturated => true,
        Some(pr) => pr.intervals.iter().any(|&iv| ranges_overlap(iv, q)),
        // Untracked: never seen, or evicted. See `EVICTED_UP_TO`.
        None => {
            let hwm = EVICTED_UP_TO.load(core::sync::atomic::Ordering::Relaxed);
            hwm != 0 && pid <= hwm
        }
    }
}

/// Drops tracked writable intervals overlapping `[addr, addr+len)` (on munmap).
pub fn clear_region(pid: u64, addr: usize, len: usize) {
    let q = (addr, end_of(addr, len.max(1)));
    let mut map = REGIONS.lock();
    if let Some(pr) = map.get_mut(&pid) {
        if pr.saturated {
            return; // cannot subtract from an imprecise saturated set
        }
        pr.touch = next_touch();
        let mut out = Vec::with_capacity(pr.intervals.len());
        for &iv in pr.intervals.iter() {
            if !ranges_overlap(iv, q) {
                out.push(iv);
                continue;
            }
            // Keep the non-overlapping head/tail slivers.
            if iv.0 < q.0 {
                out.push((iv.0, q.0));
            }
            if iv.1 > q.1 {
                out.push((q.1, iv.1));
            }
        }
        pr.intervals = out;
    }
}

/// Releases all tracked state for an exited process.
pub fn forget(pid: u64) {
    REGIONS.lock().remove(&pid);
}

/// Evicts the least-recently-touched process if inserting `pid` would exceed
/// the cap (and `pid` is not already tracked).
fn evict_if_needed(map: &mut BTreeMap<u64, ProcRegions>, pid: u64) {
    if map.len() < MAX_TRACKED_PIDS || map.contains_key(&pid) {
        return;
    }
    if let Some((&victim, _)) = map.iter().min_by_key(|(_, pr)| pr.touch) {
        map.remove(&victim);
        // Remember that we can no longer answer for this pid (or any older
        // one), so the loss reads as "assume ever-writable" rather than
        // "never was". See `EVICTED_UP_TO`.
        EVICTED_UP_TO.fetch_max(victim, core::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_globals;

    /// Returns this process's tracked intervals, or `None` when it is
    /// saturated (an imprecise "everything" set).
    fn intervals_of(pid: u64) -> Option<Vec<(usize, usize)>> {
        let map = REGIONS.lock();
        match map.get(&pid) {
            Some(pr) if pr.saturated => None,
            Some(pr) => Some(pr.intervals.clone()),
            None => Some(Vec::new()),
        }
    }

    fn reset() {
        REGIONS.lock().clear();
        EVICTED_UP_TO.store(0, core::sync::atomic::Ordering::Relaxed);
    }

    /// The list is documented as sorted and non-overlapping; nothing enforced
    /// it, so assert it after every mutation that a test performs.
    fn assert_tidy(ivs: &[(usize, usize)]) {
        for w in ivs.windows(2) {
            assert!(
                w[0].1 < w[1].0,
                "intervals must stay sorted and disjoint, got {:?} then {:?}",
                w[0],
                w[1]
            );
        }
        for iv in ivs {
            assert!(iv.0 < iv.1, "empty or inverted interval {:?}", iv);
        }
    }

    #[test]
    fn a_writable_range_is_remembered_and_a_neighbour_is_not() {
        let _g = test_globals::lock();
        reset();
        record_writable(1_001, 0x8000, 0x1000);
        assert!(is_ever_writable(1_001, 0x8000, 0x1000));
        assert!(is_ever_writable(1_001, 0x8800, 0x10));
        assert!(!is_ever_writable(1_001, 0x9000, 0x1000));
        assert!(!is_ever_writable(1_001, 0x7000, 0x1000));
    }

    #[test]
    fn filling_the_gaps_between_pages_collapses_to_one_interval() {
        let _g = test_globals::lock();
        reset();
        let pid = 1_002;
        // Alternate pages first, then the gaps between them. Under the old
        // unordered `retain` coalescing, an interval the walk had already
        // passed could touch the grown `merged` and was kept anyway: the list
        // came out overlapping, unsorted, and one slot longer each round --
        // a leak towards the MAX_REGIONS saturation cliff that a program can
        // drive deliberately with this exact pattern.
        for i in 0..64 {
            record_writable(pid, 0x10_0000 + i * 0x2000, 0x1000);
        }
        let ivs = intervals_of(pid).expect("must not be saturated");
        assert_eq!(ivs.len(), 64, "64 disjoint pages are 64 intervals");
        assert_tidy(&ivs);
        for i in 0..64 {
            record_writable(pid, 0x10_0000 + i * 0x2000 + 0x1000, 0x1000);
        }
        let ivs = intervals_of(pid).expect("must not be saturated");
        assert_tidy(&ivs);
        assert_eq!(
            ivs,
            alloc::vec![(0x10_0000, 0x10_0000 + 64 * 0x2000)],
            "the filled gaps make one contiguous range"
        );
    }

    #[test]
    fn a_range_that_swallows_several_intervals_replaces_all_of_them() {
        let _g = test_globals::lock();
        reset();
        let pid = 1_003;
        for i in 0..8 {
            record_writable(pid, 0x20_0000 + i * 0x4000, 0x1000);
        }
        assert_eq!(intervals_of(pid).unwrap().len(), 8);
        record_writable(pid, 0x20_0000, 8 * 0x4000);
        let ivs = intervals_of(pid).unwrap();
        assert_tidy(&ivs);
        assert_eq!(ivs, alloc::vec![(0x20_0000, 0x20_0000 + 8 * 0x4000)]);
    }

    #[test]
    fn adjacent_ranges_are_coalesced_but_a_one_byte_hole_is_not() {
        let _g = test_globals::lock();
        reset();
        let pid = 1_004;
        record_writable(pid, 0x1000, 0x1000);
        record_writable(pid, 0x2000, 0x1000);
        assert_eq!(intervals_of(pid).unwrap(), alloc::vec![(0x1000, 0x3000)]);
        record_writable(pid, 0x3001, 0x1000);
        let ivs = intervals_of(pid).unwrap();
        assert_tidy(&ivs);
        assert_eq!(ivs, alloc::vec![(0x1000, 0x3000), (0x3001, 0x4001)]);
    }

    #[test]
    fn unmapping_the_middle_leaves_both_ends_and_forgets_the_hole() {
        let _g = test_globals::lock();
        reset();
        let pid = 1_005;
        record_writable(pid, 0x4000, 0x4000);
        clear_region(pid, 0x5000, 0x1000);
        let ivs = intervals_of(pid).unwrap();
        assert_tidy(&ivs);
        assert_eq!(ivs, alloc::vec![(0x4000, 0x5000), (0x6000, 0x8000)]);
        assert!(is_ever_writable(pid, 0x4000, 0x1000));
        assert!(!is_ever_writable(pid, 0x5000, 0x1000));
        assert!(is_ever_writable(pid, 0x6000, 0x1000));
        // And the list still coalesces correctly around the hole afterwards.
        record_writable(pid, 0x5000, 0x1000);
        let ivs = intervals_of(pid).unwrap();
        assert_tidy(&ivs);
        assert_eq!(ivs, alloc::vec![(0x4000, 0x8000)]);
    }

    #[test]
    fn saturation_treats_the_whole_address_space_as_ever_writable() {
        let _g = test_globals::lock();
        reset();
        let pid = 1_006;
        for i in 0..=MAX_REGIONS {
            record_writable(pid, 0x100_0000 + i * 0x2000, 0x1000);
        }
        assert!(
            intervals_of(pid).is_none(),
            "past MAX_REGIONS the process must saturate, not keep counting"
        );
        assert!(is_ever_writable(pid, 0xdead_0000, 0x1000));
        // A saturated set is imprecise, so munmap cannot subtract from it.
        clear_region(pid, 0x100_0000, 0x1000);
        assert!(is_ever_writable(pid, 0x100_0000, 0x1000));
    }

    #[test]
    fn an_unknown_process_is_not_writable_while_nothing_has_been_evicted() {
        let _g = test_globals::lock();
        reset();
        assert!(!is_ever_writable(1_007, 0x1000, 0x1000));
        // pid 0 is the id hunter uses for the kernel's own events, and it is
        // the one pid that is not above an empty watermark.
        assert!(!is_ever_writable(0, 0x1000, 0x1000));
    }

    #[test]
    fn a_range_below_the_others_is_inserted_in_its_place() {
        let _g = test_globals::lock();
        reset();
        let pid = 1_009;
        record_writable(pid, 0x8000, 0x1000);
        record_writable(pid, 0x2000, 0x1000);
        record_writable(pid, 0x5000, 0x1000);
        let ivs = intervals_of(pid).unwrap();
        assert_tidy(&ivs);
        assert_eq!(
            ivs,
            alloc::vec![(0x2000, 0x3000), (0x5000, 0x6000), (0x8000, 0x9000)]
        );
    }

    #[test]
    fn an_evicted_process_reads_as_ever_writable() {
        let _g = test_globals::lock();
        reset();
        // The module promises "fail-closed for security, never fail-open", and
        // eviction broke it: a dropped process became untracked, and untracked
        // answered "never writable". So mmap(PROT_WRITE), spawn-flood the pid
        // cap, mprotect(PROT_EXEC) walked through the W^X check -- an attacker
        // could clear the record of whichever process they meant to attack.
        for pid in 1..=(MAX_TRACKED_PIDS as u64) {
            record_writable(pid, 0x1000, 0x1000);
        }
        assert!(is_ever_writable(1, 0x1000, 0x1000));
        // One more process evicts the least-recently-touched, which is pid 1.
        record_writable(MAX_TRACKED_PIDS as u64 + 1, 0x1000, 0x1000);
        assert!(
            REGIONS.lock().get(&1).is_none(),
            "pid 1 should have been the eviction victim"
        );
        assert!(
            is_ever_writable(1, 0x1000, 0x1000),
            "an evicted process must read as ever-writable, not as never-writable"
        );
        // ...and so must any address in it, since we no longer know which.
        assert!(is_ever_writable(1, 0xbeef_0000, 0x1000));
        reset();
    }

    #[test]
    fn a_process_born_after_the_last_eviction_does_not_pay_for_it() {
        let _g = test_globals::lock();
        reset();
        // Pids only ever count up, which is what makes one watermark enough:
        // a pid above it was never tracked, so it really never was writable.
        EVICTED_UP_TO.store(500, core::sync::atomic::Ordering::Relaxed);
        assert!(is_ever_writable(500, 0x1000, 0x1000));
        assert!(!is_ever_writable(501, 0x1000, 0x1000));
        reset();
    }

    #[test]
    fn a_process_exiting_does_not_raise_the_eviction_watermark() {
        let _g = test_globals::lock();
        reset();
        // `forget` is task exit: a legitimate removal, not a lost record. If it
        // raised the watermark, every pid below the highest one that has ever
        // exited would answer "ever writable" -- which on a running system is
        // all of them.
        record_writable(9_000, 0x1000, 0x1000);
        forget(9_000);
        assert_eq!(EVICTED_UP_TO.load(core::sync::atomic::Ordering::Relaxed), 0);
        assert!(!is_ever_writable(9_000, 0x1000, 0x1000));
        assert!(!is_ever_writable(1, 0x1000, 0x1000));
    }

    #[test]
    fn a_zero_length_record_is_ignored_and_a_zero_length_query_still_hits() {
        let _g = test_globals::lock();
        reset();
        let pid = 1_008;
        record_writable(pid, 0x1000, 0);
        assert_eq!(intervals_of(pid).unwrap(), Vec::new());
        record_writable(pid, 0x1000, 0x1000);
        // mprotect(len=0) is a no-op in Linux, but asking about address 0x1000
        // must not silently answer "no" just because the length rounded to
        // nothing -- `is_ever_writable` widens it to one byte on purpose.
        assert!(is_ever_writable(pid, 0x1000, 0));
    }
}
