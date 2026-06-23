//! Eclipse's own lightweight kernel observability ("our own perf").
//!
//! Rather than emulating the Linux `perf` tool's ring-buffer ABI, this is a
//! homegrown, always-on accounting layer surfaced as plain text:
//!
//! - **`/proc/perf`** — system-wide syscall accounting (calls + time per
//!   syscall, busiest first).
//! - **`/proc/<pid>/perf`** — the same broken down for one process.
//!
//! The syscall dispatcher calls [`record`] once per syscall with the elapsed
//! time; everything here is lock-free atomics on the hot path. Report rendering
//! (rare) resolves syscall numbers to names through a resolver registered by
//! `linux-syscall` (which owns the `Sys` enum), so this crate needs no
//! arch-specific name table.

use crate::process::LinuxProcess;
use alloc::{boxed::Box, string::String, vec::Vec};
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use lock::Mutex;

/// Size of the per-syscall tables. Must exceed the largest syscall number in
/// use (Eclipse's custom syscalls go up to ~601).
pub const PERF_NR: usize = 640;

/// System-wide call counts, indexed by syscall number.
static SYS_COUNT: [AtomicU64; PERF_NR] = [const { AtomicU64::new(0) }; PERF_NR];
/// System-wide cumulative time spent in each syscall, in nanoseconds.
static SYS_NS: [AtomicU64; PERF_NR] = [const { AtomicU64::new(0) }; PERF_NR];

/// Resolver from syscall number to a human name, registered by `linux-syscall`.
static NAME_RESOLVER: Mutex<Option<fn(u32) -> Option<String>>> = Mutex::new(None);

/// Register the syscall-name resolver. Called once by `linux-syscall`.
pub fn set_name_resolver(f: fn(u32) -> Option<String>) {
    *NAME_RESOLVER.lock() = Some(f);
}

fn name_of(num: u32) -> String {
    if let Some(f) = *NAME_RESOLVER.lock() {
        if let Some(n) = f(num) {
            return n;
        }
    }
    alloc::format!("sys_{}", num)
}

/// Record one completed syscall: bump the global tables and the calling
/// process's own counters. `ns` is the wall-clock time the syscall took.
pub fn record(proc: &LinuxProcess, num: u32, ns: u64) {
    if (num as usize) < PERF_NR {
        SYS_COUNT[num as usize].fetch_add(1, Relaxed);
        SYS_NS[num as usize].fetch_add(ns, Relaxed);
    }
    proc.perf().record(num, ns);
}

/// Per-process syscall accounting, stored inline on [`LinuxProcess`] so it is
/// freed with the process.
pub struct ProcPerf {
    count: AtomicU64,
    ns: AtomicU64,
    per: Box<[AtomicU32]>,
}

impl Default for ProcPerf {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcPerf {
    /// Create a zeroed per-process accounting table.
    pub fn new() -> Self {
        let mut per = Vec::with_capacity(PERF_NR);
        for _ in 0..PERF_NR {
            per.push(AtomicU32::new(0));
        }
        ProcPerf {
            count: AtomicU64::new(0),
            ns: AtomicU64::new(0),
            per: per.into_boxed_slice(),
        }
    }

    fn record(&self, num: u32, ns: u64) {
        self.count.fetch_add(1, Relaxed);
        self.ns.fetch_add(ns, Relaxed);
        if (num as usize) < self.per.len() {
            self.per[num as usize].fetch_add(1, Relaxed);
        }
    }

    /// `(total calls, total nanoseconds)`.
    pub fn totals(&self) -> (u64, u64) {
        (self.count.load(Relaxed), self.ns.load(Relaxed))
    }
}

fn fmt_table(out: &mut String, mut rows: Vec<(u32, u64, u64)>) {
    // Busiest syscall first.
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    let _ = writeln!(
        out,
        "  {:<20} {:>12} {:>12} {:>10}",
        "SYSCALL", "CALLS", "TOTAL ms", "AVG us"
    );
    for (num, calls, ns) in rows {
        if calls == 0 {
            continue;
        }
        let total_ms = ns as f64 / 1_000_000.0;
        let avg_us = if calls > 0 {
            (ns as f64 / calls as f64) / 1000.0
        } else {
            0.0
        };
        let _ = writeln!(
            out,
            "  {:<20} {:>12} {:>12.3} {:>10.2}",
            name_of(num),
            calls,
            total_ms,
            avg_us
        );
    }
}

/// Render `/proc/perf`: system-wide syscall accounting.
pub fn global_report() -> String {
    let uptime = kernel_hal::timer::timer_now().as_secs_f64();
    let mut total_calls = 0u64;
    let mut rows: Vec<(u32, u64, u64)> = Vec::new();
    for i in 0..PERF_NR {
        let calls = SYS_COUNT[i].load(Relaxed);
        if calls != 0 {
            let ns = SYS_NS[i].load(Relaxed);
            total_calls += calls;
            rows.push((i as u32, calls, ns));
        }
    }
    let mut out = String::new();
    let _ = writeln!(out, "eclipse perf — system-wide syscall accounting");
    let _ = writeln!(out);
    let rate = if uptime > 0.0 {
        total_calls as f64 / uptime
    } else {
        0.0
    };
    let _ = writeln!(out, "uptime:         {:.2} s", uptime);
    let _ = writeln!(out, "syscalls total: {} ({:.0}/s avg)", total_calls, rate);
    let _ = writeln!(out);
    fmt_table(&mut out, rows);
    out
}

/// Render `/proc/<pid>/perf`: one process's syscall accounting.
pub fn proc_report(proc: &LinuxProcess, pid: u64) -> String {
    let perf = proc.perf();
    let (total, ns) = perf.totals();
    let mut rows: Vec<(u32, u64, u64)> = Vec::new();
    for i in 0..perf.per.len() {
        let calls = perf.per[i].load(Relaxed) as u64;
        if calls != 0 {
            rows.push((i as u32, calls, 0));
        }
    }
    let mut out = String::new();
    let path = proc.execute_path();
    let name = if path.is_empty() { "?" } else { path.as_str() };
    let _ = writeln!(out, "eclipse perf — pid {} ({})", pid, name);
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "syscalls total: {}   time in syscalls: {:.3} ms",
        total,
        ns as f64 / 1_000_000.0
    );
    let _ = writeln!(out);
    // Per-process timing is not tracked per-syscall (only totals), so the
    // TOTAL/AVG columns are left blank here; the CALLS breakdown is what
    // distinguishes processes.
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    let _ = writeln!(out, "  {:<20} {:>12}", "SYSCALL", "CALLS");
    for (num, calls, _) in rows {
        let _ = writeln!(out, "  {:<20} {:>12}", name_of(num), calls);
    }
    out
}
