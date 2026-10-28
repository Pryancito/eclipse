//! The three pidfd syscalls through the syscall.
//!
//! `pidfd.rs` has three test modules and all of them call a helper:
//! `pidfd_open_flags`, `pidfd_open_target`, `check_pidfd_siginfo`. Eleven
//! tests decide what the siginfo rules are, and **the line that asks them
//! had never run**: the bodies were seven lines at zero.
//!
//! What only a syscall can show here:
//!
//! 1. **That `pidfd_getfd` asks whether the caller may attach at all.**
//!    `__pidfd_fget()` asks `ptrace_may_access(task,
//!    PTRACE_MODE_ATTACH_REALCREDS)` and answers `EPERM`; nothing asked
//!    here, so any process could take any open file out of any other -- its
//!    listening socket, its log, the pipe it is reading. And the question
//!    comes BEFORE the fd is looked up, so a caller with no business in
//!    that process cannot find out which of its descriptors are open
//!    either.
//! 2. **That the caller's `siginfo_t` reaches the check.** The argument
//!    used to be taken and dropped with `let _ = info`, which is the one
//!    shape of bug that looks exactly like correct code: the rules can all
//!    be right and still be asked about nothing.
//! 3. **Which argument each one rejects first.** A flag word nobody has is
//!    `EINVAL` before the descriptor is read, so a bad flag on a
//!    descriptor nobody opened is `EINVAL` and not `EBADF`.
//!
//! Fixed pids live in 49_1xx here.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::signal::SignalCode;
use linux_object::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// `SIGTERM`, and three `si_code`s: `SI_QUEUE`, which `sigqueue(3)` uses and
/// which a sender may claim at anybody, and `SI_USER` and `SI_KERNEL`, which
/// it may not -- Linux refuses every code `>= 0` at another process, because
/// those are the ones the kernel itself fills in.
const SIGTERM: usize = 15;
const SI_QUEUE: i32 = -1;
const SI_USER: i32 = 0;
const SI_KERNEL: i32 = 0x80;

/// An ordinary unprivileged user, as `setresuid` leaves one.
const A_PLAIN_USER: usize = 1000;

/// A descriptor nobody opened.
const A_FD_OF_NOBODY: usize = 90;

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// A whole `siginfo_t` as a sender fills one in: `SigInfo`'s padding field
/// is private, so this goes through the constructor the kernel's own
/// senders use.
fn a_siginfo(signal: Signal, code: i32) -> SigInfo {
    SigInfo::from_user(signal, 0, 0, SignalCode(code))
}

/// Where userspace would say that lives.
fn at<T>(info: &T) -> usize {
    info as *const T as usize
}

#[test]
fn pidfd_open_refuses_a_number_that_is_not_a_pid_at_all() {
    // `pid == 0` is the caller's own process group to `kill`, and nothing to
    // `pidfd_open`; above `INT_MAX` the number cannot be a `pid_t`. Both are
    // decided before any lookup, which is what makes them `EINVAL` and not
    // `ESRCH`.
    let proc = a_process(49_100, "pidfd-open");
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            assert_eq!(
                sys.sys_pidfd_open(0, 0),
                Err(LxError::EINVAL),
                "pid zero is not a process here"
            );
            assert_eq!(
                sys.sys_pidfd_open(i32::MAX as usize + 1, 0),
                Err(LxError::EINVAL),
                "and a number too big for a pid_t is not one either"
            );
            assert_eq!(
                sys.sys_pidfd_open(i32::MAX as usize, 0),
                Err(LxError::ESRCH),
                "the biggest pid there can be is merely nobody's"
            );
            assert!(
                sys.sys_pidfd_open(49_100, 0).is_ok(),
                "and its own pid opens"
            );
        })
    });
}

#[test]
fn the_two_flag_words_are_read_before_the_descriptor() {
    // A flag nobody has, on a descriptor nobody opened: `EINVAL` says the
    // flags were read first and `EBADF` would say they were not.
    let proc = a_process(49_101, "pidfd-flags");
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let nobodys = FileDesc::from(A_FD_OF_NOBODY);
            // Two scopes at once, and a bit nobody has.
            for flags in [3, 8] {
                assert_eq!(
                    sys.sys_pidfd_send_signal(nobodys, SIGTERM, UserInPtr::from(0), flags),
                    Err(LxError::EINVAL),
                    "pidfd_send_signal flags {flags:#x}"
                );
            }
            // One scope is a flag word it has: the descriptor is next.
            assert_eq!(
                sys.sys_pidfd_send_signal(nobodys, SIGTERM, UserInPtr::from(0), 1),
                Err(LxError::EBADF),
                "PIDFD_SIGNAL_THREAD is a flag pidfd_send_signal knows"
            );
            assert_eq!(
                sys.sys_pidfd_getfd(nobodys, 0, 1),
                Err(LxError::EINVAL),
                "and pidfd_getfd takes none either"
            );
            assert_eq!(
                sys.sys_pidfd_getfd(nobodys, -1, 0),
                Err(LxError::EINVAL),
                "a negative descriptor is refused the same way"
            );
            // With the flags right, the descriptor is what is wrong.
            assert_eq!(
                sys.sys_pidfd_send_signal(nobodys, SIGTERM, UserInPtr::from(0), 0),
                Err(LxError::EBADF),
                "and only then is the descriptor read"
            );
            assert_eq!(sys.sys_pidfd_getfd(nobodys, 0, 0), Err(LxError::EBADF));
        })
    });
}

