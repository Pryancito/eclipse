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
fn perform_context_switch(from_pid: ProcessId, to_pid: ProcessId) {
    let mut stats = SCHEDULER_STATS.lock();
    stats.context_switches += 1;
    drop(stats);
    
    set_current_process(Some(to_pid));
    
    // Obtener contextos
    let mut from_process = get_process(from_pid).unwrap();
    let to_process = get_process(to_pid).unwrap();
    
    // Realizar switch
    unsafe {
        crate::process::switch_context(&mut from_process.context, &to_process.context);
    }
    
    // Actualizar proceso from después del switch
    update_process(from_pid, from_process);
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
