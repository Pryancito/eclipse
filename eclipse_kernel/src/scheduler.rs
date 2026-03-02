//! Scheduler básico round-robin con soporte SMP completo

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use crate::process::{ProcessId, ProcessState, get_process, update_process, current_process_id, set_current_process};
use spin::Mutex;

/// Número máximo de CPUs soportadas (debe coincidir con process::MAX_CPUS)
const MAX_CPUS: usize = 16;

/// Cola de procesos ready (ampliada para SMP)
const READY_QUEUE_SIZE: usize = 512;
static READY_QUEUE: Mutex<[Option<ProcessId>; READY_QUEUE_SIZE]> = Mutex::new([None; READY_QUEUE_SIZE]);
static QUEUE_HEAD: Mutex<usize> = Mutex::new(0);
static QUEUE_TAIL: Mutex<usize> = Mutex::new(0);

/// Contextos idle por CPU para APs.
/// CPU 0 (BSP) utiliza el contexto del proceso kernel (PID 0) como idle.
/// CPUs 1..MAX_CPUS guardan aquí su contexto idle inicial para poder volver
/// al bucle idle cuando no hay ningún proceso usuario listo.
///
/// Safety: cada elemento [i] es accedido exclusivamente por la CPU i (indexada
/// por cpu_id = APIC ID % MAX_CPUS). No se requiere sincronización adicional
/// porque dos CPUs distintas nunca acceden al mismo índice simultáneamente.
/// Este patrón es idéntico al usado en boot.rs para CPU_DATA/CPU_TSSES/CPU_GDTS.
static mut AP_IDLE_CONTEXTS: [crate::process::Context; MAX_CPUS] =
    [const { crate::process::Context::new() }; MAX_CPUS];

/// Indica si el contexto idle del AP ya fue guardado al menos una vez.
static AP_IDLE_CONTEXT_VALID: [AtomicBool; MAX_CPUS] =
    [const { AtomicBool::new(false) }; MAX_CPUS];

/// Estadísticas del scheduler
pub struct SchedulerStats {
    pub context_switches: u64,
    pub total_ticks: u64,
    pub idle_ticks: u64,
}

static SCHEDULER_STATS: Mutex<SchedulerStats> = Mutex::new(SchedulerStats {
    context_switches: 0,
    total_ticks: 0,
    idle_ticks: 0,
});

/// Cuántas veces se dio CPU a cada PID en la última ventana (se lee y resetea en el heartbeat).
const MAX_PIDS: usize = 64;
static RUN_COUNTS: [AtomicU32; MAX_PIDS] = [const { AtomicU32::new(0) }; MAX_PIDS];

/// Devuelve y resetea los conteos de ejecución por PID (para el heartbeat de depuración).
pub fn take_run_counts() -> [u32; MAX_PIDS] {
    let mut out = [0u32; MAX_PIDS];
    for i in 0..MAX_PIDS {
        out[i] = RUN_COUNTS[i].swap(0, Ordering::Relaxed);
    }
    out
}

/// Agregar proceso a la cola ready
pub fn enqueue_process(pid: ProcessId) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut queue = READY_QUEUE.lock();
        let head = QUEUE_HEAD.lock();
        let mut tail = QUEUE_TAIL.lock();
        
        let next_tail = (*tail + 1) % READY_QUEUE_SIZE;
        if next_tail != *head {
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

/// Returns the virtual address of the ready queue tail pointer.
/// Used for MONITOR/MWAIT idle optimization.
pub fn ready_queue_tail_addr() -> usize {
    let tail = QUEUE_TAIL.lock();
    &*tail as *const usize as usize
}

/// Tick del scheduler (llamado desde timer interrupt)
pub fn tick() {
    let mut stats = SCHEDULER_STATS.lock();
    stats.total_ticks += 1;
    
    // Si el proceso actual es el kernel (PID 0), es tiempo idle
    let current_pid = crate::process::current_process_id();
    if current_pid == Some(0) {
        stats.idle_ticks += 1;
    } else if let Some(pid) = current_pid {
        // Incrementar ticks del proceso actual
        x86_64::instructions::interrupts::without_interrupts(|| {
            let mut table = crate::process::PROCESS_TABLE.lock();
            if let Some(p) = table[pid as usize].as_mut() {
                p.cpu_ticks += 1;
            }
        });
    }
    
    let ticks = stats.total_ticks;
    drop(stats);
    
    // Wake up sleeping processes whose wake_tick has arrived.
    // Only locks SLEEP_QUEUE (never PROCESS_TABLE) to avoid deadlock with
    // syscall paths that hold PROCESS_TABLE with interrupts enabled.
    wake_sleeping_processes(ticks);
    
    // Cada 10 ticks, hacer un context switch
    if ticks % 10 == 0 {
        schedule();
    }
}

/// Entry in the sleep queue: a process waiting to be re-scheduled after a delay.
#[derive(Clone, Copy)]
struct SleepEntry {
    pid: ProcessId,
    wake_tick: u64,
    valid: bool,
}

impl SleepEntry {
    const fn empty() -> Self {
        Self { pid: 0, wake_tick: 0, valid: false }
    }
}

/// Cola de sleep ampliada para soportar múltiples CPUs sin desbordamientos.
const SLEEP_QUEUE_SIZE: usize = 256;
static SLEEP_QUEUE: Mutex<[SleepEntry; SLEEP_QUEUE_SIZE]> = Mutex::new([SleepEntry::empty(); SLEEP_QUEUE_SIZE]);

/// Register a process to be re-queued after `wake_tick` timer ticks have elapsed.
/// Called from sys_nanosleep after setting the process state to Blocked.
/// Si el PID ya está en la cola, solo actualiza el wake_tick si es más tarde,
/// evitando añadir la misma entrada múltiples veces (deduplicación SMP).
pub fn add_sleep(pid: ProcessId, wake_tick: u64) {
    let mut added = false;
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut q = SLEEP_QUEUE.lock();
        // Primera pasada: comprobar si el PID ya está en la cola (deduplicación).
        for entry in q.iter_mut() {
            if entry.valid && entry.pid == pid {
                // Ya está durmiendo; ampliar el plazo si la nueva petición es más tardía.
                if wake_tick > entry.wake_tick {
                    entry.wake_tick = wake_tick;
                }
                added = true;
                break;
            }
        }
        // Segunda pasada: buscar un slot vacío si el PID no estaba ya en la cola.
        if !added {
            for entry in q.iter_mut() {
                if !entry.valid {
                    entry.pid = pid;
                    entry.wake_tick = wake_tick;
                    entry.valid = true;
                    added = true;
                    break;
                }
            }
        }
    });
    if !added {
        // Sleep queue full: fall back to immediate enqueue so the process isn't lost.
        crate::serial::serial_print("[SCHED] WARNING: sleep queue full, waking process immediately\n");
        enqueue_process(pid);
    }
}

