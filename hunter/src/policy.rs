extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use lock::Mutex;

lazy_static::lazy_static! {
    /// Registry of process-specific whitelists
    pub static ref GLOBAL_POLICIES: Mutex<BTreeMap<u64, Vec<u32>>> = Mutex::new(BTreeMap::new());
}

static ENFORCE_MODE: AtomicBool = AtomicBool::new(true); // default to true to block violations

/// Sets whether policy violations should block execution (true) or just warn (false).
pub fn set_enforcement_mode(enabled: bool) {
    ENFORCE_MODE.store(enabled, Ordering::Relaxed);
}

/// Gets whether policy violations should block execution (true) or just warn (false).
pub fn get_enforcement_mode() -> bool {
    ENFORCE_MODE.load(Ordering::Relaxed)
}

/// Registers a whitelist of allowed syscall numbers for a process.
pub fn register_policy(pid: u64, allowed_syscalls: Vec<u32>) {
    let mut policies = GLOBAL_POLICIES.lock();
    policies.insert(pid, allowed_syscalls);
}

/// Removes the security policy for a process (e.g., when it exits).
pub fn remove_policy(pid: u64) {
    let mut policies = GLOBAL_POLICIES.lock();
    policies.remove(&pid);
}

/// Checks if a syscall is allowed for a given process ID.
/// Returns Ok(()) if allowed, or Err(enforce) indicating violation.
pub fn is_syscall_allowed(pid: u64, syscall_num: u32) -> Result<(), bool> {
    let policies = GLOBAL_POLICIES.lock();
    if let Some(allowed) = policies.get(&pid) {
        if allowed.contains(&syscall_num) {
            Ok(())
        } else {
            // Not in whitelist. Err(true) if in enforce mode (block), Err(false) if report mode (warn)
            Err(get_enforcement_mode())
        }
    } else {
        // No policy registered for this PID: default permissive
        Ok(())
    }
}
