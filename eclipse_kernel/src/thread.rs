//! # Gestión de Hilos del Kernel

use crate::{KernelError, KernelResult};
use alloc::{
    string::{String, ToString},
    vec::Vec,
};

#[derive(Debug, Clone)]
pub struct ThreadInfo {
    pub thread_id: u32,
    pub process_id: u32,
    pub name: String,
    pub state: ThreadState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadState {
    Running,
    Suspended,
    Terminated,
}

pub struct ThreadManager {
    threads: Vec<ThreadInfo>,
    next_thread_id: u32,
}

impl ThreadManager {
    pub fn new() -> Self {
        Self {
            threads: Vec::new(),
            next_thread_id: 1,
        }
    }

    pub fn initialize(&mut self) -> KernelResult<()> {
        // Crear hilo principal del sistema
        let system_thread = ThreadInfo {
            thread_id: 0,
            process_id: 0,
            name: "System Main Thread".to_string(),
            state: ThreadState::Running,
        };
        self.threads.push(system_thread);
        Ok(())
    }

    pub fn create_thread(&mut self, process_id: u32, name: &str) -> KernelResult<u32> {
        let thread_id = self.next_thread_id;
        self.next_thread_id += 1;

        let thread_info = ThreadInfo {
            thread_id,
            process_id,
            name: name.to_string(),
            state: ThreadState::Running,
        };

        self.threads.push(thread_info);
        Ok(thread_id)
    }

    pub fn terminate_thread(&mut self, thread_id: u32) -> KernelResult<()> {
        if let Some(thread) = self.threads.iter_mut().find(|t| t.thread_id == thread_id) {
            thread.state = ThreadState::Terminated;
            Ok(())
        } else {
            Err(KernelError::ThreadError)
        }
    }
}

static mut THREAD_MANAGER: Option<ThreadManager> = None;

pub fn initialize() -> KernelResult<()> {
    unsafe {
        THREAD_MANAGER = Some(ThreadManager::new());
        if let Some(ref mut manager) = THREAD_MANAGER {
            manager.initialize()?;
        }
    }
    Ok(())
}

pub fn create_thread(process_id: u32, name: &str) -> KernelResult<u32> {
    unsafe {
        if let Some(ref mut manager) = THREAD_MANAGER {
            manager.create_thread(process_id, name)
        } else {
            Err(KernelError::ThreadError)
        }
    }
}

pub fn terminate_thread(thread_id: u32) -> KernelResult<()> {
    unsafe {
        if let Some(ref mut manager) = THREAD_MANAGER {
            manager.terminate_thread(thread_id)
        } else {
            Err(KernelError::ThreadError)
        }
    }
}

/// Función init() requerida por main.rs
pub fn init() -> KernelResult<()> {
    initialize()
}

/// Procesar cola de hilos (función requerida por main.rs)
pub fn process_thread_queue() {
    // Implementación básica para procesar cola de hilos
    unsafe {
        if let Some(ref mut manager) = THREAD_MANAGER {
            // Procesar hilos en estado Running
            for thread in manager.threads.iter_mut() {
                if thread.state == ThreadState::Running {
                    // Simular procesamiento del hilo
                    // En un kernel real, aquí se ejecutaría el código del hilo
                }
            }
        }
    }
}

/// Obtener estadísticas de hilos (compatible con main.rs)
pub fn get_thread_stats() -> (usize, usize, usize) {
    unsafe {
        if let Some(ref manager) = THREAD_MANAGER {
            let running_threads = manager
                .threads
                .iter()
                .filter(|t| t.state == ThreadState::Running)
                .count();
            let ready_threads = manager
                .threads
                .iter()
                .filter(|t| t.state == ThreadState::Suspended)
                .count();
            let blocked_threads = 0; // Simplificado
            (running_threads, ready_threads, blocked_threads)
        } else {
            (0, 0, 0)
        }
    }
}
