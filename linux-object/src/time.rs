//! Linux time objects

use alloc::sync::Arc;
use core::time::Duration;
use rcore_fs::vfs::*;

/// TimeSpec struct for clock_gettime, similar to Timespec
#[repr(C)]
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct TimeSpec {
    /// seconds
    pub sec: usize,
    /// nano seconds
    pub nsec: usize,
}

/// TimeVal struct for gettimeofday
#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct TimeVal {
    /// seconds
    pub sec: usize,
    /// microsecond
    pub usec: usize,
}

/// ITimerVal struct for setitimer/getitimer
#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct ITimerVal {
    /// timer interval
    pub interval: TimeVal,
    /// current value
    pub value: TimeVal,
}

/// `struct itimerspec` for `timer_settime`/`timer_gettime` (nanosecond res).
#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct ITimerSpec {
    /// timer period (0 = one-shot)
    pub interval: TimeSpec,
    /// time until next expiration
    pub value: TimeSpec,
}

/// Nanoseconds in a second, the range `tv_nsec` is allowed to hold.
pub const NSEC_PER_SEC: usize = 1_000_000_000;
/// Microseconds in a second, likewise for `tv_usec`.
pub const USEC_PER_SEC: usize = 1_000_000;

impl From<TimeVal> for Duration {
    fn from(t: TimeVal) -> Self {
        Duration::from_secs(t.sec as u64).saturating_add(Duration::from_micros(t.usec as u64))
    }
}

impl From<Duration> for TimeVal {
    fn from(d: Duration) -> Self {
        TimeVal {
            sec: d.as_secs() as usize,
            usec: d.subsec_micros() as usize,
        }
    }
}

/// Kernel-side state of one `setitimer(2)` slot
/// (ITIMER_REAL / ITIMER_VIRTUAL / ITIMER_PROF).
#[derive(Debug, Default, Clone, Copy)]
pub struct ItimerSlot {
    /// Reload period; zero means one-shot.
    pub interval: Duration,
    /// Absolute expiry on the boot-monotonic clock; `None` while disarmed.
    pub deadline: Option<Duration>,
    /// Bumped on every arm/disarm. A timer callback captures the value it was
    /// armed with and compares before firing, so a replaced or cancelled timer
    /// expires silently instead of delivering a stale signal.
    pub generation: u64,
}

impl TimeVal {
    /// create TimeVal
    pub fn now() -> TimeVal {
        TimeSpec::now().into()
    }
    /// Monotonic time since boot (`CLOCK_MONOTONIC`). Used to timestamp evdev
    /// input events: libinput selects `CLOCK_MONOTONIC` via `EVIOCSCLOCKID` and
    /// compares event times against `clock_gettime(CLOCK_MONOTONIC)`. Stamping
    /// events with the wall clock instead makes libinput's timers (button
    /// debounce, tap, scroll) see multi-second offsets and misbehave.
    pub fn now_monotonic() -> TimeVal {
        TimeSpec::now_monotonic().into()
    }
    /// to msec
    pub fn to_msec(&self) -> usize {
        self.sec
            .saturating_mul(1_000)
            .saturating_add(self.usec / 1_000)
    }

    /// Same rule as [`TimeSpec::valid`], in microseconds. `setitimer(2)`
    /// rejects an out-of-range `tv_usec` with `EINVAL`.
    pub fn valid(&self) -> bool {
        self.usec < USEC_PER_SEC && (self.sec as isize) >= 0
    }

    /// See [`TimeSpec::try_into_poll_msecs`]; `select(2)` takes its timeout
    /// as a `timeval`.
    pub fn try_into_poll_msecs(&self) -> crate::error::LxResult<isize> {
        if self.valid() {
            Ok(self.to_msec().min(isize::MAX as usize) as isize)
        } else {
            Err(crate::error::LxError::EINVAL)
        }
    }

    /// The duration this `timeval` names, or `EINVAL` if it does not name
    /// one.
    pub fn try_into_duration(&self) -> crate::error::LxResult<Duration> {
        if self.valid() {
            Ok(Duration::new(self.sec as u64, (self.usec * 1_000) as u32))
        } else {
            Err(crate::error::LxError::EINVAL)
        }
    }
}

impl TimeSpec {
    /// Build from a kernel `Duration` (seconds since Unix epoch for wall clock).
    pub fn from_duration(time: Duration) -> TimeSpec {
        TimeSpec {
            sec: time.as_secs() as usize,
            nsec: time.subsec_nanos() as usize,
        }
    }

    /// Wall-clock time (`CLOCK_REALTIME`, `gettimeofday`, `date`).
    pub fn now() -> TimeSpec {
        Self::from_duration(kernel_hal::timer::wall_clock_now())
    }

    /// Monotonic time since boot (`CLOCK_MONOTONIC`).
    pub fn now_monotonic() -> TimeSpec {
        Self::from_duration(kernel_hal::timer::timer_now())
    }

    /// update TimeSpec for a file inode
    /// TODO: more precise; update when write
    pub fn update(inode: &Arc<dyn INode>) {
        let now = TimeSpec::now().into();
        if let Ok(mut metadata) = inode.metadata() {
            metadata.atime = now;
            metadata.mtime = now;
            metadata.ctime = now;
            // silently fail for device file
            inode.set_metadata(&metadata).ok();
        }
    }

    /// to msec
    pub fn to_msec(&self) -> usize {
        self.sec
            .saturating_mul(1_000)
            .saturating_add(self.nsec / 1_000_000)
    }

    /// Whether this is a `timespec` the kernel will act on.
    ///
    /// Linux checks the same two things before every sleep and every timer
    /// arm: the nanoseconds must be a fraction of a second, and the seconds
    /// must not be negative. `tv_sec` and `tv_nsec` are signed in the uAPI
    /// and unsigned here, so a negative value arrives as a very large one --
    /// the top bit is the sign bit either way, which is what the second test
    /// reads.
    pub fn valid(&self) -> bool {
        self.nsec < NSEC_PER_SEC && (self.sec as isize) >= 0
    }

    /// Timeout in milliseconds for `poll` and `select`, in the
    /// representation those take: a **negative** value there means "wait for
    /// ever". A cast alone is not enough, because the milliseconds are a
    /// `usize` and anything from 2^63 up comes out negative -- a program
    /// asking for a very long but finite wait would silently get an infinite
    /// one and hang with no way to tell why. Clamping keeps it finite.
    pub fn try_into_poll_msecs(&self) -> crate::error::LxResult<isize> {
        if self.valid() {
            Ok(self.to_msec().min(isize::MAX as usize) as isize)
        } else {
            Err(crate::error::LxError::EINVAL)
        }
    }

    /// The duration this `timespec` names, or `EINVAL` if it does not name
    /// one. This is what a syscall taking a timeout from userspace wants:
    /// Linux rejects an out-of-range `timespec` before it sleeps, rather
    /// than sleeping for some other length of time.
    pub fn try_into_duration(&self) -> crate::error::LxResult<Duration> {
        if self.valid() {
            Ok(Duration::new(self.sec as u64, self.nsec as u32))
        } else {
            Err(crate::error::LxError::EINVAL)
        }
    }
}

impl From<Timespec> for TimeSpec {
    fn from(t: Timespec) -> Self {
        Self {
            sec: t.sec as _,
            nsec: t.nsec as _,
        }
    }
}

