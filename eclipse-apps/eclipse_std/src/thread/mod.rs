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

    /// Signal the thread to wake up from a park() call.
    pub fn unpark(&self) {
        // En una implementación real, esto interactuaría con un semáforo o condvar por hilo.
        // Por ahora, como no tenemos un thread-local storage completo que std reconozca,
        // esto es un NO-OP que permite compilar crates que lo usan (como once_cell).
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

/// Duerme el hilo actual durante `dur`.
///
/// No llama a `libc::nanosleep` directamente: con LTO/`opt-level=z` LLVM a veces integra
/// la preparación del `timespec` y el `syscall` de forma que la pila queda en un layout
/// que en Eclipse OS acaba en **#GP en ring 3** (p. ej. al escribir el segundo campo).
/// Usar [`eclipse_syscall::call::syscall2`] mantiene el tramo crítico estable y alineado
/// con el manejador del kernel (`SYS_NANOSLEEP` = 35).
#[inline(never)]
pub fn sleep(dur: Duration) {
    unsafe {
        let mut ts = timespec {
            tv_sec: dur.secs as time_t,
            tv_nsec: dur.nanos as c_long,
        };
        let extra_secs = ts.tv_nsec / 1_000_000_000;
        if extra_secs != 0 {
            ts.tv_nsec %= 1_000_000_000;
            ts.tv_sec = ts.tv_sec.saturating_add(extra_secs);
        }
        let ts_ptr = &ts as *const timespec as usize;
        let _ = eclipse_syscall::syscall2(
            eclipse_syscall::number::SYS_NANOSLEEP,
            ts_ptr,
            0,
        );
    }
}

/// Yield the current thread
pub fn yield_now() {
    unsafe {
        crate::libc::yield_cpu();
    }
}

/// Block the current thread until another thread calls unpark() on it.
pub fn park() {
    // Stub implementation to satisfy once_cell and other crates.
    // In a real std, this would wait on a thread-local condition variable.
    yield_now();
}

/// Block the current thread until another thread calls unpark() or a timeout occurs.
pub fn park_timeout(_dur: Duration) {
    yield_now();
}
