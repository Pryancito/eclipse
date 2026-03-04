//! Funciones de utilidad estándar.
//! Símbolos C (no_mangle + extern "C") para que el linker resuelva las referencias
//! que emite el compilador al optimizar copias/zeroing de memoria.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};
use core::ops::{Deref, DerefMut};

/// A simple spinlock that can hold any value `T`.
///
/// Suitable for both single-core and SMP use: the owner spins (with a
/// `core::hint::spin_loop()` pause hint) until the lock is free.
///
/// `Spinlock<T>` is `Send + Sync` whenever `T: Send`, so it can safely live
/// in `static` storage and be accessed from any CPU.
pub struct Spinlock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for Spinlock<T> {}
unsafe impl<T: Send> Sync for Spinlock<T> {}

impl<T> Spinlock<T> {
    /// Create a new, unlocked `Spinlock` containing `value`.
    pub const fn new(value: T) -> Self {
        Spinlock {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(value),
        }
    }

    /// Acquire the lock, spinning until it is available.
    /// Returns a `SpinlockGuard` that automatically releases the lock on drop.
    pub fn lock(&self) -> SpinlockGuard<'_, T> {
        loop {
            if self.locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
            core::hint::spin_loop();
        }
        SpinlockGuard { lock: self }
    }
}

/// RAII guard returned by [`Spinlock::lock`].
pub struct SpinlockGuard<'a, T> {
    lock: &'a Spinlock<T>,
}

impl<'a, T> Deref for SpinlockGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<'a, T> DerefMut for SpinlockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<'a, T> Drop for SpinlockGuard<'a, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

/// Símbolo C requerido por el linker en target no-GNU (x86_64-unknown-eclipse).
#[cfg(not(target_env = "gnu"))]
#[no_mangle]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, n: usize) {
    for i in 0..n {
        *dest.add(i) = *src.add(i);
    }
}

#[cfg(target_env = "gnu")]
pub unsafe fn memcpy(dest: *mut u8, src: *const u8, n: usize) {
    for i in 0..n {
        *dest.add(i) = *src.add(i);
    }
}

/// Símbolo C requerido por el linker en target no-GNU (x86_64-unknown-eclipse).
#[cfg(not(target_env = "gnu"))]
#[no_mangle]
pub unsafe extern "C" fn memset(dest: *mut u8, val: u8, n: usize) {
    for i in 0..n {
        *dest.add(i) = val;
    }
}

#[cfg(target_env = "gnu")]
pub unsafe fn memset(dest: *mut u8, val: u8, n: usize) {
    for i in 0..n {
        *dest.add(i) = val;
    }
}

/// Símbolo C requerido por el linker (comparación de slices, etc.).
#[cfg(not(target_env = "gnu"))]
#[no_mangle]
pub unsafe extern "C" fn bcmp(s1: *const u8, s2: *const u8, n: usize) -> i32 {
    for i in 0..n {
        if *s1.add(i) != *s2.add(i) {
            return 1;
        }
    }
    0
}

/// Símbolo C requerido por el linker (swap/copias con solapamiento).
#[cfg(not(target_env = "gnu"))]
#[no_mangle]
pub unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, n: usize) {
    if n == 0 {
        return;
    }
    if dest < src as *mut u8 {
        for i in 0..n {
            *dest.add(i) = *src.add(i);
        }
    } else if dest > src as *mut u8 {
        for i in (0..n).rev() {
            *dest.add(i) = *src.add(i);
        }
    }
}

pub unsafe fn memcmp(s1: *const u8, s2: *const u8, n: usize) -> i32 {
    for i in 0..n {
        let a = *s1.add(i);
        let b = *s2.add(i);
        if a != b {
            return (a as i32) - (b as i32);
        }
    }
    0
}

pub unsafe fn strlen(s: *const u8) -> usize {
    let mut len = 0;
    while *s.add(len) != 0 {
        len += 1;
    }
    len
}
