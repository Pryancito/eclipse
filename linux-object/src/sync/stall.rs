//! Waits that have NOT ended yet.
//!
//! Every counter this kernel keeps about a blocking wait is recorded when the
//! wait *finishes*: `zcore_drivers::scheme::prewait` adds its parked
//! microseconds "as each wait ends", the per-ioctl profile times the dispatch
//! that runs afterwards, and `trace_wait_error` prints the errno a wait
//! returned. A wait that never returns is therefore invisible to all of them
//! — the one case where the machine has the answer and no way to say it.
//!
//! That is the shape of a frozen window. `glxgears` under Xwayland draws one
//! frame and stops with no FPS line; the process is alive, the compositor is
//! alive, and `/proc/gpudbg` shows a profile full of small numbers because
//! the frame is not slow, it is parked. Which wait it is parked in decides
//! everything: a **shared** futex is the libxshmfence/DRI3 handshake with the
//! X server (see [`super::shared_futex`]), a `SYNCOBJ_WAIT` is a GPU fence
//! that never landed, and a private futex is an ordinary in-process mutex and
//! says to look elsewhere entirely. Guessing between those three has cost two
//! rounds of fixes aimed at the wrong one.
//!
//! So each interesting wait registers here before it parks and deregisters on
//! the way out, and a wait still registered past [`STALL_AFTER`] gets one
//! `klog` line naming the process and what it is waiting on. The table is also
//! readable at `/proc/stalls`, which is the same information without having to
//! catch the line in `dmesg`.
//!
//! Three things to respect if this is ever touched:
//!
//! - **The scan runs from [`register`], not from a timer.** Annunciating from
//!   a `timer_set` callback would mean taking this lock and emitting to the
//!   kernel log from IRQ context. There is no need: a stalled process does not
//!   stop the rest of the machine, and anything else that blocks walks through
//!   `register` and does the scan in ordinary syscall context. If nothing in
//!   the system ever blocks again, nobody is reading `dmesg` either.
//! - **One line per [`SCAN_MIN_GAP`], with the suppressed ones counted**, for
//!   the reason `trace_wait_error` documents at length: an unthrottled trace in
//!   a wait path pushed 17 minutes of kernel log out of the ring buffer and
//!   destroyed the evidence it had been added to collect.
//! - **An entry is annunciated once**, and once more when it is finally
//!   released, with how long it took. "Released after 37 s" is a different
//!   bug from "never released" and the two must not read alike.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use kernel_hal::sync::Mutex;

/// How long a wait has to be outstanding before it is worth a line.
///
/// A compositor's own frame pacing parks for ~16 ms, a contended mutex for
/// microseconds, and a GPU fence for at most a few frames. Two seconds is past
/// anything healthy and still well inside the patience of someone watching a
/// window that has stopped.
const STALL_AFTER: u64 = 2_000_000_000;

/// Minimum gap between two annunciated lines, suppressed ones counted.
const SCAN_MIN_GAP: u64 = 100_000_000;

/// Which wait an entry belongs to.
///
/// Shared and private futexes are deliberately separate kinds rather than one
/// kind with a flag: telling them apart is the single most useful thing this
/// table does, because a shared futex word is the DRI3 handshake every GL
/// application under Xwayland blocks on and a private one never leaves the
/// process.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `FUTEX_WAIT` on a word two processes share — libxshmfence/DRI3.
    FutexShared,
    /// `FUTEX_WAIT` on a process-private word — an ordinary mutex or condvar.
    FutexPrivate,
    /// `DRM_IOCTL_SYNCOBJ_WAIT` / `SYNCOBJ_TIMELINE_WAIT`.
    SyncobjWait,
    /// `poll()` on a `sync_file` exported from a syncobj.
    SyncFilePoll,
}

