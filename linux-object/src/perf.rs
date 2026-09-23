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
use alloc::collections::BTreeMap;
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
type NameResolver = fn(u32) -> Option<String>;
static NAME_RESOLVER: Mutex<Option<NameResolver>> = Mutex::new(None);

/// Register the syscall-name resolver. Called once by `linux-syscall`.
pub fn set_name_resolver(f: fn(u32) -> Option<String>) {
    *NAME_RESOLVER.lock() = Some(f);
}

/// Resolve a syscall number to a human name via the resolver registered by
/// `linux-syscall`, falling back to `sys_<n>`. Public so other observability
/// surfaces (e.g. `boot_trace`) can label syscalls too.
pub fn name_of(num: u32) -> String {
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
    /// Per-syscall call count.
    per: Box<[AtomicU32]>,
    /// Per-syscall cumulative time (ns), so `/proc/<pid>/perf` shows latency.
    per_ns: Box<[AtomicU64]>,
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
        let mut per_ns = Vec::with_capacity(PERF_NR);
        for _ in 0..PERF_NR {
            per.push(AtomicU32::new(0));
            per_ns.push(AtomicU64::new(0));
        }
        ProcPerf {
            count: AtomicU64::new(0),
            ns: AtomicU64::new(0),
            per: per.into_boxed_slice(),
            per_ns: per_ns.into_boxed_slice(),
        }
    }

    fn record(&self, num: u32, ns: u64) {
        self.count.fetch_add(1, Relaxed);
        self.ns.fetch_add(ns, Relaxed);
        if (num as usize) < self.per.len() {
            self.per[num as usize].fetch_add(1, Relaxed);
            self.per_ns[num as usize].fetch_add(ns, Relaxed);
        }
    }

    /// `(total calls, total nanoseconds)`.
    pub fn totals(&self) -> (u64, u64) {
        (self.count.load(Relaxed), self.ns.load(Relaxed))
    }
}

fn fmt_table(out: &mut String, mut rows: Vec<(u32, u64, u64)>) {
    // Busiest syscall first.
    rows.sort_by_key(|r| core::cmp::Reverse(r.1));
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

// ---------------------------------------------------------------------------
// Kernel runtime stats, surfaced at `/proc/perf/kernel` (heat / busy-spin
// debugging). Counters live in `kernel_hal::kstats`.
// ---------------------------------------------------------------------------

fn irq_note(vector: u16) -> &'static str {
    // LAPIC vectors use base 0xf0 on x86_64 (see kernel-hal x86_64 trap.rs).
    match vector {
        0xf0 => "LAPIC spurious",
        0xf1 => "LAPIC timer",
        0xf2 => "LAPIC error",
        _ => "",
    }
}
/// The `cpu temp` line's value, in whole degrees and one decimal.
///
/// The sensor reads *below* TjMax and the absolute figure is `TjMax - readout`,
/// so a cold part — or one whose `MSR_TEMPERATURE_TARGET` reads back low — can
/// hand us a negative milli-degree value. Splitting that with `/` and `%`, as
/// this used to, gives a signed remainder: -27500 printed as `-27.-5`, and
/// anything between 0 and -1000 lost its sign entirely and printed `0.-5`.
/// Take the magnitude apart and carry the sign once.
fn fmt_temp_c(mc: i32) -> String {
    let neg = mc < 0;
    let mag = mc.unsigned_abs();
    alloc::format!(
        "{}{}.{} C",
        if neg { "-" } else { "" },
        mag / 1000,
        (mag % 1000) / 100
    )
}

/// The `fork phases` line, or `None` when no fork has been measured yet.
///
/// Takes [`zircon_object::vm::fork_phase_stats`]'s tuple whole: `(mappings,
/// total, create_child, protect_for_cow, map_committed, allocations)`, all
/// cumulative nanoseconds over `mappings` cloned.
///
/// `rest` is what the three named phases do not account for. It used to
/// subtract only `create_child` and `protect_for_cow`, leaving the whole of
/// `map_committed` inside it — and `map_committed` is normally the largest
/// phase of the three, so the line printed it twice: once under its own name
/// and again as "rest", which is the column a reader scans precisely to find
/// fork time that *is not* in a named phase.
fn fork_phase_line(stats: (u64, u64, u64, u64, u64, u64)) -> Option<String> {
    let (n, total, create, protect, committed, allocs) = stats;
    if n == 0 {
        return None;
    }
    let us = |v: u64| v as f64 / 1000.0 / n as f64;
    let rest = total
        .saturating_sub(create)
        .saturating_sub(protect)
        .saturating_sub(committed);
    Some(alloc::format!(
        "fork phases:  {} mappings cloned, {:.1} us each (create_child {:.1}, \
         protect {:.1}, map_committed {:.1}, rest {:.1}), {:.1} allocs each",
        n,
        us(total),
        us(create),
        us(protect),
        us(committed),
        us(rest),
        allocs as f64 / n as f64,
    ))
}

/// `part` as a percentage of `whole`, or zero when there is no whole. The
/// counters behind these are all monotonic, so a difference between two
/// snapshots can be zero — and a zero denominator is "nothing happened in
/// this window", not a division to go through with.
fn pct_of(part: u64, whole: u64) -> f64 {
    if whole > 0 {
        part as f64 * 100.0 / whole as f64
    } else {
        0.0
    }
}

/// Idle share of one span: `idle_ns` against the `span_ns × cpus` of capacity
/// those cores had. Clamped at 100%, because the idle counters are per-CPU
/// and a core that came online mid-span can report more idle time than the
/// span is long.
fn pct_idle(idle_ns: u64, span_ns: u64, cpus: u64) -> f64 {
    let cap = span_ns.saturating_mul(cpus);
    if cap > 0 {
        (idle_ns as f64 * 100.0 / cap as f64).min(100.0)
    } else {
        0.0
    }
}

/// One reading of every counter the busy verdict is derived from, taken at
/// `uptime_ns`. Kept whole so the *next* read can be measured against it:
/// every one of these is monotonic since boot, so a figure for "now" is the
/// difference between two of these and never a counter on its own.
#[derive(Clone, Copy, Debug, Default)]
struct Sample {
    uptime_ns: u64,
    idle_ns: u64,
    idle_cb_total: u64,
    idle_cb_busy: u64,
    polled: u64,
    weak: u64,
    timer_ticks: u64,
    /// Scheduler ticks that interrupted ring 3, summed over the cores.
    tick_user: u64,
    /// Scheduler ticks taken, summed over the cores.
    tick_total: u64,
}

/// The state at boot: all counters zero at uptime zero. Measuring `cur`
/// against this gives the lifetime averages, which is what the first read of
/// the file after boot has to fall back to.
const BOOT: Sample = Sample {
    uptime_ns: 0,
    idle_ns: 0,
    idle_cb_total: 0,
    idle_cb_busy: 0,
    polled: 0,
    weak: 0,
    timer_ticks: 0,
    tick_user: 0,
    tick_total: 0,
};

/// The figures the busy attribution is read from, all measured over the same
/// span so the verdict describes one moment rather than several.
#[derive(Clone, Copy, Debug, Default)]
struct Busy {
    /// Length of the span these were measured over, in seconds.
    window_s: f64,
    idle_pct: f64,
    busy_pct: f64,
    cb_work_pct: f64,
    polls_per_s: f64,
    weak_per_s: f64,
    timer_per_s: f64,
    /// Share of scheduler ticks that interrupted ring 3.
    user_pct: f64,
    /// Whether these came from a real window, or are the lifetime averages
    /// of the first read after boot.
    windowed: bool,
    /// Whether every core but the one rendering this is parked in `hlt`.
    all_halted: bool,
}

