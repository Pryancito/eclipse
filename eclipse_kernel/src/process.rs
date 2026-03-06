//! Gestión de procesos y context switching

use core::arch::asm;
use spin::Mutex;
use alloc::sync::Arc;
use alloc::vec::Vec;

/// ID de proceso
pub type ProcessId = u32;
pub const KERNEL_STACK_SIZE: usize = 32768; // 32KB stack for kernel operations

/// Estado de un proceso
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ProcessState {
    Ready,
    Running,
    Blocked,
    Terminated,
}

/// Virtual Memory Area (VMA) region
#[derive(Clone, Copy, Debug)]
pub struct VMARegion {
    pub start: u64,
    pub end: u64,
    pub flags: u64,
    pub file_backed: bool,
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

/// Process Control Block
#[derive(Clone)]
pub struct Process {
    pub id: ProcessId,
    pub state: ProcessState,
    pub context: Context,
    pub stack_base: u64,
    pub stack_size: usize,
    pub priority: u8,
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
    pub exit_code: u64,                   // Exit code passed to sys_exit; read by sys_wait
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
            time_slice: 0,
            parent_pid: None,
            kernel_stack_top: 0,
            resources,
            fs_base: 0,
            gs_base: 0,
            is_linux: false,
            wake_tick: 0,
            name: [0; 16],
            cpu_ticks: 0,
            mem_frames: 0,
            current_cpu: NO_CPU,
            exit_code: 0,
        }
    }
}


/// Tabla de procesos
pub const MAX_PROCESSES: usize = 256;
pub static PROCESS_TABLE: Mutex<[Option<Process>; MAX_PROCESSES]> = Mutex::new([const { None }; MAX_PROCESSES]);
static NEXT_PID: Mutex<ProcessId> = Mutex::new(1);

/// Máximo número de CPUs soportadas
const MAX_CPUS: usize = 128;
/// Proceso actual por cada CPU

/// Obtener ID de la CPU actual (O(1) vía GS segment)
pub fn get_cpu_id() -> usize {
    crate::boot::get_cpu_id_gs()
}

pub fn next_pid() -> ProcessId {
    let mut next_pid = NEXT_PID.lock();
    let pid = *next_pid;
    *next_pid += 1;
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
pub fn create_process(entry_point: u64, stack_base: u64, stack_size: usize, phdr_va: u64, phnum: u64, phentsize: u64, initial_brk: u64) -> Option<ProcessId> {
    let pid = next_pid();
    let cr3 = crate::memory::create_process_paging();
    
    if create_process_with_pid(pid, cr3, entry_point, stack_base, stack_size, phdr_va, phnum, phentsize, initial_brk) {
        Some(pid)
    } else {
        None
    }
}

/// Inicializar un proceso con un PID y espacio de direcciones ya creados.
/// phdr_va, phnum, and phentsize are passed to jump_to_userspace for the auxv (AT_PHDR, AT_PHNUM, AT_PHENT).
pub fn create_process_with_pid(pid: ProcessId, cr3: u64, entry_point: u64, stack_base: u64, stack_size: usize, phdr_va: u64, phnum: u64, phentsize: u64, initial_brk: u64) -> bool {
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
                process.stack_base = stack_base;
                process.stack_size = stack_size;
                process.priority = 5; 
                process.time_slice = 10; 
                
                let kernel_stack_top_aligned = kernel_stack_top & !0xF;

                process.context.rip = crate::elf_loader::jump_to_userspace as *const () as u64;
                process.context.rdi = entry_point;                            
                process.context.rsi = stack_base + stack_size as u64;         
                process.context.rdx = phdr_va;                                
                process.context.rcx = phnum;                                  
                process.context.r8 = phentsize;                               
                process.context.rsp = kernel_stack_top_aligned;               
                process.context.rflags = 0x002; 
                process.kernel_stack_top = kernel_stack_top_aligned; 
                process.mem_frames = (stack_size / 4096) as u64; 
                
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

/// Ejecutar un binario ELF como un nuevo proceso
pub fn spawn_process(elf_data: &[u8], name: &str) -> Result<ProcessId, &'static str> {
    crate::serial::serial_printf(format_args!("[spawn] ENTERED for process: {}\n", name));
    let pid = next_pid();

    crate::serial::serial_print("[spawn] calling create_process_paging\n");
    let cr3 = crate::memory::create_process_paging();
    crate::serial::serial_printf(format_args!("[spawn] create_process_paging returned cr3=0x{:x}\n", cr3));

    crate::serial::serial_print("[spawn] calling load_elf_into_space\n");
    let (entry_point, max_vaddr, segment_frames) = crate::elf_loader::load_elf_into_space(cr3, elf_data)?;
    crate::serial::serial_printf(format_args!("[spawn] load_elf_into_space done entry=0x{:x}\n", entry_point));
    let (phdr_va, phnum, phentsize) = crate::elf_loader::get_elf_phdr_info(elf_data)?;

    crate::serial::serial_print("[spawn] calling setup_user_stack\n");
    let stack_base = 0x20000000;
    let stack_size = 0x100000; // 1MB — enough for deep compositor render call stacks
    let _stack_top = crate::elf_loader::setup_user_stack(cr3, stack_base, stack_size)?;
    crate::serial::serial_print("[spawn] setup_user_stack done\n");

    crate::serial::serial_print("[spawn] calling create_process_with_pid\n");
    if create_process_with_pid(pid, cr3, entry_point, stack_base, stack_size, phdr_va, phnum, phentsize, max_vaddr) {
        if let Some(mut proc) = get_process(pid) {
            let n = core::cmp::min(name.len(), 16);
            proc.name[..n].copy_from_slice(&name.as_bytes()[..n]);
            proc.mem_frames += segment_frames;
            update_process(pid, proc);
        }
        crate::fd::fd_init_stdio(pid);
        crate::serial::serial_printf(format_args!("[spawn] SUCCESS for process: {}\n", name));
        Ok(pid)
    } else {
        crate::serial::serial_printf(format_args!("[spawn] FAILED to insert process into table: {}\n", name));
        Err("Failed to insert process into table")
    }
}

/// Obtener proceso actual (O(1) vía GS segment, sin lock)
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
                        p.state = ProcessState::Terminated;
                        // Eliminar de la tabla inversa PID → slot (O(1) para IPC)
                        crate::ipc::unregister_pid_slot(pid);
                        // Limpiar el buzón IPC para que el próximo proceso del slot
                        // no reciba mensajes del anterior.
                        crate::ipc::clear_mailbox_slot(slot_idx);
                        break;
                    }
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
                child.state = ProcessState::Blocked;
                child.current_cpu = NO_CPU;
                child.parent_pid = Some(current_pid);
                child.kernel_stack_top = kernel_stack_top_aligned;
                
                // Inherit missing fields (CRITICAL for TLS/Libc stability)
                child.fs_base = parent.fs_base;
                child.gs_base = parent.gs_base;
                child.is_linux = parent.is_linux;
                child.priority = parent.priority;
                child.time_slice = parent.time_slice;
                child.stack_base = parent.stack_base;
                child.stack_size = parent.stack_size;
                child.mem_frames = parent.mem_frames;

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
