//! Gestión de procesos y context switching

use core::arch::asm;
use spin::Mutex;

/// ID de proceso
pub type ProcessId = u32;

/// Estado de un proceso
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ProcessState {
    Ready,
    Running,
    Blocked,
    Terminated,
}

/// Estructura de contexto salvado de un proceso
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Context {
    // Registros de propósito general
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    
    // Stack pointer
    pub rsp: u64,
    
    // Instruction pointer
    pub rip: u64,
    
    // Flags
    pub rflags: u64,
}

impl Context {
    pub const fn new() -> Self {
        Self {
            rax: 0,
            rbx: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            rbp: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rsp: 0,
            rip: 0,
            rflags: 0x002, // IF disabled by default
        }
    }
}

/// Process Control Block
#[derive(Clone, Copy)]
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
}

impl Process {
    pub const fn new() -> Self {
        Self {
            id: 0,
            state: ProcessState::Ready,
            context: Context::new(),
            stack_base: 0,
            stack_size: 0,
            priority: 0,
            time_slice: 0,
            parent_pid: None,
            kernel_stack_top: 0,
        }
    }
}

/// Tabla de procesos
const MAX_PROCESSES: usize = 64;
pub static PROCESS_TABLE: Mutex<[Option<Process>; MAX_PROCESSES]> = Mutex::new([None; MAX_PROCESSES]);
static NEXT_PID: Mutex<ProcessId> = Mutex::new(1);
static CURRENT_PROCESS: Mutex<Option<ProcessId>> = Mutex::new(None);

// Stack pool for forked processes (simple static allocation)
const STACK_POOL_SIZE: usize = 8; // Support up to 8 concurrent child processes
const CHILD_STACK_SIZE: usize = 262144; // 256KB
#[repr(align(16))]
struct StackPool {
    stacks: [[u8; CHILD_STACK_SIZE]; STACK_POOL_SIZE],
    used: [bool; STACK_POOL_SIZE],
}

static mut STACK_POOL: StackPool = StackPool {
    stacks: [[0; CHILD_STACK_SIZE]; STACK_POOL_SIZE],
    used: [false; STACK_POOL_SIZE],
};

static STACK_POOL_LOCK: Mutex<()> = Mutex::new(());

/// Allocate a stack from the pool
fn allocate_stack() -> Option<u64> {
    let _lock = STACK_POOL_LOCK.lock();
    unsafe {
        for i in 0..STACK_POOL_SIZE {
            if !STACK_POOL.used[i] {
                STACK_POOL.used[i] = true;
                return Some(STACK_POOL.stacks[i].as_mut_ptr() as u64);
            }
        }
    }
    None
}


/// Inicializar el proceso kernel (PID 0)
pub fn init_kernel_process() {
    let mut table = PROCESS_TABLE.lock();
    // No tocamos NEXT_PID, dejamos que empiece en 1
    
    // Allocate kernel stack for PID 0 (Idle/Kernel task)
    // Even though it runs on the boot stack initially, we need a valid TSS RSP0 
    // for when it's scheduled back in, just in case.
    let kernel_stack_size = 8192;
    let kernel_stack = alloc::vec![0u8; kernel_stack_size];
    let kernel_stack_top = kernel_stack.as_ptr() as u64 + kernel_stack_size as u64;
    core::mem::forget(kernel_stack); // Leak
    
    let kernel_stack_top_aligned = kernel_stack_top & !0xF;

    let mut process = Process::new();
    process.id = 0;
    process.state = ProcessState::Running;
    process.priority = 0; // Prioridad más baja
    process.time_slice = 10;
    process.kernel_stack_top = kernel_stack_top_aligned;
    
    // Configurar contexto inicial
    // Cuando el scheduler cambie a PID 0, necesita saber el RSP.
    // PERO, PID 0 ya está "corriendo". Su contexto real se guarda
    // cuando llamamos a switch_context(0, X).
    // Así que solo necesitamos kernel_stack_top para el TSS.
    
    // Insertar en la tabla (slot 0)
    table[0] = Some(process);
    
    // Establecer como actual
    *CURRENT_PROCESS.lock() = Some(0);
}

