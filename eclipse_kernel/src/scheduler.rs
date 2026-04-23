//! Scheduler básico round-robin con soporte SMP completo

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use crate::process::{ProcessId, ProcessState, get_process, update_process, current_process_id, set_current_process, modify_process};
use spin::Mutex;
use alloc::collections::{BTreeMap, VecDeque};

/// Número máximo de CPUs soportadas (debe coincidir con process::MAX_CPUS)
pub const MAX_CPUS: usize = 32;

/// Estructura de cola para una CPU
/// Peso de cada nivel de prioridad (0-9). Basado en la tabla de nice de Linux.
const PRIO_TO_WEIGHT: [u64; 10] = [
    88761, 71755, 56483, 46273, 36291, // 0-4 (Alta prioridad)
    29154, 23254, 18705, 14949, 11916, // 5-9 (Baja prioridad)
];
const NICE_0_LOAD: u64 = 29154; // Peso para prioridad 5 (default)

/// Estructura de cola CFS para una CPU
struct FairRunQueue {
    tasks: Option<BTreeMap<u64, VecDeque<ProcessId>>>,
    min_vruntime: u64,
}

impl FairRunQueue {
    const fn new() -> Self {
        Self {
            tasks: None,
            min_vruntime: 0,
        }
    }

    fn push(&mut self, pid: ProcessId, vruntime: u64) {
        if self.tasks.is_none() {
            self.tasks = Some(BTreeMap::new());
        }
        let tasks = self.tasks.as_mut().unwrap();
        tasks.entry(vruntime).or_insert_with(VecDeque::new).push_back(pid);
        
        // Actualizar min_vruntime si es necesario (el más pequeño de la cola)
        if vruntime < self.min_vruntime || tasks.len() == 1 {
            self.min_vruntime = vruntime;
        }
    }

    fn pop(&mut self) -> Option<ProcessId> {
        let tasks = if let Some(ref mut t) = self.tasks {
            t
        } else {
            return None;
        };

        let mut first_key = None;
        let mut pid = None;
        let mut vruntime_to_set = None;

        if let Some((&vruntime, queue)) = tasks.iter_mut().next() {
            pid = queue.pop_front();
            if queue.is_empty() {
                first_key = Some(vruntime);
            }
            if pid.is_some() {
                vruntime_to_set = Some(vruntime);
            }
        }

        if let Some(v) = vruntime_to_set {
            self.min_vruntime = v;
        }

        if let Some(k) = first_key {
            tasks.remove(&k);
        }

        pid
    }
}

/// Colas de procesos ready (CFS), una por cada CPU.
static READY_QUEUES: [Mutex<FairRunQueue>; MAX_CPUS] =
    [const { Mutex::new(FairRunQueue::new()) }; MAX_CPUS];

/// Contador round-robin para distribuir procesos nuevos entre CPUs.
static NEW_PROC_RR_CPU: AtomicU32 = AtomicU32::new(0);

/// Contador global de procesos en colas de Ready (para optimizar bucles idle).
static RUNNABLE_COUNT: AtomicU32 = AtomicU32::new(0);

/// Versiones de las colas para MWAIT/MONITOR (indican si la cola ha cambiado)
static QUEUE_VERSIONS: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];

pub fn ready_queue_tail_addr() -> u64 {
    let cpu_id = crate::process::get_cpu_id();
    &QUEUE_VERSIONS[cpu_id] as *const _ as u64
}

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

/// Buffer scratch por CPU para context switch de procesos muertos.
///
/// Cuando un proceso termina (exit/kill) su PID slot queda desregistrado ANTES
/// de que la CPU que lo ejecutaba haya hecho el context switch.  perform_context_switch
/// busca el contexto del proceso saliente con pid_to_slot_fast; si devuelve None,
/// usamos este buffer scratch de la CPU para guardar los registros (que son
/// descartados, ya que el proceso está muerto).  Esto evita que el CPU quede
/// atascado ejecutando el `loop {}` del proceso muerto indefinidamente.
///
/// Safety: idéntica a AP_IDLE_CONTEXTS — cada CPU solo escribe su índice propio.
static mut SCRATCH_CONTEXTS: [crate::process::Context; MAX_CPUS] =
    [const { crate::process::Context::new() }; MAX_CPUS];

