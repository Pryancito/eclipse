//! eventfd support for Eclipse OS
//!
//! Provides Linux-compatible signaling mechanism.

use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use spin::Mutex;
use crate::scheme::{Scheme, error, Stat};

pub struct EventFd {
    pub counter: u64,
    pub flags: u32,
}

pub struct EventFdScheme {
    instances: Mutex<BTreeMap<usize, EventFd>>,
    next_id: Mutex<usize>,
}

impl EventFdScheme {
    pub fn new() -> Self {
        Self {
            instances: Mutex::new(BTreeMap::new()),
            next_id: Mutex::new(1),
        }
    }
}

impl Scheme for EventFdScheme {
    fn open(&self, path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        // Path format: <initval>/<flags> (scheme name is stripped by caller)
        let parts: Vec<&str> = path.split('/').collect();
        let initval: u64 = parts.get(0).and_then(|&s| s.parse().ok()).unwrap_or(0);
        let flags: u32 = parts.get(1).and_then(|&s| s.parse().ok()).unwrap_or(0);

        let mut id_gen = self.next_id.lock();
        let id = *id_gen;
        *id_gen += 1;
        
        self.instances.lock().insert(id, EventFd { counter: initval, flags });
        Ok(id)
    }

    fn read(&self, id: usize, buffer: &mut [u8]) -> Result<usize, usize> {
        if buffer.len() < 8 { return Err(error::EINVAL); }
        
        let mut instances = self.instances.lock();
        let evfd = instances.get_mut(&id).ok_or(error::EBADF)?;
        
        if evfd.counter == 0 {
             // For now, return EAGAIN if no events are pending.
             // Labwc uses non-blocking read in most cases.
             return Err(error::EAGAIN);
        }
        
        let val = if (evfd.flags & 0x1) != 0 { // EFD_SEMAPHORE
            evfd.counter -= 1;
            1
        } else {
            let v = evfd.counter;
            evfd.counter = 0;
            v
        };
        
        buffer[..8].copy_from_slice(&val.to_ne_bytes());
        Ok(8)
    }

    fn write(&self, id: usize, buffer: &[u8]) -> Result<usize, usize> {
        if buffer.len() < 8 { return Err(error::EINVAL); }
        let mut val_bytes = [0u8; 8];
        val_bytes.copy_from_slice(&buffer[..8]);
        let val = u64::from_ne_bytes(val_bytes);
        
        if val == u64::MAX { return Err(error::EINVAL); }
        
        let mut instances = self.instances.lock();
        let evfd = instances.get_mut(&id).ok_or(error::EBADF)?;
        
        if u64::MAX - evfd.counter < val {
             return Err(error::EAGAIN);
        }
        
        evfd.counter += val;
        Ok(8)
    }

    fn close(&self, id: usize) -> Result<usize, usize> {
        if self.instances.lock().remove(&id).is_some() {
            Ok(0)
        } else {
            Err(error::EBADF)
        }
    }

    fn fstat(&self, _id: usize, stat: &mut Stat) -> Result<usize, usize> {
        stat.mode = 0o666;
        Ok(0)
    }
    
    fn lseek(&self, _id: usize, _offset: isize, _whence: usize) -> Result<usize, usize> {
        Err(error::ESPIPE)
    }

    fn poll(&self, id: usize, events: usize) -> Result<usize, usize> {
        let instances = self.instances.lock();
        let evfd = instances.get(&id).ok_or(error::EBADF)?;
        
        let mut ready = 0;
        if (events & crate::scheme::event::POLLIN) != 0 && evfd.counter > 0 {
            ready |= crate::scheme::event::POLLIN;
        }
        // eventfd is always ready for write (it only blocks if it reaches u64::MAX - 1)
        if (events & crate::scheme::event::POLLOUT) != 0 {
            ready |= crate::scheme::event::POLLOUT;
        }
        
        Ok(ready)
    }
}

pub static EVENTFD_SCHEME: spin::Once<Arc<EventFdScheme>> = spin::Once::new();

pub fn get_eventfd_scheme() -> &'static Arc<EventFdScheme> {
    EVENTFD_SCHEME.call_once(|| Arc::new(EventFdScheme::new()))
}
