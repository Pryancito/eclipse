//! Scheduler de Procesos para Eclipse OS
//!
//! Implementa diferentes algoritmos de scheduling

use crate::process::process::{ProcessControlBlock, ProcessId, ProcessPriority, ThreadId};

/// Algoritmos de scheduling disponibles
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SchedulingAlgorithm {
    /// Round Robin - turnos equitativos
    RoundRobin,
    /// Por prioridad - procesos de mayor prioridad primero
    Priority,
    /// First Come First Served - primero en llegar, primero en ser servido
    FCFS,
    /// Shortest Job First - trabajo más corto primero
    SJF,
    /// Multilevel Feedback Queue - colas múltiples con retroalimentación
    MLFQ,
}

/// Cola de procesos
#[derive(Debug, Clone)]
pub struct ProcessQueue {
    /// Procesos en la cola
    pub processes: [Option<ProcessId>; 256],
    /// Índice del primer proceso
    pub head: usize,
    /// Índice del último proceso
    pub tail: usize,
    /// Número de procesos en la cola
    pub count: usize,
}

impl ProcessQueue {
    /// Crear una nueva cola vacía
    pub const fn new() -> Self {
        Self {
            processes: [None; 256],
            head: 0,
            tail: 0,
            count: 0,
        }
    }

    /// Verificar si la cola está vacía
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Verificar si la cola está llena
    pub fn is_full(&self) -> bool {
        self.count >= 256
    }

    /// Agregar un proceso al final de la cola
    pub fn enqueue(&mut self, pid: ProcessId) -> bool {
        if self.is_full() {
            return false;
        }

        self.processes[self.tail] = Some(pid);
        self.tail = (self.tail + 1) % 256;
        self.count += 1;
        true
    }

    /// Remover el primer proceso de la cola
    pub fn dequeue(&mut self) -> Option<ProcessId> {
        if self.is_empty() {
            return None;
        }

        let pid = self.processes[self.head];
        self.processes[self.head] = None;
        self.head = (self.head + 1) % 256;
        self.count -= 1;
        pid
    }

    /// Obtener el primer proceso sin removerlo
    pub fn peek(&self) -> Option<ProcessId> {
        if self.is_empty() {
            None
        } else {
            self.processes[self.head]
        }
    }

    /// Remover un proceso específico de la cola
    pub fn remove(&mut self, pid: ProcessId) -> bool {
        for i in 0..self.count {
            let index = (self.head + i) % 256;
            if let Some(queue_pid) = self.processes[index] {
                if queue_pid == pid {
                    // Mover todos los elementos hacia la izquierda
                    for j in i..self.count - 1 {
                        let current = (self.head + j) % 256;
                        let next = (self.head + j + 1) % 256;
                        self.processes[current] = self.processes[next];
                    }
                    self.processes[self.tail] = None;
                    self.tail = (self.tail + 256 - 1) % 256;
                    self.count -= 1;
                    return true;
                }
            }
        }
        false
    }
}

/// Scheduler principal del sistema
#[derive(Debug)]
pub struct ProcessScheduler {
    /// Algoritmo de scheduling actual
    pub algorithm: SchedulingAlgorithm,
    /// Cola de procesos listos
    pub ready_queue: ProcessQueue,
    /// Cola de procesos bloqueados
    pub blocked_queue: ProcessQueue,
    /// Proceso actualmente ejecutándose
    pub current_process: Option<ProcessId>,
    /// Proceso anterior (para context switching)
    pub previous_process: Option<ProcessId>,
    /// Tiempo de quantum para Round Robin
    pub quantum: u64,
    /// Tiempo actual del sistema
    pub current_time: u64,
    /// Contador de procesos creados
    pub process_count: u32,
    /// Contador de context switches
    pub context_switches: u64,
}

impl ProcessScheduler {
    /// Crear un nuevo scheduler
    pub fn new(algorithm: SchedulingAlgorithm) -> Self {
        Self {
            algorithm,
            ready_queue: ProcessQueue::new(),
            blocked_queue: ProcessQueue::new(),
            current_process: None,
            previous_process: None,
            quantum: 100, // 100ms por defecto
            current_time: 0,
            process_count: 0,
            context_switches: 0,
        }
    }

    /// Agregar un proceso al scheduler
    pub fn add_process(&mut self, pid: ProcessId) -> bool {
        if self.ready_queue.enqueue(pid) {
            self.process_count += 1;
            true
        } else {
            false
        }
    }