/// Indica si el contexto idle del AP ya fue guardado al menos una vez.
static AP_IDLE_CONTEXT_VALID: [AtomicBool; MAX_CPUS] =
    [const { AtomicBool::new(false) }; MAX_CPUS];

/// Quantum restante por CPU (en ms/ticks). Se inicializa en 10.
static mut CPU_QUANTUM: [u32; MAX_CPUS] = [10; MAX_CPUS];

/// Quantum inicial asignado al proceso actual (para calcular 'consumed' correctamente).
static mut CPU_INITIAL_QUANTUM: [u32; MAX_CPUS] = [10; MAX_CPUS];

/// Estadísticas del scheduler (atómicas para SMP)
pub struct SchedulerStats {
    pub context_switches: u64,
    pub total_ticks: u64,
    pub idle_ticks: u64,
}

static STATS_CONTEXT_SWITCHES: AtomicU64 = AtomicU64::new(0);
static STATS_TOTAL_TICKS: AtomicU64 = AtomicU64::new(0);
static STATS_IDLE_TICKS: AtomicU64 = AtomicU64::new(0);

/// Per-CPU tick counts for detailed load analysis
static CPU_TOTAL_TICKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static CPU_IDLE_TICKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// Cuántas veces se dio CPU a cada PID en la última ventana (se lee y resetea en el heartbeat).
const MAX_PIDS: usize = 256;
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
///
/// LOCKING: Acquires READY_QUEUE → PROCESS_TABLE (in that order).
/// Callers must NOT hold either lock.  The check-and-set of process state
/// is performed as a single atomic operation while READY_QUEUE is already
/// held, eliminating the TOCTOU race that existed when the two PROCESS_TABLE
/// acquisitions were separate (a second CPU could pass the dedup check before
/// the first one set the state to Ready, inserting the same PID twice).
pub fn enqueue_process(pid: ProcessId) {
    let slot = match crate::ipc::pid_to_slot_fast(pid) {
        Some(s) => s,
        None => return,
    };

    x86_64::instructions::interrupts::without_interrupts(|| {
        let (target_cpu, vruntime) = {
            let mut table = crate::process::PROCESS_TABLE.lock();
            if let Some(p) = table[slot].as_mut() {
                if p.id != pid { return; }
                if p.state == ProcessState::Ready {
                    return; // Already in a queue
                }
                if p.state == ProcessState::Running && p.current_cpu != crate::process::get_cpu_id() as u32 {
                    return; // Running on another CPU
                }
                if p.state == ProcessState::Terminated {
                    return;
                }

                p.state = ProcessState::Ready;
                
                let active_cpus = crate::cpu::get_active_cpu_count();
                let current_cpu = crate::process::get_cpu_id();

                let target = if let Some(aff) = p.cpu_affinity {
                    aff as usize % MAX_CPUS
                } else if p.last_cpu != crate::process::NO_CPU {
                    p.last_cpu as usize % MAX_CPUS
                } else {
                    let next = NEW_PROC_RR_CPU.fetch_add(1, Ordering::Relaxed) as usize;
                    next % active_cpus.max(1)
                };

                // CFS New Task / Wakeup Logic:
                // If the process was sleeping (Blocked), its vruntime might be very old.
                // We bump it to be at least min_vruntime of the target queue to avoid
                // it hogging the CPU for too long, but give it a small bonus for interactivity.
                let mut queue = READY_QUEUES[target].lock();
                if p.vruntime < queue.min_vruntime {
                    // Bonus of 20ms (20 ticks) for waking up
                    p.vruntime = queue.min_vruntime.saturating_sub(20);
                }
                
                (target, p.vruntime)
            } else {
                return;
            }
        };

        let mut queue = READY_QUEUES[target_cpu].lock();
        queue.push(pid, vruntime);
        RUNNABLE_COUNT.fetch_add(1, Ordering::SeqCst);
        QUEUE_VERSIONS[target_cpu].fetch_add(1, Ordering::SeqCst);

        if target_cpu != crate::process::get_cpu_id() {
            let apic_ids = crate::acpi::get_info().apic_ids;
            if target_cpu < apic_ids.len() {
                let target_apic_id = apic_ids[target_cpu];
                crate::apic::send_reschedule_ipi(target_apic_id);
            }
        }
    });
}