impl From<TimeSpec> for Timespec {
    fn from(t: TimeSpec) -> Self {
        Self {
            sec: t.sec as _,
            nsec: t.nsec as _,
        }
    }
}

impl From<TimeSpec> for Duration {
    /// Saturating, because this conversion is infallible and reached from a
    /// dozen call sites. `Duration::new` **panics** when the nanoseconds
    /// carry into a seconds count that overflows, and these fields come
    /// straight out of userspace: a `timespec` of all-ones (which is how a
    /// negative `tv_sec`/`tv_nsec` reads through an unsigned field) used to
    /// take the kernel down from any unprivileged process. Callers that owe
    /// userspace an error use [`TimeSpec::try_into_duration`] instead.
    fn from(t: TimeSpec) -> Self {
        Duration::from_secs(t.sec as u64).saturating_add(Duration::from_nanos(t.nsec as u64))
    }
}

impl From<TimeSpec> for TimeVal {
    fn from(t: TimeSpec) -> Self {
        Self {
            sec: t.sec,
            usec: t.nsec / 1_000,
        }
    }
}

/// RUsage for sys_getrusage() — full Linux `struct rusage` layout.
///
/// Only the two time fields carry data; the 14 trailing longs
/// (`ru_maxrss` … `ru_nivcsw`) read as zero, which is also how Linux reports
/// the fields it does not maintain. Carrying them in the struct still matters:
/// when only the two timevals were written, the tail of the caller's buffer
/// kept whatever garbage was on the stack, and rusage consumers (`time(1)`,
/// libuv's getrusage wrapper) read uninitialized memory as huge fault counts.
#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct RUsage {
    /// user CPU time used
    pub utime: TimeVal,
    /// system CPU time used
    pub stime: TimeVal,
    /// ru_maxrss … ru_nivcsw, all reported as zero
    pub other: [i64; 14],
}

/// Tms for times()
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct Tms {
    /// user time
    pub tms_utime: u64,
    /// system time
    pub tms_stime: u64,
    /// user time of children
    pub tms_cutime: u64,
    /// system time of children
    pub tms_cstime: u64,
}

/// A POSIX clock id, as `clock_gettime(2)` and friends spell it.
///
/// The conversion from the raw `usize` a syscall is handed is **fallible**,
/// and has to be: this used to be an infallible `From<usize>` ending in
/// `unreachable!()`, which made `clock_nanosleep(99, ...)` a kernel panic.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[repr(usize)]
pub enum ClockId {
    /// Wall-clock time, settable, counted from the Unix epoch.
    ClockRealTime = 0,
    /// Time since boot, never settable, never jumps.
    ClockMonotonic = 1,
    /// CPU time used by the whole process.
    ClockProcessCpuTimeId = 2,
    /// CPU time used by the calling thread.
    ClockThreadCpuTimeId = 3,
    /// Monotonic time with no NTP adjustment applied.
    ClockMonotonicRaw = 4,
    /// A cheaper, coarser `CLOCK_REALTIME`.
    ClockRealTimeCoarse = 5,
    /// A cheaper, coarser `CLOCK_MONOTONIC`.
    ClockMonotonicCoarse = 6,
    /// Like `CLOCK_MONOTONIC`, but counting time spent suspended.
    ClockBootTime = 7,
    /// `CLOCK_REALTIME` that also wakes the machine from suspend.
    ClockRealTimeAlarm = 8,
    /// `CLOCK_BOOTTIME` that also wakes the machine from suspend.
    ClockBootTimeAlarm = 9,
}

impl ClockId {
    /// The clock a process named, or `EINVAL` if it named none of them.
    ///
    /// Written out rather than range-checked so that adding a clock has to
    /// be a decision taken here and in [`clock_nanosleep_base`], not a
    /// number that quietly starts being accepted.
    pub fn from_raw(raw: usize) -> crate::error::LxResult<Self> {
        Ok(match raw {
            0 => Self::ClockRealTime,
            1 => Self::ClockMonotonic,
            2 => Self::ClockProcessCpuTimeId,
            3 => Self::ClockThreadCpuTimeId,
            4 => Self::ClockMonotonicRaw,
            5 => Self::ClockRealTimeCoarse,
            6 => Self::ClockMonotonicCoarse,
            7 => Self::ClockBootTime,
            8 => Self::ClockRealTimeAlarm,
            9 => Self::ClockBootTimeAlarm,
            // Every negative id lands here too, as a very large `usize`:
            // Linux routes those to the per-process CPU clocks, which this
            // kernel does not have. `CLOCK_TAI` is not modelled either.
            _ => return Err(crate::error::LxError::EINVAL),
        })
    }
}

/// `clock_nanosleep(2)`'s only flag: the requested time is an absolute point
/// on the given clock rather than a length.
pub const TIMER_ABSTIME: usize = 1;

/// The timeline a clock's times are counted on.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ClockBase {
    /// Counted from boot, which is what the kernel's own timer runs on.
    Monotonic,
    /// Counted from the Unix epoch.
    Wall,
}

/// What a `clock_nanosleep` call should do, worked out before anything sleeps.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SleepPlan {
    /// Sleep until this point on the monotonic timeline.
    Until(Duration),
    /// The deadline has already gone by; return at once.
    AlreadyPast,
}

/// Which timeline `clock_nanosleep` counts `clock` on, or why it cannot sleep
/// on it at all.
///
/// Linux answers this with `clockid_to_kclock` and then `kc->nsleep`, and it
/// has three different answers: a clock it does not know is `EINVAL`, one it
/// knows but whose `k_clock` has no `nsleep` is `EOPNOTSUPP`, and
/// `CLOCK_THREAD_CPUTIME_ID`'s own `nsleep` returns `EINVAL`. This used to be
/// `ClockId::from`, an infallible `From` ending in `unreachable!()`, so
/// `clock_nanosleep(99, ...)` was a kernel panic from an ordinary syscall.
pub fn clock_nanosleep_base(clock: usize) -> crate::error::LxResult<ClockBase> {
    use crate::error::LxError;
    // No catch-all arm: a clock added to `ClockId` has to say here what
    // sleeping on it means, rather than falling into someone else's answer.
    Ok(match ClockId::from_raw(clock)? {
        ClockId::ClockRealTime => ClockBase::Wall,
        ClockId::ClockMonotonic => ClockBase::Monotonic,
        // Linux sleeps on the process's own CPU time; this kernel does not
        // account it, so the sleep runs on elapsed time instead. CPU time
        // never runs ahead of elapsed time, so the sleep can end early and
        // never late, which is the benign direction for a sleep.
        // `sys_setitimer` makes the same choice for ITIMER_VIRTUAL and
        // ITIMER_PROF.
        ClockId::ClockProcessCpuTimeId => ClockBase::Monotonic,
        // `thread_cpu_nsleep` is `return -EINVAL`.
        ClockId::ClockThreadCpuTimeId => return Err(LxError::EINVAL),
        // These three carry no `nsleep` in their `k_clock` at all, which is
        // a different answer from one Linux has never heard of.
        ClockId::ClockMonotonicRaw
        | ClockId::ClockRealTimeCoarse
        | ClockId::ClockMonotonicCoarse => return Err(LxError::EOPNOTSUPP),
        // This kernel's monotonic timer does not stop for suspend, so boot
        // time and monotonic time are the same thing here.
        ClockId::ClockBootTime => ClockBase::Monotonic,
        // Linux wants CAP_WAKE_ALARM for these and wakes the machine from
        // suspend; there is no suspend here, so they are their non-alarm
        // clocks.
        ClockId::ClockRealTimeAlarm => ClockBase::Wall,
        ClockId::ClockBootTimeAlarm => ClockBase::Monotonic,
    })
}

