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
    unsafe {
        let mut temp: u64;
        
        // Guardar todos los registros de propósito general
        asm!("mov {}, rax", out(reg) temp, options(nostack, nomem));
        context.rax = temp;
        
        asm!("mov {}, rbx", out(reg) temp, options(nostack, nomem));
        context.rbx = temp;
        
        asm!("mov {}, rcx", out(reg) temp, options(nostack, nomem));
        context.rcx = temp;
        
        asm!("mov {}, rdx", out(reg) temp, options(nostack, nomem));
        context.rdx = temp;
        
        asm!("mov {}, rsi", out(reg) temp, options(nostack, nomem));
        context.rsi = temp;
        
        asm!("mov {}, rdi", out(reg) temp, options(nostack, nomem));
        context.rdi = temp;
        
        asm!("mov {}, rsp", out(reg) temp, options(nostack, nomem));
        context.rsp = temp;
        
        asm!("mov {}, rbp", out(reg) temp, options(nostack, nomem));
        context.rbp = temp;
        
        // Registros extendidos R8-R15
        asm!("mov {}, r8", out(reg) temp, options(nostack, nomem));
        context.r8 = temp;
        
        asm!("mov {}, r9", out(reg) temp, options(nostack, nomem));
        context.r9 = temp;
        
        asm!("mov {}, r10", out(reg) temp, options(nostack, nomem));
        context.r10 = temp;
        
        asm!("mov {}, r11", out(reg) temp, options(nostack, nomem));
        context.r11 = temp;
        
        asm!("mov {}, r12", out(reg) temp, options(nostack, nomem));
        context.r12 = temp;
        
        asm!("mov {}, r13", out(reg) temp, options(nostack, nomem));
        context.r13 = temp;
        
        asm!("mov {}, r14", out(reg) temp, options(nostack, nomem));
        context.r14 = temp;
        
        asm!("mov {}, r15", out(reg) temp, options(nostack, nomem));
        context.r15 = temp;
        
        // Guardar RIP (instruction pointer) - usar dirección de retorno
        asm!("lea {}, [rip]", out(reg) temp, options(nostack, nomem));
        context.rip = temp;
        
        // Guardar flags
        asm!("pushfq", "pop {}", out(reg) temp, options(nomem));
        context.rflags = temp;
        
        // Guardar selectores de segmento
        asm!("mov {0:x}, cs", out(reg) temp, options(nostack, nomem));
        context.cs = temp as u16;
        
        asm!("mov {0:x}, ds", out(reg) temp, options(nostack, nomem));
        context.ds = temp as u16;
        
        asm!("mov {0:x}, ss", out(reg) temp, options(nostack, nomem));
        context.ss = temp as u16;
        
        asm!("mov {0:x}, es", out(reg) temp, options(nostack, nomem));
        context.es = temp as u16;
        
        asm!("mov {0:x}, fs", out(reg) temp, options(nostack, nomem));
        context.fs = temp as u16;
        
        asm!("mov {0:x}, gs", out(reg) temp, options(nostack, nomem));
        context.gs = temp as u16;
    }
}

/// Cargar contexto de un proceso
pub unsafe fn load_context(context: &CpuContext) {
    // Cargar todos los registros de propósito general
    asm!("mov rax, {}", in(reg) context.rax, options(nostack, nomem));
    asm!("mov rbx, {}", in(reg) context.rbx, options(nostack, nomem));
    asm!("mov rcx, {}", in(reg) context.rcx, options(nostack, nomem));
    asm!("mov rdx, {}", in(reg) context.rdx, options(nostack, nomem));
    asm!("mov rsi, {}", in(reg) context.rsi, options(nostack, nomem));
    asm!("mov rdi, {}", in(reg) context.rdi, options(nostack, nomem));
    asm!("mov rbp, {}", in(reg) context.rbp, options(nostack, nomem));
    
    // Registros extendidos R8-R15
    asm!("mov r8, {}", in(reg) context.r8, options(nostack, nomem));
    asm!("mov r9, {}", in(reg) context.r9, options(nostack, nomem));
    asm!("mov r10, {}", in(reg) context.r10, options(nostack, nomem));
    asm!("mov r11, {}", in(reg) context.r11, options(nostack, nomem));
    asm!("mov r12, {}", in(reg) context.r12, options(nostack, nomem));
    asm!("mov r13, {}", in(reg) context.r13, options(nostack, nomem));
    asm!("mov r14, {}", in(reg) context.r14, options(nostack, nomem));
    asm!("mov r15, {}", in(reg) context.r15, options(nostack, nomem));
    
    // Cargar selectores de segmento
    asm!("mov ds, {0:x}", in(reg) context.ds as u64, options(nostack, nomem));
    asm!("mov es, {0:x}", in(reg) context.es as u64, options(nostack, nomem));
    asm!("mov fs, {0:x}", in(reg) context.fs as u64, options(nostack, nomem));
    asm!("mov gs, {0:x}", in(reg) context.gs as u64, options(nostack, nomem));
    
    // Cargar flags
    asm!("push {}", "popfq", in(reg) context.rflags, options(nomem));
    
    // Cargar stack pointer (último, para no perder el contexto)
    asm!("mov rsp, {}", in(reg) context.rsp, options(nostack, nomem));
    
    // Note: RIP se cargará con un ret o jmp desde el código que llama a load_context
}

/// Cambiar al siguiente proceso
pub fn switch_to_next_process() -> bool {
    let mut manager_guard = get_process_manager().lock();
    
    if let Some(ref mut manager) = *manager_guard {
        // Get the current process
        let current_pid = manager.current_process;
        
        // Save context of current process if it exists
        if let Some(pid) = current_pid {
            if let Some(ref mut process) = manager.processes[pid as usize] {
                save_context(&mut process.cpu_context);
                serial_write_str(&alloc::format!(
                    "CONTEXT_SWITCH: Saved context of process {}\n", pid
                ));
                
                // Set current process to Ready state (will be re-queued by scheduler)
                use crate::process::process::ProcessState;
                if process.state == ProcessState::Running {
                    process.set_state(ProcessState::Ready);
                }
            }
        }
        
        // Use scheduler to select next process
        let next_pid = manager.process_scheduler.schedule(&manager.processes);
        
        if let Some(next_pid) = next_pid {
            if next_pid != current_pid.unwrap_or(u32::MAX) {
                serial_write_str(&alloc::format!(
                    "CONTEXT_SWITCH: Switching from {:?} to {}\n",
                    current_pid, next_pid
                ));
            }
            
            // Update current process
            manager.current_process = Some(next_pid);
            
            // Mark new process as Running
            if let Some(ref mut process) = manager.processes[next_pid as usize] {
                use crate::process::process::ProcessState;
                process.set_state(ProcessState::Running);
                
                // Load context of new process
                let context = process.cpu_context;
                drop(manager_guard); // Release lock before changing context
                
                unsafe {
                    load_context(&context);
                }
                
                return true;
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

