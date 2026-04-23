//! Gestión de procesos y context switching

use core::arch::asm;
use spin::Mutex;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

/// ID de proceso
pub type ProcessId = u32;

// argv pendiente (syscalls 542/543): el padre registra bytes NUL-separados antes de
// encolar al hijo. No se consume al leer (varios lectores / crt + libc); se libera
// en `exit_process` para el PID que termina.
static PENDING_PROCESS_ARGS: Mutex<BTreeMap<ProcessId, Vec<u8>>> = Mutex::new(BTreeMap::new());

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
}

/// Virtual Memory Area (VMA) region
#[derive(Clone, Copy, Debug)]
pub struct VMARegion {
    pub start: u64,
    pub end: u64,
    pub flags: u64,
    pub file_backed: bool,
    /// Bytes mapeados de más en `mmap` anónimo (colchón del kernel). `mprotect` de musl solo
    /// cubre la longitud pedida; hay que extender el rango para quitar NX en esas páginas.
    pub anon_kernel_slack: u64,
}

impl VMARegion {
    /// Returns true if other is immediately adjacent to this VMA and has identical properties.
    pub fn can_merge(&self, other: &Self) -> bool {
        self.end == other.start 
            && self.flags == other.flags 
            && self.file_backed == other.file_backed 
            && self.anon_kernel_slack == other.anon_kernel_slack
    }
}

/// Estructura de contexto salvado de un proceso
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Context {
    // Registros de propósito general
    pub rax: u64, // 0x00
    pub rbx: u64, // 0x08
    pub rcx: u64, // 0x10
    pub rdx: u64, // 0x18
    pub rsi: u64, // 0x20
    pub rdi: u64, // 0x28
    pub rbp: u64, // 0x30
    pub r8: u64,  // 0x38
    pub r9: u64,  // 0x40
    pub r10: u64, // 0x48
    pub r11: u64, // 0x50
    pub r12: u64, // 0x58
    pub r13: u64, // 0x60
    pub r14: u64, // 0x68
    pub r15: u64, // 0x70
    
    // Punteros y estado
    pub rsp: u64,    // 0x78
    pub rip: u64,    // 0x80
    pub rflags: u64, // 0x88
}

