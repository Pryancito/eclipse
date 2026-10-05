//! Behavioural anomaly detection (the IDS half of hunter).
//!
//! Signals produced on the syscall hot path, *after* the policy check (so they
//! only observe calls that were allowed to run):
//!
//! 1. **Sensitive-syscall watch** — constant-time classification against a
//!    per-architecture table of security-relevant operations (module loading,
//!    `ptrace`, `bpf`, credential / namespace changes, `kexec`, …). Matches are
//!    logged; with the optional privileged-deny latch they can also be blocked.
//! 2. **Rate anomalies** — per-process sliding-window counters that flag
//!    syscall **floods** and **fork bombs**, plus a **system-wide** fork-rate
//!    signal so a *distributed* fork bomb (each child forks once) still trips.
//! 3. **Adaptive baseline** — an EWMA of each process's syscalls-per-window,
//!    flagging a window that spikes far above *that process's own history*.
//!    This catches a process-relative flood (e.g. a quiet 200/s task jumping
//!    to 8k/s) that stays under the absolute flood threshold, while the
//!    absolute threshold still backstops a process whose baseline is high.
//!
//! Hardening: per-arch syscall numbers (P14, finding SYS-3/IDS-3); a
//! count-based window backstop so a frozen clock cannot silence detection
//! (P12); bounded per-process state with LRU eviction (P5); throttled WATCH
//! events so an attacker cannot use them as cheap log-ring filler (P11); and an
//! optional `Enforce` mode that actually denies confirmed anomalies (P15).

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use lock::Mutex;

use crate::clock;
use crate::event_log::{record, Severity};
use crate::policy::{self, Mode};

/// Master switch for the (locked) rate heuristics. The sensitive-syscall watch
/// is always cheap and stays on regardless.
///
/// Default OFF: with it on, EVERY syscall of every process pays a clock read
/// plus an IRQ-off shard-mutex + BTreeMap update, serializing all threads of a
/// process at syscall entry — a measurable tax on exactly the workloads a
/// desktop session hammers (Wayland/pipe round-trips are several syscalls
/// each). The constant-time sensitive-syscall WATCH below stays on either way;
/// only the flood/fork-bomb rate counters are opt-in (`HUNTERANOMALY=1`).
static ANOMALY_ENABLED: AtomicBool = AtomicBool::new(false);
/// Opt-in latch: when set and the anomaly domain is `Enforce`, syscalls in the
/// privileged class are denied outright (default off — never breaks boot).
static PRIVILEGED_DENY: AtomicBool = AtomicBool::new(false);

/// Sliding-window length for the rate heuristics.
const WINDOW_NS: u64 = 1_000_000_000; // 1 second
/// Per-process syscalls/window above which we suspect a denial-of-service flood.
const FLOOD_THRESHOLD: u32 = 50_000;
/// Adaptive flood: a window whose syscall count exceeds this multiple of the
/// process's own EWMA baseline is flagged, catching process-relative spikes
/// that stay below the absolute `FLOOD_THRESHOLD`.
const ADAPTIVE_MULT: u32 = 20;
/// Floor below which the adaptive detector stays silent, so low-volume or
/// merely bursty-but-benign processes never trip it.
const ADAPTIVE_MIN_COUNT: u32 = 2_000;
/// Clean (un-flagged) windows of history required before the adaptive baseline
/// is trusted enough to fire, so a process's first windows just train it.
const ADAPTIVE_MIN_WINDOWS: u16 = 4;
/// EWMA smoothing shift for the adaptive baseline: `ewma += (sample - ewma) >> N`
/// (alpha = 1/8). Slow enough that a ramp-up cannot quietly poison the baseline.
const EWMA_SHIFT: u32 = 3;
/// Per-process clone/fork calls/window above which we suspect a fork bomb.
const FORKBOMB_THRESHOLD: u32 = 200;
/// System-wide clone/fork calls/window flagged as a distributed fork bomb.
const SYS_FORKBOMB_THRESHOLD: u64 = 2_000;
/// Count backstop: roll the window after this many syscalls even if the clock
/// has not advanced, so a frozen/lying clock cannot disable detection (P12).
const WINDOW_EVENTS_BACKSTOP: u32 = 1_000_000;
/// The same backstop for the *system-wide* fork window, in the same ratio to
/// its threshold as [`WINDOW_EVENTS_BACKSTOP`] is to [`FLOOD_THRESHOLD`].
///
/// P12 gave the per-process window a count backstop and the module heading
/// says so, but the system-wide window never got one — and it is the one that
/// starts from a literal zero. `SYS_FORK_WINDOW_START` begins at 0, so with a
/// clock that reads 0 (none registered yet, or one that does not advance)
/// `now - start >= WINDOW_NS` is false forever: the window never rolls, the
/// counter runs from boot, the storm alarm fires once on the 2001st fork the
/// machine has ever done, and `SYS_FORK_ALERTED` then latches it silent for
/// the rest of the uptime. Exactly the failure P12 was written to prevent,
/// left in the one window it did not reach.
const SYS_FORK_EVENTS_BACKSTOP: u64 = 20 * SYS_FORKBOMB_THRESHOLD;
/// WATCH events emitted per process per window before further ones are
/// suppressed (still counted), bounding self-inflicted log pressure (P11).
const WATCH_BUDGET: u32 = 16;
/// Maximum processes tracked before the least-recently-active is evicted (P5).
const MAX_TRACKED_PIDS: usize = 4096;

/// Per-architecture syscall numbers. Absent operations use `u32::MAX` as a
/// sentinel that never matches a real syscall number.
mod nr {
    pub const ABSENT: u32 = u32::MAX;

    #[cfg(target_arch = "x86_64")]
    mod imp {
        pub const PTRACE: u32 = 101;
        pub const SETUID: u32 = 105;
        pub const SETGID: u32 = 106;
        pub const SETREUID: u32 = 113;
        pub const SETRESUID: u32 = 117;
        pub const PIVOT_ROOT: u32 = 155;
        pub const CHROOT: u32 = 161;
        pub const MOUNT: u32 = 165;
        pub const REBOOT: u32 = 169;
        pub const INIT_MODULE: u32 = 175;
        pub const DELETE_MODULE: u32 = 176;
        pub const KEXEC_LOAD: u32 = 246;
        pub const KEYCTL: u32 = 250;
        pub const UNSHARE: u32 = 272;
        pub const SETNS: u32 = 308;
        pub const PROCESS_VM_WRITEV: u32 = 311;
        pub const FINIT_MODULE: u32 = 313;
        pub const KEXEC_FILE_LOAD: u32 = 320;
        pub const BPF: u32 = 321;
        pub const CLONE: u32 = 56;
        pub const FORK: u32 = 57;
        pub const VFORK: u32 = 58;
    }

    // aarch64 and riscv64 use the asm-generic syscall table.
    #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
    mod imp {
        pub const PTRACE: u32 = 117;
        pub const SETUID: u32 = 146;
        pub const SETGID: u32 = 144;
        pub const SETREUID: u32 = 145;
        pub const SETRESUID: u32 = 147;
        pub const PIVOT_ROOT: u32 = 41;
        pub const CHROOT: u32 = 51;
        pub const MOUNT: u32 = 40;
        pub const REBOOT: u32 = 142;
        pub const INIT_MODULE: u32 = 105;
        pub const DELETE_MODULE: u32 = 106;
        pub const KEXEC_LOAD: u32 = 104;
        pub const KEYCTL: u32 = 219;
        pub const UNSHARE: u32 = 97;
        pub const SETNS: u32 = 268;
        pub const PROCESS_VM_WRITEV: u32 = 271;
        pub const FINIT_MODULE: u32 = 273;
        pub const KEXEC_FILE_LOAD: u32 = 294;
        pub const BPF: u32 = 280;
        pub const CLONE: u32 = 220;
        pub const FORK: u32 = super::ABSENT; // no fork/vfork in asm-generic
        pub const VFORK: u32 = super::ABSENT;
    }

