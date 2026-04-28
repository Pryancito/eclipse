//! Gestión de procesos y context switching

use core::arch::asm;
use spin::Mutex;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use crate::vm_object::{VMObject, VMObjectType};

/// ID de proceso
pub type ProcessId = u32;

// argv pendiente (syscalls 542/543): el padre registra bytes NUL-separados antes de
// encolar al hijo. No se consume al leer (varios lectores / crt + libc); se libera
// en `exit_process` para el PID que termina.
static PENDING_PROCESS_ARGS: Mutex<BTreeMap<ProcessId, Vec<u8>>> = Mutex::new(BTreeMap::new());
/// Registro global de "cumpleaños" por usuario (UID -> ticks de su primera aparición).
pub static USER_BIRTHDAYS: Mutex<BTreeMap<u32, u64>> = Mutex::new(BTreeMap::new());

/// Obtiene el cumpleaños de un usuario. Si es la primera vez que se ve, se registra ahora.
pub fn get_user_birthday(uid: u32) -> u64 {
    let mut map = USER_BIRTHDAYS.lock();
    if let Some(&ticks) = map.get(&uid) {
        ticks
    } else {
        let ticks = crate::interrupts::ticks();
        map.insert(uid, ticks);
        ticks
    }
}


/// syscall 542: guardar argv del hijo (reemplaza entrada previa si existía).
pub fn set_pending_process_args(pid: ProcessId, data: Vec<u8>) {
    PENDING_PROCESS_ARGS.lock().insert(pid, data);
}

/// syscall 543: copiar argv NUL-separado al buffer; devuelve bytes copiados (≤ buf.len).
/// No elimina la entrada (lecturas idempotentes).
pub fn copy_pending_process_args(pid: ProcessId, buf: &mut [u8]) -> usize {
    let map = PENDING_PROCESS_ARGS.lock();
    if let Some(args) = map.get(&pid) {
        let n = args.len().min(buf.len());
        buf[..n].copy_from_slice(&args[..n]);
        n
    } else {
        0
    }
}

pub fn clear_pending_process_args(pid: ProcessId) {
    PENDING_PROCESS_ARGS.lock().remove(&pid);
}
pub const KERNEL_STACK_SIZE: usize = 32768; // 32KB stack for kernel operations

/// Estado de un proceso
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ProcessState {
    Ready,
    Running,
    Blocked,
    Terminated,
    Stopped, // For Job Control (SIGSTOP/SIGTSTP)
    WaitingForChild,
}

/// Parámetros para el planificador de Tiempo Real (EDF)
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct RTParams {
    /// Tiempo de ejecución reservado por periodo (en ticks/ms)
    pub runtime: u64,
    /// Plazo relativo desde el inicio del periodo (en ticks/ms)
    pub deadline: u64,
    /// Duración del periodo (en ticks/ms)
    pub period: u64,
    /// Deadline absoluto actual (timestamp en ticks)
    pub next_deadline: u64,
}

/// Virtual Memory Area (VMA) region
#[derive(Clone, Debug)]
pub struct VMARegion {
    pub start: u64,
    pub end: u64,
    pub flags: u64,
    pub object: Arc<Mutex<VMObject>>,
    pub offset: u64,
    pub is_huge: bool,
    pub is_shared: bool,
}

