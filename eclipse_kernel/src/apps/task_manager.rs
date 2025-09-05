//! Gestor de tareas para Eclipse OS
//! 
//! Proporciona información y control sobre procesos del sistema.

use alloc::{vec, vec::Vec};
use alloc::string::{String, ToString};
use alloc::format;

/// Información de un proceso
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub state: ProcessState,
    pub cpu_usage: f32,
    pub memory_usage: u64,
    pub priority: i32,
    pub user: String,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProcessState {
    Running,
    Sleeping,
    Stopped,
    Zombie,
    Dead,
}

/// Gestor de tareas
pub struct TaskManager {
    processes: Vec<ProcessInfo>,
}

impl TaskManager {
    pub fn new() -> Self {
        Self {
            processes: vec![
                ProcessInfo {
                    pid: 1,
                    name: "kernel".to_string(),
                    state: ProcessState::Running,
                    cpu_usage: 0.1,
                    memory_usage: 1024,
                    priority: 0,
                    user: "root".to_string(),
                    command: "/boot/kernel".to_string(),
                },
                ProcessInfo {
                    pid: 2,
                    name: "shell".to_string(),
                    state: ProcessState::Running,
                    cpu_usage: 0.5,
                    memory_usage: 2048,
                    priority: 0,
                    user: "usuario".to_string(),
                    command: "/bin/shell".to_string(),
                },
                ProcessInfo {
                    pid: 3,
                    name: "file_manager".to_string(),
                    state: ProcessState::Sleeping,
                    cpu_usage: 0.0,
                    memory_usage: 1536,
                    priority: 0,
                    user: "usuario".to_string(),
                    command: "/bin/file_manager".to_string(),
                },
            ],
        }
    }

    pub fn run(&mut self) -> Result<(), &'static str> {
        self.show_welcome();
        self.show_processes();
        self.show_help();
        Ok(())
    }

    fn show_welcome(&self) {
        self.print_info("╔══════════════════════════════════════════════════════════════╗");
        self.print_info("║                                                              ║");
        self.print_info("║                    ECLIPSE TASK MANAGER                      ║");
        self.print_info("║                                                              ║");
        self.print_info("║  Gestor de tareas y procesos del sistema                   ║");
        self.print_info("║                                                              ║");
        self.print_info("╚══════════════════════════════════════════════════════════════╝");
        self.print_info("");
    }

    fn show_processes(&self) {
        self.print_info("═══════════════════════════════════════════════════════════════");
        self.print_info("                          PROCESOS");
        self.print_info("═══════════════════════════════════════════════════════════════");
        self.print_info("PID    Nombre           Estado    CPU%   Memoria  Prioridad  Usuario");
        self.print_info("─────  ───────────────  ────────  ─────  ───────  ─────────  ───────");
        
        for process in &self.processes {
            let state_str = match process.state {
                ProcessState::Running => "Running",
                ProcessState::Sleeping => "Sleeping",
                ProcessState::Stopped => "Stopped",
                ProcessState::Zombie => "Zombie",
                ProcessState::Dead => "Dead",
            };
            
            self.print_info(&format!(
                "{:<5}  {:<15}  {:<8}  {:<5.1}  {:<6}  {:<9}  {}",
                process.pid,
                process.name,
                state_str,
                process.cpu_usage,
                process.memory_usage,
                process.priority,
                process.user
            ));
        }
        self.print_info("");
    }

    fn show_help(&self) {
        self.print_info("Comandos disponibles:");
        self.print_info("  list            - Lista todos los procesos");
        self.print_info("  kill <pid>      - Termina un proceso");
        self.print_info("  suspend <pid>   - Suspende un proceso");
        self.print_info("  resume <pid>    - Reanuda un proceso");
        self.print_info("  priority <pid> <priority> - Cambia prioridad");
        self.print_info("  info <pid>      - Muestra información detallada");
        self.print_info("  refresh         - Actualiza la lista");
        self.print_info("  quit            - Sale del gestor");
    }

    fn print_info(&self, text: &str) {
        // En una implementación real, esto imprimiría en la consola
        // Por ahora solo simulamos
    }
}

/// Función principal para ejecutar el gestor de tareas
pub fn run() -> Result<(), &'static str> {
    let mut task_manager = TaskManager::new();
    task_manager.run()
}