    #[cfg(not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )))]
    mod imp {
        pub const PTRACE: u32 = super::ABSENT;
        pub const SETUID: u32 = super::ABSENT;
        pub const SETGID: u32 = super::ABSENT;
        pub const SETREUID: u32 = super::ABSENT;
        pub const SETRESUID: u32 = super::ABSENT;
        pub const PIVOT_ROOT: u32 = super::ABSENT;
        pub const CHROOT: u32 = super::ABSENT;
        pub const MOUNT: u32 = super::ABSENT;
        pub const REBOOT: u32 = super::ABSENT;
        pub const INIT_MODULE: u32 = super::ABSENT;
        pub const DELETE_MODULE: u32 = super::ABSENT;
        pub const KEXEC_LOAD: u32 = super::ABSENT;
        pub const KEYCTL: u32 = super::ABSENT;
        pub const UNSHARE: u32 = super::ABSENT;
        pub const SETNS: u32 = super::ABSENT;
        pub const PROCESS_VM_WRITEV: u32 = super::ABSENT;
        pub const FINIT_MODULE: u32 = super::ABSENT;
        pub const KEXEC_FILE_LOAD: u32 = super::ABSENT;
        pub const BPF: u32 = super::ABSENT;
        pub const CLONE: u32 = super::ABSENT;
        pub const FORK: u32 = super::ABSENT;
        pub const VFORK: u32 = super::ABSENT;
    }

    pub use imp::*;
}

/// `true` if `num` equals a present (non-sentinel) syscall constant.
#[inline]
fn is(num: u32, c: u32) -> bool {
    c != nr::ABSENT && num == c
}

/// Returns `(category, severity, name)` for security-sensitive syscalls.
fn classify(num: u32) -> Option<(&'static str, Severity, &'static str)> {
    if is(num, nr::INIT_MODULE) || is(num, nr::FINIT_MODULE) {
        return Some(("MODULE", Severity::Warning, "kernel module load"));
    }
    if is(num, nr::DELETE_MODULE) {
        return Some(("MODULE", Severity::Warning, "kernel module unload"));
    }
    if is(num, nr::KEXEC_LOAD) || is(num, nr::KEXEC_FILE_LOAD) {
        return Some(("MODULE", Severity::Warning, "kexec load"));
    }
    if is(num, nr::BPF) {
        return Some(("PRIVILEGE", Severity::Notice, "bpf"));
    }
    if is(num, nr::PTRACE) {
        return Some(("PRIVILEGE", Severity::Notice, "ptrace"));
    }
    if is(num, nr::SETUID) || is(num, nr::SETGID) || is(num, nr::SETREUID) || is(num, nr::SETRESUID)
    {
        return Some(("PRIVILEGE", Severity::Notice, "credential change"));
    }
    if is(num, nr::MOUNT) || is(num, nr::PIVOT_ROOT) || is(num, nr::CHROOT) {
        return Some(("PRIVILEGE", Severity::Notice, "fs namespace"));
    }
    if is(num, nr::UNSHARE) || is(num, nr::SETNS) {
        return Some(("PRIVILEGE", Severity::Notice, "namespace change"));
    }
    if is(num, nr::PROCESS_VM_WRITEV) {
        return Some(("PRIVILEGE", Severity::Notice, "cross-process write"));
    }
    if is(num, nr::KEYCTL) {
        return Some(("PRIVILEGE", Severity::Notice, "keyring"));
    }
    if is(num, nr::REBOOT) {
        return Some(("PRIVILEGE", Severity::Notice, "reboot"));
    }
    None
}

/// Whether `num` is in the privileged class that the deny latch may block.
fn is_privileged(num: u32) -> bool {
    is(num, nr::INIT_MODULE)
        || is(num, nr::FINIT_MODULE)
        || is(num, nr::DELETE_MODULE)
        || is(num, nr::KEXEC_LOAD)
        || is(num, nr::KEXEC_FILE_LOAD)
        || is(num, nr::BPF)
        || is(num, nr::PTRACE)
        || is(num, nr::MOUNT)
        || is(num, nr::PIVOT_ROOT)
        || is(num, nr::SETNS)
}

#[inline]
fn is_fork(num: u32) -> bool {
    is(num, nr::CLONE) || is(num, nr::FORK) || is(num, nr::VFORK)
}

/// Per-process sliding-window state for the rate heuristics.
struct ProcStat {
    window_start: u64,
    syscall_count: u32,
    fork_count: u32,
    watch_count: u32,
    flood_alerted: bool,
    fork_alerted: bool,
    /// Adaptive (process-relative) flood already reported this window.
    adaptive_alerted: bool,
    /// EWMA of syscalls-per-window — the process's learned baseline.
    ewma_syscalls: u32,
    /// Count of clean windows folded into `ewma_syscalls` (warm-up gate).
    windows_observed: u16,
}

impl ProcStat {
    fn new(now: u64) -> Self {
        Self {
            window_start: now,
            syscall_count: 0,
            fork_count: 0,
            watch_count: 0,
            flood_alerted: false,
            fork_alerted: false,
            adaptive_alerted: false,
            ewma_syscalls: 0,
            windows_observed: 0,
        }
    }
    /// Resets the window if the clock advanced past it OR the event backstop
    /// tripped (the latter keeps windows progressing under a frozen clock).
    fn roll(&mut self, now: u64) {
        let elapsed = now.saturating_sub(self.window_start) >= WINDOW_NS;
        let backstop = self.syscall_count >= WINDOW_EVENTS_BACKSTOP;
        if !(elapsed || backstop) {
            return;
        }
        // Fold a *completed time* window into the adaptive baseline — but never
        // a backstop window (an in-progress flood, not a full second) and never
        // a window we already flagged, so an attacker cannot train the baseline
        // upward to silence future detection.
        if elapsed && !self.flood_alerted && !self.adaptive_alerted {
            let sample = self.syscall_count;
            let ewma = self.ewma_syscalls;
            self.ewma_syscalls = if sample >= ewma {
                ewma + ((sample - ewma) >> EWMA_SHIFT)
            } else {
                ewma - ((ewma - sample) >> EWMA_SHIFT)
            };
            self.windows_observed = self.windows_observed.saturating_add(1);
        }
        self.window_start = now;
        self.syscall_count = 0;
        self.fork_count = 0;
        self.watch_count = 0;
        self.flood_alerted = false;
        self.fork_alerted = false;
        self.adaptive_alerted = false;
    }
    /// Whether the current window is a flood *relative to this process's own*
    /// learned baseline. Pure (no clock, no logging) so it is unit-testable.
    fn adaptive_flood(&self) -> bool {
        self.windows_observed >= ADAPTIVE_MIN_WINDOWS
            && self.syscall_count > ADAPTIVE_MIN_COUNT
            && (self.syscall_count as u64) > (ADAPTIVE_MULT as u64) * (self.ewma_syscalls as u64)
    }
}