impl Kind {
    /// The name this kind is printed under.
    pub fn name(self) -> &'static str {
        match self {
            Kind::FutexShared => "futex(shared)",
            Kind::FutexPrivate => "futex(private)",
            Kind::SyncobjWait => "SYNCOBJ_WAIT",
            Kind::SyncFilePoll => "poll(sync_file)",
        }
    }

    /// What [`Entry::detail`] means for this kind, for the printed line.
    fn detail_label(self) -> &'static str {
        match self {
            Kind::FutexShared | Kind::FutexPrivate => "uaddr",
            Kind::SyncobjWait | Kind::SyncFilePoll => "handle",
        }
    }

    /// What [`Entry::aux`] means for this kind.
    fn aux_label(self) -> &'static str {
        match self {
            Kind::FutexShared | Kind::FutexPrivate => "want",
            Kind::SyncobjWait | Kind::SyncFilePoll => "point",
        }
    }

    /// Whether a long wait of this kind is worth a `klog` line, as opposed to
    /// merely appearing in `/proc/stalls`.
    ///
    /// **A private futex parked for ever is the healthy case.** Every idle
    /// thread pool, every `pthread_cond_wait` with no work to do, and every
    /// `__synccall` checkpoint sits in an untimed private `FUTEX_WAIT` for as
    /// long as the machine is up; a desktop has dozens at any moment.
    /// Annunciating those would put a line in the log every
    /// [`SCAN_MIN_GAP`] for ever and bury the one line that matters -- which
    /// is precisely the mistake `trace_wait_error` was written to undo. They
    /// are still recorded, so `/proc/stalls` can show them next to the rest;
    /// they just do not shout.
    ///
    /// The other three are pathological past [`STALL_AFTER`] without
    /// exception: a `sync_file` poll and a `SYNCOBJ_WAIT` are waiting on a GPU
    /// fence that should land within a frame or two, and a SHARED futex word
    /// is a handshake between two processes -- libxshmfence's `await`, which
    /// the X server releases on the next present.
    fn annunciate(self) -> bool {
        match self {
            Kind::FutexPrivate => false,
            Kind::FutexShared | Kind::SyncobjWait | Kind::SyncFilePoll => true,
        }
    }
}

/// One outstanding wait.
struct Entry {
    /// Identity the guard removes by. Monotonic, so a slot reused by a later
    /// wait is never mistaken for this one.
    id: u64,
    kind: Kind,
    pid: u64,
    name: String,
    /// The futex word's address, or the syncobj handle.
    detail: u64,
    /// The value `FUTEX_WAIT` expects, or the timeline point waited for.
    aux: i64,
    /// Monotonic ns at registration.
    start_ns: u64,
    /// Whether this entry has already had its line.
    reported: bool,
}

static STALLS: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static LAST_SCAN_NS: AtomicU64 = AtomicU64::new(0);
static SUPPRESSED: AtomicU64 = AtomicU64::new(0);
/// `stalls=off` on the kernel command line turns the whole thing off.
static ENABLED: AtomicBool = AtomicBool::new(true);

/// Turn the stall table on or off (`stalls=off`).
pub fn set_enabled(v: bool) {
    ENABLED.store(v, Ordering::Relaxed);
    if !v {
        STALLS.lock().clear();
    }
}

/// Whether the stall table is recording.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Deregisters its entry when dropped, whatever ended the wait — a wake, a
/// timeout, a signal, or the task being torn down.
///
/// Held across the `await` that parks, so the entry's lifetime is exactly the
/// wait's. There is no `forget` path: an entry that outlived its wait would be
/// a permanent false positive in `/proc/stalls`, which is worse than a missing
/// one.
#[must_use = "the wait is only recorded for as long as the guard is held"]
pub struct Guard {
    id: u64,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let released = {
            let mut table = STALLS.lock();
            match table.iter().position(|e| e.id == self.id) {
                Some(i) => {
                    let e = table.swap_remove(i);
                    e.reported.then(|| {
                        let now = kernel_hal::timer::timer_now().as_nanos() as u64;
                        (e.kind, e.pid, e.name, now.saturating_sub(e.start_ns))
                    })
                }
                None => None,
            }
        };
        // Outside the lock: `klog_warn!` takes the console's own.
        if let Some((kind, pid, name, waited_ns)) = released {
            zcore_drivers::klog_warn!(
                "[stall] {} released after {} ms for pid {} ({})",
                kind.name(),
                waited_ns / 1_000_000,
                pid,
                name
            );
        }
    }
}