fn dequeue_for_cpu(cpu_id: usize) -> Option<ProcessId> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        // 1. Local queue
        {
            let mut local_q = READY_QUEUES[cpu_id].lock();
            if let Some(pid) = local_q.pop() {
                RUNNABLE_COUNT.fetch_sub(1, Ordering::SeqCst);
                return Some(pid);
            }
        }

        // 2. Work Stealing
        let active_cpus = crate::cpu::get_active_cpu_count();
        if active_cpus <= 1 {
            return None;
        }

        let steal_limit = core::cmp::min(active_cpus, 8); 

        for offset in 1..steal_limit {
            let victim_cpu = (cpu_id + offset) % active_cpus;
            let mut victim_q = READY_QUEUES[victim_cpu].lock();
            
            // Peek at the first task in the victim's BTreeMap
            let victim_pid = if let Some(tasks) = victim_q.tasks.as_ref() {
                tasks.values().next().and_then(|q| q.front().cloned())
            } else {
                None
            };

            if let Some(pid) = victim_pid {
                let can_steal = if let Some(table) = crate::process::PROCESS_TABLE.try_lock() {
                    if let Some(slot) = crate::ipc::pid_to_slot_fast(pid) {
                        if let Some(p) = table[slot].as_ref() {
                            p.id == pid && p.cpu_affinity.is_none()
                        } else { false }
                    } else { false }
                } else {
                    false 
                };

                if can_steal {
                    let pid = victim_q.pop();
                    if pid.is_some() {
                        RUNNABLE_COUNT.fetch_sub(1, Ordering::SeqCst);
                    }
                    return pid;
                }
            }
        }
        None
    })
}
pub fn has_runnable_threads_local() -> bool {
    // Check ONLY the current CPU's ready queue.
    // This allows other idle cores to stay in HLT/MWAIT even if one core is busy.
    let cpu_id = crate::process::get_cpu_id();
    if cpu_id >= MAX_CPUS { return false; }
    
    let queue = READY_QUEUES[cpu_id].lock();
    queue.tasks.as_ref().map_or(false, |t| !t.is_empty())
}

/// Indica si hay ALGÚN proceso listo en alguna cola del sistema.
pub fn has_runnable_threads() -> bool {
    RUNNABLE_COUNT.load(Ordering::SeqCst) > 0
}


pub fn ready_queue_min_vruntime() -> u64 {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let cpu_id = crate::process::get_cpu_id();
        let q = READY_QUEUES[cpu_id].lock();
        q.min_vruntime
    })
}

/// Tick del scheduler (llamado desde timer interrupt - solo en el BSP)
pub fn tick() {
    // Solo tareas globales: despertar procesos que duermen.
    // Usamos el contador global de tiempo (uptime real ms) para el timeout.
    let global_ticks = crate::interrupts::ticks();
    wake_sleeping_processes(global_ticks);
}

