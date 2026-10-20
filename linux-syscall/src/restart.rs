//! Which syscalls a `SA_RESTART` handler makes again.
//!
//! A blocking syscall a signal cuts short answers `EINTR` here, every one of
//! them. Linux answers four different things, and only one of them reaches
//! userspace as `EINTR` when the handler was installed with `SA_RESTART`:
//!
//! - `-ERESTARTSYS`: the wait for data, for a child, for a lock, for a
//!   connection, and a `futex` wait with no timeout (`sem_wait`). With
//!   `SA_RESTART` the kernel winds the program counter back onto the syscall
//!   instruction before it builds the signal frame, so when the handler
//!   returns the same call is made again and the program never sees the
//!   signal. Without it, `EINTR`.
//! - `-ERESTARTNOHAND`: `poll`, `select`, `epoll_wait`, `pause`,
//!   `rt_sigsuspend`. `EINTR` whenever a handler ran, `SA_RESTART` or not.
//! - `-ERESTART_RESTARTBLOCK`: `nanosleep`, `clock_nanosleep`, a `futex` wait
//!   with a timeout. Also `EINTR` when a handler ran.
//! - `-ERESTARTNOINTR`: `FUTEX_LOCK_PI`. Made again whatever the handler:
//!   [`restarts_whatever_the_handler`].
//! - plain `-EINTR`: never restarted at all.
//!
//! `SA_RESTART` was stored and read by nobody, so every handler behaved as if
//! it had been installed without it: a `read` of a pipe or a terminal, a
//! `waitpid`, a `flock`, an `accept` came back `EINTR` the moment a SIGCHLD
//! or SIGWINCH handler ran. That is the error BSD-style `signal()` -- glibc's
//! and musl's both install with `SA_RESTART` -- exists to make impossible, and
//! a great deal of code reads it as fatal: a shell's `read` builtin, `fgets`
//! on a terminal (stdio sets the error flag and the loop ends), any program
//! that `waitpid`s once without a loop.
//!
//! The list is the `-ERESTARTSYS` set for the calls that wait here, and it is
//! an allow-list on purpose: restarting a call Linux answers `EINTR` turns a
//! timeout into one that starts over (`poll`), or a `sigsuspend` into one that
//! never returns.
//!
//! A job-control STOP is the other way a syscall is cut short, and it runs no
//! handler at all. With no handler Linux makes every one of the first three
//! again, so a program stopped with Ctrl-Z inside `read`, `pause` or
//! `waitpid` and continued with `fg` never knows: [`restarts_after_stop`]
//! says which of them can be made again here, which is the ones whose
//! arguments still say the same thing the second time.

use super::Sys;
/// The `fcntl` commands that wait for a lock: the only ones that can be cut
/// short, and `-ERESTARTSYS` when they are (`fcntl_setlk`).
///
/// From `sys_fcntl`, which is what decides whether the command waits, so
/// that there is one number and not two that can drift.
use crate::file::{F_OFD_SETLKW, F_SETLKW};
use core::convert::TryFrom;
use linux_object::time::TIMER_ABSTIME;

/// Whether syscall `num`, called with `args`, is made again when the handler
/// of the signal that cut it short has `SA_RESTART`.
pub fn restarts_after_handler(num: u32, args: &[usize; 6]) -> bool {
    let Ok(sys) = Sys::try_from(num) else {
        return false;
    };
    #[cfg(target_arch = "x86_64")]
    if sys == Sys::OPEN {
        return true;
    }
    match sys {
        Sys::READ
        | Sys::READV
        | Sys::PREAD64
        | Sys::PREADV
        | Sys::PREADV2
        | Sys::WRITE
        | Sys::WRITEV
        | Sys::PWRITE64
        | Sys::PWRITEV
        | Sys::PWRITEV2
        // A FIFO's `open` waits for the other end.
        | Sys::OPENAT
        | Sys::WAIT4
        | Sys::WAITID
        | Sys::ACCEPT
        | Sys::ACCEPT4
        | Sys::RECVFROM
        | Sys::RECVMSG
        | Sys::SENDTO
        | Sys::SENDMSG
        | Sys::FLOCK => true,
        Sys::FCNTL => matches!(args[1], F_SETLKW | F_OFD_SETLKW),
        // `__futex_wait()` answers `-ERESTARTSYS`, and only `futex_wait()`
        // turns it into `-ERESTART_RESTARTBLOCK`, when there is a timeout to
        // keep. glibc's `sem_wait` hands the `EINTR` to its caller, so with
        // a handler from `signal()` an untimed `sem_wait` came back -1
        // without the semaphore, where Linux goes on waiting.
        Sys::FUTEX => {
            matches!(args[1] & !FUTEX_FLAGS, FUTEX_WAIT | FUTEX_WAIT_BITSET) && args[3] == 0
        }
        _ => false,
    }
}

