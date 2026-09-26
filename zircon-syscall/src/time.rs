use {
    super::*,
    core::{
        fmt::{Debug, Formatter, Result},
        sync::atomic::{AtomicI64, Ordering},
        time::Duration,
    },
    kernel_hal::timer::timer_now,
    zircon_object::{dev::*, object::Clock, task::*},
};

/// How far UTC is from the monotonic clock, in nanoseconds.
///
/// Zircon's `zx_clock_adjust` takes an `int64_t`, and the offset is genuinely
/// signed: a machine whose UTC is behind its boot clock has a negative one.
/// This used to be a `u64` fed straight from the syscall's raw argument word,
/// so `zx_clock_adjust(ZX_CLOCK_UTC, -1000)` stored `0xFFFF_FFFF_FFFF_FC18` and
/// the next `zx_clock_get(ZX_CLOCK_UTC)` added it to the monotonic reading --
/// an overflowing `u64 + u64`, which is a panic in a debug kernel and a wrapped
/// clock in release.
static UTC_OFFSET: AtomicI64 = AtomicI64::new(0);

const ZX_CLOCK_MONOTONIC: u32 = 0;
const ZX_CLOCK_UTC: u32 = 1;
const ZX_CLOCK_THREAD: u32 = 2;

const ZX_CLOCK_ARGS_VERSION_SHIFT: u64 = 58;
const ZX_CLOCK_ARGS_VERSION_MASK: u64 = 0x3f << ZX_CLOCK_ARGS_VERSION_SHIFT;
const ZX_CLOCK_OPT_MONOTONIC: u64 = 1 << 0;
const ZX_CLOCK_OPT_CONTINUOUS: u64 = 1 << 1;
const ZX_CLOCK_OPT_AUTO_START: u64 = 1 << 2;
const ZX_CLOCK_OPT_BOOT: u64 = 1 << 3;
const ZX_CLOCK_OPT_MAPPABLE: u64 = 1 << 4;
const ZX_CLOCK_OPTS_ALL: u64 = ZX_CLOCK_OPT_MONOTONIC
    | ZX_CLOCK_OPT_CONTINUOUS
    | ZX_CLOCK_OPT_AUTO_START
    | ZX_CLOCK_OPT_BOOT
    | ZX_CLOCK_OPT_MAPPABLE;
const ZX_CLOCK_UPDATE_OPTION_SYNTHETIC_VALUE_VALID: u64 = 1 << 0;
const ZX_CLOCK_UPDATE_OPTION_REFERENCE_VALUE_VALID: u64 = 1 << 3;

