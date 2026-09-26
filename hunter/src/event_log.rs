//! Tamper-evident forensic event log for the hunter security subsystem.
//!
//! Every security-relevant decision (a blocked syscall, a suspicious exec, a
//! W^X violation, a detected anomaly) is recorded as a structured [`LogEntry`]
//! and accounted in cheap lock-free [`Stats`] counters.
//!
//! Hardening (P10/P11): a single evictable ring let an attacker flood benign
//! events to silently evict attack evidence (finding IDS-1/EVADE-1). The log is
//! therefore split into **two rings** — a large evictable one for routine
//! events and a separate reserve for evidence — so a flood of noise can never
//! evict evidence. Eviction is surfaced per ring (`dropped`,
//! `critical_dropped`) so an operator can never miss that evidence was lost.
//! An optional [`set_sink`] callback lets the kernel stream `Warning`+ events
//! to a durable off-ring sink (serial / console) before any eviction can erase
//! them.
//!
//! **What "evidence" means here is what hunter DID, not how loud the event
//! sounds.** The split used to route by [`Severity`], and that put the flood
//! straight back: [`crate::heuristics`] calls a module load or a `kexec_load`
//! `Warning` even when hunter is only watching it, so any process could fill
//! the reserve with sixteen failing `init_module` calls per window across
//! sixteen pids and flush out every blocked exec in it — IDS-1/EVADE-1 again,
//! one storey up. The ring is now chosen by the [`Verdict`] behind the event's
//! `action`: hunter blocked it, hunter reported it and let it through, or
//! hunter merely looked on. The durable sink still sees every `Warning`+
//! event, watched ones included, so nothing that used to reach serial stopped
//! reaching it.

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use lock::Mutex;

use crate::clock;

lazy_static::lazy_static! {
    /// Evictable ring for routine `Info`/`Notice` events.
    static ref GENERAL_LOG: Mutex<IntrusionLog> = Mutex::new(IntrusionLog::new(512));
    /// Reserved ring for `Warning`/`Critical` evidence; far slower to evict
    /// because low-severity noise cannot land here.
    static ref PRIORITY_LOG: Mutex<IntrusionLog> = Mutex::new(IntrusionLog::new(256));
}

/// Monotonic sequence number handed to every recorded event.
static SEQ: AtomicU64 = AtomicU64::new(0);

// Lock-free running totals, so a `/proc/hunter` read or a health check never
// needs to walk (or lock) either ring.
static TOTAL: AtomicU64 = AtomicU64::new(0);
static BLOCKED: AtomicU64 = AtomicU64::new(0);
static WARNINGS: AtomicU64 = AtomicU64::new(0);
/// Report-mode violations that were logged but allowed to proceed.
static WARNINGS_ALLOWED: AtomicU64 = AtomicU64::new(0);
static CRITICALS: AtomicU64 = AtomicU64::new(0);
/// Low-severity events evicted from the general ring.
static DROPPED: AtomicU64 = AtomicU64::new(0);
/// High-severity events evicted from the priority ring (should stay ~0).
static CRITICAL_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Optional durable sink for `Warning`+ events, stored as an erased pointer.
static SINK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Severity of a security event, ordered from least to most urgent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Informational; routine bookkeeping (subsystem init, policy changes).
    Info,
    /// Noteworthy but expected (a watched-but-allowed sensitive syscall).
    Notice,
    /// A policy violation or suspicious behaviour.
    Warning,
    /// A high-confidence attack indicator (active enforcement kicked in).
    Critical,
}

impl Severity {
    /// Short, fixed-width tag used in the rendered report.
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "INFO",
            Severity::Notice => "NOTICE",
            Severity::Warning => "WARN",
            Severity::Critical => "CRIT",
        }
    }
    /// High-severity events go to the reserved ring and the durable sink.
    fn is_priority(self) -> bool {
        matches!(self, Severity::Warning | Severity::Critical)
    }
}

