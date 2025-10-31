//! Syscalls relacionadas con procesos
//! 
//! Este módulo implementa las syscalls para gestión de procesos, incluyendo
//! creación, terminación, espera y ejecución de procesos.

use crate::debug::serial_write_str;
use super::{SyscallArgs, SyscallResult, SyscallError};
use super::types::*;
use core::str::FromStr;

/// Gestor de procesos
pub struct ProcessManager {
    /// Tabla de procesos
    pub processes: [Option<Process>; 256],
    /// PID del proceso actual
    pub current_pid: ProcessId,
    /// Contador de PIDs
    pub next_pid: ProcessId,
}

/// Información de proceso
#[derive(Debug, Clone)]
pub struct Process {
    pub pid: ProcessId,
    pub ppid: ProcessId,
    pub uid: UserId,
    pub gid: GroupId,
    pub euid: UserId,
    pub egid: GroupId,
    pub state: ProcessState,
    pub exit_code: i32,
    pub priority: i32,
    pub cpu_time: u64,
    pub memory_usage: u64,
}

/// Estado del proceso
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProcessState {
    Running,
    Sleeping,
    Stopped,
    Zombie,
    Dead,
}

impl ProcessManager {
    /// Crear nuevo gestor de procesos
    pub fn new() -> Self {
        let mut manager = Self {
            processes: [const { None }; 256],
            current_pid: 1,
            next_pid: 2,
        };

        // Crear proceso inicial (init)
        let init_process = Process {
            pid: 1,
            ppid: 0,
            uid: 0,
            gid: 0,
            euid: 0,
            egid: 0,
            state: ProcessState::Running,
            exit_code: 0,
            priority: 0,
            cpu_time: 0,
            memory_usage: 0,
        };

        manager.processes[1] = Some(init_process);
        manager
    }

    /// Crear proceso hijo (fork)
    pub fn fork(&mut self) -> SyscallResult {
        serial_write_str(&alloc::format!("PROCESS_SYSCALL: fork() desde PID {}\n", self.current_pid));

        // Buscar slot libre
        let new_pid = self.next_pid;
        if new_pid >= 256 {
            return SyscallResult::Error(SyscallError::OutOfMemory);
        }

        // Obtener proceso padre
        let parent = self.processes[self.current_pid as usize].clone()
            .ok_or(SyscallError::InvalidOperation);
        
        let parent = match parent {
            Ok(p) => p,
            Err(e) => return SyscallResult::Error(e),
        };

        // Crear proceso hijo
        let mut child = parent;
        child.pid = new_pid;
        child.ppid = self.current_pid;
        child.state = ProcessState::Running;
        child.cpu_time = 0;
        child.memory_usage = 0;

        // Agregar a la tabla
        self.processes[new_pid as usize] = Some(child);
        self.next_pid += 1;

        serial_write_str(&alloc::format!("PROCESS_SYSCALL: Proceso hijo creado con PID {}\n", new_pid));

        // Retornar 0 al hijo, PID del hijo al padre
        if self.current_pid == 1 {
            // Simular que somos el hijo
            self.current_pid = new_pid;
            SyscallResult::Success(0)
        } else {
            // Simular que somos el padre
            SyscallResult::Success(new_pid as u64)
        }
    }

    /// Ejecutar programa (execve)
    pub fn execve(&mut self, path: &str, argv: &[&str], envp: &[&str]) -> SyscallResult {
        serial_write_str(&alloc::format!("PROCESS_SYSCALL: execve '{}' desde PID {}\n", path, self.current_pid));

        // Obtener proceso actual
        if let Some(_process) = &mut self.processes[self.current_pid as usize] {
            // TODO: Implementar ejecución real de programas
            serial_write_str(&alloc::format!("PROCESS_SYSCALL: Programa '{}' ejecutado (simulado)\n", path));
            SyscallResult::Success(0)
        } else {
            SyscallResult::Error(SyscallError::InvalidOperation)
        }
    }

    /// Terminar proceso (exit)
    pub fn exit(&mut self, exit_code: i32) -> SyscallResult {
        serial_write_str(&alloc::format!("PROCESS_SYSCALL: exit({}) desde PID {}\n", exit_code, self.current_pid));

        // Obtener proceso actual
        if let Some(process) = &mut self.processes[self.current_pid as usize] {
            process.state = ProcessState::Zombie;
            process.exit_code = exit_code;

            // Notificar al proceso padre
            let ppid = process.ppid;
            if ppid > 0 && ppid < 256 {
                if let Some(parent) = &mut self.processes[ppid as usize] {
                    // TODO: Implementar notificación real al padre
                    serial_write_str(&alloc::format!("PROCESS_SYSCALL: Proceso padre {} notificado\n", ppid));
                }
            }

            serial_write_str(&alloc::format!("PROCESS_SYSCALL: Proceso {} terminado\n", self.current_pid));
            SyscallResult::Success(0)
        } else {
            SyscallResult::Error(SyscallError::InvalidOperation)
        }
    }