/// Contexto de recursos compartidos entre hilos (proceso lógico)
pub struct ProcessResources {
    pub page_table_phys: u64,          // Physical address of the PML4
    pub vmas: Vec<VMARegion>,          // Memory mappings
    pub brk_current: u64,              // Current program break (heap end)
    pub fd_table_idx: usize,           // Index into FD_TABLES
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
        // Free the page table and all mapped physical frames.
        // teardown_process_paging safely ignores the kernel page table.
        crate::memory::teardown_process_paging(self.page_table_phys);
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
pub const SS_ONSTACK: u32 = 1;
pub const SS_DISABLE: u32 = 2;

/// Complete signal action — mirrors Linux `struct sigaction` for x86-64.
///
/// Layout (matches `struct sigaction` passed by musl/glibc via `rt_sigaction`):
///   [0..8]   handler  — `SIG_DFL` = 0, `SIG_IGN` = 1, or userspace fn ptr
///   [8..16]  flags    — `SA_*` flags
///   [16..24] restorer — `sa_restorer` pointer (required when `SA_RESTORER` is set)
///   [24..32] mask     — signals to block during this handler (64-bit bitmask)
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

/// Alternate signal stack — mirrors Linux `stack_t`.
#[derive(Clone, Copy)]
pub struct Sigaltstack {
    pub ss_sp:    u64,
    pub ss_flags: u32,
    pub ss_size:  u64,
}

impl Sigaltstack {
    pub const fn new() -> Self {
        Self { ss_sp: 0, ss_flags: SS_DISABLE, ss_size: 0 }
    }
}

/// Process Control Block
#[derive(Clone)]
pub struct Process {
    pub id: ProcessId,
    pub state: ProcessState,
    pub context: Context,
    pub stack_base: u64,
    pub stack_size: usize,
    pub priority: u8,
    pub vruntime: u64,
    pub weight: u64,
    pub time_slice: u32,
    pub parent_pid: Option<ProcessId>, // Parent process ID for fork()
    pub kernel_stack_top: u64,         // Top of the kernel stack (RSP0)
    pub resources: Arc<Mutex<ProcessResources>>, // Shared resources (VM, FDs, etc)
    pub fs_base: u64,                     // TLS base (FS_BASE)
    pub gs_base: u64,                     // Kernel/User swap GS base
    pub is_linux: bool,                   // Use Linux ABI translation
    pub wake_tick: u64,                   // Timer tick at which to wake from Blocked sleep (0 = not sleeping)
    pub name: [u8; 16],                   // Process name (truncated to 16 bytes)
    pub cpu_ticks: u64,                   // Total CPU ticks consumed
    pub mem_frames: u64,                  // Approximate physical memory usage in frames
    pub current_cpu: u32,                 // CPU currently executing this process (SMP safety); NO_CPU = not running
    pub last_cpu: u32,                    // Last CPU that executed this process (for cache affinity)
    pub exit_code: u64,                   // Exit code passed to sys_exit; read by sys_wait
    pub exit_signal: u8,                  // Signal that caused termination or stopping
    pub cpu_affinity: Option<u32>,        // None = any CPU; Some(cpu_id) = pin to that CPU
    pub ai_profile: crate::ai_core::ProcessProfile, // AI Behavior statistics
    /// Máscara de señales pendientes (bit N = señal N pendiente).
    pub pending_signals: u64,
    /// Máscara de señales bloqueadas (bit N = señal N bloqueada).
    pub signal_mask: u64,
    /// Acciones de señal registradas por el proceso vía rt_sigaction.
    pub signal_actions: [SignalAction; 64],
    /// Pila alternativa para manejadores de señal (sigaltstack).
    pub sigaltstack: Sigaltstack,
    /// Process Group ID
    pub pgid: ProcessId,
    /// Session ID
    pub sid: ProcessId,
    /// `Some((AT_BASE, AT_ENTRY))` si el arranque es vía intérprete dinámico (ld-musl).
    pub dynamic_linker_aux: Option<(u64, u64)>,
    /// Current working directory (null-terminated, max 511 chars + NUL).
    pub cwd: [u8; 512],
    pub cwd_len: usize,
    /// Si es true, cada syscall de este proceso se registra en serial (syscall `strace`, 545).
    pub syscall_trace: bool,
    /// Address to write 0 and wake on futex on exit (CLONE_CHILD_CLEARTID).
    pub clear_child_tid: u64,
    /// Address to write TID on clone (CLONE_CHILD_SETTID).
    pub set_child_tid: u64,
    /// Thread Group ID (the "PID" seen by userspace).
    pub tgid: ProcessId,
    /// Linux `CLONE_VFORK`: el padre permanece dentro de `clone` hasta que este hijo
    /// ejecuta `execve` con éxito o termina (`exit`). `Some(child_pid)` mientras espera.
    pub vfork_waiting_for_child: Option<ProcessId>,
    /// Linux `CLONE_VM` + vfork: este proceso comparte `ProcessResources` (y CR3) con el padre
    /// hasta el primer `exec` exitoso; entonces se hace copia de VM + FD propia.
    pub vfork_shared_mm_with_parent: Option<ProcessId>,
}

/// Sentinel value for current_cpu meaning "not owned by any CPU"
pub const NO_CPU: u32 = u32::MAX;


impl Process {
    pub fn new(resources: Arc<Mutex<ProcessResources>>) -> Self {
        Self {
            id: 0,
            state: ProcessState::Blocked,
            context: Context::new(),
            stack_base: 0,
            stack_size: 0,
            priority: 0,
            vruntime: 0,
            weight: 1024, // NICE_0_LOAD
            time_slice: 0,
            parent_pid: None,
            kernel_stack_top: 0,
            resources,
            fs_base: 0,
            gs_base: 0,
            is_linux: true,
            wake_tick: 0,
            name: [0; 16],
            cpu_ticks: 0,
            mem_frames: 0,
            current_cpu: NO_CPU,
            last_cpu: NO_CPU,
            exit_code: 0,
            exit_signal: 0,
            cpu_affinity: None,
            ai_profile: crate::ai_core::ProcessProfile::new(),
            pending_signals: 0,
            signal_mask: 0,
            signal_actions: [const { SignalAction::new() }; 64],
            sigaltstack: Sigaltstack::new(),
            pgid: 0,
            sid: 0,
            dynamic_linker_aux: None,
            cwd: {
                let mut buf = [0u8; 512];
                buf[0] = b'/';
                buf
            },
            cwd_len: 1,
            syscall_trace: false,
            clear_child_tid: 0,
            set_child_tid: 0,
            tgid: 0,
            vfork_waiting_for_child: None,
            vfork_shared_mm_with_parent: None,
        }
    }
}

/// Despertar al padre que hizo `vfork`/`clone(CLONE_VFORK|CLONE_VM)` esperando a este hijo.
/// Linux: el padre sale de `clone` cuando el hijo hace `execve` exitoso o `_exit`.
/// Antes de cargar un ELF en `exec*`, si el proceso es hijo vfork con `CLONE_VM`,
/// duplicar la tabla de páginas y la tabla de FDs del padre para no pisar la imagen del padre.
pub fn vfork_detach_mm_for_exec_if_needed(pid: ProcessId) -> Result<(), &'static str> {
    let needs_detach = get_process(pid)
        .map(|p| p.vfork_shared_mm_with_parent.is_some())
        .unwrap_or(false);
    if !needs_detach {
        return Ok(());
    }

