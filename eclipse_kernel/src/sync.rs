<<<<<<< HEAD
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
=======
use core::sync::atomic::{AtomicU32, Ordering};
use core::cell::UnsafeCell;
use spin::Mutex;

/// A reentrant spinlock (recursive mutex)
///
/// This allows the same CPU to acquire the lock multiple times without deadlocking.
/// Essential for logging systems where an operation under a lock might trigger
/// further logging calls (e.g. nested interrupts or exceptions).
pub struct ReentrantMutex<T> {
    lock: Mutex<()>,
    owner: AtomicU32,      // APIC ID of the owning CPU
    recursion: AtomicU32,  // Recursion depth
    data: UnsafeCell<T>,
}

const NO_OWNER: u32 = 0xFFFFFFFF;

impl<T> ReentrantMutex<T> {
    pub const fn new(value: T) -> Self {
        Self {
            lock: Mutex::new(()),
            owner: AtomicU32::new(NO_OWNER),
            recursion: AtomicU32::new(0),
            data: UnsafeCell::new(value),
        }
    }

    pub fn lock(&self) -> ReentrantMutexGuard<T> {
        let current_cpu = crate::boot::get_cpu_id() as u32;

        if self.owner.load(Ordering::Acquire) == current_cpu {
            // Reentrant acquisition: increment recursion counter
            self.recursion.fetch_add(1, Ordering::AcqRel);
        } else {
            // First-time acquisition for this CPU: take the spinlock
            let guard = self.lock.lock();
            // Store our CPU ID and initial recursion level
            // Release ordering ensures that any data modified on this CPU 
            // before this store becomes visible to other CPUs that acquire the lock.
            self.owner.store(current_cpu, Ordering::Release);
            self.recursion.store(1, Ordering::Release);
            // Forget the inner guard; we'll handle the unlock manually in Drop
            core::mem::forget(guard);
        }
        
        ReentrantMutexGuard { mutex: self }
    }
}

pub struct ReentrantMutexGuard<'a, T> {
    mutex: &'a ReentrantMutex<T>,
}

impl<'a, T> core::ops::Deref for ReentrantMutexGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        // SAFETY: We hold the lock
        unsafe { &*self.mutex.data.get() }
    }
}

impl<'a, T> core::ops::DerefMut for ReentrantMutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: We hold the lock
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<'a, T> Drop for ReentrantMutexGuard<'a, T> {
    fn drop(&mut self) {
        let rec = self.mutex.recursion.fetch_sub(1, Ordering::AcqRel);
        if rec == 1 {
            // Final release: clear owner and unlock the inner spinlock
            self.mutex.owner.store(NO_OWNER, Ordering::Release);
            unsafe {
                self.mutex.lock.force_unlock();
            }
        }
    }
}

// Ensure the mutex can be safely shared across threads if T is Send
unsafe impl<T: Send> Sync for ReentrantMutex<T> {}
unsafe impl<T: Send> Send for ReentrantMutex<T> {}
>>>>>>> ebec8d09 (sync)
