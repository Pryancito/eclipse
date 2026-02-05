//! Scheduler básico round-robin

use crate::process::{ProcessId, ProcessState, get_process, update_process, current_process_id, set_current_process};
use spin::Mutex;
use core::sync::atomic::{AtomicBool, Ordering};

/// Flag to indicate if the scheduler is enabled
static SCHEDULER_ENABLED: AtomicBool = AtomicBool::new(false);

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
    // Don't schedule if scheduler is not enabled yet
    if !SCHEDULER_ENABLED.load(Ordering::SeqCst) {
        return;
    }
    
    // Obtener proceso actual
    let current_pid = current_process_id();
    
    crate::serial::serial_print("[SCHEDULER] schedule() called, current_pid=");
    if let Some(pid) = current_pid {
        crate::serial::serial_print_dec(pid as u64);
    } else {
        crate::serial::serial_print("None");
    }
    crate::serial::serial_print("\n");
    
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
    crate::serial::serial_print("[SCHEDULER] Dequeuing next process...\n");
    if let Some(next_pid) = dequeue_process() {
        crate::serial::serial_print("[SCHEDULER] Next PID: ");
        crate::serial::serial_print_dec(next_pid as u64);
        crate::serial::serial_print("\n");
        
        if let Some(mut next_process) = get_process(next_pid) {
            next_process.state = ProcessState::Running;
            update_process(next_pid, next_process);
            
            // Hacer context switch
            if let Some(current_pid) = current_pid {
                // Check if this is the kernel process (PID 0) - it has no real context to save
                if current_pid == 0 {
                    // This is the initial switch from kernel to first user process
                    set_current_process(Some(next_pid));
                    crate::serial::serial_print("[SCHEDULER] Initial switch from kernel (PID 0) to user process\n");
                    perform_initial_context_switch(next_pid);
                } else if current_pid != next_pid {
                    perform_context_switch(current_pid, next_pid);
                }
            } else {
                // No current process - this is the initial context switch to first user process
                set_current_process(Some(next_pid));
                crate::serial::serial_print("[SCHEDULER] About to perform initial context switch\n");
                perform_initial_context_switch(next_pid);
            }
        }
    } else {
        crate::serial::serial_print("[SCHEDULER] No process in queue!\n");
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

/// Perform initial context switch to first user process
/// This is called when there is no current process context to save
#[inline(never)]
fn perform_initial_context_switch(to_pid: ProcessId) -> ! {
    crate::serial::serial_print("[SCHEDULER] Performing initial context switch to PID ");
    crate::serial::serial_print_dec(to_pid as u64);
    crate::serial::serial_print("\n");
    
    let mut stats = SCHEDULER_STATS.lock();
    stats.context_switches += 1;
    drop(stats);
    
    crate::serial::serial_print("[SCHEDULER] Getting process context...\n");
    
    // Get process context and setup
    let (to_ctx_ptr, to_kernel_stack, to_page_table) = {
        let mut table = crate::process::PROCESS_TABLE.lock();
        let to_process = table[to_pid as usize].as_mut().expect("To process not found");
        let to_ptr = &to_process.context as *const crate::process::Context;
        let kstack = to_process.kernel_stack_top;
        let pt = to_process.page_table_phys;
        (to_ptr, kstack, pt)
    };
    
    crate::serial::serial_print("[SCHEDULER] Context obtained, updating TSS...\n");
    
    // Update TSS RSP0 for the next process
    crate::boot::set_tss_stack(to_kernel_stack);
    
    crate::serial::serial_print("[SCHEDULER] TSS updated, NOT switching address space (will switch in userspace trampoline)...\n");
    
    // DON'T switch address space here! The kernel code we're running is only mapped
    // in the kernel page table. Switching now would cause a page fault.
    // The address space switch will happen in jump_to_userspace via iretq.
    // Just verify the page table is valid
    if to_page_table == 0 {
        crate::serial::serial_print("[SCHEDULER] ERROR: Process has no page table!\n");
        loop {
            unsafe { core::arch::asm!("hlt") };
        }
    }
    
    crate::serial::serial_print("[SCHEDULER] Address space switched, calling switch_context...\n");
    
    // Load the context and jump to the process
    // We use switch_context but with a dummy source context
    let mut dummy_context = crate::process::Context {
        rax: 0, rbx: 0, rcx: 0, rdx: 0,
        rsi: 0, rdi: 0, rbp: 0, rsp: 0,
        r8: 0, r9: 0, r10: 0, r11: 0,
        r12: 0, r13: 0, r14: 0, r15: 0,
        rip: 0,
        rflags: 0,
    };
    
    unsafe {
        // Call switch_context which will save dummy context (which we'll never use)
        // and restore the target process context
        crate::process::switch_context(&mut dummy_context, &*to_ctx_ptr);
    }
    
    // This should never be reached since switch_context jumps to the target process
    crate::serial::serial_print("[SCHEDULER] ERROR: Returned from switch_context!\n");
    loop {
        unsafe { core::arch::asm!("hlt") };
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

/// Enable the scheduler - allows schedule() to perform context switches
pub fn enable() {
    SCHEDULER_ENABLED.store(true, Ordering::SeqCst);
    crate::serial::serial_print("[SCHEDULER] Enabled\n");
}

/// Disable the scheduler - schedule() will return immediately
#[allow(dead_code)]
pub fn disable() {
    SCHEDULER_ENABLED.store(false, Ordering::SeqCst);
    crate::serial::serial_print("[SCHEDULER] Disabled\n");
}
