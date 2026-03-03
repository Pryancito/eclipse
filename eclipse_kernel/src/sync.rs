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

impl<T> ReentrantMutex<T> {
    pub const fn new(val: T) -> Self {
        Self {
            owner: AtomicI32::new(-1),
            depth: AtomicU32::new(0),
            data: core::cell::UnsafeCell::new(val),
        }
    }

    /// Get the current CPU's id for ownership tracking.
    /// Reads gs:[16] which holds the cpu_id written by `load_gdt()`.
    /// Returns 0 if GDT is not yet loaded (BSP-only early boot phase).
    fn current_cpu() -> i32 {
        let id: u32;
        unsafe {
            core::arch::asm!(
                "mov {0:e}, gs:[16]",
                out(reg) id,
                options(nomem, nostack, preserves_flags)
            );
        }
        id as i32
    }

    pub fn lock(&self) -> ReentrantMutexGuard<'_, T> {
        let me = Self::current_cpu();
        if self.owner.load(Ordering::Acquire) == me {
            // Same CPU re-entering: just increment depth.
            self.depth.fetch_add(1, Ordering::Relaxed);
        } else {
            // Spin until we can acquire.
            loop {
                match self.owner.compare_exchange_weak(
                    -1, me, Ordering::Acquire, Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(_) => core::hint::spin_loop(),
                }
            }
            self.depth.store(1, Ordering::Relaxed);
        }
        ReentrantMutexGuard { lock: self }
    }
}

impl<T> Drop for ReentrantMutexGuard<'_, T> {
    fn drop(&mut self) {
        let d = self.lock.depth.load(Ordering::Relaxed);
        if d <= 1 {
            self.lock.depth.store(0, Ordering::Relaxed);
            self.lock.owner.store(-1, Ordering::Release);
        } else {
            self.lock.depth.fetch_sub(1, Ordering::Relaxed);
        }
    }
}