#[test]
fn the_siginfo_the_caller_gave_is_the_one_that_gets_judged() {
    // The eleven tests of `pidfd_siginfo_tests` decide these rules; what
    // this one shows is that the struct arrives. A null pointer is the
    // ordinary call, a struct that disagrees with the signal number is
    // `EINVAL`, and a kernel code aimed at somebody else is `EPERM`.
    let target = a_process(49_102, "pidfd-siginfo-target");
    let proc = a_process(49_103, "pidfd-siginfo");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let _ = &target;
            let fd = FileDesc::from(
                sys.sys_pidfd_open(49_102, 0)
                    .expect("a pidfd on a live process"),
            );

            assert_eq!(
                sys.sys_pidfd_send_signal(fd, 0, UserInPtr::from(0), 0),
                Ok(0),
                "a null siginfo and signal zero is the liveness probe"
            );

            let wrong_number = a_siginfo(Signal::SIGINT, SI_USER);
            assert_eq!(
                sys.sys_pidfd_send_signal(fd, SIGTERM, UserInPtr::from(at(&wrong_number)), 0),
                Err(LxError::EINVAL),
                "a struct that does not describe this very call"
            );

            let forged = a_siginfo(Signal::SIGTERM, SI_KERNEL);
            assert_eq!(
                sys.sys_pidfd_send_signal(fd, SIGTERM, UserInPtr::from(at(&forged)), 0),
                Err(LxError::EPERM),
                "a kernel code cannot be claimed at another process"
            );

            let as_a_kill = a_siginfo(Signal::SIGTERM, SI_USER);
            assert_eq!(
                sys.sys_pidfd_send_signal(fd, SIGTERM, UserInPtr::from(at(&as_a_kill)), 0),
                Err(LxError::EPERM),
                "nor a plain kill's code, which is also one the kernel writes"
            );

            let honest = a_siginfo(Signal::SIGTERM, SI_QUEUE);
            assert_eq!(
                sys.sys_pidfd_send_signal(fd, SIGTERM, UserInPtr::from(at(&honest)), 0),
                Ok(0),
                "and the one code a sender may claim goes through"
            );
        })
    });
}

#[test]
fn pidfd_getfd_of_a_process_that_has_gone_is_esrch() {
    // The fd outlives the process, which is the point of a pidfd; what it
    // cannot do is reach into one that has exited.
    let target = a_process(49_104, "pidfd-gone-target");
    let proc = a_process(49_105, "pidfd-gone");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = FileDesc::from(
                sys.sys_pidfd_open(49_104, 0)
                    .expect("a pidfd on a live process"),
            );
            target.exit(0);
            assert_eq!(
                sys.sys_pidfd_getfd(fd, 0, 0),
                Err(LxError::ESRCH),
                "a process that has exited has no descriptors to take"
            );
        })
    });
}

#[test]
fn pidfd_getfd_is_not_a_way_into_another_users_process() {
    // The hole this closes: a pidfd is not a permission. `pidfd_open(2)`
    // takes any pid and checks nothing, on purpose, because the check
    // belongs to whatever you then do with the fd -- and nothing here did
    // it. The proof that the question comes before the lookup is a
    // descriptor the target does not have open: `EPERM`, not `EBADF`.
    let target = a_process(49_106, "pidfd-perm-target");
    // Its leader, which `pidfd_getfd` takes the descriptor out of.
    let leader = Thread::create_linux(&target).unwrap();
    let proc = a_process(49_107, "pidfd-perm");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = FileDesc::from(
                sys.sys_pidfd_open(49_106, 0)
                    .expect("a pidfd on a live process"),
            );
            // As root the two processes are each other's business, so what
            // is wrong is only the descriptor.
            assert_eq!(
                sys.sys_pidfd_getfd(fd, A_FD_OF_NOBODY as i32, 0),
                Err(LxError::EBADF),
                "root reaches the lookup"
            );

            assert_eq!(
                sys.sys_setresuid(A_PLAIN_USER, A_PLAIN_USER, A_PLAIN_USER),
                Ok(0),
                "dropping to a plain user"
            );
            assert_eq!(
                sys.sys_pidfd_getfd(fd, A_FD_OF_NOBODY as i32, 0),
                Err(LxError::EPERM),
                "and a plain user may not even ask"
            );
            assert_eq!(
                sys.sys_pidfd_getfd(fd, 0, 0),
                Err(LxError::EPERM),
                "not about a descriptor the target really has either"
            );
        })
    });
    target.exit(0);
    leader.terminate_abandoned();
}