/// Whether syscall `num`, called with `args`, is made again whatever the
/// handler of the signal that cut it short, `SA_RESTART` or not
/// (`-ERESTARTNOINTR`).
///
/// Taking a PI lock is the one that waits here: `futex_lock_pi()` turns its
/// `-EINTR` into `-ERESTARTNOINTR`, and futex(2) lists `EINTR` for the waits
/// only. glibc's `pthread_mutex_lock` relies on it. It handles `ESRCH` and
/// `EDEADLK` from `FUTEX_LOCK_PI` and takes anything else as the lock taken,
/// so the `EINTR` this answered when a handler ran over a contended
/// `PTHREAD_PRIO_INHERIT` mutex gave it a second owner. The timeouts are
/// absolute, so the same registers wait for the same end.
pub fn restarts_whatever_the_handler(num: u32, args: &[usize; 6]) -> bool {
    Sys::try_from(num) == Ok(Sys::FUTEX)
        && matches!(args[1] & !FUTEX_FLAGS, FUTEX_LOCK_PI | FUTEX_LOCK_PI2)
}

/// `futex` operations, with the flags `FUTEX_CMD_MASK` takes off.
const FUTEX_WAIT: usize = 0;
const FUTEX_LOCK_PI: usize = 6;
const FUTEX_WAIT_BITSET: usize = 9;
const FUTEX_LOCK_PI2: usize = 13;
/// `FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME`.
const FUTEX_FLAGS: usize = 0x80 | 0x100;

