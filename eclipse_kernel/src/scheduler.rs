//! Scheduler básico round-robin

use crate::process::{ProcessId, ProcessState, get_process, update_process, current_process_id, set_current_process};
use spin::Mutex;

/// Cola de procesos ready
const READY_QUEUE_SIZE: usize = 64;
static READY_QUEUE: Mutex<[Option<ProcessId>; READY_QUEUE_SIZE]> = Mutex::new([None; READY_QUEUE_SIZE]);
static QUEUE_HEAD: Mutex<usize> = Mutex::new(0);
static QUEUE_TAIL: Mutex<usize> = Mutex::new(0);

/// Estadísticas del scheduler
pub struct SchedulerStats {
    pub context_switches: u64,
    pub total_ticks: u64,
}

static SCHEDULER_STATS: Mutex<SchedulerStats> = Mutex::new(SchedulerStats {
    context_switches: 0,
    total_ticks: 0,
});

/// Agregar proceso a la cola ready
pub fn enqueue_process(pid: ProcessId) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut queue = READY_QUEUE.lock();
        let mut tail = QUEUE_TAIL.lock();
        let head = *QUEUE_HEAD.lock();
        
        let next_tail = (*tail + 1) % READY_QUEUE_SIZE;
        if next_tail != head {
            queue[*tail] = Some(pid);
            *tail = next_tail;
        }
    });
}

/// Sacar siguiente proceso de la cola ready
fn dequeue_process() -> Option<ProcessId> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut queue = READY_QUEUE.lock();
        let mut head = QUEUE_HEAD.lock();
        let tail = *QUEUE_TAIL.lock();
        
        if *head == tail {
            return None;
        }
        
        let pid = queue[*head].take();
        *head = (*head + 1) % READY_QUEUE_SIZE;
        pid
    })
}

/// Tick del scheduler (llamado desde timer interrupt)
pub fn tick() {
    let mut stats = SCHEDULER_STATS.lock();
    stats.total_ticks += 1;
    let ticks = stats.total_ticks;
    drop(stats);
    
    // Debug print every 100 ticks
    // if ticks % 100 == 0 {
    //    crate::serial::serial_print("SCHEDULER: Tick ");
    //    crate::serial::serial_print_dec(ticks);
    //    crate::serial::serial_print("\n");
    // }
    
    // Cada 10 ticks, hacer un context switch
    if ticks % 10 == 0 {
        schedule();
    }
}

/// Función principal de scheduling
pub fn schedule() {
    x86_64::instructions::interrupts::without_interrupts(|| {
        // Obtener proceso actual
        let current_pid = current_process_id();
        
        // Si hay un proceso actual en ejecución, guardarlo en la cola
        if let Some(pid) = current_pid {
            if let Some(mut process) = get_process(pid) {
                if process.state == ProcessState::Running {
                    process.state = ProcessState::Ready;
                    update_process(pid, process);
                    enqueue_process(pid);
                }
            }
        }
        
        // Obtener siguiente proceso de la cola
        if let Some(next_pid) = dequeue_process() {
            if let Some(mut next_process) = get_process(next_pid) {
                next_process.state = ProcessState::Running;
                update_process(next_pid, next_process);
                
                // Hacer context switch
                if let Some(current_pid) = current_pid {
                    if current_pid != next_pid {
                        perform_context_switch(current_pid, next_pid);
                    }
                } else {
                    crate::serial::serial_printf(format_args!("[Sched] Core {} picking first process PID {}\n", 
                        crate::process::get_cpu_id(), next_pid));
                    set_current_process(Some(next_pid));
                    // Initial switch to first process if none was running
                    // We need a dummy context to switch from
                    let mut dummy = crate::process::Context::new();
                    perform_context_switch_to(&mut dummy, next_pid);
                }
            }
        }
    });
}

fn perform_context_switch(from_pid: ProcessId, to_pid: ProcessId) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut stats = SCHEDULER_STATS.lock();
        stats.context_switches += 1;
        drop(stats);
        
        set_current_process(Some(to_pid));
        
        // Use a temporary scope to get a pointer to the context
        let from_ptr = {
            let mut table = crate::process::PROCESS_TABLE.lock();
            let from_process = table[from_pid as usize].as_mut().expect("From process not found");
            &mut from_process.context as *mut crate::process::Context
        };
        
        perform_context_switch_to(unsafe { &mut *from_ptr }, to_pid);
    });
}

fn perform_context_switch_to(from_ctx: &mut crate::process::Context, to_pid: ProcessId) {
    let (to_ptr, to_kernel_stack, to_page_table, to_fs_base) = {
        let mut table = crate::process::PROCESS_TABLE.lock();
        let to_process = table[to_pid as usize].as_mut().expect("To process not found");
        (
            &to_process.context as *const crate::process::Context,
            to_process.kernel_stack_top,
            to_process.page_table_phys,
            to_process.fs_base
        )
    };
    
    // Update TSS RSP0
    crate::boot::set_tss_stack(to_kernel_stack);
    
    // Save current FS_BASE to from_ctx (optional, if we want to support user-mode changes being persisted)
    // Actually, we should probably save it back to the Process struct, but switch_context takes from_ctx.
    // Let's just restore the new one.
    unsafe {
        use core::arch::asm;
        let msr_fs_base = 0xC0000100u32;
        
        // Load new FS_BASE
        let low = to_fs_base as u32;
        let high = (to_fs_base >> 32) as u32;
        asm!("wrmsr", in("ecx") msr_fs_base, in("eax") low, in("edx") high, options(nomem, nostack, preserves_flags));
    }
    
    // Switch address space if necessary
    // The kernel is mapped identically in all address spaces, so CR3 switching is safe
    let next_cr3 = {
        let current_cr3 = crate::memory::get_cr3();
        if to_page_table != 0 && to_page_table != current_cr3 {
            to_page_table
        } else {
            0
        }
    };
    
    // Perform raw context switch
    unsafe {
        crate::process::switch_context(from_ctx, &*to_ptr, next_cr3);
    }
}

/// Yield - ceder CPU voluntariamente
pub fn yield_cpu() {
    schedule();
}

/// Dormir el proceso actual (stub - no implementado completamente)
pub fn sleep(_ticks: u64) {
    // TODO: Implementar lista de procesos bloqueados con timer
    yield_cpu();
}

/// Obtener estadísticas del scheduler
pub fn get_stats() -> SchedulerStats {
    let stats = SCHEDULER_STATS.lock();
    SchedulerStats {
        context_switches: stats.context_switches,
        total_ticks: stats.total_ticks,
    }
}

/// Inicializar scheduler
pub fn init() {
    crate::serial::serial_print("Scheduler initialized\n");
}
