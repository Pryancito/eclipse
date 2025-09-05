//! Gestor de Procesos para Eclipse OS
//! 
//! Coordina procesos, threads y scheduling

use crate::process::process::{
    ProcessControlBlock, ProcessId, ProcessState, ProcessPriority, 
    ThreadInfo, ThreadId, get_next_tid
};
use crate::process::scheduler::{
    ProcessScheduler, ThreadScheduler, SchedulingAlgorithm, 
    SchedulerStats, ThreadSchedulerStats
};

/// Máximo número de procesos en el sistema
const MAX_PROCESSES: usize = 1024;

/// Máximo número de threads por proceso
const MAX_THREADS_PER_PROCESS: usize = 64;

/// Gestor principal de procesos
#[derive(Debug)]
pub struct ProcessManager {
    /// Tabla de procesos
    pub processes: [Option<ProcessControlBlock>; MAX_PROCESSES],
    /// Tabla de threads
    pub threads: [Option<ThreadInfo>; MAX_PROCESSES * MAX_THREADS_PER_PROCESS],
    /// Scheduler de procesos
    pub process_scheduler: ProcessScheduler,
    /// Scheduler de threads
    pub thread_scheduler: ThreadScheduler,
    /// Proceso actualmente ejecutándose
    pub current_process: Option<ProcessId>,
    /// Thread actualmente ejecutándose
    pub current_thread: Option<ThreadId>,
    /// Contador de procesos activos
    pub active_processes: u32,
    /// Contador de threads activos
    pub active_threads: u32,
    /// Tiempo del sistema
    pub system_time: u64,
}

impl ProcessManager {
    /// Crear un nuevo gestor de procesos
    pub fn new() -> Self {
        Self {
            processes: [(); MAX_PROCESSES].map(|_| None),
            threads: [(); MAX_PROCESSES * MAX_THREADS_PER_PROCESS].map(|_| None),
            process_scheduler: ProcessScheduler::new(SchedulingAlgorithm::RoundRobin),
            thread_scheduler: ThreadScheduler::new(),
            current_process: None,
            current_thread: None,
            active_processes: 0,
            active_threads: 0,
            system_time: 0,
        }
    }

    /// Inicializar el gestor de procesos
    pub fn init(&mut self) -> Result<(), &'static str> {
        // Crear el proceso kernel (PID 0)
        let kernel_pid = 0;
        let mut kernel_process = ProcessControlBlock::new(
            kernel_pid,
            "kernel",
            ProcessPriority::Critical
        );
        kernel_process.set_state(ProcessState::Running);
        kernel_process.creation_time = self.system_time;
        
        self.processes[kernel_pid as usize] = Some(kernel_process);
        self.current_process = Some(kernel_pid);
        self.active_processes = 1;

        // Agregar el proceso kernel al scheduler
        self.process_scheduler.add_process(kernel_pid);