    /// Esperar cambio de estado de proceso (wait4)
    pub fn wait4(&mut self, pid: ProcessId, wstatus: *mut i32, options: i32, rusage: *mut u8) -> SyscallResult {
        serial_write_str(&alloc::format!("PROCESS_SYSCALL: wait4(pid={}, options={}) desde PID {}\n", 
                                        pid, options, self.current_pid));

        // Buscar procesos hijos zombie
        for i in 0..256 {
            if let Some(process) = &self.processes[i] {
                if process.ppid == self.current_pid && process.state == ProcessState::Zombie {
                    // Proceso hijo zombie encontrado
                    let child_pid = process.pid;
                    let exit_code = process.exit_code;

                    // Escribir status si se proporciona
                    if !wstatus.is_null() {
                        unsafe {
                            *wstatus = exit_code;
                        }
                    }

                    // Limpiar proceso zombie
                    self.processes[i] = None;

                    serial_write_str(&alloc::format!("PROCESS_SYSCALL: Proceso hijo {} recogido, exit_code={}\n", 
                                                    child_pid, exit_code));
                    return SyscallResult::Success(child_pid as u64);
                }
            }
        }

        // No hay procesos hijos zombie
        if (options & 0x1) != 0 { // WNOHANG
            serial_write_str("PROCESS_SYSCALL: No hay procesos hijos zombie (WNOHANG)\n");
            SyscallResult::Success(0)
        } else {
            // TODO: Implementar espera bloqueante
            serial_write_str("PROCESS_SYSCALL: Espera bloqueante no implementada\n");
            SyscallResult::Error(SyscallError::NotImplemented)
        }
    }

    /// Obtener PID del proceso actual
    pub fn getpid(&self) -> ProcessId {
        self.current_pid
    }

    /// Obtener PID del proceso padre
    pub fn getppid(&self) -> ProcessId {
        if let Some(process) = &self.processes[self.current_pid as usize] {
            process.ppid
        } else {
            0
        }
    }

    /// Obtener UID del proceso actual
    pub fn getuid(&self) -> UserId {
        if let Some(process) = &self.processes[self.current_pid as usize] {
            process.uid
        } else {
            0
        }
    }

    /// Obtener GID del proceso actual
    pub fn getgid(&self) -> GroupId {
        if let Some(process) = &self.processes[self.current_pid as usize] {
            process.gid
        } else {
            0
        }
    }

    /// Obtener UID efectivo del proceso actual
    pub fn geteuid(&self) -> UserId {
        if let Some(process) = &self.processes[self.current_pid as usize] {
            process.euid
        } else {
            0
        }
    }

    /// Obtener GID efectivo del proceso actual
    pub fn getegid(&self) -> GroupId {
        if let Some(process) = &self.processes[self.current_pid as usize] {
            process.egid
        } else {
            0
        }
    }

    /// Establecer UID del proceso actual
    pub fn setuid(&mut self, uid: UserId) -> SyscallResult {
        serial_write_str(&alloc::format!("PROCESS_SYSCALL: setuid({}) desde PID {}\n", uid, self.current_pid));

        if let Some(process) = &mut self.processes[self.current_pid as usize] {
            process.uid = uid;
            process.euid = uid;
            serial_write_str(&alloc::format!("PROCESS_SYSCALL: UID establecido a {}\n", uid));
            SyscallResult::Success(0)
        } else {
            SyscallResult::Error(SyscallError::InvalidOperation)
        }
    }

    /// Establecer GID del proceso actual
    pub fn setgid(&mut self, gid: GroupId) -> SyscallResult {
        serial_write_str(&alloc::format!("PROCESS_SYSCALL: setgid({}) desde PID {}\n", gid, self.current_pid));

        if let Some(process) = &mut self.processes[self.current_pid as usize] {
            process.gid = gid;
            process.egid = gid;
            serial_write_str(&alloc::format!("PROCESS_SYSCALL: GID establecido a {}\n", gid));
            SyscallResult::Success(0)
        } else {
            SyscallResult::Error(SyscallError::InvalidOperation)
        }
    }