/// Number of independently-locked shards for the per-process stats. The map
/// is written on the syscall hot path from every CPU; a single global lock
/// serialized all of them (and disabled IRQs while doing so). Sharding by pid
/// keeps unrelated processes on unrelated locks/cachelines — same-pid calls
/// still serialize, which the per-pid counters require anyway.
const STAT_SHARDS: usize = 16;

/// One shard, padded to its own cacheline so neighbouring shard locks do not
/// false-share.
#[repr(align(64))]
struct StatShard(Mutex<BTreeMap<u64, ProcStat>>);

lazy_static::lazy_static! {
    static ref PROC_STATS: [StatShard; STAT_SHARDS] = {
        // `Mutex::new` is const but arrays of non-Copy need explicit build.
        core::array::from_fn(|_| StatShard(Mutex::new(BTreeMap::new())))
    };
}

#[inline]
fn stats_shard(pid: u64) -> &'static Mutex<BTreeMap<u64, ProcStat>> {
    &PROC_STATS[(pid as usize) % STAT_SHARDS].0
}

/// System-wide fork accounting for distributed fork-bomb detection.
static SYS_FORK_WINDOW_START: AtomicU64 = AtomicU64::new(0);
static SYS_FORK_COUNT: AtomicU64 = AtomicU64::new(0);
static SYS_FORK_ALERTED: AtomicBool = AtomicBool::new(false);
lazy_static::lazy_static! {
    /// Taken only to start a new system-wide fork window, so the three stores
    /// that make one up cannot interleave. See [`roll_sys_fork_window`].
    static ref SYS_FORK_ROLL: Mutex<()> = Mutex::new(());
}

/// Starts a new system-wide fork window if the current one is over, and returns
/// whether this call was the one that started it.
///
/// The reset used to be a bare load / compare / three stores. There is no
/// single-CPU way to notice what that costs, which is why it survived: it only
/// misbehaves when several CPUs fork at once, which is precisely the
/// *distributed* fork bomb this counter exists to catch. Every CPU that saw the
/// window expire zeroed it, and a CPU delayed between its load and its stores
/// zeroed a window that had already been counted -- clearing `SYS_FORK_ALERTED`
/// with it, so the storm went back to counting from nothing.
///
/// The decision is re-taken under a lock, which is what makes it one roll per
/// window: the second CPU in finds the window it wanted to replace already
/// replaced. The lock-free pre-check keeps the fork path's usual case to two
/// relaxed loads, and the lock itself is reached about once a second.
fn roll_sys_fork_window(now: u64) -> bool {
    if !window_is_over(now) {
        return false;
    }
    let _guard = SYS_FORK_ROLL.lock();
    if !window_is_over(now) {
        return false;
    }
    SYS_FORK_WINDOW_START.store(now, Ordering::Relaxed);
    SYS_FORK_COUNT.store(0, Ordering::Relaxed);
    SYS_FORK_ALERTED.store(false, Ordering::Relaxed);
    true
}

/// Whether the current system-wide fork window has run out, by the clock or by
/// the count backstop.
fn window_is_over(now: u64) -> bool {
    let start = SYS_FORK_WINDOW_START.load(Ordering::Relaxed);
    now.saturating_sub(start) >= WINDOW_NS
        || SYS_FORK_COUNT.load(Ordering::Relaxed) >= SYS_FORK_EVENTS_BACKSTOP
}

