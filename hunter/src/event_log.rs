//! Forensic event log for the hunter security subsystem.
//!
//! Every security-relevant decision (a blocked syscall, a suspicious exec, a
//! W^X violation, a detected anomaly) is recorded here as a structured
//! [`LogEntry`] in a bounded ring buffer, and accounted in cheap lock-free
//! [`Stats`] counters. The ring is rendered as human-readable text through
//! [`render`] and surfaced to userspace at `/proc/hunter`.

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use lock::Mutex;

use crate::clock;

lazy_static::lazy_static! {
    /// Global ring buffer of security incidents.
    pub static ref GLOBAL_LOG: Mutex<IntrusionLog> = Mutex::new(IntrusionLog::new(512));
}

/// Monotonic sequence number handed to every recorded event.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Lock-free running totals, so a `/proc/hunter` read or a quick health check
/// never needs to walk (or lock) the whole ring.
static TOTAL: AtomicU64 = AtomicU64::new(0);
static BLOCKED: AtomicU64 = AtomicU64::new(0);
static WARNINGS: AtomicU64 = AtomicU64::new(0);
static CRITICALS: AtomicU64 = AtomicU64::new(0);
/// Number of events dropped because the ring wrapped (oldest evicted).
static DROPPED: AtomicUsize = AtomicUsize::new(0);

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

    /// Appends an entry, evicting the oldest if the ring is full.
    pub fn push(&mut self, entry: LogEntry) {
        if self.entries.len() < self.max_size {
            self.entries.push(entry);
        } else {
            // Overwrite in place so we never re-shift the whole Vec (O(1)).
            self.entries[self.head] = entry;
            self.head = (self.head + 1) % self.max_size;
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
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
    pub criticals: u64,
    pub dropped: usize,
}

/// Returns a snapshot of the global event counters.
pub fn stats() -> Stats {
    Stats {
        total: TOTAL.load(Ordering::Relaxed),
        blocked: BLOCKED.load(Ordering::Relaxed),
        warnings: WARNINGS.load(Ordering::Relaxed),
        criticals: CRITICALS.load(Ordering::Relaxed),
        dropped: DROPPED.load(Ordering::Relaxed),
    }
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
    if action == "BLOCKED" {
        BLOCKED.fetch_add(1, Ordering::Relaxed);
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
    GLOBAL_LOG.lock().push(entry);
}

/// Back-compat helper: appends an event without explicit severity/category.
///
/// The severity is inferred from the legacy `action` string so existing
/// call sites keep working while new code uses [`record`].
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

/// Renders the recent event ring as human-readable text for `/proc/hunter`.
pub fn render() -> String {
    let entries = GLOBAL_LOG.lock().get_entries();
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
