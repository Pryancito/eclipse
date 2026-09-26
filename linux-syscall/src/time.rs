//! Syscalls for time
//! - clock_gettime
//!
use crate::outparams::commit_and_report_old;
use crate::Syscall;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use core::convert::TryFrom;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::time::Duration;
use kernel_hal::{user::UserInOutPtr, user::UserInPtr, user::UserOutPtr};
use lazy_static::lazy_static;
use linux_object::error::{LxError, SysResult};
use linux_object::process::ProcessExt;
use linux_object::process::CAP_SYS_TIME;
use linux_object::signal::{SigInfo, Signal};
use linux_object::thread::ThreadExt;
use linux_object::time::*;
use lock::Mutex;
use zircon_object::object::{KernelObject, KoID};
use zircon_object::task::{Status, Thread, ROOT_JOB};

const USEC_PER_TICK: usize = 10000;

/// Linux `struct timex` (x86_64 / LP64): `adjtimex(2)` / `clock_adjtime(2)`.
/// Layout is 208 bytes (modes u32 + pad + longs + timeval + PPS + TAI + reserved).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Timex {
    /// Mode selector (`ADJ_*` bits).
    pub modes: u32,
    /// Time offset (µs, or ns if `STA_NANO`).
    pub offset: i64,
    /// Frequency offset (scaled ppm, 2^-16 ppm units).
    pub freq: i64,
    /// Maximum error (µs).
    pub maxerror: i64,
    /// Estimated error (µs).
    pub esterror: i64,
    /// Clock command/status (`STA_*` bits).
    pub status: i32,
    /// PLL time constant.
    pub constant: i64,
    /// Clock precision (µs, read-only).
    pub precision: i64,
    /// Frequency tolerance (scaled ppm, read-only).
    pub tolerance: i64,
    /// Current time (read-only except `ADJ_SETOFFSET`).
    pub time: TimeValI64,
    /// µs between clock ticks.
    pub tick: i64,
    /// PPS frequency (scaled ppm, read-only).
    pub ppsfreq: i64,
    /// PPS jitter (µs, read-only).
    pub jitter: i64,
    /// Interval duration (s, shift, read-only).
    pub shift: i32,
    /// PPS stability (scaled ppm, read-only).
    pub stabil: i64,
    /// Jitter limit exceeded (read-only).
    pub jitcnt: i64,
    /// Calibration intervals (read-only).
    pub calcnt: i64,
    /// Calibration errors (read-only).
    pub errcnt: i64,
    /// Stability limit exceeded (read-only).
    pub stbcnt: i64,
    /// TAI offset (seconds).
    pub tai: i32,
    /// Padding to Linux's 208-byte `struct timex`.
    pub reserved: [i32; 11],
}

/// `struct timeval` with signed fields, as in the kernel `timex.time` member
/// (so `ADJ_SETOFFSET` can step the clock backwards).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TimeValI64 {
    /// Seconds.
    pub sec: i64,
    /// Microseconds, or nanoseconds if `STA_NANO` / `ADJ_NANO`.
    pub usec: i64,
}

// linux/timex.h — modes
const ADJ_OFFSET: u32 = 0x0001;
const ADJ_FREQUENCY: u32 = 0x0002;
const ADJ_MAXERROR: u32 = 0x0004;
const ADJ_ESTERROR: u32 = 0x0008;
const ADJ_STATUS: u32 = 0x0010;
const ADJ_TIMECONST: u32 = 0x0020;
const ADJ_TAI: u32 = 0x0080;
const ADJ_SETOFFSET: u32 = 0x0100;
const ADJ_MICRO: u32 = 0x1000;
const ADJ_NANO: u32 = 0x2000;
const ADJ_TICK: u32 = 0x4000;
const ADJ_OFFSET_SINGLESHOT: u32 = 0x8001;
const ADJ_OFFSET_SS_READ: u32 = 0xa001;

// linux/timex.h — status
const STA_UNSYNC: i32 = 0x0040;
const STA_NANO: i32 = 0x2000;

// linux/timex.h — return codes
const TIME_OK: usize = 0;
const TIME_ERROR: usize = 5;

/// NTP PLL knobs persisted across `adjtimex` calls. Frequency is recorded
/// (so ntpd's read-back matches what it wrote) but not slewed continuously;
/// offsets and `ADJ_SETOFFSET` step `CLOCK_REALTIME` immediately.
struct NtpState {
    freq: i64,
    maxerror: i64,
    esterror: i64,
    status: i32,
    constant: i64,
    tick: i64,
    tai: i32,
    /// Leftover µs/ns from `adjtime(3)` / `ADJ_OFFSET_SINGLESHOT`. Applied
    /// immediately, so this reads back as 0 after a set.
    offset_remain: i64,
    nano: bool,
}

impl Default for NtpState {
    fn default() -> Self {
        Self {
            freq: 0,
            maxerror: 16_000_000, // 16 s, Linux's unsync default
            esterror: 16_000_000,
            status: STA_UNSYNC,
            constant: 7,
            tick: 10_000, // USER_HZ=100
            tai: 0,
            offset_remain: 0,
            nano: false,
        }
    }
}

lazy_static! {
    static ref NTP_STATE: Mutex<NtpState> = Mutex::new(NtpState::default());
}

fn wall_clock_add_ns(delta_ns: i64) {
    let now = kernel_hal::timer::wall_clock_now();
    let new = if delta_ns >= 0 {
        now.saturating_add(Duration::from_nanos(delta_ns as u64))
    } else {
        let mag = delta_ns.unsigned_abs();
        now.saturating_sub(Duration::from_nanos(mag))
    };
    kernel_hal::timer::wall_clock_set(new);
}

/// `ADJ_ADJTIME`: this call is the old `adjtime(3)`, not the NTP interface.
/// Kernel-internal (`include/linux/timex.h`), not in the uapi header.
const ADJ_ADJTIME: u32 = 0x8000;
/// `ADJ_OFFSET_READONLY`: with `ADJ_ADJTIME`, read the leftover offset rather
/// than set one. Shares a bit with `ADJ_NANO`, which is why it only means
/// anything inside the `ADJ_ADJTIME` branch.
const ADJ_OFFSET_READONLY: u32 = 0x2000;

/// Whether these `modes` ask `adjtimex(2)` to *change* the clock, which is
/// what needs `CAP_SYS_TIME`, as opposed to merely reading its state.
///
/// The three privilege tests of `timekeeping_validate_timex`
/// (`kernel/time/timekeeping.c`), and no more:
///
/// ```c
/// if (txc->modes & ADJ_ADJTIME) {
///         if (!(txc->modes & ADJ_OFFSET_SINGLESHOT))  return -EINVAL;
///         if (!(txc->modes & ADJ_OFFSET_READONLY) &&
///             !capable(CAP_SYS_TIME))                 return -EPERM;
/// } else {
///         /* In order to modify anything, you gotta be super-user! */
///         if (txc->modes && !capable(CAP_SYS_TIME))   return -EPERM;
/// }
/// if (txc->modes & ADJ_SETOFFSET) {
///         /* In order to inject time, you gotta be super-user! */
///         if (!capable(CAP_SYS_TIME))                 return -EPERM;
/// ```
///
/// `modes == 0` is the pure read every program is entitled to, and
/// `ADJ_OFFSET_SS_READ` is the read-only `adjtime(3)`; everything else moves
/// the machine's clock.
fn adjtimex_changes_the_clock(modes: u32) -> bool {
    if modes & ADJ_SETOFFSET != 0 {
        return true;
    }
    if modes & ADJ_ADJTIME != 0 {
        modes & ADJ_OFFSET_READONLY == 0
    } else {
        modes != 0
    }
}

/// Apply a userspace `timex` and fill the read-only / current fields in place.
/// Returns the NTP status code (`TIME_OK` / `TIME_ERROR`), not 0-vs-errno.
fn adjtimex_apply(tx: &mut Timex) -> Result<usize, LxError> {
    let modes = tx.modes;
    // `ADJ_ADJTIME` is `adjtime(3)`, which is a single-shot offset and
    // nothing else: `timekeeping_validate_timex` refuses the bit on its own
    // rather than silently doing nothing with it.
    if modes & ADJ_ADJTIME != 0 && modes & ADJ_OFFSET_SINGLESHOT != ADJ_OFFSET_SINGLESHOT {
        return Err(LxError::EINVAL);
    }
    if modes == ADJ_OFFSET_SS_READ {
        let st = NTP_STATE.lock();
        tx.offset = st.offset_remain;
        fill_timex_readonly(tx, &st);
        return Ok(ntp_time_state(&st));
    }

    let mut st = NTP_STATE.lock();

    if modes & ADJ_NANO != 0 {
        st.nano = true;
    } else if modes & ADJ_MICRO != 0 {
        st.nano = false;
    }

    if modes & ADJ_TICK != 0 {
        // Linux rejects ticks outside [900000/USER_HZ, 1100000/USER_HZ].
        if tx.tick < 9000 || tx.tick > 11000 {
            return Err(LxError::EINVAL);
        }
        st.tick = tx.tick;
    }
    if modes & ADJ_FREQUENCY != 0 {
        st.freq = tx.freq;
    }
    if modes & ADJ_MAXERROR != 0 {
        st.maxerror = tx.maxerror;
    }
    if modes & ADJ_ESTERROR != 0 {
        st.esterror = tx.esterror;
    }
    if modes & ADJ_TIMECONST != 0 {
        st.constant = tx.constant;
    }
    if modes & ADJ_TAI != 0 {
        if tx.tai < 0 {
            return Err(LxError::EINVAL);
        }
        st.tai = tx.tai;
    }
    if modes & ADJ_STATUS != 0 {
        // Userspace may clear STA_UNSYNC once NTP is happy; keep STA_NANO
        // consistent with the resolution bit we own.
        st.status = tx.status & !STA_NANO;
    }

    let singleshot = (modes & ADJ_OFFSET_SINGLESHOT) == ADJ_OFFSET_SINGLESHOT;
    if modes & ADJ_SETOFFSET != 0 {
        let nsec = setoffset_ns(&tx.time, st.nano)?;
        wall_clock_add_ns(nsec);
    } else if singleshot || modes & ADJ_OFFSET != 0 {
        let nsec = if st.nano {
            tx.offset
        } else {
            tx.offset.saturating_mul(1_000)
        };
        wall_clock_add_ns(nsec);
        st.offset_remain = 0;
    }

    if st.nano {
        st.status |= STA_NANO;
    } else {
        st.status &= !STA_NANO;
    }

    fill_timex_readonly(tx, &st);
    Ok(ntp_time_state(&st))
}

