//! Synchronization Module - Mutex and Condvar using eclipse-libc pthread

use core::ptr;
use core::cell::UnsafeCell;
use eclipse_libc::*;

/// Mutual exclusion primitive
pub struct Mutex<T: ?Sized> {
    inner: UnsafeCell<pthread_mutex_t>,
    data: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    /// Create a new mutex
    pub const fn new(value: T) -> Self {
        Mutex {
            inner: UnsafeCell::new(PTHREAD_MUTEX_INITIALIZER),
            data: UnsafeCell::new(value),
        }
    }
    
    /// Lock the mutex
    pub fn lock(&self) -> MutexGuard<T> {
        unsafe {
            pthread_mutex_lock(self.inner.get());
        }
        
        MutexGuard {
            mutex: self,
        }
    }
    
    /// Try to lock the mutex
    pub fn try_lock(&self) -> Option<MutexGuard<T>> {
        unsafe {
            let result = pthread_mutex_trylock(self.inner.get());
            if result == 0 {
                Some(MutexGuard { mutex: self })
            } else {
                None
            }
        }
    }
}

/// Mutex guard that automatically unlocks on drop
pub struct MutexGuard<'a, T: ?Sized + 'a> {
    mutex: &'a Mutex<T>,
}

impl<'a, T: ?Sized> core::ops::Deref for MutexGuard<'a, T> {
    type Target = T;
    
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<'a, T: ?Sized> core::ops::DerefMut for MutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<'a, T: ?Sized> Drop for MutexGuard<'a, T> {
    fn drop(&mut self) {
        unsafe {
            pthread_mutex_unlock(self.mutex.inner.get());
        }
    }
}

/// Condition variable
pub struct Condvar {
    inner: UnsafeCell<pthread_cond_t>,
}

unsafe impl Send for Condvar {}
unsafe impl Sync for Condvar {}

impl Condvar {
    /// Create a new condition variable
    pub const fn new() -> Self {
        Condvar {
            inner: UnsafeCell::new(PTHREAD_COND_INITIALIZER),
        }
    }
    
    /// Wait on the condition variable
    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        unsafe {
            pthread_cond_wait(self.inner.get(), guard.mutex.inner.get());
        }
        guard
    }
    
    /// Signal one waiting thread
    pub fn notify_one(&self) {
        unsafe {
            pthread_cond_signal(self.inner.get());
        }
    }
    
    /// Signal all waiting threads
    pub fn notify_all(&self) {
        unsafe {
            pthread_cond_broadcast(self.inner.get());
        }
    }
}

pub use self::mutex::Mutex;
pub use self::condvar::Condvar;

mod mutex {
    pub use super::Mutex;
}

mod condvar {
    pub use super::Condvar;
}