/// Record a wait that is about to park, and scan for ones that have gone on
/// too long. `None` when the table is off, in which case nothing is recorded
/// and the caller simply waits as it always did.
///
/// `detail` and `aux` are the two numbers that identify the wait: see
/// [`Kind::detail_label`] and [`Kind::aux_label`].
pub fn register(kind: Kind, detail: u64, aux: i64) -> Option<Guard> {
    if !enabled() {
        return None;
    }
    let now = kernel_hal::timer::timer_now().as_nanos() as u64;
    // The scan BEFORE the push, so a wait never annunciates itself.
    scan(now);
    // One short `String` per park. The lesson about a global atomic costing
    // 313 ns is about a cost paid by EVERY syscall; this one is paid only by a
    // caller that is about to leave the CPU, next to a context switch that
    // costs microseconds. The name has to be taken now rather than at
    // annunciation time because by then the only handle on the process is a
    // pid, and a pid is exactly what a frozen window cannot be looked up by.
    let (pid, name) = crate::process::current_process_pid_name();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    STALLS.lock().push(Entry {
        id,
        kind,
        pid,
        name,
        detail,
        aux,
        start_ns: now,
        reported: false,
    });
    Some(Guard { id })
}

/// True when `now` is at least `gap` past `last`.
///
/// `last == 0` is "never scanned" and is always due, which also covers the
/// first instants after boot where the clock itself reads 0. A `now` BEFORE
/// `last` is a clock that moved backwards: due as well, because the
/// alternative is a gate that stays shut until the clock catches up with a
/// reading it may never reach again.
fn scan_is_due(now: u64, last: u64, gap: u64) -> bool {
    last == 0 || now < last || now.saturating_sub(last) >= gap
}

/// Decide whether an entry has been outstanding long enough to annunciate.
///
/// A pure function so the threshold can be tested without a clock: `now`
/// before `start_ns` is a clock that moved backwards under us, which is not a
/// stall.
fn is_stalled(now: u64, start_ns: u64, after: u64) -> bool {
    now >= start_ns && now - start_ns >= after
}

/// Annunciate at most one overdue entry, oldest first.
fn scan(now: u64) {
    let last = LAST_SCAN_NS.load(Ordering::Relaxed);
    if !scan_is_due(now, last, SCAN_MIN_GAP) {
        return;
    }
    // Relaxed and not a compare-exchange loop, as in `trace_wait_error`: two
    // CPUs racing here print two lines, which is the whole cost of being
    // wrong.
    LAST_SCAN_NS.store(now, Ordering::Relaxed);
    let overdue = {
        let mut table = STALLS.lock();
        // Oldest first: the wait that has been stuck longest is the one that
        // explains the others, and a frozen frame loop queues up behind it.
        let pick = table
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                !e.reported && e.kind.annunciate() && is_stalled(now, e.start_ns, STALL_AFTER)
            })
            .min_by_key(|(_, e)| e.start_ns)
            .map(|(i, _)| i);
        match pick {
            Some(i) => {
                table[i].reported = true;
                let e = &table[i];
                Some((
                    e.kind,
                    e.pid,
                    e.name.clone(),
                    e.detail,
                    e.aux,
                    now.saturating_sub(e.start_ns),
                    table.len(),
                ))
            }
            None => None,
        }
    };
    let Some((kind, pid, name, detail, aux, waited_ns, outstanding)) = overdue else {
        return;
    };
    let dropped = SUPPRESSED.swap(0, Ordering::Relaxed);
    // Outside the lock: `klog_warn!` takes the console's own.
    zcore_drivers::klog_warn!(
        "[stall] {} for pid {} ({}) {}={:#x} {}={} waiting {} ms, {} wait(s) outstanding{}",
        kind.name(),
        pid,
        name,
        kind.detail_label(),
        detail,
        kind.aux_label(),
        aux,
        waited_ns / 1_000_000,
        outstanding,
        if dropped == 0 {
            String::new()
        } else {
            alloc::format!(" (+{dropped} scans suppressed)")
        }
    );
}