/// Crear un nuevo proceso
pub fn create_process(entry_point: u64, stack_base: u64, stack_size: usize) -> Option<ProcessId> {
    // Allocate kernel stack for this process
    // For now, we use a fixed size buffer from heap or a pool could be better.
    // Let's alloc from heap using vec!
    // 8KB kernel stack should be enough
    let kernel_stack_size = 8192;
    let kernel_stack = alloc::vec![0u8; kernel_stack_size];
    let kernel_stack_top = kernel_stack.as_ptr() as u64 + kernel_stack_size as u64;
    
    // Leak the memory so it persists for the process lifetime (simplification for now)
    // Ideally we should store the Vec in the Process struct to drop it later
    core::mem::forget(kernel_stack);

    let mut table = PROCESS_TABLE.lock();
    let mut next_pid = NEXT_PID.lock();
    
    // Buscar slot libre
    for slot in table.iter_mut() {
        if slot.is_none() {
            let pid = *next_pid;
            *next_pid += 1;
            
            let mut process = Process::new();
            process.id = pid;
            process.state = ProcessState::Ready;
            process.stack_base = stack_base;
            process.stack_size = stack_size;
            process.priority = 5; // Prioridad media por defecto
            process.time_slice = 10; // 10 ticks
            
            // ALIGN STACK to 16 bytes to ensure SSE/Function calls work correctly in trampoline
            let kernel_stack_top_aligned = kernel_stack_top & !0xF;

            // Configurar contexto inicial
            // En vez de saltar directo al userspace (que mantendría Ring 0),
            // saltamos a una función trampolín que hace `iretq` para cambiar a Ring 3
            process.context.rip = crate::elf_loader::jump_to_userspace as u64;
            process.context.rdi = entry_point;                            // arg1 para jump_to_userspace
            process.context.rsi = stack_base + stack_size as u64;         // arg2 para jump_to_userspace
            process.context.rsp = kernel_stack_top_aligned;               // Stack del kernel para el trampolín
            process.context.rflags = 0x002; // IF disabled (until iretq enables it for userspace)
            process.kernel_stack_top = kernel_stack_top_aligned; // Use aligned stack top for TSS too
            
            crate::serial::serial_print("[PROCESS] Created PID ");
            crate::serial::serial_print_dec(pid as u64);
            crate::serial::serial_print(" | RIP (Trampoline): ");
            crate::serial::serial_print_hex(process.context.rip);
            crate::serial::serial_print(" | RSP (Kernel): ");
            crate::serial::serial_print_hex(process.context.rsp);
            crate::serial::serial_print("\n");
            
            *slot = Some(process);
            return Some(pid);
        }
    }
    
    None
}

/// Obtener proceso actual
pub fn current_process_id() -> Option<ProcessId> {
    *CURRENT_PROCESS.lock()
}

/// Establecer proceso actual
pub fn set_current_process(pid: Option<ProcessId>) {
    *CURRENT_PROCESS.lock() = pid;
}

/// Obtener proceso por ID
pub fn get_process(pid: ProcessId) -> Option<Process> {
    let table = PROCESS_TABLE.lock();
    for process in table.iter() {
        if let Some(p) = process {
            if p.id == pid {
                return Some(*p);
            }
        }
    }
    None
}

/// Actualizar proceso
pub fn update_process(pid: ProcessId, process: Process) {
    let mut table = PROCESS_TABLE.lock();
    for slot in table.iter_mut() {
        if let Some(p) = slot {
            if p.id == pid {
                *p = process;
                return;
            }
        }
    }
}

/// Cambiar de contexto entre procesos
/// 
/// Esta función guarda el contexto del proceso actual y carga el contexto del siguiente proceso
/// 
/// # Safety
/// Esta función es unsafe porque manipula directamente registros de CPU
pub unsafe fn switch_context(from: &mut Context, to: &Context) {
    asm!(
        // Guardar contexto actual (usando rdi = from)
        "mov [rdi + 0x00], rax",
        "mov [rdi + 0x08], rbx",
        "mov [rdi + 0x10], rcx",
        "mov [rdi + 0x18], rdx",
        "mov [rdi + 0x20], rsi",
        // rdi está en uso, pero guarda su valor original (que recibimos)
        // No, el valor de rdi NO está en rdi ahora mismo? 
        // Sí, porque 'from' está pinneado a rdi. 
        // PERO rdi caller-saved/callee-saved issues? 
        // No, 'from' es un puntero. El valor del registro RDI del proceso 'actual' 
        // antes de llamar a esta funcion es lo que queremos guardar?
        // No, queremos guardar el estado de los registros general purpose.
        // El rdi que usamos es el puntero 'from'. Si queremos guardar el rdi del proceso,
        // tendríamos que haberlo pusheado o algo?
        // En System V ABI, RDI es el primer argumento.
        // Así que RDI tiene el puntero 'from'. EL valor RDI del proceso es 'from'.
        // Eso está bien para 'from'.
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
        
        // ==========================================
        // Restaurar contexto nuevo (usando rsi = to)
        // ==========================================
        
        // 1. Cambiar Stack
        "mov rsp, [rsi + 0x78]",
        
        // 2. Preparar stack para iretq/ret simulado
        // Orden inverso al pop: Queremos que popfq saque RFLAGS y ret saque RIP.
        // Stack crece hacia abajo.
        // Push RIP (queda más "abajo" en memoria alta)
        "push qword ptr [rsi + 0x80]", // RIP
        // Push RFLAGS (queda más "arriba" en memoria baja, tope del stack)
        "push qword ptr [rsi + 0x88]", // RFLAGS
        
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
    );
}