/// What `clock_nanosleep` should do, worked out before anything sleeps.
///
/// `request` is a length when `TIMER_ABSTIME` is clear, and a point on
/// `clock`'s own timeline when it is set. Only that one bit is looked at:
/// `common_nsleep` does `flags & TIMER_ABSTIME`, so a flag word carrying
/// other bits is a relative sleep in Linux and not an error.
///
/// The answer is always on the monotonic timeline, because that is the only
/// one the kernel's timer can be asked to wake on.
pub fn plan_clock_nanosleep(
    clock: usize,
    flags: usize,
    request: Duration,
    now_monotonic: Duration,
    now_wall: Duration,
) -> crate::error::LxResult<SleepPlan> {
    let base = clock_nanosleep_base(clock)?;
    if flags & TIMER_ABSTIME == 0 {
        return Ok(SleepPlan::Until(now_monotonic.saturating_add(request)));
    }
    // An absolute request is a point on the clock's own timeline, so what is
    // left of it is the distance from that clock's `now`. Sleeping for the
    // absolute value itself is what the tree used to do, and on
    // CLOCK_REALTIME that is decades.
    let now = match base {
        ClockBase::Monotonic => now_monotonic,
        ClockBase::Wall => now_wall,
    };
    let remaining = request.saturating_sub(now);
    if remaining.is_zero() {
        Ok(SleepPlan::AlreadyPast)
    } else {
        Ok(SleepPlan::Until(now_monotonic.saturating_add(remaining)))
    }
}

/// The timeline `timerfd_create(2)` counts `clock` on, or `EINVAL` if it will
/// not take that clock at all.
///
/// Linux (`fs/timerfd.c`) names its five clocks outright and answers `EINVAL`
/// for everything else. It is a shorter list than [`clock_nanosleep_base`]'s
/// because a timerfd is armed against a hardware timer: there is no CPU-time
/// timerfd and no coarse one.
pub fn timerfd_clock_base(clock: usize) -> crate::error::LxResult<ClockBase> {
    use crate::error::LxError;
    // No catch-all arm, for the same reason `clock_nanosleep_base` has none:
    // a clock added to `ClockId` has to say here what a timerfd on it means.
    Ok(match ClockId::from_raw(clock)? {
        ClockId::ClockRealTime => ClockBase::Wall,
        ClockId::ClockMonotonic => ClockBase::Monotonic,
        // `timerfd_create` lists neither the CPU clocks nor the coarse ones.
        ClockId::ClockProcessCpuTimeId
        | ClockId::ClockThreadCpuTimeId
        | ClockId::ClockMonotonicRaw
        | ClockId::ClockRealTimeCoarse
        | ClockId::ClockMonotonicCoarse => return Err(LxError::EINVAL),
        // This kernel's monotonic timer does not stop for suspend, so boot
        // time and monotonic time are the same thing here.
        ClockId::ClockBootTime => ClockBase::Monotonic,
        // Linux wants CAP_WAKE_ALARM for these and wakes the machine from
        // suspend; there is no suspend here, so they are their non-alarm
        // clocks. Same choice as `clock_nanosleep_base`.
        ClockId::ClockRealTimeAlarm => ClockBase::Wall,
        ClockId::ClockBootTimeAlarm => ClockBase::Monotonic,
    })
}

/// The timeline `timer_create(2)` counts `clock` on, or why it will not take
/// that clock at all.
///
/// Linux looks the clock up in `posix_clocks[]` and then asks it for a
/// `timer_create`: an id it has never heard of is `EINVAL`, and one whose
/// `k_clock` carries no `timer_create` — the raw and coarse clocks — is
/// `EOPNOTSUPP`. The two CPU clocks do have one, unlike `nsleep`.
pub fn posix_timer_clock_base(clock: usize) -> crate::error::LxResult<ClockBase> {
    use crate::error::LxError;
    Ok(match ClockId::from_raw(clock)? {
        ClockId::ClockRealTime => ClockBase::Wall,
        ClockId::ClockMonotonic => ClockBase::Monotonic,
        // This kernel does not account CPU time, so these tick on elapsed
        // time instead — the same substitution `clock_nanosleep_base` makes
        // for the process clock and `sys_setitimer` for ITIMER_VIRTUAL and
        // ITIMER_PROF.
        ClockId::ClockProcessCpuTimeId | ClockId::ClockThreadCpuTimeId => ClockBase::Monotonic,
        // No `timer_create` in their `k_clock`, which is a different answer
        // from a clock Linux has never heard of.
        ClockId::ClockMonotonicRaw
        | ClockId::ClockRealTimeCoarse
        | ClockId::ClockMonotonicCoarse => return Err(LxError::EOPNOTSUPP),
        ClockId::ClockBootTime => ClockBase::Monotonic,
        ClockId::ClockRealTimeAlarm => ClockBase::Wall,
        ClockId::ClockBootTimeAlarm => ClockBase::Monotonic,
    })
}

/// The point on the **monotonic** timeline a timer armed with `value` must
/// wake at, which is the only kind of deadline the kernel's timer takes.
///
/// `value` is a length when `abs` is clear (`TFD_TIMER_ABSTIME` /
/// `TIMER_ABSTIME` unset), and a point on `base`'s own timeline when it is
/// set. Arming an absolute wall-clock time as if it were a monotonic one is
/// what the tree used to do for `clock_nanosleep` — see
/// [`plan_clock_nanosleep`], which says the same thing about sleeping — and
/// on `CLOCK_REALTIME` the gap between the two is the whole age of the Unix
/// epoch, so the timer fires decades late, which is to say never.
///
/// A deadline already gone by comes back as `now_monotonic`, not as an
/// error: `timer_set` serves a past deadline as soon as it can, and Linux
/// counts one expiration straight away for it.
pub fn timer_arm_deadline(
    base: ClockBase,
    abs: bool,
    value: Duration,
    now_monotonic: Duration,
    now_wall: Duration,
) -> Duration {
    if !abs {
        return now_monotonic.saturating_add(value);
    }
    let now = match base {
        ClockBase::Monotonic => now_monotonic,
        ClockBase::Wall => now_wall,
    };
    now_monotonic.saturating_add(value.saturating_sub(now))
}

/// Whose CPU time a CPU clock adds up. `0` is the caller's own process or
/// thread, which is what `CPUCLOCK_PID(clock) == 0` means in Linux.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum CpuClockOwner {
    /// A whole process (a thread group), by pid.
    Process(usize),
    /// One thread, by tid.
    Thread(usize),
}

/// Which of a task's times a CPU clock adds up: Linux's `CPUCLOCK_PROF`,
/// `CPUCLOCK_VIRT` and `CPUCLOCK_SCHED`, in that order.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum CpuClockKind {
    /// User plus kernel time (`utime + stime`).
    Prof,
    /// User time only.
    Virt,
    /// Time on the CPU as the scheduler counts it (`sum_exec_runtime`).
    Sched,
}