impl Busy {
    /// Every figure of the span between `prev` and `cur`, over `cpus` online
    /// cores.
    ///
    /// `prev` is the state at the previous read of this file. `None` — the
    /// first read after boot — measures against [`BOOT`] instead, which is the
    /// same arithmetic and yields the lifetime averages; `windowed` records
    /// which of the two happened, because the verdict is only allowed to speak
    /// for a real window.
    ///
    /// A `prev` that is not strictly older than `cur` is *not* a window: two
    /// reads inside one timer granule, or a clock that stepped back, leave a
    /// span of zero, and a span of zero has no capacity to be idle in — the
    /// idle share would come out 0% and the busy share a flat 100%, with
    /// `windowed` claiming it was measured. That falls back to lifetime too.
    fn over(prev: Option<Sample>, cur: Sample, cpus: u64) -> Busy {
        let windowed = prev.is_some_and(|p| cur.uptime_ns > p.uptime_ns);
        let base = if windowed { prev.unwrap() } else { BOOT };
        let d_ns = cur.uptime_ns.saturating_sub(base.uptime_ns);
        let d_s = d_ns as f64 / 1e9;
        let per_s = |now: u64, then: u64| {
            if d_s > 0.0 {
                now.saturating_sub(then) as f64 / d_s
            } else {
                0.0
            }
        };
        // `pct_idle` is where the clamp lives, so the busy share needs no
        // second one: a duplicated guard is a guard nothing can test.
        let idle_pct = pct_idle(cur.idle_ns.saturating_sub(base.idle_ns), d_ns, cpus);
        Busy {
            window_s: d_s,
            idle_pct,
            busy_pct: 100.0 - idle_pct,
            cb_work_pct: pct_of(
                cur.idle_cb_busy.saturating_sub(base.idle_cb_busy),
                cur.idle_cb_total.saturating_sub(base.idle_cb_total),
            ),
            polls_per_s: per_s(cur.polled, base.polled),
            weak_per_s: per_s(cur.weak, base.weak),
            timer_per_s: per_s(cur.timer_ticks, base.timer_ticks),
            user_pct: pct_of(
                cur.tick_user.saturating_sub(base.tick_user),
                cur.tick_total.saturating_sub(base.tick_total),
            ),
            windowed,
            all_halted: false,
        }
    }

    /// Pegged, with every run-loop counter at zero and the timer barely
    /// ticking: the busy time is being spent outside the run loop entirely,
    /// which is a spin with interrupts disabled. Only the NMI probe can see
    /// it, so the report captures the spin RIPs when this holds.
    fn off_scheduler_wedge(&self) -> bool {
        self.windowed
            && self.busy_pct > 50.0
            && self.cb_work_pct < 1.0
            && self.weak_per_s < 1.0
            && self.polls_per_s < 1.0
            && self.timer_per_s < 50.0
    }

    /// Name the dominant non-halting path, for the one line of the report
    /// that survives a `head -30` or a photo of the console.
    ///
    /// Nothing is decided without a window: on the first read every figure
    /// is a lifetime average, and on a long-uptime box that is a busy% near
    /// 100 with the poll, weak and timer rates rounding to zero — which is
    /// exactly the shape of a kernel livelock, on a perfectly idle machine.
    fn attribution(&self) -> &'static str {
        if !self.windowed {
            "unknown — first read has only the lifetime average; \
             run `cat` again for a live (windowed) verdict"
        } else if self.busy_pct < 50.0 {
            "none — cores mostly reach halt"
        } else if self.all_halted {
            // Reached only with a real window (the `!windowed` arm is first),
            // so the busy time above was genuinely spent inside it — it has
            // simply stopped since. Saying "lifetime-average artifact" here,
            // as this used to, sent the reader to re-read a figure they were
            // already looking at.
            "NONE NOW — every core is parked in hlt this instant; the busy time above \
             happened during the window and has already stopped"
        } else if self.cb_work_pct > 50.0 {
            "deferred-job drain — idle callback keeps finding work, cores never halt"
        } else if self.weak_per_s > 100.0 {
            "weak-executor yields — long futures preempted, scheduler re-spins (kernel)"
        } else if self.user_pct > 60.0 {
            "user thread busy-spin — a process is pegging the cpu (see tick ctx below)"
        } else if self.polls_per_s > 5000.0 {
            "task busy-poll — a coroutine re-polled without ever sleeping (kernel)"
        } else if self.off_scheduler_wedge() {
            "OFF-SCHEDULER spin — pegged with no run-loop activity and timers stalled: \
             interrupts-disabled kernel livelock (see wedge rips)"
        } else {
            "unclear — read tick ctx (%user) and nmi probe rip below"
        }
    }
}

