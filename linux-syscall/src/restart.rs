//! Which syscalls a `SA_RESTART` handler makes again.
//!
//! A blocking syscall a signal cuts short answers `EINTR` here, every one of
//! them. Linux answers four different things, and only one of them reaches
//! userspace as `EINTR` when the handler was installed with `SA_RESTART`:
//!
//! - `-ERESTARTSYS`: the wait for data, for a child, for a lock, for a
//!   connection. With `SA_RESTART` the kernel winds the program counter back
//!   onto the syscall instruction before it builds the signal frame, so when
//!   the handler returns the same call is made again and the program never
//!   sees the signal. Without it, `EINTR`.
//! - `-ERESTARTNOHAND`: `poll`, `select`, `epoll_wait`, `pause`,
//!   `rt_sigsuspend`. `EINTR` whenever a handler ran, `SA_RESTART` or not.
//! - `-ERESTART_RESTARTBLOCK`: `nanosleep`, `clock_nanosleep`, a `futex` wait
//!   with a timeout. Also `EINTR` when a handler ran.
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

use super::Sys;
use core::convert::TryFrom;

/// `fcntl` commands that wait for a lock: the only ones that can be cut
/// short, and `-ERESTARTSYS` when they are (`fcntl_setlk`).
const F_SETLKW: usize = 7;
const F_OFD_SETLKW: usize = 38;

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
            Sys::FUTEX,
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

    #[test]
    fn fcntl_is_made_again_only_when_it_waits_for_a_lock() {
        let fcntl = |cmd: usize| restarts_after_handler(Sys::FCNTL as u32, &[3, cmd, 0, 0, 0, 0]);
        assert!(fcntl(F_SETLKW));
        assert!(fcntl(F_OFD_SETLKW));
        // F_SETLK and F_OFD_SETLK, which answer EAGAIN instead of waiting.
        assert!(!fcntl(6));
        assert!(!fcntl(37));
    }

    #[test]
    fn a_number_that_is_no_syscall_is_not() {
        assert!(!restarts_after_handler(u32::MAX, &[0; 6]));
    }
}