/// What hunter *did* about an event, behind the `action` string that
/// `/proc/hunter` renders.
///
/// Two decisions turn on this — which counter moves and which ring the entry
/// lands in — and each used to be taken on its own: the counters by comparing
/// `action` against the two literals `"BLOCKED"` and `"WARNING"` in an
/// `if`/`else if` with no final arm, the ring by asking the severity whether it
/// was high. Both were wrong about the same events. `"WATCH"` (a watched
/// `kexec_load`) and `"ELF_BLOCKED"` (which [`log_event`] classifies as a block
/// in its own `match`) fell off the end of the `if`, so they moved neither
/// counter: `/proc/hunter` grew a `warnings=` it could not account for in
/// `blocked=` + `reported=`, which is the one question the line exists to
/// answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// hunter stopped the operation.
    Blocked,
    /// A violation hunter recorded and let proceed (report mode).
    Reported,
    /// hunter only looked on, or wrote bookkeeping: not a violation, and so
    /// not evidence. An action nobody has classified lands here, which keeps a
    /// new call site out of the reserve ring until someone decides otherwise.
    Observed,
}

impl Verdict {
    /// Whether hunter acted, which is what the reserve ring holds.
    fn is_evidence(self) -> bool {
        !matches!(self, Verdict::Observed)
    }
}

/// The verdict behind an `action` string, in the one place that decides it.
pub fn verdict(action: &str) -> Verdict {
    match action {
        "BLOCKED" | "ELF_BLOCKED" => Verdict::Blocked,
        "WARNING" => Verdict::Reported,
        _ => Verdict::Observed,
    }
}

/// A single recorded security event.
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// Monotonic sequence number (gaps reveal evicted events).
    pub seq: u64,
    /// Timestamp in nanoseconds since boot (0 before the clock is wired).
    pub ts_ns: u64,
    /// Offending / acting process id (0 = kernel / subsystem itself).
    pub pid: u64,
    /// Severity of the event.
    pub severity: Severity,
    /// Coarse domain, e.g. `"SYSCALL"`, `"EXEC"`, `"WX"`, `"ANOMALY"`.
    pub category: &'static str,
    /// What hunter did, e.g. `"BLOCKED"`, `"WARNING"`, `"ALLOWED"`.
    pub action: &'static str,
    /// Free-form, human-readable detail.
    pub description: String,
}

/// Bounded ring buffer of [`LogEntry`].
pub struct IntrusionLog {
    entries: Vec<LogEntry>,
    head: usize,
    max_size: usize,
}

impl IntrusionLog {
    /// Creates an empty log holding at most `max_size` entries.
    pub const fn new(max_size: usize) -> Self {
        Self {
            entries: Vec::new(),
            head: 0,
            max_size,
        }
    }

    /// Appends an entry. Returns `true` if an older entry was evicted.
    pub fn push(&mut self, entry: LogEntry) -> bool {
        if self.max_size == 0 {
            // A ring that holds nothing drops every event, and reporting the
            // drop is the whole answer: the caller counts it. Falling through
            // instead indexed `entries[0]` of an empty `Vec` and then divided
            // by zero, so the first event a zero-sized ring ever saw took the
            // kernel down -- from a bound the caller of `new` chooses.
            return true;
        }
        if self.entries.len() < self.max_size {
            self.entries.push(entry);
            false
        } else {
            // Overwrite in place so we never re-shift the whole Vec (O(1)).
            self.entries[self.head] = entry;
            self.head = (self.head + 1) % self.max_size;
            true
        }
    }

    /// Empties the ring. Test-only: the rings are process-wide and a test that
    /// counts what is in one has to start from empty.
    #[cfg(test)]
    pub fn clear(&mut self) {
        self.entries.clear();
        self.head = 0;
    }

    /// Returns the entries in chronological (oldest-first) order.
    pub fn get_entries(&self) -> Vec<LogEntry> {
        if self.entries.len() < self.max_size {
            return self.entries.clone();
        }
        let mut out = Vec::with_capacity(self.entries.len());
        out.extend_from_slice(&self.entries[self.head..]);
        out.extend_from_slice(&self.entries[..self.head]);
        out
    }
}

/// Lock-free snapshot of the running counters.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub total: u64,
    pub blocked: u64,
    pub warnings: u64,
    pub warnings_allowed: u64,
    pub criticals: u64,
    pub dropped: u64,
    pub critical_dropped: u64,
}