    let child_slot = crate::ipc::pid_to_slot_fast(pid).ok_or("vfork detach: no slot")?;

    let (shared_pt, src_fd_slot, vmas, brk) = {
        let p = get_process(pid).ok_or("vfork detach: process")?;
        let r = p.resources.lock();
        (r.page_table_phys, r.fd_table_idx, r.vmas.clone(), r.brk_current)
    };

    if src_fd_slot != child_slot {
        crate::fd::fd_duplicate_table_slots(src_fd_slot, child_slot);
    }

    let new_cr3 = crate::memory::clone_process_paging(shared_pt);

    let new_resources = Arc::new(Mutex::new(ProcessResources {
        page_table_phys: new_cr3,
        vmas,
        brk_current: brk,
        fd_table_idx: child_slot,
    }));

    modify_process(pid, |p| {
        p.resources = new_resources;
        p.vfork_shared_mm_with_parent = None;
    })
    .map_err(|_| "vfork detach: process vanished")?;

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
                if p.vfork_waiting_for_child == Some(child_pid) {
                    p.vfork_waiting_for_child = None;
                    return;
                }
            }
        }
    });
}


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
        let mut process = Process::new(resources);
        process.id = 0;
        process.state = ProcessState::Running;
        process.current_cpu = 0;
        process.last_cpu = 0;
        process.priority = 0;
        process.time_slice = 10;
        process.kernel_stack_top = kernel_stack_top_aligned;
        let name = b"kernel";
        let len = core::cmp::min(name.len(), 16);
        process.name[..len].copy_from_slice(&name[..len]);
        
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
                *slot = None; 
                
                // Allocate a unique FD table index for the new process resources
                // For simplicity, we use same slot_idx as the fd_table_idx initially.
                let resources = Arc::new(Mutex::new(ProcessResources::new(cr3, slot_idx)));
                {
                    let mut r = resources.lock();
                    r.brk_current = initial_brk;
                }

                let mut process = Process::new(resources);
                process.id = pid;
                process.tgid = pid;
                process.stack_base = stack_base;
                process.stack_size = stack_size;
                process.priority = 5; 
                process.vruntime = 0;
                process.weight = 1024;
                process.time_slice = 10; 
                
                let kernel_stack_top_aligned = kernel_stack_top & !0xF;

                process.dynamic_linker_aux = dynamic_linker_aux;
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
                process.mem_frames = (stack_size / 4096) as u64; 
                process.fs_base = tls_base;
                
                crate::serial::serial_printf(format_args!(
                    "[PROC] Created process PID: {} slot: {} CR3: {:#018X}\n",
                    pid, slot_idx, cr3
                ));

                *slot = Some(process);
                // Registrar en tabla inversa PID → slot O(1) para IPC
                crate::ipc::register_pid_slot(pid, slot_idx);
                return true;
            }
        }
        false
    })
}