/// Render `/proc/perf/kernel`: idle vs busy, timer ticks and per-vector IRQs.
pub fn kernel_report() -> String {
    let ks = kernel_hal::kstats::snapshot();
    let (sched_polled, sched_weak) = kernel_hal::kstats::sched_stats();
    let uptime_ns = kernel_hal::timer::timer_now().as_nanos() as u64;
    let uptime_s = uptime_ns as f64 / 1e9;
    let total_cpus = kernel_hal::cpu::cpu_count().max(1) as u64;
    // Average busy% over the cores that actually came online, NOT the configured
    // CPU count: an AP that failed SMP bring-up never runs the idle loop, so
    // counting it in the denominator would charge its idle time as "busy" and
    // inflate the figure (a partial bring-up under QEMU/TCG is common). On a
    // healthy boot online == total and this is identical to before.
    let online_cpus = (kernel_hal::online_cpu_count() as u64).clamp(1, total_cpus);

    // ── Lifetime (since boot) figures ──
    // Kept alongside the windowed ones: a window says what the box is doing
    // now, the lifetime average says whether it has ever done anything else.
    let rate = |n: u64| {
        if uptime_s > 0.0 {
            n as f64 / uptime_s
        } else {
            0.0
        }
    };

    // ── Windowed figures (delta since the previous read of this file) ──
    // A lifetime average is useless for "is the box busy *now*": on a long uptime
    // an early busy spell (boot, or a since-fixed busy-spin) keeps it pegged near
    // 100% even when every core is currently halted in `hlt`. The NMI probe sees
    // the truth (all cores caught at the post-`hlt` RIP) but the headline didn't.
    // Diff against the last snapshot so a second `cat` a moment later reports the
    // live figure. First read after boot has no previous sample and falls back to
    // the lifetime numbers.
    //
    // `tick_user`/`tick_total` are the aggregate ring-3 share of scheduler ticks
    // across all cores: a high figure on a pegged box means a *user* thread is
    // busy-spinning (a runaway process); a low figure points the spin at kernel
    // code (a poll/lock loop). They go into the snapshot with everything else so
    // they get differenced too — this pair used to reach the verdict raw, as a
    // share of every tick since boot, which is a lifetime figure inside a verdict
    // that announces itself as "now": an early user-space burner kept saying
    // "user thread busy-spin" long after the process was gone, over whatever the
    // box was really doing.
    let (tick_user, tick_total) = ks
        .tick_percpu
        .iter()
        .fold((0u64, 0u64), |(u, t), (_, total, user, _)| {
            (u + *user, t + *total)
        });
    static LAST: Mutex<Option<Sample>> = Mutex::new(None);
    let cur = Sample {
        uptime_ns,
        idle_ns: ks.idle_ns,
        idle_cb_total: ks.idle_cb_total,
        idle_cb_busy: ks.idle_cb_busy,
        polled: sched_polled,
        weak: sched_weak,
        timer_ticks: ks.timer_ticks,
        tick_user,
        tick_total,
    };
    let prev = LAST.lock().replace(cur);
    // "Current" view drives the busy/attribution logic and the per-second rates;
    // falls back to lifetime on the first read after boot.
    let mut busy = Busy::over(prev, cur, online_cpus);
    let life = Busy::over(None, cur, online_cpus);
    // Local names for the report lines below, which show the same figures the
    // verdict was read from.
    let (
        win_s,
        idle_pct,
        busy_pct,
        cb_work_pct,
        polls_per_s,
        weak_per_s,
        timer_per_s,
        user_pct,
        have_window,
    ) = (
        busy.window_s,
        busy.idle_pct,
        busy.busy_pct,
        busy.cb_work_pct,
        busy.polls_per_s,
        busy.weak_per_s,
        busy.timer_per_s,
        busy.user_pct,
        busy.windowed,
    );
    let (life_idle_pct, life_busy_pct) = (life.idle_pct, life.busy_pct);

    let mut out = String::new();
    let _ = writeln!(out, "eclipse perf — kernel runtime stats");
    let _ = writeln!(out);
    match kernel_hal::cpu::cpu_temperature_mc() {
        Some(mc) => {
            let _ = writeln!(out, "cpu temp:     {}", fmt_temp_c(mc));
        }
        None => {
            let _ = writeln!(out, "cpu temp:     n/a (no sensor or running in a VM)");
        }
    }
    // Adaptive P-state governor (bare metal only): shows whether any core is
    // currently throttled for heat, and cpu0's live ceiling vs its base.
    if let Some((throttled, ceiling, base)) = kernel_hal::cpu::pstate_governor_summary() {
        if throttled > 0 {
            let _ = writeln!(
                out,
                "pstate gov:   {} core(s) throttling — cpu0 ceiling {}/{}",
                throttled, ceiling, base
            );
        } else {
            let _ = writeln!(
                out,
                "pstate gov:   nominal — cpu0 ceiling {}/{}",
                ceiling, base
            );
        }
    }
    let _ = writeln!(
        out,
        "uptime:       {:.2} s   cpus: {} online ({} configured)",
        uptime_s, online_cpus, total_cpus
    );
    if have_window {
        let _ = writeln!(
            out,
            "cpu idle:     {:.1}%   busy: {:.1}%   (last {:.2}s window over {} online cpu(s))",
            idle_pct, busy_pct, win_s, online_cpus
        );
        let _ = writeln!(
            out,
            "  lifetime:   idle {:.1}%   busy {:.1}%   (since boot — skewed by past load)",
            life_idle_pct, life_busy_pct
        );
    } else {
        let _ = writeln!(
            out,
            "cpu idle:     {:.1}%   busy: {:.1}%   (lifetime since boot — run `cat` again for a recent-window figure)",
            idle_pct, busy_pct
        );
    }
    let avg_nap_us = if ks.idle_entries > 0 {
        ks.idle_ns as f64 / ks.idle_entries as f64 / 1000.0
    } else {
        0.0
    };
    let _ = writeln!(
        out,
        "idle naps:    {}  (avg {:.1} us/nap, lifetime)",
        ks.idle_entries, avg_nap_us
    );
    // [diag] Busy attribution — kept HIGH in the report, *before* the per-CPU
    // breakdowns. With many cores those lists are 20+ lines each and push the
    // attribution counters (idle-callback %, sched polls, tick ctx, NMI rip) past
    // the first screenful, so a `head -30` or a phone photo of the console shows
    // 100% busy with no hint of *why*. This single line names the dominant
    // non-halting path so even a truncated capture localises the spin; the
    // per-CPU tick ctx (%user) and NMI rip lines below pin it exactly. All the
    // figures here are the windowed ones (or lifetime on the first read), so the
    // verdict matches what the box is doing *now*, not its lifetime average.
    kernel_hal::kstats::capture_cpu_rips();
    let nmi = kernel_hal::kstats::nmi_rips();
    // How many cores are parked in idle `hlt` *right now* (robust per-CPU flag set
    // around enable_and_hlt — not a build-dependent RIP match). The core rendering
    // this report is itself busy, so a fully-idle box shows online-1 here. If
    // essentially every other core is halted, a high busy% is a lifetime-average
    // artifact, not a live spin.
    let idle_now = kernel_hal::kstats::cpus_idle_now() as u64;
    busy.all_halted = online_cpus > 1 && idle_now >= online_cpus - 1;
    let all_halted = busy.all_halted;
    let off_sched_wedge = busy.off_scheduler_wedge();
    let _ = writeln!(
        out,
        "cores idle now: {}/{} parked in hlt this instant",
        idle_now, online_cpus
    );
    let suspect = busy.attribution();
    let _ = writeln!(out, "busy attribution: {}", suspect);
    let _ = writeln!(
        out,
        "  (idle-cb work {:.0}%, weak-yield {:.0}/s, task-polls {:.0}/s, tick {:.0}% user, timer {:.0}/s{})",
        cb_work_pct, weak_per_s, polls_per_s, user_pct, timer_per_s,
        if have_window { "" } else { " — lifetime" }
    );
    if off_sched_wedge && !all_halted && !nmi.is_empty() {
        // Distinct current RIPs across cores. A single shared value means every
        // wedged core is stuck at the same spin site (one lock / one loop);
        // resolve it with addr2line against the kernel image.
        let mut rips: Vec<u64> = nmi.iter().map(|(_, r)| *r).collect();
        rips.sort_unstable();
        rips.dedup();
        let _ = write!(out, "  wedge rips ({} core(s),", nmi.len());
        let _ = write!(out, " {} distinct):", rips.len());
        for r in rips.iter().take(6) {
            let _ = write!(out, " {:#x}", r);
        }
        if rips.len() > 6 {
            let _ = write!(out, " (+{} more)", rips.len() - 6);
        }
        let _ = writeln!(out);
    }
    // [diag] Per-CPU nap breakdown: a core driving the HID poll should show many
    // short naps (low avg us); a deeply-idle core shows few long naps.
    for (cpu, naps, ns) in &ks.idle_percpu {
        let avg_us = if *naps > 0 {
            *ns as f64 / *naps as f64 / 1000.0
        } else {
            0.0
        };
        let _ = writeln!(
            out,
            "  cpu{}: {} naps ({:.0}/s), avg {:.0} us/nap",
            cpu,
            naps,
            rate(*naps),
            avg_us
        );
    }
    // [diag] Per-CPU tick context: of the scheduler ticks that hit each core, how
    // many interrupted user mode (a ring-3 thread burning CPU) vs kernel mode. A
    // pegged core with a high user% is running a CPU-bound user thread; a high
    // kernel% on a pegged core points at a kernel-side spin (lock / poll loop).
    if !ks.tick_percpu.is_empty() {
        let _ = writeln!(out, "tick ctx (user/total, last rip):");
        for (cpu, total, user, rip) in &ks.tick_percpu {
            let pct = if *total > 0 {
                *user as f64 * 100.0 / *total as f64
            } else {
                0.0
            };
            let _ = writeln!(
                out,
                "  cpu{}: {}/{} ({:.0}% user) rip={:#x}",
                cpu, user, total, pct, rip
            );
        }
    }
    // [diag] NMI probe: interrupt every other CPU (delivered even with IRQs off)
    // and report its *current* RIP. For a core wedged in an interrupts-disabled
    // spin this is the actual spin site — resolve with addr2line. `nmi` was
    // captured once up in the busy-attribution block; reuse it here.
    if !nmi.is_empty() {
        let _ = writeln!(out, "nmi probe (current rip per cpu):");
        for (cpu, rip) in &nmi {
            let _ = writeln!(out, "  cpu{}: {:#x}", cpu, rip);
        }
    }
    // [diag] xHCI HID poll rate by path. Input is delivered from these polls;
    // when idle, `iowait` falls to ~0 and `timer` alone must keep input alive.
    let _ = writeln!(
        out,
        "hid polls:    timer {} ({:.0}/s), iowait {} ({:.0}/s)",
        ks.hid_poll_timer,
        rate(ks.hid_poll_timer),
        ks.hid_poll_iowait,
        rate(ks.hid_poll_iowait)
    );
    let _ = writeln!(
        out,
        "timer ticks:  {}  ({:.0}/s)",
        ks.timer_ticks,
        rate(ks.timer_ticks)
    );
    let _ = writeln!(
        out,
        "interrupts:   {}  ({:.0}/s)",
        ks.irq_total,
        rate(ks.irq_total)
    );
    // Idle-callback hit rate: the scheduler only halts when this finds no
    // deferred work, so a high "had work" share means the CPUs busy-spin
    // draining jobs (the heat signature) rather than sleeping. `cb_work_pct` is
    // computed above for the busy-attribution summary.
    let _ = writeln!(
        out,
        "idle callback: {} calls ({:.0}/s), {:.1}% found deferred work",
        ks.idle_cb_total,
        rate(ks.idle_cb_total),
        cb_work_pct
    );
    let _ = writeln!(
        out,
        "deferred jobs pending now: {}",
        kernel_hal::deferred_job::pending_deferred_jobs()
    );
    // `sched_polled`/`sched_weak` were sampled above for the attribution summary.
    let _ = writeln!(
        out,
        "sched: {} task polls ({:.0}/s), {} weak-exec yields ({:.0}/s)",
        sched_polled, polls_per_s, sched_weak, weak_per_s
    );
    // Which scheduler/timer mode this boot is running in, so a captured report
    // is self-describing when compared against another.
    {
        let (deadline, wakeup) = kernel_hal::kstats::sched_switches();
        let _ = writeln!(
            out,
            "sched mode:   deadline-timer={} wakeup-preempt={} cow-fork={} fork-gather={}",
            if deadline { "on" } else { "OFF" },
            if wakeup { "on" } else { "OFF" },
            if zircon_object::vm::cow_fork_enabled() {
                "on"
            } else {
                "OFF"
            },
            if zircon_object::vm::fork_gather_enabled() {
                "on"
            } else {
                "OFF"
            },
        );
    }
    // Copy-on-write tree census. A fork inserts a hidden node above every
    // mapping's VMO and the child's exit should collapse it again; `hidden`
    // failing to return to its pre-fork value is a tree that did not, and that
    // is measurable from userspace by reading this around a `fork`.
    {
        let (paged, hidden, snapshots) = zircon_object::vm::cow_tree_stats();
        let _ = writeln!(
            out,
            "cow tree:     {} paged vmos live, {} hidden live, {} snapshots taken",
            paged, hidden, snapshots
        );
    }
    // Where a fork's time actually goes, per mapping cloned.
    {
        let stats = zircon_object::vm::fork_phase_stats();
        if let Some(line) = fork_phase_line(stats) {
            let _ = writeln!(out, "{}", line);
        }
    }
    // Kernel heap profile (only populated with `HEAPPROF=1`). A `dealloc`
    // average far above `alloc`, and one that climbs with the number of live
    // same-sized objects, is the buddy allocator's linear free-list scan.
    {
        let (ac, acy, dc, dcy) = kernel_hal::kstats::heap_prof_stats();
        if ac > 0 || dc > 0 {
            let _ = writeln!(
                out,
                "heap prof:    alloc {} calls, {} cyc avg; dealloc {} calls, {} cyc avg",
                ac,
                acy.checked_div(ac).unwrap_or(0),
                dc,
                dcy.checked_div(dc).unwrap_or(0),
            );
        }
    }
    // The per-VMO mapping list a fork walks once per mapping.
    {
        let (scans, entries, dead, max) = zircon_object::vm::mapping_list_stats();
        if scans > 0 {
            let _ = writeln!(
                out,
                "vmo maplist:  {} scans, {:.1} entries avg, {} dead, {} longest",
                scans,
                entries as f64 / scans as f64,
                dead,
                max
            );
        }
    }
    // The eager-copy fallback: mappings a fork could not share.
    {
        let (n, bytes, ns) = zircon_object::vm::fork_eager_stats();
        let _ = writeln!(
            out,
            "fork eager:   {} mappings copied ({} KiB, {:.1} ms total)",
            n,
            bytes / 1024,
            ns as f64 / 1e6,
        );
    }
    // vDSO state. Three things can independently stop `clock_gettime` from
    // being answered in userspace — the build had no C compiler, the image
    // could not be placed in physical memory, or the TSC is not fit to be read
    // directly — and all three degrade silently into "the syscall is still
    // taken". This is where a boot says which, without needing a bisect.
    {
        let _ = writeln!(out, "vdso:         {}", crate::vdso::status());
    }
    // Deadline timer: re-arms against ticks. The timer is programmed for the
    // nearest pending deadline instead of rounding every sleep/poll timeout up
    // to the 4 ms scheduler tick; this is the sanity check that it is buying
    // precision rather than degenerating into an interrupt storm.
    {
        let per_tick = if ks.timer_ticks > 0 {
            ks.timer_rearms as f64 / ks.timer_ticks as f64
        } else {
            0.0
        };
        let _ = writeln!(
            out,
            "timer rearms: {} ({:.0}/s, {:.2} per tick)",
            ks.timer_rearms,
            rate(ks.timer_rearms),
            per_tick
        );
    }
    // Tick gaps: the time between consecutive ticks on one busy CPU, which
    // should never exceed the 4 ms period by much. A gap of tens of ms is a
    // CPU that did not run -- under KVM a vCPU the host descheduled, on
    // hardware an interrupts-off section -- and every wait parked on that
    // CPU's re-scan (audio feeders among them) was served that late. Ticks
    // that interrupted the idle halt are counted apart: a halted vCPU the
    // host wakes late harms nobody.
    let _ = writeln!(
        out,
        "timer tick gaps: max {:.1} ms, {} over 12 ms on a busy CPU{}, {} on an idle one",
        ks.tick_gap_max_ns as f64 / 1e6,
        ks.tick_gaps_late,
        if ks.tick_gaps_late > 0 {
            alloc::format!(
                " (last {:.1} ms at uptime {:.3} s)",
                ks.tick_gap_last_late_ns as f64 / 1e6,
                ks.tick_gap_last_late_at_ns as f64 / 1e9
            )
        } else {
            String::new()
        },
        ks.tick_gaps_late_idle
    );
    // Wake-up preemption: how often a task became runnable on a CPU that was
    // busy with someone else, and how often that actually cut the running
    // thread's timeslice short. Without this the woken task waits out the full
    // slice (up to 20 ms) — invisible to single-threaded benchmarks, very
    // visible when using the machine.
    {
        let (req, taken) = kernel_hal::kstats::wakeup_preempt_stats();
        let pct = if req > 0 {
            taken as f64 * 100.0 / req as f64
        } else {
            0.0
        };
        let _ = writeln!(
            out,
            "wakeup preempt: {} requests ({:.0}/s), {} honoured ({:.1}%)",
            req,
            rate(req),
            taken,
            pct
        );
    }
    // Coroutine-stack hand-out guard health. Non-zero means the live-stack
    // registry filled and dropped an insert, so the buddy allocator can hand a
    // live executor stack to a Vec/Box/VMO frame — the [double-alloc] that
    // zeroes a live stack and (shared buddy arena) a userspace page, i.e. the
    // lunarbar/lunarbg/labwc wl_list NULL-deref crash. Zero rules that path out.
    {
        let (alloc_drops, live_drops) = kernel_hal::kstats::coroutine_stack_registry_drops();
        if alloc_drops > 0 || live_drops > 0 {
            let _ = writeln!(
                out,
                "coroutine-stack registry OVERFLOWED: {} alloc-guard drops, {} live drops                  — the buddy can hand out a LIVE stack (double-alloc / userspace corruption risk)",
                alloc_drops, live_drops
            );
        } else {
            let _ = writeln!(
                out,
                "coroutine-stack registry: no overflow (hand-out guard complete)"
            );
        }
    }
    let _ = writeln!(out);
    if busy_pct > 50.0 {
        let _ = writeln!(
            out,
            "note: CPUs are busy >50% while you read this — if the system looks idle,",
        );
        let _ = writeln!(
            out,
            "      something is busy-spinning (a likely source of heat). The busiest",
        );
        let _ = writeln!(
            out,
            "      IRQ vectors below hint at runaway interrupt sources."
        );
        let _ = writeln!(out);
    }
    let _ = writeln!(
        out,
        "  {:>8}  {:>12}  {:>10}  NOTE",
        "VECTOR", "COUNT", "PER SEC"
    );
    let mut irqs = ks.irqs;
    irqs.sort_by_key(|r| core::cmp::Reverse(r.1));
    for (v, c) in irqs {
        let _ = writeln!(
            out,
            "  {:>#8x}  {:>12}  {:>10.0}  {}",
            v,
            c,
            rate(c),
            irq_note(v)
        );
    }
    out
}