    /// Remover un proceso del scheduler
    pub fn remove_process(&mut self, pid: ProcessId) -> bool {
        let mut removed = false;

        // Remover de la cola de listos
        if self.ready_queue.remove(pid) {
            removed = true;
        }

        // Remover de la cola de bloqueados
        if self.blocked_queue.remove(pid) {
            removed = true;
        }

        // Si es el proceso actual, limpiarlo
        if self.current_process == Some(pid) {
            self.current_process = None;
            removed = true;
        }

        if removed {
            self.process_count -= 1;
        }

        removed
    }

    /// Bloquear el proceso actual
    pub fn block_current_process(&mut self) -> Option<ProcessId> {
        if let Some(pid) = self.current_process {
            self.blocked_queue.enqueue(pid);
            self.current_process = None;
            Some(pid)
        } else {
            None
        }
    }

    /// Desbloquear un proceso
    pub fn unblock_process(&mut self, pid: ProcessId) -> bool {
        if self.blocked_queue.remove(pid) {
            self.ready_queue.enqueue(pid)
        } else {
            false
        }
    }

    /// Seleccionar el siguiente proceso a ejecutar
    pub fn select_next_process(
        &mut self,
        processes: &[Option<ProcessControlBlock>],
    ) -> Option<ProcessId> {
        match self.algorithm {
            SchedulingAlgorithm::RoundRobin => self.select_round_robin(),
            SchedulingAlgorithm::Priority => self.select_priority(processes),
            SchedulingAlgorithm::FCFS => self.select_fcfs(),
            SchedulingAlgorithm::SJF => self.select_sjf(processes),
            SchedulingAlgorithm::MLFQ => self.select_mlfq(processes),
        }
    }

    /// Selección Round Robin
    fn select_round_robin(&mut self) -> Option<ProcessId> {
        self.ready_queue.dequeue()
    }

    /// Selección por prioridad
    fn select_priority(&mut self, processes: &[Option<ProcessControlBlock>]) -> Option<ProcessId> {
        let mut best_pid = None;
        let mut best_priority = ProcessPriority::Background;

        // Buscar el proceso con mayor prioridad
        for i in 0..self.ready_queue.count {
            let index = (self.ready_queue.head + i) % 256;
            if let Some(pid) = self.ready_queue.processes[index] {
                if let Some(Some(pcb)) = processes.get(pid as usize) {
                    if pcb.priority < best_priority {
                        best_priority = pcb.priority;
                        best_pid = Some(pid);
                    }
                }
            }
        }

        // Remover el proceso seleccionado de la cola
        if let Some(pid) = best_pid {
            self.ready_queue.remove(pid);
        }

        best_pid
    }

    /// Selección FCFS (First Come First Served)
    fn select_fcfs(&mut self) -> Option<ProcessId> {
        self.ready_queue.dequeue()
    }

    /// Selección SJF (Shortest Job First)
    fn select_sjf(&mut self, processes: &[Option<ProcessControlBlock>]) -> Option<ProcessId> {
        let mut best_pid = None;
        let mut shortest_time = u64::MAX;

        // Buscar el proceso con menor tiempo de CPU
        for i in 0..self.ready_queue.count {
            let index = (self.ready_queue.head + i) % 256;
            if let Some(pid) = self.ready_queue.processes[index] {
                if let Some(Some(pcb)) = processes.get(pid as usize) {
                    if pcb.cpu_time < shortest_time {
                        shortest_time = pcb.cpu_time;
                        best_pid = Some(pid);
                    }
                }
            }
        }

        // Remover el proceso seleccionado de la cola
        if let Some(pid) = best_pid {
            self.ready_queue.remove(pid);
        }

        best_pid
    }

    /// Selección MLFQ (Multilevel Feedback Queue)
    fn select_mlfq(&mut self, processes: &[Option<ProcessControlBlock>]) -> Option<ProcessId> {
        // Implementación simplificada de MLFQ
        // En una implementación real, tendríamos múltiples colas con diferentes quantums
        self.select_priority(processes)
    }

    /// Realizar context switch
    pub fn context_switch(&mut self, new_pid: ProcessId) -> Option<ProcessId> {
        let old_pid = self.current_process;

        self.previous_process = old_pid;
        self.current_process = Some(new_pid);
        self.context_switches += 1;

        old_pid
    }

    /// Actualizar tiempo del sistema
    pub fn update_time(&mut self, delta_time: u64) {
        self.current_time += delta_time;
    }

