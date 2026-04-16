//! Epoll support for Eclipse OS
//!
//! Provides Linux-compatible event notification system using the Scheme interface.

use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use spin::Mutex;
use crate::scheme::{Scheme, error, Stat};

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EpollEvent {
    pub events: u32,
    pub data: u64,
}

pub struct EpollInstance {
    // FD being watched -> event configuration
    pub watched: BTreeMap<usize, EpollEvent>,
}

pub struct EpollScheme {
    instances: Mutex<BTreeMap<usize, EpollInstance>>,
    next_id: Mutex<usize>,
}

impl EpollScheme {
    pub fn new() -> Self {
        Self {
            instances: Mutex::new(BTreeMap::new()),
            next_id: Mutex::new(1),
        }
    }
    
    pub fn get_instance_watched_fds(&self, resource_id: usize) -> Option<Vec<(usize, EpollEvent)>> {
        let instances = self.instances.lock();
        instances.get(&resource_id).map(|inst| {
            inst.watched.iter().map(|(&fd, &ev)| (fd, ev)).collect()
        })
    }
    
    pub fn ctl(&self, resource_id: usize, op: usize, fd: usize, event: Option<EpollEvent>) -> Result<usize, usize> {
        let mut instances = self.instances.lock();
        let instance = instances.get_mut(&resource_id).ok_or(error::EBADF)?;
        
        const EPOLL_CTL_ADD: usize = 1;
        const EPOLL_CTL_DEL: usize = 2;
        const EPOLL_CTL_MOD: usize = 3;
        
        match op {
            EPOLL_CTL_ADD => {
                if instance.watched.contains_key(&fd) {
                    return Err(error::EEXIST);
                }
                instance.watched.insert(fd, event.ok_or(error::EINVAL)?);
                Ok(0)
            }
            EPOLL_CTL_DEL => {
                if instance.watched.remove(&fd).is_some() {
                    Ok(0)
                } else {
                    Err(error::ENOENT)
                }
            }
            EPOLL_CTL_MOD => {
                if let Some(e) = instance.watched.get_mut(&fd) {
                    *e = event.ok_or(error::EINVAL)?;
                    Ok(0)
                } else {
                    Err(error::ENOENT)
                }
            }
            _ => Err(error::EINVAL),
        }
    }
}

impl Scheme for EpollScheme {
    fn open(&self, _path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        let mut id_gen = self.next_id.lock();
        let id = *id_gen;
        *id_gen += 1;
        
        let mut instances = self.instances.lock();
        instances.insert(id, EpollInstance { watched: BTreeMap::new() });
        Ok(id)
    }

    fn close(&self, id: usize) -> Result<usize, usize> {
        let mut instances = self.instances.lock();
        if instances.remove(&id).is_some() {
            Ok(0)
        } else {
            Err(error::EBADF)
        }
    }

    fn read(&self, _id: usize, _buffer: &mut [u8]) -> Result<usize, usize> { Err(error::EINVAL) }
    fn write(&self, _id: usize, _buffer: &[u8]) -> Result<usize, usize> { Err(error::EINVAL) }
    fn lseek(&self, _id: usize, _offset: isize, _whence: usize) -> Result<usize, usize> { Err(error::ESPIPE) }
    
    fn fstat(&self, _id: usize, stat: &mut Stat) -> Result<usize, usize> {
        stat.mode = 0o666; // Placeholder
        Ok(0)
    }
}

/// Global instance of EpollScheme
pub static EPOLL_SCHEME: spin::Once<alloc::sync::Arc<EpollScheme>> = spin::Once::new();

pub fn get_epoll_scheme() -> &'static alloc::sync::Arc<EpollScheme> {
    EPOLL_SCHEME.call_once(|| alloc::sync::Arc::new(EpollScheme::new()))
}