impl VMARegion {
    /// Returns true if other is immediately adjacent to this VMA and has identical properties.
    pub fn can_merge(&self, other: &Self) -> bool {
        self.end == other.start 
            && self.flags == other.flags 
            && self.is_huge == other.is_huge
            && self.is_shared == other.is_shared
            && self.offset + (self.end - self.start) == other.offset
            && Arc::ptr_eq(&self.object, &other.object)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Context {
    pub rax: u64, pub rbx: u64, pub rcx: u64, pub rdx: u64,
    pub rsi: u64, pub rdi: u64, pub rbp: u64,
    pub r8: u64,  pub r9: u64,  pub r10: u64, pub r11: u64,
    pub r12: u64, pub r13: u64, pub r14: u64, pub r15: u64,
    pub rsp: u64,
    pub rip: u64,
    pub rflags: u64,
    pub fs_base: u64,
    pub gs_base: u64,
}

/// Recursos compartidos por todos los hilos de un proceso (Address Space, FDs)
pub struct ProcessResources {
    pub page_table_phys: u64,
    pub vmas: Vec<VMARegion>,
    pub brk_current: u64,
    pub fd_table_idx: usize,
}

impl ProcessResources {
    pub fn new(page_table_phys: u64, fd_table_idx: usize) -> Self {
        Self {
            page_table_phys,
            vmas: Vec::new(),
            brk_current: 0,
            fd_table_idx,
        }
    }
}

impl Drop for ProcessResources {
    fn drop(&mut self) {
        crate::memory::teardown_process_paging(self.page_table_phys);
    }
}

/// Contenedor de proceso (estilo FreeBSD `struct proc`)
pub struct Proc {
    pub id: ProcessId,
    pub parent_pid: Option<ProcessId>,
    pub resources: Arc<Mutex<ProcessResources>>,
    pub name: [u8; 16],
    pub signal_actions: [SignalAction; 64],
    pub uid: u32,
    pub gid: u32,
    pub euid: u32,
    pub egid: u32,
    pub suid: u32,
    pub sgid: u32,
    pub exit_code: i32,
    pub exit_signal: i32,
    pub notified_stopped: bool,
    pub notified_continued: bool,
    pub is_linux: bool,
    pub syscall_trace: bool,
    pub mem_frames: u64,
    pub vfork_waiting_for_child: Option<ProcessId>,
    pub vfork_shared_mm_with_parent: Option<ProcessId>,
    pub cwd: [u8; 128],
    pub cwd_len: usize,
    pub pgid: ProcessId,
    pub sid: ProcessId,
    pub dynamic_linker_aux: Option<(u64, u64)>,
    pub umask: u32,
    pub supplementary_groups: [u32; 32],
    pub supplementary_groups_len: usize,
}

/// Hilo de ejecución (estilo FreeBSD `struct thread`)
#[derive(Clone)]
pub struct Process {
    pub id: ProcessId,           // TID
    pub tgid: ProcessId,         // PID del proceso (Thread Group ID)
    pub state: ProcessState,
    pub context: Context,
    pub stack_base: u64,
    pub stack_size: usize,
    pub kernel_stack_top: u64,
    pub current_cpu: u32,
    pub last_cpu: u32,
    pub time_slice: u32,
    pub priority: u8,
    pub rt_params: Option<RTParams>,
    pub ai_profile: crate::ai_core::ProcessProfile,
    pub kernel_stack: Option<Vec<u8>>,
    pub signal_mask: u64,
    pub pending_signals: u64,
    pub sigaltstack: Sigaltstack,
    pub cpu_affinity: Option<u32>,
    pub fs_base: u64,
    pub gs_base: u64,
    pub proc: Arc<Mutex<Proc>>, // Referencia al contenedor de proceso
    pub vruntime: u64,
    pub weight: u64,
    pub wake_tick: u64,
    pub cpu_ticks: u64,
    pub clear_child_tid: u64,
    pub set_child_tid: u64,
    pub start_time: u64,
}

impl Process {
    pub fn new(id: ProcessId, proc: Arc<Mutex<Proc>>) -> Self {
        Process {
            id,
            tgid: id,
            state: ProcessState::Blocked,
            context: Context::new(),
            stack_base: 0,
            stack_size: 0,
            kernel_stack_top: 0,
            current_cpu: NO_CPU,
            last_cpu: NO_CPU,
            time_slice: 10,
            priority: 10,
            rt_params: None,
            ai_profile: crate::ai_core::ProcessProfile::new(),
            kernel_stack: None,
            signal_mask: 0,
            pending_signals: 0,
            sigaltstack: Sigaltstack { ss_sp: 0, ss_flags: SS_DISABLE as i32, ss_size: 0 },
            cpu_affinity: None,
            fs_base: 0,
            gs_base: 0,
            proc,
            vruntime: 0,
            weight: 1024,
            wake_tick: 0,
            cpu_ticks: 0,
            clear_child_tid: 0,
            set_child_tid: 0,
            start_time: crate::interrupts::ticks(),
        }
    }

    pub fn get_uid(&self) -> u32 { self.proc.lock().uid }
    pub fn get_gid(&self) -> u32 { self.proc.lock().gid }
    pub fn get_euid(&self) -> u32 { self.proc.lock().euid }
    pub fn get_egid(&self) -> u32 { self.proc.lock().egid }
    pub fn get_pgid(&self) -> ProcessId { self.proc.lock().pgid }
    pub fn get_sid(&self) -> ProcessId { self.proc.lock().sid }

    pub fn set_pending_signal(&mut self, sig: u32) {
        if sig > 0 && sig <= 64 {
            self.pending_signals |= 1 << (sig - 1);
        }
    }
}

impl Context {
    pub const fn new() -> Self {
        Self {
            rax: 0, rbx: 0, rcx: 0, rdx: 0, rsi: 0, rdi: 0, rbp: 0,
            r8: 0, r9: 0, r10: 0, r11: 0, r12: 0, r13: 0, r14: 0, r15: 0,
            rsp: 0,
            rip: 0,
            rflags: 0x002, // IF disabled by default
            fs_base: 0,
            gs_base: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Signal infrastructure — POSIX/Linux ABI
// ---------------------------------------------------------------------------

/// SA_* flags (Linux x86-64 compatible).
pub const SA_NOCLDSTOP:  u64 = 1;
pub const SA_NOCLDWAIT:  u64 = 2;
pub const SA_SIGINFO:    u64 = 4;
pub const SA_RESTORER:   u64 = 0x0400_0000;
pub const SA_ONSTACK:    u64 = 0x0800_0000;
pub const SA_RESTART:    u64 = 0x1000_0000;
pub const SA_NODEFER:    u64 = 0x4000_0000;
pub const SA_RESETHAND:  u64 = 0x8000_0000;

/// SS_* flags for `sigaltstack`.
pub const SS_ONSTACK: i32 = 1;
pub const SS_DISABLE: i32 = 2;

/// Complete signal action — mirrors Linux `struct sigaction` for x86-64.
#[derive(Clone, Copy)]
pub struct SignalAction {
    pub handler:  u64,
    pub flags:    u64,
    pub restorer: u64,
    pub mask:     u64,
}

impl SignalAction {
    pub const fn new() -> Self {
        Self { handler: 0, flags: 0, restorer: 0, mask: 0 }
    }
}

impl Default for SignalAction {
    fn default() -> Self { Self::new() }
}

/// Alternate signal stack — mirrors Linux `stack_t`.
#[derive(Clone, Copy)]
pub struct Sigaltstack {
    pub ss_sp:    u64,
    pub ss_flags: i32,
    pub ss_size:  u64,
}

impl Sigaltstack {
    pub const fn new() -> Self {
        Self { ss_sp: 0, ss_flags: SS_DISABLE as i32, ss_size: 0 }
    }
}

/// Despertar al padre que hizo `vfork`/`clone(CLONE_VFORK|CLONE_VM)` esperando a este hijo.
/// Linux: el padre sale de `clone` cuando el hijo hace `execve` exitoso o `_exit`.
/// Antes de cargar un ELF en `exec*`, si el proceso es hijo vfork con `CLONE_VM`,
/// duplicar la tabla de páginas y la tabla de FDs del padre para no pisar la imagen del padre.
pub fn vfork_detach_mm_for_exec_if_needed(pid: ProcessId) -> Result<(), &'static str> {
    let p = get_process(pid).ok_or("vfork detach: no process")?;
    let parent_pid = {
        let mut proc = p.proc.lock();
        let parent_pid = proc.vfork_shared_mm_with_parent.take();
        if parent_pid.is_none() {
            return Ok(());
        }
        parent_pid.unwrap()
    };

    let child_slot = crate::ipc::pid_to_slot_fast(pid).ok_or("vfork detach: no slot")?;
    
    // Create NEW address space for child (detaching from parent)
    let new_cr3 = crate::memory::create_process_paging();
    
    // Copy parent's FDs into a new table (detaching from parent's shared table)
    crate::fd::fd_clone_for_fork(parent_pid, pid);

    {
        let process = get_process(pid).ok_or("vfork detach: child lost")?;
        let proc = process.proc.lock();
        let mut r = proc.resources.lock();
        r.page_table_phys = new_cr3;
        r.fd_table_idx = child_slot;
    }

    unsafe {
        crate::memory::set_cr3(new_cr3);
    }
    x86_64::instructions::tlb::flush_all();

    Ok(())
}

pub fn vfork_wake_parent_waiting_for_child(child_pid: ProcessId) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut table = PROCESS_TABLE.lock();
        for slot in table.iter_mut() {
            if let Some(p) = slot {
                let mut proc = p.proc.lock();
                if proc.vfork_waiting_for_child == Some(child_pid) {
                    proc.vfork_waiting_for_child = None;
                    return;
                }
            }
        }
    });
}


/// Sentinel value for current_cpu meaning "not owned by any CPU"
pub const NO_CPU: u32 = u32::MAX;

/// Tabla de procesos
pub const MAX_PROCESSES: usize = 256;
pub static PROCESS_TABLE: Mutex<[Option<Process>; MAX_PROCESSES]> = Mutex::new([const { None }; MAX_PROCESSES]);
static NEXT_PID: Mutex<ProcessId> = Mutex::new(1);

/// Tabla de procesos bloqueados en sys_wait() esperando que algún hijo termine.
/// Cada entrada es el PID del proceso padre bloqueado.
/// Cuando un proceso termina, se desbloquea el padre (si está en esta tabla).
pub static CHILD_WAIT_WAITERS: Mutex<[Option<ProcessId>; MAX_PROCESSES]> =
    Mutex::new([None; MAX_PROCESSES]);

/// Registra un proceso padre como bloqueado esperando a un hijo.
/// Idempotente: si `parent_pid` ya está en la tabla, no duplica entradas.
pub fn register_child_waiter(parent_pid: ProcessId) {
    let mut waiters = CHILD_WAIT_WAITERS.lock();
    if waiters.iter().any(|s| *s == Some(parent_pid)) {
        return;
    }
    for slot in waiters.iter_mut() {
        if slot.is_none() {
            *slot = Some(parent_pid);
            return;
        }
    }
}

/// Elimina todas las entradas de `parent_pid` en la tabla de espera de hijos.
pub fn unregister_child_waiter(parent_pid: ProcessId) {
    let mut waiters = CHILD_WAIT_WAITERS.lock();
    for slot in waiters.iter_mut() {
        if *slot == Some(parent_pid) {
            *slot = None;
        }
    }
}

/// Despierta al proceso `parent_pid` si está en la cola de wait de hijos.
/// Llamado por exit_process() / sys_kill cuando un hijo termina.
///
/// Importante: **no** quitamos aquí a `parent_pid` de `CHILD_WAIT_WAITERS`.
/// Si lo hiciéramos mientras el padre sigue `Running` (ventana entre
/// `register_child_waiter` y marcar `Blocked`), el padre podía bloquearse después
/// sin que volviera a llegar ningún wake → shell colgado tras cerrar glxgears.
/// La entrada se limpia solo con `unregister_child_waiter` al salir de `sys_wait`.
pub fn wake_parent_from_wait(parent_pid: ProcessId) {
    let waiting = {
        let waiters = CHILD_WAIT_WAITERS.lock();
        waiters.iter().any(|s| *s == Some(parent_pid))
    };
    if !waiting {
        return;
    }

    crate::scheduler::enqueue_process(parent_pid);
}

/// Máximo número de CPUs soportadas
const MAX_CPUS: usize = 32;
/// Proceso actual por cada CPU

/// Obtener ID de la CPU actual (O(1) vía GS segment)
pub fn get_cpu_id() -> usize {
    crate::boot::get_cpu_id_gs()
}

pub fn next_pid() -> ProcessId {
    let mut next_pid = NEXT_PID.lock();
    let mut pid = *next_pid;
    // PID 0 está reservado para el proceso kernel. Si por cualquier
    // corrupción/condición inicial el contador cae en 0, forzamos
    // a arrancar desde 1 para que el scheduler/CR3 no se queden en
    // el espacio de direcciones del kernel.
    if pid == 0 {
        pid = 1;
        *next_pid = 2;
        return pid;
    }

    *next_pid = pid.wrapping_add(1);
    pid
}


// Inicializar el proceso kernel (PID 0)
pub fn init_kernel_process() {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut table = PROCESS_TABLE.lock();
        
        let kernel_stack_size = KERNEL_STACK_SIZE;
        let kernel_stack = alloc::vec![0u8; kernel_stack_size];
        let kernel_stack_top = kernel_stack.as_ptr() as u64 + kernel_stack_size as u64;
        core::mem::forget(kernel_stack);
        
        let kernel_stack_top_aligned = kernel_stack_top & !0xF;

        let cr3 = crate::memory::get_cr3();
        let resources = Arc::new(Mutex::new(ProcessResources::new(cr3, 0)));
        let proc_obj = Arc::new(Mutex::new(Proc {
            id: 0,
            parent_pid: None,
            resources,
            name: *b"kernel\0\0\0\0\0\0\0\0\0\0",
            signal_actions: [SignalAction::default(); 64],
            uid: 0, gid: 0, euid: 0, egid: 0, suid: 0, sgid: 0,
            exit_code: 0, exit_signal: 0,
            notified_stopped: false, notified_continued: false,
            is_linux: true,
            syscall_trace: false,
            mem_frames: 0,
            vfork_waiting_for_child: None,
            vfork_shared_mm_with_parent: None,
            cwd: {
                let mut buf = [0u8; 128];
                buf[0] = b'/';
                buf
            },
            cwd_len: 1,
            pgid: 0,
            sid: 0,
            dynamic_linker_aux: None,
            umask: 0o022,
            supplementary_groups: [0; 32],
            supplementary_groups_len: 0,
        }));

        let mut process = Process::new(0, proc_obj);
        process.state = ProcessState::Running;
        process.current_cpu = 0;
        process.last_cpu = 0;
        process.priority = 0;
        process.time_slice = 10;
        process.kernel_stack_top = kernel_stack_top_aligned;
        
        table[0] = Some(process);
        drop(table);
        
        // Register PID 0 in the inverse slot map so that pid_to_slot_fast(0) returns
        // Some(0) immediately without falling back to the O(N) PROCESS_TABLE scan.
        // Without this, perform_context_switch(0, X) deadlocks on single-CPU systems:
        // it holds PROCESS_TABLE.lock() and then the O(N) fallback tries to acquire it
        // again, spinning forever.
        crate::ipc::register_pid_slot(0, 0);
        set_current_process(Some(0));
    });
}

