//! Gestor de Procesos para Eclipse OS
//!
//! Coordina procesos, threads y scheduling

use crate::process::process::{
    get_next_tid, ProcessControlBlock, ProcessId, ProcessPriority, ProcessState, ThreadId,
    ThreadInfo,
};
use crate::process::scheduler::{
    ProcessScheduler, SchedulerStats, SchedulingAlgorithm, ThreadScheduler, ThreadSchedulerStats,
};
use crate::process::{MAX_PROCESSES, MAX_THREADS_PER_PROCESS};

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
        let mut kernel_process =
            ProcessControlBlock::new(kernel_pid, "kernel", ProcessPriority::Critical);
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
    pub fn create_process(
        &mut self,
        name: &str,
        priority: ProcessPriority,
    ) -> Result<ProcessId, &'static str> {
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
        use crate::process::stack_allocator::deallocate_process_stack;
        use crate::debug::serial_write_str;
        
        if pid as usize >= MAX_PROCESSES {
            return Err("Invalid process ID");
        }

        if let Some(ref mut process) = self.processes[pid as usize] {
            serial_write_str(&alloc::format!("TERMINATE: Terminando proceso {}\n", pid));
            
            process.terminate(0); // Código de salida 0
            process.set_state(ProcessState::Terminated);

            // Liberar stack del proceso si existe
            if process.stack_info.is_some() {
                match deallocate_process_stack(pid) {
                    Ok(_) => {
                        serial_write_str(&alloc::format!("TERMINATE: Stack del proceso {} liberado\n", pid));
                    }
                    Err(e) => {
                        serial_write_str(&alloc::format!("TERMINATE: Error liberando stack: {}\n", e));
                    }
                }
            }

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

    /// Fork - crear proceso hijo (copia del proceso actual)
    pub fn fork_process(&mut self, parent_pid: ProcessId) -> Result<ProcessId, &'static str> {
        use crate::process::stack_allocator::allocate_process_stack;
        use crate::debug::serial_write_str;
        
        if parent_pid as usize >= MAX_PROCESSES {
            return Err("Invalid parent PID");
        }

        // Obtener el proceso padre
        let parent_process = self.processes[parent_pid as usize]
            .as_ref()
            .ok_or("Parent process not found")?
            .clone();

        // Buscar un slot libre para el hijo
        for i in 1..MAX_PROCESSES {
            if self.processes[i].is_none() {
                let child_pid = i as ProcessId;
                
                serial_write_str(&alloc::format!("FORK: Creando proceso hijo {} (padre: {})\n", child_pid, parent_pid));
                
                // Asignar stack NUEVO para el hijo
                let child_stack = match allocate_process_stack(child_pid) {
                    Ok(stack) => stack,
                    Err(e) => {
                        serial_write_str(&alloc::format!("FORK: Error asignando stack: {}\n", e));
                        return Err(e);
                    }
                };
                
                serial_write_str(&alloc::format!(
                    "FORK: Stack asignado para hijo {} -> 0x{:016x}\n",
                    child_pid, child_stack.top
                ));
                
                // Crear proceso hijo como copia del padre
                let mut child_process = parent_process.clone();
                child_process.pid = child_pid;
                child_process.parent_pid = Some(parent_pid);
                child_process.state = ProcessState::Ready; // El hijo empieza Ready
                child_process.creation_time = self.system_time;
                child_process.cpu_time = 0; // El hijo empieza con tiempo 0
                
                // IMPORTANTE: Configurar RAX = 0 en el hijo
                // Esto hace que fork() retorne 0 al proceso hijo
                child_process.cpu_context.rax = 0;
                
                // CRÍTICO: Configurar stack NUEVO para el hijo
                child_process.cpu_context.rsp = child_stack.top;
                child_process.cpu_context.rbp = child_stack.top;
                child_process.stack_info = Some(child_stack);
                
                serial_write_str(&alloc::format!(
                    "FORK: Hijo {} configurado con RSP=0x{:016x}\n",
                    child_pid, child_stack.top
                ));
                
                // El hijo hereda la tabla de file descriptors (copia)
                // En un sistema real, aquí se marcaría el espacio de memoria como copy-on-write
                
                self.processes[i] = Some(child_process);
                self.active_processes += 1;

                // Agregar al scheduler
                if self.process_scheduler.add_process(child_pid) {
                    serial_write_str(&alloc::format!("FORK: Proceso hijo {} creado exitosamente\n", child_pid));
                    return Ok(child_pid);
                } else {
                    // Si falla el scheduler, limpiar y liberar stack
                    use crate::process::stack_allocator::deallocate_process_stack;
                    let _ = deallocate_process_stack(child_pid);
                    self.processes[i] = None;
                    self.active_processes -= 1;
                    return Err("Failed to add child process to scheduler");
                }
            }
        }

        Err("No free process slots available")
    }

    /// Crear un nuevo thread
    pub fn create_thread(
        &mut self,
        pid: ProcessId,
        priority: ProcessPriority,
    ) -> Result<ThreadId, &'static str> {
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
use spin::Mutex;

static PROCESS_MANAGER: Mutex<Option<ProcessManager>> = Mutex::new(None);

/// Inicializar el gestor de procesos global
pub fn init_process_manager() -> Result<(), &'static str> {
    let mut manager_guard = PROCESS_MANAGER.lock();
    let mut manager = ProcessManager::new();
    manager.init()?;
    *manager_guard = Some(manager);
    Ok(())
}

/// Obtener el gestor de procesos global
pub fn get_process_manager() -> &'static Mutex<Option<ProcessManager>> {
    &PROCESS_MANAGER
}
