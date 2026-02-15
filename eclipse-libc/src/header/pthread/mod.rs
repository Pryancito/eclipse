//! pthread.h - POSIX threads
use crate::types::*;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct pthread_t {
    pub thread_id: u64,
}

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
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

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct pthread_mutex_t {
    pub lock: c_int,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct pthread_cond_t {
    pub value: c_int,
}

pub const PTHREAD_MUTEX_INITIALIZER: pthread_mutex_t = pthread_mutex_t { lock: 0 };
pub const PTHREAD_COND_INITIALIZER: pthread_cond_t = pthread_cond_t { value: 0 };

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_lock(_mutex: *mut pthread_mutex_t) -> c_int { 0 }
#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" { pub fn pthread_mutex_lock(mutex: *mut pthread_mutex_t) -> c_int; }

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_trylock(_mutex: *mut pthread_mutex_t) -> c_int { 0 }
#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" { pub fn pthread_mutex_trylock(mutex: *mut pthread_mutex_t) -> c_int; }

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_unlock(_mutex: *mut pthread_mutex_t) -> c_int { 0 }
#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" { pub fn pthread_mutex_unlock(mutex: *mut pthread_mutex_t) -> c_int; }

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn pthread_cond_wait(_cond: *mut pthread_cond_t, _mutex: *mut pthread_mutex_t) -> c_int { 0 }
#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" { pub fn pthread_cond_wait(cond: *mut pthread_cond_t, mutex: *mut pthread_mutex_t) -> c_int; }

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn pthread_cond_signal(_cond: *mut pthread_cond_t) -> c_int { 0 }
#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" { pub fn pthread_cond_signal(cond: *mut pthread_cond_t) -> c_int; }

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn pthread_cond_broadcast(_cond: *mut pthread_cond_t) -> c_int { 0 }
#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" { pub fn pthread_cond_broadcast(cond: *mut pthread_cond_t) -> c_int; }

// yield_cpu is custom, but sched_yield is POSIX
#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn yield_cpu() {
    eclipse_syscall::call::sched_yield().ok();
}

#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
#[no_mangle]
pub unsafe extern "C" fn yield_cpu() {
    // On Linux we can call sched_yield from libc
    extern "C" { fn sched_yield() -> c_int; }
    sched_yield();
}
