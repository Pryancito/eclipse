//! Process-injection detection (part of the [`behavior`] domain).
//!
//! Injecting code into another process -- to hide in it, to borrow its
//! privileges or its network access, to steal what it holds -- comes down to
//! writing into memory that is not yours, or taking over the thread that runs
//! it. On Linux that is `process_vm_writev(2)`, `ptrace(2)` with a `POKE*` or
//! `SET*REGS` request, or `ptrace(ATTACH/SEIZE)` to get there, and the writes
//! to `/proc/<pid>/mem` that work through the same access check.
//!
//! The sensitive-syscall watch in [`crate::heuristics`] already logs every
//! one of these as a `NOTICE`, because it cannot tell a debugger from an
//! implant. The difference is who the target is: `gdb ./prog` and
//! `strace -f make` trace processes they started, an implant writes into a
//! process it did not. So this module raises an alarm only when the target is
//! neither the writer itself nor one of its descendants (from the lineage
//! [`behavior`] keeps from `fork`), and the writer's program is not on the
//! operator's `exempt` list (`exempt /usr/bin/gdb` for `gdb -p`).
//!
//! Hooks:
//! - `ptrace` is judged from the raw syscall arguments in
//!   [`on_syscall`], before the kernel looks at them. Eclipse does not
//!   implement `ptrace` yet (it answers `ENOSYS`), but an injector trying it
//!   is worth the alarm all the same, and the hook is ready when it does.
//! - `process_vm_writev` is judged by the kernel *after* it has resolved the
//!   target (a thread id names its process), through [`check_process_write`].
//!   The same hook is meant for a writer of `/proc/<pid>/mem`, which Eclipse
//!   does not have either.
//!
//! [`behavior`]: crate::behavior

extern crate alloc;

use alloc::format;

use crate::behavior::{self, Detector};
use crate::heuristics::nr;

/// `ptrace` requests that write into, or take control of, the target. The
/// numbers are the same on every architecture Eclipse builds for (aarch64
/// has no `SETREGS`/`SETFPREGS`, and nothing sends them there).
mod req {
    pub const POKETEXT: usize = 4;
    pub const POKEDATA: usize = 5;
    pub const POKEUSER: usize = 6;
    pub const SETREGS: usize = 13;
    pub const SETFPREGS: usize = 15;
    pub const ATTACH: usize = 16;
    pub const SETREGSET: usize = 0x4205;
    pub const SEIZE: usize = 0x4206;
}

/// What a `ptrace` request does to its target, if it is one this module
/// cares about.
fn ptrace_effect(request: usize) -> Option<&'static str> {
    match request {
        req::POKETEXT | req::POKEDATA | req::POKEUSER => Some("ptrace write into"),
        req::SETREGS | req::SETFPREGS | req::SETREGSET => Some("ptrace register write into"),
        req::ATTACH | req::SEIZE => Some("ptrace attach to"),
        _ => None,
    }
}

/// Syscall hook, called for every permitted syscall after the policy check.
/// Answers whether the call may proceed. Only `ptrace` is looked at, and with
/// the domain off not even that.
#[inline]
pub fn on_syscall(pid: u64, num: u32, args: &[usize; 6]) -> bool {
    if nr::PTRACE == nr::ABSENT || num != nr::PTRACE || !behavior::watching() {
        return true;
    }
    match ptrace_effect(args[0]) {
        Some(effect) => check_process_write(pid, args[1] as u64, effect),
        None => true,
    }
}

/// `pid` is about to write into (or take over) process `target` by `how`
/// (`"process_vm_writev into"`, ...). Answers whether it may.
pub fn check_process_write(pid: u64, target: u64, how: &str) -> bool {
    if !behavior::watching() || behavior::is_self_or_descendant(pid, target) {
        return true;
    }
    // One event per target in a row: an injector loops on its target, and the
    // denial under Enforce does not depend on the event being logged.
    if !behavior::note_injection_target(pid, target) {
        // Already reported: the verdict is the one it was then.
        return behavior::mode() != crate::policy::Mode::Enforce || behavior::is_exempt(pid);
    }
    behavior::alarm(
        pid,
        Detector::Injection,
        format!(
            "possible process injection: {} pid {}, which is not its own descendant",
            how, target
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{self, Mode};
    use crate::test_globals;

    fn reset() {
        policy::reset_for_test();
        behavior::reset_for_test();
    }

    fn ptrace(pid: u64, request: usize, target: u64) -> bool {
        on_syscall(pid, nr::PTRACE, &[request, target as usize, 0, 0, 0, 0])
    }

    #[test]
    fn a_debugger_may_write_into_what_it_started() {
        let _g = test_globals::lock();
        reset();
        behavior::set_mode(Mode::Enforce);
        behavior::on_fork(100, 101);
        behavior::on_fork(101, 102);
        assert!(check_process_write(100, 101, "process_vm_writev into"));
        assert!(check_process_write(100, 102, "process_vm_writev into"));
        assert!(check_process_write(100, 100, "process_vm_writev into"));
        if nr::PTRACE != nr::ABSENT {
            assert!(ptrace(100, req::POKEDATA, 102));
        }
        assert_eq!(behavior::alerts_for_test(), (0, 0));
        reset();
    }

    #[test]
    fn writing_into_a_stranger_is_injection() {
        let _g = test_globals::lock();
        reset();
        behavior::on_fork(1, 200);
        behavior::on_fork(1, 300);
        // Report: allowed, and one event however often it loops.
        for _ in 0..50 {
            assert!(check_process_write(300, 200, "process_vm_writev into"));
        }
        assert_eq!(behavior::alerts_for_test(), (0, 1));
        // Nor is a parent fair game for its child.
        assert!(check_process_write(200, 1, "process_vm_writev into"));
        assert_eq!(behavior::alerts_for_test(), (0, 2));

        behavior::set_mode(Mode::Enforce);
        assert!(!check_process_write(300, 200, "process_vm_writev into"));
        assert!(!check_process_write(300, 200, "process_vm_writev into"));
        if nr::PTRACE != nr::ABSENT {
            assert!(!ptrace(300, req::ATTACH, 200));
            assert!(!ptrace(300, req::SETREGS, 200));
            // Reading, and asking to be traced, are not injection.
            assert!(ptrace(300, 2 /* PEEKDATA */, 200));
            assert!(ptrace(300, 0 /* TRACEME */, 0));
        }
        reset();
    }

    #[test]
    fn exempt_tracers_and_the_off_switch() {
        let _g = test_globals::lock();
        reset();
        behavior::set_mode(Mode::Enforce);
        behavior::add_exempt(alloc::string::String::from("/usr/bin/gdb"));
        behavior::on_exec(400, "/usr/bin/gdb");
        assert!(check_process_write(400, 500, "ptrace attach to"));
        behavior::set_mode(Mode::Off);
        assert!(check_process_write(401, 500, "ptrace attach to"));
        assert_eq!(behavior::alerts_for_test(), (0, 0));
        reset();
    }
}
