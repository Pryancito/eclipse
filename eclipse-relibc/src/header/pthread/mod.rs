//! pthread.h - POSIX threads
use crate::types::*;
use core::sync::atomic::{AtomicI32, Ordering};

/// Spinlock states for pthread_mutex_t and FILE locks.
const MUTEX_UNLOCKED: i32 = 0;
const MUTEX_LOCKED: i32 = 1;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct pthread_t {
    pub thread_id: u64,
}

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn pthread_create(
    thread: *mut pthread_t,
    _attr: *const c_void,
    _start_routine: extern "C" fn(*mut c_void) -> *mut c_void,
    _arg: *mut c_void
) -> c_int {
    // This is a stub - real implementation would use SYS_CLONE
    if !thread.is_null() {
        (*thread).thread_id = 1; // Dummy ID
    }
    // TODO: implement actual threading using SYS_CLONE
    -1
}

#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" {
    pub fn pthread_create(
        thread: *mut pthread_t,
        attr: *const c_void,
        start_routine: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void
    ) -> c_int;
}

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn pthread_join(_thread: pthread_t, _retval: *mut *mut c_void) -> c_int {
    0
}

#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" {
    pub fn pthread_join(thread: pthread_t, retval: *mut *mut c_void) -> c_int;
}

#[allow(non_camel_case_types)]
pub type pthread_attr_t = c_void;
#[allow(non_camel_case_types)]
pub type pthread_mutexattr_t = c_void;
#[allow(non_camel_case_types)]
pub type pthread_condattr_t = c_void;

/// pthread_mutex_t: uses an AtomicI32 spinlock.
/// 0 = unlocked, 1 = locked.
/// AtomicI32 has the same size and alignment as c_int, so the C ABI is compatible.
#[repr(C)]
pub struct pthread_mutex_t {
    pub lock: AtomicI32,
}

impl Default for pthread_mutex_t {
    fn default() -> Self {
        pthread_mutex_t { lock: AtomicI32::new(0) }
    }
}

/// pthread_cond_t: uses an AtomicI32 counter for futex-based signalling.
/// Each signal/broadcast increments the counter; wait checks if the counter
/// has changed after re-acquiring the mutex.
#[repr(C)]
pub struct pthread_cond_t {
    pub value: AtomicI32,
}

impl Default for pthread_cond_t {
    fn default() -> Self {
        pthread_cond_t { value: AtomicI32::new(0) }
    }
}

pub const PTHREAD_MUTEX_INITIALIZER: pthread_mutex_t = pthread_mutex_t { lock: AtomicI32::new(0) };
pub const PTHREAD_COND_INITIALIZER: pthread_cond_t = pthread_cond_t { value: AtomicI32::new(0) };

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_init(mutex: *mut pthread_mutex_t, _attr: *const c_void) -> c_int {
    if mutex.is_null() { return crate::header::errno::EINVAL; }
    (*mutex).lock.store(0, Ordering::Relaxed);
    0
}
#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" { pub fn pthread_mutex_init(mutex: *mut pthread_mutex_t, attr: *const c_void) -> c_int; }

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_destroy(_mutex: *mut pthread_mutex_t) -> c_int { 0 }
#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" { pub fn pthread_mutex_destroy(mutex: *mut pthread_mutex_t) -> c_int; }

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_lock(mutex: *mut pthread_mutex_t) -> c_int {
    if mutex.is_null() { return crate::header::errno::EINVAL; }
    loop {
        if (*mutex).lock
            .compare_exchange_weak(MUTEX_UNLOCKED, MUTEX_LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return 0;
        }
        core::hint::spin_loop();
    }
}
#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" { pub fn pthread_mutex_lock(mutex: *mut pthread_mutex_t) -> c_int; }

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_trylock(mutex: *mut pthread_mutex_t) -> c_int {
    if mutex.is_null() { return crate::header::errno::EINVAL; }
    match (*mutex).lock
        .compare_exchange(MUTEX_UNLOCKED, MUTEX_LOCKED, Ordering::Acquire, Ordering::Relaxed)
    {
        Ok(_) => 0,
        Err(_) => crate::header::errno::EBUSY,
    }
}
#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" { pub fn pthread_mutex_trylock(mutex: *mut pthread_mutex_t) -> c_int; }

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_unlock(mutex: *mut pthread_mutex_t) -> c_int {
    if mutex.is_null() { return crate::header::errno::EINVAL; }
    (*mutex).lock.store(MUTEX_UNLOCKED, Ordering::Release);
    0
}
#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" { pub fn pthread_mutex_unlock(mutex: *mut pthread_mutex_t) -> c_int; }

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn pthread_cond_init(cond: *mut pthread_cond_t, _attr: *const c_void) -> c_int {
    if cond.is_null() { return crate::header::errno::EINVAL; }
    (*cond).value.store(0, Ordering::Relaxed);
    0
}
#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" { pub fn pthread_cond_init(cond: *mut pthread_cond_t, attr: *const c_void) -> c_int; }

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn pthread_cond_destroy(_cond: *mut pthread_cond_t) -> c_int { 0 }
#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" { pub fn pthread_cond_destroy(cond: *mut pthread_cond_t) -> c_int; }

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn pthread_cond_wait(cond: *mut pthread_cond_t, mutex: *mut pthread_mutex_t) -> c_int {
    if cond.is_null() || mutex.is_null() { return crate::header::errno::EINVAL; }
    // Snapshot the condition counter before releasing the mutex
    let seq = (*cond).value.load(Ordering::Relaxed);
    // Release the mutex while we wait
    pthread_mutex_unlock(mutex);
    // Sleep in the kernel while the condition counter hasn't changed
    let _ = eclipse_syscall::call::futex_wait(&(*cond).value, seq);
    // Re-acquire the mutex before returning
    pthread_mutex_lock(mutex);
    0
}
#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" { pub fn pthread_cond_wait(cond: *mut pthread_cond_t, mutex: *mut pthread_mutex_t) -> c_int; }

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn pthread_cond_signal(cond: *mut pthread_cond_t) -> c_int {
    if cond.is_null() { return crate::header::errno::EINVAL; }
    (*cond).value.fetch_add(1, Ordering::Release);
    let _ = eclipse_syscall::call::futex_wake(&(*cond).value, 1);
    0
}
#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" { pub fn pthread_cond_signal(cond: *mut pthread_cond_t) -> c_int; }

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn pthread_cond_broadcast(cond: *mut pthread_cond_t) -> c_int {
    if cond.is_null() { return crate::header::errno::EINVAL; }
    (*cond).value.fetch_add(1, Ordering::Release);
    let _ = eclipse_syscall::call::futex_wake(&(*cond).value, u32::MAX);
    0
}
#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" { pub fn pthread_cond_broadcast(cond: *mut pthread_cond_t) -> c_int; }

// yield_cpu is now in sys_eclipse.rs