/// Crear un nuevo proceso (bajo nivel). phdr_va/phnum/phentsize for auxv (AT_PHDR/AT_PHNUM/AT_PHENT).
pub fn create_process(entry_point: u64, stack_base: u64, stack_size: usize, phdr_va: u64, phnum: u64, phentsize: u64, initial_brk: u64, tls_base: u64) -> Option<ProcessId> {
    let pid = next_pid();
    let cr3 = crate::memory::create_process_paging();
    
    if create_process_with_pid(pid, cr3, entry_point, stack_base, stack_size, phdr_va, phnum, phentsize, initial_brk, tls_base, None) {
        Some(pid)
    } else {
        None
    }
}

/// Inicializar un proceso con un PID y espacio de direcciones ya creados.
/// phdr_va, phnum, and phentsize are passed to jump_to_userspace for the auxv (AT_PHDR, AT_PHNUM, AT_PHENT).
pub fn create_process_with_pid(
    pid: ProcessId,
    cr3: u64,
    entry_point: u64,
    stack_base: u64,
    stack_size: usize,
    phdr_va: u64,
    phnum: u64,
    phentsize: u64,
    initial_brk: u64,
    tls_base: u64,
    dynamic_linker_aux: Option<(u64, u64)>,
) -> bool {
    // Allocate kernel stack for this process
    let kernel_stack_size = KERNEL_STACK_SIZE;
    let kernel_stack = alloc::vec![0u8; kernel_stack_size];
    let kernel_stack_top = kernel_stack.as_ptr() as u64 + kernel_stack_size as u64;
    core::mem::forget(kernel_stack);

    // CRITICAL: Disable interrupts to avoid deadlock with scheduler timer interrupt
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut table = PROCESS_TABLE.lock();
        
        // Buscar slot libre (enumerate para conocer el índice del slot)
        // Accept both empty slots and slots whose previous occupant has Terminated,
        // so that the 256-entry table can be reused across process lifetimes.
        // Without this, after 63 processes exit the table fills up permanently.
        for (slot_idx, slot) in table.iter_mut().enumerate() {
            let slot_available = slot.is_none()
                || matches!(slot, Some(ref p) if
                    p.state == ProcessState::Terminated
                    // Only evict a Terminated slot once no CPU is still using its context.
                    // perform_context_switch() holds a raw pointer into the slot's context
                    // until switch_context() atomically clears current_cpu.  Evicting the
                    // slot before that point causes the new process's context to be
                    // overwritten by the old CPU's register save.
                    && p.current_cpu == crate::process::NO_CPU);
            if slot_available {
                // Allocate a unique FD table index for the new process resources
                // For simplicity, we use same slot_idx as the fd_table_idx initially.
                let resources = Arc::new(Mutex::new(ProcessResources::new(cr3, slot_idx)));
                {
                    let mut r = resources.lock();
                    r.brk_current = initial_brk;
                }

                let proc_obj = Arc::new(Mutex::new(Proc {
                    id: pid,
                    parent_pid: current_process_id(),
                    resources,
                    name: [0; 16],
                    signal_actions: [SignalAction::default(); 64],
                    uid: 0, gid: 0, euid: 0, egid: 0, suid: 0, sgid: 0,
                    exit_code: 0, exit_signal: 0,
                    notified_stopped: false,
                    notified_continued: false,
                    is_linux: true,
                    syscall_trace: false,
                    mem_frames: (stack_size / 4096) as u64,
                    vfork_waiting_for_child: None,
                    vfork_shared_mm_with_parent: None,
                    cwd: [0u8; 128],
                    cwd_len: 0,
                    pgid: pid,
                    sid: pid,
                    dynamic_linker_aux,
                    umask: 0o022,
                    supplementary_groups: [0; 32],
                    supplementary_groups_len: 0,
                }));

                let mut process = Process::new(pid, proc_obj);
                process.stack_base = stack_base;
                process.stack_size = stack_size;
                process.priority = 5; 
                process.vruntime = 0;
                process.weight = 1024;
                process.time_slice = 10; 
                
                let kernel_stack_top_aligned = kernel_stack_top & !0xF;

                let trampoline: u64 = if dynamic_linker_aux.is_some() {
                    crate::elf_loader::jump_to_userspace_dynamic_linker as *const () as u64
                } else {
                    crate::elf_loader::jump_to_userspace as *const () as u64
                };
                process.context.rip = trampoline;
                process.context.rdi = entry_point;                            
                process.context.rsi = stack_base + stack_size as u64;         
                process.context.rdx = phdr_va;                                
                process.context.rcx = phnum;                                  
                process.context.r8 = phentsize;                               
                process.context.r9 = tls_base; // 6th argument: tls_base
                process.context.rsp = kernel_stack_top_aligned;               
                process.context.rflags = 0x002; 
                process.kernel_stack_top = kernel_stack_top_aligned; 
                process.fs_base = tls_base;
                
                crate::serial::serial_printf(format_args!(
                    "[PROC] Created process PID: {} slot: {} CR3: {:#018X}\n",
                    pid, slot_idx, cr3
                ));

                crate::serial::serial_print("[debug] create_process_with_pid: assigning slot\n");
                *slot = Some(process);
                crate::serial::serial_print("[debug] create_process_with_pid: registering pid slot\n");
                // Registrar en tabla inversa PID → slot O(1) para IPC
                crate::ipc::register_pid_slot(pid, slot_idx);
                crate::serial::serial_print("[debug] create_process_with_pid: resetting syscall counters\n");
                // Reset the fast syscall counter for the new process slot
                crate::ai_core::SYSCALL_COUNTERS[slot_idx].store(0, core::sync::atomic::Ordering::Relaxed);
                crate::serial::serial_print("[debug] create_process_with_pid: returning true\n");
                return true;
            }
        }
        crate::serial::serial_print("[debug] create_process_with_pid: no slot available\n");
        false
    })
}

/// Registra en `vmas` los segmentos ELF cargados y la pila fija `[stack_base, stack_base+stack_size)`.
///
/// Tras `exec` / `execve` el kernel vacía `vmas`; si no se vuelven a registrar, un `mmap` con
/// pista o `MAP_FIXED` puede solapar `unmap_user_range` con la pila en 0x20000000..0x20100000
/// y provocar #PF en RSP; además el fallo bajo demanda no puede reponer hojas sin VMA.
pub fn register_mmap_vma(pid: ProcessId, start: u64, len: u64, prot: u64, flags: u32) {
    if let Some(mut proc) = get_process(pid) {
        let r_arc = proc.proc.lock().resources.clone();
        let mut r = r_arc.lock();
        let obj = crate::vm_object::VMObject::new_anonymous(len as u64);
        r.vmas.push(VMARegion {
            start,
            end: start + len as u64,
            flags: prot,
            object: obj,
            offset: 0,
            is_huge: false,
            is_shared: (flags & 0x01) != 0, // MAP_SHARED is 0x01
        });
        drop(r);
        update_process(pid, proc);
    }
}

pub fn register_post_exec_vm_as(
    pid: ProcessId,
    loaded: &crate::elf_loader::ExecLoadResult,
    stack_base: u64,
    stack_size: u64,
) {
    crate::serial::serial_printf(format_args!("[debug] register_post_exec_vm_as pid={}\n", pid));
    x86_64::instructions::interrupts::without_interrupts(|| {
        if let Some(mut proc) = get_process(pid) {
            crate::serial::serial_print("[debug] register_post_exec_vm_as: got process\n");
            let r_arc = proc.proc.lock().resources.clone();
            let mut r = r_arc.lock();
            for i in 0..loaded.loaded_vma_count {
                let (start, end) = loaded.loaded_vma_ranges[i];
                if start < end {
                    r.vmas.push(VMARegion {
                        start,
                        end,
                        // NOTE: wlroots/labwc forks early and triggers CoW write faults.
                        // Until we track per-segment protections here, mark the whole loaded
                        // image as RWX so CoW can validate writes against a writable VMA.
                        // (PTE permissions still come from ELF p_flags.)
                        flags:             0x7, // PROT_READ | PROT_WRITE | PROT_EXEC
                        object:            crate::vm_object::VMObject::new_anonymous((end - start) as u64),
                        offset:            0,
                        is_huge:           false,
                        is_shared:         false,
                    });
                }
            }
            r.vmas.push(VMARegion {
                start:             stack_base,
                end:               stack_base.saturating_add(stack_size),
                flags:             0x3, // PROT_READ | PROT_WRITE
                object:            crate::vm_object::VMObject::new_anonymous(stack_size),
                offset:            0,
                is_huge:           false,
                is_shared:         false,
            });
            drop(r);
            update_process(pid, proc);
            crate::serial::serial_print("[debug] register_post_exec_vm_as: updated process\n");
        }
    });
    crate::serial::serial_print("[debug] register_post_exec_vm_as: done\n");
}

