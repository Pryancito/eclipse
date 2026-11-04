//! The `*_time64` calls: the 32-bit world's answer to 2038.
//!
//! These are the one family in this whole layer where the width of a
//! structure is a property of the **call** and not of the process. Everywhere
//! else, [`crate::narrow`] asks the process how wide a word is --- Linux's
//! `in_compat_syscall()` --- and that is the rule this directory is built on.
//! Here it is wrong: the SAME 32-bit process gets
//!
//! | call | i386 number | `struct timespec` |
//! |---|---|---|
//! | `clock_gettime` | 265 | two 32-bit words, 8 bytes |
//! | `clock_gettime64` | 403 | two 64-bit words, 16 bytes |
//!
//! because `struct __kernel_timespec` is 64-bit at every width. That is what
//! the `64` in the name means, and it is why Linux has two entry points
//! rather than one with a test in it.
//!
//! ## Why they are not a row in either allowlist
//!
//! A row would send number 403 to the native `clock_gettime`, whose handler
//! asks the process --- and would get back eight bytes where the program is
//! waiting for sixteen. The program would read its `tv_nsec` out of the
//! second half of its `tv_sec` and the eight bytes above its own structure
//! would stay as they were. So the number cannot simply be translated: the
//! handler has to be told.
//!
//! The entry points below do exactly that, by calling the same core the
//! native entry calls with `abi32 = false`. Nothing else differs: the clock
//! numbers, the capability check on `clock_settime`, the `EINVAL` for a clock
//! that does not exist and the `TIMER_ABSTIME` handling are all the native
//! ones, in one place, reached from both widths.
//!
//! ## Why it matters that these exist at all
//!
//! A musl built for i386 in the last few years calls **only** the `_time64`
//! forms: `clock_gettime(CLOCK_REALTIME)` in a 32-bit program is i386 403 and
//! not 265. With 403 answering `ENOSYS` the program has no clock --- not a
//! wrong time, no time --- and every `gettimeofday`, every `time()`, every
//! timestamp in a log and every timeout computed from one fails.
//!
//! ## What is here
//!
//! `clock_gettime64` (403), `clock_settime64` (404),
//! `clock_getres_time64` (406), `clock_nanosleep_time64` (407),
//! `timerfd_gettime64` (410), `timerfd_settime64` (411) and `futex_time64`
//! (422), each an entry point that calls the native handler's own core with
//! `abi32 = false`. The three last are the ones a THREADED 32-bit program
//! reaches: 422 is every `pthread_cond_timedwait`, `sem_timedwait` and
//! `pthread_mutex_timedlock` of a modern i386 musl, and the two `timerfd`
//! numbers are behind every timer it arms.
//!
//! And `utimensat_time64` (412), `ppoll_time64` (414) and
//! `sched_rr_get_interval_time64` (423). 412 is the number a modern i386
//! musl's `utimensat` and `futimens` actually send, and 414 the one behind
//! its `poll` with a deadline.
//!
//! 412 is worth singling out: its `times` is an ARRAY of two
//! `struct __kernel_timespec`, so it is the one call that is in BOTH of this
//! layer's rules at once --- by i386's 320 the width of the pair is the
//! PROCESS's and the pair is sixteen bytes, by 412 it is the CALL's and the
//! pair is thirty-two. One core, `Syscall::utimensat_at`, reached from both
//! with the answer handed in.
//!
//! And `timer_gettime64` (408) with `timer_settime64` (409), the POSIX
//! timers of `timer_create(2)`. Their `struct itimerspec` is converted in
//! three places rather than one, because a timer on a CPU clock
//! (`CLOCK_PROCESS_CPUTIME_ID`, `CLOCK_THREAD_CPUTIME_ID`) takes a path of
//! its own that arms nothing and reads the clock instead, so the width goes
//! down into `cpu_timer_settime` too.
//!
//! And `pselect6_time64` (413), the SECOND call in both rules at once, and
//! the one that made the second rule's shape plain: its timeout is the
//! number's, and the two words of its sixth argument ---
//! `{ const sigset_t *ss; size_t ss_len; }` --- are the process's, because
//! they are `compat_sigset_argpack` whichever number the call came in by.
//! Converting them is also what let i386's plain `pselect6` (308) into the
//! translation table at last, so the two numbers reach the one handler.
//!
//! Those two words carry the one field no structure in [`crate::narrow`] had
//! before: a **pointer**, which is zero-extended and not sign-extended. See
//! `PSelectSigsetArg`.
//!
//! ## What is still missing
//!
//! `recvmmsg_time64` (417); and `rt_sigtimedwait_time64` (421), which passes
//! a `siginfo_t` outwards.