fn setoffset_ns(tv: &TimeValI64, nano: bool) -> Result<i64, LxError> {
    let frac = tv.usec;
    let limit = if nano { 1_000_000_000 } else { 1_000_000 };
    if frac <= -limit || frac >= limit {
        return Err(LxError::EINVAL);
    }
    let sec_ns = tv.sec.saturating_mul(1_000_000_000);
    let frac_ns = if nano {
        frac
    } else {
        frac.saturating_mul(1_000)
    };
    Ok(sec_ns.saturating_add(frac_ns))
}

fn fill_timex_readonly(tx: &mut Timex, st: &NtpState) {
    tx.freq = st.freq;
    tx.maxerror = st.maxerror;
    tx.esterror = st.esterror;
    tx.status = st.status;
    tx.constant = st.constant;
    tx.precision = 1;
    tx.tolerance = 32_768_000; // Linux MAXFREQ (500 ppm, scaled)
    tx.tick = st.tick;
    tx.tai = st.tai;
    let now = TimeSpec::now();
    tx.time.sec = now.sec as i64;
    tx.time.usec = if st.nano {
        now.nsec as i64
    } else {
        (now.nsec / 1_000) as i64
    };
}

fn ntp_time_state(st: &NtpState) -> usize {
    if st.status & STA_UNSYNC != 0 {
        TIME_ERROR
    } else {
        TIME_OK
    }
}

