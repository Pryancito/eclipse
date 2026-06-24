//! Policy engine for the hunter security subsystem.
//!
//! Holds three independently-tunable enforcement domains plus the data that
//! drives them:
//!
//! * **syscall** — per-process syscall whitelists (a lightweight seccomp).
//! * **wx**      — write-xor-execute memory policy (mmap / mprotect).
//! * **exec**    — which filesystem paths a binary may be executed from.
//!
//! Each domain has its own [`Mode`] so an operator can run, say, syscall
//! filtering in `Enforce` while keeping W^X in `Report` (audit-only) — the
//! classic "monitor first, enforce later" rollout.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, Ordering};
use lock::Mutex;

lazy_static::lazy_static! {
    /// Registry of process-specific syscall whitelists.
    pub static ref GLOBAL_POLICIES: Mutex<BTreeMap<u64, Vec<u32>>> = Mutex::new(BTreeMap::new());

    /// Path prefixes a binary must never be executed from (untrusted, writable
    /// world locations). Configurable at runtime.
    pub static ref UNTRUSTED_EXEC_PREFIXES: Mutex<Vec<alloc::string::String>> =
        Mutex::new(default_untrusted_prefixes());
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

// Default stance: enforce explicit syscall whitelists (they are opt-in per
// process, so this is safe), but only *report* W^X and exec-path violations —
// real dynamic linkers and JITs transiently create W+X mappings and the base
// system legitimately execs from a variety of paths, so blocking by default
// would risk breaking userspace. Operators opt into enforcement.
static SYSCALL_MODE: AtomicU8 = AtomicU8::new(2); // Enforce
static WX_MODE: AtomicU8 = AtomicU8::new(1); // Report
static EXEC_MODE: AtomicU8 = AtomicU8::new(1); // Report

fn default_untrusted_prefixes() -> Vec<alloc::string::String> {
    vec![
        alloc::string::String::from("/tmp/"),
        alloc::string::String::from("/var/tmp/"),
        alloc::string::String::from("/dev/shm/"),
    ]
}

/// Sets the enforcement mode for the syscall-filtering domain.
pub fn set_syscall_mode(mode: Mode) {
    SYSCALL_MODE.store(mode.to_u8(), Ordering::Relaxed);
}
/// Returns the current syscall-filtering mode.
pub fn syscall_mode() -> Mode {
    Mode::from_u8(SYSCALL_MODE.load(Ordering::Relaxed))
}

/// Sets the enforcement mode for the W^X memory domain.
pub fn set_wx_mode(mode: Mode) {
    WX_MODE.store(mode.to_u8(), Ordering::Relaxed);
}
/// Returns the current W^X mode.
pub fn wx_mode() -> Mode {
    Mode::from_u8(WX_MODE.load(Ordering::Relaxed))
}

/// Sets the enforcement mode for the executable-path domain.
pub fn set_exec_mode(mode: Mode) {
    EXEC_MODE.store(mode.to_u8(), Ordering::Relaxed);
}
/// Returns the current executable-path mode.
pub fn exec_mode() -> Mode {
    Mode::from_u8(EXEC_MODE.load(Ordering::Relaxed))
}

// ---- Back-compat shims for the original boolean enforcement switch --------

/// Sets whether syscall violations block (`true`, Enforce) or warn (`false`,
/// Report). Retained for backward compatibility; prefer [`set_syscall_mode`].
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

/// Checks whether a syscall is allowed for a given process.
///
/// Returns `Ok(())` when allowed (either explicitly whitelisted, or no policy
/// is registered for the pid), or `Err(enforce)` on a violation where
/// `enforce` is `true` if the action should be blocked and `false` if it
/// should merely be reported.
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
pub fn add_untrusted_exec_prefix(prefix: alloc::string::String) {
    let mut list = UNTRUSTED_EXEC_PREFIXES.lock();
    if !list.iter().any(|p| *p == prefix) {
        list.push(prefix);
    }
}

/// Returns `true` when executing from `path` should be treated as untrusted
/// (world-writable location or a path-traversal attempt).
pub fn is_untrusted_exec_path(path: &str) -> bool {
    if path.contains("../") {
        return true;
    }
    let list = UNTRUSTED_EXEC_PREFIXES.lock();
    list.iter().any(|p| path.starts_with(p.as_str()))
}