/// Ejecutar un binario ELF como un nuevo proceso.
///
/// Responsabilidades kernel vs ld.so: `ELF_LOADING.md` en este crate.
pub fn spawn_process(elf_data: &[u8], name: &str) -> Result<ProcessId, &'static str> {
    crate::serial::serial_printf(format_args!("[spawn] ENTERED for process: {}\n", name));
    let pid = next_pid();

    crate::serial::serial_print("[spawn] calling create_process_paging\n");
    let cr3 = crate::memory::create_process_paging();
    crate::serial::serial_printf(format_args!("[spawn] create_process_paging returned cr3=0x{:x}\n", cr3));

    crate::serial::serial_print("[spawn] calling load_elf_into_space\n");
    let loaded = crate::elf_loader::load_elf_into_space(cr3, &crate::elf_loader::SliceDataProvider(elf_data))?;
    crate::serial::serial_printf(format_args!(
        "[spawn] load_elf_into_space done entry=0x{:x} TLS={:x}\n",
        loaded.entry_point, loaded.tls_base
    ));

    crate::serial::serial_print("[spawn] calling setup_user_stack\n");
    let stack_base = 0x20000000;
    let stack_size = 0x100000; // 1MB — enough for deep compositor render call stacks
    let _stack_top = crate::elf_loader::setup_user_stack(cr3, stack_base, stack_size)?;
    crate::serial::serial_print("[spawn] setup_user_stack done\n");

    crate::serial::serial_print("[spawn] calling create_process_with_pid\n");
    if create_process_with_pid(
        pid,
        cr3,
        loaded.entry_point,
        stack_base,
        stack_size,
        loaded.phdr_va,
        loaded.phnum,
        loaded.phentsize,
        loaded.max_vaddr,
        loaded.tls_base,
        loaded.dynamic_linker,
    ) {
        crate::serial::serial_print("[debug] spawn_process: entering post-create block\n");
        x86_64::instructions::interrupts::without_interrupts(|| {
            crate::serial::serial_print("[debug] spawn_process: taking table lock\n");
            let mut table = PROCESS_TABLE.lock();
            crate::serial::serial_print("[debug] spawn_process: table lock acquired\n");
            let mut parent_info = None;
            if let Some(parent_pid) = current_process_id() {
                // Find parent in the ALREADY LOCKED table to avoid recursive deadlock
                for p_entry in table.iter().flatten() {
                    if p_entry.id == parent_pid {
                        let p_proc = p_entry.proc.lock();
                        parent_info = Some((p_proc.pgid, p_proc.sid, p_proc.cwd, p_proc.cwd_len));
                        break;
                    }
                }
            }

            if let Some(slot) = crate::ipc::pid_to_slot_fast(pid) {
                if let Some(p) = table[slot].as_mut() {
                    let mut proc = p.proc.lock();
                    let n = core::cmp::min(name.len(), 16);
                    proc.name[..n].copy_from_slice(&name.as_bytes()[..n]);
                    proc.mem_frames += loaded.segment_frames;
                    
                    if let Some((pgid, sid, cwd, cwd_len)) = parent_info {
                        proc.pgid = pgid;
                        proc.sid = sid;
                        proc.cwd = cwd;
                        proc.cwd_len = cwd_len;
                    }
                }
            }
        });
        crate::serial::serial_print("[spawn] calling fd_init_stdio\n");
        crate::fd::fd_init_stdio(pid);
        crate::serial::serial_print("[spawn] calling register_post_exec_vm_as\n");
        register_post_exec_vm_as(pid, &loaded, stack_base, stack_size as u64);
        crate::serial::serial_printf(format_args!("[spawn] SUCCESS for process: {}\n", name));
        Ok(pid)
    } else {
        crate::serial::serial_printf(format_args!("[spawn] FAILED to insert process into table: {}\n", name));
        Err("Failed to insert process into table")
    }
}

pub fn current_process_id() -> Option<ProcessId> {
    let pid: u32;
    unsafe {
        asm!(
            "mov {0:e}, gs:[20]",
            out(reg) pid,
            options(nomem, nostack, preserves_flags)
        );
    }
    if pid == 0xFFFF_FFFF {
        None
    } else {
        Some(pid)
    }
}

pub fn get_process_altstack(pid: ProcessId) -> (u64, u64, i32) {
    let table = PROCESS_TABLE.lock();
    for slot in table.iter() {
        if let Some(p) = slot {
            if p.id == pid {
                return (p.sigaltstack.ss_sp, p.sigaltstack.ss_size, p.sigaltstack.ss_flags);
            }
        }
    }
    (0, 0, SS_DISABLE)
}

/// Establecer proceso actual (O(1) vía GS segment, sin lock)
pub fn set_current_process(pid: Option<ProcessId>) {
    let val = pid.unwrap_or(0xFFFF_FFFF);
    unsafe {
        asm!(
            "mov gs:[20], {0:e}",
            in(reg) val,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Obtener proceso por ID
pub fn get_process(pid: ProcessId) -> Option<Process> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let table = PROCESS_TABLE.lock();
        // O(1) lookup via fast PID-to-slot map
        if let Some(slot) = crate::ipc::pid_to_slot_fast(pid) {
            if let Some(p) = table[slot].as_ref() {
                if p.id == pid {
                    return Some(p.clone());
                }
            }
        }
        // Fallback: linear scan
        for process in table.iter() {
            if let Some(p) = process {
                if p.id == pid {
                    return Some(p.clone());
                }
            }
        }
        None
    })
}

/// Obtener PID de un proceso por su nombre
pub fn get_process_by_name(name: &str) -> Option<ProcessId> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let table = PROCESS_TABLE.lock();
        let name_bytes = name.as_bytes();
        let name_len = core::cmp::min(name_bytes.len(), 16);
        
        for slot in table.iter() {
            if let Some(p) = slot {
                let proc = p.proc.lock();
                let p_name_len = proc.name.iter().position(|&b| b == 0).unwrap_or(16);
                if p_name_len == name_len && &proc.name[..name_len] == &name_bytes[..name_len] {
                    return Some(p.id);
                }
            }
        }
        None
    })
}

/// Obtener el índice de slot (0..MAX_PROCESSES) de un proceso por su PID.
/// A diferencia del PID (que es monotónico), el slot index es reutilizable
/// y siempre cabe en el array de mailboxes IPC (también de 64 entradas).
/// Devuelve None si el proceso no existe o está terminado.
pub fn pid_to_slot(pid: ProcessId) -> Option<usize> {
    // Try fast lookup first (O(1))
    if let Some(slot) = crate::ipc::pid_to_slot_fast(pid) {
        return Some(slot);
    }
    x86_64::instructions::interrupts::without_interrupts(|| {
        let table = PROCESS_TABLE.lock();
        for (i, slot) in table.iter().enumerate() {
            if let Some(p) = slot {
                if p.id == pid {
                    return Some(i);
                }
            }
        }
        None
    })
}

/// Get process page table physical address
pub fn get_process_page_table(pid: Option<ProcessId>) -> u64 {
    if let Some(pid) = pid {
        if let Some(process) = get_process(pid) {
            return process.proc.lock().resources.lock().page_table_phys;
        }
    }
    0
}

/// Actualizar proceso (seguro para SMP)
pub fn update_process(pid: ProcessId, mut new_process: Process) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut table = PROCESS_TABLE.lock();
        // O(1) lookup
        if let Some(slot) = crate::ipc::pid_to_slot_fast(pid) {
            if let Some(p) = table[slot].as_mut() {
                if p.id == pid {
                    let real_cpu = p.current_cpu;
                    let real_state = p.state;
                    *p = new_process;
                    p.current_cpu = real_cpu;
                    p.state = real_state;
                    return;
                }
            }
        }
        // Fallback: búsqueda lineal por si el PID no coincide con el índice de slot
        for slot in table.iter_mut() {
            if let Some(p) = slot {
                if p.id == pid {
                    let real_cpu = p.current_cpu;
                    let real_state = p.state;
                    *p = new_process;
                    p.current_cpu = real_cpu;
                    p.state = real_state;
                    return;
                }
            }
        }
    });
}

/// Modify process state (callback-based, safe for SMP)
pub fn modify_process<F>(pid: ProcessId, f: F) -> Result<(), &'static str>
where
    F: FnOnce(&mut Process),
{
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut table = PROCESS_TABLE.lock();
        // O(1) lookup
        if let Some(slot) = crate::ipc::pid_to_slot_fast(pid) {
            if let Some(p) = table[slot].as_mut() {
                if p.id == pid {
                    let real_cpu = p.current_cpu;
                    let real_state = p.state;
                    f(p);
                    p.current_cpu = real_cpu;
                    p.state = real_state;
                    return Ok(());
                }
            }
        }
        for slot in table.iter_mut() {
            if let Some(p) = slot {
                if p.id == pid {
                    let real_cpu = p.current_cpu;
                    let real_state = p.state;
                    f(p);
                    p.current_cpu = real_cpu;
                    p.state = real_state;
                    return Ok(());
                }
            }
        }
        Err("Process not found")
    })
}

pub fn get_uid(pid: ProcessId) -> u32 { get_process(pid).map(|p| p.get_uid()).unwrap_or(0) }
pub fn get_gid(pid: ProcessId) -> u32 { get_process(pid).map(|p| p.get_gid()).unwrap_or(0) }
pub fn get_euid(pid: ProcessId) -> u32 { get_process(pid).map(|p| p.get_euid()).unwrap_or(0) }
pub fn get_egid(pid: ProcessId) -> u32 { get_process(pid).map(|p| p.get_egid()).unwrap_or(0) }
pub fn get_pgid(pid: ProcessId) -> ProcessId { get_process(pid).map(|p| p.get_pgid()).unwrap_or(0) }
pub fn get_sid(pid: ProcessId) -> ProcessId { get_process(pid).map(|p| p.get_sid()).unwrap_or(0) }