impl CpuClockKind {
    /// The clock's reading, from a task's user and kernel nanoseconds. The
    /// scheduler's count is the two added, the only account this kernel
    /// keeps of time on the CPU.
    pub fn value_ns(self, utime_ns: u64, stime_ns: u64) -> u64 {
        match self {
            Self::Virt => utime_ns,
            Self::Prof | Self::Sched => utime_ns.saturating_add(stime_ns),
        }
    }
}

/// A CPU-time clock: whose time, and which of their times.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct CpuClock {
    /// Whose.
    pub owner: CpuClockOwner,
    /// Which.
    pub kind: CpuClockKind,
}

impl CpuClock {
    /// The CPU clock `raw` names; `Ok(None)` when it names a system clock
    /// (or nothing) instead, and the error Linux gives for a CPU clock id
    /// that is malformed.
    ///
    /// `clockid_t` is a 32-bit `int`, and Linux spells the CPU clocks in two
    /// ways (`include/linux/posix-timers.h`): `CLOCK_PROCESS_CPUTIME_ID` (2)
    /// and `CLOCK_THREAD_CPUTIME_ID` (3) for the caller's own, and every
    /// **negative** id, which is what `clock_getcpuclockid(3)` and
    /// `pthread_getcpuclockid(3)` hand back: `~pid << 3 | kind`, with bit 2
    /// set for a thread. Bits 0-2 all set (`CLOCKFD`) is a dynamic clock, a
    /// PTP device's fd, which this kernel has none of, so `EBADF`, as
    /// `get_clock_desc` answers for an fd that is not one; a kind of 3 on a
    /// thread is `EINVAL`, from `pid_for_clock`.
    pub fn from_raw(raw: usize) -> crate::error::LxResult<Option<Self>> {
        use crate::error::LxError;
        // The register carries the `int` zero- or sign-extended: read the
        // low 32 bits either way.
        let id = raw as u32 as i32;
        if id >= 0 {
            return Ok(match id {
                2 => Some(Self {
                    owner: CpuClockOwner::Process(0),
                    kind: CpuClockKind::Sched,
                }),
                3 => Some(Self {
                    owner: CpuClockOwner::Thread(0),
                    kind: CpuClockKind::Sched,
                }),
                _ => None,
            });
        }
        const CLOCKFD: i32 = 3;
        const CLOCKFD_MASK: i32 = 7;
        if id & CLOCKFD_MASK == CLOCKFD {
            return Err(LxError::EBADF);
        }
        let kind = match id & 3 {
            0 => CpuClockKind::Prof,
            1 => CpuClockKind::Virt,
            2 => CpuClockKind::Sched,
            _ => return Err(LxError::EINVAL),
        };
        // `CPUCLOCK_PID`: the complement of what is left above the three
        // low bits, with the sign kept while shifting.
        let pid = !(id >> 3) as usize;
        let owner = if id & 4 != 0 {
            CpuClockOwner::Thread(pid)
        } else {
            CpuClockOwner::Process(pid)
        };
        Ok(Some(Self { owner, kind }))
    }
}

/// Where `clock_gettime(2)` reads a clock from.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ClockSource {
    /// The wall clock, counted from the Unix epoch.
    Wall,
    /// The monotonic clock, counted from boot.
    Monotonic,
    /// A task's CPU time.
    Cpu(CpuClock),
}

/// What `clock_gettime(2)` and `clock_getres(2)` read for `clock`, or
/// `EINVAL` if it is not a clock.
///
/// Every clock `ClockId` knows can be read, whatever `clock_nanosleep` or
/// `timer_create` think of it: `posix_clocks[]` gives each one a
/// `clock_get_timespec`. The two CPU clocks used to be `EINVAL` here, with
/// the CPU accounting that `getrusage(2)` and `times(2)` report sitting
/// right there, so `clock()` in glibc and musl, which is
/// `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)`, returned -1 to every program.
pub fn clock_gettime_source(clock: usize) -> crate::error::LxResult<ClockSource> {
    if let Some(cpu) = CpuClock::from_raw(clock)? {
        return Ok(ClockSource::Cpu(cpu));
    }
    Ok(match ClockId::from_raw(clock)? {
        ClockId::ClockRealTime | ClockId::ClockRealTimeCoarse | ClockId::ClockRealTimeAlarm => {
            ClockSource::Wall
        }
        ClockId::ClockMonotonic
        | ClockId::ClockMonotonicRaw
        | ClockId::ClockMonotonicCoarse
        | ClockId::ClockBootTime
        | ClockId::ClockBootTimeAlarm => ClockSource::Monotonic,
        // Named by number above, before `ClockId` is asked.
        ClockId::ClockProcessCpuTimeId => ClockSource::Cpu(CpuClock {
            owner: CpuClockOwner::Process(0),
            kind: CpuClockKind::Sched,
        }),
        ClockId::ClockThreadCpuTimeId => ClockSource::Cpu(CpuClock {
            owner: CpuClockOwner::Thread(0),
            kind: CpuClockKind::Sched,
        }),
    })
}

/// The CPU clocks of `clock_gettime(2)`, which it refused.
#[cfg(test)]
mod cpu_clock_tests {
    use super::*;
    use crate::error::LxError;
    use CpuClockKind::{Prof, Sched, Virt};
    use CpuClockOwner::{Process, Thread};

    fn cpu(owner: CpuClockOwner, kind: CpuClockKind) -> ClockSource {
        ClockSource::Cpu(CpuClock { owner, kind })
    }

    /// Linux's `MAKE_PROCESS_CPUCLOCK` / `MAKE_THREAD_CPUCLOCK`, as a
    /// 32-bit `clockid_t`.
    fn cpuclock(pid: i32, kind: i32, thread: bool) -> i32 {
        (!pid << 3) | kind | if thread { 4 } else { 0 }
    }

    /// `clock()` in glibc and musl is `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)`,
    /// and Python's `time.thread_time()` is the thread one: both were EINVAL.
    #[test]
    fn two_and_three_are_the_callers_own_cpu_clocks() {
        assert_eq!(clock_gettime_source(2), Ok(cpu(Process(0), Sched)));
        assert_eq!(clock_gettime_source(3), Ok(cpu(Thread(0), Sched)));
        // The negative spellings of the same two, as the kernel defines them.
        assert_eq!(cpuclock(0, 2, false), -6);
        assert_eq!(cpuclock(0, 2, true), -2);
        assert_eq!(
            clock_gettime_source(-6_i32 as u32 as usize),
            Ok(cpu(Process(0), Sched))
        );
        assert_eq!(
            clock_gettime_source(-2_i32 as u32 as usize),
            Ok(cpu(Thread(0), Sched))
        );
    }

    /// What `clock_getcpuclockid(3)` and `pthread_getcpuclockid(3)` hand
    /// back for another task: the pid or tid, and the kind, in one number.
    /// The register may carry the `int` zero-extended (`mov edi`) or
    /// sign-extended: both are read the same.
    #[test]
    fn a_negative_id_names_a_task_and_a_kind() {
        for pid in [1, 77, 1234, 0x0fff_ffff] {
            for (kind, expect) in [(0, Prof), (1, Virt), (2, Sched)] {
                for thread in [false, true] {
                    let id = cpuclock(pid, kind, thread);
                    assert!(id < 0, "{}", id);
                    let owner = if thread {
                        Thread(pid as usize)
                    } else {
                        Process(pid as usize)
                    };
                    for raw in [id as u32 as usize, id as isize as usize] {
                        assert_eq!(
                            clock_gettime_source(raw),
                            Ok(cpu(owner, expect)),
                            "pid {} kind {} thread {} raw {:#x}",
                            pid,
                            kind,
                            thread,
                            raw
                        );
                    }
                }
            }
        }
    }

