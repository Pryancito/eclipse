//! timerfd — temporizadores vía descriptor (compatible con Linux en lo esencial).
//!
//! Semántica: `timerfd_create` + `timerfd_settime` / `timerfd_gettime`, lectura de 8 bytes
//! (u64 nativo-endian, número de expiraciones desde la última lectura) y `poll(POLLIN)`
//! cuando hay expiraciones pendientes.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use spin::Mutex;

use crate::scheme::error;
use crate::scheme::{Scheme, Stat};
use crate::scheme::event;

/// Linux `TFD_TIMER_ABSTIME`
pub const TFD_TIMER_ABSTIME: i32 = 1;
/// Linux `TFD_NONBLOCK`
pub const TFD_NONBLOCK: u32 = 0x800;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Itimerspec {
    pub it_interval: Timespec,
    pub it_value: Timespec,
}

struct TimerState {
    clockid: u32,
    nonblock: bool,
    refcnt: u32,
    armed: bool,
    /// Periodo entre expiraciones (0 = one-shot).
    interval_ms: u64,
    /// Próxima expiración en la misma escala que `now_ms()` para este `clockid`.
    deadline_ms: u64,
    pending: u64,
}

impl TimerState {
    fn now_pair(&self) -> (u64, u64) {
        let uptime = crate::scheduler::get_stats().total_ticks;
        let wall = crate::syscalls::WALL_TIME_OFFSET
            .load(Ordering::Relaxed)
            .saturating_mul(1000)
            .saturating_add(uptime);
        (uptime, wall)
    }

    fn now_for_clock(&self) -> u64 {
        let (uptime, wall) = self.now_pair();
        match self.clockid {
            0 => wall,
            _ => uptime,
        }
    }

    /// Avanza expiraciones hasta el instante actual.
    fn advance(&mut self) {
        if !self.armed {
            return;
        }
        let now = self.now_for_clock();
        while self.armed && now >= self.deadline_ms {
            self.pending = self.pending.saturating_add(1);
            if self.interval_ms == 0 {
                self.armed = false;
                break;
            }
            self.deadline_ms = self.deadline_ms.saturating_add(self.interval_ms);
        }
    }

    fn itimerspec_remaining(&self) -> Itimerspec {
        let mut cur = Itimerspec::default();
        cur.it_interval = timespec_from_ms(self.interval_ms);
        if !self.armed {
            return cur;
        }
        let now = self.now_for_clock();
        let rem_ms = self.deadline_ms.saturating_sub(now);
        cur.it_value = timespec_from_ms(rem_ms);
        cur
    }
}

fn timespec_from_ms(ms: u64) -> Timespec {
    Timespec {
        tv_sec: (ms / 1000) as i64,
        tv_nsec: ((ms % 1000) * 1_000_000) as i64,
    }
}

fn timespec_to_ms(ts: Timespec) -> Result<u64, usize> {
    if ts.tv_sec < 0 || ts.tv_nsec < 0 {
        return Err(error::EINVAL);
    }
    if ts.tv_nsec >= 1_000_000_000 {
        return Err(error::EINVAL);
    }
    let s = ts.tv_sec as u64;
    let ns = ts.tv_nsec as u64;
    Ok(s.saturating_mul(1000).saturating_add(ns / 1_000_000))
}

fn is_monotonic_like(clockid: u32) -> bool {
    matches!(clockid, 1 | 4 | 7)
}

pub struct TimerFdScheme {
    instances: Mutex<BTreeMap<usize, TimerState>>,
    next_id: Mutex<usize>,
}

impl TimerFdScheme {
    pub fn new() -> Self {
        Self {
            instances: Mutex::new(BTreeMap::new()),
            next_id: Mutex::new(1),
        }
    }

    pub fn settime(
        &self,
        id: usize,
        flags: i32,
        new: &Itimerspec,
        old_user_ptr: u64,
    ) -> Result<(), usize> {
        let interval_ms = timespec_to_ms(new.it_interval)?;
        let value_ms = timespec_to_ms(new.it_value)?;

        let mut map = self.instances.lock();
        let t = map.get_mut(&id).ok_or(error::EBADF)?;

        t.advance();

        if old_user_ptr != 0 {
            if !crate::syscalls::is_user_pointer(old_user_ptr, core::mem::size_of::<Itimerspec>() as u64) {
                return Err(error::EFAULT);
            }
            let old = t.itimerspec_remaining();
            unsafe {
                core::ptr::write_unaligned(old_user_ptr as *mut Itimerspec, old);
            }
        }

        t.interval_ms = interval_ms;

        // Ambos ceros → desarmar (conservamos interval_ms como en Linux para gettime).
        if value_ms == 0 && new.it_value.tv_sec == 0 && new.it_value.tv_nsec == 0 {
            t.armed = false;
            return Ok(());
        }

        let (uptime, wall) = t.now_pair();
        let now = match t.clockid {
            0 => wall,
            _ => uptime,
        };

        let deadline_ms = if (flags & TFD_TIMER_ABSTIME) != 0 {
            if t.clockid == 0 {
                value_ms
            } else if is_monotonic_like(t.clockid) {
                value_ms
            } else {
                return Err(error::EINVAL);
            }
        } else {
            now.saturating_add(value_ms)
        };

        t.deadline_ms = deadline_ms;
        t.armed = true;
        t.advance();
        Ok(())
    }