pub fn process_count() -> usize {
    PROCESS_TABLE.lock().iter().flatten().count()
}

/// Modify process state (bypasses metadata-only protection in update_process)
pub fn modify_process_state(pid: ProcessId, new_state: ProcessState) -> Result<(), &'static str> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut table = PROCESS_TABLE.lock();
        // O(1) lookup
        if let Some(slot_idx) = crate::ipc::pid_to_slot_fast(pid) {
            if let Some(p) = table[slot_idx].as_mut() {
                if p.id == pid {
                    p.state = new_state;
                    return Ok(());
                }
            }
        }
        // O(N) lookup fallback
        for slot in table.iter_mut() {
            if let Some(p) = slot {
                if p.id == pid {
                    p.state = new_state;
                    return Ok(());
                }
            }
        }
        Err("Process not found")
    })
}

/// Atomic compare-and-set process state
pub fn compare_and_set_process_state(pid: ProcessId, expected: ProcessState, new_state: ProcessState) -> Result<bool, &'static str> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut table = PROCESS_TABLE.lock();
        if let Some(slot_idx) = crate::ipc::pid_to_slot_fast(pid) {
            if let Some(p) = table[slot_idx].as_mut() {
                if p.id == pid {
                    if p.state == expected {
                        p.state = new_state;
                        return Ok(true);
                    } else {
                        return Ok(false);
                    }
                }
            }
        }
        // O(N) lookup fallback
        for slot in table.iter_mut() {
            if let Some(p) = slot {
                if p.id == pid {
                    if p.state == expected {
                        p.state = new_state;
                        return Ok(true);
                    } else {
                        return Ok(false);
                    }
                }
            }
        }
        Err("Process not found")
    })
}

/// Cambiar de contexto entre procesos
/// 
/// Esta función guarda el contexto del proceso actual y carga el contexto del siguiente proceso
/// Optionally switches CR3 (Control Register 3) if next_cr3 is not 0.
/// If clear_addr is non-zero, writes NO_CPU (0xFFFFFFFF) as a u32 to that address
/// immediately after saving the 'from' context, before restoring 'to'. This atomically
/// releases CPU ownership of the 'from' process the moment its context is fully saved,
/// eliminating the race between clearing current_cpu and context save.
/// 
/// # Safety
/// Esta función es unsafe porque manipula directamente registros de CPU
#[no_mangle]
pub unsafe extern "C" fn switch_context(from: &mut Context, to: &Context, next_cr3: u64, clear_addr: u64) {
    asm!(
        // Guardar contexto actual (usando rdi = from)
        // Note: rcx = clear_addr (4th argument in SysV ABI)
        "mov [rdi + 0x00], rax",
        "mov [rdi + 0x08], rbx",
        "mov [rdi + 0x10], rcx",
        "mov [rdi + 0x18], rdx",
        "mov [rdi + 0x20], rsi",
        // rdi está en uso, pero guarda su valor original (que recibimos)
        "mov [rdi + 0x28], rdi", 
        "mov [rdi + 0x30], rbp",
        "mov [rdi + 0x38], r8",
        "mov [rdi + 0x40], r9",
        "mov [rdi + 0x48], r10",
        "mov [rdi + 0x50], r11",
        "mov [rdi + 0x58], r12",
        "mov [rdi + 0x60], r13",
        "mov [rdi + 0x68], r14",
        "mov [rdi + 0x70], r15",
        
        // Guardar RSP actual
        "mov rax, rsp",
        "mov [rdi + 0x78], rax",
        
        // Guardar RIP (dirección de retorno)
        "lea rax, [rip + 2f]",
        "mov [rdi + 0x80], rax",
        
        // Guardar RFLAGS
        "pushfq",
        "pop rax",
        "mov [rdi + 0x88], rax",
        
        // Atomic ownership release: clear the from-process current_cpu (write NO_CPU = 0xFFFFFFFF)
        // now that the full context is saved. rcx still holds the original clear_addr argument.
        // This eliminates the race between clearing current_cpu and actually saving the context.
        "test rcx, rcx",
        "jz 4f",
        "mov eax, 0xFFFFFFFF",
        "mov dword ptr [rcx], eax",
        "4:",

        // ==========================================
        // Restaurar contexto nuevo (usando rsi = to)
        // ==========================================
        
        // 0. Cambiar CR3 si es necesario (Atomic-ish switch with Stack)
        // rdx holds next_cr3
        "test rdx, rdx",
        "jz 3f",
        "mov cr3, rdx",
        "3:",

        // 1. Cambiar Stack
        "mov rsp, [rsi + 0x78]",
        
        // 2. Preparar stack para iretq/ret simulado
        "push qword ptr [rsi + 0x80]", // RIP (at 0x80)
        "push qword ptr [rsi + 0x88]", // RFLAGS (at 0x88)
        
        // 3. Restaurar GP registers (EXCEPTO RSI que tiene el puntero 'to')
        "mov rax, [rsi + 0x00]",
        "mov rbx, [rsi + 0x08]",
        "mov rcx, [rsi + 0x10]",
        "mov rdx, [rsi + 0x18]",
        "mov rdi, [rsi + 0x28]",
        "mov rbp, [rsi + 0x30]",
        "mov r8,  [rsi + 0x38]",
        "mov r9,  [rsi + 0x40]",
        "mov r10, [rsi + 0x48]",
        "mov r11, [rsi + 0x50]",
        "mov r12, [rsi + 0x58]",
        "mov r13, [rsi + 0x60]",
        "mov r14, [rsi + 0x68]",
        "mov r15, [rsi + 0x70]",
        
        // 4. Restaurar RSI (Ultimo, porque usabamos rsi como puntero 'to')
        "mov rsi, [rsi + 0x20]",
        
        // 5. Restaurar RFLAGS y RIP
        "popfq", // Restaura RFLAGS
        "ret",   // Restaura RIP (pop rip; jmp rip)
        
        "2:",
        in("rdi") from,
        in("rsi") to,
        in("rdx") next_cr3,
        in("rcx") clear_addr,
    );
}

/// Terminar proceso actual
pub fn exit_process() {
    if let Some(pid) = current_process_id() {
        crate::kqueue::get_kqueue_scheme().trigger_global(crate::kqueue::EVFILT_PROC, pid as u64, crate::kqueue::NOTE_EXIT as i64);
        clear_pending_process_args(pid);
        vfork_wake_parent_waiting_for_child(pid);

        // Re-parent children to PID 1 (init)
        let target_pid = pid;
        x86_64::instructions::interrupts::without_interrupts(|| {
            let mut table = PROCESS_TABLE.lock();
            for slot in table.iter_mut() {
                if let Some(p) = slot {
                    let mut proc = p.proc.lock();
                    if proc.parent_pid == Some(target_pid) {
                        proc.parent_pid = Some(1);
                    }
                }
            }
        });

        // Collect open file descriptors so we can close them outside the lock
        let mut to_close: [(usize, usize); crate::fd::MAX_FDS_PER_PROCESS] =
            [(0, 0); crate::fd::MAX_FDS_PER_PROCESS];
        let mut close_count = 0;

        x86_64::instructions::interrupts::without_interrupts(|| {
            let mut tables = crate::fd::FD_TABLES.lock();
            // Use the slot index (not the raw PID) to index FD_TABLES.
            // pid_to_slot_fast is safe to call here: the process is still in PROCESS_TABLE
            // (we haven't marked it Terminated yet) so the slot lookup will succeed.
            let pid_idx = match crate::ipc::pid_to_slot_fast(target_pid) {
                Some(i) => i,
                None => return,
            };
            if pid_idx < crate::fd::MAX_FD_PROCESSES {
                for fd in 0..crate::fd::MAX_FDS_PER_PROCESS {
                    if tables[pid_idx].fds[fd].in_use {
                        to_close[close_count] = (
                            tables[pid_idx].fds[fd].scheme_id,
                            tables[pid_idx].fds[fd].resource_id,
                        );
                        close_count += 1;
                        tables[pid_idx].fds[fd].in_use = false;
                    }
                }
            }
        });

        // Close scheme resources outside the FD table lock
        for i in 0..close_count {
            if crate::scheme::close(to_close[i].0, to_close[i].1).is_err() {
                crate::serial::serial_printf(format_args!(
                    "[PROC] exit: scheme::close failed for scheme_id={} resource_id={}\n",
                    to_close[i].0, to_close[i].1
                ));
            }
        }

        x86_64::instructions::interrupts::without_interrupts(|| {
            let mut table = PROCESS_TABLE.lock();
            for (slot_idx, slot) in table.iter_mut().enumerate() {
                if let Some(p) = slot {
                    if p.id == target_pid {
                        // CLONE_CHILD_CLEARTID: write 0 to clear_child_tid and futex-wake it.
                        // Musl's pthread_join waits on this.
                        if p.clear_child_tid != 0 {
                            let addr = p.clear_child_tid;
                            if crate::syscalls::is_user_pointer(addr, 4) {
                                unsafe {
                                    (addr as *mut u32).write_unaligned(0);
                                }
                                // Futex wake all waiters on this address
                                crate::syscalls::futex_wake_all_atomic(addr);
                            }
                        }

                        p.state = ProcessState::Terminated;
                        // NO llamamos a unregister_pid_slot aquí: el proceso queda
                        // como zombie (slot registrado, estado=Terminated) hasta que
                        // el padre llame wait().  Esto permite que sys_wait_impl
                        // encuentre al hijo mediante get_process() y lea su exit_code.
                        // El slot se libera en sys_wait_impl al cosechar el zombie.
                        //
                        // Limpiar el buzón IPC para que no lleguen mensajes nuevos.
                        crate::ipc::clear_mailbox_slot(slot_idx);
                        break;
                    }
                }
            }
        });

        // Wake any parent blocked in sys_wait() waiting for this child.
        // We call wake_parent_from_wait outside the PROCESS_TABLE lock to avoid
        // deadlock (wake_parent_from_wait re-acquires it).
        // We read parent_pid while we still have the process info cached.
        let parent_pid = {
            let table = PROCESS_TABLE.lock();
            table.iter().find_map(|slot| {
                slot.as_ref().and_then(|p| {
                    if p.id == pid { p.proc.lock().parent_pid } else { None }
                })
            })
        };
        if let Some(ppid) = parent_pid {
            // Send SIGCHLD to parent (SIG 17) so it wakes from wait() / handler.
            set_pending_signal(ppid, 17);
            wake_parent_from_wait(ppid);
        }
    }
}