use super::*;
use crate::file::ITimerSpec;
use linux_object::fs::FileDesc;
use linux_object::time::TimeSpec;

/// i386's number for `clock_gettime64`.
pub(super) const I386_CLOCK_GETTIME64: u32 = 403;
/// i386's number for `clock_settime64`.
pub(super) const I386_CLOCK_SETTIME64: u32 = 404;
/// i386's number for `clock_getres_time64`.
pub(super) const I386_CLOCK_GETRES_TIME64: u32 = 406;
/// i386's number for `clock_nanosleep_time64`.
pub(super) const I386_CLOCK_NANOSLEEP_TIME64: u32 = 407;
/// i386's number for `timerfd_gettime64`.
pub(super) const I386_TIMERFD_GETTIME64: u32 = 410;
/// i386's number for `timerfd_settime64`.
pub(super) const I386_TIMERFD_SETTIME64: u32 = 411;
/// i386's number for `futex_time64`.
pub(super) const I386_FUTEX_TIME64: u32 = 422;
/// i386's number for `utimensat_time64`.
pub(super) const I386_UTIMENSAT_TIME64: u32 = 412;
/// i386's number for `ppoll_time64`.
pub(super) const I386_PPOLL_TIME64: u32 = 414;
/// i386's number for `sched_rr_get_interval_time64`.
pub(super) const I386_SCHED_RR_GET_INTERVAL_TIME64: u32 = 423;
/// i386's number for `timer_gettime64`.
pub(super) const I386_TIMER_GETTIME64: u32 = 408;
/// i386's number for `timer_settime64`.
pub(super) const I386_TIMER_SETTIME64: u32 = 409;
/// i386's number for `pselect6_time64`.
pub(super) const I386_PSELECT6_TIME64: u32 = 413;

