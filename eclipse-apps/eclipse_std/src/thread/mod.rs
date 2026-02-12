//! Thread Module - Threading support using eclipse-libc pthread

use core::ptr;
use eclipse_libc::*;
use eclipse_libc::header::time::{timespec, nanosleep};
use ::alloc::boxed::Box;

/// Thread handle
pub struct Thread {
    handle: pthread_t,
}

/// Join handle for a spawned thread
pub struct JoinHandle<T> {
    thread: Thread,
    _phantom: core::marker::PhantomData<T>,
}

impl Thread {
    /// Get the current thread
    pub fn current() -> Thread {
        unsafe {
            Thread {
                handle: core::mem::zeroed(), // TODO: proper implementation
            }
        }
    }
    
    /// Get thread ID
    pub fn id(&self) -> ThreadId {
        ThreadId(self.handle.thread_id)
    }
}

/// Thread ID
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadId(u64);

/// Spawn a new thread
pub fn spawn<F, T>(f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    unsafe {
        // Box the closure
        let boxed = ::alloc::boxed::Box::new(f);
        let raw = ::alloc::boxed::Box::into_raw(boxed);
        
        // Thread wrapper function
        extern "C" fn thread_wrapper<F, T>(arg: *mut c_void) -> *mut c_void
        where
            F: FnOnce() -> T + Send + 'static,
        {
            unsafe {
                let boxed = ::alloc::boxed::Box::from_raw(arg as *mut F);
                let _ = boxed();
                ptr::null_mut()
            }
        }
        
        // Create pthread
        let mut handle: pthread_t = core::mem::zeroed();
        let result = pthread_create(
            &mut handle as *mut pthread_t,
            ptr::null(),
            thread_wrapper::<F, T>,
            raw as *mut c_void
        );
        
        if result != 0 {
            panic!("Failed to create thread");
        }
        
        JoinHandle {
            thread: Thread { handle },
            _phantom: core::marker::PhantomData,
        }
    }
}

impl<T> JoinHandle<T> {
    /// Wait for the thread to finish
    pub fn join(self) -> Result<T, ()> {
        unsafe {
            let result = pthread_join(self.thread.handle, ptr::null_mut());
            if result == 0 {
                // TODO: return actual value
                Err(())
            } else {
                Err(())
            }
        }
    }
}

use crate::time::Duration;

/// Sleep for a duration
pub fn sleep(dur: Duration) {
    unsafe {
        let ts = timespec {
            tv_sec: dur.secs as i64,
            tv_nsec: dur.nanos as i64,
        };
        nanosleep(&ts as *const timespec, ptr::null_mut());
    }
}

/// Yield the current thread
pub fn yield_now() {
    unsafe {
        yield_cpu();
    }
}