/// Remove a reaped zombie process from PROCESS_TABLE and from the PID→slot map.
///
/// Must be called only after the parent has successfully read the exit code via
/// wait().  Clears both the PID_SLOT_MAP entry and the PROCESS_TABLE slot so
/// that the process no longer appears in `ps` or any other process list query.
pub fn remove_process(pid: ProcessId) {
    // Retrieve the slot index while the PID is still registered.
    let slot = crate::ipc::pid_to_slot_fast(pid);
    // Remove the PID→slot mapping first.
    crate::ipc::unregister_pid_slot(pid);
    // Now null out the PROCESS_TABLE slot so the entry is truly gone.
    if let Some(slot_idx) = slot {
        x86_64::instructions::interrupts::without_interrupts(|| {
            let mut table = PROCESS_TABLE.lock();
            if let Some(p) = table[slot_idx].as_ref() {
                if p.id == pid {
                    table[slot_idx] = None;
                }
            }
        });
    }
}

/// Listar todos los procesos
pub fn list_processes() -> [(ProcessId, ProcessState); MAX_PROCESSES] {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let table = PROCESS_TABLE.lock();
        let mut result = [(0, ProcessState::Terminated); MAX_PROCESSES];

        for (i, slot) in table.iter().enumerate() {
            if let Some(p) = slot {
                result[i] = (p.id, p.state);
            }
        }

        result
    })
}

/// Fork current process - create child process
/// Returns: Some(child_pid) on success, None on error
///
/// SMP note: the expensive clone_process_paging() and fd_clone_for_fork()
/// are performed BEFORE acquiring PROCESS_TABLE, so the lock is held only
/// for a brief insertion.  This prevents long scheduler stalls on other
/// CPUs that need PROCESS_TABLE during a fork.
pub fn fork_process(parent_context: &Context) -> Option<ProcessId> {
    let current_pid = current_process_id()?;
    let parent = get_process(current_pid)?;
    // IMPORTANT: do not hold the parent `Proc` lock across fd cloning.
    // `fd_clone_for_fork()` calls `pid_to_fd_idx()` → `get_process()` → `proc.lock()`,
    // which would deadlock if we kept this lock until the end of fork.
    let p_proc = parent.proc.lock();
    let parent_resources = Arc::clone(&p_proc.resources);
    let parent_name = p_proc.name;
    let parent_signal_actions = p_proc.signal_actions;
    let parent_ids = (p_proc.uid, p_proc.gid, p_proc.euid, p_proc.egid, p_proc.suid, p_proc.sgid);
    let parent_misc = (
        p_proc.is_linux,
        p_proc.syscall_trace,
        p_proc.mem_frames,
        p_proc.cwd,
        p_proc.cwd_len,
        p_proc.pgid,
        p_proc.sid,
        p_proc.dynamic_linker_aux,
        p_proc.umask,
        p_proc.supplementary_groups,
        p_proc.supplementary_groups_len,
    );
    drop(p_proc);

    // ── Phase 1: Expensive work BEFORE acquiring any kernel-global lock ──────
    let child_cr3 = crate::memory::clone_process_paging(parent_resources.lock().page_table_phys);

    let kernel_stack = alloc::vec![0u8; KERNEL_STACK_SIZE];
    let kernel_stack_top = kernel_stack.as_ptr() as u64 + KERNEL_STACK_SIZE as u64;
    let kernel_stack_top_aligned = kernel_stack_top & !0xF;

    let kstack_ptr = unsafe {
        let mut p = kernel_stack_top_aligned as *mut u64;
        p = p.offset(-1); *p = 0x23;                  // SS
        p = p.offset(-1); *p = parent_context.rsp;    // RSP
        p = p.offset(-1); *p = parent_context.rflags; // RFLAGS
        p = p.offset(-1); *p = 0x1b;                  // CS
        p = p.offset(-1); *p = parent_context.rip;    // RIP
        p
    };
    crate::serial::serial_printf(format_args!(
        "[fork] build_iret_frame parent_rip={:#x} parent_rsp={:#x} parent_rflags={:#x}\n",
        parent_context.rip, parent_context.rsp, parent_context.rflags
    ));
    unsafe {
        crate::serial::serial_printf(format_args!(
            "[fork] child_iret @{:p}: RIP={:#x} CS={:#x} RFLAGS={:#x} RSP={:#x} SS={:#x}\n",
            kstack_ptr,
            *kstack_ptr,
            *kstack_ptr.add(1),
            *kstack_ptr.add(2),
            *kstack_ptr.add(3),
            *kstack_ptr.add(4),
        ));
    }

    let child_resources = {
        let r = parent_resources.lock();
        let mut child_vmas = Vec::new();
        for vma in r.vmas.iter() {
            let mut new_vma = vma.clone();
            if !vma.is_shared {
                // For private mappings, we need a new VMObject that initially points to the same frames (COW)
                new_vma.object = vma.object.lock().clone_for_fork();
            }
            // For shared mappings, new_vma.object already points to the same Arc<Mutex<VMObject>> via vma.clone()
            child_vmas.push(new_vma);
        }

        Arc::new(Mutex::new(ProcessResources {
            page_table_phys: child_cr3,
            vmas: child_vmas,
            brk_current: r.brk_current,
            // IMPORTANT: this must be the child's PROCESS_TABLE slot index, not the parent's.
            // We don't know the child's slot yet (it is chosen in Phase 2), so set a placeholder
            // and fix it immediately after we reserve a slot.
            fd_table_idx: 0,
        }))
    };

    // ── Phase 2: Critical section — insert child into PROCESS_TABLE ───
    let result = x86_64::instructions::interrupts::without_interrupts(|| {
        let mut table = PROCESS_TABLE.lock();
        let mut next_pid = NEXT_PID.lock();

        for (slot_idx, slot) in table.iter_mut().enumerate() {
            let slot_available = slot.is_none()
                || matches!(slot, Some(ref p) if
                    p.state == ProcessState::Terminated
                    && p.current_cpu == NO_CPU);

            if slot_available {
                let child_pid = *next_pid;
                *next_pid += 1;

                // Create new Proc for child (POSIX fork creates a new process)
                let child_proc_obj = Arc::new(Mutex::new(Proc {
                    id: child_pid,
                    parent_pid: Some(current_pid),
                    resources: child_resources,
                    name: parent_name,
                    signal_actions: parent_signal_actions,
                    uid: parent_ids.0, gid: parent_ids.1,
                    euid: parent_ids.2, egid: parent_ids.3,
                    suid: parent_ids.4, sgid: parent_ids.5,
                    exit_code: 0, exit_signal: 0,
                    notified_stopped: false, notified_continued: false,
                    is_linux: parent_misc.0,
                    syscall_trace: parent_misc.1,
                    mem_frames: parent_misc.2,
                    vfork_waiting_for_child: None,
                    vfork_shared_mm_with_parent: None,
                    cwd: parent_misc.3,
                    cwd_len: parent_misc.4,
                    pgid: parent_misc.5,
                    sid: parent_misc.6,
                    dynamic_linker_aux: parent_misc.7,
                    umask: parent_misc.8,
                    supplementary_groups: parent_misc.9,
                    supplementary_groups_len: parent_misc.10,
                }));

                let mut child = Process::new(child_pid, child_proc_obj);
                child.kernel_stack = Some(kernel_stack);
                child.tgid = child_pid;
                child.state = ProcessState::Blocked;
                child.kernel_stack_top = kernel_stack_top_aligned;
                // For clone(CLONE_SETTLS) we must honor the TLS base coming from the syscall context.
                // Even for plain fork, parent_context.fs_base should match the parent's current FS_BASE.
                child.fs_base = parent_context.fs_base;
                child.gs_base = parent_context.gs_base;
                child.priority = parent.priority;
                child.vruntime = parent.vruntime;
                child.weight = parent.weight;
                child.time_slice = parent.time_slice;
                child.stack_base = parent.stack_base;
                child.stack_size = parent.stack_size;
                child.cpu_affinity = parent.cpu_affinity;
                child.signal_mask = parent.signal_mask;
                child.sigaltstack = parent.sigaltstack;

                // Set up context so the child resumes via fork_child_setup
                child.context.rip = crate::interrupts::fork_child_setup as *const () as u64;
                child.context.rsp = kstack_ptr as u64;
                child.context.rax = 0; // fork() returns 0 in child

                // Preserve user-visible GP registers across fork (Linux semantics):
                // child returns with the same register state as parent, except RAX=0.
                child.context.rbx = parent_context.rbx;
                child.context.rcx = parent_context.rcx;
                child.context.rdx = parent_context.rdx;
                child.context.rsi = parent_context.rsi;
                child.context.rdi = parent_context.rdi;
                child.context.rbp = parent_context.rbp;
                child.context.r8  = parent_context.r8;
                child.context.r9  = parent_context.r9;
                child.context.r10 = parent_context.r10;
                child.context.r11 = parent_context.r11;
                child.context.r12 = parent_context.r12;
                child.context.r13 = parent_context.r13;
                child.context.r14 = parent_context.r14;
                child.context.r15 = parent_context.r15;
                
                // Fix up the child's FD table index now that slot_idx is known.
                // Without this, parent and child would share the same FD table slot,
                // causing silent FD corruption and random hangs right after fork().
                {
                    let proc = child.proc.lock();
                    proc.resources.lock().fd_table_idx = slot_idx;
                }

                *slot = Some(child);
                crate::ipc::register_pid_slot(child_pid, slot_idx);
                return Some(child_pid);
            }
        }
        None
    });

    let child_pid = result?;
    crate::serial::serial_printf(format_args!(
        "[fork] parent={} child={} before fd_clone_for_fork\n",
        current_pid, child_pid
    ));
    crate::fd::fd_clone_for_fork(current_pid, child_pid);
    crate::serial::serial_printf(format_args!(
        "[fork] parent={} child={} after fd_clone_for_fork\n",
        current_pid, child_pid
    ));
    Some(child_pid)
}