/// Registra en `vmas` los segmentos ELF cargados y la pila fija `[stack_base, stack_base+stack_size)`.
///
/// Tras `exec` / `execve` el kernel vacía `vmas`; si no se vuelven a registrar, un `mmap` con
/// pista o `MAP_FIXED` puede solapar `unmap_user_range` con la pila en 0x20000000..0x20100000
/// y provocar #PF en RSP; además el fallo bajo demanda no puede reponer hojas sin VMA.
pub fn register_post_exec_vm_as(
    pid: ProcessId,
    loaded: &crate::elf_loader::ExecLoadResult,
    stack_base: u64,
    stack_size: u64,
) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        if let Some(mut proc) = get_process(pid) {
            let mut r = proc.resources.lock();
            for i in 0..loaded.loaded_vma_count {
                let (start, end) = loaded.loaded_vma_ranges[i];
                if start < end {
                    r.vmas.push(VMARegion {
                        start,
                        end,
                        flags:             0x5, // PROT_READ | PROT_EXEC
                        file_backed:       true,
                        anon_kernel_slack: 0,
                    });
                }
            }
            r.vmas.push(VMARegion {
                start:             stack_base,
                end:               stack_base.saturating_add(stack_size),
                flags:             0x3, // PROT_READ | PROT_WRITE
                file_backed:       false,
                anon_kernel_slack: 0,
            });
            drop(r);
            update_process(pid, proc);
        }
    });
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
        x86_64::instructions::interrupts::without_interrupts(|| {
            let mut table = PROCESS_TABLE.lock();
            if let Some(p) = table.iter_mut().find(|s| s.as_ref().map_or(false, |p| p.id == pid)) {
                if let Some(proc) = p {
                    let n = core::cmp::min(name.len(), 16);
                    proc.name[..n].copy_from_slice(&name.as_bytes()[..n]);
                    proc.mem_frames += loaded.segment_frames;
                    if loaded.tls_base != 0 {
                        proc.fs_base = loaded.tls_base;
                    }
                    proc.dynamic_linker_aux = loaded.dynamic_linker;
                    // is_linux is always true (all processes use Linux/musl ABI)
                }
            }
        });
        if let Some(parent_pid) = current_process_id() {
             if let Some(parent) = get_process(parent_pid) {
                 if let Some(mut proc) = get_process(pid) {
                     proc.pgid = parent.pgid;
                     proc.sid = parent.sid;
                     proc.mem_frames = (0x100000 / 4096) + loaded.segment_frames; // stack + segments
                    if loaded.tls_base != 0 {
                        proc.fs_base = loaded.tls_base;
                    }
                    proc.dynamic_linker_aux = loaded.dynamic_linker;
                    crate::process::update_process(pid, proc);
                 }
             }
        }
        crate::fd::fd_init_stdio(pid);
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
                let p_name_len = p.name.iter().position(|&b| b == 0).unwrap_or(16);
                if p_name_len == name_len && &p.name[..name_len] == &name_bytes[..name_len] {
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
            return process.resources.lock().page_table_phys;
        }
    }
    0
}