    /// Establecer UID real y efectivo
    pub fn setreuid(&mut self, ruid: UserId, euid: UserId) -> SyscallResult {
        serial_write_str(&alloc::format!("PROCESS_SYSCALL: setreuid({}, {}) desde PID {}\n", 
                                        ruid, euid, self.current_pid));

        if let Some(process) = &mut self.processes[self.current_pid as usize] {
            process.uid = ruid;
            process.euid = euid;
            serial_write_str(&alloc::format!("PROCESS_SYSCALL: UID real={}, efectivo={}\n", ruid, euid));
            SyscallResult::Success(0)
        } else {
            SyscallResult::Error(SyscallError::InvalidOperation)
        }
    }

    /// Establecer GID real y efectivo
    pub fn setregid(&mut self, rgid: GroupId, egid: GroupId) -> SyscallResult {
        serial_write_str(&alloc::format!("PROCESS_SYSCALL: setregid({}, {}) desde PID {}\n", 
                                        rgid, egid, self.current_pid));

        if let Some(process) = &mut self.processes[self.current_pid as usize] {
            process.gid = rgid;
            process.egid = egid;
            serial_write_str(&alloc::format!("PROCESS_SYSCALL: GID real={}, efectivo={}\n", rgid, egid));
            SyscallResult::Success(0)
        } else {
            SyscallResult::Error(SyscallError::InvalidOperation)
        }
    }

    /// Enviar señal a proceso
    pub fn kill(&mut self, pid: ProcessId, sig: i32) -> SyscallResult {
        serial_write_str(&alloc::format!("PROCESS_SYSCALL: kill(pid={}, sig={}) desde PID {}\n", 
                                        pid, sig, self.current_pid));

        if pid < 0 || pid >= 256 {
            return SyscallResult::Error(SyscallError::InvalidArgument);
        }

        if let Some(process) = &mut self.processes[pid as usize] {
            match sig {
                SIGTERM => {
                    process.state = ProcessState::Dead;
                    serial_write_str(&alloc::format!("PROCESS_SYSCALL: Proceso {} terminado con SIGTERM\n", pid));
                }
                SIGKILL => {
                    process.state = ProcessState::Dead;
                    serial_write_str(&alloc::format!("PROCESS_SYSCALL: Proceso {} terminado con SIGKILL\n", pid));
                }
                _ => {
                    serial_write_str(&alloc::format!("PROCESS_SYSCALL: Señal {} enviada a proceso {}\n", sig, pid));
                }
            }
            SyscallResult::Success(0)
        } else {
            SyscallResult::Error(SyscallError::InvalidArgument)
        }
    }

    /// Cambiar directorio de trabajo
    pub fn chdir(&mut self, path: &str) -> SyscallResult {
        serial_write_str(&alloc::format!("PROCESS_SYSCALL: chdir('{}') desde PID {}\n", path, self.current_pid));

        if let Some(_process) = &mut self.processes[self.current_pid as usize] {
            // TODO: Implementar cambio de directorio real
            serial_write_str(&alloc::format!("PROCESS_SYSCALL: Directorio de trabajo cambiado a '{}' (simulado)\n", path));
            SyscallResult::Success(0)
        } else {
            SyscallResult::Error(SyscallError::InvalidOperation)
        }
    }

    /// Cambiar directorio de trabajo por fd
    pub fn fchdir(&mut self, fd: i32) -> SyscallResult {
        serial_write_str(&alloc::format!("PROCESS_SYSCALL: fchdir(fd={}) desde PID {}\n", fd, self.current_pid));

        // TODO: Implementar cambio de directorio por fd
        serial_write_str("PROCESS_SYSCALL: fchdir no implementado\n");
        SyscallResult::Error(SyscallError::NotImplemented)
    }

    /// Obtener directorio de trabajo actual
    pub fn getcwd(&self) -> &str {
        // TODO: Implementar obtención real del directorio de trabajo
        "/"
    }

    /// Obtener información de debug
    pub fn debug_info(&self) {
        serial_write_str("PROCESS_SYSCALL: Información de procesos:\n");
        serial_write_str(&alloc::format!("PROCESS_SYSCALL: Proceso actual: PID {}\n", self.current_pid));

        let mut count = 0;
        for process in &self.processes {
            if let Some(process) = process {
                count += 1;
                serial_write_str(&alloc::format!("PROCESS_SYSCALL: PID {}: PPID {}, Estado: {:?}, UID: {}\n", 
                                                process.pid, process.ppid, process.state, process.uid));
            }
        }

        if count == 0 {
            serial_write_str("PROCESS_SYSCALL: No hay procesos\n");
        }
    }
}