/// Check sleep queue and re-enqueue processes whose sleep timer has expired.
/// This runs in timer interrupt context; it must NOT lock PROCESS_TABLE.
fn wake_sleeping_processes(current_tick: u64) {
    // Collect PIDs to wake with the sleep queue lock held, then release it
    // before calling enqueue_process (which acquires READY_QUEUE locks).
    let mut pids_to_wake = [0u32; SLEEP_QUEUE_SIZE];
    let mut count = 0usize;

    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut q = SLEEP_QUEUE.lock();
        for entry in q.iter_mut() {
            if entry.valid && current_tick >= entry.wake_tick {
                if count < SLEEP_QUEUE_SIZE {
                    pids_to_wake[count] = entry.pid;
                    count += 1;
                }
                entry.valid = false;
            }
        }
    });

    for i in 0..count {
        enqueue_process(pids_to_wake[i]);
    }
}

/// Función principal de scheduling con soporte SMP completo.
///
/// Invariantes SMP:
/// - PID 0 (kernel/idle del BSP) NUNCA se mete en la cola global de ready.
///   Actúa como idle privado del BSP; los APs nunca lo ejecutan.
/// - Cada AP tiene su propio contexto idle en AP_IDLE_CONTEXTS[cpu_id].
///   Al cambiar de "sin proceso" a un proceso usuario se guarda el contexto
///   idle; al volver (proceso bloqueado y cola vacía) se restaura.
/// - Cuando el proceso actual está bloqueado y no hay siguiente proceso:
///   BSP vuelve a PID 0; AP vuelve a su contexto idle.
pub fn schedule() {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let cpu_id = crate::process::get_cpu_id();
        let current_pid = current_process_id();

        // Paso 1: Si hay un proceso usuario en ejecución (Running), preemptarlo
        // y meterlo en la cola ready. PID 0 NUNCA se encola globalmente.
        if let Some(pid) = current_pid {
            if pid != 0 {
                if let Some(mut process) = get_process(pid) {
                    if process.state == ProcessState::Running {
                        process.state = ProcessState::Ready;
                        update_process(pid, process);
                        enqueue_process(pid);
                    }
                }
            }
        }

        // Paso 2: Obtener el siguiente proceso de la cola.
        if let Some(next_pid) = dequeue_process() {
            if (next_pid as usize) < MAX_PIDS {
                RUN_COUNTS[next_pid as usize].fetch_add(1, Ordering::Relaxed);
            }
            if let Some(mut next_process) = get_process(next_pid) {
                next_process.state = ProcessState::Running;
                update_process(next_pid, next_process);

                match current_pid {
                    Some(cur) if cur == next_pid => {
                        // Mismo proceso (único en cola), continúa sin cambio de contexto.
                    }
                    Some(cur) => {
                        perform_context_switch(cur, next_pid);
                    }
                    None => {
                        // AP sin proceso actual: guardar contexto idle y saltar al proceso.
                        crate::serial::serial_printf(format_args!(
                            "[Sched] Core {} picking first process PID {}\n",
                            cpu_id, next_pid));
                        set_current_process(Some(next_pid));
                        if cpu_id < MAX_CPUS {
                            // Guardar el contexto idle de este AP para poder volver más tarde.
                            // Safety: cada CPU escribe únicamente su propia ranura [cpu_id].
                            let idle_ctx = unsafe { &mut AP_IDLE_CONTEXTS[cpu_id] };
                            AP_IDLE_CONTEXT_VALID[cpu_id].store(true, Ordering::SeqCst);
                            perform_context_switch_to(idle_ctx, next_pid);
                        } else {
                            // cpu_id fuera de rango: fallback al dummy original.
                            let mut dummy = crate::process::Context::new();
                            perform_context_switch_to(&mut dummy, next_pid);
                        }
                    }
                }
            }
        } else {
            // Paso 3: No hay proceso listo. Si el proceso actual está bloqueado
            // debemos cambiar a un contexto idle para no continuar ejecutándolo.
            let is_blocked = current_pid
                .and_then(|pid| get_process(pid))
                .map(|p| p.state == ProcessState::Blocked)
                .unwrap_or(false);

            if is_blocked {
                let blocked_pid = current_pid.unwrap();

                if cpu_id == 0 {
                    // BSP: volver al proceso kernel (PID 0) que actúa como idle.
                    set_current_process(Some(0));
                    if let Some(mut p0) = get_process(0) {
                        p0.state = ProcessState::Running;
                        update_process(0, p0);
                    }
                    perform_context_switch(blocked_pid, 0);
                } else if cpu_id < MAX_CPUS && AP_IDLE_CONTEXT_VALID[cpu_id].load(Ordering::SeqCst) {
                    // AP: restaurar el contexto idle guardado la primera vez que el AP
                    // tomó un proceso usuario. Esto devuelve la ejecución al bucle idle.
                    //
                    // Safety: cada CPU escribe y lee únicamente su propia ranura
                    // (indexada por cpu_id), por lo que no hay carreras entre CPUs.
                    // El from_ptr apunta al contexto en PROCESS_TABLE; aunque el lock se
                    // libera antes de switch_context, ningún otro hilo modificará ese
                    // proceso (está Blocked) antes de que se complete la transferencia.
                    set_current_process(None);
                    let from_ptr = {
                        let mut table = crate::process::PROCESS_TABLE.lock();
                        match table[blocked_pid as usize].as_mut() {
                            Some(p) => &mut p.context as *mut crate::process::Context,
                            None => return,
                        }
                    };
                    let to_ctx = unsafe { &AP_IDLE_CONTEXTS[cpu_id] };
                    unsafe {
                        crate::process::switch_context(&mut *from_ptr, to_ctx, 0);
                    }
                }
                // Si el contexto idle del AP aún no es válido (ap_entry() todavía no
                // completó la primera transferencia a un proceso usuario) o cpu_id está
                // fuera de rango, se retorna sin cambio. La deduplicación en add_sleep
                // impide el bucle de saturación, y el AP entrará en idle normalmente
                // en el próximo tick del APIC timer.
            }
            // Si no está bloqueado (yield normal sin otros procesos), el proceso
            // actual simplemente continúa (se usa como idle implícito).
        }
    });
}

