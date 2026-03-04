//! Synchronization primitives for the Eclipse kernel.

use core::sync::atomic::{AtomicU64, Ordering};

/// A spinlock-based reentrant mutex that allows the same CPU to re-acquire it.
/// Uses the per-CPU `cpu_id` stored in GS:[16] to identify the owning CPU.
/// 
/// The state is stored in a single AtomicU64 to ensure atomic updates of both
/// owner and depth.
/// Layout: [ 32 bits: owner (i32) | 32 bits: depth (u32) ]
pub struct ReentrantMutex<T> {
    state: AtomicU64,
    data: core::cell::UnsafeCell<T>,
}

const NO_CPU: i32 = -1;

unsafe impl<T: Send> Send for ReentrantMutex<T> {}
unsafe impl<T: Send> Sync for ReentrantMutex<T> {}

pub struct ReentrantMutexGuard<'a, T> {
    lock: &'a ReentrantMutex<T>,
}

impl<T> core::ops::Deref for ReentrantMutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> core::ops::DerefMut for ReentrantMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> ReentrantMutex<T> {
    pub const fn new(val: T) -> Self {
        Self {
            // Initial state: NO_CPU (-1) and depth 0
            state: AtomicU64::new(((NO_CPU as u64) << 32) | 0),
            data: core::cell::UnsafeCell::new(val),
        }
    }

    #[inline]
    fn pack(owner: i32, depth: u32) -> u64 {
        ((owner as u32 as u64) << 32) | (depth as u64)
    }

    #[inline]
    fn unpack(state: u64) -> (i32, u32) {
        ((state >> 32) as i32, (state & 0xFFFFFFFF) as u32)
    }

    /// Get the current CPU's id for ownership tracking.
    pub fn current_cpu() -> i32 {
        let mut id: u32;
        unsafe {
            core::arch::asm!(
                "mov {0:e}, gs:[16]",
                out(reg) id,
                options(nomem, nostack, preserves_flags)
            );
        }
        
        // Sanity check: if gs is not set up (long mode transition/early AP),
        // it will read 0xFFFF_FFFF (uninitialized value in CPU_DATA).
        // Fallback to the slow but reliable LAPIC ID to avoid lock owner collisions.
        if id == 0xFFFF_FFFF {
            return crate::apic::get_id() as i32;
        }
        
        id as i32
    }

    pub fn lock(&self) -> ReentrantMutexGuard<'_, T> {
        let me = Self::current_cpu();
        loop {
            let current = self.state.load(Ordering::Acquire);
            let (owner, depth) = Self::unpack(current);

            if owner == me {
                // Same CPU re-entering: increment depth atomically.
                let next = Self::pack(me, depth + 1);
                if self.state.compare_exchange_weak(current, next, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                    return ReentrantMutexGuard { lock: self };
                }
            } else if owner == NO_CPU {
                // Try to acquire ownership.
                let next = Self::pack(me, 1);
                if self.state.compare_exchange_weak(current, next, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                    return ReentrantMutexGuard { lock: self };
                }
            }
            
            // Spin until available or re-entrant.
            core::hint::spin_loop();
        }
    }

    pub fn try_lock(&self) -> Option<ReentrantMutexGuard<'_, T>> {
        let me = Self::current_cpu();
        let current = self.state.load(Ordering::Acquire);
        let (owner, depth) = Self::unpack(current);

        if owner == me {
            let next = Self::pack(me, depth + 1);
            if self.state.compare_exchange_weak(current, next, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                return Some(ReentrantMutexGuard { lock: self });
            }
        } else if owner == NO_CPU {
            let next = Self::pack(me, 1);
            if self.state.compare_exchange_weak(current, next, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                return Some(ReentrantMutexGuard { lock: self });
            }
        }
        None
    }

    /// Forcedly unlock the mutex.
    /// Danger: should ONLY be used in restricted setup paths to clear inherited locks.
    pub unsafe fn force_unlock(&self) {
        self.state.store(Self::pack(NO_CPU, 0), Ordering::Release);
    }
}

impl<T> Drop for ReentrantMutexGuard<'_, T> {
    fn drop(&mut self) {
        loop {
            let current = self.lock.state.load(Ordering::Acquire);
            let (owner, depth) = ReentrantMutex::<T>::unpack(current);
            
            let next = if depth <= 1 {
                ReentrantMutex::<T>::pack(NO_CPU, 0)
            } else {
                ReentrantMutex::<T>::pack(owner, depth - 1)
            };

            if self.lock.state.compare_exchange_weak(current, next, Ordering::Release, Ordering::Relaxed).is_ok() {
                break;
            }
            core::hint::spin_loop();
        }
    }
}