/// `/proc/stalls`: every wait outstanding right now, longest first.
pub fn render() -> String {
    if !enabled() {
        return String::from("[stalls] off (stalls=off)\n");
    }
    let now = kernel_hal::timer::timer_now().as_nanos() as u64;
    let mut rows: Vec<(u64, Kind, u64, String, u64, i64)> = {
        let table = STALLS.lock();
        table
            .iter()
            .map(|e| {
                (
                    now.saturating_sub(e.start_ns),
                    e.kind,
                    e.pid,
                    e.name.clone(),
                    e.detail,
                    e.aux,
                )
            })
            .collect()
    };
    // Longest-waiting first: the oldest wait is the one that explains the
    // others, and `Reverse` is how a descending key sort is spelled.
    rows.sort_unstable_by_key(|r| core::cmp::Reverse(r.0));
    let mut s = alloc::format!(
        "[stalls] {} outstanding, annunciated past {} ms\n",
        rows.len(),
        STALL_AFTER / 1_000_000
    );
    for (age_ns, kind, pid, name, detail, aux) in rows {
        s.push_str(&alloc::format!(
            "  {:>8} ms  {:<16} pid {:<6} {:<16} {}={:#x} {}={}\n",
            age_ns / 1_000_000,
            kind.name(),
            pid,
            name,
            kind.detail_label(),
            detail,
            kind.aux_label(),
            aux
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `STALLS` and the gate are process-global and cargo runs a crate's tests
    /// in threads, so the tests that touch them take this first. The pure
    /// decision functions below take nothing and need nothing.
    static SERIAL: spin::Mutex<()> = spin::Mutex::new(());

    fn drain() {
        STALLS.lock().clear();
        LAST_SCAN_NS.store(0, Ordering::SeqCst);
        SUPPRESSED.store(0, Ordering::SeqCst);
        ENABLED.store(true, Ordering::SeqCst);
    }

    #[test]
    fn a_wait_shorter_than_the_threshold_is_not_a_stall() {
        assert!(!is_stalled(1_000, 0, STALL_AFTER));
        assert!(!is_stalled(STALL_AFTER - 1, 0, STALL_AFTER));
    }

    #[test]
    fn a_wait_at_the_threshold_is_a_stall() {
        assert!(is_stalled(STALL_AFTER, 0, STALL_AFTER));
        assert!(is_stalled(STALL_AFTER * 3, 0, STALL_AFTER));
    }

    /// A clock that reads before the registration cannot make a wait look
    /// like it has been outstanding for the whole of `u64`.
    #[test]
    fn a_clock_that_went_backwards_is_not_a_stall() {
        assert!(!is_stalled(5, 1_000_000_000, STALL_AFTER));
    }

    #[test]
    fn the_gate_lets_the_first_line_through_and_then_holds() {
        assert!(scan_is_due(0, 0, SCAN_MIN_GAP), "never scanned");
        assert!(!scan_is_due(1_000, 1, SCAN_MIN_GAP));
        assert!(scan_is_due(SCAN_MIN_GAP + 1, 1, SCAN_MIN_GAP));
    }

    /// The gate must not latch shut when the clock goes backwards: `now <
    /// last` is a scan that is due, not one that has to wait out a gap it can
    /// never reach.
    #[test]
    fn the_gate_does_not_latch_shut_on_a_clock_that_went_backwards() {
        assert!(scan_is_due(5, 1_000_000_000, SCAN_MIN_GAP));
    }

    /// Shared and private futexes have to read differently, because which one
    /// it is decides where to look next.
    #[test]
    fn a_shared_futex_and_a_private_one_are_named_apart() {
        assert_ne!(Kind::FutexShared.name(), Kind::FutexPrivate.name());
        assert!(Kind::FutexShared.name().contains("shared"));
    }

    #[test]
    fn every_kind_labels_both_of_its_numbers() {
        for kind in [
            Kind::FutexShared,
            Kind::FutexPrivate,
            Kind::SyncobjWait,
            Kind::SyncFilePoll,
        ] {
            assert!(!kind.name().is_empty());
            assert!(!kind.detail_label().is_empty());
            assert!(!kind.aux_label().is_empty());
        }
    }

    #[test]
    fn the_hatch_is_on_by_default_and_off_records_nothing() {
        let _s = SERIAL.lock();
        drain();
        assert!(enabled(), "on by default");
        set_enabled(false);
        assert!(register(Kind::FutexShared, 0x1000, -1).is_none());
        assert_eq!(STALLS.lock().len(), 0);
        set_enabled(true);
        drain();
    }

    /// The guard is what bounds an entry's life. A wait that ends has to leave
    /// the table, or `/proc/stalls` fills with waits that are long over.
    #[test]
    fn a_guard_dropped_takes_its_entry_with_it() {
        let _s = SERIAL.lock();
        drain();
        let g = register(Kind::SyncobjWait, 7, 3).expect("on");
        assert_eq!(STALLS.lock().len(), 1);
        drop(g);
        assert_eq!(STALLS.lock().len(), 0);
        drain();
    }

    /// Two waits of the same shape are two entries: a guard removes its own
    /// and not the other one's.
    #[test]
    fn two_waits_on_the_same_word_are_two_entries() {
        let _s = SERIAL.lock();
        drain();
        let a = register(Kind::FutexShared, 0x2000, -1).expect("on");
        let b = register(Kind::FutexShared, 0x2000, -1).expect("on");
        assert_eq!(STALLS.lock().len(), 2);
        drop(a);
        assert_eq!(STALLS.lock().len(), 1);
        drop(b);
        assert_eq!(STALLS.lock().len(), 0);
        drain();
    }

    /// `register` scans BEFORE it pushes, so the wait being registered can
    /// never be the one annunciated -- its age is zero.
    #[test]
    fn a_fresh_wait_is_never_its_own_stall() {
        let _s = SERIAL.lock();
        drain();
        let g = register(Kind::FutexShared, 0x3000, -1).expect("on");
        assert!(
            !STALLS.lock()[0].reported,
            "a wait that just started is not overdue"
        );
        drop(g);
        drain();
    }

    /// The whole point of the split: an idle thread pool must not put a line
    /// in the log every gap for ever.
    #[test]
    fn a_private_futex_is_recorded_but_never_annunciated() {
        assert!(!Kind::FutexPrivate.annunciate());
        assert!(Kind::FutexShared.annunciate());
        assert!(Kind::SyncobjWait.annunciate());
        assert!(Kind::SyncFilePoll.annunciate());
    }

    /// Recorded means recorded: a private futex still shows in
    /// `/proc/stalls`, which is where a parked thread pool belongs.
    #[test]
    fn a_private_futex_still_shows_in_the_table() {
        let _s = SERIAL.lock();
        drain();
        let g = register(Kind::FutexPrivate, 0x4000, 0).expect("on");
        let out = render();
        assert!(out.contains("futex(private)"), "{}", out);
        assert!(out.contains("1 outstanding"), "{}", out);
        drop(g);
        drain();
    }

    /// The scan picks the oldest overdue entry, and skips one whose kind does
    /// not annunciate however long it has been there.
    #[test]
    fn the_scan_skips_a_private_futex_and_takes_the_shared_one() {
        let _s = SERIAL.lock();
        drain();
        let a = register(Kind::FutexPrivate, 0x5000, 0).expect("on");
        let b = register(Kind::FutexShared, 0x5001, -1).expect("on");
        // Backdate both past the threshold, the private one the older.
        {
            let mut t = STALLS.lock();
            t[0].start_ns = 1;
            t[1].start_ns = 2;
        }
        let now = STALL_AFTER + 10;
        LAST_SCAN_NS.store(0, Ordering::SeqCst);
        scan(now);
        let reported: Vec<(Kind, bool)> =
            STALLS.lock().iter().map(|e| (e.kind, e.reported)).collect();
        assert_eq!(
            reported,
            alloc::vec![(Kind::FutexPrivate, false), (Kind::FutexShared, true)]
        );
        drop(a);
        drop(b);
        drain();
    }

    /// An entry gets one line, not one per scan.
    #[test]
    fn an_entry_is_annunciated_once() {
        let _s = SERIAL.lock();
        drain();
        let g = register(Kind::SyncobjWait, 9, 1).expect("on");
        STALLS.lock()[0].start_ns = 1;
        let now = STALL_AFTER + 10;
        LAST_SCAN_NS.store(0, Ordering::SeqCst);
        scan(now);
        assert!(STALLS.lock()[0].reported);
        // A second scan finds nothing to say, so the gate is not spent and
        // the suppressed count does not move.
        LAST_SCAN_NS.store(0, Ordering::SeqCst);
        SUPPRESSED.store(0, Ordering::SeqCst);
        scan(now + 1);
        assert_eq!(SUPPRESSED.load(Ordering::SeqCst), 0);
        drop(g);
        drain();
    }

    #[test]
    fn render_says_off_when_it_is_off() {
        let _s = SERIAL.lock();
        drain();
        set_enabled(false);
        assert!(render().contains("off"));
        set_enabled(true);
        drain();
    }

    #[test]
    fn render_lists_an_outstanding_wait_with_both_of_its_numbers() {
        let _s = SERIAL.lock();
        drain();
        let g = register(Kind::SyncFilePoll, 0x2a, 5).expect("on");
        let out = render();
        assert!(out.contains("poll(sync_file)"), "{}", out);
        assert!(out.contains("handle=0x2a"), "{}", out);
        assert!(out.contains("point=5"), "{}", out);
        assert!(out.contains("1 outstanding"), "{}", out);
        drop(g);
        assert!(render().contains("0 outstanding"));
        drain();
    }
}
