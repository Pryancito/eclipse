#![no_std]

extern crate alloc;
#[macro_use]
extern crate log;

pub mod event_log;
pub mod policy;

use alloc::format;
use alloc::string::String;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityViolation {
    SyscallBlocked,
    InvalidBinary,
}

/// Initializes the hunter security subsystem in the kernel.
pub fn init() {
    event_log::log_event(0, "SYSTEM", String::from("hunter security subsystem v0.1.0 initialized"));
    info!("hunter: security subsystem v0.1.0 initialized in kernel space");
}

/// Dynamic check for system calls. Returns Err if the system call is blocked by policy.
pub fn check_syscall(pid: u64, num: u32, args: &[usize; 6]) -> Result<(), SecurityViolation> {
    match policy::is_syscall_allowed(pid, num) {
        Ok(()) => Ok(()),
        Err(enforce) => {
            let desc = format!("Unauthorized syscall #{} invoked with args: {:x?}", num, args);
            event_log::log_event(pid, if enforce { "BLOCKED" } else { "WARNING" }, desc.clone());
            warn!("SYS_SECURITY violation [pid={}]: {}", pid, desc);
            if enforce {
                Err(SecurityViolation::SyscallBlocked)
            } else {
                Ok(())
            }
        }
    }
}

/// Dynamic check for ELF binary execution paths and header integrity.
pub fn check_elf_binary(path: &str, elf_data: &[u8]) -> bool {
    // Integrity Check: Block execution from potentially untrusted paths
    if path.starts_with("/tmp/") || path.contains("../") {
        event_log::log_event(
            0,
            "ELF_BLOCKED",
            format!("Blocked execution of untrusted binary path: {}", path),
        );
        warn!("SYS_SECURITY: blocked execution of untrusted binary: {}", path);
        return false;
    }

    // Binary Verification: Ensure ELF signature is correct
    if elf_data.len() < 4 || &elf_data[0..4] != b"\x7fELF" {
        event_log::log_event(
            0,
            "ELF_BLOCKED",
            format!("Invalid ELF header signature for binary: {}", path),
        );
        warn!("SYS_SECURITY: invalid ELF magic header for binary: {}", path);
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn test_seccomp_policy() {
        policy::set_enforcement_mode(true);
        // Default permissive when no policy is registered
        assert_eq!(check_syscall(42, 1, &[0; 6]), Ok(()));

        // Register policy for PID 42: only allow syscall 1 and 2
        policy::register_policy(42, vec![1, 2]);

        assert_eq!(check_syscall(42, 1, &[0; 6]), Ok(()));
        assert_eq!(check_syscall(42, 2, &[0; 6]), Ok(()));
        assert_eq!(check_syscall(42, 3, &[0; 6]), Err(SecurityViolation::SyscallBlocked));

        // Test permissive warning-only mode
        policy::set_enforcement_mode(false);
        assert_eq!(check_syscall(42, 3, &[0; 6]), Ok(()));

        policy::remove_policy(42);
        assert_eq!(check_syscall(42, 3, &[0; 6]), Ok(()));
    }

    #[test]
    fn test_elf_validation() {
        // Safe path and valid ELF magic
        assert!(check_elf_binary("/bin/init", b"\x7fELFsomething"));

        // Unsafe path (starts with /tmp/)
        assert!(!check_elf_binary("/tmp/malicious", b"\x7fELFsomething"));

        // Unsafe path (contains path traversal)
        assert!(!check_elf_binary("/bin/../tmp/malicious", b"\x7fELF"));

        // Invalid ELF magic
        assert!(!check_elf_binary("/bin/init", b"MZsomething"));
    }
}