    /// The two malformed shapes get Linux's two different answers.
    #[test]
    fn a_dynamic_clock_is_ebadf_and_a_fourth_kind_on_a_thread_is_einval() {
        // `CLOCKFD`: low three bits 011 name a PTP device's fd.
        assert_eq!(
            clock_gettime_source(cpuclock(5, 3, false) as u32 as usize),
            Err(LxError::EBADF)
        );
        // Kind 3 with the thread bit: `pid_for_clock` says no such kind.
        assert_eq!(
            clock_gettime_source(cpuclock(5, 3, true) as u32 as usize),
            Err(LxError::EINVAL)
        );
    }

    /// The system clocks read the wall or the monotonic clock, the alarm
    /// pair included: `CLOCK_REALTIME_ALARM` and `CLOCK_BOOTTIME_ALARM` are
    /// readable by anyone in Linux (`alarm_clock_get_timespec`); only a
    /// timer on them wants `CAP_WAKE_ALARM`. They were EINVAL here too.
    #[test]
    fn the_system_clocks_read_the_wall_or_the_monotonic_clock() {
        for clock in [0, 5, 8] {
            assert_eq!(
                clock_gettime_source(clock),
                Ok(ClockSource::Wall),
                "{}",
                clock
            );
            assert_eq!(CpuClock::from_raw(clock), Ok(None));
        }
        for clock in [1, 4, 6, 7, 9] {
            assert_eq!(
                clock_gettime_source(clock),
                Ok(ClockSource::Monotonic),
                "{}",
                clock
            );
            assert_eq!(CpuClock::from_raw(clock), Ok(None));
        }
        for clock in [10, 11, 99, i32::MAX as usize] {
            assert_eq!(
                clock_gettime_source(clock),
                Err(LxError::EINVAL),
                "{}",
                clock
            );
        }
    }

    /// `CPUCLOCK_VIRT` is user time alone; the other two add the kernel's.
    #[test]
    fn virt_is_user_time_and_the_other_two_add_kernel_time() {
        assert_eq!(Virt.value_ns(700, 300), 700);
        assert_eq!(Prof.value_ns(700, 300), 1000);
        assert_eq!(Sched.value_ns(700, 300), 1000);
        assert_eq!(Prof.value_ns(u64::MAX, 1), u64::MAX);
    }
}

#[cfg(test)]
mod time_tests {
    //! Tests for the arithmetic every timeout in the kernel goes through.
    //!
    //! `TimeSpec` and `TimeVal` are the uAPI structs, filled in by userspace
    //! and read here without a copy. In the uAPI both fields are **signed**
    //! (`time_t` and `long`); here they are `usize`, so a negative value
    //! arrives as a very large one. That mismatch is the whole hazard: a
    //! `timespec` of all-ones used to reach `Duration::new`, whose documented
    //! behaviour on a nanosecond carry that overflows the seconds is to
    //! panic -- from any unprivileged process, through `nanosleep`.

    use super::*;

    const NEG_ONE: usize = usize::MAX;

    /// `clock_nanosleep` used to turn both its clock id and its flag word
    /// into Rust enums through an infallible `From<usize>` that ended in
    /// `unreachable!()`, so **any process could panic the kernel** with
    /// `clock_nanosleep(99, 0, &ts, NULL)`. `clock_gettime`, which takes the
    /// same id, has always answered `EINVAL` for one it does not know: two
    /// doors to the same thing and only one of them checked.
    #[test]
    fn a_clock_nanosleep_on_a_clock_that_is_not_one_is_not_a_kernel_panic() {
        use crate::error::LxError;
        for (clock, base) in [
            (0, ClockBase::Wall),
            (1, ClockBase::Monotonic),
            (2, ClockBase::Monotonic),
            (7, ClockBase::Monotonic),
            (8, ClockBase::Wall),
            (9, ClockBase::Monotonic),
        ] {
            assert_eq!(clock_nanosleep_base(clock), Ok(base), "clock {}", clock);
        }
        // CLOCK_THREAD_CPUTIME_ID has an `nsleep` and it says EINVAL.
        assert_eq!(clock_nanosleep_base(3), Err(LxError::EINVAL));
        // These three have no `nsleep` at all, which is a different answer.
        for clock in [4, 5, 6] {
            assert_eq!(
                clock_nanosleep_base(clock),
                Err(LxError::EOPNOTSUPP),
                "clock {}",
                clock
            );
        }
        // Past the end of the table, and every negative id, which reaches
        // this kernel as a very large `usize`.
        for clock in [10, 11, 99, usize::MAX, -1_isize as usize, -6_isize as usize] {
            assert_eq!(
                clock_nanosleep_base(clock),
                Err(LxError::EINVAL),
                "clock {}",
                clock
            );
        }
    }

    /// The one that hangs a program rather than killing the kernel: with
    /// `TIMER_ABSTIME` the request is a **point in time**, and both arms of
    /// the old `match` slept for it as though it were a length.
    #[test]
    fn an_absolute_deadline_is_not_slept_as_a_length() {
        let now_mono = Duration::from_secs(3_600);
        let now_wall = Duration::from_secs(1_700_000_000);

        // CLOCK_MONOTONIC: the request is already on the timeline we wake on.
        assert_eq!(
            plan_clock_nanosleep(
                1,
                TIMER_ABSTIME,
                now_mono + Duration::from_secs(2),
                now_mono,
                now_wall
            ),
            Ok(SleepPlan::Until(now_mono + Duration::from_secs(2)))
        );

        // CLOCK_REALTIME: the request counts from the epoch, so only the
        // distance from wall-clock now goes on the monotonic timeline. The
        // old code slept for the whole epoch value: fifty-odd years.
        assert_eq!(
            plan_clock_nanosleep(
                0,
                TIMER_ABSTIME,
                now_wall + Duration::from_secs(2),
                now_mono,
                now_wall
            ),
            Ok(SleepPlan::Until(now_mono + Duration::from_secs(2)))
        );
    }

    /// An absolute deadline that has already gone by returns at once. Linux
    /// does not sleep, and it does not fail either.
    #[test]
    fn an_absolute_deadline_already_gone_by_returns_at_once() {
        let now_mono = Duration::from_secs(3_600);
        let now_wall = Duration::from_secs(1_700_000_000);
        for (clock, request) in [
            (1, now_mono),
            (1, Duration::ZERO),
            (0, now_wall),
            (0, Duration::from_secs(1)),
        ] {
            assert_eq!(
                plan_clock_nanosleep(clock, TIMER_ABSTIME, request, now_mono, now_wall),
                Ok(SleepPlan::AlreadyPast),
                "clock {} request {:?}",
                clock,
                request
            );
        }
    }

