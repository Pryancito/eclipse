//! Policy engine for the hunter security subsystem.
//!
//! Holds the independently-tunable enforcement domains plus the data that
//! drives them:
//!
//! * **syscall** — per-process syscall whitelists (a lightweight seccomp).
//! * **wx**      — write-xor-execute memory policy (mmap / mprotect).
//! * **exec**    — which filesystem paths a binary may be executed from.
//! * **anomaly** — whether detected floods / fork bombs are blocked or only logged.
//!
//! Each domain has its own [`Mode`] (`Off` / `Report` / `Enforce`) so the
//! subsystem can be rolled out audit-first and tightened per-domain.
//!
//! Hardening (P13): every mutator now records an audit event of the
//! `old -> new` transition, mode loads/stores use `SeqCst` (these gate
//! enforcement; a racy read must not silently drop a check), and the control
//! plane can be put into a one-way **tighten-only** mode so a single relaxing
//! store cannot quietly neutralise hunter after boot.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::{format, vec};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use lock::Mutex;

use crate::event_log::{record, Severity};

lazy_static::lazy_static! {
    /// Registry of process-specific syscall whitelists.
    pub static ref GLOBAL_POLICIES: Mutex<BTreeMap<u64, Vec<u32>>> = Mutex::new(BTreeMap::new());

    /// Path prefixes a binary must never be executed from (untrusted, writable
    /// world locations). Configurable at runtime.
    pub static ref UNTRUSTED_EXEC_PREFIXES: Mutex<Vec<String>> =
        Mutex::new(default_untrusted_prefixes());

    /// Optional default whitelist applied to new images at exec time. `None`
    /// keeps the seccomp domain opt-in (default-permissive), preserving boot.
    pub static ref DEFAULT_WHITELIST: Mutex<Option<Vec<u32>>> = Mutex::new(None);
}

/// What hunter does when a policy in a given domain is violated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Ignore the domain entirely (no checks, no logging).
    Off,
    /// Log the violation but allow the action (audit / IDS mode).
    Report,
    /// Log the violation and block the action (active enforcement).
    Enforce,
}

impl Mode {
    fn to_u8(self) -> u8 {
        match self {
            Mode::Off => 0,
            Mode::Report => 1,
            Mode::Enforce => 2,
        }
    }
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Mode::Off,
            1 => Mode::Report,
            _ => Mode::Enforce,
        }
    }
    /// Human-readable tag for the `/proc/hunter` header.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Report => "report",
            Mode::Enforce => "enforce",
        }
    }
}

// Default stance: enforce explicit syscall whitelists (opt-in per process, so
// safe), but only *report* W^X / exec-path / anomaly violations so real
// dynamic linkers, JITs and the base system are never broken by default.
static SYSCALL_MODE: AtomicU8 = AtomicU8::new(2); // Enforce
static WX_MODE: AtomicU8 = AtomicU8::new(1); // Report
static EXEC_MODE: AtomicU8 = AtomicU8::new(1); // Report
static ANOMALY_MODE: AtomicU8 = AtomicU8::new(1); // Report

/// One-way latch: once set, modes may only move towards stricter enforcement.
static TIGHTEN_ONLY: AtomicBool = AtomicBool::new(false);

fn default_untrusted_prefixes() -> Vec<String> {
    vec![
        String::from("/tmp/"),
        String::from("/var/tmp/"),
        String::from("/dev/shm/"),
    ]
}

/// Applies a mode transition for one domain, honouring the tighten-only latch
/// and recording an audit event. Returns the mode actually in effect after.
fn apply_mode(slot: &AtomicU8, domain: &'static str, requested: Mode) -> Mode {
    let current = Mode::from_u8(slot.load(Ordering::SeqCst));
    if current == requested {
        return current;
    }
    // Under the tighten-only latch, refuse any relaxation.
    if TIGHTEN_ONLY.load(Ordering::SeqCst) && requested.to_u8() < current.to_u8() {
        record(
            0,
            Severity::Warning,
            "CONTROL",
            "WARNING",
            format!(
                "refused relaxing {} mode {} -> {} (tighten-only latch)",
                domain,
                current.as_str(),
                requested.as_str()
            ),
        );
        return current;
    }
    slot.store(requested.to_u8(), Ordering::SeqCst);
    record(
        0,
        Severity::Notice,
        "CONTROL",
        "CONFIG",
        format!(
            "{} mode {} -> {}",
            domain,
            current.as_str(),
            requested.as_str()
        ),
    );
    requested
}

/// Sets the enforcement mode for the syscall-filtering domain.
pub fn set_syscall_mode(mode: Mode) {
    apply_mode(&SYSCALL_MODE, "syscall", mode);
}
/// Returns the current syscall-filtering mode.
pub fn syscall_mode() -> Mode {
    Mode::from_u8(SYSCALL_MODE.load(Ordering::SeqCst))
}

/// Sets the enforcement mode for the W^X memory domain.
pub fn set_wx_mode(mode: Mode) {
    apply_mode(&WX_MODE, "wx", mode);
}
/// Returns the current W^X mode.
pub fn wx_mode() -> Mode {
    Mode::from_u8(WX_MODE.load(Ordering::SeqCst))
}

