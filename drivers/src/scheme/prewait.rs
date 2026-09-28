//! How long a DRM ioctl was parked BEFORE the driver ever saw it.
//!
//! Five DRM ioctls block by contract -- `WAIT_VBLANK`, `SYNCOBJ_WAIT` and its
//! timeline form, a `MODE_ATOMIC` carrying `IN_FENCE_FD`, `GEM_CPU_PREP`, and
//! a legacy present with a render fence still in flight. `INode::io_control`
//! is synchronous, so each of them is split: an `async fn` in `sys_ioctl`
//! parks the thread until the condition is met, and only then does the
//! synchronous arm run and answer. Without that split the sync arm spin-polls
//! the whole timeout and pegs a core.
//!
//! The consequence for anyone reading `/proc/gpudbg` is that the parking is
//! INVISIBLE. The driver's per-ioctl profile times the dispatch, which starts
//! after the wait is already over, so an ioctl that slept four milliseconds
//! and then answered in three microseconds is reported as three microseconds.
//! A frame that spends most of its time parked therefore shows up as a
//! profile full of small numbers and no explanation of where the frame went.
//!
//! This module is that explanation: one counter set per kind of pre-wait,
//! recorded by `linux-object`'s `drm_scheme` as each wait ends, printed in the
//! gpudbg profile next to the ioctl table. Diff two reads across a run of
//! glxgears and the parked microseconds per frame are the answer to "where
//! did the frame go", in the place where the answer can actually be had --
//! on the machine with the GPU in it.
//!
//! Nothing here is on a hot path in the sense that matters: one relaxed
//! add per BLOCKING ioctl, made once the wait it measures has already ended.

use core::sync::atomic::{AtomicU64, Ordering};

/// Which blocking ioctl a parked stretch belongs to.
///
/// The discriminants index [`STATS`], so they are consecutive from zero and
/// [`Kind::ALL`] lists every one of them; a kind added without a line there
/// would be counted and never printed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `DRM_IOCTL_WAIT_VBLANK`.
    WaitVblank = 0,
    /// `DRM_IOCTL_SYNCOBJ_WAIT` and `SYNCOBJ_TIMELINE_WAIT`.
    SyncobjWait = 1,
    /// `DRM_IOCTL_MODE_ATOMIC` carrying an `IN_FENCE_FD`.
    AtomicInFence = 2,
    /// `DRM_IOCTL_NOUVEAU_GEM_CPU_PREP`.
    CpuPrep = 3,
    /// `SETCRTC` or `PAGE_FLIP` waiting for the fb's render fence.
    PresentFence = 4,
}

impl Kind {
    /// Every kind, in the order they are printed.
    pub const ALL: [Kind; 5] = [
        Kind::WaitVblank,
        Kind::SyncobjWait,
        Kind::AtomicInFence,
        Kind::CpuPrep,
        Kind::PresentFence,
    ];

    /// The name this kind is printed under.
    pub fn name(self) -> &'static str {
        match self {
            Kind::WaitVblank => "WAIT_VBLANK",
            Kind::SyncobjWait => "SYNCOBJ_WAIT",
            Kind::AtomicInFence => "ATOMIC_IN_FENCE",
            Kind::CpuPrep => "GEM_CPU_PREP",
            Kind::PresentFence => "PRESENT_FENCE",
        }
    }
}

#[derive(Default)]
struct Stat {
    /// Times this pre-wait ran at all, parked or not.
    calls: AtomicU64,
    /// Times it found the condition already met and never parked. Against
    /// `calls` this says whether the wait is doing anything: a compositor
    /// whose fences always land first shows `parked` near zero, and then the
    /// frame went somewhere else.
    parked: AtomicU64,
    /// Microseconds spent parked, summed.
    parked_us: AtomicU64,
    /// The longest single park.
    max_us: AtomicU64,
    /// Probes taken across every park: how many times the loop woke, looked
    /// and went back to sleep. Against `parked_us` it says whether the
    /// backoff is sleeping too long (few probes, much time) or waking too
    /// often (many probes, little time).
    probes: AtomicU64,
    /// Parks that ended on the deadline rather than on the condition.
    timeouts: AtomicU64,
}

impl Stat {
    const fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
            parked: AtomicU64::new(0),
            parked_us: AtomicU64::new(0),
            max_us: AtomicU64::new(0),
            probes: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
        }
    }
}

static STATS: [Stat; Kind::ALL.len()] = [const { Stat::new() }; Kind::ALL.len()];

/// Account one finished pre-wait.
///
/// `us` is how long it was parked (0 when the condition was already met),
/// `probes` how many times it woke to look, and `timed_out` whether it gave
/// up on its deadline instead of on the condition.
pub fn record(kind: Kind, us: u64, probes: u32, timed_out: bool) {
    let s = &STATS[kind as usize];
    s.calls.fetch_add(1, Ordering::Relaxed);
    if us > 0 || probes > 0 {
        s.parked.fetch_add(1, Ordering::Relaxed);
        s.parked_us.fetch_add(us, Ordering::Relaxed);
        s.max_us.fetch_max(us, Ordering::Relaxed);
        s.probes.fetch_add(probes as u64, Ordering::Relaxed);
    }
    if timed_out {
        s.timeouts.fetch_add(1, Ordering::Relaxed);
    }
}