/// Tick local por CPU (manejado por el timer de cada LAPIC).
/// Implementa el quantum de 10ms para el scheduler.
pub fn local_tick() {
    let cpu_id = crate::process::get_cpu_id();
    if cpu_id >= MAX_CPUS { return; }

    // Actualizar estadísticas de este CPU (atómico - sin lock global de stats)
    STATS_TOTAL_TICKS.fetch_add(1, Ordering::Relaxed);
    CPU_TOTAL_TICKS[cpu_id].fetch_add(1, Ordering::Relaxed);
    
    let current_pid = crate::process::current_process_id();
    // PID 0 actúa como idle del BSP, pero los APs usan un idle propio
    // guardando `set_current_process(None)`. Para que el % CPU sea correcto,
    // contamos ambos como "idle".
    if current_pid == Some(0) || current_pid.is_none() {
        STATS_IDLE_TICKS.fetch_add(1, Ordering::Relaxed);
        CPU_IDLE_TICKS[cpu_id].fetch_add(1, Ordering::Relaxed);
        // Note: p.cpu_ticks is now updated in schedule() to avoid global lock contention here.
    }

    unsafe {
        if CPU_QUANTUM[cpu_id] > 0 {
            CPU_QUANTUM[cpu_id] -= 1;
        }

        if CPU_QUANTUM[cpu_id] == 0 {
            schedule();
        }
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
pub fn schedule() -> u64 {
    let mut sleep_duration = 0;

    x86_64::instructions::interrupts::without_interrupts(|| {
        let cpu_id = crate::process::get_cpu_id();
        let current_pid = current_process_id();
        let mut should_requeue = false;
        
        // Paso 1: Si hay un proceso usuario en ejecución (Running), preemptarlo
        // y actualizar su vruntime antes de meterlo en la cola ready.
        if let Some(pid) = current_pid {
            if pid != 0 {
                {
                    let mut table = crate::process::PROCESS_TABLE.lock();
                    if let Some(slot) = crate::ipc::pid_to_slot_fast(pid) {
                        if let Some(process) = table[slot].as_mut() {
                            if process.id == pid && process.current_cpu == cpu_id as u32 {
                                // CFS vruntime update
                                let consumed = unsafe { CPU_INITIAL_QUANTUM[cpu_id].saturating_sub(CPU_QUANTUM[cpu_id]) };
                                process.cpu_ticks += consumed as u64;

                                // vruntime += consumed * (NICE_0_LOAD / weight)
                                let delta_vruntime = (consumed as u64 * NICE_0_LOAD) / process.weight.max(1);
                                process.vruntime += delta_vruntime;
                                
                                // Update AI profile burst duration
                                process.ai_profile.update_burst(consumed as u64);

                                // Reset quantum based on AI and priority
                                let base_q = process.ai_profile.predict_burst().max(10).min(50) as u32;
                                // Give more quantum to high priority tasks
                                let next_q = (base_q as u64 * process.weight / NICE_0_LOAD).max(5).min(100) as u32;

                                unsafe { 
                                    CPU_QUANTUM[cpu_id] = next_q;
                                    CPU_INITIAL_QUANTUM[cpu_id] = next_q;
                                }
                                
                                if process.state == ProcessState::Running {
                                    should_requeue = true;
                                }
                            }
                        }
                    }
                }

                if should_requeue {
                    enqueue_process(pid);
                }
            }
        }

        // Paso 2: Obtener el siguiente proceso de la cola CFS.
        if let Some(next_pid) = dequeue_for_cpu(cpu_id) {
            if (next_pid as usize) < MAX_PIDS {
                RUN_COUNTS[next_pid as usize].fetch_add(1, Ordering::Relaxed);
            }
            
            let mut should_requeue_pid = None;
            let next_process_exists = {
                let mut table = crate::process::PROCESS_TABLE.lock();
                if let Some(slot) = crate::ipc::pid_to_slot_fast(next_pid) {
                    if let Some(next_process) = table[slot].as_mut() {
                        if next_process.id != next_pid {
                            false
                        } else if next_process.current_cpu != crate::process::NO_CPU && next_process.current_cpu != cpu_id as u32 {
                            should_requeue_pid = Some(next_pid);
                            false
                        } else if next_process.state == ProcessState::Terminated {
                            false
                        } else {
                            next_process.state = ProcessState::Running;
                            next_process.current_cpu = cpu_id as u32;
                            next_process.last_cpu = cpu_id as u32; 
                            true
                        }
                    } else {
                        false
                    }
                } else {
                    false
                }
            };

            if let Some(requeue_pid) = should_requeue_pid {
                enqueue_process(requeue_pid);
            }

            if next_process_exists {
                match current_pid {
                    Some(cur) if cur == next_pid => {
                        // Mismo proceso
                        let next_q = {
                            let table = crate::process::PROCESS_TABLE.lock();
                            if let Some(slot) = crate::ipc::pid_to_slot_fast(next_pid) {
                                table[slot].as_ref().map(|p| p.ai_profile.predict_burst()).unwrap_or(10)
                            } else { 10 }
                        }.max(10).min(50) as u32;

                        unsafe { 
                            crate::scheduler::CPU_QUANTUM[cpu_id] = next_q; 
                            crate::scheduler::CPU_INITIAL_QUANTUM[cpu_id] = next_q;
                        }
                    }
                    Some(cur) => {
                        // Cambio de contexto
                        let next_q = {
                            let table = crate::process::PROCESS_TABLE.lock();
                            if let Some(slot) = crate::ipc::pid_to_slot_fast(next_pid) {
                                table[slot].as_ref().map(|p| p.ai_profile.predict_burst()).unwrap_or(10)
                            } else { 10 }
                        }.max(5).min(50) as u32;

                        unsafe { 
                            crate::scheduler::CPU_QUANTUM[cpu_id] = next_q; 
                            crate::scheduler::CPU_INITIAL_QUANTUM[cpu_id] = next_q;
                        }
                        perform_context_switch(cur, next_pid);
                    }
                    None => {
                        // Transición desde idle
                        let next_q = {
                            let table = crate::process::PROCESS_TABLE.lock();
                            if let Some(slot) = crate::ipc::pid_to_slot_fast(next_pid) {
                                table[slot].as_ref().map(|p| p.ai_profile.predict_burst()).unwrap_or(10)
                            } else { 10 }
                        }.max(10).min(50) as u32;

                        unsafe { 
                            crate::scheduler::CPU_QUANTUM[cpu_id] = next_q; 
                            crate::scheduler::CPU_INITIAL_QUANTUM[cpu_id] = next_q;
                        }
                        
                        if cpu_id < MAX_CPUS {
                            x86_64::instructions::interrupts::without_interrupts(|| {
                                let idle_ctx = unsafe { &mut AP_IDLE_CONTEXTS[cpu_id] };
                                AP_IDLE_CONTEXT_VALID[cpu_id].store(true, Ordering::SeqCst);
                                perform_context_switch_to(idle_ctx, next_pid);
                            });
                        } else {
                            x86_64::instructions::interrupts::without_interrupts(|| {
                                let mut dummy = crate::process::Context::new();
                                perform_context_switch_to(&mut dummy, next_pid);
                            });
                        }
                    }
                }
            }
        } else {
            // Paso 3: No hay procesos listos.
            if let Some(pid) = current_pid {
                if pid != 0 {
                    if cpu_id == 0 {
                        // BSP: volver al proceso kernel (PID 0)
                        x86_64::instructions::interrupts::without_interrupts(|| {
                            {
                                let mut table = crate::process::PROCESS_TABLE.lock();
                                if let Some(p0) = table[0].as_mut() {
                                    p0.state = ProcessState::Running;
                                    p0.current_cpu = 0;
                                }
                            }
                            perform_context_switch(pid, 0);
                        });
                    } else if cpu_id < MAX_CPUS && AP_IDLE_CONTEXT_VALID[cpu_id].load(Ordering::SeqCst) {
                        // AP: restaurar el contexto idle
                        x86_64::instructions::interrupts::without_interrupts(|| {
                            set_current_process(None);
                            let (from_ptr, clear_ptr): (*mut crate::process::Context, u64) = {
                                let mut table = crate::process::PROCESS_TABLE.lock();
                                match crate::ipc::pid_to_slot_fast(pid) {
                                    Some(slot) => {
                                        match table[slot].as_mut() {
                                            Some(p) if p.id == pid => {
                                                let ctx_ptr = &mut p.context as *mut crate::process::Context;
                                                let cpu_ptr = &mut p.current_cpu as *mut u32 as u64;
                                                (ctx_ptr, cpu_ptr)
                                            },
                                            _ => (unsafe { &mut SCRATCH_CONTEXTS[cpu_id] as *mut crate::process::Context }, 0),
                                        }
                                    },
                                    None => (unsafe { &mut SCRATCH_CONTEXTS[cpu_id] as *mut crate::process::Context }, 0),
                                }
                            };
                            let to_ctx = unsafe { &AP_IDLE_CONTEXTS[cpu_id] };
                            unsafe {
                                crate::process::switch_context(&mut *from_ptr, to_ctx, 0, clear_ptr);
                            }
                        });
                    }
                }
            }
            
            let current_tick = crate::interrupts::ticks();
            let mut min_wake = None;
            {
                let q = SLEEP_QUEUE.lock();
                for entry in q.iter() {
                    if entry.valid {
                        if min_wake.is_none() || entry.wake_tick < min_wake.unwrap() {
                            min_wake = Some(entry.wake_tick);
                        }
                    }
                }
            }

            sleep_duration = match min_wake {
                Some(tick) if tick > current_tick => {
                    (tick - current_tick).min(50).max(1)
                },
                Some(_) => 0,
                None => 10,
            };
        }
        sleep_duration
    })
}


fn perform_context_switch(from_pid: ProcessId, to_pid: ProcessId) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let cpu_id = crate::process::get_cpu_id();
        let mut table = crate::process::PROCESS_TABLE.lock();
        // Use pid_to_slot_fast: PIDs may exceed 63 after slot reuse.
        //
        // Si el slot del proceso saliente no se encuentra (fue desregistrado porque el
        // proceso llamó exit() o fue matado por sys_kill antes de que se hiciera el
        // context switch), usamos el buffer SCRATCH_CONTEXTS de esta CPU para guardar
        // los registros actuales.  Los datos guardados se descartan — el proceso ya
        // está muerto — pero esto permite que el context switch continúe y que la CPU
        // empiece a ejecutar el proceso siguiente en lugar de quedar atascada en el
        // `loop {}` del proceso muerto.
        let from_ptr: *mut crate::process::Context = match crate::ipc::pid_to_slot_fast(from_pid) {
            Some(slot) => {
                match table[slot].as_mut() {
                    Some(p) if p.id == from_pid => &mut p.context as *mut crate::process::Context,
                    _ => {
                        if cpu_id < MAX_CPUS {
                            unsafe { &mut SCRATCH_CONTEXTS[cpu_id] as *mut crate::process::Context }
                        } else {
                            return;
                        }
                    }
                }
            }
            None => {
                // PID slot unregistered: process already exited or was killed.
                // Use per-CPU scratch buffer so the switch can still proceed.
                if cpu_id < MAX_CPUS {
                    unsafe { &mut SCRATCH_CONTEXTS[cpu_id] as *mut crate::process::Context }
                } else {
                    return;
                }
            }
        };
        let to_slot = match crate::ipc::pid_to_slot_fast(to_pid) {
            Some(s) => s,
            None => return,
        };
        let to_exists = table[to_slot].as_ref().map_or(false, |p| p.id == to_pid);
        drop(table);
        if !to_exists {
            return;
        }
        STATS_CONTEXT_SWITCHES.fetch_add(1, Ordering::Relaxed);
        // Note: set_current_process is called inside perform_context_switch_to,
        // AFTER computing the from-process ownership pointer, so current_process_id()
        // still returns from_pid when clear_addr is calculated.
        perform_context_switch_to(unsafe { &mut *from_ptr }, to_pid);
    });
}

