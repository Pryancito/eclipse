//! Behavioural anomaly detection (the IDS half of hunter).
//!
//! Two complementary signals are produced on the syscall hot path:
//!
//! 1. **Sensitive-syscall watch** — a constant-time classification of each
//!    syscall number against a small table of security-relevant operations
//!    (module loading, `ptrace`, privilege changes, namespace escapes, …).
//!    Matches are logged as audit events; they are never blocked here.
//!
//! 2. **Rate anomalies** — cheap per-process sliding-window counters that flag
//!    syscall *floods* and *fork bombs*. Each anomaly is reported at most once
//!    per window to avoid log storms.
//!
//! The syscall-number table is the Linux x86_64 ABI. On other architectures the
//! numbers differ, so the worst case is a spurious-but-harmless audit note; the
//! rate heuristics are architecture-independent.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use core::sync::atomic::{AtomicBool, Ordering};
use lock::Mutex;

use crate::clock;
use crate::event_log::{record, Severity};

/// Master switch for the (locked) rate heuristics. The sensitive-syscall watch
/// is always cheap and stays on regardless.
static ANOMALY_ENABLED: AtomicBool = AtomicBool::new(true);

/// Sliding-window length for the rate heuristics.
const WINDOW_NS: u64 = 1_000_000_000; // 1 second
/// Per-process syscalls/window above which we suspect a denial-of-service flood.
const FLOOD_THRESHOLD: u32 = 50_000;
/// Per-process clone/fork calls/window above which we suspect a fork bomb.
const FORKBOMB_THRESHOLD: u32 = 200;

// --- Linux x86_64 syscall numbers we treat as security-sensitive -----------
const SYS_PTRACE: u32 = 101;
const SYS_SETUID: u32 = 105;
const SYS_SETGID: u32 = 106;
const SYS_SETREUID: u32 = 113;
const SYS_SETRESUID: u32 = 117;
const SYS_CHROOT: u32 = 161;
const SYS_MOUNT: u32 = 165;
const SYS_PIVOT_ROOT: u32 = 155;
const SYS_REBOOT: u32 = 169;
const SYS_INIT_MODULE: u32 = 175;
const SYS_DELETE_MODULE: u32 = 176;
const SYS_KEXEC_LOAD: u32 = 246;
const SYS_KEYCTL: u32 = 250;
const SYS_UNSHARE: u32 = 272;
const SYS_PROCESS_VM_WRITEV: u32 = 311;
const SYS_FINIT_MODULE: u32 = 313;
const SYS_KEXEC_FILE_LOAD: u32 = 320;
const SYS_BPF: u32 = 321;
const SYS_SETNS: u32 = 308;

const SYS_CLONE: u32 = 56;
const SYS_FORK: u32 = 57;
const SYS_VFORK: u32 = 58;

/// Returns `(category, severity, name)` for security-sensitive syscalls.
fn classify(num: u32) -> Option<(&'static str, Severity, &'static str)> {
    let (cat, sev, name) = match num {
        SYS_INIT_MODULE | SYS_FINIT_MODULE => ("MODULE", Severity::Warning, "kernel module load"),
        SYS_DELETE_MODULE => ("MODULE", Severity::Warning, "kernel module unload"),
        SYS_KEXEC_LOAD | SYS_KEXEC_FILE_LOAD => ("MODULE", Severity::Warning, "kexec load"),
        SYS_BPF => ("PRIVILEGE", Severity::Notice, "bpf"),
        SYS_PTRACE => ("PRIVILEGE", Severity::Notice, "ptrace"),
        SYS_SETUID | SYS_SETGID | SYS_SETREUID | SYS_SETRESUID => {
            ("PRIVILEGE", Severity::Notice, "credential change")
        }
        SYS_MOUNT | SYS_PIVOT_ROOT | SYS_CHROOT => ("PRIVILEGE", Severity::Notice, "fs namespace"),
        SYS_UNSHARE | SYS_SETNS => ("PRIVILEGE", Severity::Notice, "namespace change"),
        SYS_PROCESS_VM_WRITEV => ("PRIVILEGE", Severity::Notice, "cross-process write"),
        SYS_KEYCTL => ("PRIVILEGE", Severity::Notice, "keyring"),
        SYS_REBOOT => ("PRIVILEGE", Severity::Notice, "reboot"),
        _ => return None,
    };
    Some((cat, sev, name))
}

/// Per-process sliding-window state for the rate heuristics.
struct ProcStat {
    window_start: u64,
    syscall_count: u32,
    fork_count: u32,
    flood_alerted: bool,
    fork_alerted: bool,
}

impl ProcStat {
    fn new(now: u64) -> Self {
        Self {
            window_start: now,
            syscall_count: 0,
            fork_count: 0,
            flood_alerted: false,
            fork_alerted: false,
        }
    }
    /// Resets the window if `now` has advanced past it.
    fn roll(&mut self, now: u64) {
        if now.saturating_sub(self.window_start) >= WINDOW_NS {
            self.window_start = now;
            self.syscall_count = 0;
            self.fork_count = 0;
            self.flood_alerted = false;
            self.fork_alerted = false;
        }
    }
}

lazy_static::lazy_static! {
    static ref PROC_STATS: Mutex<BTreeMap<u64, ProcStat>> = Mutex::new(BTreeMap::new());
}

/// Enables or disables the per-process rate heuristics.
pub fn set_anomaly_detection(enabled: bool) {
    ANOMALY_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Inspects one syscall for anomalies. Called on the syscall hot path *after*
/// the policy check, so it only ever observes calls that were allowed to run.
pub fn on_syscall(pid: u64, num: u32) {
    // (1) Constant-time sensitive-syscall classification — no lock, no alloc
    //     unless it actually matches something noteworthy.
    if let Some((category, severity, name)) = classify(num) {
        record(
            pid,
            severity,
            category,
            "WATCH",
            format!("sensitive syscall #{} ({})", num, name),
        );
    }

    // (2) Rate heuristics behind the master switch.
    if !ANOMALY_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let now = clock::now_ns();
    let is_fork = matches!(num, SYS_CLONE | SYS_FORK | SYS_VFORK);

    let mut alert_flood = false;
    let mut alert_fork = false;
    {
        let mut stats = PROC_STATS.lock();
        let st = stats.entry(pid).or_insert_with(|| ProcStat::new(now));
        st.roll(now);
        st.syscall_count = st.syscall_count.saturating_add(1);
        if is_fork {
            st.fork_count = st.fork_count.saturating_add(1);
        }
        if !st.flood_alerted && st.syscall_count > FLOOD_THRESHOLD {
            st.flood_alerted = true;
            alert_flood = true;
        }
        if !st.fork_alerted && st.fork_count > FORKBOMB_THRESHOLD {
            st.fork_alerted = true;
            alert_fork = true;
        }
    }

    if alert_flood {
        record(
            pid,
            Severity::Warning,
            "ANOMALY",
            "WARNING",
            format!(
                "syscall flood: >{} syscalls within {}ms",
                FLOOD_THRESHOLD,
                WINDOW_NS / 1_000_000
            ),
        );
    }
    if alert_fork {
        record(
            pid,
            Severity::Warning,
            "ANOMALY",
            "WARNING",
            format!(
                "possible fork bomb: >{} clone/fork within {}ms",
                FORKBOMB_THRESHOLD,
                WINDOW_NS / 1_000_000
            ),
        );
    }
}

/// Drops per-process heuristic state when a process exits.
pub fn forget(pid: u64) {
    PROC_STATS.lock().remove(&pid);
}
