//! Módulo de Gestión de Procesos para Eclipse OS
//! 
//! Este módulo proporciona todas las funcionalidades de gestión de procesos:
//! - Estructura de procesos (PCB)
//! - Scheduler con múltiples algoritmos
//! - Gestión de threads
//! - Context switching
//! - Estadísticas del sistema

pub mod process;
pub mod scheduler;
pub mod manager;

// Re-exportar las estructuras principales
pub use process::{
    ProcessId, ProcessState, ProcessPriority, ThreadId
};
pub use scheduler::SchedulingAlgorithm;
pub use manager::{
    init_process_manager, get_process_manager
};

/// Constantes del sistema de procesos
pub const MAX_PROCESSES: usize = 1024;
pub const MAX_THREADS_PER_PROCESS: usize = 64;
pub const DEFAULT_QUANTUM: u64 = 100; // 100ms
pub const THREAD_QUANTUM: u64 = 50; // 50ms

/// Inicializar el sistema de procesos completo
pub fn init_process_system() -> Result<(), &'static str> {
    // Inicializar el gestor de procesos
    init_process_manager()?;
    
    Ok(())
}

/// Obtener información del sistema de procesos
pub fn get_process_system_info() -> ProcessSystemInfo {
    if let Some(manager) = get_process_manager() {
        let stats = manager.get_stats();
        ProcessSystemInfo {
            total_processes: stats.total_processes,
            total_threads: stats.total_threads,
            current_process: stats.current_process,
            current_thread: stats.current_thread,
            system_time: stats.system_time,
            scheduler_algorithm: stats.process_scheduler_stats.algorithm,
            context_switches: stats.process_scheduler_stats.context_switches,
        }
    } else {
        ProcessSystemInfo {
            total_processes: 0,
            total_threads: 0,
            current_process: None,
            current_thread: None,
            system_time: 0,
            scheduler_algorithm: SchedulingAlgorithm::RoundRobin,
            context_switches: 0,
        }
    }
}

/// Información del sistema de procesos
#[derive(Debug, Clone)]
pub struct ProcessSystemInfo {
    pub total_processes: u32,
    pub total_threads: u32,
    pub current_process: Option<ProcessId>,
    pub current_thread: Option<ThreadId>,
    pub system_time: u64,
    pub scheduler_algorithm: SchedulingAlgorithm,
    pub context_switches: u64,
}

/// Funciones de utilidad para procesos
pub mod utils {
    use super::*;

    /// Verificar si un PID es válido
    pub fn is_valid_pid(pid: ProcessId) -> bool {
        pid < MAX_PROCESSES as ProcessId
    }

    /// Verificar si un TID es válido
    pub fn is_valid_tid(tid: ThreadId) -> bool {
        tid > 0 && tid < (MAX_PROCESSES * MAX_THREADS_PER_PROCESS) as ThreadId
    }

    /// Convertir prioridad a valor numérico
    pub fn priority_to_value(priority: ProcessPriority) -> u8 {
        priority as u8
    }

    /// Convertir valor numérico a prioridad
    pub fn value_to_priority(value: u8) -> ProcessPriority {
        match value {
            0 => ProcessPriority::Critical,
            1 => ProcessPriority::High,
            2 => ProcessPriority::Normal,
            3 => ProcessPriority::Low,
            4 => ProcessPriority::Background,
            _ => ProcessPriority::Normal,
        }
    }

    /// Obtener nombre del estado del proceso
    pub fn state_to_string(state: ProcessState) -> &'static str {
        match state {
            ProcessState::New => "New",
            ProcessState::Ready => "Ready",
            ProcessState::Running => "Running",
            ProcessState::Blocked => "Blocked",
            ProcessState::Terminated => "Terminated",
            ProcessState::Zombie => "Zombie",
        }
    }

    /// Obtener nombre de la prioridad
    pub fn priority_to_string(priority: ProcessPriority) -> &'static str {
        match priority {
            ProcessPriority::Critical => "Critical",
            ProcessPriority::High => "High",
            ProcessPriority::Normal => "Normal",
            ProcessPriority::Low => "Low",
            ProcessPriority::Background => "Background",
        }
    }

    /// Obtener nombre del algoritmo de scheduling
    pub fn algorithm_to_string(algorithm: SchedulingAlgorithm) -> &'static str {
        match algorithm {
            SchedulingAlgorithm::RoundRobin => "Round Robin",
            SchedulingAlgorithm::Priority => "Priority",
            SchedulingAlgorithm::FCFS => "First Come First Served",
            SchedulingAlgorithm::SJF => "Shortest Job First",
            SchedulingAlgorithm::MLFQ => "Multilevel Feedback Queue",
        }
    }
}