    /// Without the flag the request is a length, and the clock's own epoch
    /// does not come into it.
    #[test]
    fn a_relative_sleep_counts_from_now_whatever_the_clock() {
        let now_mono = Duration::from_secs(3_600);
        let now_wall = Duration::from_secs(1_700_000_000);
        for clock in [0, 1, 2, 7, 8, 9] {
            assert_eq!(
                plan_clock_nanosleep(clock, 0, Duration::from_millis(250), now_mono, now_wall),
                Ok(SleepPlan::Until(now_mono + Duration::from_millis(250))),
                "clock {}",
                clock
            );
        }
        // CLOCK_BOOTTIME used to fall into an empty arm and return without
        // sleeping at all, which turns a one-second sleep into a busy loop.
        assert_ne!(
            plan_clock_nanosleep(7, 0, Duration::from_secs(1), now_mono, now_wall),
            Ok(SleepPlan::AlreadyPast)
        );
    }

    /// `common_nsleep` does `flags & TIMER_ABSTIME`, so a flag word with
    /// other bits in it is a relative sleep in Linux, not an error and
    /// certainly not a panic.
    #[test]
    fn only_the_abstime_bit_of_the_flag_word_is_looked_at() {
        let now_mono = Duration::from_secs(3_600);
        let now_wall = Duration::from_secs(1_700_000_000);
        let relative = Ok(SleepPlan::Until(now_mono + Duration::from_secs(5)));
        for flags in [0, 2, 4, 0x8000, usize::MAX ^ 1] {
            assert_eq!(
                plan_clock_nanosleep(1, flags, Duration::from_secs(5), now_mono, now_wall),
                relative,
                "flags {:#x}",
                flags
            );
        }
        for flags in [1, 3, 5, usize::MAX] {
            assert_eq!(
                plan_clock_nanosleep(
                    1,
                    flags,
                    now_mono + Duration::from_secs(5),
                    now_mono,
                    now_wall
                ),
                relative,
                "flags {:#x}",
                flags
            );
        }
    }

    /// The clock is checked before the deadline is worked out, so a bad id
    /// is refused whether the request is absolute or relative.
    #[test]
    fn the_clock_is_refused_before_anything_is_computed() {
        use crate::error::LxError;
        for flags in [0, TIMER_ABSTIME] {
            assert_eq!(
                plan_clock_nanosleep(
                    5,
                    flags,
                    Duration::from_secs(1),
                    Duration::ZERO,
                    Duration::ZERO
                ),
                Err(LxError::EOPNOTSUPP)
            );
            assert_eq!(
                plan_clock_nanosleep(
                    99,
                    flags,
                    Duration::from_secs(1),
                    Duration::ZERO,
                    Duration::ZERO
                ),
                Err(LxError::EINVAL)
            );
        }
    }

    #[test]
    fn a_timespec_from_userspace_can_never_panic_the_conversion() {
        // Every bit pattern userspace can put in the struct has to come out
        // as *some* duration. This is the one that used to take the kernel
        // down: tv_sec = -1, tv_nsec = -1.
        let d: Duration = TimeSpec {
            sec: NEG_ONE,
            nsec: NEG_ONE,
        }
        .into();
        assert_eq!(d, Duration::MAX);
        let _: Duration = TimeSpec {
            sec: NEG_ONE,
            nsec: 0,
        }
        .into();
        let _: Duration = TimeSpec {
            sec: 0,
            nsec: NEG_ONE,
        }
        .into();
        let _: Duration = TimeVal {
            sec: NEG_ONE,
            usec: NEG_ONE,
        }
        .into();
    }

    #[test]
    fn an_out_of_range_timespec_is_refused_rather_than_slept_on() {
        // What a syscall owes userspace is EINVAL, not a sleep of some other
        // length. Linux checks exactly these two things.
        assert!(TimeSpec {
            sec: 1,
            nsec: NSEC_PER_SEC
        }
        .try_into_duration()
        .is_err());
        assert!(TimeSpec {
            sec: NEG_ONE,
            nsec: 0
        }
        .try_into_duration()
        .is_err());
        assert!(TimeSpec {
            sec: 1,
            nsec: NEG_ONE
        }
        .try_into_duration()
        .is_err());
    }

    #[test]
    fn the_refusal_is_einval_and_not_some_other_error() {
        // `nanosleep` returning the wrong errno sends glibc down a different
        // path; EINTR in particular would have it retry for ever.
        assert!(matches!(
            TimeSpec {
                sec: 0,
                nsec: NSEC_PER_SEC
            }
            .try_into_duration(),
            Err(crate::error::LxError::EINVAL)
        ));
    }

    #[test]
    fn the_last_nanosecond_of_a_second_is_still_valid() {
        // The boundary is exclusive: 999_999_999 is the largest legal value
        // and rejecting it would break every `sleep 0.999999999`.
        assert!(TimeSpec {
            sec: 0,
            nsec: NSEC_PER_SEC - 1
        }
        .valid());
        assert!(!TimeSpec {
            sec: 0,
            nsec: NSEC_PER_SEC
        }
        .valid());
    }

    #[test]
    fn a_zero_timespec_is_valid_and_means_no_wait() {
        // `nanosleep({0,0})` is a legal way to yield, and `timer_settime`
        // uses an all-zero value to disarm. Refusing it would break both.
        let z = TimeSpec { sec: 0, nsec: 0 };
        assert!(z.valid());
        assert_eq!(z.try_into_duration().unwrap(), Duration::ZERO);
    }

    #[test]
    fn the_largest_positive_seconds_value_is_accepted() {
        // A `time_t` is signed, so the largest legal `tv_sec` is
        // `isize::MAX`. It is roughly 292 billion years, and programs do use
        // it as "wait for ever".
        let forever = TimeSpec {
            sec: isize::MAX as usize,
            nsec: 0,
        };
        assert!(forever.valid());
        assert!(forever.try_into_duration().is_ok());
        // One more and the sign bit is set, which is a negative time_t.
        assert!(!TimeSpec {
            sec: isize::MAX as usize + 1,
            nsec: 0
        }
        .valid());
    }

    #[test]
    fn a_valid_timespec_converts_to_exactly_the_duration_it_names() {
        assert_eq!(
            TimeSpec {
                sec: 3,
                nsec: 500_000_000
            }
            .try_into_duration()
            .unwrap(),
            Duration::from_millis(3_500)
        );
        assert_eq!(
            TimeSpec { sec: 0, nsec: 1 }.try_into_duration().unwrap(),
            Duration::from_nanos(1)
        );
    }

    #[test]
    fn the_two_conversions_agree_wherever_both_are_defined() {
        // The infallible one saturates and the checked one refuses, but on a
        // timespec userspace is allowed to send they must give the same
        // answer, or a syscall would sleep for a different length than the
        // one it validated.
        for &sec in &[0usize, 1, 2, 1_000, 1 << 40] {
            for &nsec in &[0usize, 1, 999, 500_000_000, NSEC_PER_SEC - 1] {
                let t = TimeSpec { sec, nsec };
                assert_eq!(
                    Duration::from(t),
                    t.try_into_duration().unwrap(),
                    "sec {} nsec {}",
                    sec,
                    nsec
                );
            }
        }
    }