impl Syscall<'_> {
    /// finds the resolution (precision) of the specified clock clockid, and,
    /// if buffer is non-NULL, stores it in the struct timespec pointed to by buffer
    pub fn sys_clock_gettime(&self, clock: usize, mut buf: UserOutPtr<TimeSpec>) -> SysResult {
        trace!("clock_gettime: id={:?} buf={:?}", clock, buf);
        if buf.is_null() {
            return Err(LxError::EINVAL);
        }
        let ts = match clock {
            0 | 5 => TimeSpec::now(), // CLOCK_REALTIME, CLOCK_REALTIME_COARSE
            1 | 4 | 6 | 7 => TimeSpec::now_monotonic(),
            _ => return Err(LxError::EINVAL),
        };
        buf.write(ts)?;

        trace!("clock_gettime: {:?}", ts);

        Ok(0)
    }

    /// finds the resolution (precision) of the specified clock, and, if the
    /// buffer is non-NULL, stores it in the struct timespec pointed to by it.
    /// glibc/musl and some applications (e.g. Firefox) treat a garbage or
    /// unwritten resolution as a fatal condition, so always fill the struct.
    pub fn sys_clock_getres(&self, clock: usize, mut buf: UserOutPtr<TimeSpec>) -> SysResult {
        trace!("clock_getres: id={:?} buf={:?}", clock, buf);
        // Reject unknown clocks the same way clock_gettime does.
        match clock {
            0..=7 => {}
            _ => return Err(LxError::EINVAL),
        }
        if buf.is_null() {
            return Ok(0);
        }
        // We service these clocks from a nanosecond-granularity timer source.
        let res = TimeSpec { sec: 0, nsec: 1 };
        buf.write(res)?;
        Ok(0)
    }

    /// set the time of the clock with id clockid
    pub fn sys_clock_settime(&self, clock: usize, timespec: UserInPtr<TimeSpec>) -> SysResult {
        info!(
            "clock_settime: id={:?} timespec={:?}",
            clock,
            timespec.read_if_not_null()?
        );
        if clock != 0 {
            return Err(LxError::EINVAL);
        }
        // Linux routes both clock setters through `security_settime64`, whose
        // default (`security/commoncap.c`) is the whole rule:
        //
        // ```c
        // int cap_settime(const struct timespec64 *ts, const struct timezone *tz)
        // {
        //         if (!capable(CAP_SYS_TIME))
        //                 return -EPERM;
        //         return 0;
        // }
        // ```
        if !self.linux_process().capable(CAP_SYS_TIME) {
            return Err(LxError::EPERM);
        }
        let ts = timespec.read()?;
        let target = Duration::new(ts.sec as u64, ts.nsec as u32);
        kernel_hal::timer::wall_clock_set(target);
        Ok(0)
    }

    /// legacy settimeofday (seconds + microseconds since Unix epoch)
    pub fn sys_settimeofday(&mut self, tv: UserInPtr<TimeVal>, tz: UserInPtr<u8>) -> SysResult {
        info!("settimeofday: tv={:?}, tz={:?}", tv, tz);
        if !tz.is_null() {
            return Err(LxError::EINVAL);
        }
        // The same `cap_settime` gate as `clock_settime`: one clock, one rule.
        if !self.linux_process().capable(CAP_SYS_TIME) {
            return Err(LxError::EPERM);
        }
        let timeval = tv.read()?;
        let target = Duration::new(timeval.sec as u64, timeval.usec as u32 * 1_000);
        kernel_hal::timer::wall_clock_set(target);
        Ok(0)
    }

    /// `adjtimex(2)` — NTP clock discipline. OpenNTPD / busybox ntpd call this
    /// to step or slew `CLOCK_REALTIME` and to read STA_UNSYNC. A missing
    /// implementation logged `unknown syscall: ADJTIMEX` and returned ENOSYS,
    /// so the daemon never synced.
    pub fn sys_adjtimex(&self, mut tx: UserInOutPtr<Timex>) -> SysResult {
        if tx.is_null() {
            return Err(LxError::EINVAL);
        }
        let mut timex = tx.read()?;
        // `adjtimex` is two calls in one: a read of the NTP state, which any
        // program may do (OpenNTPD polls `STA_UNSYNC` this way), and a change
        // to the system clock, which is `CAP_SYS_TIME`. `modes` is what says
        // which -- see [`adjtimex_changes_the_clock`].
        if adjtimex_changes_the_clock(timex.modes) && !self.linux_process().capable(CAP_SYS_TIME) {
            return Err(LxError::EPERM);
        }
        let state = adjtimex_apply(&mut timex)?;
        tx.write(timex)?;
        Ok(state)
    }

    /// `clock_adjtime(2)` — `adjtimex` for a given clock. Only
    /// `CLOCK_REALTIME` is adjustable here (same as Linux for the system clock).
    pub fn sys_clock_adjtime(&self, clock: usize, tx: UserInOutPtr<Timex>) -> SysResult {
        match clock {
            0 => self.sys_adjtimex(tx), // CLOCK_REALTIME
            _ => Err(LxError::EINVAL),
        }
    }

    /// get the time with second and microseconds
    pub fn sys_gettimeofday(
        &mut self,
        mut tv: UserOutPtr<TimeVal>,
        tz: UserInPtr<u8>,
    ) -> SysResult {
        trace!("gettimeofday: tv: {:?}, tz: {:?}", tv, tz);
        // don't support tz
        if !tz.is_null() {
            return Err(LxError::EINVAL);
        }

        let timeval = TimeVal::now();
        tv.write(timeval)?;

        trace!("gettimeofday: {:?}", timeval);

        Ok(0)
    }

    /// get time in seconds
    #[cfg(target_arch = "x86_64")]
    pub fn sys_time(&mut self, mut time: UserOutPtr<u64>) -> SysResult {
        trace!("time: time: {:?}", time);
        if time.is_null() {
            return Err(LxError::EINVAL);
        }
        let sec = TimeSpec::now().sec;
        time.write(sec as u64)?;
        Ok(sec)
    }

    /// JUST FOR TEST, DO NOT USE IT
    pub fn sys_block_in_kernel(&self) -> SysResult {
        // DEAD LOOP
        error!("loop in kernel");
        let mut old = TimeSpec::now().sec;
        loop {
            let sec = TimeSpec::now().sec;
            if sec == old {
                core::hint::spin_loop();
                continue;
            }
            old = sec;
            warn!("1 seconds past");
        }
    }

    /// get resource usage
    /// (see [linux man getrusage(2)](https://www.man7.org/linux/man-pages/man2/getrusage.2.html)).
    ///
    /// `ru_utime` is the accumulated user-mode time of the target's threads
    /// (measured around every user-mode entry in the run loop); `ru_stime` is
    /// the process's accumulated in-kernel syscall time from the perf
    /// accounting. The previous implementation wrote the wall-clock
    /// time-since-boot into both fields, which made any "CPU used" computation
    /// (`time(1)`, build systems' self-profiling) nonsense.
    /// `RUSAGE_CHILDREN` reports zeros: usage of reaped children is not
    /// retained.
    pub fn sys_getrusage(&mut self, who: usize, mut rusage: UserOutPtr<RUsage>) -> SysResult {
        info!("getrusage: who: {}, rusage: {:?}", who, rusage);
        const RUSAGE_SELF: isize = 0;
        const RUSAGE_CHILDREN: isize = -1;
        const RUSAGE_THREAD: isize = 1;
        if rusage.is_null() {
            return Err(LxError::EINVAL);
        }
        let (utime_ns, stime_ns) = match who as isize {
            RUSAGE_SELF => (
                process_user_time_ns(self.zircon_process()),
                self.linux_process().perf().totals().1,
            ),
            // Per-thread kernel time is not split out of the process total;
            // report the thread's user time and zero kernel time.
            RUSAGE_THREAD => (self.thread.get_time(), 0),
            // Totals of children this process has reaped, accumulated at
            // wait4/waitid time exactly like Linux does.
            RUSAGE_CHILDREN => self.linux_process().children_cpu_ns(),
            _ => return Err(LxError::EINVAL),
        };
        rusage.write(RUsage {
            utime: Duration::from_nanos(utime_ns).into(),
            stime: Duration::from_nanos(stime_ns).into(),
            ..RUsage::default()
        })?;
        Ok(0)
    }

    /// stores the current process times in the struct tms that buf points to
    /// (see [linux man times(2)](https://www.man7.org/linux/man-pages/man2/times.2.html)).
    ///
    /// `tms_utime`/`tms_stime` come from the same accounting as
    /// [`sys_getrusage`](Self::sys_getrusage), converted to clock ticks
    /// (100 Hz here). Times of terminated children are not retained, so
    /// `tms_cutime`/`tms_cstime` read zero. The return value stays the
    /// wall-clock tick count since boot.
    pub fn sys_times(&mut self, mut buf: UserOutPtr<Tms>) -> SysResult {
        info!("times: buf: {:?}", buf);

        // 10_000 us per tick (100 Hz) → 10_000_000 ns per tick.
        const NSEC_PER_TICK: u64 = USEC_PER_TICK as u64 * 1_000;

        let tv = TimeVal::now();
        let tick = (tv.sec * 1_000_000 + tv.usec) / USEC_PER_TICK;

        if !buf.is_null() {
            let utime_ns = process_user_time_ns(self.zircon_process());
            let stime_ns = self.linux_process().perf().totals().1;
            let (cutime_ns, cstime_ns) = self.linux_process().children_cpu_ns();
            let new_buf = Tms {
                tms_utime: utime_ns / NSEC_PER_TICK,
                tms_stime: stime_ns / NSEC_PER_TICK,
                tms_cutime: cutime_ns / NSEC_PER_TICK,
                tms_cstime: cstime_ns / NSEC_PER_TICK,
            };
            buf.write(new_buf)?;
        } else {
            warn!("sys_times: Invalid buf {:x?}", buf);
        }

        info!("tick: {:?}", tick);
        Ok(tick)
    }

    /// clock nanosleep
    pub async fn sys_clock_nanosleep(
        &mut self,
        clockid: usize,
        flags: usize,
        req: UserInPtr<TimeSpec>,
        rem: UserOutPtr<TimeSpec>,
    ) -> SysResult {
        let _ = self.maybe_handle_tty_intr()?;
        info!(
            "clock_nanosleep: clockid={}, flags={:#x}, req={:?}, rem={:?}",
            clockid, flags, req, rem
        );
        use kernel_hal::timer;
        // Same rule as `nanosleep`: reject an out-of-range `timespec`
        // instead of sleeping for whatever it happens to convert to.
        let request: Duration = req.read()?.try_into_duration()?;
        // Every decision this call makes, taken before anything sleeps. The
        // clock id and the flag word both used to go through an infallible
        // `From<usize>` ending in `unreachable!()`, so an id or a flag this
        // kernel did not list was a kernel panic from an ordinary syscall.
        let plan = plan_clock_nanosleep(
            clockid,
            flags,
            request,
            timer::timer_now(),
            timer::wall_clock_now(),
        )?;
        // `rem` only ever carries what a signal left over from a *relative*
        // sleep. Linux does not write it on success, and ignores it entirely
        // when TIMER_ABSTIME is set. The sleep itself is the interruptible
        // one of `nanosleep`: this was a plain `sleep_until`, which no
        // signal could cut short.
        let rem = if flags & TIMER_ABSTIME != 0 {
            UserOutPtr::from(0)
        } else {
            rem
        };
        if let SleepPlan::Until(deadline) = plan {
            crate::task::sleep_or_eintr(self.thread, deadline, rem).await?;
        }
        Ok(0)
    }

    /// set value of an interval timer
    /// (see [linux man setitimer(2)](https://www.man7.org/linux/man-pages/man2/setitimer.2.html)).
    ///
    /// Full stateful semantics: the previous value (remaining time + interval)
    /// comes back through `old_value`, a non-zero `interval` re-arms the timer
    /// on every expiry, and writing a zero `value` disarms a pending timer —
    /// the piece the old fire-and-forget implementation missed, and what
    /// `alarm(0)` needs in order to actually cancel.
    ///
    /// Each timer kind delivers its own signal (SIGALRM / SIGVTALRM /
    /// SIGPROF). ITIMER_VIRTUAL and ITIMER_PROF should count process CPU
    /// time; this kernel does not account CPU time per process, so they tick
    /// on the wall clock — an upper bound of the correct expiry. Profilers get
    /// their SIGPROF stream, just at wall-clock rate.
    pub fn sys_setitimer(
        &mut self,
        which: usize,
        new_value: UserInPtr<ITimerVal>,
        mut old_value: UserOutPtr<ITimerVal>,
    ) -> SysResult {
        let val = new_value.read()?;
        info!(
            "setitimer: which={}, new_value={:?}, old_value={:?}",
            which, val, old_value
        );
        if which > ITIMER_PROF {
            return Err(LxError::EINVAL);
        }
        // Linux validates both fields of both timevals: the microseconds
        // have to be a fraction of a second and the seconds must not be
        // negative.
        if !val.value.valid() || !val.interval.valid() {
            return Err(LxError::EINVAL);
        }
        let value = Duration::from(val.value);
        let interval = Duration::from(val.interval);
        let now = kernel_hal::timer::timer_now();
        let owner = self.zircon_process().id();
        let (old, arm) = {
            let mut slots = self.linux_process().itimers().lock();
            let slot = &mut slots[which];
            let old = itimerval_from_slot(slot, now);
            slot.generation += 1;
            let arm = if value.is_zero() {
                // Disarm. POSIX: a zero it_value stops the timer regardless of
                // what the interval field says.
                slot.interval = Duration::ZERO;
                slot.deadline = None;
                None
            } else {
                slot.interval = interval;
                let deadline = now + value;
                slot.deadline = Some(deadline);
                Some((deadline, slot.generation))
            };
            (old, arm)
        };
        // Reported LAST, after the timer is armed. Writing it here used to
        // abort the call on a faulting pointer with the slot ALREADY changed
        // and `arm_itimer` never reached: the process was left with a timer
        // that `getitimer` counts down and that never fires.
        commit_and_report_old(old, &mut old_value, || {
            if let Some((deadline, generation)) = arm {
                arm_itimer(owner, which, deadline, generation);
            }
            Ok(())
        })?;
        Ok(0)
    }

    /// get value of an interval timer
    /// (see [linux man getitimer(2)](https://www.man7.org/linux/man-pages/man2/getitimer.2.html)).
    ///
    /// Reports the live state kept by [`sys_setitimer`](Self::sys_setitimer):
    /// time remaining until the next expiry plus the reload interval.
    pub fn sys_getitimer(&self, which: usize, mut curr_value: UserOutPtr<ITimerVal>) -> SysResult {
        info!("getitimer: which={}", which);
        if which > ITIMER_PROF {
            return Err(LxError::EINVAL);
        }
        let now = kernel_hal::timer::timer_now();
        let val = {
            let slots = self.linux_process().itimers().lock();
            itimerval_from_slot(&slots[which], now)
        };
        curr_value.write(val)?;
        Ok(0)
    }

    /// Schedule SIGALRM (busybox `ping` uses this for read timeouts)
    /// (see [linux man alarm(2)](https://www.man7.org/linux/man-pages/man2/alarm.2.html)).
    ///
    /// Shares the ITIMER_REAL slot, exactly like Linux: `alarm` and
    /// `setitimer(ITIMER_REAL)` overwrite each other, `alarm(0)` cancels the
    /// pending alarm, and the return value is the seconds that were left on
    /// the previous one (rounded up, so a live timer never reports 0).
    pub fn sys_alarm(&self, seconds: usize) -> SysResult {
        info!("alarm: seconds={}", seconds);
        let now = kernel_hal::timer::timer_now();
        let owner = self.zircon_process().id();
        let (remaining_secs, arm) = {
            let mut slots = self.linux_process().itimers().lock();
            let slot = &mut slots[ITIMER_REAL];
            let remaining = slot
                .deadline
                .map(|d| d.saturating_sub(now))
                .unwrap_or_default();
            // Round up: returning 0 would mean "no alarm was pending".
            let remaining_secs =
                remaining.as_secs() as usize + usize::from(remaining.subsec_nanos() > 0);
            slot.generation += 1;
            slot.interval = Duration::ZERO;
            if seconds == 0 {
                slot.deadline = None;
                (remaining_secs, None)
            } else {
                let deadline = now + Duration::from_secs(seconds as u64);
                slot.deadline = Some(deadline);
                (remaining_secs, Some((deadline, slot.generation)))
            }
        };
        if let Some((deadline, generation)) = arm {
            arm_itimer(owner, ITIMER_REAL, deadline, generation);
        }
        Ok(remaining_secs)
    }

    /// `timer_create`: create a per-process POSIX interval timer. The notify
    /// signal comes from `sevp` (`struct sigevent`); a null `sevp` defaults to
    /// SIGALRM, `SIGEV_NONE` delivers no signal. The new timer id is written to
    /// `timerid` (an `int`, the kernel's `timer_t`).
    pub fn sys_timer_create(&self, clockid: usize, sevp: usize, timerid: usize) -> SysResult {
        let clock = posix_timer_clock_base(clockid)?;
        let notify = if sevp == 0 {
            TimerNotify::SIGALRM_TO_PROCESS
        } else {
            // struct sigevent: sigev_value (8 bytes) @ 0, sigev_signo @ 8,
            // sigev_notify @ 12, and sigev_notify_thread_id @ 16 (64 bytes in
            // all, so the four reads are inside it whatever the notify).
            let value_p: UserInPtr<usize> = sevp.into();
            let signo_p: UserInPtr<i32> = (sevp + 8).into();
            let notify_p: UserInPtr<i32> = (sevp + 12).into();
            let tid_p: UserInPtr<i32> = (sevp + 16).into();
            let event = SigEvent {
                value: value_p.read()?,
                signo: signo_p.read()?,
                notify: notify_p.read()?,
                thread_id: tid_p.read()?,
            };
            let proc = self.zircon_process();
            timer_notify_from_sigevent(&event, |tid| proc.get_child(tid).is_ok())?
        };
        ensure_posix_timers_die_with_their_owner();
        let id = NEXT_TIMER_ID.fetch_add(1, Ordering::Relaxed);
        POSIX_TIMERS.lock().insert(
            id,
            PosixTimer {
                owner: self.zircon_process().id(),
                notify,
                clock,
                interval: Duration::ZERO,
                next: Duration::ZERO,
                generation: 0,
                overrun_last: 0,
            },
        );
        let mut out: UserOutPtr<i32> = timerid.into();
        out.write(id as i32)?;
        Ok(0)
    }

    /// `timer_settime`: arm/disarm a timer. `it_value == 0` disarms; otherwise
    /// the timer fires at `it_value` (relative, or absolute with TIMER_ABSTIME)
    /// and then every `it_interval`.
    pub fn sys_timer_settime(
        &self,
        id: usize,
        flags: usize,
        new_value: UserInPtr<ITimerSpec>,
        mut old_value: UserOutPtr<ITimerSpec>,
    ) -> SysResult {
        let owner = self.zircon_process().id();
        let spec = new_value.read()?;
        // Linux validates both timespecs of the itimerspec before arming.
        if !spec.interval.valid() || !spec.value.valid() {
            return Err(LxError::EINVAL);
        }
        let interval = timespec_to_duration(spec.interval);
        let init = timespec_to_duration(spec.value);
        // Both clocks read before the lock, so the deadline arithmetic below
        // happens with the map held and no HAL call under it.
        let now = kernel_hal::timer::timer_now();
        let now_wall = kernel_hal::timer::wall_clock_now();

        let (old, arm) = {
            let mut timers = POSIX_TIMERS.lock();
            let t = timers
                .get_mut(&id)
                .filter(|t| t.owner == owner)
                .ok_or(LxError::EINVAL)?;
            let remaining = t.next.checked_sub(now).unwrap_or(Duration::ZERO);
            let old = ITimerSpec {
                interval: TimeSpec::from_duration(t.interval),
                value: TimeSpec::from_duration(remaining),
            };
            // Bump the generation so any in-flight one-shot is dropped.
            t.generation += 1;
            t.interval = interval;
            let arm = if init.is_zero() {
                t.next = Duration::ZERO;
                None
            } else {
                // `init` is a point on the timer's OWN clock when
                // TIMER_ABSTIME is set, and the kernel timer only takes
                // monotonic deadlines.
                let deadline =
                    timer_arm_deadline(t.clock, flags & TIMER_ABSTIME != 0, init, now, now_wall);
                t.next = deadline;
                Some((deadline, t.generation))
            };
            (old, arm)
        };
        // Same as `setitimer`: armed first, reported after. A faulting
        // `old_value` used to leave the timer recorded as due and never armed.
        commit_and_report_old(old, &mut old_value, || {
            if let Some((deadline, gen)) = arm {
                arm_posix_timer(id, deadline, gen);
            }
            Ok(())
        })?;
        Ok(0)
    }

    /// `timer_gettime`: report the time until next expiration and the interval.
    pub fn sys_timer_gettime(&self, id: usize, curr_value: usize) -> SysResult {
        let owner = self.zircon_process().id();
        let now = kernel_hal::timer::timer_now();
        let out = {
            let timers = POSIX_TIMERS.lock();
            let t = timers
                .get(&id)
                .filter(|t| t.owner == owner)
                .ok_or(LxError::EINVAL)?;
            let remaining = if t.next.is_zero() {
                Duration::ZERO
            } else {
                t.next.checked_sub(now).unwrap_or(Duration::ZERO)
            };
            ITimerSpec {
                interval: TimeSpec::from_duration(t.interval),
                value: TimeSpec::from_duration(remaining),
            }
        };
        let mut p: UserOutPtr<ITimerSpec> = curr_value.into();
        p.write(out)?;
        Ok(0)
    }

    /// `timer_delete`: destroy a timer (cancels any pending fire via generation).
    pub fn sys_timer_delete(&self, id: usize) -> SysResult {
        let owner = self.zircon_process().id();
        let mut timers = POSIX_TIMERS.lock();
        match timers.get(&id) {
            Some(t) if t.owner == owner => {
                timers.remove(&id);
                Ok(0)
            }
            _ => Err(LxError::EINVAL),
        }
    }

    /// `timer_getoverrun`: the overrun count of the timer's last expiry,
    /// which used to be a fixed 0 (see [`forward_periodic`]).
    pub fn sys_timer_getoverrun(&self, id: usize) -> SysResult {
        let owner = self.zircon_process().id();
        let timers = POSIX_TIMERS.lock();
        match timers.get(&id) {
            Some(t) if t.owner == owner => Ok(t.overrun_last.min(DELAYTIMER_MAX) as usize),
            _ => Err(LxError::EINVAL),
        }
    }
}

