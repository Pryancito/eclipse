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
            rflags: 0x202, // IF (interrupts enabled)
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
        }
    }
}

/// Tabla de procesos
const MAX_PROCESSES: usize = 64;
static PROCESS_TABLE: Mutex<[Option<Process>; MAX_PROCESSES]> = Mutex::new([None; MAX_PROCESSES]);
static NEXT_PID: Mutex<ProcessId> = Mutex::new(1);
static CURRENT_PROCESS: Mutex<Option<ProcessId>> = Mutex::new(None);

/// Crear un nuevo proceso
pub fn create_process(entry_point: u64, stack_base: u64, stack_size: usize) -> Option<ProcessId> {
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
            
            // Configurar contexto inicial
            process.context.rip = entry_point;
            process.context.rsp = stack_base + stack_size as u64 - 16; // Dejar espacio
            process.context.rflags = 0x202; // IF enabled
            
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
        // Guardar contexto actual
        "mov [{from} + 0x00], rax",
        "mov [{from} + 0x08], rbx",
        "mov [{from} + 0x10], rcx",
        "mov [{from} + 0x18], rdx",
        "mov [{from} + 0x20], rsi",
        "mov [{from} + 0x28], rdi",
        "mov [{from} + 0x30], rbp",
        "mov [{from} + 0x38], r8",
        "mov [{from} + 0x40], r9",
        "mov [{from} + 0x48], r10",
        "mov [{from} + 0x50], r11",
        "mov [{from} + 0x58], r12",
        "mov [{from} + 0x60], r13",
        "mov [{from} + 0x68], r14",
        "mov [{from} + 0x70], r15",
        
        // Guardar RSP actual
        "mov rax, rsp",
        "mov [{from} + 0x78], rax",
        
        // Guardar RIP (dirección de retorno)
        "lea rax, [rip + 2f]",
        "mov [{from} + 0x80], rax",
        
        // Guardar RFLAGS
        "pushfq",
        "pop rax",
        "mov [{from} + 0x88], rax",
        
        // Restaurar contexto nuevo
        "mov rax, [{to} + 0x00]",
        "mov rbx, [{to} + 0x08]",
        "mov rcx, [{to} + 0x10]",
        "mov rdx, [{to} + 0x18]",
        "mov rsi, [{to} + 0x20]",
        "mov rdi, [{to} + 0x28]",
        "mov rbp, [{to} + 0x30]",
        "mov r8,  [{to} + 0x38]",
        "mov r9,  [{to} + 0x40]",
        "mov r10, [{to} + 0x48]",
        "mov r11, [{to} + 0x50]",
        "mov r12, [{to} + 0x58]",
        "mov r13, [{to} + 0x60]",
        "mov r14, [{to} + 0x68]",
        "mov r15, [{to} + 0x70]",
        
        // Restaurar RSP
        "mov rsp, [{to} + 0x78]",
        
        // Restaurar RFLAGS
        "push qword ptr [{to} + 0x88]",
        "popfq",
        
        // Saltar a RIP
        "jmp [{to} + 0x80]",
        
        "2:",
        from = in(reg) from,
        to = in(reg) to,
        options(noreturn)
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
