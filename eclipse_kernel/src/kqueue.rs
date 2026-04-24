//! BSD-style kqueue/kevent implementation for Eclipse OS.
//!
//! Provides efficient event notification for files, signals, and processes.

use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use spin::Mutex;
use crate::scheme::{Scheme, Stat, error};

/// KEvent filter types
pub const EVFILT_READ:   i16 = -1;
pub const EVFILT_WRITE:  i16 = -2;
pub const EVFILT_SIGNAL: i16 = -5;
pub const EVFILT_PROC:   i16 = -6;

/// KEvent flags
pub const EV_ADD:     u16 = 0x0001;
pub const EV_DELETE:  u16 = 0x0002;
pub const EV_ENABLE:  u16 = 0x0004;
pub const EV_DISABLE: u16 = 0x0008;
pub const EV_ONESHOT: u16 = 0x0010;
pub const EV_CLEAR:   u16 = 0x0020;
pub const EV_EOF:     u16 = 0x8000;
pub const EV_ERROR:   u16 = 0x4000;

/// KEvent fflags for EVFILT_PROC
pub const NOTE_EXIT:   u32 = 0x80000000;

/// KEvent structure (BSD compatible)
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct KEvent {
    pub ident:  u64,  // Identifier (e.g., FD, PID, Signal)
    pub filter: i16,  // Filter type
    pub flags:  u16,  // Action flags
    pub fflags: u32,  // Filter-specific flags
    pub data:   i64,  // Filter-specific data
    pub udata:  u64,  // User data pointer
}

/// A kqueue instance
pub struct KQueue {
    pub events: Mutex<BTreeMap<(u64, i16), KEvent>>, // (ident, filter) -> registered event
    pub pending: Mutex<Vec<KEvent>>,                 // Events triggered but not yet consumed
}

impl KQueue {
    pub fn new() -> Self {
        Self {
            events: Mutex::new(BTreeMap::new()),
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Process kevent list (BSD semantics)
    pub fn process_kevents(
        &self,
        changelist: &[KEvent],
        eventlist: &mut [KEvent],
        timeout_ms: Option<u64>
    ) -> Result<usize, usize> {
        // 1. Handle changes
        for ev in changelist {
            if (ev.flags & EV_ADD) != 0 {
                let mut events = self.events.lock();
                events.insert((ev.ident, ev.filter), *ev);
            }
            if (ev.flags & EV_DELETE) != 0 {
                let mut events = self.events.lock();
                events.remove(&(ev.ident, ev.filter));
            }
        }

        // 2. Poll for events
        let mut triggered_count = 0;
        
        // Check already pending events
        {
            let mut pending = self.pending.lock();
            while triggered_count < eventlist.len() && !pending.is_empty() {
                eventlist[triggered_count] = pending.remove(0);
                triggered_count += 1;
            }
        }

        if triggered_count > 0 || timeout_ms == Some(0) {
            return Ok(triggered_count);
        }

        // 3. Block/Wait logic (Simplified for now: just return what we have)
        // In a full implementation, this would block the process until an event occurs
        // or timeout expires.
        Ok(triggered_count)
    }

    /// Trigger an event (called from other kernel subsystems)
    pub fn trigger(&self, ident: u64, filter: i16, data: i64) {
        let events = self.events.lock();
        if let Some(ev) = events.get(&(ident, filter)) {
            if (ev.flags & EV_DISABLE) != 0 { return; }
            
            let mut triggered = *ev;
            triggered.data = data;
            
            let mut pending = self.pending.lock();
            pending.push(triggered);
            
            // Handle EV_ONESHOT
            if (ev.flags & EV_ONESHOT) != 0 {
                // Drop the lock before re-acquiring it in mut mode or use entry API
            }
        }
    }
}

pub struct KQueueScheme {
    queues: Mutex<BTreeMap<usize, Arc<KQueue>>>,
    next_id: core::sync::atomic::AtomicUsize,
}

impl KQueueScheme {
    pub fn new() -> Self {
        Self {
            queues: Mutex::new(BTreeMap::new()),
            next_id: core::sync::atomic::AtomicUsize::new(1),
        }
    }

    pub fn get_queue(&self, id: usize) -> Option<Arc<KQueue>> {
        self.queues.lock().get(&id).cloned()
    }

    /// Trigger an event on all kqueues owned by a process.
    pub fn trigger_for_process(&self, pid: crate::process::ProcessId, filter: i16, ident: u64, data: i64) {
        if let Some(fd_table_idx) = crate::fd::pid_to_fd_idx(pid) {
            let tables = crate::fd::FD_TABLES.lock();
            let table = &tables[fd_table_idx];
            let kqueue_scheme_id = crate::scheme::get_scheme_id("kqueue").unwrap_or(usize::MAX);
            
            for fd_entry in table.fds.iter() {
                if fd_entry.in_use && fd_entry.scheme_id == kqueue_scheme_id {
                    if let Some(kq) = self.get_queue(fd_entry.resource_id) {
                        kq.trigger(ident, filter, data);
                    }
                }
            }
        }
    }

    /// Trigger an event on all kqueues (for global events like process exit).
    pub fn trigger_global(&self, filter: i16, ident: u64, data: i64) {
        let queues = self.queues.lock();
        for kq in queues.values() {
            kq.trigger(ident, filter, data);
        }
    }
}

impl Scheme for KQueueScheme {
    fn open(&self, _path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        let id = self.next_id.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        let queue = Arc::new(KQueue::new());
        self.queues.lock().insert(id, queue);
        Ok(id)
    }

    fn read(&self, _id: usize, _buffer: &mut [u8], _offset: u64) -> Result<usize, usize> {
        Err(error::EINVAL)
    }

    fn write(&self, _id: usize, _buffer: &[u8], _offset: u64) -> Result<usize, usize> {
        Err(error::EINVAL)
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize, _current_offset: u64) -> Result<usize, usize> {
        Err(error::ESPIPE)
    }

    fn close(&self, id: usize) -> Result<usize, usize> {
        if self.queues.lock().remove(&id).is_some() {
            Ok(0)
        } else {
            Err(error::EBADF)
        }
    }

    fn fstat(&self, _id: usize, stat: &mut Stat) -> Result<usize, usize> {
        stat.mode = 0x8000; // Regular file
        Ok(0)
    }

    fn poll(&self, id: usize, events: usize) -> Result<usize, usize> {
        let queue = self.get_queue(id).ok_or(error::EBADF)?;
        let mut revents = 0;
        if events & crate::scheme::event::POLLIN != 0 {
            if !queue.pending.lock().is_empty() {
                revents |= crate::scheme::event::POLLIN;
            }
        }
        Ok(revents)
    }
}

static KQUEUE_SCHEME: spin::Once<Arc<KQueueScheme>> = spin::Once::new();

pub fn get_kqueue_scheme() -> &'static Arc<KQueueScheme> {
    KQUEUE_SCHEME.call_once(|| Arc::new(KQueueScheme::new()))
}
