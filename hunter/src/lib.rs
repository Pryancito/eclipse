//! # hunter — kernel security subsystem for Eclipse OS
//!
//! `hunter` is a small in-kernel security solution combining an **LSM-style
//! enforcement layer** with a **behavioural intrusion-detection system**. The
//! kernel calls into a handful of well-defined hooks; hunter consults its
//! [`policy`] engine, runs [`heuristics`], and records every decision in a
//! forensic [`event_log`] that userspace can read at `/proc/hunter`.
//!
//! ## Hooks
//!
//! | Hook                  | Kernel call site            | Domain        |
//! |-----------------------|-----------------------------|---------------|
//! | [`check_syscall`]     | syscall dispatch            | seccomp + IDS |
//! | [`check_elf_binary`]  | `execve`                    | exec integrity|
//! | [`check_mmap`]        | `mmap`                      | W^X memory    |
//! | [`check_mprotect`]    | `mprotect`                  | W^X memory    |
//! | [`task_exit`]         | process exit                | cleanup       |
//!
//! Each enforcement domain has an independent [`policy::Mode`] (`Off` /
//! `Report` / `Enforce`) so the subsystem can be rolled out audit-first and
//! tightened to active blocking per-domain.

#![no_std]

extern crate alloc;
#[macro_use]
extern crate log;

pub mod clock;
pub mod event_log;
pub mod heuristics;
pub mod policy;

use alloc::format;
use alloc::string::String;

pub use clock::set_time_source;
pub use event_log::Severity;
pub use policy::Mode;

/// Reasons hunter rejects an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityViolation {
    /// A syscall was not on the calling process's whitelist.
    SyscallBlocked,
    /// An ELF binary failed integrity or path policy checks.
    InvalidBinary,
    /// A memory mapping requested simultaneous write and execute (W^X).
    WriteExecViolation,
}

/// Initializes the hunter security subsystem in the kernel.
///
/// Call once during boot, *after* [`set_time_source`] so the very first event
/// carries a real timestamp.
pub fn init() {
    event_log::record(
        0,
        Severity::Info,
        "SYSTEM",
        "INIT",
        format!(
            "hunter security subsystem v{} initialized (syscall={}, wx={}, exec={})",
            env!("CARGO_PKG_VERSION"),
            policy::syscall_mode().as_str(),
            policy::wx_mode().as_str(),
            policy::exec_mode().as_str(),
        ),
    );
    info!("hunter: security subsystem v{} online", env!("CARGO_PKG_VERSION"));
}

/// Syscall hook. Enforces the per-process whitelist and feeds the anomaly
/// detector. Returns `Err` only when policy is in `Enforce` and the call is
/// denied; the caller should then fail the syscall (e.g. with `EPERM`).
pub fn check_syscall(pid: u64, num: u32, args: &[usize; 6]) -> Result<(), SecurityViolation> {
    let verdict = match policy::is_syscall_allowed(pid, num) {
        Ok(()) => Ok(()),
        Err(enforce) => {
            let desc = format!("unauthorized syscall #{} args={:x?}", num, args);
            if enforce {
                event_log::record(pid, Severity::Critical, "SYSCALL", "BLOCKED", desc.clone());
                warn!("hunter: BLOCKED syscall [pid={}] {}", pid, desc);
                Err(SecurityViolation::SyscallBlocked)
            } else {
                event_log::record(pid, Severity::Warning, "SYSCALL", "WARNING", desc.clone());
                warn!("hunter: syscall violation (report) [pid={}] {}", pid, desc);
                Ok(())
            }
        }
    };
    // Only profile calls that were actually permitted to proceed.
    if verdict.is_ok() {
        heuristics::on_syscall(pid, num);
    }
    verdict
}

/// `execve` hook. Validates ELF integrity (magic) and the executable path
/// policy. Returns `true` if execution may proceed.
///
/// * Invalid ELF magic is **always** rejected (a non-ELF would fail to load
///   anyway, and this catches the attempt for the audit trail).
/// * An untrusted path is rejected only when [`policy::exec_mode`] is
///   `Enforce`; in `Report` it is logged and allowed.
pub fn check_elf_binary(path: &str, elf_data: &[u8]) -> bool {
    // Integrity: a valid ELF must start with the 0x7F 'E' 'L' 'F' magic.
    if elf_data.len() < 4 || &elf_data[0..4] != b"\x7fELF" {
        event_log::record(
            0,
            Severity::Warning,
            "EXEC",
            "BLOCKED",
            format!("invalid ELF magic for {}", path),
        );
        warn!("hunter: rejected non-ELF binary: {}", path);
        return false;
    }

    // Path policy: refuse / flag execution from untrusted, world-writable
    // locations and path-traversal attempts.
    let exec_mode = policy::exec_mode();
    if exec_mode != Mode::Off && policy::is_untrusted_exec_path(path) {
        if exec_mode == Mode::Enforce {
            event_log::record(
                0,
                Severity::Critical,
                "EXEC",
                "BLOCKED",
                format!("blocked exec from untrusted path: {}", path),
            );
            warn!("hunter: BLOCKED exec from untrusted path: {}", path);
            return false;
        }
        event_log::record(
            0,
            Severity::Warning,
            "EXEC",
            "WARNING",
            format!("exec from untrusted path: {}", path),
        );
        warn!("hunter: exec from untrusted path (report): {}", path);
    }

    true
}