#[repr(C)]
#[derive(Clone, Copy)]
struct ClockCreateArgsV1 {
    backstop_time: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ClockUpdateArgsV1 {
    rate_adjust: i32,
    padding: [u8; 4],
    value: i64,
    error_bound: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ClockUpdateArgsV2 {
    rate_adjust: i32,
    padding: [u8; 4],
    synthetic_value: i64,
    reference_value: i64,
    error_bound: u64,
}

impl Syscall<'_> {
    /// Create a new clock object.
    pub fn sys_clock_create(
        &self,
        options: u64,
        user_args: UserInPtr<u8>,
        mut out: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        let version = options >> ZX_CLOCK_ARGS_VERSION_SHIFT;
        if version > 1 || options & !(ZX_CLOCK_ARGS_VERSION_MASK | ZX_CLOCK_OPTS_ALL) != 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        let backstop = match version {
            0 => 0,
            1 => {
                UserInPtr::<ClockCreateArgsV1>::from(user_args.as_addr())
                    .read()?
                    .backstop_time
            }
            _ => unreachable!(),
        };
        let clock = Clock::new(backstop, options & ZX_CLOCK_OPT_MAPPABLE != 0);
        install_handle(
            self.thread.proc(),
            Handle::new(clock, Rights::DEFAULT_CLOCK),
            &mut out,
        )
    }

    /// Acquire the current time.
    ///
    /// + Returns the current time of clock_id via `time`.
    /// + Returns whether `clock_id` was valid.
    pub fn sys_clock_get(&self, clock_id: u32, mut time: UserOutPtr<u64>) -> ZxResult {
        info!("clock.get: id={}", clock_id);
        match clock_id {
            ZX_CLOCK_MONOTONIC => {
                time.write(timer_now().as_nanos() as u64)?;
                Ok(())
            }
            ZX_CLOCK_UTC => {
                time.write(utc_now(
                    timer_now().as_nanos() as u64,
                    UTC_OFFSET.load(Ordering::Relaxed),
                ))?;
                Ok(())
            }
            ZX_CLOCK_THREAD => {
                time.write(self.thread.get_time())?;
                Ok(())
            }
            _ => Err(ZxError::NOT_SUPPORTED),
        }
    }

    /// Perform a basic read of the clock.
    pub fn sys_clock_read(&self, handle: HandleValue, mut now: UserOutPtr<u64>) -> ZxResult {
        info!("clock.read: handle={:#x?}", handle);
        let clock = self
            .thread
            .proc()
            .get_object_with_rights::<Clock>(handle, Rights::READ)?;
        now.write(clock.read() as u64)?;
        Ok(())
    }

    pub fn sys_clock_adjust(&self, resource: HandleValue, clock_id: u32, offset: i64) -> ZxResult {
        info!(
            "clock.adjust: resource={:#x?}, id={:#x}, offset={:#x}",
            resource, clock_id, offset
        );
        let proc = self.thread.proc();
        proc.get_object::<Resource>(resource)?
            .validate(ResourceKind::ROOT)?;
        match clock_id {
            ZX_CLOCK_MONOTONIC => Err(ZxError::ACCESS_DENIED),
            ZX_CLOCK_UTC => {
                UTC_OFFSET.store(offset, Ordering::Relaxed);
                Ok(())
            }
            _ => Err(ZxError::INVALID_ARGS),
        }
    }

    /// Make adjustments to a clock object.
    pub fn sys_clock_update(
        &self,
        handle: HandleValue,
        options: u64,
        user_args: UserInPtr<u8>,
    ) -> ZxResult {
        let clock = self
            .thread
            .proc()
            .get_object_with_rights::<Clock>(handle, Rights::WRITE)?;
        let version = options >> ZX_CLOCK_ARGS_VERSION_SHIFT;
        let flags = options & !ZX_CLOCK_ARGS_VERSION_MASK;
        let now = timer_now().as_nanos() as i64;
        match version {
            1 => {
                let args = UserInPtr::<ClockUpdateArgsV1>::from(user_args.as_addr()).read()?;
                if flags & ZX_CLOCK_UPDATE_OPTION_SYNTHETIC_VALUE_VALID != 0 {
                    clock.update(now, args.value);
                }
            }
            2 => {
                let args = UserInPtr::<ClockUpdateArgsV2>::from(user_args.as_addr()).read()?;
                if flags & ZX_CLOCK_UPDATE_OPTION_SYNTHETIC_VALUE_VALID != 0 {
                    let reference = if flags & ZX_CLOCK_UPDATE_OPTION_REFERENCE_VALUE_VALID != 0 {
                        args.reference_value
                    } else {
                        now
                    };
                    clock.update(reference, args.synthetic_value);
                }
            }
            _ => return Err(ZxError::INVALID_ARGS),
        }
        Ok(())
    }

    /// Sleep for some number of nanoseconds.
    ///
    /// A `deadline` value less than or equal to 0 immediately yields the thread.
    pub async fn sys_nanosleep(&self, deadline: Deadline) -> ZxResult {
        info!("nanosleep: deadline={:?}", deadline);
        if deadline.0 <= 0 {
            kernel_hal::thread::yield_now().await;
        } else {
            let future = kernel_hal::thread::sleep_until(deadline.into());
            pin_mut!(future);
            self.thread
                .blocking_run(
                    future,
                    ThreadState::BlockedSleeping,
                    Deadline::forever().into(),
                    None,
                )
                .await?;
        }
        Ok(())
    }
}

/// UTC as monotonic-plus-offset, saturating at both ends.
///
/// Split out of `sys_clock_get` so the arithmetic can be tested without a
/// thread: it is the one place a signed, user-supplied offset meets an unsigned
/// clock reading. Saturating is the honest answer at both ends -- a UTC before
/// the epoch is reported as the epoch rather than as an enormous time, and a
/// clock pushed past `u64::MAX` stops there rather than wrapping to zero.
fn utc_now(mono_nanos: u64, offset: i64) -> u64 {
    mono_nanos.saturating_add_signed(offset)
}

#[repr(transparent)]
pub struct Deadline(i64);

impl From<usize> for Deadline {
    fn from(x: usize) -> Self {
        Deadline(x as i64)
    }
}

impl Deadline {
    pub fn is_positive(&self) -> bool {
        self.0.is_positive()
    }

    pub fn forever() -> Self {
        Deadline(i64::MAX)
    }
}

impl From<Deadline> for Duration {
    fn from(deadline: Deadline) -> Self {
        Duration::from_nanos(deadline.0.max(0) as u64)
    }
}

impl Debug for Deadline {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        if self.0 <= 0 {
            write!(f, "NoWait")
        } else if self.0 == i64::MAX {
            write!(f, "Forever")
        } else {
            write!(f, "At({:?})", Duration::from_nanos(self.0 as u64))
        }
    }
}

