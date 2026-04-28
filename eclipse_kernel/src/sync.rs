//! Synchronization primitives for the Eclipse kernel.

use core::sync::atomic::{AtomicU64, AtomicU32, Ordering};

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

const NO_CPU: i32 = 0;

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

    /// Current owner key for re-entrancy.
    ///
    /// IMPORTANT: this lock must remain re-entrant even if the scheduler migrates a task between
    /// CPUs. Using only the CPU id can deadlock if a task acquires the lock, migrates, and then
    /// tries to re-enter or unlock from another CPU.
    ///
    /// Strategy:
    /// - Prefer the current process id (pid) when available.
    /// - Fall back to CPU id early during boot / when no pid exists.
    fn current_owner_key() -> u32 {
        if let Some(pid) = crate::process::current_process_id() {
            // 1-indexed so 0 still represents NO_CPU/unused in the packed state.
            (pid as u32).wrapping_add(1)
        } else {
            // High-bit namespace for early/boot CPU-based ownership.
            let cpu = crate::boot::get_cpu_id() as u32;
            0x8000_0000u32 | cpu.wrapping_add(1)
        }
    }

    /// Get the current CPU's id for debugging/telemetry.
    pub fn current_cpu() -> i32 {
        #[cfg(test)]
        {
            return 0; // Host tests always single-core for now
        }

        #[cfg(not(test))]
        {
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
            // Fallback to the reliable (but slower) get_cpu_id to avoid lock owner collisions.
            if id == 0xFFFF_FFFF || id >= 32 { // MAX_SMP_CPUS
                return crate::boot::get_cpu_id() as i32;
            }
            
            id as i32
        }
    }

    pub fn lock(&self) -> ReentrantMutexGuard<'_, T> {
        let me = Self::current_owner_key();
        loop {
            let current = self.state.load(Ordering::Acquire);
            let (owner_raw, depth) = Self::unpack(current);
            let owner = owner_raw as u32;

            if owner == me {
                // Same CPU re-entering: increment depth atomically.
                let next = Self::pack(me as i32, depth + 1);
                if self.state.compare_exchange_weak(current, next, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                    return ReentrantMutexGuard { lock: self };
                }
            } else if owner == 0 {
                // Try to acquire ownership.
                let next = Self::pack(me as i32, 1);
                if self.state.compare_exchange_weak(current, next, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                    return ReentrantMutexGuard { lock: self };
                }
            }
            
            // Spin until available or re-entrant.
            core::hint::spin_loop();
        }
    }

    pub fn try_lock(&self) -> Option<ReentrantMutexGuard<'_, T>> {
        let me = Self::current_owner_key();
        let current = self.state.load(Ordering::Acquire);
        let (owner_raw, depth) = Self::unpack(current);
        let owner = owner_raw as u32;

        if owner == me {
            let next = Self::pack(me as i32, depth + 1);
            if self.state.compare_exchange_weak(current, next, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                return Some(ReentrantMutexGuard { lock: self });
            }
        } else if owner == 0 {
            let next = Self::pack(me as i32, 1);
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

/// Mutex con soporte para Herencia de Prioridad (Real-Time).
/// Rastrea el PID del propietario para permitir que el planificador eleve su prioridad
/// si un proceso de tiempo real se bloquea esperando este recurso.
pub struct RtMutex<T> {
    owner: AtomicU32,
    inner: spin::Mutex<T>,
}

pub struct RtMutexGuard<'a, T> {
    lock: &'a RtMutex<T>,
    guard: spin::MutexGuard<'a, T>,
}

impl<T> RtMutex<T> {
    pub const fn new(val: T) -> Self {
        Self {
            owner: AtomicU32::new(0),
            inner: spin::Mutex::new(val),
        }
    }

    pub fn lock(&self) -> RtMutexGuard<'_, T> {
        let me = crate::process::current_process_id().unwrap_or(0);
        
        // Si el lock está ocupado, podríamos necesitar herencia de prioridad aquí.
        // Por ahora, implementamos el rastreo del owner.
        let guard = self.inner.lock();
        self.owner.store(me as u32, Ordering::Release);
        
        RtMutexGuard { lock: self, guard }
    }

    pub fn owner(&self) -> u32 {
        self.owner.load(Ordering::Acquire)
    }
}

impl<T> core::ops::Deref for RtMutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T { &*self.guard }
}

impl<T> core::ops::DerefMut for RtMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T { &mut *self.guard }
}

impl<T> Drop for RtMutexGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.owner.store(0, Ordering::Release);
    }
}