// ---------------------------------------------------------------------------
// Sampling profiler ("our own perf top"), surfaced at `/proc/perf/top`.
// ---------------------------------------------------------------------------

/// Cluster sampled instruction pointers into 64-byte buckets so a hot function
/// aggregates instead of scattering across every instruction.
const PC_BUCKET: u64 = 64;
/// Cap on distinct buckets tracked, to bound memory and lock time. Once full,
/// new addresses are counted as "dropped" rather than inserted.
const TOP_MAX: usize = 4096;

struct SampleState {
    total: u64,
    dropped: u64,
    map: BTreeMap<u64, u64>,
}

impl SampleState {
    /// Add one instruction-pointer sample. A `pc` of zero is not an address,
    /// it is "the context had no saved PC", and counting it would put a
    /// phantom bucket at the top of the profile.
    fn add(&mut self, pc: u64) {
        if pc == 0 {
            return;
        }
        let key = pc & !(PC_BUCKET - 1);
        self.total += 1;
        if let Some(c) = self.map.get_mut(&key) {
            *c += 1;
        } else if self.map.len() < TOP_MAX {
            self.map.insert(key, 1);
        } else {
            self.dropped += 1;
        }
    }
}

static SAMPLES: Mutex<SampleState> = Mutex::new(SampleState {
    total: 0,
    dropped: 0,
    map: BTreeMap::new(),
});