/// Sets the enforcement mode for the executable-path domain.
pub fn set_exec_mode(mode: Mode) {
    apply_mode(&EXEC_MODE, "exec", mode);
}
/// Returns the current executable-path mode.
pub fn exec_mode() -> Mode {
    Mode::from_u8(EXEC_MODE.load(Ordering::SeqCst))
}

/// Sets the enforcement mode for the anomaly (flood / fork-bomb) domain.
pub fn set_anomaly_mode(mode: Mode) {
    apply_mode(&ANOMALY_MODE, "anomaly", mode);
}
/// Returns the current anomaly mode.
pub fn anomaly_mode() -> Mode {
    Mode::from_u8(ANOMALY_MODE.load(Ordering::SeqCst))
}

/// Engages the one-way tighten-only latch: after this, no domain can be
/// relaxed (only moved towards `Enforce`). Typically called once boot is done.
pub fn seal_tighten_only() {
    if !TIGHTEN_ONLY.swap(true, Ordering::SeqCst) {
        record(
            0,
            Severity::Notice,
            "CONTROL",
            "CONFIG",
            String::from("tighten-only latch engaged"),
        );
    }
}
/// Whether the tighten-only latch is engaged.
pub fn is_tighten_only() -> bool {
    TIGHTEN_ONLY.load(Ordering::SeqCst)
}

// ---- Back-compat shims for the original boolean enforcement switch --------

/// Sets whether syscall violations block (`true`) or warn (`false`).
pub fn set_enforcement_mode(enabled: bool) {
    set_syscall_mode(if enabled { Mode::Enforce } else { Mode::Report });
}
/// Returns `true` when syscall violations are blocked.
pub fn get_enforcement_mode() -> bool {
    syscall_mode() == Mode::Enforce
}

// ---- Syscall whitelists ---------------------------------------------------

/// Registers a whitelist of allowed syscall numbers for a process.
pub fn register_policy(pid: u64, allowed_syscalls: Vec<u32>) {
    GLOBAL_POLICIES.lock().insert(pid, allowed_syscalls);
}

/// Removes the security policy for a process (e.g. when it exits).
pub fn remove_policy(pid: u64) {
    GLOBAL_POLICIES.lock().remove(&pid);
}

/// Number of processes that currently have a syscall whitelist registered.
pub fn active_policy_count() -> usize {
    GLOBAL_POLICIES.lock().len()
}

/// Sets (or clears) the default whitelist applied to freshly-exec'd images.
/// `None` keeps the syscall domain opt-in / default-permissive.
pub fn set_default_whitelist(list: Option<Vec<u32>>) {
    *DEFAULT_WHITELIST.lock() = list;
}

/// Applies the default whitelist to `pid` if one is configured (P4): makes the
/// seccomp domain reachable without changing behaviour when unset.
pub fn apply_default_policy(pid: u64) {
    if let Some(list) = DEFAULT_WHITELIST.lock().as_ref() {
        GLOBAL_POLICIES.lock().insert(pid, list.clone());
    }
}

/// Inherits the parent's whitelist into a forked child so a process cannot
/// shed its policy merely by forking (P4).
pub fn inherit_policy(parent_pid: u64, child_pid: u64) {
    let mut map = GLOBAL_POLICIES.lock();
    if let Some(list) = map.get(&parent_pid).cloned() {
        map.insert(child_pid, list);
    }
}

/// Checks whether a syscall is allowed for a given process.
///
/// Returns `Ok(())` when allowed, or `Err(enforce)` on a violation where
/// `enforce` is `true` if the action should be blocked.
pub fn is_syscall_allowed(pid: u64, syscall_num: u32) -> Result<(), bool> {
    let mode = syscall_mode();
    if mode == Mode::Off {
        return Ok(());
    }
    let policies = GLOBAL_POLICIES.lock();
    match policies.get(&pid) {
        Some(allowed) if allowed.contains(&syscall_num) => Ok(()),
        Some(_) => Err(mode == Mode::Enforce),
        // No policy registered for this pid: default permissive.
        None => Ok(()),
    }
}

// ---- Executable-path policy ----------------------------------------------

/// Adds a path prefix to the untrusted-execution list (idempotent).
pub fn add_untrusted_exec_prefix(prefix: String) {
    let mut list = UNTRUSTED_EXEC_PREFIXES.lock();
    if !list.iter().any(|p| *p == prefix) {
        list.push(prefix);
    }
}

/// Returns `true` when executing from `path` should be treated as untrusted:
/// a world-writable location, a path-traversal attempt, or a `/proc/*/fd/*`
/// magic-link that smuggles past prefix matching (P6, finding ELF-2/ELF-3).
pub fn is_untrusted_exec_path(path: &str) -> bool {
    if path.contains("../") || path.contains("/./") {
        return true;
    }
    // /proc/self/fd/N and /proc/<pid>/fd/N resolve to an arbitrary opened
    // inode, defeating textual prefix checks — always treat as untrusted.
    if path.starts_with("/proc/") && path.contains("/fd/") {
        return true;
    }
    let list = UNTRUSTED_EXEC_PREFIXES.lock();
    list.iter().any(|p| path.starts_with(p.as_str()))
}