fn perform_context_switch(from_pid: ProcessId, to_pid: ProcessId) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut table = crate::process::PROCESS_TABLE.lock();
        let from_ptr = match table[from_pid as usize].as_mut() {
            Some(p) => &mut p.context as *mut crate::process::Context,
            None => return, // Process exited, skip switch
        };
        let to_exists = table[to_pid as usize].is_some();
        drop(table);
        if !to_exists {
            return;
        }
        let mut stats = SCHEDULER_STATS.lock();
        stats.context_switches += 1;
        drop(stats);
        set_current_process(Some(to_pid));
        perform_context_switch_to(unsafe { &mut *from_ptr }, to_pid);
    });
}

fn perform_context_switch_to(from_ctx: &mut crate::process::Context, to_pid: ProcessId) {
    let (to_ptr, to_kernel_stack, to_page_table, to_fs_base) = {
        let mut table = crate::process::PROCESS_TABLE.lock();
        let to_process = match table[to_pid as usize].as_mut() {
            Some(p) => p,
            None => return,
        };
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
        crate::process::set_current_process(Some(to_pid));
        crate::process::switch_context(from_ctx, &*to_ptr, next_cr3);
    }
}

/// Yield - ceder CPU voluntariamente
pub fn yield_cpu() {
    // Si somos el único proceso listo, evitamos el hot-loop llamando a pause() (Nivel 2)
    // Esto es especialmente útil en spinlocks de espacio de usuario o drivers.
    schedule();
    crate::cpu::pause();
}

/// Dormir el proceso actual (stub - no implementado completamente)
pub fn sleep(_ticks: u64) {
    // TODO: Implementar lista de procesos bloqueados con timer
    yield_cpu();
}

/// Obtener estadísticas del scheduler
pub fn get_stats() -> SchedulerStats {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let stats = SCHEDULER_STATS.lock();
        SchedulerStats {
            context_switches: stats.context_switches,
            total_ticks: stats.total_ticks,
            idle_ticks: stats.idle_ticks,
        }
    })
}

/// Inicializar scheduler
pub fn init() {
    crate::serial::serial_print("Scheduler initialized\n");
    crate::interrupts::unmask_mouse_irq();
}