/// What `timer_create(2)` was told to do on expiry: its `struct sigevent`,
/// decoded once by `good_sigevent`'s rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TimerNotify {
    /// Signal to deliver; 0 for `SIGEV_NONE`.
    signo: usize,
    /// `sigev_value`, handed back to the handler as `si_value`. glibc's
    /// `SIGEV_THREAD` helper thread keeps the timer it must run the
    /// callback for in here, so with it dropped no `SIGEV_THREAD` timer
    /// ever ran its function.
    value: usize,
    /// `SIGEV_THREAD_ID`: the one thread the signal goes to. `None` is a
    /// process-directed signal, like `kill(2)`.
    thread: Option<KoID>,
}

impl TimerNotify {
    /// A null `sevp`: `SIGALRM` to the process, `si_value` = the timer id
    /// (the kernel fills `sigev_value.sival_int` with it, timer_create(2)).
    const SIGALRM_TO_PROCESS: Self = TimerNotify {
        signo: Signal::SIGALRM as usize,
        value: 0,
        thread: None,
    };
}

/// The fields of `struct sigevent` a timer reads.
#[derive(Debug, Clone, Copy)]
struct SigEvent {
    value: usize,
    signo: i32,
    notify: i32,
    thread_id: i32,
}

const SIGEV_SIGNAL: i32 = 0;
const SIGEV_NONE: i32 = 1;
const SIGEV_THREAD: i32 = 2;
const SIGEV_THREAD_ID: i32 = 4;
const SIGRTMAX: i32 = 64;

/// `good_sigevent()`: which notifications a timer may be created with.
/// `SIGEV_THREAD_ID` names a thread of the caller's own process, or it is
/// `EINVAL`; the signal has to be a real one; `SIGEV_THREAD` reaches the
/// kernel only from a program bypassing libc, and is a plain signal there.
///
/// This used to take `sigev_signo` as it came (0 and 200 both "worked":
/// nothing was ever delivered), accept any `sigev_notify`, and read neither
/// `sigev_value` nor the thread id, so every timer fired at the process.
fn timer_notify_from_sigevent(
    event: &SigEvent,
    is_my_thread: impl Fn(KoID) -> bool,
) -> Result<TimerNotify, LxError> {
    let thread = match event.notify {
        SIGEV_NONE => {
            return Ok(TimerNotify {
                signo: 0,
                value: event.value,
                thread: None,
            })
        }
        SIGEV_SIGNAL | SIGEV_THREAD => None,
        SIGEV_THREAD_ID => {
            if event.thread_id <= 0 || !is_my_thread(event.thread_id as KoID) {
                return Err(LxError::EINVAL);
            }
            Some(event.thread_id as KoID)
        }
        _ => return Err(LxError::EINVAL),
    };
    if event.signo <= 0 || event.signo > SIGRTMAX {
        return Err(LxError::EINVAL);
    }
    Ok(TimerNotify {
        signo: event.signo as usize,
        value: event.value,
        thread,
    })
}

/// A per-process POSIX interval timer (`timer_create`).
struct PosixTimer {
    /// Owning process KoID; a process may only operate on its own timers.
    owner: KoID,
    /// Where the expiry goes: signal, `si_value`, and which thread.
    notify: TimerNotify,
    /// The timeline `timer_create`'s `clockid` names, which is what an
    /// absolute `timer_settime` counts against. The id used to be dropped
    /// on the floor (`_clockid`), so a `CLOCK_REALTIME` timer armed with
    /// `TIMER_ABSTIME` got a monotonic deadline of seconds-since-1970 and
    /// never fired.
    clock: ClockBase,
    /// Period for a periodic timer; `ZERO` = one-shot.
    interval: Duration,
    /// Absolute monotonic deadline of the next expiry; `ZERO` = disarmed.
    next: Duration,
    /// Bumped by settime/delete to invalidate an already-scheduled one-shot
    /// (`timer_set` callbacks are not cancellable, so they check this).
    generation: u64,
    /// The overrun count of the last expiry (`it_overrun_last`): the
    /// periods that went by without a signal of their own because the
    /// timer was late, what `timer_getoverrun(2)` and `si_overrun` report.
    overrun_last: u32,
}

lazy_static! {
    static ref POSIX_TIMERS: Mutex<BTreeMap<usize, PosixTimer>> = Mutex::new(BTreeMap::new());
}
static NEXT_TIMER_ID: AtomicUsize = AtomicUsize::new(1);

fn timespec_to_duration(ts: TimeSpec) -> Duration {
    Duration::from_secs(ts.sec as u64) + Duration::from_nanos(ts.nsec as u64)
}

/// Accumulated user-mode nanoseconds of `proc`: its live threads plus the
/// process-level accumulator of already-exited ones (Linux keeps counting a
/// joined thread's time in the process totals). This is the utime side of
/// getrusage(2)/times(2); kernel-side time is accounted per-syscall by
/// `linux_object::perf` and reported as stime.
fn process_user_time_ns(proc: &alloc::sync::Arc<zircon_object::task::Process>) -> u64 {
    let live: u64 = proc
        .thread_ids()
        .into_iter()
        .filter_map(|tid| proc.get_child(tid).ok())
        .filter_map(|obj| obj.downcast_arc::<Thread>().ok())
        .map(|t| t.get_time())
        .sum();
    live + proc.dead_threads_time()
}

/// `ITIMER_*` indices from the uapi.
const ITIMER_REAL: usize = 0;
const ITIMER_VIRTUAL: usize = 1;
const ITIMER_PROF: usize = 2;

/// The signal an expiring interval-timer slot delivers (setitimer(2)).
fn itimer_signo(which: usize) -> usize {
    match which {
        ITIMER_VIRTUAL => Signal::SIGVTALRM as usize,
        ITIMER_PROF => Signal::SIGPROF as usize,
        _ => Signal::SIGALRM as usize,
    }
}

