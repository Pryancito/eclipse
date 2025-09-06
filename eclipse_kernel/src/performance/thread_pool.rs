//! Thread Pool para Eclipse OS
//!
//! Este módulo implementa un thread pool optimizado para
//! mejorar el rendimiento del sistema multihilo

use crate::process::{ThreadId, ProcessId};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use alloc::vec::Vec;
use alloc::collections::VecDeque;

/// Estado de un worker thread
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WorkerState {
    Idle,
    Busy,
    Terminated,
}

/// Worker thread del pool
#[derive(Debug, Clone)]
pub struct WorkerThread {
    pub thread_id: ThreadId,
    pub process_id: ProcessId,
    pub state: WorkerState,
    pub task_count: u64,
    pub total_work_time: u64,
    pub last_activity: u64,
    pub cpu_affinity: Option<usize>,
}

/// Tarea para el thread pool
#[derive(Debug, Clone)]
pub struct Task {
    pub task_id: u64,
    pub priority: u8,
    pub estimated_duration: u64,
    pub data: Vec<u8>,
    pub callback: Option<fn() -> ()>,
}

/// Configuración del thread pool
#[derive(Debug, Clone)]
pub struct ThreadPoolConfig {
    pub min_threads: usize,
    pub max_threads: usize,
    pub initial_threads: usize,
    pub thread_timeout: u64,
    pub task_queue_size: usize,
    pub enable_work_stealing: bool,
    pub enable_cpu_affinity: bool,
    pub enable_dynamic_scaling: bool,
}

impl Default for ThreadPoolConfig {
    fn default() -> Self {
        Self {
            min_threads: 2,
            max_threads: 16,
            initial_threads: 4,
            thread_timeout: 30000000000, // 30 segundos en nanosegundos
            task_queue_size: 1000,
            enable_work_stealing: true,
            enable_cpu_affinity: false,
            enable_dynamic_scaling: true,
        }
    }
}

/// Thread Pool optimizado
pub struct ThreadPool {
    config: ThreadPoolConfig,
    workers: Vec<WorkerThread>,
    task_queue: VecDeque<Task>,
    next_task_id: AtomicU64,
    total_tasks_completed: AtomicUsize,
    total_work_time: AtomicU64,
    pool_utilization: AtomicU64, // 0-100
    active_workers: AtomicUsize,
}

impl ThreadPool {
    /// Crear un nuevo thread pool
    pub fn new() -> Self {
        let config = ThreadPoolConfig::default();
        
        Self {
            config,
            workers: Vec::new(),
            task_queue: VecDeque::new(),
            next_task_id: AtomicU64::new(1),
            total_tasks_completed: AtomicUsize::new(0),
            total_work_time: AtomicU64::new(0),
            pool_utilization: AtomicU64::new(0),
            active_workers: AtomicUsize::new(0),
        }
    }

    /// Inicializar el thread pool
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        // Limpiar workers existentes
        self.workers.clear();
        self.task_queue.clear();
        
        // Crear workers iniciales
        for i in 0..self.config.initial_threads {
            let worker = WorkerThread {
                thread_id: i as u32,
                process_id: 0, // Kernel process
                state: WorkerState::Idle,
                task_count: 0,
                total_work_time: 0,
                last_activity: self.get_current_time(),
                cpu_affinity: if self.config.enable_cpu_affinity { Some(i) } else { None },
            };
            self.workers.push(worker);
        }
        
        self.active_workers.store(self.config.initial_threads, Ordering::Release);
        