/// clone(CLONE_THREAD|CLONE_VM|CLONE_FILES|CLONE_SIGHAND|...): create a new thread (TID)
/// sharing the same `Proc` (tgid/resources/fd table) as the parent.
pub fn clone_thread_process(
    parent_pid: ProcessId,
    child_user_context: &Context,
    clear_child_tid: u64,
    set_child_tid: u64,
) -> Option<ProcessId> {
    let parent = get_process(parent_pid)?;
    let parent_tgid = parent.tgid;
    let parent_proc = Arc::clone(&parent.proc);

    let kernel_stack = alloc::vec![0u8; KERNEL_STACK_SIZE];
    let kernel_stack_top = kernel_stack.as_ptr() as u64 + KERNEL_STACK_SIZE as u64;
    let kernel_stack_top_aligned = kernel_stack_top & !0xF;

    let kstack_ptr = unsafe {
        let mut p = kernel_stack_top_aligned as *mut u64;
        p = p.offset(-1); *p = 0x23;                        // SS
        p = p.offset(-1); *p = child_user_context.rsp;      // RSP
        p = p.offset(-1); *p = child_user_context.rflags;   // RFLAGS
        p = p.offset(-1); *p = 0x1b;                        // CS
        p = p.offset(-1); *p = child_user_context.rip;      // RIP
        p
    };
    crate::serial::serial_printf(format_args!(
        "[clone_thread] parent={} tgid={} child_rip={:#x} child_rsp={:#x} child_rflags={:#x} fs_base={:#x}\n",
        parent_pid, parent_tgid, child_user_context.rip, child_user_context.rsp, child_user_context.rflags, child_user_context.fs_base
    ));
    unsafe {
        crate::serial::serial_printf(format_args!(
            "[clone_thread] iret @{:p}: RIP={:#x} CS={:#x} RFLAGS={:#x} RSP={:#x} SS={:#x}\n",
            kstack_ptr,
            *kstack_ptr,
            *kstack_ptr.add(1),
            *kstack_ptr.add(2),
            *kstack_ptr.add(3),
            *kstack_ptr.add(4),
        ));
    }

    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut table = PROCESS_TABLE.lock();
        let mut next_pid = NEXT_PID.lock();

        for (slot_idx, slot) in table.iter_mut().enumerate() {
            let slot_available = slot.is_none()
                || matches!(slot, Some(ref p) if
                    p.state == ProcessState::Terminated
                    && p.current_cpu == NO_CPU);
            if !slot_available {
                continue;
            }

            let child_tid = *next_pid;
            *next_pid += 1;

            let mut child = Process::new(child_tid, Arc::clone(&parent_proc));
            child.kernel_stack = Some(kernel_stack);
            child.kernel_stack_top = kernel_stack_top_aligned;
            child.tgid = parent_tgid;
            child.state = ProcessState::Blocked;

            // Inherit scheduler-ish fields from parent.
            child.priority = parent.priority;
            child.vruntime = parent.vruntime;
            child.weight = parent.weight;
            child.time_slice = parent.time_slice;
            child.cpu_affinity = parent.cpu_affinity;
            child.signal_mask = parent.signal_mask;
            child.sigaltstack = parent.sigaltstack;

            // Thread-specific user context: child returns 0 from clone().
            child.fs_base = child_user_context.fs_base;
            child.gs_base = child_user_context.gs_base;

            // Ensure user-visible GP registers match Linux semantics: on return from clone/fork
            // the child sees the same register state as the parent except RAX=0.
            child.context.rbx = child_user_context.rbx;
            child.context.rcx = child_user_context.rcx;
            child.context.rdx = child_user_context.rdx;
            child.context.rsi = child_user_context.rsi;
            child.context.rdi = child_user_context.rdi;
            child.context.rbp = child_user_context.rbp;
            child.context.r8  = child_user_context.r8;
            child.context.r9  = child_user_context.r9;
            child.context.r10 = child_user_context.r10;
            child.context.r11 = child_user_context.r11;
            child.context.r12 = child_user_context.r12;
            child.context.r13 = child_user_context.r13;
            child.context.r14 = child_user_context.r14;
            child.context.r15 = child_user_context.r15;

            child.context.rip = crate::interrupts::fork_child_setup as *const () as u64;
            child.context.rsp = kstack_ptr as u64;
            child.context.rax = 0;

            child.clear_child_tid = clear_child_tid;
            child.set_child_tid = set_child_tid;

            *slot = Some(child);
            crate::ipc::register_pid_slot(child_tid, slot_idx);
            return Some(child_tid);
        }
        None
    })
}

/// `clone(CLONE_VM|CLONE_VFORK|…)` sin `CLONE_THREAD`: hijo comparte el mismo `ProcessResources`
/// (tabla de páginas y `fd_table_idx`) que el padre hasta `execve` / `exit` (comportamiento Linux).
pub fn vfork_process_shared_vm(parent_context: &Context) -> Option<ProcessId> {
    let current_pid = current_process_id()?;
    let parent = get_process(current_pid)?;
    let p_proc = parent.proc.lock();

    let child_resources = Arc::clone(&p_proc.resources);

    let kernel_stack = alloc::vec![0u8; KERNEL_STACK_SIZE];
    let kernel_stack_top = kernel_stack.as_ptr() as u64 + KERNEL_STACK_SIZE as u64;
    let kernel_stack_top_aligned = kernel_stack_top & !0xF;

    let kstack_ptr = unsafe {
        let mut p = kernel_stack_top_aligned as *mut u64;
        p = p.offset(-1); *p = 0x23;                  // SS
        p = p.offset(-1); *p = parent_context.rsp;    // RSP
        p = p.offset(-1); *p = parent_context.rflags; // RFLAGS
        p = p.offset(-1); *p = 0x1b;                  // CS
        p = p.offset(-1); *p = parent_context.rip;    // RIP
        p
    };

    let result = x86_64::instructions::interrupts::without_interrupts(|| {
        let mut table = PROCESS_TABLE.lock();
        let mut next_pid = NEXT_PID.lock();

        for (slot_idx, slot) in table.iter_mut().enumerate() {
            let slot_available = slot.is_none()
                || matches!(slot, Some(ref p) if
                    p.state == ProcessState::Terminated
                    && p.current_cpu == NO_CPU);
            if slot_available {
                let child_pid = *next_pid;
                *next_pid += 1;

                // Create new Proc but SHARING the same ProcessResources
                let child_proc_obj = Arc::new(Mutex::new(Proc {
                    id: child_pid,
                    parent_pid: Some(current_pid),
                    resources: child_resources,
                    name: p_proc.name,
                    signal_actions: p_proc.signal_actions,
                    uid: p_proc.uid, gid: p_proc.gid,
                    euid: p_proc.euid, egid: p_proc.egid,
                    suid: p_proc.suid, sgid: p_proc.sgid,
                    exit_code: 0, exit_signal: 0,
                    notified_stopped: false, notified_continued: false,
                    is_linux: p_proc.is_linux,
                    syscall_trace: p_proc.syscall_trace,
                    mem_frames: p_proc.mem_frames,
                    vfork_waiting_for_child: None,
                    vfork_shared_mm_with_parent: Some(current_pid),
                    cwd: p_proc.cwd,
                    cwd_len: p_proc.cwd_len,
                    pgid: p_proc.pgid,
                    sid: p_proc.sid,
                    dynamic_linker_aux: p_proc.dynamic_linker_aux,
                    umask: p_proc.umask,
                    supplementary_groups: p_proc.supplementary_groups,
                    supplementary_groups_len: p_proc.supplementary_groups_len,
                }));

                let mut child = Process::new(child_pid, child_proc_obj);
                child.kernel_stack = Some(kernel_stack);
                child.tgid = child_pid;
                child.state = ProcessState::Blocked;
                child.kernel_stack_top = kernel_stack_top_aligned;
                child.fs_base = parent.fs_base;
                child.gs_base = parent.gs_base;
                child.priority = parent.priority;
                child.vruntime = parent.vruntime;
                child.weight = parent.weight;
                child.time_slice = parent.time_slice;
                child.stack_base = parent.stack_base;
                child.stack_size = parent.stack_size;
                child.cpu_affinity = parent.cpu_affinity;
                child.signal_mask = parent.signal_mask;
                child.sigaltstack = parent.sigaltstack;

                // Set up context so the child resumes via fork_child_setup
                child.context.rip = crate::interrupts::fork_child_setup as *const () as u64;
                child.context.rsp = kstack_ptr as u64;
                child.context.rax = 0; // vfork() returns 0 in child
                
                *slot = Some(child);
                crate::ipc::register_pid_slot(child_pid, slot_idx);
                return Some(child_pid);
            }
        }
        None
    });

    result
}

