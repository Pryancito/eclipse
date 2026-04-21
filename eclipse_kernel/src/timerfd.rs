//! timerfd support for Eclipse OS.
//!
//! Provides Linux-compatible `timerfd_create` / `timerfd_settime` / `timerfd_gettime`
//! functionality as a kernel Scheme.  Timers are polled via the global tick counter so
//! no dedicated hardware timer is required beyond the existing PIT/APIC interrupt.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use spin::Mutex;
use crate::scheme::{Scheme, Stat, error};

// ── Timespec / itimerspec ABI structs (matches Linux x86_64) ──────────────────

/// Linux `struct timespec` (64-bit version — tv_sec and tv_nsec are both 64-bit).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

impl Timespec {
    /// Convert to milliseconds (saturating).
    #[inline]
    fn to_ms(&self) -> u64 {
        if self.tv_sec < 0 {
            return 0;
        }
        (self.tv_sec as u64)
            .saturating_mul(1000)
            .saturating_add((self.tv_nsec.max(0) as u64) / 1_000_000)
    }

    /// Build a Timespec from milliseconds.
    #[inline]
    fn from_ms(ms: u64) -> Self {
        Self {
            tv_sec: (ms / 1000) as i64,
            tv_nsec: ((ms % 1000) * 1_000_000) as i64,
        }
    }

    /// True if both fields are zero (disarmed / zero interval).
    #[inline]
    fn is_zero(&self) -> bool {
        self.tv_sec == 0 && self.tv_nsec == 0
    }
}

/// Linux `struct itimerspec`.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Itimerspec {
    /// Interval for periodic timer (0 = one-shot).
    pub it_interval: Timespec,
    /// Initial expiry time (0 = disarmed).
    pub it_value: Timespec,
}

// ── Internal timer state ───────────────────────────────────────────────────────

struct TimerFdState {
    /// Clock ID passed to `timerfd_create`.
    _clockid: u32,
    /// Tick count at which the current armed window started.
    armed_at_ticks: u64,
    /// Duration until first expiry in milliseconds (0 = disarmed).
    value_ms: u64,
    /// Periodic interval in milliseconds (0 = one-shot).
    interval_ms: u64,
    /// Number of expirations that have not yet been consumed by `read`.
    expirations: u64,
}

impl TimerFdState {
    fn new(clockid: u32) -> Self {
        Self {
            _clockid: clockid,
            armed_at_ticks: 0,
            value_ms: 0,
            interval_ms: 0,
            expirations: 0,
        }
    }

    /// Return current kernel tick count (10 ms per tick by default).
    fn now_ms() -> u64 {
        // ticks() returns the raw PIT/APIC counter; each tick ≈ 1 ms (or 10 ms —
        // depends on the configured IRQ0 rate).  We use it as a monotonic counter.
        crate::interrupts::ticks()
    }

    /// Refresh internal expiration count based on current time.  Called before
    /// `read` or `poll` to account for elapsed wall-clock time.
    fn refresh(&mut self) {
        if self.value_ms == 0 {
            return; // disarmed
        }
        let now = Self::now_ms();
        let elapsed = now.saturating_sub(self.armed_at_ticks);

        if elapsed < self.value_ms {
            return; // not yet expired
        }

        // Compute how many expirations have occurred since last refresh.
        // First expiry at armed_at_ticks + value_ms, then every interval_ms.
        let after_first = elapsed - self.value_ms;
        let new_count: u64 = if self.interval_ms == 0 {
            // One-shot: disarm after the first expiry.
            self.value_ms = 0;
            1
        } else {
            let extra = after_first / self.interval_ms;
            // Advance arm point so next poll computes correctly.
            self.armed_at_ticks += self.value_ms + extra * self.interval_ms;
            self.value_ms = self.interval_ms; // ongoing periodic
            1 + extra
        };

        self.expirations = self.expirations.saturating_add(new_count);
    }
}

// ── TimerFdScheme ──────────────────────────────────────────────────────────────

pub struct TimerFdScheme {
    timers: Mutex<BTreeMap<usize, TimerFdState>>,
    next_id: Mutex<usize>,
}

impl TimerFdScheme {
    pub fn new() -> Self {
        Self {
            timers: Mutex::new(BTreeMap::new()),
            next_id: Mutex::new(1),
        }
    }

    // ── Extra methods called directly from syscalls.rs ─────────────────────