    #[test]
    fn a_huge_timeout_stays_finite_instead_of_meaning_for_ever() {
        // `poll` and `select` spell "wait for ever" as a negative isize. The
        // milliseconds are a usize, so anything from 2^63 up casts to a
        // negative value and the caller silently gets an infinite wait with
        // no way to tell why. A legal `tv_sec` reaches that easily: the
        // field is a signed 64-bit time_t, and 2^63 milliseconds is only
        // about 2^53 seconds.
        let huge = TimeSpec {
            sec: 1 << 60,
            nsec: 0,
        };
        assert!(huge.valid(), "this is a timespec userspace may send");
        let ms = huge.try_into_poll_msecs().unwrap();
        assert!(ms > 0, "a finite wait came out as {}", ms);
        assert_eq!(ms, isize::MAX, "and is clamped, not wrapped");
        // The largest legal one, too.
        let ms = TimeSpec {
            sec: isize::MAX as usize,
            nsec: 0,
        }
        .try_into_poll_msecs()
        .unwrap();
        assert!(ms > 0);
        // `select(2)` takes a timeval and has exactly the same hazard.
        let huge = TimeVal {
            sec: 1 << 60,
            usec: 0,
        };
        assert!(huge.valid());
        let ms = huge.try_into_poll_msecs().unwrap();
        assert!(ms > 0, "a finite wait came out as {}", ms);
        assert_eq!(ms, isize::MAX);
    }

    #[test]
    fn an_out_of_range_timeout_is_refused_by_the_poll_conversion_too() {
        assert!(TimeSpec {
            sec: NEG_ONE,
            nsec: 0
        }
        .try_into_poll_msecs()
        .is_err());
        assert!(TimeSpec {
            sec: 0,
            nsec: NSEC_PER_SEC
        }
        .try_into_poll_msecs()
        .is_err());
        assert!(TimeVal {
            sec: NEG_ONE,
            usec: 0
        }
        .try_into_poll_msecs()
        .is_err());
        assert!(TimeVal {
            sec: 0,
            usec: USEC_PER_SEC
        }
        .try_into_poll_msecs()
        .is_err());
    }

    #[test]
    fn an_ordinary_timeout_converts_to_the_milliseconds_it_names() {
        // Zero has to stay zero: `poll` reads it as "check and return now",
        // and turning it into anything else makes a non-blocking poll block.
        assert_eq!(
            TimeSpec { sec: 0, nsec: 0 }.try_into_poll_msecs().unwrap(),
            0
        );
        assert_eq!(
            TimeSpec {
                sec: 0,
                nsec: 100_000_000
            }
            .try_into_poll_msecs()
            .unwrap(),
            100
        );
        assert_eq!(
            TimeVal {
                sec: 2,
                usec: 500_000
            }
            .try_into_poll_msecs()
            .unwrap(),
            2_500
        );
    }

    #[test]
    fn a_timeval_converts_to_exactly_the_duration_it_names() {
        assert_eq!(
            TimeVal {
                sec: 1,
                usec: 250_000
            }
            .try_into_duration()
            .unwrap(),
            Duration::from_millis(1_250)
        );
        assert!(TimeVal {
            sec: 0,
            usec: USEC_PER_SEC
        }
        .try_into_duration()
        .is_err());
        // And agrees with the infallible conversion where both are defined.
        for &(sec, usec) in &[(0usize, 0usize), (1, 1), (7, 999_999), (1 << 40, 500_000)] {
            let t = TimeVal { sec, usec };
            assert_eq!(Duration::from(t), t.try_into_duration().unwrap());
        }
    }

    #[test]
    fn a_timeval_uses_the_same_rule_in_microseconds() {
        assert!(TimeVal {
            sec: 0,
            usec: USEC_PER_SEC - 1
        }
        .valid());
        assert!(!TimeVal {
            sec: 0,
            usec: USEC_PER_SEC
        }
        .valid());
        assert!(!TimeVal {
            sec: NEG_ONE,
            usec: 0
        }
        .valid());
    }

    #[test]
    fn to_msec_does_not_overflow_on_a_hostile_value() {
        // `sec * 1000` wraps silently in release and panics in debug. Both
        // are reachable from a `poll` timeout.
        assert_eq!(
            TimeSpec {
                sec: NEG_ONE,
                nsec: NEG_ONE
            }
            .to_msec(),
            usize::MAX
        );
        assert_eq!(
            TimeVal {
                sec: NEG_ONE,
                usec: NEG_ONE
            }
            .to_msec(),
            usize::MAX
        );
    }

    #[test]
    fn to_msec_truncates_rather_than_rounds() {
        // Rounding up would make a zero-length poll timeout block for a
        // millisecond; rounding a 1999-microsecond wait to 2 ms is also
        // wrong, but truncation is what every caller assumes.
        assert_eq!(
            TimeSpec {
                sec: 0,
                nsec: 999_999
            }
            .to_msec(),
            0
        );
        assert_eq!(
            TimeSpec {
                sec: 2,
                nsec: 1_999_999
            }
            .to_msec(),
            2_001
        );
        assert_eq!(TimeVal { sec: 1, usec: 999 }.to_msec(), 1_000);
    }

    #[test]
    fn a_timespec_narrowed_to_a_timeval_keeps_its_seconds() {
        // `sec` is copied straight across; only the sub-second part loses
        // resolution. Dividing the seconds too would be a 1000x error.
        let v: TimeVal = TimeSpec {
            sec: 7,
            nsec: 123_456_789,
        }
        .into();
        assert_eq!((v.sec, v.usec), (7, 123_456));
    }

    #[test]
    fn a_duration_round_trips_through_a_timeval() {
        for ms in [0u64, 1, 999, 1_000, 1_001, 123_456] {
            let d = Duration::from_millis(ms);
            let v: TimeVal = d.into();
            assert_eq!(Duration::from(v), d, "{} ms", ms);
        }
    }

    #[test]
    fn a_duration_keeps_its_seconds_and_its_fraction_apart() {
        // `subsec_micros`, not `as_micros`: the latter is the *whole*
        // duration, and putting it in the microseconds field would make
        // every timeval a thousand times too long.
        let v: TimeVal = Duration::from_micros(2_500_000).into();
        assert_eq!((v.sec, v.usec), (2, 500_000));
    }

    #[test]
    fn a_duration_below_a_microsecond_does_not_become_a_whole_one() {
        let v: TimeVal = Duration::from_nanos(999).into();
        assert_eq!((v.sec, v.usec), (0, 0));
    }

    #[test]
    fn from_duration_splits_a_timespec_the_same_way() {
        let t = TimeSpec::from_duration(Duration::new(5, 999_999_999));
        assert_eq!((t.sec, t.nsec), (5, 999_999_999));
        assert!(t.valid(), "a value we produced must be one we accept");
    }

    #[test]
    fn everything_we_produce_is_something_we_accept() {
        // A value the kernel hands back to userspace and then gets handed
        // again (the `rem` of an interrupted sleep, a `timer_gettime`) has
        // to survive the round trip.
        for ns in [0u64, 1, 999_999_999, 1_000_000_000, 1_500_000_000, 1 << 40] {
            let t = TimeSpec::from_duration(Duration::from_nanos(ns));
            assert!(t.valid(), "{} ns produced an invalid timespec", ns);
            assert_eq!(t.try_into_duration().unwrap(), Duration::from_nanos(ns));
        }
    }

    /// A plausible wall clock: seconds since 1970 as this machine would read
    /// them. The whole hazard is how far this is from a plausible uptime.
    const NOW_WALL: Duration = Duration::from_secs(1_789_000_000);
    /// A plausible uptime: forty-two seconds since boot.
    const NOW_MONO: Duration = Duration::from_secs(42);