/// Returns a snapshot of the global event counters.
pub fn stats() -> Stats {
    Stats {
        total: TOTAL.load(Ordering::Relaxed),
        blocked: BLOCKED.load(Ordering::Relaxed),
        warnings: WARNINGS.load(Ordering::Relaxed),
        warnings_allowed: WARNINGS_ALLOWED.load(Ordering::Relaxed),
        criticals: CRITICALS.load(Ordering::Relaxed),
        dropped: DROPPED.load(Ordering::Relaxed),
        critical_dropped: CRITICAL_DROPPED.load(Ordering::Relaxed),
    }
}

/// Registers a durable sink invoked (outside all ring locks) for every
/// `Warning`/`Critical` event, so evidence reaches serial/console before any
/// in-memory eviction can erase it.
///
/// **Sealed, like [`crate::clock::set_time_source`]**: only the first
/// registration takes effect. The two are registered on adjacent lines of
/// `zCore`'s boot, and only one of them used to be sealed -- yet this sink is
/// the single copy of a high-severity event that outlives a ring eviction, so
/// code that could replace it could turn the durable half of the forensic log
/// off at runtime, which is the threat the clock's seal is spelled out
/// against. One compare-exchange, so the pointer is the seal and there is no
/// check-then-store for a second caller to land in.
pub fn set_sink(sink: fn(&LogEntry)) {
    let _ = SINK.compare_exchange(
        core::ptr::null_mut(),
        sink as *mut (),
        Ordering::SeqCst,
        Ordering::SeqCst,
    );
}

/// Empties both rings, zeroes every counter and forgets the durable sink.
///
/// All of it is process-wide, and [`set_sink`] is sealed on purpose, so a test
/// that wants a sink of its own is the only thing that may break that seal --
/// which is why this exists and why it is test-only. Callers hold
/// [`crate::test_globals::lock`].
#[cfg(test)]
pub(crate) fn reset_for_test() {
    GENERAL_LOG.lock().clear();
    PRIORITY_LOG.lock().clear();
    SEQ.store(0, Ordering::SeqCst);
    for c in [
        &TOTAL,
        &BLOCKED,
        &WARNINGS,
        &WARNINGS_ALLOWED,
        &CRITICALS,
        &DROPPED,
        &CRITICAL_DROPPED,
    ]
    .iter()
    {
        c.store(0, Ordering::SeqCst);
    }
    SINK.store(core::ptr::null_mut(), Ordering::SeqCst);
}