/// Render a slot as the userspace `struct itimerval`: the reload interval plus
/// the time remaining until expiry (all zeros when disarmed). Pure, so the
/// remaining-time arithmetic is unit-testable.
fn itimerval_from_slot(slot: &ItimerSlot, now: Duration) -> ITimerVal {
    ITimerVal {
        interval: slot.interval.into(),
        value: slot
            .deadline
            .map(|d| d.saturating_sub(now))
            .unwrap_or_default()
            .into(),
    }
}

/// Schedule the one-shot that fires itimer `which` of process `owner` at
/// `deadline`, valid only while the slot's generation still matches `gen`
/// (the same protocol as [`arm_posix_timer`]). A periodic timer re-arms
/// itself from the callback; the process is looked up by id each time, so an
/// exited process just fails the lookup and the chain stops without the timer
/// wheel keeping the process object alive.
fn arm_itimer(owner: KoID, which: usize, deadline: Duration, gen: u64) {
    kernel_hal::timer::timer_set(
        deadline,
        Box::new(move |now| {
            if let Some(next) = expire_itimer(owner, which, gen, now) {
                arm_itimer(owner, which, next, gen);
            }
        }),
    );
}

/// The largest overrun count `timer_getoverrun(2)` and `si_overrun` report
/// (`DELAYTIMER_MAX`, `i32::MAX` on Linux).
const DELAYTIMER_MAX: u32 = i32::MAX as u32;

/// Where a periodic timer whose expiry was at `expiry` fires next, given
/// that the wheel got to it at `now`, and how many periods went by in
/// between without a signal of their own: `hrtimer_forward`. Drift-free
/// (the next expiry is a whole number of periods after the programmed
/// one), and never in the past.
///
/// Both timer callbacks used to step exactly one period from the
/// programmed expiry, whatever the time was: a process stopped for ten
/// seconds with a 1 ms timer got its next expiry 9 999 ms in the past, and
/// the wheel fired it again at once, and again, ten thousand back-to-back
/// expiries -- one signal per missed period, each `timer_set` with a
/// deadline already gone -- where Linux delivers ONE, with `si_overrun` and
/// `timer_getoverrun(2)` saying how many were skipped.
fn forward_periodic(expiry: Duration, interval: Duration, now: Duration) -> (Duration, u32) {
    let next = expiry + interval;
    if next > now {
        return (next, 0);
    }
    // `now - expiry` whole periods have gone by since the programmed
    // expiry; the first one is the expiry that fires now, the rest are
    // overruns, and the next expiry is the period after all of them.
    let elapsed = now - expiry;
    let periods = elapsed.as_nanos() / interval.as_nanos();
    let overrun = periods.min(DELAYTIMER_MAX as u128) as u32;
    // Saturating all the way: `interval` comes from userspace.
    let advance = (periods + 1).saturating_mul(interval.as_nanos());
    let advance = Duration::from_nanos(advance.min(u64::MAX as u128) as u64);
    (expiry.saturating_add(advance), overrun)
}

/// One expiry of the `setitimer(2)` slot `which` of `owner`, at `now`:
/// deliver its signal and say when it fires next, if it is periodic.
/// Nothing if the slot was re-armed or disarmed since (`gen`).
fn expire_itimer(owner: KoID, which: usize, gen: u64, now: Duration) -> Option<Duration> {
    let mut fire = false;
    let mut rearm = None;
    if let Some(proc) = ROOT_JOB.find_process(owner) {
        if let Some(lp) = proc.try_linux() {
            let mut slots = lp.itimers().lock();
            let slot = &mut slots[which];
            if slot.generation == gen {
                if let Some(expiry) = slot.deadline {
                    fire = true;
                    if slot.interval.is_zero() {
                        slot.deadline = None;
                    } else {
                        let (next, _overrun) = forward_periodic(expiry, slot.interval, now);
                        slot.deadline = Some(next);
                        rearm = Some(next);
                    }
                }
            }
        }
    }
    if fire {
        if let Some((signal, info)) = itimer_expiry_signal(which) {
            deliver_timer_signal(owner, None, signal, info);
        }
    }
    rearm
}

/// Disarm and delete every POSIX timer owned by `owner`, returning how many
/// there were.
///
/// `execve` must do this: timer_create(2) says timers "are not inherited by a
/// child created via fork(2), and are disarmed and deleted during an
/// execve(2)", and Linux does it in `begin_new_exec()` -> `exit_itimers()`.
/// Left behind, a timer keeps firing at the same pid -- which past the exec
/// is a DIFFERENT program. The new image takes a `SIGALRM`, or whatever
/// signal the old one registered, from a timer it never created, and the
/// default action for those is to die.
///
/// Dropping the entry is all the disarming needed: an already-scheduled
/// one-shot looks itself up by id when it fires (see `arm_posix_timer`) and
/// finds nothing.
///
/// Interval timers (`setitimer`) are deliberately NOT touched here:
/// setitimer(2) says those ARE preserved across an `execve`.
pub fn drop_posix_timers_of(owner: KoID) -> usize {
    let mut timers = POSIX_TIMERS.lock();
    let before = timers.len();
    timers.retain(|_, t| t.owner != owner);
    before - timers.len()
}

/// Whether the process-exit hook that deletes a dead process's timers is in
/// place. Registered on the first `timer_create`, once: `linux-object` runs its
/// exit hooks from the `PROCESS_TERMINATED` callback and cannot name this
/// crate's table itself.
static EXIT_HOOK_REGISTERED: AtomicBool = AtomicBool::new(false);

/// `exit_itimers()` in `do_exit()`: a process's POSIX timers die with it.
///
/// They did not. The table is keyed by the owner's pid and nothing consulted
/// it when a process ended, so a timer outlived its creator: a periodic one
/// went on re-arming itself from its own callback and firing a signal at the
/// dead pid on every period, for as long as the machine stayed up, and every
/// one-shot or disarmed entry stayed in the table. A program that runs on a
/// `timer_create` tick and is started and killed a few hundred times leaves
/// that many timers ticking behind it.
fn ensure_posix_timers_die_with_their_owner() {
    if !EXIT_HOOK_REGISTERED.swap(true, Ordering::AcqRel) {
        linux_object::process::register_process_exit_hook(|pid| {
            drop_posix_timers_of(pid);
        });
    }
}

/// Whether `pid` is a live process: present and not yet exited. A zombie
/// counts as dead, because Linux deletes the timers in `do_exit`, before the
/// parent has reaped anything.
fn process_is_alive(pid: KoID) -> bool {
    ROOT_JOB
        .find_process(pid)
        .is_some_and(|p| !matches!(p.status(), Status::Exited(_)))
}

/// What an expiring `setitimer(2)` slot delivers: its signal, sent by the
/// kernel with nobody behind it (`it_real_fn` -> `SEND_SIG_PRIV`: `SI_KERNEL`,
/// pid and uid 0).
fn itimer_expiry_signal(which: usize) -> Option<(Signal, SigInfo)> {
    let signal = Signal::try_from(itimer_signo(which) as u8).ok()?;
    Some((signal, SigInfo::from_kernel(signal)))
}

/// What an expiring POSIX timer delivers: `SI_TIMER` with the timer's id
/// and the `sigev_value` it was created with (`posix_timer_event` ->
/// `send_sigqueue`), or nothing for `SIGEV_NONE`.
fn posix_timer_expiry_signal(
    id: usize,
    notify: TimerNotify,
    overrun: u32,
) -> Option<(Signal, SigInfo)> {
    if notify.signo == 0 {
        return None;
    }
    let signal = Signal::try_from(notify.signo as u8).ok()?;
    let overrun = overrun.min(DELAYTIMER_MAX) as i32;
    Some((
        signal,
        SigInfo::timer(signal, id as i32, overrun, notify.value),
    ))
}

/// Deliver an expired timer's signal, once: to the process (`kill_pid_info`:
/// one thread that has it unblocked, else pending on the process until one
/// does) or, for `SIGEV_THREAD_ID`, to the one thread the timer named.
///
/// This used to set the bit on EVERY thread of the process, straight into
/// the pending set, with no `siginfo_t` and without the disposition being
/// looked at: a threaded program with a `SIGALRM` or `SIGPROF` handler ran
/// it once per thread per expiry, and one `alarm(2)` came back as `EINTR`
/// in every thread's blocking syscall at once.
fn deliver_timer_signal(owner: KoID, target: Option<KoID>, signal: Signal, info: SigInfo) {
    match target {
        None => {
            let _ = linux_object::process::send_signal_to_process_with_info(
                owner as usize,
                signal,
                Some(info),
            );
        }
        Some(tid) => {
            // The thread may be gone by now: a timer is deleted with its
            // process, not with one of its threads, and Linux drops the
            // signal then too.
            if let Some(proc) = ROOT_JOB.find_process(owner) {
                if let Ok(obj) = proc.get_child(tid) {
                    if let Ok(thread) = obj.downcast_arc::<Thread>() {
                        thread.lock_linux().queue_signal(signal, Some(info));
                        linux_object::process::wake_signal_sleeper(&thread);
                    }
                }
            }
        }
    }
}

/// Schedule the one-shot that fires timer `id` at `deadline`, valid only while
/// the timer still exists and its generation matches `gen`. A periodic timer
/// re-arms itself from the callback.
fn arm_posix_timer(id: usize, deadline: Duration, gen: u64) {
    kernel_hal::timer::timer_set(
        deadline,
        Box::new(move |now| {
            if let Some(next) = expire_posix_timer(id, gen, now) {
                arm_posix_timer(id, next, gen);
            }
        }),
    );
}