/// Per-timer-tick hook: record one user-space sample and forward it to any
/// active Linux-`perf` ring buffer. Cheap; called from the timer-interrupt
/// return path while a user thread was running.
pub fn tick(pid: i32, tid: i32, cpu: u32, pc: u64) {
    sample_pc(pc);
    crate::fs::perf_sample_user(pid, tid, cpu, pc);
}

/// Add one instruction-pointer sample to the global histogram.
fn sample_pc(pc: u64) {
    SAMPLES.lock().add(pc);
}

/// Render `/proc/perf/top`: hottest sampled instruction-pointer buckets.
pub fn top_report() -> String {
    let (total, dropped, rows) = {
        let s = SAMPLES.lock();
        let rows: Vec<(u64, u64)> = s.map.iter().map(|(&k, &v)| (k, v)).collect();
        (s.total, s.dropped, rows)
    };
    render_top(total, dropped, rows)
}

/// Render the `/proc/perf/top` table from a histogram snapshot: `total`
/// samples offered, `dropped` of them for want of a free bucket, and the
/// `(bucket, count)` pairs the table does hold.
///
/// OVERHEAD is a share of the samples this table actually *represents*
/// (`total - dropped`), not of every sample ever taken. The two are the same
/// number until the table fills, and after that only the first is a profile:
/// the table is never evicted from, so once `TOP_MAX` distinct buckets have
/// been seen, every later address is counted only in `dropped`. Dividing by
/// `total` then shrank every surviving row towards zero as the box ran, and a
/// hot loop that started after the table filled did not appear at all — a
/// profile that says "nothing is hot" while a core is pegged. The warning line
/// below says so outright, because a correct relative profile of the wrong
/// addresses still is not the answer.
fn render_top(total: u64, dropped: u64, mut rows: Vec<(u64, u64)>) -> String {
    rows.sort_by_key(|r| core::cmp::Reverse(r.1));

    let mut out = String::new();
    let _ = writeln!(out, "eclipse perf — sampled CPU profile (user space)");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "samples: {}   tracked buckets: {}   dropped (table full): {}",
        total,
        rows.len(),
        dropped
    );
    let _ = writeln!(out, "bucket granularity: {} bytes", PC_BUCKET);
    let _ = writeln!(out);
    if total == 0 {
        let _ = writeln!(
            out,
            "(no samples yet — a user process must be running on a timer tick)"
        );
        return out;
    }
    let kept = total.saturating_sub(dropped);
    if dropped > 0 {
        let _ = writeln!(
            out,
            "WARNING: the bucket table filled at {} entries and is never evicted from, so",
            TOP_MAX
        );
        let _ = writeln!(
            out,
            "         {} of {} samples landed on addresses it has no room for. The",
            dropped, total
        );
        let _ = writeln!(
            out,
            "         percentages below are shares of the {} it does hold, and the",
            kept
        );
        let _ = writeln!(out, "         hottest code may not be in this list at all.");
        let _ = writeln!(out);
    }
    if kept == 0 {
        let _ = writeln!(
            out,
            "(every sample was dropped — the table holds nothing to report)"
        );
        return out;
    }
    let _ = writeln!(
        out,
        "  {:>7}  {:>10}  {:<18}",
        "OVERHEAD", "SAMPLES", "IP (bucket)"
    );
    for (addr, count) in rows.into_iter().take(40) {
        let pct = count as f64 * 100.0 / kept as f64;
        let _ = writeln!(out, "  {:>6.2}%  {:>10}  {:#018x}", pct, count, addr);
    }
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
            rows.push((i as u32, calls, perf.per_ns[i].load(Relaxed)));
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
    fmt_table(&mut out, rows);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two figures are the same to within a rounding error of the last decimal
    /// the report prints.
    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 0.05, "{} != {}", a, b);
    }

    /// A snapshot `ns` nanoseconds into the boot, with every counter at zero.
    /// Tests set only the fields they are about.
    fn at(ns: u64) -> Sample {
        Sample {
            uptime_ns: ns,
            ..Default::default()
        }
    }

    /// A windowed reading of a pegged box with the timer ticking normally, so
    /// no arm of the ladder fires on its own. Tests set the one field they
    /// are about and read the verdict back.
    fn pegged() -> Busy {
        Busy {
            window_s: 1.0,
            idle_pct: 1.0,
            busy_pct: 99.0,
            timer_per_s: 250.0,
            windowed: true,
            ..Default::default()
        }
    }

    // ── The arithmetic the figures are built from ──────────────────────────

    #[test]
    fn a_share_of_nothing_is_zero_rather_than_a_division() {
        // Every counter behind these is monotonic, so a window in which
        // nothing happened gives a zero denominator. That is "no data", not a
        // division to go through with.
        assert_eq!(pct_of(0, 0), 0.0);
        assert_eq!(pct_of(7, 0), 0.0);
        // No span, and no cores to have been idle on it.
        assert_eq!(pct_idle(5, 0, 4), 0.0);
        assert_eq!(pct_idle(5, 1_000, 0), 0.0);
        close(pct_of(1, 4), 25.0);
    }

    #[test]
    fn an_idle_share_is_capped_at_a_hundred_per_cent() {
        // Four cores over a one-second span have four core-seconds to be idle
        // in.
        close(pct_idle(2_000_000_000, 1_000_000_000, 4), 50.0);
        close(pct_idle(4_000_000_000, 1_000_000_000, 4), 100.0);
        // A core that came online mid-window reports nap time from before the
        // window's start, so the sum can exceed the capacity. Past 100% the
        // busy share would go negative and the verdict would read as idle.
        assert_eq!(pct_idle(9_000_000_000, 1_000_000_000, 4), 100.0);
    }

    // ── What a window is, and is not ───────────────────────────────────────

    #[test]
    fn the_first_read_after_boot_reports_the_lifetime_average_and_says_so() {
        let cur = Sample {
            uptime_ns: 10_000_000_000,
            idle_ns: 20_000_000_000,
            idle_cb_total: 400,
            idle_cb_busy: 100,
            polled: 5_000,
            weak: 30,
            timer_ticks: 2_500,
            tick_user: 90,
            tick_total: 100,
        };
        let b = Busy::over(None, cur, 4);
        assert!(
            !b.windowed,
            "nothing to measure against is not a measurement"
        );
        close(b.window_s, 10.0);
        close(b.idle_pct, 50.0); // 20s of nap over 4 cores x 10s
        close(b.busy_pct, 50.0);
        close(b.cb_work_pct, 25.0);
        close(b.polls_per_s, 500.0);
        close(b.weak_per_s, 3.0);
        close(b.timer_per_s, 250.0);
        close(b.user_pct, 90.0);
    }

    #[test]
    fn a_window_charges_only_what_happened_inside_it() {
        // An early user-space burner: 990 of the first 1000 ticks were ring 3.
        let prev = Sample {
            uptime_ns: 1_000_000_000,
            tick_user: 990,
            tick_total: 1_000,
            ..Default::default()
        };
        // A second later the process is gone and every new tick is kernel.
        let cur = Sample {
            uptime_ns: 2_000_000_000,
            tick_user: 990,
            tick_total: 1_100,
            ..Default::default()
        };
        let win = Busy::over(Some(prev), cur, 1);
        assert!(win.windowed);
        close(win.user_pct, 0.0);
        // The lifetime average still reads as if it were burning, which is the
        // figure the verdict used to be handed.
        close(Busy::over(None, cur, 1).user_pct, 90.0);
    }

    #[test]
    fn a_window_of_no_length_is_not_a_window() {
        // Two reads inside one timer granule. The span has no capacity to be
        // idle in, so the idle share comes out zero and the busy share a flat
        // 100% -- which must not be handed to the verdict as measured.
        let prev = at(5_000_000_000);
        let cur = Sample {
            idle_ns: 4_000_000_000,
            ..at(5_000_000_000)
        };
        let b = Busy::over(Some(prev), cur, 1);
        assert!(!b.windowed);
        close(b.busy_pct, 20.0); // the lifetime figure, not 100
        assert!(b.attribution().starts_with("unknown"));
    }

    #[test]
    fn a_clock_that_stepped_back_is_not_a_window() {
        let prev = at(9_000_000_000);
        let cur = Sample {
            idle_ns: 8_000_000_000,
            ..at(5_000_000_000)
        };
        let b = Busy::over(Some(prev), cur, 2);
        assert!(!b.windowed);
        close(b.busy_pct, 20.0);
    }

    #[test]
    fn the_window_subtracts_the_state_at_its_start_from_every_figure() {
        // A box that was hammered for its first minute and has been quiet
        // since: every cumulative counter carries that minute, and a figure
        // that forgets to subtract it reports the old load forever.
        let prev = Sample {
            uptime_ns: 60_000_000_000,
            idle_ns: 6_000_000_000,
            idle_cb_total: 10_000,
            idle_cb_busy: 9_000,
            polled: 600_000,
            weak: 60_000,
            timer_ticks: 15_000,
            tick_user: 14_000,
            tick_total: 15_000,
        };
        // One further second, on two cores, spent almost entirely halted.
        let cur = Sample {
            uptime_ns: 61_000_000_000,
            idle_ns: 7_800_000_000,
            idle_cb_total: 10_100,
            idle_cb_busy: 9_001,
            polled: 600_010,
            weak: 60_001,
            timer_ticks: 15_250,
            tick_user: 14_002,
            tick_total: 15_250,
        };
        let b = Busy::over(Some(prev), cur, 2);
        close(b.idle_pct, 90.0);
        close(b.busy_pct, 10.0);
        close(b.cb_work_pct, 1.0);
        close(b.polls_per_s, 10.0);
        close(b.weak_per_s, 1.0);
        close(b.timer_per_s, 250.0);
        close(b.user_pct, 0.8);
        assert!(b.attribution().starts_with("none — cores"));

        // The same counters read as the lifetime average still describe the
        // first minute, which is the reading this window exists to replace.
        let life = Busy::over(None, cur, 2);
        close(life.idle_pct, 6.4);
        close(life.cb_work_pct, 89.1);
        close(life.user_pct, 91.8);
        assert!(life.attribution().starts_with("unknown"));
    }

    #[test]
    fn the_rates_are_per_second_of_the_window_not_of_the_uptime() {
        // Ten seconds up, the last half-second measured.
        let prev = Sample {
            uptime_ns: 9_500_000_000,
            polled: 1_000,
            weak: 100,
            timer_ticks: 2_000,
            ..Default::default()
        };
        let cur = Sample {
            uptime_ns: 10_000_000_000,
            polled: 1_500,
            weak: 150,
            timer_ticks: 2_125,
            ..Default::default()
        };
        let b = Busy::over(Some(prev), cur, 1);
        close(b.window_s, 0.5);
        close(b.polls_per_s, 1_000.0);
        close(b.weak_per_s, 100.0);
        close(b.timer_per_s, 250.0);
        // Over the whole uptime the same counters are a quarter of that.
        close(Busy::over(None, cur, 1).polls_per_s, 150.0);
    }

    #[test]
    fn a_counter_that_went_backwards_does_not_wrap_the_window() {
        // Nothing here should ever go down, but a counter reset (a CPU that
        // came back, a snapshot that raced a write) would come out as a
        // near-u64::MAX delta and peg every rate.
        let prev = Sample {
            uptime_ns: 1_000_000_000,
            idle_ns: 5_000_000_000,
            idle_cb_total: 500,
            idle_cb_busy: 200,
            polled: 900,
            weak: 90,
            timer_ticks: 250,
            tick_user: 80,
            tick_total: 100,
        };
        let cur = at(2_000_000_000);
        let b = Busy::over(Some(prev), cur, 1);
        close(b.idle_pct, 0.0);
        close(b.cb_work_pct, 0.0);
        close(b.polls_per_s, 0.0);
        close(b.weak_per_s, 0.0);
        close(b.timer_per_s, 0.0);
        close(b.user_pct, 0.0);
    }

    #[test]
    fn the_idle_share_is_spread_over_the_cores_that_came_online() {
        // An AP that failed SMP bring-up never runs the idle loop: counting it
        // in the denominator charges its absence as busy time.
        let prev = at(1_000_000_000);
        let cur = Sample {
            idle_ns: 2_000_000_000,
            ..at(2_000_000_000)
        };
        close(Busy::over(Some(prev), cur, 2).idle_pct, 100.0);
        close(Busy::over(Some(prev), cur, 4).idle_pct, 50.0);
    }

    // ── The verdict ladder ─────────────────────────────────────────────────

    #[test]
    fn nothing_is_attributed_without_a_window() {
        // A long-uptime box that was busy once reads as 100% busy with every
        // rate rounding to zero, which is the exact shape of a kernel
        // livelock. Refusing to judge is the whole point of the first arm.
        let b = Busy {
            windowed: false,
            ..pegged()
        };
        assert!(b.attribution().starts_with("unknown"));
        assert!(!b.off_scheduler_wedge());
    }

    #[test]
    fn a_box_that_reaches_halt_is_not_attributed() {
        let b = Busy {
            busy_pct: 49.0,
            cb_work_pct: 99.0,
            ..pegged()
        };
        assert!(b.attribution().starts_with("none — cores"));
    }

    #[test]
    fn every_core_parked_in_hlt_says_the_busy_time_has_already_stopped() {
        let b = Busy {
            all_halted: true,
            cb_work_pct: 99.0,
            ..pegged()
        };
        let verdict = b.attribution();
        assert!(verdict.starts_with("NONE NOW"), "{}", verdict);
        // This arm is only reachable with a real window (the no-window arm is
        // first), so the busy% above was measured, not averaged. Blaming a
        // lifetime average here sent the reader back to a figure they were
        // already looking at.
        assert!(!verdict.contains("lifetime"), "{}", verdict);
    }

    #[test]
    fn the_ladder_names_the_first_path_that_fits() {
        let drain = Busy {
            cb_work_pct: 51.0,
            weak_per_s: 1_000.0,
            user_pct: 99.0,
            polls_per_s: 1e6,
            ..pegged()
        };
        assert!(drain.attribution().starts_with("deferred-job drain"));

        let weak = Busy {
            weak_per_s: 101.0,
            user_pct: 99.0,
            polls_per_s: 1e6,
            ..pegged()
        };
        assert!(weak.attribution().starts_with("weak-executor yields"));

        let user = Busy {
            user_pct: 61.0,
            polls_per_s: 1e6,
            ..pegged()
        };
        assert!(user.attribution().starts_with("user thread busy-spin"));

        let poll = Busy {
            polls_per_s: 5_001.0,
            ..pegged()
        };
        assert!(poll.attribution().starts_with("task busy-poll"));

        assert!(pegged().attribution().starts_with("unclear"));
    }

    #[test]
    fn an_off_scheduler_wedge_needs_every_run_loop_counter_quiet() {
        let wedged = Busy {
            timer_per_s: 49.0,
            ..pegged()
        };
        assert!(wedged.off_scheduler_wedge());
        assert!(wedged.attribution().starts_with("OFF-SCHEDULER spin"));

        // Any one of the conjuncts alone rules it out: the run loop is running,
        // so whatever the cores are doing they are still reaching it.
        for other in [
            Busy {
                busy_pct: 50.0,
                ..wedged
            },
            Busy {
                cb_work_pct: 1.0,
                ..wedged
            },
            Busy {
                weak_per_s: 1.0,
                ..wedged
            },
            Busy {
                polls_per_s: 1.0,
                ..wedged
            },
            Busy {
                timer_per_s: 50.0,
                ..wedged
            },
            Busy {
                windowed: false,
                ..wedged
            },
        ] {
            assert!(!other.off_scheduler_wedge(), "wedge claimed at {:?}", other);
        }
    }

    #[test]
    fn a_user_space_burner_is_named_only_while_it_is_burning() {
        // The whole chain, from two snapshots to the one line of the report
        // that survives a photo of the console.
        let prev = Sample {
            uptime_ns: 60_000_000_000,
            tick_user: 14_000,
            tick_total: 15_000,
            timer_ticks: 15_000,
            ..Default::default()
        };
        let mut burning = Busy::over(
            Some(prev),
            Sample {
                uptime_ns: 61_000_000_000,
                tick_user: 14_240,
                tick_total: 15_250,
                timer_ticks: 15_250,
                ..Default::default()
            },
            1,
        );
        burning.all_halted = false;
        assert!(burning.attribution().starts_with("user thread busy-spin"));

        // Same box a second later with the process gone: the cores are still
        // pegged (something else now), but the ring-3 share of the window has
        // collapsed and the verdict must stop pointing at user space.
        let mut gone = Busy::over(
            Some(prev),
            Sample {
                uptime_ns: 61_000_000_000,
                tick_user: 14_000,
                tick_total: 15_250,
                timer_ticks: 15_250,
                ..Default::default()
            },
            1,
        );
        gone.all_halted = false;
        assert!(!gone.attribution().contains("user thread"));
    }

    // ── The lines of the report ────────────────────────────────────────────

    #[test]
    fn a_sub_zero_temperature_keeps_its_sign_on_both_halves() {
        assert_eq!(fmt_temp_c(42_500), "42.5 C");
        assert_eq!(fmt_temp_c(0), "0.0 C");
        // The sensor reads below TjMax, so a cold part -- or one with a low
        // MSR_TEMPERATURE_TARGET -- goes negative. A signed remainder printed
        // the fraction with its own minus sign.
        assert_eq!(fmt_temp_c(-27_500), "-27.5 C");
        // And between zero and minus one degree the integer half rounds to a
        // bare "0", so the sign has to be carried separately or it is lost.
        assert_eq!(fmt_temp_c(-500), "-0.5 C");
    }

    #[test]
    fn the_fork_phases_add_up_to_the_total() {
        // Two mappings, 100 us of fork each: 10 create_child, 20 protect,
        // 60 map_committed, so 10 is unaccounted.
        let line = fork_phase_line((2, 200_000, 20_000, 40_000, 120_000, 8)).unwrap();
        assert!(
            line.contains("2 mappings cloned, 100.0 us each"),
            "{}",
            line
        );
        assert!(line.contains("create_child 10.0"), "{}", line);
        assert!(line.contains("protect 20.0"), "{}", line);
        assert!(line.contains("map_committed 60.0"), "{}", line);
        // Leaving map_committed inside "rest" printed the largest phase twice,
        // under its own name and again as the unaccounted remainder -- which
        // is the column read precisely to find time that is in no phase.
        assert!(line.contains("rest 10.0"), "{}", line);
        assert!(line.contains("4.0 allocs each"), "{}", line);
    }

    #[test]
    fn fork_phases_that_overrun_their_total_do_not_wrap_the_rest() {
        // The phases are summed by separate atomics and read one at a time, so
        // a fork in flight can be counted in a phase but not yet in the total.
        let line = fork_phase_line((1, 1_000, 900, 900, 900, 0)).unwrap();
        assert!(line.contains("rest 0.0"), "{}", line);
    }

    #[test]
    fn no_fork_measured_prints_no_line() {
        assert!(fork_phase_line((0, 0, 0, 0, 0, 0)).is_none());
    }

    #[test]
    fn the_syscall_table_is_busiest_first_and_skips_what_was_never_called() {
        let mut out = String::new();
        fmt_table(
            &mut out,
            alloc::vec![(1, 5, 5_000_000), (2, 50, 1_000_000), (3, 0, 0)],
        );
        let names: Vec<&str> = out
            .lines()
            .skip(1)
            .map(|l| l.trim().split_whitespace().next().unwrap())
            .collect();
        assert_eq!(names, alloc::vec!["sys_2", "sys_1"]);
        // A row that was never called is not a row; its average would be a
        // division by zero.
        assert!(!out.contains("sys_3"), "{}", out);
        // 5 calls over 5 ms total is 1000 us each; the total column is ms.
        let busiest = out.lines().nth(1).unwrap();
        assert!(busiest.contains("1.000"), "{}", busiest);
        let second = out.lines().nth(2).unwrap();
        assert!(
            second.contains("5.000") && second.contains("1000.00"),
            "{}",
            second
        );
    }

    #[test]
    fn an_unknown_syscall_number_is_named_after_its_number() {
        // No resolver is registered in a unit-test binary -- `linux-syscall`
        // installs it -- so this is the fallback path, and it must never be
        // empty or the table loses its first column.
        assert_eq!(name_of(4_242), "sys_4242");
    }

    #[test]
    fn only_the_lapic_vectors_carry_a_note() {
        assert_eq!(irq_note(0xf0), "LAPIC spurious");
        assert_eq!(irq_note(0xf1), "LAPIC timer");
        assert_eq!(irq_note(0xf2), "LAPIC error");
        assert_eq!(irq_note(0xf3), "");
        assert_eq!(irq_note(0x21), "");
    }

    #[test]
    fn a_syscall_past_the_table_still_counts_in_the_process_total() {
        let p = ProcPerf::new();
        p.record(3, 1_000);
        p.record(3, 3_000);
        p.record(PERF_NR as u32, 500); // out of range, must not index
        p.record(u32::MAX, 500);
        assert_eq!(p.totals(), (4, 5_000));
        assert_eq!(p.per[3].load(Relaxed), 2);
        assert_eq!(p.per_ns[3].load(Relaxed), 4_000);
    }

    // ── The sampling profiler ──────────────────────────────────────────────

    fn empty_samples() -> SampleState {
        SampleState {
            total: 0,
            dropped: 0,
            map: BTreeMap::new(),
        }
    }

    #[test]
    fn samples_cluster_into_buckets_so_one_function_adds_up() {
        let mut s = empty_samples();
        s.add(0x1000);
        s.add(0x1004);
        s.add(0x103f);
        s.add(0x1040); // the next bucket along
        assert_eq!(s.total, 4);
        assert_eq!(s.map.get(&0x1000), Some(&3));
        assert_eq!(s.map.get(&0x1040), Some(&1));
    }

    #[test]
    fn a_context_with_no_saved_pc_is_not_a_sample() {
        let mut s = empty_samples();
        s.add(0);
        assert_eq!(s.total, 0);
        assert!(s.map.is_empty());
    }

    #[test]
    fn a_full_table_drops_new_addresses_and_keeps_counting_the_old() {
        let mut s = empty_samples();
        // From one, not zero: a zero pc is not an address and `add` drops it,
        // so starting there would leave the table one bucket short.
        for i in 1..=TOP_MAX as u64 {
            s.add(i * PC_BUCKET);
        }
        assert_eq!(s.map.len(), TOP_MAX);
        assert_eq!(s.dropped, 0);
        s.add(0x9999_0000);
        assert_eq!(s.dropped, 1);
        assert_eq!(s.map.len(), TOP_MAX);
        // An address already in the table is still counted, full or not.
        s.add(0);
        s.add(PC_BUCKET);
        assert_eq!(s.map.get(&PC_BUCKET), Some(&2));
        assert_eq!(s.total, TOP_MAX as u64 + 2);
    }

    #[test]
    fn an_empty_profile_says_there_is_nothing_yet() {
        let out = render_top(0, 0, Vec::new());
        assert!(out.contains("no samples yet"), "{}", out);
        assert!(!out.contains("OVERHEAD"), "{}", out);
    }

    #[test]
    fn overheads_are_shares_of_the_samples_the_table_holds() {
        let out = render_top(100, 0, alloc::vec![(0x2000, 25), (0x1000, 75)]);
        let rows: Vec<&str> = out
            .lines()
            .skip_while(|l| !l.contains("OVERHEAD"))
            .collect();
        assert!(
            rows[1].contains("75.00%") && rows[1].contains("0x0000000000001000"),
            "{}",
            out
        );
        assert!(rows[2].contains("25.00%"), "{}", out);
        assert!(!out.contains("WARNING"), "{}", out);
    }

    #[test]
    fn a_full_bucket_table_says_so_and_scales_to_what_it_holds() {
        // 1000 samples taken, 900 of them on addresses the table had no room
        // for. Dividing the survivors by 1000 shows a flat profile with no row
        // over 8%, on a box where one address has every sample the table saw.
        let out = render_top(1_000, 900, alloc::vec![(0x1000, 80), (0x2000, 20)]);
        assert!(out.contains("WARNING"), "{}", out);
        assert!(out.contains("900 of 1000 samples"), "{}", out);
        assert!(
            out.contains("hottest code may not be in this list"),
            "{}",
            out
        );
        let rows: Vec<&str> = out
            .lines()
            .skip_while(|l| !l.contains("OVERHEAD"))
            .collect();
        assert!(rows[1].contains("80.00%"), "{}", out);
        assert!(rows[2].contains("20.00%"), "{}", out);
    }

    #[test]
    fn a_profile_whose_every_sample_was_dropped_reports_no_table() {
        let out = render_top(50, 50, Vec::new());
        assert!(out.contains("every sample was dropped"), "{}", out);
        assert!(!out.contains("OVERHEAD"), "{}", out);
    }
}
