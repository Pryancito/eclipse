//! Synchronization primitives for the Eclipse kernel.

use core::sync::atomic::{AtomicI32, AtomicU32, Ordering};

/// A spinlock-based reentrant mutex that allows the same CPU to re-acquire it.
/// Uses the per-CPU `cpu_id` stored in GS:[16] to identify the owning CPU.
pub struct ReentrantMutex<T> {
    /// -1 = unlocked, otherwise the cpu_id (cast to i32) of the holding CPU.
    owner: AtomicI32,
    /// Recursion depth for the owning CPU.
    depth: AtomicU32,
    data: core::cell::UnsafeCell<T>,
}

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
            owner: AtomicI32::new(-2),
            depth: AtomicU32::new(0),
            data: core::cell::UnsafeCell::new(val),
        }
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
        // it will read 0xFFFFFFFF (uninitialized value in CPU_DATA).
        // Fallback to the slow but reliable LAPIC ID to avoid lock owner collisions.
        if id == 0xFFFF_FFFF {
            return crate::apic::get_id() as i32;
        }
        
        id as i32
    }

    pub fn lock(&self) -> ReentrantMutexGuard<'_, T> {
        let me = Self::current_cpu();
        if self.owner.load(Ordering::Acquire) == me {
            // Same CPU re-entering: just increment depth.
            self.depth.fetch_add(1, Ordering::Relaxed);
        } else {
            // Spin until we can acquire ownership.
            while self.owner.compare_exchange_weak(
                -2, me, Ordering::Acquire, Ordering::Relaxed,
            ).is_err() {
                core::hint::spin_loop();
            }
            // Now we ARE the owner. Increment depth to 1.
            // Using fetch_add here is safer than store(1) to handle interrupts 
            // that might have occurred between CAS and this line.
            self.depth.fetch_add(1, Ordering::Relaxed);
        }
        ReentrantMutexGuard { lock: self }
    }

    pub fn try_lock(&self) -> Option<ReentrantMutexGuard<'_, T>> {
        let me = Self::current_cpu();
        if self.owner.load(Ordering::Acquire) == me {
            self.depth.fetch_add(1, Ordering::Relaxed);
            Some(ReentrantMutexGuard { lock: self })
        } else {
            if self.owner.compare_exchange_weak(
                -2, me, Ordering::Acquire, Ordering::Relaxed,
            ).is_ok() {
                self.depth.fetch_add(1, Ordering::Relaxed);
                Some(ReentrantMutexGuard { lock: self })
            } else {
                None
            }
        }
    }

    /// Forcedly unlock the mutex.
    /// Danger: should ONLY be used in fork_child_trampoline to clear inherited locks.
    pub unsafe fn force_unlock(&self) {
        self.depth.store(0, Ordering::Relaxed);
        self.owner.store(-2, Ordering::Release);
    }
}

impl<T> Drop for ReentrantMutexGuard<'_, T> {
    fn drop(&mut self) {
        let d = self.lock.depth.load(Ordering::Relaxed);
        if d <= 1 {
            self.lock.depth.store(0, Ordering::Relaxed);
            self.lock.owner.store(-2, Ordering::Release);
        } else {
            self.lock.depth.fetch_sub(1, Ordering::Relaxed);
        }
    }
}