/// The pre-wait table for the `/proc/gpudbg` profile, one line per kind that
/// has been called, plus a header. Empty when nothing has blocked at all.
pub fn profile_lines() -> alloc::string::String {
    use core::fmt::Write;
    let mut s = alloc::string::String::new();
    if Kind::ALL
        .iter()
        .all(|&k| STATS[k as usize].calls.load(Ordering::Relaxed) == 0)
    {
        return s;
    }
    let _ = writeln!(
        s,
        "[gpudbg]  pre-wait (parked BEFORE the driver saw the ioctl; not in the table above)"
    );
    let _ = writeln!(
        s,
        "[gpudbg]  {:<16} {:>8} {:>8} {:>13} {:>11} {:>9} {:>9}",
        "wait", "calls", "parked", "parked_us", "max_us", "probes", "timeouts"
    );
    for &k in Kind::ALL.iter() {
        let st = &STATS[k as usize];
        let calls = st.calls.load(Ordering::Relaxed);
        if calls == 0 {
            continue;
        }
        let _ = writeln!(
            s,
            "[gpudbg]  {:<16} {:>8} {:>8} {:>13} {:>11} {:>9} {:>9}",
            k.name(),
            calls,
            st.parked.load(Ordering::Relaxed),
            st.parked_us.load(Ordering::Relaxed),
            st.max_us.load(Ordering::Relaxed),
            st.probes.load(Ordering::Relaxed),
            st.timeouts.load(Ordering::Relaxed),
        );
    }
    s
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    /// The counters are global, so two of these tests running at once would
    /// read each other's increments between their own before and after.
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The counters are global and cumulative, so a test reads a kind's line
    /// before and after rather than assuming it starts at zero.
    fn line_of(kind: Kind) -> alloc::string::String {
        profile_lines()
            .lines()
            .find(|l| l.contains(kind.name()))
            .map(alloc::string::String::from)
            .unwrap_or_default()
    }

    fn nth_number(line: &str, n: usize) -> u64 {
        line.split_whitespace()
            .filter_map(|w| w.parse::<u64>().ok())
            .nth(n)
            .unwrap_or(0)
    }

    /// A wait that never parked is still a call, and it adds nothing to the
    /// parked time. This is the line that says "the fence was already there",
    /// and reading it as zero parked microseconds is the whole point: it sends
    /// whoever is hunting a slow frame somewhere else.
    #[test]
    fn a_wait_that_did_not_park_counts_as_a_call_and_no_time() {
        let _g = test_lock();
        let before = line_of(Kind::AtomicInFence);
        let (calls, parked, us) = (
            nth_number(&before, 0),
            nth_number(&before, 1),
            nth_number(&before, 2),
        );
        record(Kind::AtomicInFence, 0, 0, false);
        let after = line_of(Kind::AtomicInFence);
        assert_eq!(nth_number(&after, 0), calls + 1, "the call was not counted");
        assert_eq!(nth_number(&after, 1), parked, "it was counted as parked");
        assert_eq!(nth_number(&after, 2), us, "it added parked microseconds");
    }

    /// A park is counted once, its microseconds summed and its longest kept.
    #[test]
    fn a_park_adds_its_time_its_probes_and_raises_the_maximum() {
        let _g = test_lock();
        let before = line_of(Kind::CpuPrep);
        let (calls, parked, us, probes) = (
            nth_number(&before, 0),
            nth_number(&before, 1),
            nth_number(&before, 2),
            nth_number(&before, 4),
        );
        record(Kind::CpuPrep, 700, 3, false);
        record(Kind::CpuPrep, 4_000, 9, false);
        let after = line_of(Kind::CpuPrep);
        assert_eq!(nth_number(&after, 0), calls + 2);
        assert_eq!(nth_number(&after, 1), parked + 2);
        assert_eq!(nth_number(&after, 2), us + 4_700);
        assert!(
            nth_number(&after, 3) >= 4_000,
            "the longest park was not kept"
        );
        assert_eq!(nth_number(&after, 4), probes + 12);
    }

    /// A wait that woke, looked and went back to sleep without the clock
    /// having moved is still a park: the probes are what make it one. A
    /// virtual or coarse clock reading 0 us must not turn a loop that span
    /// into a wait that never happened.
    #[test]
    fn a_park_the_clock_did_not_see_is_still_a_park() {
        let _g = test_lock();
        let before = line_of(Kind::PresentFence);
        let parked = nth_number(&before, 1);
        record(Kind::PresentFence, 0, 5, false);
        let after = line_of(Kind::PresentFence);
        assert_eq!(nth_number(&after, 1), parked + 1);
        assert_eq!(nth_number(&after, 4), nth_number(&before, 4) + 5);
    }

    /// A park that ran out its deadline is counted apart: it is the shape of
    /// a hung ring, and it must not read as a wait that was satisfied.
    #[test]
    fn a_park_that_timed_out_is_counted_apart() {
        let _g = test_lock();
        let before = line_of(Kind::WaitVblank);
        let timeouts = nth_number(&before, 5);
        record(Kind::WaitVblank, 100_000, 120, true);
        let after = line_of(Kind::WaitVblank);
        assert_eq!(nth_number(&after, 5), timeouts + 1);
    }

    /// Every kind is printed under its own name, and every kind is in `ALL`.
    /// A kind left out of `ALL` would be recorded and never shown.
    #[test]
    fn every_kind_is_listed_and_named_once() {
        let _g = test_lock();
        assert_eq!(Kind::ALL.len(), 5);
        for (i, &k) in Kind::ALL.iter().enumerate() {
            assert_eq!(k as usize, i, "{:?} is not at its own index", k);
            record(k, 1, 1, false);
        }
        let out = profile_lines();
        for &k in Kind::ALL.iter() {
            assert_eq!(
                out.matches(k.name()).count(),
                1,
                "{} is not printed exactly once:\n{}",
                k.name(),
                out
            );
        }
    }
}