/// One expiry of POSIX timer `id`, at `now`: deliver its signal, with the
/// overrun count of this expiry in `si_overrun` and kept for
/// `timer_getoverrun`, and say when it fires next if it is periodic.
/// Nothing if the timer was deleted or re-armed since (`gen`).
fn expire_posix_timer(id: usize, gen: u64, now: Duration) -> Option<Duration> {
    // The owner first, and outside the table's lock: `find_process` takes the
    // job's. A timer whose owner has died since it was armed (the exit hook
    // and this callback can race) is deleted here instead of fired, and a
    // periodic one is not re-armed: nothing is left to receive its signal.
    let owner = POSIX_TIMERS
        .lock()
        .get(&id)
        .filter(|t| t.generation == gen)
        .map(|t| t.owner)?;
    if !process_is_alive(owner) {
        POSIX_TIMERS.lock().remove(&id);
        return None;
    }
    let mut fire = None;
    let mut rearm = None;
    {
        let mut timers = POSIX_TIMERS.lock();
        if let Some(t) = timers.get_mut(&id) {
            if t.generation == gen {
                let overrun = if t.interval.is_zero() {
                    t.next = Duration::ZERO;
                    0
                } else {
                    let (next, overrun) = forward_periodic(t.next, t.interval, now);
                    t.next = next;
                    rearm = Some(next);
                    overrun
                };
                t.overrun_last = overrun;
                fire = Some((t.owner, t.notify, overrun));
            }
        }
    }
    if let Some((owner, notify, overrun)) = fire {
        if let Some((signal, info)) = posix_timer_expiry_signal(id, notify, overrun) {
            deliver_timer_signal(owner, notify.thread, signal, info);
        }
    }
    rearm
}

#[cfg(test)]
mod itimer_tests {
    use super::*;

    #[test]
    fn disarmed_slot_reads_all_zeros() {
        let slot = ItimerSlot::default();
        let v = itimerval_from_slot(&slot, Duration::from_secs(100));
        assert_eq!((v.value.sec, v.value.usec), (0, 0));
        assert_eq!((v.interval.sec, v.interval.usec), (0, 0));
    }

    #[test]
    fn armed_slot_reports_remaining_and_interval() {
        let slot = ItimerSlot {
            interval: Duration::from_millis(1500),
            deadline: Some(Duration::from_secs(10)),
            generation: 3,
        };
        let v = itimerval_from_slot(&slot, Duration::from_millis(7750));
        // 10s - 7.75s = 2.25s remaining.
        assert_eq!((v.value.sec, v.value.usec), (2, 250_000));
        assert_eq!((v.interval.sec, v.interval.usec), (1, 500_000));
    }

    #[test]
    fn expired_deadline_saturates_to_zero() {
        let slot = ItimerSlot {
            interval: Duration::ZERO,
            deadline: Some(Duration::from_secs(5)),
            generation: 1,
        };
        let v = itimerval_from_slot(&slot, Duration::from_secs(9));
        assert_eq!((v.value.sec, v.value.usec), (0, 0));
    }

    #[test]
    fn itimer_slots_deliver_their_own_signals() {
        assert_eq!(itimer_signo(ITIMER_REAL), Signal::SIGALRM as usize);
        assert_eq!(itimer_signo(ITIMER_VIRTUAL), Signal::SIGVTALRM as usize);
        assert_eq!(itimer_signo(ITIMER_PROF), Signal::SIGPROF as usize);
    }
}

#[cfg(test)]
mod adjtimex_tests {
    extern crate std;
    use super::*;

    /// `NTP_STATE` is one global kernel state and cargo runs a crate's tests in
    /// threads, so every test that resets it takes this first.
    ///
    /// Without it `frequency_roundtrips` failed about 4% of runs on its own and
    /// once in a hundred full-suite runs: it writes a frequency and reads it
    /// back, and a neighbour's `*NTP_STATE.lock() = NtpState::default()` landing
    /// in between makes the readback 0. That reads as a real regression in
    /// `adjtimex` and is not one. CI runs with `--test-threads=1`, so it never
    /// sees any of this.
    static LOCK: self::std::sync::Mutex<()> = self::std::sync::Mutex::new(());

    fn serialised() -> self::std::sync::MutexGuard<'static, ()> {
        // A test that panics while holding this poisons it; the tests that
        // follow are not at fault, so step over the poison.
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn timex_matches_linux_x86_64_layout() {
        assert_eq!(core::mem::size_of::<Timex>(), 208);
        assert_eq!(core::mem::align_of::<Timex>(), 8);
        assert_eq!(core::mem::offset_of!(Timex, offset), 8);
        assert_eq!(core::mem::offset_of!(Timex, time), 72);
        assert_eq!(core::mem::offset_of!(Timex, tai), 160);
    }

    #[test]
    fn read_reports_unsync_and_fills_tick() {
        let _serialised = serialised();
        *NTP_STATE.lock() = NtpState::default();
        let mut tx = Timex::default();
        let r = adjtimex_apply(&mut tx).unwrap();
        assert_eq!(r, TIME_ERROR);
        assert_eq!(tx.tick, 10_000);
        assert_eq!(tx.status & STA_UNSYNC, STA_UNSYNC);
        assert!(tx.time.sec >= 0);
    }

    #[test]
    fn clearing_unsync_returns_time_ok() {
        let _serialised = serialised();
        *NTP_STATE.lock() = NtpState::default();
        let mut tx = Timex {
            modes: ADJ_STATUS,
            status: 0,
            ..Default::default()
        };
        let r = adjtimex_apply(&mut tx).unwrap();
        assert_eq!(r, TIME_OK);
        assert_eq!(tx.status & STA_UNSYNC, 0);
    }

    #[test]
    fn setoffset_rejects_out_of_range_usec() {
        let _serialised = serialised();
        *NTP_STATE.lock() = NtpState::default();
        let mut tx = Timex {
            modes: ADJ_SETOFFSET,
            time: TimeValI64 {
                sec: 0,
                usec: 1_000_000,
            },
            ..Default::default()
        };
        assert_eq!(adjtimex_apply(&mut tx), Err(LxError::EINVAL));
    }

    // ---- who may move the clock -------------------------------------

    #[test]
    fn a_pure_read_needs_no_privilege() {
        // `modes == 0` asks for nothing and is how OpenNTPD polls STA_UNSYNC.
        assert!(!adjtimex_changes_the_clock(0));
    }

    #[test]
    fn the_read_only_adjtime_needs_no_privilege_either() {
        // ADJ_OFFSET_SS_READ is ADJ_ADJTIME | ADJ_OFFSET_SINGLESHOT |
        // ADJ_OFFSET_READONLY: read the leftover offset, change nothing.
        assert!(!adjtimex_changes_the_clock(ADJ_OFFSET_SS_READ));
    }

    #[test]
    fn the_writing_adjtime_does() {
        // The same call without the read-only bit sets the offset.
        assert!(adjtimex_changes_the_clock(ADJ_OFFSET_SINGLESHOT));
    }

    #[test]
    fn injecting_an_offset_needs_it_even_alongside_the_read_only_bit() {
        // Linux asks about ADJ_SETOFFSET on its own, outside the ADJ_ADJTIME
        // branch, so the read-only bit does not buy a free time injection.
        assert!(adjtimex_changes_the_clock(ADJ_SETOFFSET));
        assert!(adjtimex_changes_the_clock(
            ADJ_OFFSET_SS_READ | ADJ_SETOFFSET
        ));
    }

    #[test]
    fn every_mode_that_writes_something_needs_the_privilege() {
        // "In order to modify anything, you gotta be super-user!" -- every
        // bit `adjtimex_apply` acts on, one by one, so a bit added to that
        // function without a thought lands here.
        for modes in [
            ADJ_OFFSET,
            ADJ_FREQUENCY,
            ADJ_MAXERROR,
            ADJ_ESTERROR,
            ADJ_STATUS,
            ADJ_TIMECONST,
            ADJ_TAI,
            ADJ_SETOFFSET,
            ADJ_MICRO,
            ADJ_NANO,
            ADJ_TICK,
        ] {
            assert!(
                adjtimex_changes_the_clock(modes),
                "modes {:#x} slipped through",
                modes
            );
        }
    }

    #[test]
    fn the_read_only_bit_only_means_anything_with_adjtime() {
        // It shares a bit with ADJ_NANO, which is a *change* of resolution:
        // outside the ADJ_ADJTIME branch the same 0x2000 must not excuse it.
        assert_eq!(ADJ_OFFSET_READONLY, ADJ_NANO);
        assert!(adjtimex_changes_the_clock(ADJ_NANO));
    }

    #[test]
    fn adjtime_without_its_singleshot_bit_is_rejected() {
        // `timekeeping_validate_timex`: ADJ_ADJTIME means `adjtime(3)`, which
        // is a single-shot offset and nothing else. Accepting the bare bit
        // did nothing at all and said it had worked.
        let _serialised = serialised();
        *NTP_STATE.lock() = NtpState::default();
        let mut tx = Timex {
            modes: ADJ_ADJTIME,
            ..Default::default()
        };
        assert_eq!(adjtimex_apply(&mut tx), Err(LxError::EINVAL));

        let mut tx = Timex {
            modes: ADJ_ADJTIME | ADJ_FREQUENCY,
            freq: 1,
            ..Default::default()
        };
        assert_eq!(adjtimex_apply(&mut tx), Err(LxError::EINVAL));
    }

    #[test]
    fn the_two_singleshot_calls_still_go_through() {
        // The bit is only refused on its own: both real spellings carry
        // ADJ_OFFSET_SINGLESHOT.
        let _serialised = serialised();
        *NTP_STATE.lock() = NtpState::default();
        let mut tx = Timex {
            modes: ADJ_OFFSET_SS_READ,
            ..Default::default()
        };
        assert!(adjtimex_apply(&mut tx).is_ok());
        let mut tx = Timex {
            modes: ADJ_OFFSET_SINGLESHOT,
            ..Default::default()
        };
        assert!(adjtimex_apply(&mut tx).is_ok());
    }

    #[test]
    fn frequency_roundtrips() {
        let _serialised = serialised();
        *NTP_STATE.lock() = NtpState::default();
        let mut tx = Timex {
            modes: ADJ_FREQUENCY,
            freq: 65536,
            ..Default::default()
        };
        adjtimex_apply(&mut tx).unwrap();
        let mut readback = Timex::default();
        adjtimex_apply(&mut readback).unwrap();
        assert_eq!(readback.freq, 65536);
    }

    #[test]
    fn bad_tick_is_einval() {
        let _serialised = serialised();
        *NTP_STATE.lock() = NtpState::default();
        let mut tx = Timex {
            modes: ADJ_TICK,
            tick: 1,
            ..Default::default()
        };
        assert_eq!(adjtimex_apply(&mut tx), Err(LxError::EINVAL));
    }

