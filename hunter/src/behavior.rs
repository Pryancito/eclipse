//! The `behavior` domain: what a process *does*, as opposed to what it *is*.
//!
//! Every other domain judges a program before it runs (exec allowlist, hashes,
//! signatures) or one operation at a time (syscall whitelist, W^X). Neither
//! sees the two attacks this domain exists for, because both are made of
//! operations that are each perfectly ordinary:
//!
//! - **Ransomware** ([`crate::ransomware`]) writes files, renames them and
//!   deletes them -- what every editor, package manager and `tar` does all
//!   day. What gives it away is the *shape*: dozens of files in a few seconds,
//!   their new contents indistinguishable from random bytes, the originals
//!   renamed to one new extension or unlinked. And, more bluntly, touching a
//!   decoy file nobody has any reason to touch (a *canary*).
//! - **Process injection** ([`crate::injection`]) writes into another
//!   process's memory (`process_vm_writev`, `ptrace(POKE*)`) or takes it over
//!   (`ptrace(ATTACH)`). A debugger does that too -- to the program it started.
//!   What gives injection away is the *relationship*: the target is not the
//!   writer's own descendant.
//!
//! This file holds what both share: the domain's [`Mode`], the process
//! lineage (who forked whom, which program each one is), the operator's
//! `exempt` list, and *containment* -- under `Enforce`, a process caught at
//! ransomware loses the right to modify files (and so do the children it
//! forks afterwards) until it exits or the operator `release`s it. Denying
//! only the one operation that crossed the threshold would let it carry on
//! with the next file a moment later.
//!
//! The domain starts in `Report`: an alarm is recorded, nothing is blocked,
//! until the operator has seen what it fires on in their system.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use lock::Mutex;

use crate::event_log::{record, Severity};
use crate::policy::{self, Mode};

/// The domain's boot stance; see the module heading.
const DEFAULT_BEHAVIOR_MODE: Mode = Mode::Report;
static BEHAVIOR_MODE: AtomicU8 = AtomicU8::new(DEFAULT_BEHAVIOR_MODE.to_u8());

/// Processes whose lineage is remembered before the oldest is dropped. A
/// dropped entry only costs an injection check its "is it my child?" answer
/// for that process, never memory without bound under a spawn flood.
const MAX_TASKS: usize = 8192;
/// Denials of a contained process that are logged before the rest are only
/// counted: a contained ransomware keeps trying, and each try is not news.
const CONTAINED_LOG_BUDGET: u32 = 4;
/// Exempt programs the operator may list; a bound so a script cannot grow
/// kernel memory without end.
const MAX_EXEMPT: usize = 256;

/// What hunter remembers of a live process for this domain.
#[derive(Clone, Default)]
struct Task {
    /// The process that forked it (0 when unknown).
    parent: u64,
    /// The program it is running, as `execve` named it.
    exe: Option<String>,
    /// Exempt from both detectors (its program is on the `exempt` list).
    exempt: bool,
    /// Under containment: may not write, rename or unlink files.
    contained: bool,
    /// Denials logged while contained (see [`CONTAINED_LOG_BUDGET`]).
    denials_logged: u32,
    /// The last process this one was reported writing into, so a loop of
    /// `process_vm_writev` into one target is one event and not thousands.
    last_injection_target: u64,
}

lazy_static::lazy_static! {
    static ref TASKS: Mutex<BTreeMap<u64, Task>> = Mutex::new(BTreeMap::new());
    static ref EXEMPT: Mutex<Vec<String>> = Mutex::new(Vec::new());
}

/// How many processes are contained right now. Every file write asks whether
/// its process is one of them; with none contained (the normal case) the
/// answer is this one load and the lineage lock is never taken.
static CONTAINED: AtomicUsize = AtomicUsize::new(0);
/// Alarms raised since boot, for `/proc/hunter`.
static RANSOMWARE_ALERTS: AtomicU64 = AtomicU64::new(0);
static INJECTION_ALERTS: AtomicU64 = AtomicU64::new(0);

/// Sets the domain's mode, through the same latch-checked path as the others.
pub fn set_mode(mode: Mode) {
    policy::apply_mode(&BEHAVIOR_MODE, "behavior", mode);
}

/// The domain's current mode.
pub fn mode() -> Mode {
    Mode::from_u8(BEHAVIOR_MODE.load(Ordering::SeqCst))
}

