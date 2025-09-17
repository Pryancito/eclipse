//! Context switching para Eclipse OS
//! 
//! Implementa el cambio de contexto entre procesos

use super::process::{Process, CpuContext};
use super::{ProcessId, ProcessState};

/// Información de contexto de cambio
#[derive(Debug, Clone)]
pub struct ContextSwitchInfo {
    pub from_pid: ProcessId,
    pub to_pid: ProcessId,
    pub switch_time: u64,
    pub reason: ContextSwitchReason,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ContextSwitchReason {
    TimeSliceExpired,
    ProcessBlocked,
    ProcessTerminated,
    HigherPriorityProcess,
    SystemCall,
    Interrupt,
    VoluntaryYield,
}

/// Gestor de cambio de contexto
pub struct ContextManager {
    current_process: Option<ProcessId>,
    context_switch_count: u64,
    total_switch_time: u64,
    last_switch_time: u64,
}

impl ContextManager {
    pub fn new() -> Self {
        Self {
            current_process: None,
            context_switch_count: 0,
            total_switch_time: 0,
            last_switch_time: 0,
        }
    }

    pub fn switch_context(&mut self, from_process: &mut Process, to_process: &mut Process, reason: ContextSwitchReason) -> Result<ContextSwitchInfo, &'static str> {
        let current_time = self.get_current_time();
        
        // Guardar contexto del proceso actual
        let saved_context = from_process.save_cpu_context();
        
        // Actualizar estado del proceso saliente
        if from_process.state == ProcessState::Running {
            from_process.state = ProcessState::Ready;
        }
        
        // Restaurar contexto del proceso entrante
        to_process.restore_cpu_context(saved_context);
        to_process.state = ProcessState::Running;
        to_process.last_run_time = current_time;
        
        // Actualizar información del cambio de contexto
        let switch_info = ContextSwitchInfo {
            from_pid: from_process.pid,
            to_pid: to_process.pid,
            switch_time: current_time,
            reason,
        };
        
        self.current_process = Some(to_process.pid);
        self.context_switch_count += 1;
        
        if self.last_switch_time > 0 {
            self.total_switch_time += current_time - self.last_switch_time;
        }
        self.last_switch_time = current_time;
        
        Ok(switch_info)
    }

    pub fn save_context(&mut self, process: &mut Process) -> CpuContext {
        process.save_cpu_context()
    }

    pub fn restore_context(&mut self, process: &mut Process, context: CpuContext) {
        process.restore_cpu_context(context);
        process.state = ProcessState::Running;
        process.last_run_time = self.get_current_time();
        self.current_process = Some(process.pid);
    }

    pub fn get_current_process(&self) -> Option<ProcessId> {
        self.current_process
    }

    pub fn get_context_switch_count(&self) -> u64 {
        self.context_switch_count
    }

    pub fn get_average_switch_time(&self) -> f64 {
        if self.context_switch_count == 0 {
            0.0
        } else {
            self.total_switch_time as f64 / self.context_switch_count as f64
        }
    }

    fn get_current_time(&self) -> u64 {
        // En un sistema real, esto obtendría el tiempo actual del sistema
        0 // Simulado
    }
}

/// Funciones de bajo nivel para cambio de contexto
pub mod asm {
    use super::CpuContext;

    /// Guardar el contexto actual de CPU
    /// Esta función debe ser implementada en ensamblador
    pub unsafe fn save_cpu_context(context: *mut CpuContext) {
        // En una implementación real, esto guardaría todos los registros
        // usando instrucciones de ensamblador
        core::arch::asm!(
            "mov [rdi + 0x00], rax",
            "mov [rdi + 0x08], rbx",
            "mov [rdi + 0x10], rcx",
            "mov [rdi + 0x18], rdx",
            "mov [rdi + 0x20], rsi",
            "mov [rdi + 0x28], rdi",
            "mov [rdi + 0x30], rbp",
            "mov [rdi + 0x38], rsp",
            "mov [rdi + 0x40], r8",
            "mov [rdi + 0x48], r9",
            "mov [rdi + 0x50], r10",
            "mov [rdi + 0x58], r11",
            "mov [rdi + 0x60], r12",
            "mov [rdi + 0x68], r13",
            "mov [rdi + 0x70], r14",
            "mov [rdi + 0x78], r15",
            "mov [rdi + 0x80], rip",
            "pushf",
            "pop rax",
            "mov [rdi + 0x88], rax",
            "mov [rdi + 0x90], cs",
            "mov [rdi + 0x92], ds",
            "mov [rdi + 0x94], es",
            "mov [rdi + 0x96], fs",
            "mov [rdi + 0x98], gs",
            "mov [rdi + 0x9a], ss",
        );
    }

    /// Restaurar el contexto de CPU
    /// Esta función debe ser implementada en ensamblador
    pub unsafe fn restore_cpu_context(context: *const CpuContext) {
        // En una implementación real, esto restauraría todos los registros
        // usando instrucciones de ensamblador
        core::arch::asm!(
            "mov rax, [rdi + 0x00]",
            "mov rbx, [rdi + 0x08]",
            "mov rcx, [rdi + 0x10]",
            "mov rdx, [rdi + 0x18]",
            "mov rsi, [rdi + 0x20]",
            "mov rbp, [rdi + 0x30]",
            "mov rsp, [rdi + 0x38]",
            "mov r8, [rdi + 0x40]",
            "mov r9, [rdi + 0x48]",
            "mov r10, [rdi + 0x50]",
            "mov r11, [rdi + 0x58]",
            "mov r12, [rdi + 0x60]",
            "mov r13, [rdi + 0x68]",
            "mov r14, [rdi + 0x70]",
            "mov r15, [rdi + 0x78]",
            "mov rax, [rdi + 0x88]",
            "push rax",
            "popf",
            "mov rax, [rdi + 0x00]",
            "mov rdi, [rdi + 0x28]",
            "jmp [rdi + 0x80]",
        );
    }

    /// Cambio de contexto atómico
    /// Esta función realiza el cambio de contexto completo
    pub unsafe fn atomic_context_switch(from_context: *mut CpuContext, to_context: *const CpuContext) {
        // Guardar contexto actual
        save_cpu_context(from_context);
        
        // Restaurar nuevo contexto
        restore_cpu_context(to_context);
    }
}