    #[test]
    fn setoffset_ns_micro_and_nano() {
        let us = TimeValI64 {
            sec: 1,
            usec: 500_000,
        };
        assert_eq!(setoffset_ns(&us, false).unwrap(), 1_500_000_000);
        let ns = TimeValI64 { sec: 0, usec: 250 };
        assert_eq!(setoffset_ns(&ns, true).unwrap(), 250);
        let neg = TimeValI64 { sec: -1, usec: 0 };
        assert_eq!(setoffset_ns(&neg, false).unwrap(), -1_000_000_000);
    }
}

#[cfg(test)]
mod exec_timer_tests {
    //! timer_create(2): POSIX timers "are disarmed and deleted during an
    //! execve(2)". They fire at a pid, and past an exec that pid is a
    //! different program, so one left armed delivers a signal to an image
    //! that never asked for it — and the default action for `SIGALRM` is to
    //! die.
    extern crate std;
    use super::*;

    /// `POSIX_TIMERS` is one map shared by the whole test binary, so every
    /// test that writes it takes this first. CI runs with `--test-threads=1`
    /// and would never see the interference; a developer running the suite in
    /// parallel would, as a count that is off by someone else's timer.
    static LOCK: self::std::sync::Mutex<()> = self::std::sync::Mutex::new(());

    fn serialised() -> self::std::sync::MutexGuard<'static, ()> {
        // A test that panics while holding this poisons it; the tests that
        // follow are not at fault, so step over the poison.
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register an armed, periodic timer owned by `owner`, as
    /// `timer_create` + `timer_settime` would leave one.
    fn a_timer_of(owner: KoID) -> usize {
        let id = NEXT_TIMER_ID.fetch_add(1, Ordering::Relaxed);
        POSIX_TIMERS.lock().insert(
            id,
            PosixTimer {
                owner,
                notify: TimerNotify::SIGALRM_TO_PROCESS,
                clock: ClockBase::Monotonic,
                interval: Duration::from_secs(1),
                next: Duration::from_secs(1),
                generation: 0,
                overrun_last: 0,
            },
        );
        id
    }

    fn still_there(id: usize) -> bool {
        POSIX_TIMERS.lock().contains_key(&id)
    }

    #[test]
    fn an_exec_deletes_the_timers_of_that_process_and_no_others() {
        let _guard = serialised();
        let (mine, neighbour) = (0x4711_0001, 0x4711_0002);
        let one = a_timer_of(mine);
        let two = a_timer_of(mine);
        let theirs = a_timer_of(neighbour);

        assert_eq!(drop_posix_timers_of(mine), 2);

        assert!(!still_there(one));
        assert!(!still_there(two));
        assert!(
            still_there(theirs),
            "an exec in one process took another process's timer"
        );
        POSIX_TIMERS.lock().remove(&theirs);
    }

    #[test]
    fn deleting_is_what_stops_an_already_scheduled_one_shot() {
        let _guard = serialised();
        let owner = 0x4711_0003;
        let id = a_timer_of(owner);
        drop_posix_timers_of(owner);
        // This is the whole disarm: the callback `arm_posix_timer` left in
        // the timer wheel cannot be cancelled, so when it fires it looks
        // itself up by id and does nothing at all if the entry is gone.
        assert!(POSIX_TIMERS.lock().get(&id).is_none());
    }

    #[test]
    fn a_second_exec_finds_nothing_left_to_delete() {
        let _guard = serialised();
        let owner = 0x4711_0004;
        a_timer_of(owner);
        assert_eq!(drop_posix_timers_of(owner), 1);
        assert_eq!(drop_posix_timers_of(owner), 0);
    }

    #[test]
    fn an_exec_in_a_process_with_no_timers_deletes_nothing() {
        let _guard = serialised();
        let theirs = a_timer_of(0x4711_0005);
        assert_eq!(drop_posix_timers_of(0x4711_0006), 0);
        assert!(still_there(theirs));
        POSIX_TIMERS.lock().remove(&theirs);
    }
}

#[cfg(test)]
mod exit_timer_tests {
    //! `exit_itimers()`: a process's POSIX timers die with it. They did not:
    //! nothing looked at the table when a process ended, so a periodic timer
    //! kept re-arming itself and firing at the dead pid for ever, and every
    //! other entry of the dead process stayed in the table.
    //!
    //! Each test owns its pids and asserts only on its own ids, so it holds
    //! with the rest of the binary running in parallel.
    use super::*;
    use linux_object::process::LinuxProcess;
    use rcore_fs_ramfs::RamFS;
    use zircon_object::task::Process;

    fn a_periodic_timer_of(owner: KoID) -> usize {
        let id = NEXT_TIMER_ID.fetch_add(1, Ordering::Relaxed);
        POSIX_TIMERS.lock().insert(
            id,
            PosixTimer {
                owner,
                notify: TimerNotify::SIGALRM_TO_PROCESS,
                clock: ClockBase::Monotonic,
                interval: Duration::from_secs(1),
                next: Duration::from_secs(1),
                generation: 0,
                overrun_last: 0,
            },
        );
        id
    }

    fn still_there(id: usize) -> bool {
        POSIX_TIMERS.lock().contains_key(&id)
    }

    /// The death of a process built the way the kernel builds them, through
    /// `create_linux`, which is where the exit callback is registered.
    #[test]
    fn a_process_that_dies_takes_its_timers_with_it_and_nobody_elses() {
        let (mine, neighbour) = (0x4712_0001, 0x4712_0002);
        let proc = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, mine).unwrap();
        ensure_posix_timers_die_with_their_owner();
        let one = a_periodic_timer_of(mine);
        let two = a_periodic_timer_of(mine);
        let theirs = a_periodic_timer_of(neighbour);

        proc.exit(0);

