//! Linux time objects

use alloc::sync::Arc;
use core::time::Duration;
use rcore_fs::vfs::*;

/// TimeSpec struct for clock_gettime, similar to Timespec
#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
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

/// Clock id
#[derive(Debug)]
#[repr(usize)]
pub enum ClockId {
    /// missing documentation
    ClockRealTime = 0,
    /// missing documentation
    ClockMonotonic = 1,
    /// missing documentation
    ClockProcessCpuTimeId = 2,
    /// missing documentation
    ClockThreadCpuTimeId = 3,
    /// missing documentation
    ClockMonotonicRaw = 4,
    /// missing documentation
    ClockRealTimeCoarse = 5,
    /// missing documentation
    ClockMonotonicCoarse = 6,
    /// missing documentation
    ClockBootTime = 7,
    /// missing documentation
    ClockRealTimeAlarm = 8,
    /// missing documentation
    ClockBootTimeAlarm = 9,
}

impl From<usize> for ClockId {
    fn from(t: usize) -> ClockId {
        match t {
            0 => ClockId::ClockRealTime,
            1 => ClockId::ClockMonotonic,
            2 => ClockId::ClockProcessCpuTimeId,
            3 => ClockId::ClockThreadCpuTimeId,
            4 => ClockId::ClockMonotonicRaw,
            5 => ClockId::ClockRealTimeCoarse,
            6 => ClockId::ClockMonotonicCoarse,
            7 => ClockId::ClockBootTime,
            8 => ClockId::ClockRealTimeAlarm,
            9 => ClockId::ClockBootTimeAlarm,
            _ => unreachable!(),
        }
    }
}

/// Clock Flags
#[derive(Debug)]
#[repr(usize)]
pub enum ClockFlags {
    /// missing documentation
    ZeroFlag = 0,
    /// missing documentation
    TimerAbsTime = 1,
}

impl From<usize> for ClockFlags {
    fn from(t: usize) -> ClockFlags {
        match t {
            0 => ClockFlags::ZeroFlag,
            1 => ClockFlags::TimerAbsTime,
            _ => unreachable!(),
        }
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
}