/// Enables or disables the per-process rate heuristics.
pub fn set_anomaly_detection(enabled: bool) {
    ANOMALY_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Enables/disables the privileged-syscall deny latch (only bites when the
/// anomaly domain is also `Enforce`). Default off; opt-in only.
pub fn set_privileged_deny(enabled: bool) {
    PRIVILEGED_DENY.store(enabled, Ordering::Relaxed);
}

/// Inspects one syscall for anomalies. Returns `true` to allow the call, or
/// `false` to deny it (only ever happens when the anomaly domain is `Enforce`).
/// Called on the syscall hot path *after* the policy check.
pub fn on_syscall(pid: u64, num: u32) -> bool {
    // (1) Constant-time sensitive-syscall classification (no lock, no clock).
    let classified = classify(num);
    let anomaly = ANOMALY_ENABLED.load(Ordering::Relaxed);

    // Hot-path early out: an ordinary syscall with the rate heuristics off
    // touches no shared state at all.
    if classified.is_none() && !anomaly {
        return true;
    }

    let enforce = policy::anomaly_mode() == Mode::Enforce;

    if let Some((category, _severity, name)) = classified {
        // Privileged-deny latch: block module/ptrace/bpf/... under Enforce.
        if enforce && PRIVILEGED_DENY.load(Ordering::Relaxed) && is_privileged(num) {
            record(
                pid,
                Severity::Critical,
                category,
                "BLOCKED",
                format!("blocked privileged syscall #{} ({})", num, name),
            );
            return false;
        }
    }

    let now = clock::now_ns();
    let forking = anomaly && is_fork(num);

    // Single pass under the pid's shard lock: WATCH throttling and the rate
    // counters share one roll of the window (the original code locked and
    // rolled twice for sensitive syscalls).
    let mut emit_watch = false;
    let mut alert_flood = false;
    let mut alert_fork = false;
    let mut alert_adaptive = false;
    // Per-process window count and baseline captured under the lock for the
    // adaptive anomaly message.
    let mut spike_count = 0u32;
    let mut baseline = 0u32;
    {
        let mut stats = stats_shard(pid).lock();
        evict_if_needed(&mut stats, pid);
        let st = stats.entry(pid).or_insert_with(|| ProcStat::new(now));
        st.roll(now);
        if classified.is_some() {
            // Throttled WATCH so an attacker cannot use these as log-ring filler.
            st.watch_count = st.watch_count.saturating_add(1);
            emit_watch = st.watch_count <= WATCH_BUDGET;
        }
        if anomaly {
            st.syscall_count = st.syscall_count.saturating_add(1);
            if forking {
                st.fork_count = st.fork_count.saturating_add(1);
            }
            if !st.flood_alerted && st.syscall_count > FLOOD_THRESHOLD {
                st.flood_alerted = true;
                alert_flood = true;
            }
            // Adaptive flood: suppressed once the absolute flood already fired
            // this window, since that path reports the same burst more precisely.
            if !st.flood_alerted && !st.adaptive_alerted && st.adaptive_flood() {
                st.adaptive_alerted = true;
                alert_adaptive = true;
                spike_count = st.syscall_count;
                baseline = st.ewma_syscalls;
            }
            if !st.fork_alerted && st.fork_count > FORKBOMB_THRESHOLD {
                st.fork_alerted = true;
                alert_fork = true;
            }
        }
    }
    if let (true, Some((category, severity, name))) = (emit_watch, classified) {
        record(
            pid,
            severity,
            category,
            "WATCH",
            format!("sensitive syscall #{} ({})", num, name),
        );
    }
    if !anomaly {
        return true;
    }

    // System-wide fork-rate window (lock-free), for distributed fork bombs.
    let mut alert_sys_fork = false;
    if forking {
        roll_sys_fork_window(now);
        let total = SYS_FORK_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if total > SYS_FORKBOMB_THRESHOLD && !SYS_FORK_ALERTED.swap(true, Ordering::Relaxed) {
            alert_sys_fork = true;
        }
    }

    let mut deny = false;
    if alert_flood {
        log_anomaly(
            pid,
            enforce,
            format!(
                "syscall flood: >{} syscalls within {}ms",
                FLOOD_THRESHOLD,
                WINDOW_NS / 1_000_000
            ),
        );
        deny |= enforce;
    }
    if alert_adaptive {
        log_anomaly(
            pid,
            enforce,
            format!(
            "adaptive syscall spike: {} within {}ms is >{}x the per-process baseline (~{}/window)",
            spike_count,
            WINDOW_NS / 1_000_000,
            ADAPTIVE_MULT,
            baseline
        ),
        );
        deny |= enforce;
    }
    if alert_fork {
        log_anomaly(
            pid,
            enforce,
            format!(
                "possible fork bomb: >{} clone/fork within {}ms",
                FORKBOMB_THRESHOLD,
                WINDOW_NS / 1_000_000
            ),
        );
        deny |= enforce;
    }
    if alert_sys_fork {
        log_anomaly(
            pid,
            enforce,
            format!(
                "system-wide fork storm: >{} clone/fork within {}ms",
                SYS_FORKBOMB_THRESHOLD,
                WINDOW_NS / 1_000_000
            ),
        );
        deny |= enforce;
    }

    !deny
}

fn log_anomaly(pid: u64, enforce: bool, msg: alloc::string::String) {
    if enforce {
        record(pid, Severity::Critical, "ANOMALY", "BLOCKED", msg);
    } else {
        record(pid, Severity::Warning, "ANOMALY", "WARNING", msg);
    }
}

/// Resets a process's anomaly window across `execve` so a benign-then-malicious
/// image transition cannot launder accumulated counters (P4).
pub fn on_exec(pid: u64) {
    let now = clock::now_ns();
    stats_shard(pid).lock().insert(pid, ProcStat::new(now));
}

/// Drops per-process heuristic state when a process exits.
pub fn forget(pid: u64) {
    stats_shard(pid).lock().remove(&pid);
}

/// Evicts the least-recently-active process if inserting `pid` would exceed the
/// cap (and `pid` is not already tracked), bounding memory under spawn floods.
fn evict_if_needed(map: &mut BTreeMap<u64, ProcStat>, pid: u64) {
    // `map` is one shard; bound each shard to its slice of the global cap so
    // total tracked state stays at MAX_TRACKED_PIDS across all shards.
    if map.len() < MAX_TRACKED_PIDS / STAT_SHARDS || map.contains_key(&pid) {
        return;
    }
    if let Some((&victim, _)) = map.iter().min_by_key(|(_, st)| st.window_start) {
        map.remove(&victim);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Drives `ProcStat` directly with synthetic timestamps so the adaptive
    // baseline is exercised without the global (sealed) clock.
    #[test]
    fn adaptive_baseline_learns_then_flags_relative_spike() {
        let mut st = ProcStat::new(0);
        let mut t = 0u64;

        // Several calm windows of ~200 syscalls each train the baseline.
        for _ in 0..6 {
            st.syscall_count = 200;
            t += WINDOW_NS;
            st.roll(t);
        }
        assert!(st.windows_observed >= ADAPTIVE_MIN_WINDOWS);
        assert!(st.ewma_syscalls > 0 && st.ewma_syscalls < ADAPTIVE_MIN_COUNT);

        // A calm window — and a fork-heavy but low-volume one — do not trip.
        st.syscall_count = 200;
        assert!(!st.adaptive_flood());
        st.syscall_count = ADAPTIVE_MIN_COUNT; // exactly at the floor: still quiet
        assert!(!st.adaptive_flood());

        // A spike far above this process's own baseline (and above the floor)
        // trips, even though it is well below the absolute FLOOD_THRESHOLD.
        st.syscall_count = 8_000;
        assert!(st.syscall_count < FLOOD_THRESHOLD);
        assert!(st.adaptive_flood());
    }

    #[test]
    fn flagged_window_does_not_poison_baseline() {
        let mut st = ProcStat::new(0);
        let mut t = 0u64;
        for _ in 0..4 {
            st.syscall_count = 100;
            t += WINDOW_NS;
            st.roll(t);
        }
        let before = st.ewma_syscalls;

        // A window we already flagged must not be folded into the baseline,
        // otherwise an attacker could train it upward to silence detection.
        st.syscall_count = 1_000_000;
        st.adaptive_alerted = true;
        t += WINDOW_NS;
        st.roll(t);
        assert_eq!(st.ewma_syscalls, before);
    }

    #[test]
    fn cold_process_never_trips_adaptively() {
        // Before the warm-up gate, even a large count does not trip (no
        // trusted baseline yet); the absolute threshold still backstops it.
        let mut st = ProcStat::new(0);
        st.syscall_count = 40_000;
        assert!(st.windows_observed < ADAPTIVE_MIN_WINDOWS);
        assert!(!st.adaptive_flood());
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;
    use crate::test_globals;

    extern crate std;

    fn reset() {
        SYS_FORK_WINDOW_START.store(0, Ordering::Relaxed);
        SYS_FORK_COUNT.store(0, Ordering::Relaxed);
        SYS_FORK_ALERTED.store(false, Ordering::Relaxed);
    }

    #[test]
    fn a_live_window_is_not_rolled() {
        let _g = test_globals::lock();
        reset();
        assert!(roll_sys_fork_window(WINDOW_NS * 4));
        SYS_FORK_COUNT.store(SYS_FORKBOMB_THRESHOLD + 1, Ordering::Relaxed);
        SYS_FORK_ALERTED.store(true, Ordering::Relaxed);
        // Half a second later the storm is still the same storm: rolling here
        // would reset the count under a live clock and hide it.
        assert!(!roll_sys_fork_window(WINDOW_NS * 4 + WINDOW_NS / 2));
        assert_eq!(
            SYS_FORK_COUNT.load(Ordering::Relaxed),
            SYS_FORKBOMB_THRESHOLD + 1
        );
        reset();
    }

    #[test]
    fn a_finished_window_is_rolled_and_rearms_the_alarm() {
        let _g = test_globals::lock();
        reset();
        assert!(roll_sys_fork_window(WINDOW_NS * 4));
        SYS_FORK_COUNT.store(SYS_FORKBOMB_THRESHOLD + 1, Ordering::Relaxed);
        SYS_FORK_ALERTED.store(true, Ordering::Relaxed);
        assert!(roll_sys_fork_window(WINDOW_NS * 5));
        assert_eq!(SYS_FORK_COUNT.load(Ordering::Relaxed), 0);
        assert!(!SYS_FORK_ALERTED.load(Ordering::Relaxed));
        reset();
    }

    #[test]
    fn a_clock_that_never_advances_does_not_silence_the_fork_storm_alarm() {
        let _g = test_globals::lock();
        reset();
        // `now_ns()` reads 0 until the kernel registers a time source, and a
        // frozen clock reads the same value forever. `SYS_FORK_WINDOW_START`
        // starts at 0 too, so `now - start >= WINDOW_NS` was false for the
        // whole uptime: the counter ran from boot, the alarm fired once on the
        // 2001st fork the machine had ever done, and SYS_FORK_ALERTED then
        // latched it silent. P12 put a count backstop on the per-process
        // window for exactly this and the module heading says so -- the
        // system-wide window simply never got one.
        let frozen = 0u64;
        assert!(!roll_sys_fork_window(frozen), "nothing to roll yet");
        SYS_FORK_ALERTED.store(true, Ordering::Relaxed);
        SYS_FORK_COUNT.store(SYS_FORK_EVENTS_BACKSTOP - 1, Ordering::Relaxed);
        assert!(!roll_sys_fork_window(frozen), "still below the backstop");
        SYS_FORK_COUNT.store(SYS_FORK_EVENTS_BACKSTOP, Ordering::Relaxed);
        assert!(
            roll_sys_fork_window(frozen),
            "the count backstop must roll the window with the clock frozen"
        );
        assert_eq!(SYS_FORK_COUNT.load(Ordering::Relaxed), 0);
        assert!(
            !SYS_FORK_ALERTED.load(Ordering::Relaxed),
            "a rolled window must re-arm the alarm"
        );
        reset();
    }

    #[test]
    fn the_backstop_is_far_above_the_threshold_it_backs() {
        // If the backstop were near the threshold it would roll the window in
        // the middle of a real storm and reset the very count that detects it.
        assert!(SYS_FORK_EVENTS_BACKSTOP > SYS_FORKBOMB_THRESHOLD * 4);
        assert!(WINDOW_EVENTS_BACKSTOP as u64 > FLOOD_THRESHOLD as u64 * 4);
    }

    #[test]
    fn only_one_cpu_rolls_a_given_window() {
        let _g = test_globals::lock();
        reset();
        // This is the ordering half, and no single-threaded mutation can see
        // it: the reset used to be a bare load / compare / three stores, so
        // every CPU that saw the window expire zeroed it -- on the many-CPU
        // storm this signal exists for. A CPU delayed between its load and its
        // stores zeroed a window that had already been counted and cleared the
        // alarm with it. A real writer thread is the only witness there is.
        const THREADS: usize = 8;
        const ROUNDS: u64 = 200;
        // Two phases per round: every thread is lined up on the same expired
        // window before anyone tries to roll it, and nobody runs ahead into
        // the next round until this one is settled.
        let gate = self::std::sync::Arc::new(self::std::sync::Barrier::new(THREADS));
        let rolls = self::std::sync::Arc::new(core::sync::atomic::AtomicU64::new(0));
        let mut handles = alloc::vec::Vec::new();
        for _ in 0..THREADS {
            let gate = gate.clone();
            let rolls = rolls.clone();
            handles.push(self::std::thread::spawn(move || {
                for r in 1..=ROUNDS {
                    gate.wait();
                    if roll_sys_fork_window(r * WINDOW_NS * 2) {
                        rolls.fetch_add(1, Ordering::Relaxed);
                    }
                    gate.wait();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            rolls.load(Ordering::Relaxed),
            ROUNDS,
            "one window, one roll: {} CPUs saw each of the {} windows expire",
            THREADS,
            ROUNDS
        );
        assert_eq!(
            SYS_FORK_WINDOW_START.load(Ordering::Relaxed),
            ROUNDS * WINDOW_NS * 2,
            "the last window to be started is the last deadline seen"
        );
        reset();
    }
}

#[cfg(test)]
mod classification_tests {
    //! The sensitive-syscall table: constant-time, lock-free, and the half of
    //! the IDS that runs even with the rate counters switched off. None of it
    //! touches global state, so none of it needs the test lock.
    //!
    //! Every operation named below was in the table with nothing asserting
    //! it. Dropping a line, or demoting its severity, changed nothing any
    //! test could see -- and this table is the whole of what hunter watches
    //! for on the syscall path.

    use super::*;

    #[test]
    fn the_sentinel_for_an_absent_operation_never_matches_anything() {
        // An architecture without an operation spells it `ABSENT`
        // (`u32::MAX`). A syscall number of `u32::MAX` must not read as
        // "every absent operation at once", which is what dropping either
        // half of `is` does.
        assert!(!is(nr::ABSENT, nr::ABSENT));
        assert!(!is(0, nr::ABSENT));
        assert!(classify(nr::ABSENT).is_none());
        assert!(!is_privileged(nr::ABSENT));
        assert!(!is_fork(nr::ABSENT));
    }

    #[test]
    fn an_ordinary_syscall_is_not_watched_at_all() {
        // `read` on x86_64, and on none of the lists here.
        assert!(classify(0).is_none());
        assert!(!is_privileged(0));
        assert!(!is_fork(0));
    }

    #[test]
    fn loading_a_kernel_module_is_a_warning_whichever_call_does_it() {
        let expected = Some(("MODULE", Severity::Warning, "kernel module load"));
        assert_eq!(classify(nr::INIT_MODULE), expected);
        assert_eq!(classify(nr::FINIT_MODULE), expected);
    }

    #[test]
    fn unloading_a_kernel_module_is_watched_like_loading_one() {
        assert_eq!(
            classify(nr::DELETE_MODULE),
            Some(("MODULE", Severity::Warning, "kernel module unload"))
        );
    }

    #[test]
    fn both_ways_of_loading_a_new_kernel_are_watched() {
        let expected = Some(("MODULE", Severity::Warning, "kexec load"));
        assert_eq!(classify(nr::KEXEC_LOAD), expected);
        assert_eq!(classify(nr::KEXEC_FILE_LOAD), expected);
    }

    #[test]
    fn loading_bpf_is_watched() {
        assert_eq!(
            classify(nr::BPF),
            Some(("PRIVILEGE", Severity::Notice, "bpf"))
        );
    }

    #[test]
    fn attaching_to_another_process_is_watched() {
        assert_eq!(
            classify(nr::PTRACE),
            Some(("PRIVILEGE", Severity::Notice, "ptrace"))
        );
    }

    #[test]
    fn every_way_of_changing_credentials_is_one_category() {
        let expected = Some(("PRIVILEGE", Severity::Notice, "credential change"));
        assert_eq!(classify(nr::SETUID), expected);
        assert_eq!(classify(nr::SETGID), expected);
        assert_eq!(classify(nr::SETREUID), expected);
        assert_eq!(classify(nr::SETRESUID), expected);
    }

    #[test]
    fn every_way_of_moving_the_filesystem_root_is_watched() {
        let expected = Some(("PRIVILEGE", Severity::Notice, "fs namespace"));
        assert_eq!(classify(nr::MOUNT), expected);
        assert_eq!(classify(nr::PIVOT_ROOT), expected);
        assert_eq!(classify(nr::CHROOT), expected);
    }

    #[test]
    fn both_ways_of_changing_namespace_are_watched() {
        let expected = Some(("PRIVILEGE", Severity::Notice, "namespace change"));
        assert_eq!(classify(nr::UNSHARE), expected);
        assert_eq!(classify(nr::SETNS), expected);
    }

    #[test]
    fn writing_into_the_memory_of_another_process_is_watched() {
        assert_eq!(
            classify(nr::PROCESS_VM_WRITEV),
            Some(("PRIVILEGE", Severity::Notice, "cross-process write"))
        );
    }

    #[test]
    fn reaching_the_keyring_is_watched() {
        assert_eq!(
            classify(nr::KEYCTL),
            Some(("PRIVILEGE", Severity::Notice, "keyring"))
        );
    }

    #[test]
    fn asking_the_machine_to_reboot_is_watched() {
        assert_eq!(
            classify(nr::REBOOT),
            Some(("PRIVILEGE", Severity::Notice, "reboot"))
        );
    }

    #[test]
    fn the_deny_latch_covers_the_calls_that_hand_over_the_kernel() {
        // Everything that loads code into the kernel, reads or writes another
        // process, or moves the filesystem root under it.
        for num in [
            nr::INIT_MODULE,
            nr::FINIT_MODULE,
            nr::DELETE_MODULE,
            nr::KEXEC_LOAD,
            nr::KEXEC_FILE_LOAD,
            nr::BPF,
            nr::PTRACE,
            nr::MOUNT,
            nr::PIVOT_ROOT,
            nr::SETNS,
        ] {
            assert!(is_privileged(num), "syscall #{} left the deny class", num);
        }
        // Watched, but not something the latch may block: denying these
        // outright breaks ordinary programs, which is why the latch is a
        // narrower list than `classify`.
        assert!(!is_privileged(nr::SETUID));
        assert!(!is_privileged(nr::REBOOT));
        assert!(!is_privileged(nr::KEYCTL));
    }

    #[test]
    fn every_spelling_of_fork_the_target_has_counts_as_forking() {
        // The table is per-architecture and the absent entries are the
        // sentinel, which never matches anything on purpose (the first test in
        // this module is that contract). asm-generic -- aarch64 and riscv64 --
        // has no `fork` and no `vfork` at all, so demanding all three
        // unconditionally is demanding that the sentinel does match. The
        // `Unit Test` job runs on x86_64 only, which is why that passed.
        for (name, num) in [
            ("clone", nr::CLONE),
            ("fork", nr::FORK),
            ("vfork", nr::VFORK),
        ] {
            if num == nr::ABSENT {
                assert!(!is_fork(num), "{} is absent on this target", name);
            } else {
                assert!(is_fork(num), "{} is a way of forking", name);
            }
        }
        assert!(!is_fork(nr::PTRACE));
    }
}

#[cfg(test)]
mod window_roll_tests {
    //! `ProcStat::roll` and `adaptive_flood` on their own, with synthetic
    //! timestamps: no clock, no locks, no log. What the existing `tests`
    //! module above covers is the baseline learning; this covers when a
    //! window ends, what it carries into the next one, and where each
    //! threshold actually sits.

    use super::*;

    #[test]
    fn a_window_that_only_ran_out_of_time_still_rolls() {
        let mut st = ProcStat::new(0);
        st.syscall_count = 5;
        st.fork_count = 2;
        st.roll(WINDOW_NS);
        assert_eq!(st.syscall_count, 0);
        assert_eq!(st.fork_count, 0);
    }

    #[test]
    fn a_window_that_only_ran_out_of_room_still_rolls() {
        // The count backstop is the half that survives a frozen clock, so it
        // has to roll the window on its own.
        let mut st = ProcStat::new(0);
        st.syscall_count = WINDOW_EVENTS_BACKSTOP;
        st.roll(0);
        assert_eq!(st.syscall_count, 0);
    }

    #[test]
    fn a_backstop_window_does_not_train_the_baseline() {
        // Only a completed *time* window is a sample of the process's
        // ordinary rate; one cut short by the count backstop is the middle of
        // a flood, and folding it in would teach the baseline the flood.
        let mut st = ProcStat::new(0);
        st.syscall_count = WINDOW_EVENTS_BACKSTOP;
        st.roll(0);
        assert_eq!(st.windows_observed, 0);
        assert_eq!(st.ewma_syscalls, 0);
    }

    #[test]
    fn a_window_flagged_by_the_absolute_flood_does_not_train_the_baseline() {
        // The sibling of `flagged_window_does_not_poison_baseline`, which
        // only ever set `adaptive_alerted`: the same has to hold for a window
        // the absolute threshold flagged, or a flood trains the baseline that
        // is supposed to detect the next one.
        let mut st = ProcStat::new(0);
        let mut t = 0u64;
        for _ in 0..4 {
            st.syscall_count = 100;
            t += WINDOW_NS;
            st.roll(t);
        }
        let before = st.ewma_syscalls;
        st.syscall_count = 1_000_000;
        st.flood_alerted = true;
        t += WINDOW_NS;
        st.roll(t);
        assert_eq!(st.ewma_syscalls, before);
    }

    #[test]
    fn the_baseline_moves_one_eighth_of_the_way_towards_each_sample() {
        let mut st = ProcStat::new(0);
        st.syscall_count = 800;
        st.roll(WINDOW_NS);
        assert_eq!(st.ewma_syscalls, 100, "0 + (800 - 0) >> 3");
        st.syscall_count = 800;
        st.roll(WINDOW_NS * 2);
        assert_eq!(st.ewma_syscalls, 100 + ((800 - 100) >> 3));
    }

    #[test]
    fn a_quieter_window_brings_the_baseline_down() {
        let mut st = ProcStat::new(0);
        st.ewma_syscalls = 800;
        st.syscall_count = 0;
        st.roll(WINDOW_NS);
        assert_eq!(st.ewma_syscalls, 800 - (800 >> 3));
    }

    #[test]
    fn a_rolled_window_starts_where_the_clock_is_now() {
        // Left at zero, every later call looks like a finished window and the
        // live one is rolled out from under the process -- which resets the
        // very counters that were about to trip.
        let mut st = ProcStat::new(0);
        st.roll(WINDOW_NS * 4);
        st.syscall_count = 9;
        st.roll(WINDOW_NS * 4 + WINDOW_NS / 2);
        assert_eq!(
            st.syscall_count, 9,
            "half a second later is the same window"
        );
    }

    #[test]
    fn a_rolled_window_rearms_every_alarm_it_carried() {
        let mut st = ProcStat::new(0);
        st.flood_alerted = true;
        st.fork_alerted = true;
        st.adaptive_alerted = true;
        st.roll(WINDOW_NS);
        assert!(!st.flood_alerted);
        assert!(!st.fork_alerted);
        assert!(!st.adaptive_alerted);
    }

    #[test]
    fn the_warm_up_gate_is_satisfied_by_exactly_its_own_number_of_windows() {
        let mut st = ProcStat::new(0);
        st.ewma_syscalls = 10;
        st.syscall_count = ADAPTIVE_MIN_COUNT + 1;
        st.windows_observed = ADAPTIVE_MIN_WINDOWS - 1;
        assert!(!st.adaptive_flood(), "one window short is still warming up");
        st.windows_observed = ADAPTIVE_MIN_WINDOWS;
        assert!(st.adaptive_flood());
    }

    #[test]
    fn the_quiet_floor_is_a_number_to_pass_not_to_reach() {
        let mut st = ProcStat::new(0);
        st.windows_observed = ADAPTIVE_MIN_WINDOWS;
        st.ewma_syscalls = 1;
        st.syscall_count = ADAPTIVE_MIN_COUNT;
        assert!(!st.adaptive_flood());
        st.syscall_count = ADAPTIVE_MIN_COUNT + 1;
        assert!(st.adaptive_flood());
    }

    #[test]
    fn the_spike_is_a_multiple_of_the_baseline_not_a_distance_from_it() {
        let mut st = ProcStat::new(0);
        st.windows_observed = ADAPTIVE_MIN_WINDOWS;
        st.ewma_syscalls = 200;
        st.syscall_count = 3_000;
        assert!(
            st.syscall_count > ADAPTIVE_MIN_COUNT,
            "past the quiet floor"
        );
        assert!(
            !st.adaptive_flood(),
            "fifteen times the baseline is not twenty"
        );
        st.syscall_count = ADAPTIVE_MULT * 200;
        assert!(!st.adaptive_flood(), "exactly the multiple is not above it");
        st.syscall_count = ADAPTIVE_MULT * 200 + 1;
        assert!(st.adaptive_flood());
    }
}

#[cfg(test)]
mod tracking_tests {
    //! Which shard a process lands on, what happens when a shard fills up,
    //! and what the process lifecycle hooks do to all of it. The bound on
    //! tracked processes is what stops a spawn flood turning the detector
    //! into the memory leak it was watching for.

    use super::*;

    /// A clock far from zero, so "the window starts now" and "the window
    /// starts at boot" cannot be confused for one another.
    const T_EXEC: u64 = 9 * WINDOW_NS;
    fn exec_clock() -> u64 {
        T_EXEC
    }

    fn a_shard_full_of_processes() -> BTreeMap<u64, ProcStat> {
        let mut map = BTreeMap::new();
        for i in 0..(MAX_TRACKED_PIDS / STAT_SHARDS) as u64 {
            // Ascending `window_start`: pid 0 is the least recently active.
            map.insert(i, ProcStat::new(1_000 + i));
        }
        map
    }

    #[test]
    fn a_process_always_lands_on_the_same_shard_and_every_shard_is_reachable() {
        for pid in 0..STAT_SHARDS as u64 {
            assert!(
                core::ptr::eq(stats_shard(pid), stats_shard(pid + STAT_SHARDS as u64)),
                "pid {} moved shard between windows",
                pid
            );
        }
        for a in 0..STAT_SHARDS as u64 {
            for b in (a + 1)..STAT_SHARDS as u64 {
                assert!(
                    !core::ptr::eq(stats_shard(a), stats_shard(b)),
                    "pids {} and {} share a shard, so one shard is never used",
                    a,
                    b
                );
            }
        }
    }

    #[test]
    fn a_full_shard_evicts_the_least_recently_active_process() {
        let cap = MAX_TRACKED_PIDS / STAT_SHARDS;
        let mut map = a_shard_full_of_processes();
        assert_eq!(map.len(), cap);
        evict_if_needed(&mut map, 9_999);
        assert_eq!(map.len(), cap - 1);
        assert!(!map.contains_key(&0), "the oldest window is the one to go");
        assert!(map.contains_key(&((cap - 1) as u64)));
    }

    #[test]
    fn a_shard_below_its_share_of_the_cap_evicts_nobody() {
        let mut map = a_shard_full_of_processes();
        map.remove(&0);
        let before = map.len();
        evict_if_needed(&mut map, 9_999);
        assert_eq!(map.len(), before);
    }

    #[test]
    fn a_process_already_tracked_evicts_nobody() {
        // Its own entry is the one being updated, so nothing has to make room.
        let mut map = a_shard_full_of_processes();
        let before = map.len();
        evict_if_needed(&mut map, 0);
        assert_eq!(map.len(), before);
        assert!(map.contains_key(&0));
    }

    #[test]
    fn an_exec_starts_the_new_image_on_a_window_of_its_own() {
        let _g = crate::test_globals::lock();
        crate::clock::reset_for_test();
        crate::clock::set_time_source(exec_clock);
        let pid = 7_101;
        {
            let mut m = stats_shard(pid).lock();
            let mut st = ProcStat::new(0);
            st.syscall_count = 4_000;
            st.fork_count = 40;
            m.insert(pid, st);
        }
        on_exec(pid);
        {
            let m = stats_shard(pid).lock();
            let st = m.get(&pid).expect("on_exec keeps tracking the process");
            assert_eq!(st.syscall_count, 0, "a new image cannot inherit counters");
            assert_eq!(st.fork_count, 0);
            assert_eq!(
                st.window_start, T_EXEC,
                "the new window starts now, not at boot"
            );
        }
        forget(pid);
        crate::clock::reset_for_test();
    }

    #[test]
    fn a_process_that_exits_is_forgotten() {
        let _g = crate::test_globals::lock();
        let pid = 7_102;
        stats_shard(pid).lock().insert(pid, ProcStat::new(0));
        forget(pid);
        assert!(!stats_shard(pid).lock().contains_key(&pid));
    }

    #[test]
    fn a_fork_reseeds_the_child_and_leaves_the_parent_counting() {
        // The hook lives in the crate root; what it must do is here, because
        // only this module can see the counters it is supposed to reset.
        let _g = crate::test_globals::lock();
        crate::clock::reset_for_test();
        crate::clock::set_time_source(exec_clock);
        let (parent, child) = (7_201u64, 7_202u64);
        for pid in [parent, child] {
            let mut st = ProcStat::new(0);
            st.syscall_count = 900;
            stats_shard(pid).lock().insert(pid, st);
        }
        crate::task_fork(parent, child);
        assert_eq!(
            stats_shard(child).lock().get(&child).unwrap().syscall_count,
            0,
            "the child starts its own window"
        );
        assert_eq!(
            stats_shard(parent)
                .lock()
                .get(&parent)
                .unwrap()
                .syscall_count,
            900,
            "and the parent keeps counting where it was"
        );
        forget(parent);
        forget(child);
        crate::clock::reset_for_test();
    }

    #[test]
    fn an_execve_resets_the_anomaly_window_of_the_process_that_did_it() {
        // Otherwise a benign program can run up to just under a threshold and
        // then exec the payload, which arrives with the counters laundered.
        let _g = crate::test_globals::lock();
        crate::policy::reset_for_test();
        let pid = 7_203;
        let mut st = ProcStat::new(0);
        st.syscall_count = 900;
        stats_shard(pid).lock().insert(pid, st);
        crate::task_exec(pid, "/bin/payload");
        assert_eq!(stats_shard(pid).lock().get(&pid).unwrap().syscall_count, 0);
        forget(pid);
    }

    #[test]
    fn a_process_that_exits_leaves_no_anomaly_state_for_the_next_one() {
        // Pids are recycled, so state left behind is state the next process
        // to get this number inherits.
        let _g = crate::test_globals::lock();
        crate::policy::reset_for_test();
        let pid = 7_204;
        let mut st = ProcStat::new(0);
        st.syscall_count = 900;
        stats_shard(pid).lock().insert(pid, st);
        crate::task_exit(pid);
        assert!(!stats_shard(pid).lock().contains_key(&pid));
    }
}

#[cfg(test)]
mod syscall_path_tests {
    //! `on_syscall` end to end: what reaches the log, what is throttled, what
    //! is denied, and where each alarm's threshold sits. Everything it
    //! touches is process-wide -- the clock, the log, the control plane, the
    //! shards, the system-wide fork window -- so every test takes
    //! [`crate::test_globals::lock`] and resets what it uses.

    use super::*;
    use crate::event_log;
    use crate::test_globals;

    /// A syscall on none of the lists here (`read` on x86_64).
    const ORDINARY: u32 = 0;
    /// Frozen mid-uptime, so a seeded window neither rolls nor looks like boot.
    const NOW: u64 = 5 * WINDOW_NS;
    fn frozen_clock() -> u64 {
        NOW
    }

    fn fresh() {
        crate::policy::reset_for_test();
        crate::clock::reset_for_test();
        crate::clock::set_time_source(frozen_clock);
        set_anomaly_detection(false);
        set_privileged_deny(false);
        SYS_FORK_WINDOW_START.store(NOW, Ordering::Relaxed);
        SYS_FORK_COUNT.store(0, Ordering::Relaxed);
        SYS_FORK_ALERTED.store(false, Ordering::Relaxed);
        event_log::reset_for_test();
    }

    fn done(pid: u64) {
        forget(pid);
        set_anomaly_detection(false);
        set_privileged_deny(false);
        crate::clock::reset_for_test();
    }

    /// Puts `pid` in its shard with a window that is already open at [`NOW`].
    fn seed(pid: u64, f: impl FnOnce(&mut ProcStat)) {
        let mut st = ProcStat::new(NOW);
        f(&mut st);
        stats_shard(pid).lock().insert(pid, st);
    }

    fn counted(pid: u64) -> u32 {
        stats_shard(pid).lock().get(&pid).unwrap().syscall_count
    }

    fn lines_with(needle: &str) -> usize {
        event_log::render()
            .lines()
            .filter(|l| l.contains(needle))
            .count()
    }

    #[test]
    fn a_sensitive_syscall_is_watched_even_with_the_rate_counters_off() {
        // The watch is the always-on half: the rate heuristics are opt-in
        // (`HUNTERANOMALY=1`) because they cost a clock read and a lock per
        // syscall, and the default build must still see a ptrace.
        let _g = test_globals::lock();
        fresh();
        let pid = 7_301;
        assert!(!ANOMALY_ENABLED.load(Ordering::Relaxed));
        assert!(on_syscall(pid, nr::PTRACE));
        assert_eq!(lines_with("sensitive syscall"), 1);
        done(pid);
    }

    #[test]
    fn the_deny_latch_only_bites_the_privileged_class() {
        let _g = test_globals::lock();
        fresh();
        crate::policy::set_anomaly_mode(Mode::Enforce);
        set_privileged_deny(true);
        let pid = 7_302;
        assert!(!on_syscall(pid, nr::PTRACE), "ptrace is in the class");
        assert!(
            on_syscall(pid, nr::REBOOT),
            "a reboot is watched, not in the deny class"
        );
        done(pid);
    }

    #[test]
    fn the_deny_latch_does_nothing_while_the_domain_only_reports() {
        // Enforcement is a property of the domain; the latch only says which
        // calls it may reach.
        let _g = test_globals::lock();
        fresh();
        crate::policy::set_anomaly_mode(Mode::Report);
        set_privileged_deny(true);
        let pid = 7_303;
        assert!(on_syscall(pid, nr::PTRACE));
        done(pid);
    }

    #[test]
    fn the_watch_events_of_one_window_are_capped_and_the_calls_still_count() {
        // The throttle is there so an attacker cannot use hunter's own
        // WATCH events as cheap filler to push a real one out of the ring --
        // but what it drops is the log line, never the count.
        let _g = test_globals::lock();
        fresh();
        set_anomaly_detection(true);
        let pid = 7_304;
        seed(pid, |_| {});
        let calls = WATCH_BUDGET + 4;
        for _ in 0..calls {
            assert!(on_syscall(pid, nr::PTRACE));
        }
        assert_eq!(lines_with("sensitive syscall"), WATCH_BUDGET as usize);
        assert_eq!(counted(pid), calls);
        done(pid);
    }

    #[test]
    fn the_flood_alarm_fires_once_past_its_threshold_and_not_at_it() {
        let _g = test_globals::lock();
        fresh();
        set_anomaly_detection(true);
        let pid = 7_305;
        seed(pid, |st| st.syscall_count = FLOOD_THRESHOLD - 1);
        assert!(on_syscall(pid, ORDINARY));
        assert_eq!(counted(pid), FLOOD_THRESHOLD);
        assert_eq!(
            lines_with("ANOMALY"),
            0,
            "exactly at the threshold is not a flood"
        );
        assert!(on_syscall(pid, ORDINARY));
        assert_eq!(lines_with("syscall flood"), 1);
        assert!(on_syscall(pid, ORDINARY));
        assert_eq!(
            lines_with("ANOMALY"),
            1,
            "one flood is one event, not one per syscall"
        );
        done(pid);
    }

    #[test]
    fn a_flood_is_not_also_reported_as_a_spike_over_its_own_baseline() {
        // Both detectors see the same burst; the absolute one describes it
        // more precisely, so the adaptive one stands down for that window.
        let _g = test_globals::lock();
        fresh();
        set_anomaly_detection(true);
        let pid = 7_306;
        seed(pid, |st| {
            st.syscall_count = FLOOD_THRESHOLD;
            st.ewma_syscalls = 1;
            st.windows_observed = ADAPTIVE_MIN_WINDOWS;
        });
        assert!(on_syscall(pid, ORDINARY));
        assert_eq!(lines_with("syscall flood"), 1);
        assert_eq!(lines_with("adaptive syscall spike"), 0);
        assert_eq!(lines_with("ANOMALY"), 1);
        done(pid);
    }

    #[test]
    fn the_fork_bomb_alarm_fires_once_past_its_threshold_and_not_at_it() {
        let _g = test_globals::lock();
        fresh();
        set_anomaly_detection(true);
        let pid = 7_307;
        seed(pid, |st| st.fork_count = FORKBOMB_THRESHOLD - 1);
        assert!(on_syscall(pid, nr::CLONE));
        assert_eq!(lines_with("ANOMALY"), 0);
        assert!(on_syscall(pid, nr::CLONE));
        assert_eq!(lines_with("possible fork bomb"), 1);
        assert!(on_syscall(pid, nr::CLONE));
        assert_eq!(lines_with("ANOMALY"), 1);
        done(pid);
    }

    #[test]
    fn the_system_wide_storm_counts_the_fork_that_asks_about_it() {
        // The count is read back after this fork is added, so the machine's
        // 2001st fork is the one that trips the alarm, not its 2002nd.
        let _g = test_globals::lock();
        fresh();
        set_anomaly_detection(true);
        let pid = 7_308;
        seed(pid, |_| {});
        SYS_FORK_COUNT.store(SYS_FORKBOMB_THRESHOLD - 1, Ordering::Relaxed);
        assert!(on_syscall(pid, nr::FORK));
        assert_eq!(
            lines_with("system-wide fork storm"),
            0,
            "exactly at the threshold is not a storm"
        );
        assert!(on_syscall(pid, nr::FORK));
        assert_eq!(lines_with("system-wide fork storm"), 1);
        done(pid);
    }

    #[test]
    fn the_system_wide_storm_is_reported_once_per_window() {
        // Every fork of the storm passes the threshold; only the first of
        // them may take the alarm.
        let _g = test_globals::lock();
        fresh();
        set_anomaly_detection(true);
        let pid = 7_309;
        seed(pid, |_| {});
        SYS_FORK_COUNT.store(SYS_FORKBOMB_THRESHOLD, Ordering::Relaxed);
        for _ in 0..3 {
            assert!(on_syscall(pid, nr::FORK));
        }
        assert_eq!(lines_with("system-wide fork storm"), 1);
        done(pid);
    }

    #[test]
    fn an_anomaly_the_kernel_blocks_is_logged_as_critical_and_denies_the_call() {
        // Under `Report` the same burst is a warning that lets the call
        // through; the severity is how an operator tells the two apart.
        let _g = test_globals::lock();
        fresh();
        set_anomaly_detection(true);
        crate::policy::set_anomaly_mode(Mode::Enforce);
        let pid = 7_310;
        seed(pid, |st| st.syscall_count = FLOOD_THRESHOLD);
        let before = event_log::stats().criticals;
        assert!(
            !on_syscall(pid, ORDINARY),
            "Enforce denies the call that trips it"
        );
        assert_eq!(event_log::stats().criticals, before + 1);
        done(pid);
    }

    #[test]
    fn a_syscall_the_detector_denies_is_denied_by_the_hook_too() {
        // `check_syscall` is the kernel's entry point: an IDS verdict that
        // never reaches it is a verdict nobody acts on.
        let _g = test_globals::lock();
        fresh();
        crate::policy::set_syscall_mode(Mode::Off);
        crate::policy::set_anomaly_mode(Mode::Enforce);
        set_privileged_deny(true);
        let pid = 7_311;
        assert_eq!(
            crate::check_syscall(pid, nr::PTRACE, &[0; 6]),
            Err(crate::SecurityViolation::SyscallBlocked)
        );
        assert_eq!(crate::check_syscall(pid, ORDINARY, &[0; 6]), Ok(()));
        done(pid);
    }
}