        Ok(())
    }

    /// Enviar una tarea al pool
    pub fn submit_task(&mut self, mut task: Task) -> Result<u64, &'static str> {
        if self.task_queue.len() >= self.config.task_queue_size {
            return Err("Task queue is full");
        }
        
        // Asignar ID a la tarea
        task.task_id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
        
        // Guardar el task_id antes de mover la tarea
        let task_id = task.task_id;
        
        // Agregar a la cola de tareas
        self.task_queue.push_back(task);
        
        // Intentar asignar la tarea a un worker
        self.assign_tasks();
        
        Ok(task_id)
    }

    /// Asignar tareas a workers disponibles
    fn assign_tasks(&mut self) {
        while let Some(task) = self.task_queue.pop_front() {
            if let Some(worker) = self.find_idle_worker() {
                self.assign_task_to_worker(worker, task);
            } else {
                // No hay workers disponibles, devolver tarea a la cola
                self.task_queue.push_front(task);
                break;
            }
        }
    }

    /// Encontrar un worker idle
    fn find_idle_worker(&mut self) -> Option<usize> {
        for (index, worker) in self.workers.iter_mut().enumerate() {
            if worker.state == WorkerState::Idle {
                return Some(index);
            }
        }
        
        // Si no hay workers idle y podemos crear más
        if self.workers.len() < self.config.max_threads && self.config.enable_dynamic_scaling {
            self.create_new_worker()
        } else {
            None
        }
    }

    /// Crear un nuevo worker
    fn create_new_worker(&mut self) -> Option<usize> {
        let worker_id = self.workers.len() as u32;
        let worker = WorkerThread {
            thread_id: worker_id,
            process_id: 0,
            state: WorkerState::Idle,
            task_count: 0,
            total_work_time: 0,
            last_activity: self.get_current_time(),
            cpu_affinity: if self.config.enable_cpu_affinity { Some(worker_id as usize) } else { None },
        };
        
        self.workers.push(worker);
        self.active_workers.fetch_add(1, Ordering::Relaxed);
        
        Some(self.workers.len() - 1)
    }

    /// Asignar tarea a un worker
    fn assign_task_to_worker(&mut self, worker_index: usize, task: Task) {
        let current_time = self.get_current_time();
        let end_time = self.get_current_time();
        if let Some(worker) = self.workers.get_mut(worker_index) {
            worker.state = WorkerState::Busy;
            worker.task_count += 1;
            worker.last_activity = current_time;
            
            // Simular ejecución de la tarea directamente
            let work_time = task.estimated_duration;
            worker.total_work_time += work_time;
            self.total_work_time.fetch_add(work_time, Ordering::Relaxed);
            self.total_tasks_completed.fetch_add(1, Ordering::Relaxed);
            worker.state = WorkerState::Idle;
            worker.last_activity = end_time;
            
            // Ejecutar callback si existe
            if let Some(callback) = task.callback {
                callback();
            }
        }
    }

    /// Simular ejecución de tarea
    fn simulate_task_execution(&mut self, worker: &mut WorkerThread, task: &Task) {
        // En un sistema real, esto ejecutaría la tarea real
        // Por ahora, simulamos con el tiempo estimado
        
        let work_time = task.estimated_duration;
        worker.total_work_time += work_time;
        self.total_work_time.fetch_add(work_time, Ordering::Relaxed);
        
        // Marcar tarea como completada
        self.total_tasks_completed.fetch_add(1, Ordering::Relaxed);
        
        // Marcar worker como idle
        worker.state = WorkerState::Idle;
        let current_time = self.get_current_time();
        worker.last_activity = current_time;
        
        // Ejecutar callback si existe
        if let Some(callback) = task.callback {
            callback();
        }
    }

    /// Optimizar el thread pool
    pub fn optimize_pool(&mut self) -> Result<(), &'static str> {
        if self.config.enable_dynamic_scaling {
            self.scale_pool();
        }
        
        if self.config.enable_work_stealing {
            self.steal_work();
        }
        
        self.cleanup_idle_workers();
        self.update_utilization();
        
        Ok(())
    }

    /// Escalar el pool dinámicamente
    fn scale_pool(&mut self) {
        let current_utilization = self.pool_utilization.load(Ordering::Acquire);
        let active_workers = self.active_workers.load(Ordering::Relaxed);
        
        // Si la utilización es alta y hay tareas en cola, agregar workers
        if current_utilization > 80 && !self.task_queue.is_empty() && active_workers < self.config.max_threads {
            self.create_new_worker();
        }
        
        // Si la utilización es baja y hay muchos workers, remover algunos
        if current_utilization < 20 && active_workers > self.config.min_threads {
            self.remove_idle_worker();
        }
    }

    /// Robar trabajo entre workers
    fn steal_work(&mut self) {
        // Implementación simple de work stealing
        // En un sistema real, esto sería más complejo
        
        let busy_workers: Vec<usize> = self.workers.iter()
            .enumerate()
            .filter(|(_, worker)| worker.state == WorkerState::Busy)
            .map(|(index, _)| index)
            .collect();
        
        let idle_workers: Vec<usize> = self.workers.iter()
            .enumerate()
            .filter(|(_, worker)| worker.state == WorkerState::Idle)
            .map(|(index, _)| index)
            .collect();
        
        // Si hay workers idle y tareas en cola, redistribuir
        if !idle_workers.is_empty() && !self.task_queue.is_empty() {
            self.assign_tasks();
        }
    }

    /// Limpiar workers idle
    fn cleanup_idle_workers(&mut self) {
        let current_time = self.get_current_time();
        let timeout = self.config.thread_timeout;
        let min_threads = self.config.min_threads;
        let current_len = self.workers.len();
        
        // Remover workers que han estado idle por mucho tiempo
        self.workers.retain(|worker| {
            if worker.state == WorkerState::Idle && 
               current_time - worker.last_activity > timeout &&
               current_len > min_threads {
                self.active_workers.fetch_sub(1, Ordering::Relaxed);
                false
            } else {
                true
            }
        });
    }

    /// Remover un worker idle
    fn remove_idle_worker(&mut self) {
        if let Some(index) = self.workers.iter().position(|w| w.state == WorkerState::Idle) {
            self.workers.remove(index);
            self.active_workers.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Actualizar utilización del pool
    fn update_utilization(&mut self) {
        let active_workers = self.active_workers.load(Ordering::Relaxed);
        let busy_workers = self.workers.iter()
            .filter(|w| w.state == WorkerState::Busy)
            .count();
        
        if active_workers > 0 {
            let utilization = (busy_workers as f64 / active_workers as f64) * 100.0;
            self.pool_utilization.store(utilization as u64, Ordering::Release);
        }
    }

    /// Obtener utilización del pool
    pub fn get_utilization(&self) -> f64 {
        self.pool_utilization.load(Ordering::Acquire) as f64
    }

    /// Obtener estadísticas del pool
    pub fn get_stats(&self) -> ThreadPoolStats {
        let active_workers = self.active_workers.load(Ordering::Relaxed);
        let busy_workers = self.workers.iter()
            .filter(|w| w.state == WorkerState::Busy)
            .count();
        let idle_workers = active_workers - busy_workers;
        
        ThreadPoolStats {
            total_workers: self.workers.len(),
            active_workers,
            busy_workers,
            idle_workers,
            queued_tasks: self.task_queue.len(),
            completed_tasks: self.total_tasks_completed.load(Ordering::Relaxed),
            total_work_time: self.total_work_time.load(Ordering::Relaxed),
            utilization: self.pool_utilization.load(Ordering::Acquire) as f64,
            average_task_duration: self.calculate_average_task_duration(),
        }
    }

    /// Calcular duración promedio de tareas
    fn calculate_average_task_duration(&self) -> u64 {
        let completed_tasks = self.total_tasks_completed.load(Ordering::Relaxed);
        let total_work_time = self.total_work_time.load(Ordering::Relaxed);
        
        if completed_tasks > 0 {
            total_work_time / completed_tasks as u64
        } else {
            0
        }
    }

    /// Actualizar configuración
    pub fn update_config(&mut self, config: ThreadPoolConfig) {
        let min_threads = config.min_threads;
        let max_threads = config.max_threads;
        
        self.config = config;
        
        // Ajustar número de workers si es necesario
        if self.workers.len() < min_threads {
            while self.workers.len() < min_threads {
                self.create_new_worker();
            }
        } else if self.workers.len() > max_threads {
            while self.workers.len() > max_threads {
                self.remove_idle_worker();
            }
        }
    }

    /// Obtener tiempo actual (simulado)
    fn get_current_time(&self) -> u64 {
        static mut COUNTER: u64 = 0;
        unsafe {
            COUNTER += 1;
            COUNTER
        }
    }

    /// Obtener workers
    pub fn get_workers(&self) -> &[WorkerThread] {
        &self.workers
    }

    /// Obtener tareas en cola
    pub fn get_queued_tasks(&self) -> usize {
        self.task_queue.len()
    }

    /// Limpiar el pool
    pub fn clear(&mut self) {
        self.workers.clear();
        self.task_queue.clear();
        self.total_tasks_completed.store(0, Ordering::Relaxed);
        self.total_work_time.store(0, Ordering::Relaxed);
        self.pool_utilization.store(0, Ordering::Release);
        self.active_workers.store(0, Ordering::Relaxed);
    }
}

/// Estadísticas del thread pool
#[derive(Debug, Clone)]
pub struct ThreadPoolStats {
    pub total_workers: usize,
    pub active_workers: usize,
    pub busy_workers: usize,
    pub idle_workers: usize,
    pub queued_tasks: usize,
    pub completed_tasks: usize,
    pub total_work_time: u64,
    pub utilization: f64,
    pub average_task_duration: u64,
}