fn perform_context_switch_to(from_ctx: &mut crate::process::Context, to_pid: ProcessId) {
    let (to_ptr, to_kernel_stack, to_page_table, to_fs_base) = {
        let mut table = crate::process::PROCESS_TABLE.lock();
        // Use pid_to_slot_fast: to_pid may exceed 63 after slot reuse.
        let to_slot = match crate::ipc::pid_to_slot_fast(to_pid) {
            Some(s) => s,
            None => return,
        };
        let to_process = match table[to_slot].as_mut() {
            Some(p) if p.id == to_pid => p,
            _ => return,
        };
        // If to_pid was killed (via sys_kill) between when it was dequeued and now,
        // switching to it would resume a terminated process.  Bail out — the scheduler
        // will be called again on the next timer tick and will skip the terminated PID.
        if to_process.state == ProcessState::Terminated {
            return;
        }
        let to_ctx_ptr = &to_process.context as *const crate::process::Context;
        let to_kernel_stack = to_process.kernel_stack_top;
        let to_page_table = to_process.resources.lock().page_table_phys;
        let to_fs_base = to_process.fs_base;
        
        (to_ctx_ptr, to_kernel_stack, to_page_table, to_fs_base)
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
    let next_cr3 = {
        let current_cr3 = crate::memory::get_cr3();
        if to_page_table != 0 && to_page_table != current_cr3 {
            to_page_table
        } else {
            0
        }
    };
    
    // Obtain the address of from_pid.current_cpu BEFORE updating current_process_id,
    // so current_process_id() still returns from_pid here. switch_context will atomically
    // write NO_CPU to this address right after saving from's context, eliminating the race
    // between clearing ownership and saving the context.
    let clear_addr: u64 = if let Some(from_pid) = current_process_id() {
        if from_pid != to_pid {
            // Use pid_to_slot_fast: from_pid may exceed 63 after slot reuse.
            if let Some(slot) = crate::ipc::pid_to_slot_fast(from_pid) {
                let table = crate::process::PROCESS_TABLE.lock();
                if let Some(p) = table[slot].as_ref() {
                    if p.id == from_pid && p.current_cpu == crate::process::get_cpu_id() as u32 {
                        &p.current_cpu as *const u32 as u64
                    } else {
                        0
                    }
                } else {
                    0
                }
            } else {
                0
            }
        } else {
            0
        }
    } else {
        0
    };

    // Now commit to the new process: update per-CPU current_pid, then perform the raw
    // context switch. switch_context atomically writes NO_CPU to clear_addr immediately
    // after saving the from-context, before restoring the to-context.
    unsafe {
        crate::process::set_current_process(Some(to_pid));
        if to_pid != 0 {
            //crate::serial::serial_printf(format_args!("[SCHED] C{} switching to PID {}\n", crate::process::get_cpu_id(), to_pid));
        }
        crate::process::switch_context(from_ctx, &*to_ptr, next_cr3, clear_addr);
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
    SchedulerStats {
        context_switches: STATS_CONTEXT_SWITCHES.load(Ordering::Relaxed),
        total_ticks: STATS_TOTAL_TICKS.load(Ordering::Relaxed),
        idle_ticks: STATS_IDLE_TICKS.load(Ordering::Relaxed),
    }
}

/// Returns (total_ticks, idle_ticks) for a specific CPU
pub fn get_cpu_ticks(cpu_id: usize) -> (u64, u64) {
    if cpu_id >= MAX_CPUS { return (0, 0); }
    (
        CPU_TOTAL_TICKS[cpu_id].load(Ordering::Relaxed),
        CPU_IDLE_TICKS[cpu_id].load(Ordering::Relaxed),
    )
}

/// Inicializar scheduler
pub fn init() {
    crate::serial::serial_print("Scheduler initialized\n");
    crate::interrupts::unmask_mouse_irq();
}
