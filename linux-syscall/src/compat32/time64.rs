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
//! ## What is still missing
//!
//! The rest of the family, each of which needs the same treatment of a
//! structure this kernel already converts: `timer_gettime64` (408),
//! `timer_settime64` (409), `utimensat_time64` (412),
//! `pselect6_time64` (413), `ppoll_time64` (414), `recvmmsg_time64` (417),
//! `rt_sigtimedwait_time64` (421) and `sched_rr_get_interval_time64` (423).

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
}

#[cfg(test)]
mod tests;