    /// Verificar si es necesario hacer context switch
    pub fn should_switch(&self, processes: &[Option<ProcessControlBlock>]) -> bool {
        if let Some(current_pid) = self.current_process {
            if let Some(Some(pcb)) = processes.get(current_pid as usize) {
                // Verificar si el quantum se agotó
                if self.algorithm == SchedulingAlgorithm::RoundRobin {
                    return (self.current_time - pcb.last_run_time) >= self.quantum;
                }

                // Verificar si hay un proceso de mayor prioridad
                if self.algorithm == SchedulingAlgorithm::Priority {
                    for i in 0..self.ready_queue.count {
                        let index = (self.ready_queue.head + i) % 256;
                        if let Some(pid) = self.ready_queue.processes[index] {
                            if let Some(Some(ready_pcb)) = processes.get(pid as usize) {
                                if ready_pcb.priority < pcb.priority {
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
        }

        false
    }

    /// Obtener estadísticas del scheduler
    pub fn get_stats(&self) -> SchedulerStats {
        SchedulerStats {
            total_processes: self.process_count,
            ready_processes: self.ready_queue.count,
            blocked_processes: self.blocked_queue.count,
            current_process: self.current_process,
            context_switches: self.context_switches,
            current_time: self.current_time,
            algorithm: self.algorithm,
        }
    }

    /// Cambiar algoritmo de scheduling
    pub fn set_algorithm(&mut self, algorithm: SchedulingAlgorithm) {
        self.algorithm = algorithm;
    }

    /// Establecer quantum para Round Robin
    pub fn set_quantum(&mut self, quantum: u64) {
        self.quantum = quantum;
    }

    /// Obtener el proceso actual
    pub fn get_current_process(&self) -> Option<ProcessId> {
        self.current_process
    }

    /// Obtener el proceso anterior
    pub fn get_previous_process(&self) -> Option<ProcessId> {
        self.previous_process
    }
}

/// Estadísticas del scheduler
#[derive(Debug, Clone)]
pub struct SchedulerStats {
    pub total_processes: u32,
    pub ready_processes: usize,
    pub blocked_processes: usize,
    pub current_process: Option<ProcessId>,
    pub context_switches: u64,
    pub current_time: u64,
    pub algorithm: SchedulingAlgorithm,
}

/// Scheduler de threads
#[derive(Debug)]
pub struct ThreadScheduler {
    /// Cola de threads listos
    pub ready_threads: ProcessQueue,
    /// Thread actualmente ejecutándose
    pub current_thread: Option<ThreadId>,
    /// Quantum para threads
    pub quantum: u64,
    /// Tiempo actual del sistema
    pub current_time: u64,
    /// Contador de context switches
    pub context_switches: u64,
}

impl ThreadScheduler {
    /// Crear un nuevo scheduler de threads
    pub fn new() -> Self {
        Self {
            ready_threads: ProcessQueue::new(),
            current_thread: None,
            quantum: 50, // 50ms para threads
            current_time: 0,
            context_switches: 0,
        }
    }

    /// Agregar un thread al scheduler
    pub fn add_thread(&mut self, tid: ThreadId) -> bool {
        self.ready_threads.enqueue(tid)
    }

    /// Seleccionar el siguiente thread
    pub fn select_next_thread(&mut self) -> Option<ThreadId> {
        self.ready_threads.dequeue()
    }

    /// Realizar context switch de thread
    pub fn context_switch(&mut self, new_tid: ThreadId) -> Option<ThreadId> {
        let old_tid = self.current_thread;
        self.current_thread = Some(new_tid);
        self.context_switches += 1;
        old_tid
    }

    /// Actualizar tiempo del sistema
    pub fn update_time(&mut self, delta_time: u64) {
        self.current_time += delta_time;
    }

    /// Obtener estadísticas del scheduler de threads
    pub fn get_stats(&self) -> ThreadSchedulerStats {
        ThreadSchedulerStats {
            ready_threads: self.ready_threads.count,
            current_thread: self.current_thread,
            context_switches: self.context_switches,
            current_time: self.current_time,
        }
    }
}

/// Estadísticas del scheduler de threads
#[derive(Debug, Clone)]
pub struct ThreadSchedulerStats {
    pub ready_threads: usize,
    pub current_thread: Option<ThreadId>,
    pub context_switches: u64,
    pub current_time: u64,
}