/// Actualizar proceso (seguro para SMP)
pub fn update_process(pid: ProcessId, mut new_process: Process) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut table = PROCESS_TABLE.lock();
        if (pid as usize) < table.len() {
            if let Some(p) = table[pid as usize].as_mut() {
                // After slot reuse, table[pid] may hold a *different* process whose PID
                // happens to equal the slot index (e.g. slot 5 now holds PID 70).
                // Overwriting it without checking p.id == pid would corrupt the new
                // process's PCB with stale data from the old one.  Fall through to the
                // linear scan when the slot is occupied by a different process.
                if p.id == pid {
                    // PRESERVAR ESTADO ATÓMICO: Ownership y State actual son sagrados.
                    // Solo permitimos que update_process cambie metadatos.
                    // Si el proceso cambió de dueño o estado mientras el llamador lo editaba,
                    // mantenemos los valores de la tabla real.
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
        if (pid as usize) < table.len() {
            if let Some(p) = table[pid as usize].as_mut() {
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
        clear_pending_process_args(pid);
        // Linux vfork: el padre bloqueado en `clone` debe continuar si el hijo sale sin exec.
        vfork_wake_parent_waiting_for_child(pid);

        // Collect open file descriptors so we can close them outside the lock
        let mut to_close: [(usize, usize); crate::fd::MAX_FDS_PER_PROCESS] =
            [(0, 0); crate::fd::MAX_FDS_PER_PROCESS];
        let mut close_count = 0;

        x86_64::instructions::interrupts::without_interrupts(|| {
            let mut tables = crate::fd::FD_TABLES.lock();
            // Use the slot index (not the raw PID) to index FD_TABLES.
            // pid_to_slot_fast is safe to call here: the process is still in PROCESS_TABLE
            // (we haven't marked it Terminated yet) so the slot lookup will succeed.
            let pid_idx = match crate::ipc::pid_to_slot_fast(pid) {
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
                    if p.id == pid {
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
                    if p.id == pid { p.parent_pid } else { None }
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
    // Clone of parent released immediately — no lock held for the expensive work below.
    let parent = get_process(current_pid)?;

    // ── Phase 1: Expensive work BEFORE acquiring any kernel-global lock ──────

    // Deep copy of the parent's user address space.
    let child_cr3 = crate::memory::clone_process_paging(parent.resources.lock().page_table_phys);

    // Allocate a fresh kernel stack for the child.
    let kernel_stack = alloc::vec![0u8; KERNEL_STACK_SIZE];
    let kernel_stack_top = kernel_stack.as_ptr() as u64 + KERNEL_STACK_SIZE as u64;
    let kernel_stack_top_aligned = kernel_stack_top & !0xF;
    core::mem::forget(kernel_stack); // Leak intentionally (no proper dealloc yet)

    // Build the IRETQ frame on the child's kernel stack.
    // layout (low→high in memory): RIP, CS, RFLAGS, RSP, SS
    let kstack_ptr = unsafe {
        let mut p = kernel_stack_top_aligned as *mut u64;
        p = p.offset(-1); *p = 0x23;                  // SS  (user data)
        p = p.offset(-1); *p = parent_context.rsp;    // RSP (user stack)
        p = p.offset(-1); *p = parent_context.rflags; // RFLAGS
        p = p.offset(-1); *p = 0x1b;                  // CS  (user code)
        p = p.offset(-1); *p = parent_context.rip;    // RIP
        p
    };

    // ── Phase 2: Brief critical section — insert child into PROCESS_TABLE ───

    let result = x86_64::instructions::interrupts::without_interrupts(|| {
        let mut table = PROCESS_TABLE.lock();
        let mut next_pid = NEXT_PID.lock();

        for (slot_idx, slot) in table.iter_mut().enumerate() {
            // Accept both empty slots and slots whose previous occupant has Terminated
            // (and has been fully released by the owning CPU).  Without this, fork()
            // fails permanently once the table fills with a mix of live and Terminated
            // processes — the same condition already handled by create_process_with_pid.
            let slot_available = slot.is_none()
                || matches!(slot, Some(ref p) if
                    p.state == ProcessState::Terminated
                    && p.current_cpu == NO_CPU);
            if slot_available {
                *slot = None; // evict Terminated entry before writing the child
                let child_pid = *next_pid;
                *next_pid += 1;

                // Clone of resources for fork()
                let child_resources = {
                    let p = parent.resources.lock();
                    Arc::new(Mutex::new(ProcessResources {
                        page_table_phys: child_cr3,
                        vmas: p.vmas.clone(),
                        brk_current: p.brk_current,
                        fd_table_idx: slot_idx, // Use the new slot index for isolated FD table copy
                    }))
                };

                let mut child = Process::new(child_resources);
                child.id = child_pid;
                child.tgid = child_pid;
                child.state = ProcessState::Blocked;
                child.current_cpu = NO_CPU;
                child.last_cpu = NO_CPU;
                child.parent_pid = Some(current_pid);
                child.kernel_stack_top = kernel_stack_top_aligned;
                
                // Inherit missing fields (CRITICAL for TLS/Libc stability)
                child.fs_base = parent.fs_base;
                child.dynamic_linker_aux = parent.dynamic_linker_aux;
                child.gs_base = parent.gs_base;
                // is_linux always true (all processes use Linux/musl ABI)
                child.priority = parent.priority;
                child.vruntime = parent.vruntime;
                child.weight = parent.weight;
                child.time_slice = parent.time_slice;
                child.stack_base = parent.stack_base;
                child.stack_size = parent.stack_size;
                child.mem_frames = parent.mem_frames;
                child.cpu_affinity = parent.cpu_affinity;
                child.exit_signal = 0;
                child.syscall_trace = parent.syscall_trace;

                // Signal inheritance (POSIX fork semantics):
                // - child inherits signal dispositions and mask
                // - child's pending signals are cleared (done by Process::new())
                // - child inherits alternate signal stack
                child.signal_actions = parent.signal_actions;
                child.signal_mask    = parent.signal_mask;
                child.sigaltstack    = parent.sigaltstack;

                // Inherit process group, session, and working directory.
                child.pgid    = parent.pgid;
                child.sid     = parent.sid;
                child.cwd     = parent.cwd;
                child.cwd_len = parent.cwd_len;

                // Keep parent name (child will overwrite it with set_process_name if needed)
                let mut name = [0u8; 16];
                let parent_name_len = parent.name.iter().position(|&b| b == 0).unwrap_or(16);
                let copy_len = core::cmp::min(parent_name_len, 16);
                name[..copy_len].copy_from_slice(&parent.name[..copy_len]);
                child.name = name;

                // Set up context so the child resumes via fork_child_setup (which clears locks).
                child.context.rip = crate::interrupts::fork_child_setup as *const () as u64;
                child.context.rsp = kstack_ptr as u64;
                child.context.rax = 0; // fork() returns 0 in the child
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

                *slot = Some(child);
                // Register in O(1) IPC lookup table before releasing the lock.
                crate::ipc::register_pid_slot(child_pid, slot_idx);
                return Some((child_pid, slot_idx));
            }
        }
        None
    });

    let (child_pid, _slot_idx) = result?;

    // ── Phase 3: Clone FDs under FD_TABLES lock only (no PROCESS_TABLE) ────
    crate::fd::fd_clone_for_fork(current_pid, child_pid);

    Some(child_pid)
}

/// `clone(CLONE_VM|CLONE_VFORK|…)` sin `CLONE_THREAD`: hijo comparte el mismo `ProcessResources`
/// (tabla de páginas y `fd_table_idx`) que el padre hasta `execve` / `exit` (comportamiento Linux).
pub fn vfork_process_shared_vm(parent_context: &Context) -> Option<ProcessId> {
    let current_pid = current_process_id()?;
    let parent = get_process(current_pid)?;

    let child_resources = Arc::clone(&parent.resources);

    let kernel_stack = alloc::vec![0u8; KERNEL_STACK_SIZE];
    let kernel_stack_top = kernel_stack.as_ptr() as u64 + KERNEL_STACK_SIZE as u64;
    let kernel_stack_top_aligned = kernel_stack_top & !0xF;
    core::mem::forget(kernel_stack);

    let kstack_ptr = unsafe {
        let mut p = kernel_stack_top_aligned as *mut u64;
        p = p.offset(-1);
        *p = 0x23;
        p = p.offset(-1);
        *p = parent_context.rsp;
        p = p.offset(-1);
        *p = parent_context.rflags;
        p = p.offset(-1);
        *p = 0x1b;
        p = p.offset(-1);
        *p = parent_context.rip;
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
                *slot = None;
                let child_pid = *next_pid;
                *next_pid += 1;

                let mut child = Process::new(child_resources);
                child.id = child_pid;
                child.tgid = child_pid;
                child.state = ProcessState::Blocked;
                child.current_cpu = NO_CPU;
                child.last_cpu = NO_CPU;
                child.parent_pid = Some(current_pid);
                child.kernel_stack_top = kernel_stack_top_aligned;
                child.vfork_shared_mm_with_parent = Some(current_pid);

                child.fs_base = parent.fs_base;
                child.dynamic_linker_aux = parent.dynamic_linker_aux;
                child.gs_base = parent.gs_base;
                // is_linux always true (all processes use Linux/musl ABI)
                child.priority = parent.priority;
                child.time_slice = parent.time_slice;
                child.stack_base = parent.stack_base;
                child.stack_size = parent.stack_size;
                child.mem_frames = parent.mem_frames;
                child.cpu_affinity = parent.cpu_affinity;
                child.exit_signal = 0;
                child.syscall_trace = parent.syscall_trace;

                // Signal inheritance (POSIX fork semantics).
                child.signal_actions = parent.signal_actions;
                child.signal_mask    = parent.signal_mask;
                child.sigaltstack    = parent.sigaltstack;
                child.pgid    = parent.pgid;
                child.sid     = parent.sid;
                child.cwd     = parent.cwd;
                child.cwd_len = parent.cwd_len;

                let mut name = [0u8; 16];
                child.context.rcx = parent_context.rcx;
                child.context.rdx = parent_context.rdx;
                child.context.rsi = parent_context.rsi;
                child.context.rdi = parent_context.rdi;
                child.context.rbp = parent_context.rbp;
                child.context.r8 = parent_context.r8;
                child.context.r9 = parent_context.r9;
                child.context.r10 = parent_context.r10;
                child.context.r11 = parent_context.r11;
                child.context.r12 = parent_context.r12;
                child.context.r13 = parent_context.r13;
                child.context.r14 = parent_context.r14;
                child.context.r15 = parent_context.r15;

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
    {
        let mut table = PROCESS_TABLE.lock();
        for slot in table.iter_mut() {
            if let Some(p) = slot {
                if p.id == pid {
                    p.pending_signals |= 1u64 << signum;
                    
                    // Job Control: handle stop/cont signals
                    if signum == 19 || signum == 20 { // SIGSTOP or SIGTSTP
                        p.state = ProcessState::Stopped;
                        p.exit_signal = signum;
                        let parent_pid = p.parent_pid;
                        if let Some(ppid) = parent_pid {
                            wake_parent_from_wait(ppid);
                        }
                    } else if signum == 18 { // SIGCONT
                        if p.state == ProcessState::Stopped || p.state == ProcessState::Blocked {
                            // Don't set state here; enqueue_process will do it.
                            to_wake = Some(pid);
                            let parent_pid = p.parent_pid;
                            if let Some(ppid) = parent_pid {
                                wake_parent_from_wait(ppid);
                            }
                        }
                    } else {
                        // Despertar al proceso si está bloqueado por otras señales
                        if p.state == ProcessState::Blocked {
                            to_wake = Some(pid);
                        }
                    }
                    break;
                }
            }
        }
    }
    if let Some(pid_to_wake) = to_wake {
        crate::scheduler::enqueue_process(pid_to_wake);
    }
}

/// Consume (limpia) un bit de señal del proceso actual y devuelve la acción registrada.
/// Devuelve None si no hay señal pendiente o no hay proceso actual.
pub fn consume_pending_signal(pid: ProcessId, signum: u8) -> Option<SignalAction> {
    if signum >= 64 { return None; }
    let mask = 1u64 << signum;
    let mut table = PROCESS_TABLE.lock();
    for slot in table.iter_mut() {
        if let Some(p) = slot {
            if p.id == pid && (p.pending_signals & mask) != 0 {
                p.pending_signals &= !mask;
                return Some(p.signal_actions[signum as usize]);
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

/// Extrae la señal pendiente de menor número, la borra del bitmask y devuelve (número, acción).
/// **Respeta la máscara de señales (p.signal_mask)**, excepto para SIGKILL (9) y SIGSTOP (19).
pub fn pop_lowest_pending_signal(pid: ProcessId) -> Option<(u8, SignalAction)> {
    let mut table = PROCESS_TABLE.lock();
    for slot in table.iter_mut() {
        if let Some(p) = slot {
            if p.id != pid || p.pending_signals == 0 {
                continue;
            }
            // Filtramos las señales que NO están bloqueadas.
            // SIGKILL (9) y SIGSTOP (19) no se pueden bloquear (máscara 1 << 9 y 1 << 19).
            let unblockable = (1 << 9) | (1 << 19);
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
            let action = p.signal_actions[sig as usize];
            return Some((sig, action));
        }
    }
    None
}

/// Termina `target_pid` con `exit_code = 128 + sig` (como `sys_kill` fatal).
/// Devuelve `None` si no existe, `Some(None)` si ya era zombie, `Some(Some(ppid))` si se mató.
pub fn terminate_other_process_by_signal(target_pid: ProcessId, sig: u8) -> Option<Option<ProcessId>> {
    let result = x86_64::instructions::interrupts::without_interrupts(|| {
        let mut table = PROCESS_TABLE.lock();
        for (slot_idx, slot) in table.iter_mut().enumerate() {
            if let Some(p) = slot {
                if p.id == target_pid {
                    if p.state == ProcessState::Terminated {
                        return Some(None);
                    }
                    p.exit_code = 128 + sig as u64;
                    p.exit_signal = sig;
                    p.state = ProcessState::Terminated;
                    crate::ipc::clear_mailbox_slot(slot_idx);
                    return Some(p.parent_pid);
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
            proc.exit_code = 128 + sig as u64;
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
        let len = p.cwd_len.min(511);
        alloc::string::String::from_utf8_lossy(&p.cwd[..len]).into_owned()
    } else {
        alloc::string::String::from("/")
    }
}

/// Set the current working directory of a process.
/// Returns true on success, false if path is too long.
pub fn set_process_cwd(pid: ProcessId, new_cwd: &str) -> bool {
    let bytes = new_cwd.as_bytes();
    if bytes.len() > 511 {
        return false;
    }
    modify_process(pid, |p| {
        p.cwd_len = bytes.len();
        p.cwd[..bytes.len()].copy_from_slice(bytes);
        p.cwd[bytes.len()] = 0;
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
