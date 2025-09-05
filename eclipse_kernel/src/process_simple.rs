//! Planificador de procesos simplificado

/// Estado de un proceso
#[derive(Debug, Clone, Copy)]
pub enum ProcessState {
    Ready,
    Running,
    Blocked,
    Terminated,
}

/// Estructura de un proceso
#[derive(Clone, Copy)]
pub struct Process {
    pub id: u32,
    pub name: [u8; 32],
    pub state: ProcessState,
    pub priority: u8,
    pub cpu_time: u64,
}

impl Process {
    /// Crear un nuevo proceso
    pub fn new(id: u32, name: &str) -> Self {
        let mut process_name = [0u8; 32];
        let name_bytes = name.as_bytes();
        let copy_len = core::cmp::min(name_bytes.len(), 31);
        process_name[..copy_len].copy_from_slice(&name_bytes[..copy_len]);
        
        Self {
            id,
            name: process_name,
            state: ProcessState::Ready,
            priority: 5,
            cpu_time: 0,
        }
    }
}

/// Planificador de procesos
pub struct ProcessScheduler {
    pub processes: [Option<Process>; 64],
    pub current_process: Option<u32>,
    pub next_process_id: u32,
}

impl ProcessScheduler {
    /// Crear un nuevo planificador
    pub const fn new() -> Self {
        Self {
            processes: [None; 64],
            current_process: None,
            next_process_id: 1,
        }
    }
    
    /// Crear un nuevo proceso
    pub fn create_process(&mut self, name: &str) -> Option<u32> {
        for i in 0..64 {
            if self.processes[i].is_none() {
                let process = Process::new(self.next_process_id, name);
                let process_id = process.id;
                self.processes[i] = Some(process);
                self.next_process_id += 1;
                return Some(process_id);
            }
        }
        None
    }
    
    /// Obtener el siguiente proceso a ejecutar
    pub fn get_next_process(&mut self) -> Option<u32> {
        // Algoritmo simple: round-robin
        for i in 0..64 {
            if let Some(process) = &mut self.processes[i] {
                if matches!(process.state, ProcessState::Ready) {
                    process.state = ProcessState::Running;
                    self.current_process = Some(process.id);
                    return Some(process.id);
                }
            }
        }
        None
    }
    
    /// Obtener estadísticas del planificador
    pub fn get_stats(&self) -> (u32, u32, u32) {
        let mut total = 0;
        let mut ready = 0;
        let mut running = 0;
        
        for process in &self.processes {
            if let Some(proc) = process {
                total += 1;
                match proc.state {
                    ProcessState::Ready => ready += 1,
                    ProcessState::Running => running += 1,
                    _ => {}
                }
            }
        }
        
        (total, ready, running)
    }
}

/// Planificador global
static mut PROCESS_SCHEDULER: ProcessScheduler = ProcessScheduler::new();

/// Inicializar el planificador de procesos
pub fn init_process_scheduler() {
    unsafe {
        // Crear proceso del sistema
        PROCESS_SCHEDULER.create_process("System");
        PROCESS_SCHEDULER.create_process("Idle");
    }
}

/// Obtener estadísticas del planificador
pub fn get_scheduler_stats() -> (u32, u32, u32) {
    unsafe {
        PROCESS_SCHEDULER.get_stats()
    }
}