// Gestor global de procesos
static mut PROCESS_MANAGER: Option<ProcessManager> = None;

/// Inicializar el gestor de procesos
pub fn init_process_manager() {
    unsafe {
        PROCESS_MANAGER = Some(ProcessManager::new());
        serial_write_str("PROCESS_SYSCALL: Gestor de procesos inicializado\n");
    }
}

/// Obtener referencia al gestor de procesos
pub fn get_process_manager() -> &'static mut ProcessManager {
    unsafe {
        PROCESS_MANAGER.as_mut().expect("Gestor de procesos no inicializado")
    }
}

/// Syscall fork implementada
pub fn sys_fork_impl() -> SyscallResult {
    get_process_manager().fork()
}

/// Syscall execve implementada
pub fn sys_execve_impl(path: &str, argv: &[&str], envp: &[&str]) -> SyscallResult {
    get_process_manager().execve(path, argv, envp)
}

/// Syscall exit implementada
pub fn sys_exit_impl(exit_code: i32) -> SyscallResult {
    get_process_manager().exit(exit_code)
}

/// Syscall wait4 implementada
pub fn sys_wait4_impl(pid: ProcessId, wstatus: *mut i32, options: i32, rusage: *mut u8) -> SyscallResult {
    get_process_manager().wait4(pid, wstatus, options, rusage)
}

/// Syscall getpid implementada
pub fn sys_getpid_impl() -> ProcessId {
    get_process_manager().getpid()
}

/// Syscall getppid implementada
pub fn sys_getppid_impl() -> ProcessId {
    get_process_manager().getppid()
}

/// Syscall getuid implementada
pub fn sys_getuid_impl() -> UserId {
    get_process_manager().getuid()
}

/// Syscall getgid implementada
pub fn sys_getgid_impl() -> GroupId {
    get_process_manager().getgid()
}

/// Syscall geteuid implementada
pub fn sys_geteuid_impl() -> UserId {
    get_process_manager().geteuid()
}

/// Syscall getegid implementada
pub fn sys_getegid_impl() -> GroupId {
    get_process_manager().getegid()
}

/// Syscall setuid implementada
pub fn sys_setuid_impl(uid: UserId) -> SyscallResult {
    get_process_manager().setuid(uid)
}

/// Syscall setgid implementada
pub fn sys_setgid_impl(gid: GroupId) -> SyscallResult {
    get_process_manager().setgid(gid)
}

/// Syscall setreuid implementada
pub fn sys_setreuid_impl(ruid: UserId, euid: UserId) -> SyscallResult {
    get_process_manager().setreuid(ruid, euid)
}

/// Syscall setregid implementada
pub fn sys_setregid_impl(rgid: GroupId, egid: GroupId) -> SyscallResult {
    get_process_manager().setregid(rgid, egid)
}

/// Syscall kill implementada
pub fn sys_kill_impl(pid: ProcessId, sig: i32) -> SyscallResult {
    get_process_manager().kill(pid, sig)
}

/// Syscall chdir implementada
pub fn sys_chdir_impl(path: &str) -> SyscallResult {
    get_process_manager().chdir(path)
}

/// Syscall fchdir implementada
pub fn sys_fchdir_impl(fd: i32) -> SyscallResult {
    get_process_manager().fchdir(fd)
}

/// Syscall getcwd implementada
pub fn sys_getcwd_impl() -> &'static str {
    get_process_manager().getcwd()
}

/// Obtener información de procesos para debug
pub fn debug_process_info() {
    get_process_manager().debug_info();
}

/// Pruebas de procesos
pub fn test_process_syscalls() {
    serial_write_str("PROCESS_SYSCALL: Iniciando pruebas de syscalls de procesos\n");

    let manager = get_process_manager();

    // Probar getpid
    let pid = manager.getpid();
    serial_write_str(&alloc::format!("PROCESS_SYSCALL: PID actual: {}\n", pid));

    // Probar getuid
    let uid = manager.getuid();
    serial_write_str(&alloc::format!("PROCESS_SYSCALL: UID actual: {}\n", uid));

    // Probar fork
    let result = manager.fork();
    serial_write_str(&alloc::format!("PROCESS_SYSCALL: fork result: {:?}\n", result));

    // Mostrar información de debug
    manager.debug_info();

    serial_write_str("PROCESS_SYSCALL: Pruebas completadas\n");
}