/// W^X hook for `mmap`. Returns `true` if the mapping may proceed.
///
/// A simultaneously writable+executable mapping violates write-xor-execute.
/// Blocked only in `Enforce` mode; logged-and-allowed in `Report`.
pub fn check_mmap(pid: u64, prot_write: bool, prot_exec: bool) -> bool {
    wx_check(pid, prot_write, prot_exec, "mmap")
}

/// W^X hook for `mprotect`. Returns `true` if the protection change may proceed.
pub fn check_mprotect(pid: u64, prot_write: bool, prot_exec: bool) -> bool {
    wx_check(pid, prot_write, prot_exec, "mprotect")
}

fn wx_check(pid: u64, prot_write: bool, prot_exec: bool, op: &str) -> bool {
    if !(prot_write && prot_exec) {
        return true;
    }
    match policy::wx_mode() {
        Mode::Off => true,
        Mode::Report => {
            event_log::record(
                pid,
                Severity::Warning,
                "WX",
                "WARNING",
                format!("{}: writable+executable mapping (W^X)", op),
            );
            true
        }
        Mode::Enforce => {
            event_log::record(
                pid,
                Severity::Critical,
                "WX",
                "BLOCKED",
                format!("{}: blocked writable+executable mapping (W^X)", op),
            );
            warn!("hunter: BLOCKED W^X {} [pid={}]", op, pid);
            false
        }
    }
}

/// Process-exit hook. Releases all per-process security state so a recycled
/// pid never inherits a stale policy or anomaly counters.
pub fn task_exit(pid: u64) {
    policy::remove_policy(pid);
    heuristics::forget(pid);
}

/// Renders the full `/proc/hunter` report: a status header followed by the
/// recent event ring.
pub fn render_report() -> String {
    let s = event_log::stats();
    let mut out = String::new();
    out.push_str(&format!(
        "hunter security subsystem v{}\n",
        env!("CARGO_PKG_VERSION")
    ));
    out.push_str(&format!(
        "enforcement: syscall={} wx={} exec={}\n",
        policy::syscall_mode().as_str(),
        policy::wx_mode().as_str(),
        policy::exec_mode().as_str(),
    ));
    out.push_str(&format!(
        "events: total={} blocked={} warnings={} critical={} dropped={}\n",
        s.total, s.blocked, s.warnings, s.criticals, s.dropped
    ));
    out.push_str(&format!(
        "active syscall policies: {}\n",
        policy::active_policy_count()
    ));
    out.push_str("\nrecent events (oldest first):\n");
    out.push_str(&event_log::render());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn test_seccomp_policy() {
        policy::set_syscall_mode(Mode::Enforce);
        // Default permissive when no policy is registered.
        assert_eq!(check_syscall(42, 1, &[0; 6]), Ok(()));

        // Register policy for PID 42: only allow syscall 1 and 2.
        policy::register_policy(42, vec![1, 2]);
        assert_eq!(check_syscall(42, 1, &[0; 6]), Ok(()));
        assert_eq!(check_syscall(42, 2, &[0; 6]), Ok(()));
        assert_eq!(
            check_syscall(42, 3, &[0; 6]),
            Err(SecurityViolation::SyscallBlocked)
        );

        // Report mode warns but allows.
        policy::set_syscall_mode(Mode::Report);
        assert_eq!(check_syscall(42, 3, &[0; 6]), Ok(()));

        // Off disables the domain entirely.
        policy::set_syscall_mode(Mode::Off);
        assert_eq!(check_syscall(42, 3, &[0; 6]), Ok(()));

        // Cleanup releases the policy.
        policy::set_syscall_mode(Mode::Enforce);
        task_exit(42);
        assert_eq!(check_syscall(42, 3, &[0; 6]), Ok(()));
    }

    #[test]
    fn test_elf_validation() {
        policy::set_exec_mode(Mode::Enforce);
        // Safe path and valid ELF magic.
        assert!(check_elf_binary("/bin/init", b"\x7fELFsomething"));
        // Untrusted prefix.
        assert!(!check_elf_binary("/tmp/malicious", b"\x7fELFsomething"));
        // Path traversal.
        assert!(!check_elf_binary("/bin/../tmp/malicious", b"\x7fELF"));
        // Invalid magic is always rejected.
        assert!(!check_elf_binary("/bin/init", b"MZsomething"));

        // Report mode allows untrusted paths (still rejects bad magic).
        policy::set_exec_mode(Mode::Report);
        assert!(check_elf_binary("/tmp/tool", b"\x7fELF...."));
        assert!(!check_elf_binary("/tmp/tool", b"nope"));
    }

    #[test]
    fn test_wx_policy() {
        policy::set_wx_mode(Mode::Enforce);
        assert!(check_mmap(7, true, false)); // W only: fine
        assert!(check_mmap(7, false, true)); // X only: fine
        assert!(!check_mmap(7, true, true)); // W+X: blocked
        assert!(!check_mprotect(7, true, true));

        policy::set_wx_mode(Mode::Report);
        assert!(check_mmap(7, true, true)); // logged but allowed

        policy::set_wx_mode(Mode::Off);
        assert!(check_mmap(7, true, true));
    }
}