/// Terminar proceso actual
pub fn exit_process() {
    if let Some(pid) = current_process_id() {
        let mut table = PROCESS_TABLE.lock();
        for slot in table.iter_mut() {
            if let Some(p) = slot {
                if p.id == pid {
                    p.state = ProcessState::Terminated;
                    break;
                }
            }
        }
    }
}

/// Listar todos los procesos
pub fn list_processes() -> [(ProcessId, ProcessState); MAX_PROCESSES] {
    let table = PROCESS_TABLE.lock();
    let mut result = [(0, ProcessState::Terminated); MAX_PROCESSES];
    
    for (i, slot) in table.iter().enumerate() {
        if let Some(p) = slot {
            result[i] = (p.id, p.state);
        }
    }
    
    result
}

/// Fork current process - create child process
/// Returns: Some(child_pid) on success, None on error
pub fn fork_process() -> Option<ProcessId> {
    // Get current process
    let current_pid = current_process_id()?;
    let parent = get_process(current_pid)?;
    
    // Allocate new stack for child from pool
    let child_stack = allocate_stack()?;
    
    // Copy parent's stack to child
    // Calculate used stack size
    let parent_stack_top = parent.stack_base + parent.stack_size as u64;
    let used_stack_size = parent_stack_top - parent.context.rsp;
    
    // DEBUG: Print stack details
    crate::serial::serial_print("[PROCESS] Fork debug: PID ");
    crate::serial::serial_print_dec(parent.id as u64);
    crate::serial::serial_print("\n  Stack Base: ");
    crate::serial::serial_print_hex(parent.stack_base);
    crate::serial::serial_print("\n  Stack Top:  ");
    crate::serial::serial_print_hex(parent_stack_top);
    crate::serial::serial_print("\n  RSP:        ");
    crate::serial::serial_print_hex(parent.context.rsp);
    crate::serial::serial_print("\n  Used Size:  ");
    crate::serial::serial_print_dec(used_stack_size);
    crate::serial::serial_print("\n  Max Child:  ");
    crate::serial::serial_print_dec(CHILD_STACK_SIZE as u64);
    crate::serial::serial_print("\n");
    
    if used_stack_size as usize > CHILD_STACK_SIZE {
        crate::serial::serial_print("[PROCESS] Fork failed: Stack too large for child\n");
        return None; 
    }
    
    // Copy parent's stack to child (Top-aligned)
    let child_stack_top = child_stack + CHILD_STACK_SIZE as u64;
    let child_rsp = child_stack_top - used_stack_size;
    
    unsafe {
        let src = parent.context.rsp as *const u8;
        let dst = child_rsp as *mut u8;
        core::ptr::copy_nonoverlapping(src, dst, used_stack_size as usize);
    }
    
    // Create child process
    let mut table = PROCESS_TABLE.lock();
    let mut next_pid = NEXT_PID.lock();
    
    for slot in table.iter_mut() {
        if slot.is_none() {
            let child_pid = *next_pid;
            *next_pid += 1;
            
            let mut child = parent; // Copy parent's state
            child.id = child_pid;
            child.state = ProcessState::Ready;
            child.stack_base = child_stack;
            child.stack_size = CHILD_STACK_SIZE;
            child.parent_pid = Some(current_pid);
            
            // Adjust stack pointer for child
            child.context.rsp = child_rsp;
            
            // Child returns 0 from fork
            child.context.rax = 0;
            
            *slot = Some(child);
            
            // Parent returns child PID
            return Some(child_pid);
        }
    }
    
    None
}