impl Syscall<'_> {
    /// `clock_gettime64` (403): the native call, writing a
    /// `struct __kernel_timespec`.
    pub fn sys_clock_gettime64(&self, clock: usize, buf: UserOutPtr<TimeSpec>) -> SysResult {
        self.clock_gettime_at(clock, buf, false)
    }

    /// `clock_settime64` (404).
    pub fn sys_clock_settime64(&self, clock: usize, timespec: UserInPtr<TimeSpec>) -> SysResult {
        self.clock_settime_at(clock, timespec, false)
    }

    /// `clock_getres_time64` (406).
    pub fn sys_clock_getres_time64(&self, clock: usize, buf: UserOutPtr<TimeSpec>) -> SysResult {
        self.clock_getres_at(clock, buf, false)
    }

    /// `clock_nanosleep_time64` (407), whose `timespec` crosses in both
    /// directions: the request in, and the remainder a signal leaves out.
    pub async fn sys_clock_nanosleep_time64(
        &mut self,
        clockid: usize,
        flags: usize,
        req: UserInPtr<TimeSpec>,
        rem: UserOutPtr<TimeSpec>,
    ) -> SysResult {
        self.clock_nanosleep_at(clockid, flags, req, rem, false)
            .await
    }

    /// `timerfd_gettime64` (410): the arm time and the interval of a
    /// `timerfd`, as two `struct __kernel_timespec`.
    pub fn sys_timerfd_gettime64(
        &self,
        fd: FileDesc,
        curr_value: UserOutPtr<ITimerSpec>,
    ) -> SysResult {
        self.timerfd_gettime_at(fd, curr_value, false)
    }

    /// `timerfd_settime64` (411), whose `struct itimerspec` crosses in both
    /// directions: the new value in and the old one out.
    pub fn sys_timerfd_settime64(
        &self,
        fd: FileDesc,
        flags: usize,
        new_value: UserInPtr<ITimerSpec>,
        old_value: UserOutPtr<ITimerSpec>,
    ) -> SysResult {
        self.timerfd_settime_at(fd, flags, new_value, old_value, false)
    }

    /// `futex_time64` (422): the timeout at `val2` is a
    /// `struct __kernel_timespec`.
    ///
    /// This is the number every `pthread_cond_timedwait` and every
    /// `sem_timedwait` of a modern i386 musl reaches, so with it answering
    /// `ENOSYS` no threaded 32-bit program can wait with a deadline at all.
    /// Note that only the timeout changes width: the futex word itself is a
    /// 32-bit `int` in both ABIs, and `val`, `val2`-as-`val2` and `val3` are
    /// plain numbers.
    #[allow(clippy::too_many_arguments)]
    pub async fn sys_futex_time64(
        &self,
        uaddr: usize,
        op: u32,
        val: u32,
        val2: usize,
        uaddr2: usize,
        val3: u32,
    ) -> SysResult {
        self.futex_at(uaddr, op, val, val2, uaddr2, val3, false)
            .await
    }

    /// `utimensat_time64` (412), whose `times` is an array of two
    /// `struct __kernel_timespec`: **thirty-two** bytes from a 32-bit
    /// program, where i386's 320 passes sixteen.
    ///
    /// This is the number a modern i386 musl sends for `utimensat(2)`,
    /// `futimens(3)` and `utimes(3)` alike, so with it answering `ENOSYS` a
    /// 32-bit program cannot set a file's times at all --- which is every
    /// archive extractor, every `install`, every `cp -p`.
    pub fn sys_utimensat_time64(
        &mut self,
        dirfd: FileDesc,
        pathname: UserInPtr<u8>,
        times: kernel_hal::user::UserInOutPtr<[TimeSpec; 2]>,
        flags: usize,
    ) -> SysResult {
        self.utimensat_at(dirfd, pathname, times, flags, false)
    }

    /// `ppoll_time64` (414): the timeout is a `struct __kernel_timespec`.
    /// Everything else `ppoll` passes is a `struct pollfd` of three fields
    /// that are the same width in both ABIs, or a `sigset_t`.
    pub async fn sys_ppoll_time64(
        &mut self,
        ufds: kernel_hal::user::UserInOutPtr<crate::file::PollFd>,
        nfds: usize,
        timeout: UserInPtr<TimeSpec>,
        sigmask: UserInPtr<linux_object::signal::Sigset>,
        sigsetsize: usize,
    ) -> SysResult {
        self.ppoll_at(ufds, nfds, timeout, sigmask, sigsetsize, false)
            .await
    }

    /// `pselect6_time64` (413): the timeout is a `struct __kernel_timespec`.
    ///
    /// The SECOND structure this call passes is not a time at all and does
    /// not change width with the number: the sixth argument's
    /// `{ const sigset_t *ss; size_t ss_len; }` is `compat_sigset_argpack`
    /// for a 32-bit caller whichever number it came in by. So, like
    /// `utimensat_time64`, this call is in both of this layer's rules at
    /// once, and the core takes only the one answer that the number decides.
    #[allow(clippy::too_many_arguments)]
    pub async fn sys_pselect6_time64(
        &mut self,
        nfds: usize,
        read: kernel_hal::user::UserInOutPtr<u32>,
        write: kernel_hal::user::UserInOutPtr<u32>,
        err: kernel_hal::user::UserInOutPtr<u32>,
        timeout: kernel_hal::user::UserInOutPtr<TimeSpec>,
        sigset_arg: usize,
    ) -> SysResult {
        self.pselect6_at(nfds, read, write, err, timeout, sigset_arg, false)
            .await
    }

    /// `sched_rr_get_interval_time64` (423).
    pub fn sys_sched_rr_get_interval_time64(
        &self,
        pid: usize,
        interval: UserOutPtr<TimeSpec>,
    ) -> SysResult {
        self.sched_rr_get_interval_at(pid, interval, false)
    }

    /// `timer_gettime64` (408): what is left of a POSIX timer and its
    /// interval, as a `struct __kernel_itimerspec` --- thirty-two bytes,
    /// where 260's is sixteen.
    pub fn sys_timer_gettime64(&self, id: usize, curr_value: usize) -> SysResult {
        self.timer_gettime_at(id, curr_value, false)
    }

    /// `timer_settime64` (409), whose `itimerspec` crosses in both
    /// directions: the new value in and the old one out.
    pub fn sys_timer_settime64(
        &self,
        id: usize,
        flags: usize,
        new_value: UserInPtr<linux_object::time::ITimerSpec>,
        old_value: UserOutPtr<linux_object::time::ITimerSpec>,
    ) -> SysResult {
        self.timer_settime_at(id, flags, new_value, old_value, false)
    }
}

#[cfg(test)]
mod tests;

/// The three numbers whose core already existed and only needed the width
/// handed in: own file so the two subjects do not share a ceiling.
#[cfg(test)]
mod shared_core_tests;

/// `pselect6_time64` (413) next to i386's 308: the second call in both of
/// this layer's rules at once, and the one whose sixth argument carries the
/// first POINTER to go through [`crate::narrow`]. Own file, own subject.
#[cfg(test)]
mod pselect_tests;