/// Whether the detectors run at all. The kernel asks this before it does any
/// work of its own for them (resolving a path, downcasting a file).
#[inline]
pub fn watching() -> bool {
    BEHAVIOR_MODE.load(Ordering::Relaxed) != Mode::Off.to_u8()
}

/// Which detector raised an alarm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detector {
    Ransomware,
    Injection,
}

impl Detector {
    fn category(self) -> &'static str {
        match self {
            Detector::Ransomware => "RANSOMWARE",
            Detector::Injection => "INJECTION",
        }
    }
}

/// Raises an alarm for `pid` and answers whether the operation that caused it
/// may proceed.
///
/// An exempt process is never alarmed about. Under `Enforce` the operation is
/// denied, and a ransomware alarm also contains the process (see the module
/// heading); under `Report` it is recorded as a warning and allowed.
pub fn alarm(pid: u64, detector: Detector, what: String) -> bool {
    let enforce = mode() == Mode::Enforce;
    let (exempt, exe) = {
        let tasks = TASKS.lock();
        match tasks.get(&pid) {
            Some(t) => (t.exempt, t.exe.clone()),
            None => (false, None),
        }
    };
    if exempt {
        return true;
    }
    match detector {
        Detector::Ransomware => RANSOMWARE_ALERTS.fetch_add(1, Ordering::Relaxed),
        Detector::Injection => INJECTION_ALERTS.fetch_add(1, Ordering::Relaxed),
    };
    let who = match exe {
        Some(e) => format!("{} ", e),
        None => String::new(),
    };
    if !enforce {
        record(
            pid,
            Severity::Warning,
            detector.category(),
            "WARNING",
            format!("{}{}", who, what),
        );
        return true;
    }
    let contained = detector == Detector::Ransomware && contain(pid);
    record(
        pid,
        Severity::Critical,
        detector.category(),
        "BLOCKED",
        format!(
            "{}{}{}",
            who,
            what,
            if contained {
                "; process contained: no more file changes until it exits"
            } else {
                ""
            }
        ),
    );
    false
}

/// Puts `pid` under containment. Answers whether it was not already.
fn contain(pid: u64) -> bool {
    let mut tasks = TASKS.lock();
    insert_bounded(&mut tasks, pid);
    let t = tasks.get_mut(&pid).expect("just inserted");
    if t.contained {
        return false;
    }
    t.contained = true;
    CONTAINED.fetch_add(1, Ordering::Relaxed);
    true
}

/// Whether `pid` is contained; one atomic load when nobody is.
#[inline]
pub fn is_contained(pid: u64) -> bool {
    if CONTAINED.load(Ordering::Relaxed) == 0 {
        return false;
    }
    TASKS.lock().get(&pid).is_some_and(|t| t.contained)
}

/// Records (within the budget) that a contained process was refused `op` on
/// `path`, and answers `false` so the caller can return it as the verdict.
pub fn deny_contained(pid: u64, op: &str, path: &str) -> bool {
    let log = {
        let mut tasks = TASKS.lock();
        match tasks.get_mut(&pid) {
            Some(t) => {
                t.denials_logged = t.denials_logged.saturating_add(1);
                t.denials_logged <= CONTAINED_LOG_BUDGET
            }
            None => false,
        }
    };
    if log {
        record(
            pid,
            Severity::Critical,
            "RANSOMWARE",
            "BLOCKED",
            format!("contained process refused {} {}", op, path),
        );
    }
    false
}

/// Lifts containment from `pid`. A relaxation, so it is refused once the
/// control plane is sealed. Answers whether the process was contained.
pub fn release(pid: u64) -> bool {
    if policy::is_tighten_only() {
        record(
            0,
            Severity::Warning,
            "CONTROL",
            "WARNING",
            format!(
                "refused relaxing control change: release {} (tighten-only latch)",
                pid
            ),
        );
        return false;
    }
    let was = {
        let mut tasks = TASKS.lock();
        match tasks.get_mut(&pid) {
            Some(t) if t.contained => {
                t.contained = false;
                t.denials_logged = 0;
                CONTAINED.fetch_sub(1, Ordering::Relaxed);
                true
            }
            _ => false,
        }
    };
    record(
        0,
        Severity::Notice,
        "CONTROL",
        "CONFIG",
        format!(
            "release {}: {}",
            pid,
            if was {
                "containment lifted"
            } else {
                "was not contained"
            }
        ),
    );
    was
}