    /// `timerfd_create`'s `clockid` was taken, logged and dropped, so every
    /// timerfd ran on the monotonic clock. Linux names the five clocks a
    /// timerfd may use and answers EINVAL for the rest.
    #[test]
    fn a_timerfd_may_only_be_made_on_a_clock_a_timerfd_can_run_on() {
        use crate::error::LxError;
        for (clock, base) in [
            (0, ClockBase::Wall),      // CLOCK_REALTIME
            (1, ClockBase::Monotonic), // CLOCK_MONOTONIC
            (7, ClockBase::Monotonic), // CLOCK_BOOTTIME
            (8, ClockBase::Wall),      // CLOCK_REALTIME_ALARM
            (9, ClockBase::Monotonic), // CLOCK_BOOTTIME_ALARM
        ] {
            assert_eq!(timerfd_clock_base(clock), Ok(base), "clock {}", clock);
        }
        // The CPU clocks and the coarse ones are not on `timerfd_create`'s
        // list at all, so they are EINVAL and not EOPNOTSUPP.
        for clock in [2, 3, 4, 5, 6] {
            assert_eq!(
                timerfd_clock_base(clock),
                Err(LxError::EINVAL),
                "clock {}",
                clock
            );
        }
        assert_eq!(timerfd_clock_base(10), Err(LxError::EINVAL));
        // A negative id arrives as a very large `usize`.
        assert_eq!(timerfd_clock_base(NEG_ONE), Err(LxError::EINVAL));
    }

    /// `timer_create` took its clock id as `_clockid` — named out of the
    /// compiler's way and never read. Linux has three answers here, not two:
    /// a clock it does not know is EINVAL, and one whose `k_clock` carries no
    /// `timer_create` is EOPNOTSUPP.
    #[test]
    fn a_posix_timer_may_only_be_made_on_a_clock_that_can_carry_one() {
        use crate::error::LxError;
        for (clock, base) in [
            (0, ClockBase::Wall),
            (1, ClockBase::Monotonic),
            // Unlike `nsleep`, both CPU clocks do carry a `timer_create`.
            (2, ClockBase::Monotonic),
            (3, ClockBase::Monotonic),
            (7, ClockBase::Monotonic),
            (8, ClockBase::Wall),
            (9, ClockBase::Monotonic),
        ] {
            assert_eq!(posix_timer_clock_base(clock), Ok(base), "clock {}", clock);
        }
        for clock in [4, 5, 6] {
            assert_eq!(
                posix_timer_clock_base(clock),
                Err(LxError::EOPNOTSUPP),
                "clock {}",
                clock
            );
        }
        assert_eq!(posix_timer_clock_base(10), Err(LxError::EINVAL));
        assert_eq!(posix_timer_clock_base(NEG_ONE), Err(LxError::EINVAL));
        // `clock_nanosleep` is the one that says EINVAL for the thread CPU
        // clock; the two answers are different on purpose.
        assert_eq!(clock_nanosleep_base(3), Err(LxError::EINVAL));
    }

    /// The bug this whole vein is about: `timerfd_settime(TFD_TIMER_ABSTIME)`
    /// and `timer_settime(TIMER_ABSTIME)` both took the caller's absolute
    /// time and handed it to the kernel timer as a monotonic deadline. On
    /// `CLOCK_REALTIME` that is seconds since 1970 read as nanoseconds since
    /// boot: the timer is armed more than fifty years out and never fires.
    #[test]
    fn an_absolute_wall_clock_deadline_is_a_distance_not_a_date() {
        let deadline = timer_arm_deadline(
            ClockBase::Wall,
            true,
            NOW_WALL + Duration::from_secs(5),
            NOW_MONO,
            NOW_WALL,
        );
        assert_eq!(deadline, NOW_MONO + Duration::from_secs(5));
        // What the tree used to arm instead, for the same call.
        assert!(
            NOW_WALL + Duration::from_secs(5) > NOW_MONO + Duration::from_secs(50 * 31_557_600),
            "the old deadline was not the half-century it looked like"
        );
    }

    /// An absolute deadline on a monotonic clock IS a monotonic deadline, so
    /// the fix must not move it. Half the callers in the tree (libwayland's
    /// frame timers) are exactly this, and they worked.
    #[test]
    fn an_absolute_monotonic_deadline_is_taken_as_it_stands() {
        let want = NOW_MONO + Duration::from_millis(500);
        assert_eq!(
            timer_arm_deadline(ClockBase::Monotonic, true, want, NOW_MONO, NOW_WALL),
            want
        );
    }

    /// Without TFD_TIMER_ABSTIME the value is a length, and a length means
    /// the same thing on either clock.
    #[test]
    fn a_relative_arm_is_counted_from_now_whatever_the_clock() {
        for base in [ClockBase::Monotonic, ClockBase::Wall] {
            assert_eq!(
                timer_arm_deadline(base, false, Duration::from_secs(3), NOW_MONO, NOW_WALL),
                NOW_MONO + Duration::from_secs(3),
                "{:?}",
                base
            );
        }
    }

    /// An absolute deadline that has already gone by fires at once in Linux
    /// (one expiration, straight away), which here means a monotonic
    /// deadline of `now` — `timer_set` serves a past deadline as soon as it
    /// can. Answering with the past value itself would be the same thing;
    /// answering with `now + value` would be a timer armed for a second time
    /// around, which is what a relative reading of it does.
    #[test]
    fn an_absolute_deadline_already_gone_by_is_due_now_not_never() {
        for (base, past) in [
            (ClockBase::Wall, NOW_WALL - Duration::from_secs(60)),
            (ClockBase::Monotonic, NOW_MONO - Duration::from_secs(10)),
        ] {
            assert_eq!(
                timer_arm_deadline(base, true, past, NOW_MONO, NOW_WALL),
                NOW_MONO,
                "{:?}",
                base
            );
        }
    }

    /// The `it_value` is a `timespec` filled in by userspace, so the seconds
    /// can be anything a `time_t` holds. Neither the subtraction nor the
    /// addition may wrap or panic: `Duration`'s `+` panics on overflow, and
    /// this arithmetic runs inside `timerfd_settime`.
    #[test]
    fn an_absurd_absolute_deadline_saturates_instead_of_overflowing() {
        let huge = Duration::from_secs(u64::MAX);
        assert_eq!(
            timer_arm_deadline(ClockBase::Wall, true, huge, NOW_MONO, NOW_WALL),
            NOW_MONO.saturating_add(huge - NOW_WALL)
        );
        // And the relative arm, which adds without subtracting first.
        assert_eq!(
            timer_arm_deadline(ClockBase::Monotonic, false, huge, NOW_MONO, NOW_WALL),
            Duration::MAX
        );
    }

    /// A `CLOCK_BOOTTIME` timer is armed against this kernel's monotonic
    /// timer, which does not stop for suspend because there is no suspend.
    /// If that ever changes, boot time stops being monotonic time and both
    /// functions have to say so here rather than somewhere else.
    #[test]
    fn boot_time_is_monotonic_time_in_this_kernel() {
        assert_eq!(timerfd_clock_base(7), Ok(ClockBase::Monotonic));
        assert_eq!(posix_timer_clock_base(7), Ok(ClockBase::Monotonic));
        assert_eq!(clock_nanosleep_base(7), Ok(ClockBase::Monotonic));
    }
}
