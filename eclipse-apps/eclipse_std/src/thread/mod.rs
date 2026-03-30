//! Thread Module - Threading support using eclipse-libc pthread

use core::ptr;
use crate::libc::*;
use core::prelude::v1::*;
use ::alloc::boxed::Box;

/// Thread handle
pub struct Thread {
    handle: crate::libc::pthread_t,
}

/// Join handle for a spawned thread
pub struct JoinHandle<T> {
    thread: Thread,
    result: *mut Option<T>,
}

impl Thread {
    /// Get the current thread (TID del scheduler).
    pub fn current() -> Thread {
        let tid = eclipse_syscall::call::gettid() as u64;
        Thread {
            handle: crate::libc::pthread_t {
                thread_id: tid,
                join_cell: ptr::null_mut(),
            },
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
        let result = Box::into_raw(Box::new(None::<T>));
        let pair = Box::into_raw(Box::new((f, result)));

        extern "C" fn thread_wrapper<F, T>(arg: *mut c_void) -> *mut c_void
        where
            F: FnOnce() -> T + Send + 'static,
        {
            unsafe {
                let pair = *Box::from_raw(arg as *mut (F, *mut Option<T>));
                let (f, out) = pair;
                *out = Some(f());
                ptr::null_mut()
            }
        }

        let mut handle: crate::libc::pthread_t = core::mem::zeroed();
        let r = crate::libc::pthread_create(
            &mut handle as *mut crate::libc::pthread_t,
            ptr::null(),
            thread_wrapper::<F, T>,
            pair as *mut c_void,
        );

        if r != 0 {
            let _ = Box::from_raw(pair);
            let _ = Box::from_raw(result);
            crate::eprintln!("Failed to create thread");
            crate::libc::exit(1);
        }

        JoinHandle {
            thread: Thread { handle },
            result,
        }
    }
}

impl<T> JoinHandle<T> {
    /// Wait for the thread to finish
    pub fn join(self) -> Result<T, ()> {
        unsafe {
            if crate::libc::pthread_join(self.thread.handle, ptr::null_mut()) != 0 {
                let _ = Box::from_raw(self.result);
                return Err(());
            }
            let out = Box::from_raw(self.result);
            out.ok_or(())
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
        crate::libc::yield_cpu();
    }
}