/// Whether syscall `num`, called with `args`, is made again when a
/// job-control stop cut it short and the process has been continued, with
/// no handler run in between.
///
/// Every call [`restarts_after_handler`] makes again, and on top of them the
/// ones Linux answers `-ERESTARTNOHAND` or `-ERESTART_RESTARTBLOCK`, which
/// only a handler turns into `EINTR`: `pause`, `rt_sigsuspend`, `poll`,
/// `select`, a timed `futex` wait, `msgrcv`/`msgsnd`, `clock_nanosleep`. But
/// only where making the call again with the same registers waits for what the
/// first one did -- no timeout, or an absolute one. A relative timeout
/// (`nanosleep`, a `poll` for 100 ms) would start over from the top, and
/// Linux keeps the original end in `restart_block` for that; with nothing
/// here to keep it in, those come back `EINTR` with what they were left
/// (`nanosleep` writes `rem`), which every caller of them already handles,
/// because any handler at all gets them the same answer.
///
/// `epoll_wait`, `rt_sigtimedwait`, `semop` are plain `-EINTR` in Linux, and
/// signal(7) lists them as the calls a stop and a continue DO fail.
pub fn restarts_after_stop(num: u32, args: &[usize; 6]) -> bool {
    if restarts_after_handler(num, args) {
        return true;
    }
    let Ok(sys) = Sys::try_from(num) else {
        return false;
    };
    #[cfg(target_arch = "x86_64")]
    match sys {
        Sys::PAUSE => return true,
        // A negative timeout is "forever".
        Sys::POLL => return (args[2] as i32) < 0,
        Sys::SELECT => return args[4] == 0,
        _ => {}
    }
    match sys {
        Sys::RT_SIGSUSPEND | Sys::MSGRCV | Sys::MSGSND => true,
        // A null timeout is "forever".
        Sys::PPOLL => args[2] == 0,
        Sys::PSELECT6 => args[4] == 0,
        Sys::CLOCK_NANOSLEEP => args[1] & TIMER_ABSTIME != 0,
        Sys::FUTEX => match args[1] & !FUTEX_FLAGS {
            // FUTEX_WAIT's timeout is the one relative one.
            FUTEX_WAIT => args[3] == 0,
            FUTEX_WAIT_BITSET | FUTEX_LOCK_PI | FUTEX_LOCK_PI2 => true,
            _ => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `sys` is restarted, and its name for the message (`Sys` is
    /// not `Copy`, and the cast takes it).
    fn restarts(sys: Sys) -> (bool, Sys) {
        let num = sys as u32;
        (
            restarts_after_handler(num, &[0; 6]),
            Sys::try_from(num).unwrap(),
        )
    }

    #[test]
    fn the_waits_for_data_a_child_or_a_lock_are_made_again() {
        for sys in [
            Sys::READ,
            Sys::READV,
            Sys::PREAD64,
            Sys::PREADV,
            Sys::PREADV2,
            Sys::WRITE,
            Sys::WRITEV,
            Sys::PWRITE64,
            Sys::PWRITEV,
            Sys::PWRITEV2,
            Sys::OPENAT,
            Sys::WAIT4,
            Sys::WAITID,
            Sys::ACCEPT,
            Sys::ACCEPT4,
            Sys::RECVFROM,
            Sys::RECVMSG,
            Sys::SENDTO,
            Sys::SENDMSG,
            Sys::FLOCK,
        ] {
            let (restarts, sys) = restarts(sys);
            assert!(restarts, "{:?} is -ERESTARTSYS in Linux", sys);
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn open_is_made_again_like_openat() {
        assert!(restarts(Sys::OPEN).0);
    }

    #[test]
    fn what_linux_answers_eintr_whenever_a_handler_ran_is_not() {
        // A restarted `poll` starts its timeout over, and a restarted
        // `sigsuspend` waits for a signal that has already come.
        for sys in [
            Sys::PPOLL,
            Sys::PSELECT6,
            Sys::EPOLL_PWAIT,
            Sys::RT_SIGSUSPEND,
            Sys::RT_SIGTIMEDWAIT,
            Sys::NANOSLEEP,
            Sys::CLOCK_NANOSLEEP,
            Sys::CONNECT,
            Sys::CLOSE,
            Sys::IOCTL,
        ] {
            let (restarts, sys) = restarts(sys);
            assert!(!restarts, "{:?} must come back EINTR", sys);
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn the_x86_64_only_waits_linux_answers_eintr_are_not_either() {
        for sys in [Sys::POLL, Sys::SELECT, Sys::EPOLL_WAIT, Sys::PAUSE] {
            let (restarts, sys) = restarts(sys);
            assert!(!restarts, "{:?} must come back EINTR", sys);
        }
    }

    /// Measured on Linux 6.18 with a `SA_RESTART` handler: `FUTEX_WAIT` and
    /// `FUTEX_WAIT_BITSET` with a NULL timeout, and glibc's `sem_wait`, go
    /// on waiting; with a timeout of either kind they come back `EINTR`.
    #[test]
    fn a_futex_wait_is_made_again_only_without_a_timeout() {
        let futex = |op: usize, timeout: usize| {
            restarts_after_handler(Sys::FUTEX as u32, &[0x1000, op, 0, timeout, 0, 0])
        };
        let private = 0x80;
        let realtime = 0x100;
        assert!(futex(FUTEX_WAIT, 0));
        assert!(futex(FUTEX_WAIT | private, 0));
        assert!(futex(FUTEX_WAIT_BITSET | private, 0));
        assert!(futex(FUTEX_WAIT_BITSET | private | realtime, 0));
        assert!(!futex(FUTEX_WAIT | private, 0x2000));
        assert!(!futex(FUTEX_WAIT_BITSET | private, 0x2000));
        // A PI lock is made again whatever the handler, not only with
        // `SA_RESTART`; a wake, a requeue, an unlock, a trylock wait for
        // nothing.
        for op in [FUTEX_LOCK_PI, FUTEX_LOCK_PI2, 1, 3, 4, 7, 8, 10] {
            assert!(!futex(op | private, 0), "op {}", op);
        }
    }

    #[test]
    fn fcntl_is_made_again_only_when_it_waits_for_a_lock() {
        let fcntl = |cmd: usize| restarts_after_handler(Sys::FCNTL as u32, &[3, cmd, 0, 0, 0, 0]);
        assert!(fcntl(F_SETLKW));
        assert!(fcntl(F_OFD_SETLKW));
        // F_SETLK and F_OFD_SETLK, which answer EAGAIN instead of waiting.
        assert!(!fcntl(6));
        assert!(!fcntl(37));
        // And the two commands as their numbers, not as the constants: those
        // now come from `sys_fcntl`, which is what decides whether the call
        // waits, and asking with them would only say the two files agree.
        // This is what pins the numbers themselves.
        assert!(fcntl(7), "F_SETLKW is 7 and it waits");
        assert!(fcntl(38), "F_OFD_SETLKW is 38 and it waits");
        assert!(!fcntl(36), "F_OFD_GETLK is 36 and it does not wait");
    }

    #[test]
    fn a_number_that_is_no_syscall_is_not() {
        assert!(!restarts_after_handler(u32::MAX, &[0; 6]));
        assert!(!restarts_after_stop(u32::MAX, &[0; 6]));
    }

    /// After a stop, made again with the registers it was first made with.
    fn after_stop(sys: Sys, args: [usize; 6]) -> bool {
        restarts_after_stop(sys as u32, &args)
    }

    #[test]
    fn what_a_handler_makes_again_a_stop_makes_again_too() {
        for num in 0..600u32 {
            for args in [
                [0; 6],
                [3, F_SETLKW, 0, 0, 0, 0],
                [0x1000, FUTEX_WAIT_BITSET | 0x80, 0, 0, 0, 0],
            ] {
                if restarts_after_handler(num, &args) {
                    assert!(restarts_after_stop(num, &args), "syscall {}", num);
                }
            }
        }
    }

    #[test]
    fn a_wait_with_nowhere_to_go_but_a_handler_is_made_again_after_a_stop() {
        // `-ERESTARTNOHAND`: EINTR from any handler, made again without one.
        // A `sigsuspend` stopped and continued is still waiting for the
        // signal it was waiting for.
        for sys in [Sys::RT_SIGSUSPEND, Sys::MSGRCV, Sys::MSGSND] {
            let num = sys as u32;
            assert!(!restarts_after_handler(num, &[0; 6]), "syscall {}", num);
            assert!(restarts_after_stop(num, &[0; 6]), "syscall {}", num);
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn so_is_pause_and_an_untimed_poll_or_select() {
        assert!(after_stop(Sys::PAUSE, [0; 6]));
        assert!(after_stop(
            Sys::POLL,
            [0x1000, 1, (-1i32) as u32 as usize, 0, 0, 0]
        ));
        assert!(
            !after_stop(Sys::POLL, [0x1000, 1, 100, 0, 0, 0]),
            "100 ms would start over"
        );
        assert!(
            !after_stop(Sys::POLL, [0x1000, 1, 0, 0, 0, 0]),
            "a 0 timeout never waits"
        );
        assert!(after_stop(Sys::SELECT, [4, 0x1000, 0, 0, 0, 0]));
        assert!(!after_stop(Sys::SELECT, [4, 0x1000, 0, 0, 0x2000, 0]));
    }

    #[test]
    fn a_poll_with_a_timeout_is_not_it_would_start_over() {
        assert!(after_stop(Sys::PPOLL, [0x1000, 1, 0, 0, 0, 0]));
        assert!(!after_stop(Sys::PPOLL, [0x1000, 1, 0x2000, 0, 0, 0]));
        assert!(after_stop(Sys::PSELECT6, [4, 0x1000, 0, 0, 0, 0]));
        assert!(!after_stop(Sys::PSELECT6, [4, 0x1000, 0, 0, 0x2000, 0]));
    }

    #[test]
    fn a_sleep_is_made_again_only_to_an_absolute_end() {
        assert!(after_stop(
            Sys::CLOCK_NANOSLEEP,
            [1, TIMER_ABSTIME, 0x1000, 0, 0, 0]
        ));
        assert!(!after_stop(
            Sys::CLOCK_NANOSLEEP,
            [1, 0, 0x1000, 0x2000, 0, 0]
        ));
        // A relative sleep comes back EINTR with `rem`, as from a handler.
        assert!(!after_stop(Sys::NANOSLEEP, [0x1000, 0x2000, 0, 0, 0, 0]));
    }

    #[test]
    fn a_futex_wait_is_made_again_unless_its_timeout_is_relative() {
        let futex =
            |op: usize, timeout: usize| after_stop(Sys::FUTEX, [0x1000, op, 0, timeout, 0, 0]);
        let private = 0x80;
        let realtime = 0x100;
        assert!(futex(FUTEX_WAIT, 0));
        assert!(futex(FUTEX_WAIT | private, 0));
        assert!(
            !futex(FUTEX_WAIT | private, 0x2000),
            "FUTEX_WAIT's timeout is relative"
        );
        assert!(futex(FUTEX_WAIT_BITSET | private, 0x2000));
        assert!(futex(FUTEX_WAIT_BITSET | private | realtime, 0x2000));
        assert!(futex(FUTEX_LOCK_PI | private, 0x2000));
        assert!(futex(FUTEX_LOCK_PI2, 0));
        // Nothing to make again in a wake, a requeue, a trylock.
        for op in [1, 3, 4, 7, 8, 10] {
            assert!(!futex(op | private, 0), "op {}", op);
        }
    }

    /// `-ERESTARTNOINTR`: taking a PI lock, and nothing else that waits.
    #[test]
    fn a_pi_lock_is_made_again_whatever_the_handler() {
        let futex = |op: usize| {
            restarts_whatever_the_handler(Sys::FUTEX as u32, &[0x1000, op, 0, 0x2000, 0, 0])
        };
        let private = 0x80;
        let realtime = 0x100;
        assert!(futex(FUTEX_LOCK_PI));
        assert!(futex(FUTEX_LOCK_PI | private));
        assert!(futex(FUTEX_LOCK_PI2 | private));
        assert!(futex(FUTEX_LOCK_PI2 | private | realtime));
        // The waits are EINTR from a handler; a wake, an unlock, a trylock
        // have nothing to make again.
        for op in [FUTEX_WAIT, FUTEX_WAIT_BITSET, 1, 3, 4, 7, 8, 10] {
            assert!(!futex(op | private), "op {}", op);
        }
        assert!(
            !restarts_whatever_the_handler(Sys::READ as u32, &[3, FUTEX_LOCK_PI, 1, 0, 0, 0]),
            "a read whose second argument is 6"
        );
        assert!(!restarts_whatever_the_handler(
            u32::MAX,
            &[0, FUTEX_LOCK_PI, 0, 0, 0, 0]
        ));
    }

    /// What a handler makes again has to be left to make again in the first
    /// place, which is what [`restarts_after_stop`] answers.
    #[test]
    fn what_any_handler_makes_again_a_stop_makes_again_too() {
        for op in [FUTEX_LOCK_PI, FUTEX_LOCK_PI | 0x80, FUTEX_LOCK_PI2 | 0x180] {
            let args = [0x1000, op, 0, 0x2000, 0, 0];
            assert!(restarts_whatever_the_handler(Sys::FUTEX as u32, &args));
            assert!(
                restarts_after_stop(Sys::FUTEX as u32, &args),
                "op {:#x}",
                op
            );
        }
    }

    #[test]
    fn what_linux_fails_after_a_stop_and_a_continue_is_not() {
        // signal(7): "certain blocking interfaces can fail with the error
        // EINTR after the process is stopped by one of the stop signals and
        // then resumed via SIGCONT."
        for sys in [
            Sys::EPOLL_PWAIT,
            Sys::RT_SIGTIMEDWAIT,
            Sys::SEMTIMEDOP,
            Sys::CONNECT,
        ] {
            let num = sys as u32;
            assert!(!restarts_after_stop(num, &[0; 6]), "syscall {}", num);
        }
    }
}