    /// `timerfd_settime(fd, flags, new, old_ptr)`.
    ///
    /// `flags` 0 = relative, 1 = `TFD_TIMER_ABSTIME`.
    /// `old_ptr` if non-zero receives the previous `itimerspec`.
    pub fn settime(
        &self,
        id: usize,
        _flags: i32,
        new: &Itimerspec,
        old_ptr: u64,
    ) -> Result<(), usize> {
        let mut timers = self.timers.lock();
        let state = timers.get_mut(&id).ok_or(error::EBADF)?;

        // Optionally write the old value back to userspace.
        if old_ptr != 0 {
            let old = Itimerspec {
                it_interval: Timespec::from_ms(state.interval_ms),
                it_value: Timespec::from_ms(state.value_ms),
            };
            if crate::syscalls::is_user_pointer(old_ptr, core::mem::size_of::<Itimerspec>() as u64) {
                unsafe {
                    core::ptr::write_unaligned(old_ptr as *mut Itimerspec, old);
                }
            }
        }

        let value_ms = new.it_value.to_ms();
        let interval_ms = new.it_interval.to_ms();

        state.expirations = 0;
        state.value_ms = value_ms;
        state.interval_ms = interval_ms;
        state.armed_at_ticks = if value_ms > 0 { TimerFdState::now_ms() } else { 0 };

        Ok(())
    }

    /// `timerfd_gettime(fd, curr_ptr)` — writes current `itimerspec` into userspace.
    pub fn gettime(&self, id: usize, curr_ptr: u64) -> Result<(), usize> {
        let mut timers = self.timers.lock();
        let state = timers.get_mut(&id).ok_or(error::EBADF)?;
        state.refresh();

        if !crate::syscalls::is_user_pointer(curr_ptr, core::mem::size_of::<Itimerspec>() as u64) {
            return Err(error::EFAULT);
        }

        // it_value: remaining time until next expiry.
        let remaining_ms = if state.value_ms > 0 {
            let elapsed = TimerFdState::now_ms().saturating_sub(state.armed_at_ticks);
            state.value_ms.saturating_sub(elapsed)
        } else {
            0
        };

        let curr = Itimerspec {
            it_interval: Timespec::from_ms(state.interval_ms),
            it_value: Timespec::from_ms(remaining_ms),
        };
        unsafe {
            core::ptr::write_unaligned(curr_ptr as *mut Itimerspec, curr);
        }
        Ok(())
    }
}

impl Scheme for TimerFdScheme {
    /// Path format (after stripping "timerfd:"): `<clockid>/<flags>`
    fn open(&self, path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        let parts: alloc::vec::Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let clockid: u32 = parts.first().and_then(|&s| s.parse().ok()).unwrap_or(1);

        let mut id_gen = self.next_id.lock();
        let id = *id_gen;
        *id_gen += 1;

        self.timers.lock().insert(id, TimerFdState::new(clockid));
        Ok(id)
    }

    /// `read(2)` on a timerfd returns an 8-byte little-endian expiry count.
    /// Returns `EAGAIN` if the timer has not yet expired.
    fn read(&self, id: usize, buffer: &mut [u8], _offset: u64) -> Result<usize, usize> {
        if buffer.len() < 8 {
            return Err(error::EINVAL);
        }
        let mut timers = self.timers.lock();
        let state = timers.get_mut(&id).ok_or(error::EBADF)?;
        state.refresh();

        if state.expirations == 0 {
            return Err(error::EAGAIN);
        }

        let count = state.expirations;
        state.expirations = 0;
        buffer[..8].copy_from_slice(&count.to_ne_bytes());
        Ok(8)
    }

    /// Writes are not allowed on timerfd.
    fn write(&self, _id: usize, _buffer: &[u8], _offset: u64) -> Result<usize, usize> {
        Err(error::EBADF)
    }

    fn close(&self, id: usize) -> Result<usize, usize> {
        if self.timers.lock().remove(&id).is_some() {
            Ok(0)
        } else {
            Err(error::EBADF)
        }
    }

    fn fstat(&self, _id: usize, stat: &mut Stat) -> Result<usize, usize> {
        // Report as a regular file; no special device type needed here.
        stat.mode = 0o100600;
        Ok(0)
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize, _current_offset: u64) -> Result<usize, usize> {
        Err(error::ESPIPE)
    }

    /// `poll` / `epoll` readiness: POLLIN when at least one expiry is pending.
    fn poll(&self, id: usize, events: usize) -> Result<usize, usize> {
        let mut timers = self.timers.lock();
        let state = timers.get_mut(&id).ok_or(error::EBADF)?;
        state.refresh();

        let mut ready = 0;
        if (events & crate::scheme::event::POLLIN) != 0 && state.expirations > 0 {
            ready |= crate::scheme::event::POLLIN;
        }
        Ok(ready)
    }
}

// ── Singleton ──────────────────────────────────────────────────────────────────

pub static TIMERFD_SCHEME: spin::Once<Arc<TimerFdScheme>> =
    spin::Once::new();

pub fn get_timerfd_scheme() -> &'static Arc<TimerFdScheme> {
    TIMERFD_SCHEME.call_once(|| Arc::new(TimerFdScheme::new()))
}