    pub fn gettime(&self, id: usize, out_user_ptr: u64) -> Result<(), usize> {
        if !crate::syscalls::is_user_pointer(out_user_ptr, core::mem::size_of::<Itimerspec>() as u64) {
            return Err(error::EFAULT);
        }
        let mut map = self.instances.lock();
        let t = map.get_mut(&id).ok_or(error::EBADF)?;
        t.advance();
        let cur = t.itimerspec_remaining();
        unsafe {
            core::ptr::write_unaligned(out_user_ptr as *mut Itimerspec, cur);
        }
        Ok(())
    }
}

impl Scheme for TimerFdScheme {
    fn open(&self, path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        // Ruta relativa tras `timerfd:`: `<clockid>/<flags>` (flags decimales, p. ej. TFD_NONBLOCK=0x800).
        let parts: Vec<&str> = path.split('/').collect();
        let clockid: u32 = parts
            .first()
            .and_then(|s| s.parse().ok())
            .ok_or(error::EINVAL)?;
        let create_flags: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);

        if clockid != 0 && !is_monotonic_like(clockid) {
            return Err(error::EINVAL);
        }

        let mut id_gen = self.next_id.lock();
        let id = *id_gen;
        *id_gen += 1;

        self.instances.lock().insert(
            id,
            TimerState {
                clockid,
                nonblock: (create_flags & TFD_NONBLOCK) != 0,
                refcnt: 1,
                armed: false,
                interval_ms: 0,
                deadline_ms: 0,
                pending: 0,
            },
        );
        Ok(id)
    }

    fn read(&self, id: usize, buffer: &mut [u8], _offset: u64) -> Result<usize, usize> {
        if buffer.len() < 8 {
            return Err(error::EINVAL);
        }
        loop {
            let mut map = self.instances.lock();
            let t = map.get_mut(&id).ok_or(error::EBADF)?;
            t.advance();
            if t.pending > 0 {
                let v = t.pending;
                t.pending = 0;
                buffer[..8].copy_from_slice(&v.to_ne_bytes());
                return Ok(8);
            }
            if t.nonblock {
                return Err(error::EAGAIN);
            }
            drop(map);
            crate::scheduler::yield_cpu();
        }
    }

    fn write(&self, _id: usize, _buffer: &[u8], _offset: u64) -> Result<usize, usize> {
        Err(error::EINVAL)
    }

    fn close(&self, id: usize) -> Result<usize, usize> {
        let mut map = self.instances.lock();
        let remove = {
            let t = map.get_mut(&id).ok_or(error::EBADF)?;
            t.refcnt = t.refcnt.saturating_sub(1);
            t.refcnt == 0
        };
        if remove {
            map.remove(&id);
        }
        Ok(0)
    }

    fn fstat(&self, _id: usize, stat: &mut Stat) -> Result<usize, usize> {
        stat.mode = 0o100644;
        stat.size = 0;
        Ok(0)
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize, _current_offset: u64) -> Result<usize, usize> {
        Err(error::ESPIPE)
    }

    fn poll(&self, id: usize, events: usize) -> Result<usize, usize> {
        let mut map = self.instances.lock();
        let t = map.get_mut(&id).ok_or(error::EBADF)?;
        t.advance();
        let mut ready = 0;
        if (events & event::POLLIN) != 0 && t.pending > 0 {
            ready |= event::POLLIN;
        }
        Ok(ready)
    }

    fn dup(&self, id: usize) -> Result<usize, usize> {
        let mut map = self.instances.lock();
        let t = map.get_mut(&id).ok_or(error::EBADF)?;
        t.refcnt = t.refcnt.saturating_add(1);
        Ok(0)
    }
}

static TIMERFD_SCHEME: spin::Once<Arc<TimerFdScheme>> = spin::Once::new();

pub fn get_timerfd_scheme() -> &'static Arc<TimerFdScheme> {
    TIMERFD_SCHEME.call_once(|| Arc::new(TimerFdScheme::new()))
}