        assert!(!still_there(one), "the dead process's timer is still armed");
        assert!(!still_there(two));
        assert!(
            still_there(theirs),
            "a death in one process took another process's timer"
        );
        POSIX_TIMERS.lock().remove(&theirs);
    }

    /// The hook is registered once however many timers are created: a second
    /// `timer_create` must not stack a second copy that would run at every
    /// death (the exit path of every process is not the place to grow).
    #[test]
    fn the_exit_hook_is_registered_once() {
        let before = linux_object::process::process_exit_hook_count();
        ensure_posix_timers_die_with_their_owner();
        ensure_posix_timers_die_with_their_owner();
        ensure_posix_timers_die_with_their_owner();
        let after = linux_object::process::process_exit_hook_count();
        assert!(after <= before + 1, "{} hooks stacked up", after - before);
        assert!(EXIT_HOOK_REGISTERED.load(Ordering::Acquire));
    }

    /// The callback of a timer whose owner has already died (exited, whether
    /// or not the parent has reaped it): the timer is deleted there and then,
    /// no signal goes anywhere, and there is no next expiry to arm.
    #[test]
    fn an_expiry_after_the_owners_death_deletes_the_timer_and_does_not_rearm() {
        let owner = 0x4712_0003;
        // Built without the exit callback, so the death alone leaves the
        // entry in place and it is the expiry that must clean up.
        let proc = Process::create_with_fixed_id_ext(
            &ROOT_JOB,
            owner,
            "p",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        // A thread that never ran keeps the process in the job after
        // `exit`: findable, `Exited`, its threads still dying. That is the
        // zombie the check has to call dead, not only a pid that names
        // nothing any more.
        let _thread = Thread::create_linux(&proc).unwrap();
        let id = a_periodic_timer_of(owner);
        proc.exit(0);
        assert!(still_there(id), "the setup: nothing else deleted it");
        assert!(
            matches!(
                ROOT_JOB.find_process(owner).map(|p| p.status()),
                Some(Status::Exited(_))
            ),
            "the setup: the owner is a zombie the job still lists"
        );

        let next = expire_posix_timer(id, 0, Duration::from_secs(1));

        assert_eq!(next, None, "a dead owner's periodic timer was re-armed");
        assert!(!still_there(id), "and its entry was kept");
    }

    /// A pid that names no process at all, which is what the callback sees
    /// once the dead process is gone from the job.
    #[test]
    fn an_expiry_for_a_pid_that_names_no_process_deletes_the_timer() {
        let id = a_periodic_timer_of(0x4712_0004);
        assert_eq!(expire_posix_timer(id, 0, Duration::from_secs(1)), None);
        assert!(!still_there(id));
    }

    /// The other side of the check: a live owner's periodic timer goes on
    /// as before, with the next expiry a period later and the entry kept.
    #[test]
    fn a_live_owners_periodic_timer_still_rearms() {
        let owner = 0x4712_0005;
        let _proc = Process::create_with_fixed_id_ext(
            &ROOT_JOB,
            owner,
            "p",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        let id = a_periodic_timer_of(owner);

        let next = expire_posix_timer(id, 0, Duration::from_secs(1));

        assert_eq!(next, Some(Duration::from_secs(2)));
        assert!(still_there(id), "a live owner's timer was deleted");
        POSIX_TIMERS.lock().remove(&id);
    }
}

#[cfg(test)]
mod overrun_tests {
    //! A periodic timer that fell behind fired once per missed period, back
    //! to back, and `timer_getoverrun` always said 0.

    use super::*;
    use linux_object::process::LinuxProcess;
    use linux_object::signal::SignalCode;
    use rcore_fs_ramfs::RamFS;
    use zircon_object::task::Process;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn a_process(pid: KoID) -> (alloc::sync::Arc<Process>, alloc::sync::Arc<Thread>) {
        let proc = Process::create_with_fixed_id_ext(
            &ROOT_JOB,
            pid,
            "t",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        let thread = Thread::create_linux(&proc).unwrap();
        (proc, thread)
    }

    /// A periodic POSIX timer of `owner` whose expiry was programmed at
    /// 1 s, every 10 ms.
    fn a_periodic_timer(owner: KoID) -> usize {
        let id = NEXT_TIMER_ID.fetch_add(1, Ordering::Relaxed);
        POSIX_TIMERS.lock().insert(
            id,
            PosixTimer {
                owner,
                notify: TimerNotify::SIGALRM_TO_PROCESS,
                clock: ClockBase::Monotonic,
                interval: ms(10),
                next: ms(1000),
                generation: 0,
                overrun_last: 0,
            },
        );
        id
    }

    /// `si_overrun` where glibc reads it.
    fn si_overrun(info: &SigInfo) -> i32 {
        let b = info.as_bytes();
        i32::from_ne_bytes([b[20], b[21], b[22], b[23]])
    }

    #[test]
    fn a_timer_on_time_steps_one_period_with_no_overrun() {
        assert_eq!(forward_periodic(ms(1000), ms(10), ms(1000)), (ms(1010), 0));
        assert_eq!(forward_periodic(ms(1000), ms(10), ms(1009)), (ms(1010), 0));
    }

    #[test]
    fn a_late_timer_skips_to_the_first_period_after_now_and_counts_the_rest() {
        // 3.5 periods late: this expiry, three skipped, next at 1040.
        assert_eq!(forward_periodic(ms(1000), ms(10), ms(1035)), (ms(1040), 3));
        // Exactly two periods late: the next expiry has to be AFTER now.
        assert_eq!(forward_periodic(ms(1000), ms(10), ms(1020)), (ms(1030), 2));
        // Ten seconds late on a 1 ms timer: one expiry, not ten thousand.
        let (next, overrun) = forward_periodic(ms(1000), ms(1), ms(11000));
        assert!(next > ms(11000), "next expiry {:?} already gone", next);
        assert_eq!(overrun, 10_000);
        // Nothing to overflow on: a 1 ns period a year behind.
        let (next, overrun) = forward_periodic(
            Duration::from_secs(1),
            Duration::from_nanos(1),
            Duration::from_secs(365 * 24 * 3600),
        );
        assert!(next > Duration::from_secs(365 * 24 * 3600));
        assert_eq!(overrun, DELAYTIMER_MAX);
    }

    #[test]
    fn a_late_expiry_delivers_one_signal_with_the_overrun_and_keeps_it_for_getoverrun() {
        let (proc, thread) = a_process(43_701);
        let id = a_periodic_timer(proc.id());
        // The wheel got to the 1 s expiry at 1.5 s: the expiries at 1010,
        // 1020, ... 1500 went by, fifty of them, and only this one fires.
        let next = expire_posix_timer(id, 0, ms(1500));
        assert_eq!(next, Some(ms(1510)), "the next expiry must be after now");
        let info = thread.lock_linux().take_siginfo(Signal::SIGALRM);
        assert_eq!(
            info.code,
            SignalCode::TIMER,
            "not SI_TIMER: a POSIX timer expiry"
        );
        assert_eq!(si_overrun(&info), 50, "si_overrun");
        let timers = POSIX_TIMERS.lock();
        let t = timers.get(&id).unwrap();
        assert_eq!(t.overrun_last, 50, "timer_getoverrun would answer this");
        assert_eq!(t.next, ms(1510));
        assert!(
            !thread.lock_linux().signals.contains(Signal::SIGALRM),
            "a second signal was delivered for the same expiry"
        );
    }

    #[test]
    fn a_one_shot_expiry_disarms_and_a_stale_generation_fires_nothing() {
        let (proc, thread) = a_process(43_702);
        let id = a_periodic_timer(proc.id());
        POSIX_TIMERS.lock().get_mut(&id).unwrap().interval = Duration::ZERO;
        assert_eq!(expire_posix_timer(id, 0, ms(1500)), None);
        assert!(thread.lock_linux().signals.contains(Signal::SIGALRM));
        assert_eq!(POSIX_TIMERS.lock().get(&id).unwrap().next, Duration::ZERO);

        let id = a_periodic_timer(proc.id());
        POSIX_TIMERS.lock().get_mut(&id).unwrap().generation = 3;
        thread.lock_linux().take_siginfo(Signal::SIGALRM);
        assert_eq!(expire_posix_timer(id, 0, ms(1500)), None);
        assert!(!thread.lock_linux().signals.contains(Signal::SIGALRM));
    }

    #[test]
    fn a_late_interval_timer_fires_once_and_lands_after_now() {
        let (proc, thread) = a_process(43_703);
        {
            let lp = proc.linux();
            let mut slots = lp.itimers().lock();
            slots[ITIMER_REAL] = ItimerSlot {
                interval: ms(10),
                deadline: Some(ms(1000)),
                generation: 5,
            };
        }
        let next = expire_itimer(proc.id(), ITIMER_REAL, 5, ms(1500));
        assert_eq!(next, Some(ms(1510)), "the next expiry must be after now");
        assert_eq!(
            proc.linux().itimers().lock()[ITIMER_REAL].deadline,
            Some(ms(1510))
        );
        assert!(thread.lock_linux().signals.contains(Signal::SIGALRM));
        // A stale generation is a slot that was re-armed since: nothing.
        assert_eq!(expire_itimer(proc.id(), ITIMER_REAL, 4, ms(1600)), None);
    }
}

#[cfg(test)]
mod timer_signal_tests {
    //! A timer that expired set its signal's bit on EVERY thread of the
    //! process, with no `siginfo_t`; `timer_create` read neither
    //! `sigev_value` nor the thread of `SIGEV_THREAD_ID`, and took any
    //! `sigev_signo` or `sigev_notify`.

    use super::*;
    use linux_object::process::LinuxProcess;
    use linux_object::signal::SignalCode;
    use rcore_fs_ramfs::RamFS;
    use zircon_object::task::Process;

    fn a_process_with_two_threads(
        pid: KoID,
    ) -> (alloc::sync::Arc<Process>, [alloc::sync::Arc<Thread>; 2]) {
        let proc = Process::create_with_fixed_id_ext(
            &ROOT_JOB,
            pid,
            "t",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        let a = Thread::create_linux(&proc).unwrap();
        let b = Thread::create_linux(&proc).unwrap();
        (proc, [a, b])
    }

    fn has_pending(thread: &Thread, signal: Signal) -> bool {
        thread.lock_linux().signals.contains(signal)
    }

    /// `(si_code, si_timerid, si_overrun, si_value)` where glibc reads them.
    fn timer_fields(info: &SigInfo) -> (SignalCode, i32, i32, usize) {
        let b = info.as_bytes();
        let word = |at: usize| i32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        let mut v = [0u8; core::mem::size_of::<usize>()];
        let n = v.len();
        v.copy_from_slice(&b[24..24 + n]);
        (info.code, word(16), word(20), usize::from_ne_bytes(v))
    }

    fn event(value: usize, signo: i32, notify: i32, thread_id: i32) -> SigEvent {
        SigEvent {
            value,
            signo,
            notify,
            thread_id,
        }
    }

    #[test]
    fn an_expired_timer_signals_one_thread_not_every_thread() {
        let (proc, threads) = a_process_with_two_threads(43_301);
        let (signal, info) = itimer_expiry_signal(ITIMER_REAL).unwrap();
        deliver_timer_signal(proc.id(), None, signal, info);
        let hit = threads
            .iter()
            .filter(|t| has_pending(t, Signal::SIGALRM))
            .count();
        assert_eq!(hit, 1, "SIGALRM pending on {} of 2 threads", hit);
    }

    #[test]
    fn sigev_thread_id_goes_to_that_thread_with_si_timer_and_the_value() {
        let (proc, [other, target]) = a_process_with_two_threads(43_302);
        let notify = TimerNotify {
            signo: 34,
            value: 0xfeed_f00d,
            thread: Some(target.id()),
        };
        let (signal, info) = posix_timer_expiry_signal(7, notify, 0).unwrap();
        deliver_timer_signal(proc.id(), notify.thread, signal, info);
        assert!(
            !has_pending(&other, signal),
            "delivered to the wrong thread"
        );
        let got = target.lock_linux().take_siginfo(signal);
        assert_eq!(
            timer_fields(&got),
            (SignalCode::TIMER, 7, 0, 0xfeed_f00d),
            "not the SI_TIMER glibc's helper thread looks for"
        );
    }

    #[test]
    fn an_interval_timer_signal_comes_from_the_kernel_not_from_a_process() {
        let (signal, info) = itimer_expiry_signal(ITIMER_PROF).unwrap();
        assert_eq!(signal, Signal::SIGPROF);
        assert_eq!(timer_fields(&info), (SignalCode::KERNEL, 0, 0, 0));
    }

    #[test]
    fn sigev_none_arms_a_timer_that_delivers_nothing() {
        let notify = timer_notify_from_sigevent(&event(5, 0, SIGEV_NONE, 0), |_| false).unwrap();
        assert_eq!(notify.signo, 0);
        assert!(posix_timer_expiry_signal(1, notify, 0).is_none());
    }

    #[test]
    fn the_sigevent_has_to_name_a_real_signal_and_a_known_notify() {
        // `sigev_signo` 0 used to be "no signal" and 200 was silently
        // dropped at expiry; `good_sigevent` refuses both up front.
        for signo in [0, -1, SIGRTMAX + 1, 200] {
            assert_eq!(
                timer_notify_from_sigevent(&event(0, signo, SIGEV_SIGNAL, 0), |_| true).err(),
                Some(LxError::EINVAL),
                "signo {}",
                signo
            );
        }
        for notify in [3, 5, 8, -1] {
            assert_eq!(
                timer_notify_from_sigevent(&event(0, 14, notify, 0), |_| true).err(),
                Some(LxError::EINVAL),
                "notify {}",
                notify
            );
        }
        let ok =
            timer_notify_from_sigevent(&event(9, SIGRTMAX, SIGEV_SIGNAL, 0), |_| false).unwrap();
        assert_eq!(
            ok,
            TimerNotify {
                signo: 64,
                value: 9,
                thread: None
            }
        );
    }

    #[test]
    fn sigev_thread_id_must_be_a_thread_of_the_caller() {
        let mine = |tid: KoID| tid == 77;
        assert_eq!(
            timer_notify_from_sigevent(&event(0, 34, SIGEV_THREAD_ID, 78), mine).err(),
            Some(LxError::EINVAL)
        );
        assert_eq!(
            timer_notify_from_sigevent(&event(0, 34, SIGEV_THREAD_ID, 0), mine).err(),
            Some(LxError::EINVAL)
        );
        let ok = timer_notify_from_sigevent(&event(1, 34, SIGEV_THREAD_ID, 77), mine).unwrap();
        assert_eq!(ok.thread, Some(77));
        assert_eq!((ok.signo, ok.value), (34, 1));
    }
}