/// Records a fully-specified security event.
pub fn record(
    pid: u64,
    severity: Severity,
    category: &'static str,
    action: &'static str,
    description: String,
) {
    TOTAL.fetch_add(1, Ordering::Relaxed);
    match severity {
        Severity::Warning => {
            WARNINGS.fetch_add(1, Ordering::Relaxed);
        }
        Severity::Critical => {
            CRITICALS.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
    // What hunter did about it: one classifier, asked once, answering both the
    // counter below and the ring further down. `blocked` + `reported` now adds
    // up to every event hunter acted on, whatever its action is spelled.
    let verdict = verdict(action);
    match verdict {
        Verdict::Blocked => {
            BLOCKED.fetch_add(1, Ordering::Relaxed);
        }
        Verdict::Reported => {
            WARNINGS_ALLOWED.fetch_add(1, Ordering::Relaxed);
        }
        Verdict::Observed => {}
    }

    let entry = LogEntry {
        seq: SEQ.fetch_add(1, Ordering::Relaxed),
        ts_ns: clock::now_ns(),
        pid,
        severity,
        category,
        action,
        description,
    };

    // The durable sink sees every `Warning`+ event, acted on or not, because it
    // is the copy that survives an eviction. Read it and take its copy of the
    // entry BEFORE the ring lock: `Mutex::lock` disables interrupts, and an
    // `entry.clone()` written as the argument of `push` is evaluated after the
    // guard exists -- so the description's `String` was allocated inside the
    // critical section, with interrupts off, on every event hunter recorded.
    // Now nothing but the move into the ring happens under the lock, and an
    // event with no sink to stream to is not copied at all.
    let streamed = if severity.is_priority() {
        let p = SINK.load(Ordering::Acquire);
        if p.is_null() {
            None
        } else {
            // SAFETY: `p` is a valid `fn(&LogEntry)` sealed by `set_sink`.
            let sink: fn(&LogEntry) = unsafe { core::mem::transmute(p) };
            Some((sink, entry.clone()))
        }
    } else {
        None
    };

    // The reserve ring holds evidence; everything hunter merely watched is
    // routine and goes in the evictable one.
    let evidence = verdict.is_evidence();
    let evicted = if evidence {
        PRIORITY_LOG.lock().push(entry)
    } else {
        GENERAL_LOG.lock().push(entry)
    };
    if evicted {
        if evidence {
            CRITICAL_DROPPED.fetch_add(1, Ordering::Relaxed);
        } else {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }

    if let Some((sink, entry)) = streamed {
        sink(&entry);
    }
}

/// Back-compat helper: appends an event without explicit severity/category.
pub fn log_event(pid: u64, action: &'static str, description: String) {
    let severity = match action {
        "BLOCKED" => Severity::Critical,
        "WARNING" | "ELF_BLOCKED" => Severity::Warning,
        _ => Severity::Info,
    };
    record(pid, severity, "SYSCALL", action, description);
}

/// Formats a nanosecond timestamp as `<secs>.<ms>` since boot.
fn fmt_ts(ts_ns: u64) -> String {
    let secs = ts_ns / 1_000_000_000;
    let millis = (ts_ns % 1_000_000_000) / 1_000_000;
    format!("{}.{:03}", secs, millis)
}

/// Renders both rings, merged into chronological order, for `/proc/hunter`.
pub fn render() -> String {
    let mut entries = GENERAL_LOG.lock().get_entries();
    entries.extend(PRIORITY_LOG.lock().get_entries());
    entries.sort_by_key(|e| e.seq);

    let mut out = String::new();
    if entries.is_empty() {
        out.push_str("(no security events recorded)\n");
        return out;
    }
    for e in &entries {
        out.push_str(&format!(
            "[{:>6}] +{:>10}s pid={:<5} {:<6} {:<9} {}: {}\n",
            e.seq,
            fmt_ts(e.ts_ns),
            e.pid,
            e.severity.as_str(),
            e.category,
            e.action,
            e.description,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    //! The forensic log is the half of hunter an operator reads after the
    //! fact, and every test here names something it used to get wrong about
    //! its own two questions: *what did hunter do about this event*, and
    //! *what is it allowed to throw away*.
    //!
    //! All of this state is process-wide -- two rings, seven counters, a
    //! sealed sink, a sealed clock -- so every test takes
    //! [`crate::test_globals::lock`] and resets what it uses, the same way the
    //! policy tests do.

    use super::*;
    extern crate std;
    use core::sync::atomic::AtomicU64;

    /// How many times the durable sink has run, and the last severity it saw.
    static SINK_CALLS: AtomicU64 = AtomicU64::new(0);
    static SINK_LAST: AtomicU64 = AtomicU64::new(0);

    fn counting_sink(e: &LogEntry) {
        SINK_CALLS.fetch_add(1, Ordering::SeqCst);
        SINK_LAST.store(e.severity as u64, Ordering::SeqCst);
    }

    /// A second sink, so a test can tell which one ran.
    fn other_sink(_e: &LogEntry) {
        SINK_CALLS.fetch_add(1000, Ordering::SeqCst);
    }

    fn fresh() {
        reset_for_test();
        SINK_CALLS.store(0, Ordering::SeqCst);
        SINK_LAST.store(0, Ordering::SeqCst);
    }

    fn an_entry(seq: u64, severity: Severity) -> LogEntry {
        LogEntry {
            seq,
            ts_ns: 0,
            pid: 1,
            severity,
            category: "T",
            action: "BLOCKED",
            description: String::new(),
        }
    }

    // ---- what hunter did: the verdict behind the action string ----

    #[test]
    fn a_block_is_counted_as_a_block_whichever_way_its_action_is_spelled() {
        let _g = crate::test_globals::lock();
        fresh();
        record(1, Severity::Critical, "EXEC", "BLOCKED", String::new());
        // `log_event` calls this one a block in its own `match`, then handed it
        // to a counter that only knew the string "BLOCKED".
        log_event(1, "ELF_BLOCKED", String::from("bad elf"));
        let s = stats();
        assert_eq!(s.blocked, 2);
        assert_eq!(s.warnings_allowed, 0);
    }

    #[test]
    fn a_violation_that_was_let_through_is_counted_as_reported() {
        let _g = crate::test_globals::lock();
        fresh();
        record(1, Severity::Warning, "EXEC", "WARNING", String::new());
        let s = stats();
        assert_eq!(s.warnings_allowed, 1);
        assert_eq!(s.blocked, 0);
    }

    #[test]
    fn an_event_hunter_only_watched_is_counted_in_neither_column() {
        let _g = crate::test_globals::lock();
        fresh();
        // A watched `kexec_load`: `classify` calls it a `Warning` although
        // hunter did nothing about it.
        record(1, Severity::Warning, "MODULE", "WATCH", String::new());
        record(0, Severity::Notice, "CONTROL", "CONFIG", String::new());
        record(0, Severity::Info, "SYSTEM", "INIT", String::new());
        let s = stats();
        assert_eq!(s.total, 3);
        assert_eq!(s.blocked, 0);
        assert_eq!(s.warnings_allowed, 0);
    }

    #[test]
    fn the_books_add_up_to_every_event_hunter_acted_on() {
        let _g = crate::test_globals::lock();
        fresh();
        let events: &[(Severity, &'static str)] = &[
            (Severity::Critical, "BLOCKED"),
            (Severity::Warning, "BLOCKED"),
            (Severity::Warning, "WARNING"),
            (Severity::Warning, "WATCH"),
            (Severity::Notice, "WATCH"),
            (Severity::Notice, "LEARN"),
            (Severity::Notice, "CONFIG"),
            (Severity::Info, "INIT"),
            (Severity::Warning, "ELF_BLOCKED"),
        ];
        for (severity, action) in events.iter() {
            record(1, *severity, "T", action, String::new());
        }
        let acted = events
            .iter()
            .filter(|(_, a)| verdict(a).is_evidence())
            .count() as u64;
        let s = stats();
        assert_eq!(s.total, events.len() as u64);
        assert_eq!(s.blocked + s.warnings_allowed, acted);
        assert_eq!(s.blocked, 3);
        assert_eq!(s.warnings_allowed, 1);
        // Everything hunter merely looked on at is in neither column, and that
        // is now one rule rather than the end of an `if`/`else if` chain.
        assert_eq!(s.total - s.blocked - s.warnings_allowed, 5);
    }

    #[test]
    fn an_action_nobody_classified_is_treated_as_looking_on() {
        assert_eq!(verdict("BLOCKED"), Verdict::Blocked);
        assert_eq!(verdict("ELF_BLOCKED"), Verdict::Blocked);
        assert_eq!(verdict("WARNING"), Verdict::Reported);
        assert_eq!(verdict("WATCH"), Verdict::Observed);
        assert_eq!(verdict("SOMETHING_NEW"), Verdict::Observed);
        assert!(!verdict("WATCH").is_evidence());
        assert!(verdict("BLOCKED").is_evidence());
        assert!(verdict("WARNING").is_evidence());
    }

    #[test]
    fn the_severity_counters_stay_a_histogram_of_severity() {
        let _g = crate::test_globals::lock();
        fresh();
        // `check_elf_binary`'s deny path really does record a `Warning` whose
        // action is "BLOCKED": the two questions are separate and both answers
        // have to land.
        record(1, Severity::Warning, "EXEC", "BLOCKED", String::new());
        let s = stats();
        assert_eq!(s.warnings, 1);
        assert_eq!(s.criticals, 0);
        assert_eq!(s.blocked, 1);
    }

    // ---- what the log may throw away ----

    #[test]
    fn a_flood_of_watched_module_loads_cannot_evict_a_blocked_exec() {
        let _g = crate::test_globals::lock();
        fresh();
        record(
            1,
            Severity::Critical,
            "EXEC",
            "BLOCKED",
            String::from("THE-EVIDENCE"),
        );
        // Sixteen failing `init_module` calls per window across sixteen pids
        // is all this takes from an unprivileged process, and `classify` calls
        // every one of them a `Warning`.
        for i in 0..2000 {
            record(
                2,
                Severity::Warning,
                "MODULE",
                "WATCH",
                format!("noise {}", i),
            );
        }
        assert!(render().contains("THE-EVIDENCE"));
        let s = stats();
        assert_eq!(s.critical_dropped, 0);
        assert!(s.dropped > 0);
    }

    #[test]
    fn a_flood_of_routine_events_cannot_evict_evidence_either() {
        let _g = crate::test_globals::lock();
        fresh();
        record(
            1,
            Severity::Critical,
            "EXEC",
            "BLOCKED",
            String::from("THE-EVIDENCE"),
        );
        for i in 0..2000 {
            record(2, Severity::Info, "SYSCALL", "INIT", format!("noise {}", i));
        }
        assert!(render().contains("THE-EVIDENCE"));
        assert_eq!(stats().critical_dropped, 0);
    }

    #[test]
    fn the_reserve_is_finite_and_says_so_when_evidence_falls_out_of_it() {
        let _g = crate::test_globals::lock();
        fresh();
        record(
            1,
            Severity::Critical,
            "EXEC",
            "BLOCKED",
            String::from("THE-OLDEST"),
        );
        for i in 0..400 {
            record(2, Severity::Critical, "EXEC", "BLOCKED", format!("b {}", i));
        }
        // Evidence can only be pushed out by more evidence, and the counter an
        // operator watches is the one that says it happened.
        assert!(!render().contains("THE-OLDEST"));
        assert!(stats().critical_dropped > 0);
    }

    #[test]
    fn the_report_merges_both_rings_back_into_the_order_they_happened() {
        let _g = crate::test_globals::lock();
        fresh();
        record(1, Severity::Info, "SYSTEM", "INIT", String::from("one"));
        record(
            1,
            Severity::Critical,
            "EXEC",
            "BLOCKED",
            String::from("two"),
        );
        record(
            1,
            Severity::Notice,
            "CONTROL",
            "CONFIG",
            String::from("three"),
        );
        record(
            1,
            Severity::Warning,
            "EXEC",
            "WARNING",
            String::from("four"),
        );
        let out = render();
        let at = |needle: &str| out.find(needle).expect("entry missing from the report");
        assert!(at("one") < at("two"));
        assert!(at("two") < at("three"));
        assert!(at("three") < at("four"));
    }

    #[test]
    fn the_report_says_so_when_nothing_has_been_recorded() {
        let _g = crate::test_globals::lock();
        fresh();
        assert_eq!(render(), "(no security events recorded)\n");
    }

    // ---- the ring itself ----

    #[test]
    fn a_ring_that_holds_nothing_drops_every_event_instead_of_panicking() {
        let mut ring = IntrusionLog::new(0);
        assert!(ring.push(an_entry(0, Severity::Critical)));
        assert!(ring.push(an_entry(1, Severity::Critical)));
        assert!(ring.get_entries().is_empty());
    }

    #[test]
    fn a_ring_hands_back_what_it_was_given_before_it_wraps() {
        let mut ring = IntrusionLog::new(3);
        assert!(!ring.push(an_entry(0, Severity::Info)));
        assert!(!ring.push(an_entry(1, Severity::Info)));
        let seqs: Vec<u64> = ring.get_entries().iter().map(|e| e.seq).collect();
        assert_eq!(seqs, alloc::vec![0, 1]);
    }

    #[test]
    fn a_ring_hands_back_the_newest_entries_oldest_first_once_it_wraps() {
        let mut ring = IntrusionLog::new(3);
        for seq in 0..5 {
            let evicted = ring.push(an_entry(seq, Severity::Info));
            assert_eq!(evicted, seq >= 3);
        }
        let seqs: Vec<u64> = ring.get_entries().iter().map(|e| e.seq).collect();
        assert_eq!(seqs, alloc::vec![2, 3, 4]);
    }

    #[test]
    fn a_ring_of_one_keeps_only_the_newest_entry() {
        let mut ring = IntrusionLog::new(1);
        assert!(!ring.push(an_entry(0, Severity::Info)));
        assert!(ring.push(an_entry(1, Severity::Info)));
        assert!(ring.push(an_entry(2, Severity::Info)));
        let seqs: Vec<u64> = ring.get_entries().iter().map(|e| e.seq).collect();
        assert_eq!(seqs, alloc::vec![2]);
    }

    // ---- the durable sink ----

    #[test]
    fn the_durable_sink_cannot_be_replaced_once_it_is_registered() {
        let _g = crate::test_globals::lock();
        fresh();
        set_sink(counting_sink);
        set_sink(other_sink);
        record(1, Severity::Critical, "EXEC", "BLOCKED", String::new());
        // `other_sink` adds a thousand; one call means the first sink ran and
        // the replacement did not.
        assert_eq!(SINK_CALLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_durable_sink_sees_every_high_severity_event_watched_ones_included() {
        let _g = crate::test_globals::lock();
        fresh();
        set_sink(counting_sink);
        record(1, Severity::Critical, "EXEC", "BLOCKED", String::new());
        record(1, Severity::Warning, "EXEC", "WARNING", String::new());
        // Routed to the evictable ring, but still streamed: the durable copy
        // is what makes that routing safe.
        record(1, Severity::Warning, "MODULE", "WATCH", String::new());
        assert_eq!(SINK_CALLS.load(Ordering::SeqCst), 3);
        assert_eq!(SINK_LAST.load(Ordering::SeqCst), Severity::Warning as u64);
    }

    #[test]
    fn the_durable_sink_is_not_woken_for_routine_events() {
        let _g = crate::test_globals::lock();
        fresh();
        set_sink(counting_sink);
        record(0, Severity::Info, "SYSTEM", "INIT", String::new());
        record(0, Severity::Notice, "CONTROL", "CONFIG", String::new());
        assert_eq!(SINK_CALLS.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn an_event_recorded_with_no_sink_still_reaches_the_ring() {
        let _g = crate::test_globals::lock();
        fresh();
        record(
            1,
            Severity::Critical,
            "EXEC",
            "BLOCKED",
            String::from("no-sink"),
        );
        assert_eq!(SINK_CALLS.load(Ordering::SeqCst), 0);
        assert!(render().contains("no-sink"));
    }

    // ---- what an entry carries ----

    #[test]
    fn every_event_carries_the_next_sequence_number_and_the_clock_reading() {
        let _g = crate::test_globals::lock();
        fresh();
        clock::reset_for_test();
        clock::set_time_source(|| 1_500_000_000);
        record(9, Severity::Critical, "EXEC", "BLOCKED", String::from("a"));
        record(9, Severity::Critical, "EXEC", "BLOCKED", String::from("b"));
        let entries = PRIORITY_LOG.lock().get_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[1].seq, 1);
        assert_eq!(entries[0].ts_ns, 1_500_000_000);
        assert_eq!(entries[0].pid, 9);
        clock::reset_for_test();
    }

    #[test]
    fn a_timestamp_renders_as_seconds_and_milliseconds() {
        assert_eq!(fmt_ts(0), "0.000");
        assert_eq!(fmt_ts(999_999_999), "0.999");
        assert_eq!(fmt_ts(1_500_000_000), "1.500");
        assert_eq!(fmt_ts(61_002_000_000), "61.002");
    }

    #[test]
    fn a_severity_renders_under_its_own_fixed_width_tag() {
        assert_eq!(Severity::Info.as_str(), "INFO");
        assert_eq!(Severity::Notice.as_str(), "NOTICE");
        assert_eq!(Severity::Warning.as_str(), "WARN");
        assert_eq!(Severity::Critical.as_str(), "CRIT");
        assert!(Severity::Critical > Severity::Warning);
        assert!(Severity::Warning > Severity::Notice);
        assert!(Severity::Notice > Severity::Info);
    }

    // ---- the clock ----

    #[test]
    fn an_unregistered_clock_reads_as_zero_and_unsealed() {
        let _g = crate::test_globals::lock();
        clock::reset_for_test();
        assert!(!clock::is_sealed());
        assert_eq!(clock::now_ns(), 0);
    }

    #[test]
    fn the_first_clock_registered_is_the_one_that_answers_for_good() {
        let _g = crate::test_globals::lock();
        clock::reset_for_test();
        clock::set_time_source(|| 7);
        assert!(clock::is_sealed());
        assert_eq!(clock::now_ns(), 7);
        // An attacker who reaches the mutator must not be able to stop the
        // clock and silence the rate detectors behind it.
        clock::set_time_source(|| 0);
        clock::set_time_source(|| 9);
        assert_eq!(clock::now_ns(), 7);
        clock::reset_for_test();
    }

    #[test]
    fn a_sealed_clock_always_has_a_source_behind_it() {
        let _g = crate::test_globals::lock();
        clock::reset_for_test();
        assert_eq!(clock::is_sealed(), clock::now_ns() != 0);
        clock::set_time_source(|| 42);
        // The seal IS the pointer, so the two cannot answer differently --
        // which is what a separate flag set before the store allowed.
        assert_eq!(clock::is_sealed(), clock::now_ns() != 0);
        clock::reset_for_test();
    }
}
