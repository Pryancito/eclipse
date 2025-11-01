//! Context Switching para Eclipse OS
//!
//! Este módulo implementa el cambio de contexto entre procesos,
//! permitiendo la multitarea real.

use crate::process::process::{CpuContext, ProcessId};
use crate::process::manager::get_process_manager;
use crate::debug::serial_write_str;
use core::arch::asm;

/// Guardar contexto del proceso actual
pub fn save_context(context: &mut CpuContext) {
    // Por ahora, guardado simplificado
    // En un sistema completo, esto se haría con assembly optimizado
    unsafe {
        // Guardar registros críticos
        let mut temp: u64;
        
        asm!("mov {}, rax", out(reg) temp, options(nostack, nomem));
        context.rax = temp;
        
        asm!("mov {}, rbx", out(reg) temp, options(nostack, nomem));
        context.rbx = temp;
        
        asm!("mov {}, rsp", out(reg) temp, options(nostack, nomem));
        context.rsp = temp;
        
        asm!("mov {}, rbp", out(reg) temp, options(nostack, nomem));
        context.rbp = temp;
        
        // Guardar flags
        asm!("pushfq", "pop {}", out(reg) temp, options(nomem));
        context.rflags = temp;
        
        // Segmentos (valores fijos por ahora)
        context.cs = 0x08;
        context.ds = 0x10;
        context.ss = 0x10;
        context.es = 0x10;
    }
}

/// Cargar contexto de un proceso
pub unsafe fn load_context(context: &CpuContext) {
    // Carga simplificada por ahora
    // En un sistema completo, esto se haría con assembly optimizado
    
    // Cargar registros críticos
    asm!("mov rax, {}", in(reg) context.rax, options(nostack, nomem));
    asm!("mov rbx, {}", in(reg) context.rbx, options(nostack, nomem));
    asm!("mov rbp, {}", in(reg) context.rbp, options(nostack, nomem));
    
    // Cargar flags
    asm!("push {}", "popfq", in(reg) context.rflags, options(nomem));
    
    // Cargar stack (último)
    asm!("mov rsp, {}", in(reg) context.rsp, options(nostack, nomem));
}

/// Cambiar al siguiente proceso
pub fn switch_to_next_process() -> bool {
    let mut manager_guard = get_process_manager().lock();
    
    if let Some(ref mut manager) = *manager_guard {
        // Obtener el proceso actual
        let current_pid = manager.current_process;
        
        // Guardar contexto del proceso actual si existe
        if let Some(pid) = current_pid {
            if let Some(ref mut process) = manager.processes[pid as usize] {
                save_context(&mut process.cpu_context);
                serial_write_str(&alloc::format!(
                    "CONTEXT_SWITCH: Saved context of process {}\n", pid
                ));
            }
        }
        
        // Seleccionar el siguiente proceso del scheduler
        if let Some(next_pid) = manager.process_scheduler.get_next_process() {
            if next_pid != current_pid.unwrap_or(u32::MAX) {
                serial_write_str(&alloc::format!(
                    "CONTEXT_SWITCH: Switching from {:?} to {}\n",
                    current_pid, next_pid
                ));
                
                // Actualizar proceso actual
                manager.current_process = Some(next_pid);
                
                // Marcar proceso actual como Running
                if let Some(ref mut process) = manager.processes[next_pid as usize] {
                    use crate::process::process::ProcessState;
                    process.set_state(ProcessState::Running);
                    
                    // Cargar contexto del nuevo proceso
                    let context = process.cpu_context;
                    drop(manager_guard); // Liberar el lock antes de cambiar contexto
                    
                    unsafe {
                        load_context(&context);
                    }
                    
                    return true;
                }
            }
        }
    }
    
    false
}

/// Cambiar a un proceso específico
pub fn switch_to_process(target_pid: ProcessId) -> Result<(), &'static str> {
    let mut manager_guard = get_process_manager().lock();
    
    if let Some(ref mut manager) = *manager_guard {
        // Verificar que el proceso target existe
        if target_pid as usize >= crate::process::MAX_PROCESSES {
            return Err("Invalid target PID");
        }
        
        if manager.processes[target_pid as usize].is_none() {
            return Err("Target process not found");
        }
        
        let current_pid = manager.current_process;
        
        // Guardar contexto del proceso actual
        if let Some(pid) = current_pid {
            if let Some(ref mut process) = manager.processes[pid as usize] {
                save_context(&mut process.cpu_context);
                
                // Marcar como Ready (ya no está ejecutándose)
                use crate::process::process::ProcessState;
                if process.state == ProcessState::Running {
                    process.set_state(ProcessState::Ready);
                }
            }
        }
        
        serial_write_str(&alloc::format!(
            "CONTEXT_SWITCH: Switching to process {}\n", target_pid
        ));
        
        // Actualizar proceso actual
        manager.current_process = Some(target_pid);
        
        // Cargar contexto del proceso target
        if let Some(ref mut process) = manager.processes[target_pid as usize] {
            use crate::process::process::ProcessState;
            process.set_state(ProcessState::Running);
            
            let context = process.cpu_context;
            drop(manager_guard); // Liberar el lock
            
            unsafe {
                load_context(&context);
            }
            
            Ok(())
        } else {
            Err("Failed to load target process")
        }
    } else {
        Err("Process manager not initialized")
    }
}

/// Yield - ceder CPU al siguiente proceso voluntariamente
pub fn yield_cpu() {
    serial_write_str("CONTEXT_SWITCH: Process yielding CPU\n");
    switch_to_next_process();
}

/// Preparar contexto inicial para un nuevo proceso
pub fn prepare_initial_context(
    entry_point: u64,
    stack_pointer: u64,
    is_kernel: bool,
) -> CpuContext {
    let mut context = CpuContext::default();
    
    // Configurar entry point
    context.rip = entry_point;
    
    // Configurar stack
    context.rsp = stack_pointer;
    context.rbp = stack_pointer;
    
    // Configurar flags (interrupciones habilitadas)
    context.rflags = 0x202; // IF flag enabled
    
    // Configurar selectores de segmento
    if is_kernel {
        context.cs = 0x08;  // Kernel code segment
        context.ds = 0x10;  // Kernel data segment
        context.ss = 0x10;  // Kernel stack segment
    } else {
        context.cs = 0x1B;  // User code segment (ring 3)
        context.ds = 0x23;  // User data segment (ring 3)
        context.ss = 0x23;  // User stack segment (ring 3)
    }
    
    context.es = context.ds;
    context.fs = context.ds;
    context.gs = context.ds;
    
    // Limpiar registros generales
    context.rax = 0;
    context.rbx = 0;
    context.rcx = 0;
    context.rdx = 0;
    context.rsi = 0;
    context.rdi = 0;
    context.r8 = 0;
    context.r9 = 0;
    context.r10 = 0;
    context.r11 = 0;
    context.r12 = 0;
    context.r13 = 0;
    context.r14 = 0;
    context.r15 = 0;
    
    context
}