/// The `zx_time_t` contract, which is entirely arithmetic and was entirely
/// untested.
///
/// A Zircon deadline is an `int64_t` nanosecond count on the monotonic clock,
/// and it arrives here inside a raw machine word (`lib.rs` passes `a_n.into()`).
/// Two of its values are load-bearing sentinels -- `ZX_TIME_INFINITE` is
/// `i64::MAX` and `ZX_TIME_INFINITE_PAST` is `i64::MIN` -- and the conversion to
/// the `Duration` the scheduler sleeps on has to keep both meanings: wait
/// forever, and do not wait at all.
#[cfg(test)]
mod deadline_tests {
    use super::*;
    use alloc::format;

    #[test]
    fn a_deadline_keeps_its_sign_through_a_machine_word() {
        // The trip is `i64 as usize as i64`, which is only lossless while a
        // machine word is 64 bits wide. Every negative deadline -- the whole
        // "already passed, fire now" half of the range -- depends on it.
        for want in [
            0i64,
            1,
            -1,
            1_000_000_000,
            -1_000_000_000,
            i64::MAX,
            i64::MIN,
        ] {
            let d = Deadline::from(want as usize);
            assert_eq!(d.0, want, "{} did not survive the argument word", want);
        }
    }

    #[test]
    fn forever_is_the_infinite_deadline_and_waits_the_longest() {
        let forever = Deadline::forever();
        assert_eq!(forever.0, i64::MAX, "ZX_TIME_INFINITE is i64::MAX");
        assert!(forever.is_positive());
        assert_eq!(
            Duration::from(forever),
            Duration::from_nanos(i64::MAX as u64)
        );
        assert_eq!(format!("{:?}", Deadline::forever()), "Forever");
    }

    #[test]
    fn a_deadline_in_the_past_waits_for_nothing_at_all() {
        // ZX_TIME_INFINITE_PAST and any other already-passed deadline must come
        // out as a zero wait, not as a huge one: the caller asked to be told
        // "timed out" immediately. Clamping in the wrong direction turns a poll
        // into a hang of nearly 300 years.
        for past in [i64::MIN, -1_000_000_000, -1, 0] {
            let d = Deadline::from(past as usize);
            assert!(!d.is_positive(), "{} counts as a future deadline", past);
            assert_eq!(
                Duration::from(d),
                Duration::ZERO,
                "{} did not turn into an immediate timeout",
                past
            );
            assert_eq!(format!("{:?}", Deadline::from(past as usize)), "NoWait");
        }
    }

    #[test]
    fn a_future_deadline_is_carried_across_unchanged() {
        let d = Deadline::from(1_500_000_000usize);
        assert!(d.is_positive());
        assert_eq!(Duration::from(d), Duration::from_nanos(1_500_000_000));
        assert_eq!(
            format!("{:?}", Deadline::from(1_500_000_000usize)),
            "At(1.5s)"
        );
    }
}

/// UTC, which is monotonic plus a **signed** offset.
#[cfg(test)]
mod utc_offset_tests {
    use super::*;

    /// A negative offset moves the clock back. It used to be read as `u64` and
    /// added, so this call panicked the kernel in a debug build and wrapped the
    /// clock in release -- reachable by anything holding the root resource.
    #[test]
    fn a_negative_offset_moves_the_clock_back_instead_of_wrapping() {
        assert_eq!(utc_now(5_000, -1_000), 4_000);
        // And the old spelling of it, to show what the fix is about: the raw
        // argument word for -1000 is enormous.
        assert_eq!((-1_000i64) as u64, 0xFFFF_FFFF_FFFF_FC18);
    }

    #[test]
    fn an_offset_of_zero_is_the_monotonic_clock() {
        assert_eq!(utc_now(0, 0), 0);
        assert_eq!(utc_now(u64::MAX, 0), u64::MAX);
    }

    #[test]
    fn a_clock_pushed_out_of_range_stops_at_the_end_rather_than_wrapping() {
        // Before the epoch: report the epoch, not 2^64 minus a bit.
        assert_eq!(utc_now(100, -1_000), 0);
        assert_eq!(utc_now(0, i64::MIN), 0);
        // And past the end of the range: stop there.
        assert_eq!(utc_now(u64::MAX, 1), u64::MAX);
        assert_eq!(utc_now(u64::MAX - 1, i64::MAX), u64::MAX);
    }
}
