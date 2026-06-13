//! Syscalls for time
//! - clock_gettime
//!
use crate::Syscall;
use alloc::boxed::Box;
use core::time::Duration;
use kernel_hal::{user::UserInPtr, user::UserOutPtr};
use linux_object::error::{LxError, SysResult};
use linux_object::signal::Signal;
use linux_object::thread::ThreadExt;
use linux_object::time::*;
use zircon_object::object::KernelObject;
use zircon_object::task::Thread;

const USEC_PER_TICK: usize = 10000;

impl Syscall<'_> {
    /// finds the resolution (precision) of the specified clock clockid, and,
    /// if buffer is non-NULL, stores it in the struct timespec pointed to by buffer
    pub fn sys_clock_gettime(&self, clock: usize, mut buf: UserOutPtr<TimeSpec>) -> SysResult {
        info!("clock_gettime: id={:?} buf={:?}", clock, buf);
        if buf.is_null() {
            return Err(LxError::EINVAL);
        }
        let ts = match clock {
            0 | 5 => TimeSpec::now(), // CLOCK_REALTIME, CLOCK_REALTIME_COARSE
            1 | 4 | 6 | 7 => TimeSpec::now_monotonic(),
            _ => return Err(LxError::EINVAL),
        };
        buf.write(ts)?;

        info!("TimeSpec: {:?}", ts);

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
        let timeval = tv.read()?;
        let target = Duration::new(timeval.sec as u64, timeval.usec as u32 * 1_000);
        kernel_hal::timer::wall_clock_set(target);
        Ok(0)
    }

    /// get the time with second and microseconds
    pub fn sys_gettimeofday(
        &mut self,
        mut tv: UserOutPtr<TimeVal>,
        tz: UserInPtr<u8>,
    ) -> SysResult {
        info!("gettimeofday: tv: {:?}, tz: {:?}", tv, tz);
        // don't support tz
        if !tz.is_null() {
            return Err(LxError::EINVAL);
        }

        let timeval = TimeVal::now();
        tv.write(timeval)?;

        info!("TimeVal: {:?}", timeval);

        Ok(0)
    }

    /// get time in seconds
    #[cfg(target_arch = "x86_64")]
    pub fn sys_time(&mut self, mut time: UserOutPtr<u64>) -> SysResult {
        info!("time: time: {:?}", time);
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
    /// currently only support ru_utime and ru_stime:
    /// - `ru_utime`: user CPU time used
    /// - `ru_stime`: system CPU time used
    pub fn sys_getrusage(&mut self, who: usize, mut rusage: UserOutPtr<RUsage>) -> SysResult {
        info!("getrusage: who: {}, rusage: {:?}", who, rusage);
        if rusage.is_null() {
            return Err(LxError::EINVAL);
        }
        let new_rusage = RUsage {
            utime: TimeVal::now(),
            stime: TimeVal::now(),
        };
        rusage.write(new_rusage)?;
        Ok(0)
    }

    /// stores the current process times in the struct tms that buf points to
    pub fn sys_times(&mut self, mut buf: UserOutPtr<Tms>) -> SysResult {
        info!("times: buf: {:?}", buf);

        let tv = TimeVal::now();

        let tick = (tv.sec * 1_000_000 + tv.usec) / USEC_PER_TICK;

        if !buf.is_null() {
            let new_buf = Tms {
                tms_utime: 0,
                tms_stime: 0,
                tms_cutime: 0,
                tms_cstime: 0,
            };
            buf.write(new_buf)?;
        } else {
            warn!("sys_times: Invalid buf {:x?}", buf);
        }

        info!("tick: {:?}", tick);
        Ok(tick as usize)
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
            "clock_nanosleep: clockid={:?}, flags={:?}, req={:?}, rem={:?}",
            clockid,
            flags,
            req.read()?,
            rem
        );
        use core::time::Duration;
        use kernel_hal::{thread, timer};
        let duration: Duration = req.read()?.into();
        let clockid = ClockId::from(clockid);
        let flags = ClockFlags::from(flags);
        info!("clockid={:?}, flags={:?}", clockid, flags,);
        match clockid {
            ClockId::ClockRealTime => {
                match flags {
                    ClockFlags::ZeroFlag => {
                        thread::sleep_until(timer::deadline_after(duration)).await;
                    }
                    ClockFlags::TimerAbsTime => {
                        // 目前统一由nanosleep代替了、之后再修改
                        thread::sleep_until(timer::deadline_after(duration)).await;
                    }
                }
            }
            ClockId::ClockMonotonic => match flags {
                ClockFlags::ZeroFlag => {
                    thread::sleep_until(timer::deadline_after(duration)).await;
                }
                ClockFlags::TimerAbsTime => {
                    thread::sleep_until(timer::deadline_after(duration)).await;
                }
            },
            ClockId::ClockProcessCpuTimeId => {}
            ClockId::ClockThreadCpuTimeId => {}
            ClockId::ClockMonotonicRaw => {}
            ClockId::ClockRealTimeCoarse => {}
            ClockId::ClockMonotonicCoarse => {}
            ClockId::ClockBootTime => {}
            ClockId::ClockRealTimeAlarm => {}
            ClockId::ClockBootTimeAlarm => {}
        }
        Ok(0)
    }

    /// set value of an interval timer
    pub fn sys_setitimer(
        &mut self,
        which: usize,
        new_value: UserInPtr<ITimerVal>,
        mut old_value: UserOutPtr<ITimerVal>,
    ) -> SysResult {
        info!(
            "setitimer: which={}, new_value={:?}, old_value={:?}",
            which,
            new_value.read_if_not_null()?,
            old_value
        );
        let val = new_value.read()?;
        if val.value.sec != 0 || val.value.usec != 0 {
            let duration = Duration::from_secs(val.value.sec as u64)
                + Duration::from_micros(val.value.usec as u64);
            let deadline = kernel_hal::timer::timer_now() + duration;
            let proc = self.zircon_process().clone();
            kernel_hal::timer::timer_set(
                deadline,
                Box::new(move |_| {
                    let tids = proc.thread_ids();
                    for tid in tids {
                        if let Ok(obj) = proc.get_child(tid) {
                            if let Ok(thread) = obj.downcast_arc::<Thread>() {
                                thread.lock_linux().signals.insert(Signal::SIGALRM);
                                thread.signal_set(zircon_object::object::Signal::USER_SIGNAL_0);
                            }
                        }
                    }
                }),
            );
        }
        if !old_value.is_null() {
            old_value.write(ITimerVal::default())?;
        }
        Ok(0)
    }

    /// Schedule SIGALRM (busybox `ping` uses this for read timeouts).
    pub fn sys_alarm(&self, seconds: usize) -> SysResult {
        if seconds == 0 {
            return Ok(0);
        }
        let duration = Duration::from_secs(seconds as u64);
        let deadline = kernel_hal::timer::timer_now() + duration;
        let proc = self.zircon_process().clone();
        kernel_hal::timer::timer_set(
            deadline,
            Box::new(move |_| {
                for tid in proc.thread_ids() {
                    if let Ok(obj) = proc.get_child(tid) {
                        if let Ok(thread) = obj.downcast_arc::<Thread>() {
                            thread.lock_linux().signals.insert(Signal::SIGALRM);
                            thread.signal_set(zircon_object::object::Signal::USER_SIGNAL_0);
                        }
                    }
                }
            }),
        );
        Ok(0)
    }
}
