//! pthread.h - POSIX threads
use crate::types::*;
use alloc::boxed::Box;
use core::sync::atomic::{AtomicI32, Ordering};

/// Spinlock states for pthread_mutex_t and FILE locks.
const MUTEX_UNLOCKED: i32 = 0;
const MUTEX_LOCKED: i32 = 1;

/// Tamaño del stack de hilos creados con `pthread_create` (userspace).
const PTHREAD_STACK_SIZE: usize = 256 * 1024;
// pthread_t, pthread_mutex_t, pthread_cond_t now defined in crate::types

#[repr(C)]
struct ThreadBootstrap {
    entry: extern "C" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,
    join_cell: *mut *mut c_void,
}

unsafe extern "C" fn eclipse_thread_bootstrap(boot_raw: *mut c_void) {
    let boot = boot_raw as *mut ThreadBootstrap;
    let b = &*boot;
    let ret = (b.entry)(b.arg);
    if !b.join_cell.is_null() {
        *b.join_cell = ret;
    }
    let _ = Box::from_raw(boot);
    eclipse_syscall::call::exit(0);
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn pthread_create(
    thread: *mut pthread_t,
    _attr: *const c_void,
    start_routine: extern "C" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,
) -> c_int {
    use crate::header::sys_mman::{mmap, munmap, MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE};

    if thread.is_null() {
        return crate::header::errno::EINVAL;
    }

    let join_cell = Box::into_raw(Box::new(core::ptr::null_mut::<c_void>()));
    let boot = Box::new(ThreadBootstrap {
        entry: start_routine,
        arg,
        join_cell,
    });
    let boot_ptr = Box::into_raw(boot);
    let stack = mmap(
        core::ptr::null_mut(),
        PTHREAD_STACK_SIZE,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    );
    let stack_usize = stack as usize;
    if stack.is_null() || stack_usize == usize::MAX {
        let _ = Box::from_raw(boot_ptr);
        let _ = Box::from_raw(join_cell);
        *crate::header::errno::__errno_location() = crate::header::errno::ENOMEM;
        return -1;
    }
    let stack_top = (stack_usize + PTHREAD_STACK_SIZE) & !0xF;
    match eclipse_syscall::call::thread_create(
        stack_top,
        eclipse_thread_bootstrap as usize,
        boot_ptr as usize,
    ) {
        Ok(tid) => {
            (*thread).thread_id = tid as u64;
            (*thread).join_cell = join_cell;
            0
        }
        Err(e) => {
            let _ = Box::from_raw(boot_ptr);
            let _ = Box::from_raw(join_cell);
            let _ = munmap(stack, PTHREAD_STACK_SIZE);
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[cfg(all(any(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target))), unix), not(target_os = "eclipse"), not(any(target_os = "eclipse", eclipse_target))))]
extern "C" {
    pub fn pthread_create(
        thread: *mut pthread_t,
        attr: *const c_void,
        start_routine: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void
    ) -> c_int;
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn pthread_join(thread: pthread_t, retval: *mut *mut c_void) -> c_int {
    let mut status: u32 = 0;
    if let Err(e) = eclipse_syscall::call::wait_pid(
        &mut status as *mut u32,
        thread.thread_id as usize,
    ) {
        *crate::header::errno::__errno_location() = e.errno as c_int;
        return -1;
    }
    if !retval.is_null() && !thread.join_cell.is_null() {
        *retval = *thread.join_cell;
    }
    if !thread.join_cell.is_null() {
        let _ = Box::from_raw(thread.join_cell);
    }
    0
}

#[cfg(all(any(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target))), unix), not(target_os = "eclipse"), not(any(target_os = "eclipse", eclipse_target))))]
extern "C" {
    pub fn pthread_join(thread: pthread_t, retval: *mut *mut c_void) -> c_int;
}

#[allow(non_camel_case_types)]
pub type pthread_attr_t = c_void;
#[allow(non_camel_case_types)]
pub type pthread_mutexattr_t = c_void;
#[allow(non_camel_case_types)]
pub type pthread_condattr_t = c_void;

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_init(mutex: *mut pthread_mutex_t, _attr: *const c_void) -> c_int {
    if mutex.is_null() { return crate::header::errno::EINVAL; }
    (*mutex).lock.store(0, Ordering::Relaxed);
    0
}
#[cfg(all(any(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target))), unix), not(target_os = "eclipse"), not(any(target_os = "eclipse", eclipse_target))))]
extern "C" { pub fn pthread_mutex_init(mutex: *mut pthread_mutex_t, attr: *const c_void) -> c_int; }

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_destroy(_mutex: *mut pthread_mutex_t) -> c_int { 0 }
#[cfg(all(any(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target))), unix), not(target_os = "eclipse"), not(any(target_os = "eclipse", eclipse_target))))]
extern "C" { pub fn pthread_mutex_destroy(mutex: *mut pthread_mutex_t) -> c_int; }

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
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
#[cfg(all(any(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target))), unix), not(target_os = "eclipse"), not(any(target_os = "eclipse", eclipse_target))))]
extern "C" { pub fn pthread_mutex_lock(mutex: *mut pthread_mutex_t) -> c_int; }

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
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
#[cfg(all(any(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target))), unix), not(target_os = "eclipse"), not(any(target_os = "eclipse", eclipse_target))))]
extern "C" { pub fn pthread_mutex_trylock(mutex: *mut pthread_mutex_t) -> c_int; }

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_unlock(mutex: *mut pthread_mutex_t) -> c_int {
    if mutex.is_null() { return crate::header::errno::EINVAL; }
    (*mutex).lock.store(MUTEX_UNLOCKED, Ordering::Release);
    0
}
#[cfg(all(any(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target))), unix), not(target_os = "eclipse"), not(any(target_os = "eclipse", eclipse_target))))]
extern "C" { pub fn pthread_mutex_unlock(mutex: *mut pthread_mutex_t) -> c_int; }

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn pthread_cond_init(cond: *mut pthread_cond_t, _attr: *const c_void) -> c_int {
    if cond.is_null() { return crate::header::errno::EINVAL; }
    (*cond).value.store(0, Ordering::Relaxed);
    0
}
#[cfg(all(any(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target))), unix), not(target_os = "eclipse"), not(any(target_os = "eclipse", eclipse_target))))]
extern "C" { pub fn pthread_cond_init(cond: *mut pthread_cond_t, attr: *const c_void) -> c_int; }

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn pthread_cond_destroy(_cond: *mut pthread_cond_t) -> c_int { 0 }
#[cfg(all(any(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target))), unix), not(target_os = "eclipse"), not(any(target_os = "eclipse", eclipse_target))))]
extern "C" { pub fn pthread_cond_destroy(cond: *mut pthread_cond_t) -> c_int; }

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
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
#[cfg(all(any(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target))), unix), not(target_os = "eclipse"), not(any(target_os = "eclipse", eclipse_target))))]
extern "C" { pub fn pthread_cond_wait(cond: *mut pthread_cond_t, mutex: *mut pthread_mutex_t) -> c_int; }

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn pthread_cond_signal(cond: *mut pthread_cond_t) -> c_int {
    if cond.is_null() { return crate::header::errno::EINVAL; }
    (*cond).value.fetch_add(1, Ordering::Release);
    let _ = eclipse_syscall::call::futex_wake(&(*cond).value, 1);
    0
}
#[cfg(all(any(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target))), unix), not(target_os = "eclipse"), not(any(target_os = "eclipse", eclipse_target))))]
extern "C" { pub fn pthread_cond_signal(cond: *mut pthread_cond_t) -> c_int; }

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn pthread_cond_broadcast(cond: *mut pthread_cond_t) -> c_int {
    if cond.is_null() { return crate::header::errno::EINVAL; }
    (*cond).value.fetch_add(1, Ordering::Release);
    let _ = eclipse_syscall::call::futex_wake(&(*cond).value, u32::MAX);
    0
}
#[cfg(all(any(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target))), unix), not(target_os = "eclipse"), not(any(target_os = "eclipse", eclipse_target))))]
extern "C" { pub fn pthread_cond_broadcast(cond: *mut pthread_cond_t) -> c_int; }

// yield_cpu is now in sys_eclipse.rs
