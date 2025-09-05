//! ReactOS Rust Kernel - Synchronization
//!
//! Sistema de sincronización del kernel.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Mutex simple
#[derive(Debug)]
pub struct Mutex {
    locked: AtomicBool,
    owner: AtomicU32,
}

impl Mutex {
    /// Crear un nuevo mutex
    pub fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            owner: AtomicU32::new(0),
        }
    }
    
    /// Intentar adquirir el mutex
    pub fn try_lock(&self, thread_id: u32) -> bool {
        if self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            self.owner.store(thread_id, Ordering::Release);
            true
        } else {
            false
        }
    }
    
    /// Liberar el mutex
    pub fn unlock(&self, thread_id: u32) -> bool {
        if self.owner.load(Ordering::Acquire) == thread_id {
            self.owner.store(0, Ordering::Release);
            self.locked.store(false, Ordering::Release);
            true
        } else {
            false
        }
    }
    
    /// Verificar si está bloqueado
    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Acquire)
    }
}

/// Semáforo simple
#[derive(Debug)]
pub struct Semaphore {
    count: AtomicU32,
    max_count: u32,
}

impl Semaphore {
    /// Crear un nuevo semáforo
    pub fn new(initial_count: u32, max_count: u32) -> Self {
        Self {
            count: AtomicU32::new(initial_count),
            max_count,
        }
    }
    
    /// Intentar adquirir el semáforo
    pub fn try_acquire(&self) -> bool {
        let current = self.count.load(Ordering::Acquire);
        if current > 0 {
            self.count.compare_exchange(current, current - 1, Ordering::Acquire, Ordering::Relaxed).is_ok()
        } else {
            false
        }
    }
    
    /// Liberar el semáforo
    pub fn release(&self) -> bool {
        let current = self.count.load(Ordering::Acquire);
        if current < self.max_count {
            self.count.compare_exchange(current, current + 1, Ordering::Acquire, Ordering::Relaxed).is_ok()
        } else {
            false
        }
    }
    
    /// Obtener el conteo actual
    pub fn get_count(&self) -> u32 {
        self.count.load(Ordering::Acquire)
    }
}

/// Administrador de sincronización
pub struct SynchronizationManager {
    pub mutexes: [Option<Mutex>; 256],
    pub semaphores: [Option<Semaphore>; 256],
    pub next_mutex_id: u32,
    pub next_semaphore_id: u32,
}

impl SynchronizationManager {
    /// Crear un nuevo administrador de sincronización
    pub fn new() -> Self {
        Self {
            mutexes: [const { None }; 256],
            semaphores: [const { None }; 256],
            next_mutex_id: 1,
            next_semaphore_id: 1,
        }
    }
    
    /// Inicializar el administrador de sincronización
    pub fn init(&mut self) {
        // Crear mutex del kernel
        self.mutexes[0] = Some(Mutex::new());
        
        // Crear semáforo del kernel
        self.semaphores[0] = Some(Semaphore::new(1, 1));
    }
    
    /// Crear un nuevo mutex
    pub fn create_mutex(&mut self) -> Option<u32> {
        let mutex_id = self.next_mutex_id;
        self.next_mutex_id += 1;
        
        for i in 1..256 {
            if self.mutexes[i].is_none() {
                self.mutexes[i] = Some(Mutex::new());
                return Some(mutex_id);
            }
        }
        
        None
    }
    
    /// Crear un nuevo semáforo
    pub fn create_semaphore(&mut self, initial_count: u32, max_count: u32) -> Option<u32> {
        let semaphore_id = self.next_semaphore_id;
        self.next_semaphore_id += 1;
        
        for i in 1..256 {
            if self.semaphores[i].is_none() {
                self.semaphores[i] = Some(Semaphore::new(initial_count, max_count));
                return Some(semaphore_id);
            }
        }
        
        None
    }
    
    /// Obtener un mutex por ID
    pub fn get_mutex(&self, mutex_id: u32) -> Option<&Mutex> {
        for i in 0..256 {
            if let Some(ref mutex) = self.mutexes[i] {
                if i as u32 == mutex_id {
                    return Some(mutex);
                }
            }
        }
        None
    }
    
    /// Obtener un semáforo por ID
    pub fn get_semaphore(&self, semaphore_id: u32) -> Option<&Semaphore> {
        for i in 0..256 {
            if let Some(ref semaphore) = self.semaphores[i] {
                if i as u32 == semaphore_id {
                    return Some(semaphore);
                }
            }
        }
        None
    }
}

/// Instancia global del administrador de sincronización
static mut SYNC_MANAGER: Option<SynchronizationManager> = None;

/// Inicializar el sistema de sincronización
pub fn init() {
    unsafe {
        SYNC_MANAGER = Some(SynchronizationManager::new());
        if let Some(ref mut manager) = SYNC_MANAGER {
            manager.init();
        }
    }
}

/// Crear un nuevo mutex
pub fn create_mutex() -> Option<u32> {
    unsafe {
        if let Some(ref mut manager) = SYNC_MANAGER {
            manager.create_mutex()
        } else {
            None
        }
    }
}

/// Crear un nuevo semáforo
pub fn create_semaphore(initial_count: u32, max_count: u32) -> Option<u32> {
    unsafe {
        if let Some(ref mut manager) = SYNC_MANAGER {
            manager.create_semaphore(initial_count, max_count)
        } else {
            None
        }
    }
}

/// Intentar adquirir un mutex
pub fn try_lock_mutex(mutex_id: u32, thread_id: u32) -> bool {
    unsafe {
        if let Some(ref manager) = SYNC_MANAGER {
            if let Some(mutex) = manager.get_mutex(mutex_id) {
                mutex.try_lock(thread_id)
            } else {
                false
            }
        } else {
            false
        }
    }
}

/// Liberar un mutex
pub fn unlock_mutex(mutex_id: u32, thread_id: u32) -> bool {
    unsafe {
        if let Some(ref manager) = SYNC_MANAGER {
            if let Some(mutex) = manager.get_mutex(mutex_id) {
                mutex.unlock(thread_id)
            } else {
                false
            }
        } else {
            false
        }
    }
}

/// Intentar adquirir un semáforo
pub fn try_acquire_semaphore(semaphore_id: u32) -> bool {
    unsafe {
        if let Some(ref manager) = SYNC_MANAGER {
            if let Some(semaphore) = manager.get_semaphore(semaphore_id) {
                semaphore.try_acquire()
            } else {
                false
            }
        } else {
            false
        }
    }
}

/// Liberar un semáforo
pub fn release_semaphore(semaphore_id: u32) -> bool {
    unsafe {
        if let Some(ref manager) = SYNC_MANAGER {
            if let Some(semaphore) = manager.get_semaphore(semaphore_id) {
                semaphore.release()
            } else {
                false
            }
        } else {
            false
        }
    }
}