        Ok(())
    }

    /// Crear un nuevo proceso
    pub fn create_process(&mut self, name: &str, priority: ProcessPriority) -> Result<ProcessId, &'static str> {
        // Buscar un slot libre
        for i in 1..MAX_PROCESSES {
            if self.processes[i].is_none() {
                let pid = i as ProcessId;
                let mut process = ProcessControlBlock::new(pid, name, priority);
                process.creation_time = self.system_time;
                process.set_state(ProcessState::Ready);

                self.processes[i] = Some(process);
                self.active_processes += 1;

                // Agregar al scheduler
                if self.process_scheduler.add_process(pid) {
                    return Ok(pid);
                } else {
                    // Si falla el scheduler, limpiar
                    self.processes[i] = None;
                    self.active_processes -= 1;
                    return Err("Failed to add process to scheduler");
                }
            }
        }

        Err("No free process slots available")
    }

    /// Terminar un proceso
    pub fn terminate_process(&mut self, pid: ProcessId) -> Result<(), &'static str> {
        if pid as usize >= MAX_PROCESSES {
            return Err("Invalid process ID");
        }

        if let Some(ref mut process) = self.processes[pid as usize] {
            process.terminate(0); // Código de salida 0
            process.set_state(ProcessState::Terminated);
            
            // Remover del scheduler
            self.process_scheduler.remove_process(pid);
            
            // Si es el proceso actual, limpiar
            if self.current_process == Some(pid) {
                self.current_process = None;
            }

            self.active_processes -= 1;
            Ok(())
        } else {
            Err("Process not found")
        }
    }

    /// Crear un nuevo thread
    pub fn create_thread(&mut self, pid: ProcessId, priority: ProcessPriority) -> Result<ThreadId, &'static str> {
        // Verificar que el proceso existe
        if pid as usize >= MAX_PROCESSES || self.processes[pid as usize].is_none() {
            return Err("Parent process not found");
        }

        // Buscar un slot libre para el thread
        let start_index = (pid as usize) * MAX_THREADS_PER_PROCESS;
        for i in start_index..start_index + MAX_THREADS_PER_PROCESS {
            if self.threads[i].is_none() {
                let tid = get_next_tid();
                let mut thread = ThreadInfo::new(tid, pid, priority);
                thread.creation_time = self.system_time;
                thread.set_state(ProcessState::Ready);

                self.threads[i] = Some(thread);
                self.active_threads += 1;

                // Agregar al scheduler de threads
                if self.thread_scheduler.add_thread(tid) {
                    return Ok(tid);
                } else {
                    // Si falla el scheduler, limpiar
                    self.threads[i] = None;
                    self.active_threads -= 1;
                    return Err("Failed to add thread to scheduler");
                }
            }
        }

        Err("No free thread slots available")
    }

    /// Terminar un thread
    pub fn terminate_thread(&mut self, tid: ThreadId) -> Result<(), &'static str> {
        // Buscar el thread
        for i in 0..MAX_PROCESSES * MAX_THREADS_PER_PROCESS {
            if let Some(ref mut thread) = self.threads[i] {
                if thread.tid == tid {
                    thread.set_state(ProcessState::Terminated);
                    
                    // Remover del scheduler
                    // (En una implementación real, necesitaríamos una función remove_thread)
                    
                    // Si es el thread actual, limpiar
                    if self.current_thread == Some(tid) {
                        self.current_thread = None;
                    }

                    self.active_threads -= 1;
                    return Ok(());
                }
            }
        }

        Err("Thread not found")
    }

    /// Ejecutar el scheduler
    pub fn schedule(&mut self) -> Option<ProcessId> {
        // Seleccionar el siguiente proceso
        if let Some(next_pid) = self.process_scheduler.select_next_process(&self.processes) {
            // Realizar context switch
            let old_pid = self.process_scheduler.context_switch(next_pid);
            
            // Actualizar estado del proceso anterior
            if let Some(old_pid) = old_pid {
                if let Some(ref mut process) = self.processes[old_pid as usize] {
                    process.set_state(ProcessState::Ready);
                    // Agregar de vuelta a la cola de listos
                    self.process_scheduler.add_process(old_pid);
                }
            }

            // Actualizar estado del nuevo proceso
            if let Some(ref mut process) = self.processes[next_pid as usize] {
                process.set_state(ProcessState::Running);
            }

            self.current_process = Some(next_pid);
            Some(next_pid)
        } else {
            None
        }
    }

    /// Ejecutar el scheduler de threads
    pub fn schedule_thread(&mut self) -> Option<ThreadId> {
        // Seleccionar el siguiente thread
        if let Some(next_tid) = self.thread_scheduler.select_next_thread() {
            // Realizar context switch
            let old_tid = self.thread_scheduler.context_switch(next_tid);
            
            // Actualizar estado del thread anterior
            if let Some(old_tid) = old_tid {
                if let Some(ref mut thread) = self.find_thread_mut(old_tid) {
                    thread.set_state(ProcessState::Ready);
                    // Agregar de vuelta a la cola de listos
                    self.thread_scheduler.add_thread(old_tid);
                }
            }

            // Actualizar estado del nuevo thread
            if let Some(ref mut thread) = self.find_thread_mut(next_tid) {
                thread.set_state(ProcessState::Running);
            }

            self.current_thread = Some(next_tid);
            Some(next_tid)
        } else {
            None
        }
    }

    /// Buscar un thread por ID (simplificado)
    fn find_thread_mut(&mut self, _tid: ThreadId) -> Option<&mut ThreadInfo> {
        // Implementación simplificada - no hace nada por ahora
        None
    }

    /// Obtener información de un proceso
    pub fn get_process(&self, pid: ProcessId) -> Option<&ProcessControlBlock> {
        if (pid as usize) < MAX_PROCESSES {
            self.processes[pid as usize].as_ref()
        } else {
            None
        }
    }

    /// Obtener información mutable de un proceso
    pub fn get_process_mut(&mut self, pid: ProcessId) -> Option<&mut ProcessControlBlock> {
        if (pid as usize) < MAX_PROCESSES {
            self.processes[pid as usize].as_mut()
        } else {
            None
        }
    }

    /// Obtener información de un thread (simplificado)
    pub fn get_thread(&self, _tid: ThreadId) -> Option<&ThreadInfo> {
        // Implementación simplificada - no hace nada por ahora
        None
    }

    /// Obtener información mutable de un thread (simplificado)
    pub fn get_thread_mut(&mut self, _tid: ThreadId) -> Option<&mut ThreadInfo> {
        // Implementación simplificada - no hace nada por ahora
        None
    }

    /// Bloquear el proceso actual
    pub fn block_current_process(&mut self) -> Option<ProcessId> {
        if let Some(pid) = self.current_process {
            if let Some(ref mut process) = self.processes[pid as usize] {
                process.set_state(ProcessState::Blocked);
            }
            self.process_scheduler.block_current_process();
            self.current_process = None;
            Some(pid)
        } else {
            None
        }
    }

    /// Desbloquear un proceso
    pub fn unblock_process(&mut self, pid: ProcessId) -> bool {
        if let Some(ref mut process) = self.processes[pid as usize] {
            process.set_state(ProcessState::Ready);
            self.process_scheduler.unblock_process(pid)
        } else {
            false
        }
    }

    /// Actualizar tiempo del sistema
    pub fn update_time(&mut self, delta_time: u64) {
        self.system_time += delta_time;
        self.process_scheduler.update_time(delta_time);
        self.thread_scheduler.update_time(delta_time);
    }

    /// Obtener estadísticas del gestor de procesos
    pub fn get_stats(&self) -> ProcessManagerStats {
        ProcessManagerStats {
            total_processes: self.active_processes,
            total_threads: self.active_threads,
            current_process: self.current_process,
            current_thread: self.current_thread,
            system_time: self.system_time,
            process_scheduler_stats: self.process_scheduler.get_stats(),
            thread_scheduler_stats: self.thread_scheduler.get_stats(),
        }
    }

    /// Cambiar algoritmo de scheduling
    pub fn set_scheduling_algorithm(&mut self, algorithm: SchedulingAlgorithm) {
        self.process_scheduler.set_algorithm(algorithm);
    }

    /// Establecer quantum del scheduler
    pub fn set_quantum(&mut self, quantum: u64) {
        self.process_scheduler.set_quantum(quantum);
    }

    /// Obtener el proceso actual
    pub fn get_current_process(&self) -> Option<ProcessId> {
        self.current_process
    }

    /// Obtener el thread actual
    pub fn get_current_thread(&self) -> Option<ThreadId> {
        self.current_thread
    }
}

/// Estadísticas del gestor de procesos
#[derive(Debug, Clone)]
pub struct ProcessManagerStats {
    pub total_processes: u32,
    pub total_threads: u32,
    pub current_process: Option<ProcessId>,
    pub current_thread: Option<ThreadId>,
    pub system_time: u64,
    pub process_scheduler_stats: SchedulerStats,
    pub thread_scheduler_stats: ThreadSchedulerStats,
}

/// Instancia global del gestor de procesos
static mut PROCESS_MANAGER: Option<ProcessManager> = None;

/// Inicializar el gestor de procesos global
pub fn init_process_manager() -> Result<(), &'static str> {
    unsafe {
        PROCESS_MANAGER = Some(ProcessManager::new());
        if let Some(ref mut manager) = PROCESS_MANAGER {
            manager.init()?;
        }
    }
    Ok(())
}

/// Obtener el gestor de procesos global
pub fn get_process_manager() -> Option<&'static mut ProcessManager> {
    unsafe { PROCESS_MANAGER.as_mut() }
}
