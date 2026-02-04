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
    let mut queue = READY_QUEUE.lock();
    let mut tail = QUEUE_TAIL.lock();
    let head = *QUEUE_HEAD.lock();
    
    let next_tail = (*tail + 1) % READY_QUEUE_SIZE;
    if next_tail != head {
        queue[*tail] = Some(pid);
        *tail = next_tail;
    }
}

/// Sacar siguiente proceso de la cola ready
fn dequeue_process() -> Option<ProcessId> {
    let mut queue = READY_QUEUE.lock();
    let mut head = QUEUE_HEAD.lock();
    let tail = *QUEUE_TAIL.lock();
    
    if *head == tail {
        return None;
    }
    
    let pid = queue[*head].take();
    *head = (*head + 1) % READY_QUEUE_SIZE;
    pid
}

/// Tick del scheduler (llamado desde timer interrupt)
pub fn tick() {
    let mut stats = SCHEDULER_STATS.lock();
    stats.total_ticks += 1;
    drop(stats);
    
    // Cada 10 ticks, hacer un context switch
    let ticks = SCHEDULER_STATS.lock().total_ticks;
    if ticks % 10 == 0 {
        schedule();
    }
}

/// Función principal de scheduling
pub fn schedule() {
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
                set_current_process(Some(next_pid));
            }
        }
    }
}

/// Realizar context switch entre dos procesos
#[inline(never)]
fn perform_context_switch(from_pid: ProcessId, to_pid: ProcessId) {
    let dbg_from_pid = Some(from_pid);
    let mut stats = SCHEDULER_STATS.lock();
    stats.context_switches += 1;
    drop(stats);
    
    set_current_process(Some(to_pid));
    
    // Get raw pointers to contexts in the global table
    let (from_ctx_ptr, to_ctx_ptr, to_kernel_stack, to_page_table) = {
        let mut table = crate::process::PROCESS_TABLE.lock();
        
        let from_process = table[from_pid as usize].as_mut().expect("From process not found");
        let from_ptr = &mut from_process.context as *mut crate::process::Context;
        
        let to_process = table[to_pid as usize].as_mut().expect("To process not found");
        let to_ptr = &to_process.context as *const crate::process::Context;
        
        // Capture properties needed for debug/setup
        let kstack = to_process.kernel_stack_top;
        let pt = to_process.page_table_phys;
        
        (from_ptr, to_ptr, kstack, pt)
    };
    
    // Update TSS RSP0 for the next process
    crate::boot::set_tss_stack(to_kernel_stack);
    
    // Switch address space if necessary
    unsafe {
        let current_cr3 = crate::memory::get_cr3();
        if to_page_table != 0 && to_page_table != current_cr3 {
            core::arch::asm!("mov cr3, {}", in(reg) to_page_table);
        }
    }
    
    // Perform raw context switch
    unsafe {
        crate::process::switch_context(&mut *from_ctx_ptr, &*to_ctx_ptr);
    }
    
    // Force compilation of return path
    // This prevents LLVM from optimizing away the return or inserting ud2
    // if it mistakenly thinks the function diverges.
    // crate::serial::serial_print("Resumed from switch\n");
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