// ---------------------------------------------------------------------------
// Señales — helpers
// ---------------------------------------------------------------------------

/// Devuelve la máscara de señales pendientes de un proceso.
pub fn get_pending_signals(pid: ProcessId) -> u64 {
    PROCESS_TABLE.lock().iter()
        .find_map(|slot| slot.as_ref().filter(|p| p.id == pid).map(|p| p.pending_signals))
        .unwrap_or(0)
}

/// Establece un bit de señal como pendiente en el proceso destino y lo desbloquea
/// si está en espera (para que la señal sea entregada en la próxima iteración).
pub fn set_pending_signal(pid: ProcessId, signum: u8) {
    if signum >= 64 { return; }
    let mut to_wake = None;
    let mut parent_to_notify = None;

    {
        let mut table = PROCESS_TABLE.lock();
        for slot in table.iter_mut() {
            if let Some(p) = slot {
                if p.id == pid {
                    p.pending_signals |= 1u64 << signum;
                    
                    let mut proc = p.proc.lock();
                    // Job Control: handle stop/cont signals
                    if signum == 19 || signum == 20 { // SIGSTOP or SIGTSTP
                        p.state = ProcessState::Stopped;
                        proc.exit_signal = signum as i32;
                        proc.notified_stopped = false;
                        parent_to_notify = proc.parent_pid;
                    } else if signum == 18 { // SIGCONT
                        if p.state == ProcessState::Stopped || p.state == ProcessState::Blocked {
                            proc.exit_signal = 18;
                            proc.notified_continued = false;
                            to_wake = Some(pid);
                            parent_to_notify = proc.parent_pid;
                        }
                    } else {
                        if p.state == ProcessState::Blocked {
                            to_wake = Some(pid);
                        }
                    }
                    break;
                }
            }
        }
    }

    // Process wakeup outside the lock to avoid deadlock
    if let Some(pid_to_wake) = to_wake {
        crate::scheduler::enqueue_process(pid_to_wake);
    }

    // Notify parent outside the lock
    if let Some(ppid) = parent_to_notify {
        // Send SIGCHLD (17) to parent
        set_pending_signal(ppid, 17);
        wake_parent_from_wait(ppid);
    }
    
    // Trigger kqueue SIGNAL filter for the target process
    crate::kqueue::get_kqueue_scheme().trigger_for_process(pid, crate::kqueue::EVFILT_SIGNAL, signum as u64, 0);
}

pub fn get_signal_action(pid: ProcessId, signum: u8) -> Option<SignalAction> {
    if signum >= 64 { return None; }
    let table = PROCESS_TABLE.lock();
    table.iter().flatten().find(|p| p.id == pid).map(|p| p.proc.lock().signal_actions[signum as usize])
}

pub fn set_signal_action(pid: ProcessId, signum: u8, action: SignalAction) {
    if signum >= 64 { return; }
    if let Some(p) = get_process(pid) {
        p.proc.lock().signal_actions[signum as usize] = action;
    }
}

pub fn consume_pending_signal(pid: ProcessId, signum: u8) -> Option<SignalAction> {
    if signum >= 64 { return None; }
    let mask = 1u64 << signum;
    let mut table = PROCESS_TABLE.lock();
    for slot in table.iter_mut() {
        if let Some(p) = slot {
            if p.id == pid && (p.pending_signals & mask) != 0 {
                p.pending_signals &= !mask;
                return Some(p.proc.lock().signal_actions[signum as usize]);
            }
        }
    }
    None
}

/// POSIX/Linux: señales cuyo `SIG_DFL` es **ignorar** (no terminan el proceso).
#[inline]
pub fn signal_default_is_ignore(signum: u8) -> bool {
    match signum {
        17 | 23 | 28 => true, // SIGCHLD, SIGURG, SIGWINCH
        _ => false,
    }
}

pub fn pop_lowest_pending_signal(pid: ProcessId) -> Option<(u8, SignalAction)> {
    let mut table = PROCESS_TABLE.lock();
    for slot in table.iter_mut() {
        if let Some(p) = slot {
            if p.id != pid || p.pending_signals == 0 {
                continue;
            }
            let unblockable = (1 << 9) | (1 << 18); // SIGKILL and SIGSTOP
            let deliverable = p.pending_signals & (!p.signal_mask | unblockable);
            
            if deliverable == 0 {
                return None;
            }
            
            let bit = deliverable.trailing_zeros();
            if bit >= 64 {
                return None;
            }
            let sig = bit as u8;
            let mask = 1u64 << sig;
            p.pending_signals &= !mask;
            let action = p.proc.lock().signal_actions[sig as usize];
            return Some((sig, action));
        }
    }
    None
}

pub fn terminate_other_process_by_signal(target_pid: ProcessId, sig: u8) -> Option<Option<ProcessId>> {
    let result = x86_64::instructions::interrupts::without_interrupts(|| {
        let mut table = PROCESS_TABLE.lock();
        for (slot_idx, slot) in table.iter_mut().enumerate() {
            if let Some(p) = slot {
                if p.id == target_pid {
                    if p.state == ProcessState::Terminated {
                        return Some(None);
                    }
                    {
                        let mut proc = p.proc.lock();
                        proc.exit_code = (128 + sig as u64) as i32;
                        proc.exit_signal = sig as i32;
                    }
                    p.state = ProcessState::Terminated;
                    crate::ipc::clear_mailbox_slot(slot_idx);
                    return Some(p.proc.lock().parent_pid);
                }
            }
        }
        None
    });
    // Send SIGCHLD to parent outside the lock to avoid deadlock.
    if let Some(Some(ppid)) = result {
        set_pending_signal(ppid, 17); // SIGCHLD
        wake_parent_from_wait(ppid);
    }
    result
}

/// Tras cada tick de timer/interrupción: entrega solo SIGKILL y señales fatales SIG_DFL.
/// Los manejadores de usuario con restorer se dejan pendientes para que
/// `deliver_pending_signals_for_current` los entregue correctamente al retorno de syscall.
pub fn deliver_pending_signals_noctx() {
    let Some(pid) = current_process_id() else { return };
    if let Some(p) = get_process(pid) {
        if p.state == ProcessState::Terminated {
            return;
        }
    } else {
        return;
    }

    loop {
        let Some((sig, action)) = pop_lowest_pending_signal(pid) else {
            break;
        };

        if action.handler == 1 {
            // SIG_IGN
            continue;
        }

        if action.handler != 0 {
            // Userspace handler with restorer: re-queue and defer to syscall return path.
            set_pending_signal(pid, sig);
            break;
        }

        // SIG_DFL: check if fatal
        let terminate = sig == 9  // SIGKILL: always fatal
            || !signal_default_is_ignore(sig);

        if !terminate {
            continue;
        }

        if let Some(mut proc) = get_process(pid) {
            proc.proc.lock().exit_code = (128 + sig as u64) as i32;
            update_process(pid, proc);
        }
        exit_process();
        crate::scheduler::yield_cpu();
        return;
    }
}

/// Get the current working directory of a process as a &str (from its cwd buffer).
pub fn get_process_cwd(pid: ProcessId) -> alloc::string::String {
    if let Some(p) = get_process(pid) {
        let proc = p.proc.lock();
        let len = proc.cwd_len.min(127);
        alloc::string::String::from_utf8_lossy(&proc.cwd[..len]).into_owned()
    } else {
        alloc::string::String::from("/")
    }
}

/// Set the current working directory of a process.
/// Returns true on success, false if path is too long.
pub fn set_process_cwd(pid: ProcessId, new_cwd: &str) -> bool {
    let bytes = new_cwd.as_bytes();
    if bytes.len() > 127 {
        return false;
    }
    modify_process(pid, |p| {
        let mut proc = p.proc.lock();
        proc.cwd_len = bytes.len();
        proc.cwd[..bytes.len()].copy_from_slice(bytes);
        proc.cwd[bytes.len()] = 0;
    }).is_ok()
}

/// Resolve a path against a process's cwd (for relative paths).
pub fn resolve_path_cwd(pid: ProcessId, path: &str) -> alloc::string::String {
    if path.starts_with('/') {
        return alloc::string::String::from(path);
    }
    let cwd = get_process_cwd(pid);
    if cwd == "/" {
        alloc::format!("/{}", path)
    } else {
        alloc::format!("{}/{}", cwd, path)
    }
}