/// Adds a program to the exempt list: neither detector alarms about a process
/// running it. Exempting is a relaxation, so it is refused once sealed. It
/// applies from the program's next `execve` on.
pub fn add_exempt(path: String) {
    if policy::is_tighten_only() {
        record(
            0,
            Severity::Warning,
            "CONTROL",
            "WARNING",
            format!(
                "refused relaxing control change: exempt {} (tighten-only latch)",
                path
            ),
        );
        return;
    }
    let mut list = EXEMPT.lock();
    if list.contains(&path) || list.len() >= MAX_EXEMPT {
        return;
    }
    record(
        0,
        Severity::Notice,
        "CONTROL",
        "CONFIG",
        format!("behavior exempt: {}", path),
    );
    list.push(path);
}

/// Whether `path` is on the exempt list.
fn is_exempt_program(path: &str) -> bool {
    EXEMPT.lock().iter().any(|p| p == path)
}

/// Inserts a default entry for `pid`, dropping the lowest pid first if the
/// table is full. Pids grow, so the lowest is the oldest process; losing it
/// costs only its lineage, as the [`MAX_TASKS`] note says.
fn insert_bounded(tasks: &mut BTreeMap<u64, Task>, pid: u64) {
    if tasks.contains_key(&pid) {
        return;
    }
    if tasks.len() >= MAX_TASKS {
        if let Some(&victim) = tasks.keys().next() {
            if tasks.remove(&victim).is_some_and(|t| t.contained) {
                CONTAINED.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
    tasks.insert(pid, Task::default());
}

/// `fork`/`clone`: the child is the parent's child, runs the parent's
/// program, and inherits its containment -- forking must not be a way out.
pub fn on_fork(parent: u64, child: u64) {
    let mut tasks = TASKS.lock();
    let inherited = tasks.get(&parent).cloned().unwrap_or_default();
    insert_bounded(&mut tasks, child);
    let t = tasks.get_mut(&child).expect("just inserted");
    let was_contained = t.contained;
    *t = Task {
        parent,
        exe: inherited.exe,
        exempt: inherited.exempt,
        contained: inherited.contained,
        denials_logged: 0,
        last_injection_target: 0,
    };
    match (was_contained, t.contained) {
        (false, true) => {
            CONTAINED.fetch_add(1, Ordering::Relaxed);
        }
        (true, false) => {
            CONTAINED.fetch_sub(1, Ordering::Relaxed);
        }
        _ => {}
    }
}

/// `execve`: the process now runs `path`. Containment survives it, so a
/// contained process cannot launder itself by exec'ing a fresh image.
pub fn on_exec(pid: u64, path: &str) {
    let exempt = is_exempt_program(path);
    let mut tasks = TASKS.lock();
    insert_bounded(&mut tasks, pid);
    let t = tasks.get_mut(&pid).expect("just inserted");
    t.exe = Some(String::from(path));
    t.exempt = exempt;
    t.last_injection_target = 0;
}

/// Process exit: drop everything held for it.
pub fn forget(pid: u64) {
    if TASKS.lock().remove(&pid).is_some_and(|t| t.contained) {
        CONTAINED.fetch_sub(1, Ordering::Relaxed);
    }
    crate::ransomware::forget(pid);
}

/// Hops followed up the lineage before giving up; far deeper than any real
/// process tree, and a bound in case a recycled pid ever made a loop.
const MAX_ANCESTRY: usize = 64;

/// Whether `target` is `ancestor` itself or one of its descendants.
pub(crate) fn is_self_or_descendant(ancestor: u64, target: u64) -> bool {
    let tasks = TASKS.lock();
    let mut cur = target;
    for _ in 0..MAX_ANCESTRY {
        if cur == ancestor {
            return true;
        }
        match tasks.get(&cur) {
            Some(t) if t.parent != 0 && t.parent != cur => cur = t.parent,
            _ => return false,
        }
    }
    false
}

/// Whether `pid` runs an exempt program.
pub(crate) fn is_exempt(pid: u64) -> bool {
    TASKS.lock().get(&pid).is_some_and(|t| t.exempt)
}

/// Notes that `pid` is being reported writing into `target`, and answers
/// whether that is news (a different target from last time).
pub(crate) fn note_injection_target(pid: u64, target: u64) -> bool {
    let mut tasks = TASKS.lock();
    insert_bounded(&mut tasks, pid);
    let t = tasks.get_mut(&pid).expect("just inserted");
    let news = t.last_injection_target != target;
    t.last_injection_target = target;
    news
}

/// The `behavior:` line of `/proc/hunter`.
pub fn render_status() -> String {
    format!(
        "behavior: canaries={} exempt={} contained={} ransomware-alerts={} injection-alerts={}\n",
        crate::ransomware::canary_count(),
        EXEMPT.lock().len(),
        CONTAINED.load(Ordering::Relaxed),
        RANSOMWARE_ALERTS.load(Ordering::Relaxed),
        INJECTION_ALERTS.load(Ordering::Relaxed),
    )
}

/// Test-only: the domain back to its boot state.
#[cfg(test)]
pub(crate) fn reset_for_test() {
    BEHAVIOR_MODE.store(DEFAULT_BEHAVIOR_MODE.to_u8(), Ordering::SeqCst);
    TASKS.lock().clear();
    EXEMPT.lock().clear();
    CONTAINED.store(0, Ordering::SeqCst);
    RANSOMWARE_ALERTS.store(0, Ordering::SeqCst);
    INJECTION_ALERTS.store(0, Ordering::SeqCst);
    crate::ransomware::reset_for_test();
}

#[cfg(test)]
pub(crate) fn alerts_for_test() -> (u64, u64) {
    (
        RANSOMWARE_ALERTS.load(Ordering::SeqCst),
        INJECTION_ALERTS.load(Ordering::SeqCst),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_globals;

    fn reset() {
        policy::reset_for_test();
        reset_for_test();
    }

    #[test]
    fn descendants_are_found_up_the_lineage() {
        let _g = test_globals::lock();
        reset();
        on_fork(10, 11);
        on_fork(11, 12);
        on_fork(10, 13);
        assert!(is_self_or_descendant(10, 10));
        assert!(is_self_or_descendant(10, 12));
        assert!(is_self_or_descendant(11, 12));
        assert!(!is_self_or_descendant(11, 13), "a sibling is not a child");
        assert!(!is_self_or_descendant(12, 10), "nor is a parent");
        assert!(!is_self_or_descendant(10, 999), "nor a stranger");
        reset();
    }

    #[test]
    fn containment_survives_fork_and_exec_and_ends_at_exit() {
        let _g = test_globals::lock();
        reset();
        set_mode(Mode::Enforce);
        assert!(!alarm(20, Detector::Ransomware, String::from("test")));
        assert!(is_contained(20));
        on_fork(20, 21);
        assert!(
            is_contained(21),
            "a child forked after the alarm is contained"
        );
        on_exec(20, "/bin/fresh");
        assert!(is_contained(20), "exec does not launder containment");
        forget(20);
        forget(21);
        assert!(!is_contained(20));
        assert_eq!(CONTAINED.load(Ordering::SeqCst), 0);
        reset();
    }

    #[test]
    fn report_mode_records_and_allows() {
        let _g = test_globals::lock();
        reset();
        assert_eq!(mode(), Mode::Report);
        assert!(alarm(30, Detector::Ransomware, String::from("test")));
        assert!(!is_contained(30));
        assert_eq!(alerts_for_test(), (1, 0));
        reset();
    }

    #[test]
    fn an_exempt_program_is_never_alarmed_about() {
        let _g = test_globals::lock();
        reset();
        set_mode(Mode::Enforce);
        add_exempt(String::from("/usr/bin/gdb"));
        on_exec(40, "/usr/bin/gdb");
        assert!(alarm(40, Detector::Injection, String::from("test")));
        assert_eq!(alerts_for_test(), (0, 0));
        reset();
    }

    #[test]
    fn release_and_exempt_are_refused_once_sealed() {
        let _g = test_globals::lock();
        reset();
        set_mode(Mode::Enforce);
        alarm(50, Detector::Ransomware, String::from("test"));
        policy::seal_tighten_only();
        assert!(!release(50));
        assert!(is_contained(50));
        add_exempt(String::from("/usr/bin/evil"));
        assert!(!is_exempt_program("/usr/bin/evil"));
        policy::reset_for_test();
        assert!(release(50));
        assert!(!is_contained(50));
        reset();
    }
}
