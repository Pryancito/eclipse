//! Gestión de Procesos para Eclipse OS
//!
//! Implementa PCB, estados de proceso y operaciones básicas

use core::sync::atomic::{AtomicU32, Ordering};
use super::file_descriptor::FileDescriptorTable;
use super::stack_allocator::StackInfo;
use alloc::string::String;
use alloc::vec::Vec;
use hashbrown::HashMap;

/// ID único de proceso
pub type ProcessId = u32;

/// ID único de thread
pub type ThreadId = u32;

/// Estados de un proceso
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProcessState {
    /// Proceso recién creado
    New,
    /// Proceso listo para ejecutar
    Ready,
    /// Proceso ejecutándose
    Running,
    /// Proceso bloqueado esperando evento
    Blocked,
    /// Proceso terminado
    Terminated,
    /// Proceso en estado zombie
    Zombie,
}

/// Prioridades de proceso
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub enum ProcessPriority {
    /// Prioridad crítica del sistema
    Critical = 0,
    /// Prioridad alta
    High = 1,
    /// Prioridad normal
    Normal = 2,
    /// Prioridad baja
    Low = 3,
    /// Prioridad de fondo
    Background = 4,
}

/// Información de CPU para un proceso
#[derive(Debug, Clone, Copy)]
pub struct CpuContext {
    /// Registros generales
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    /// Registro de instrucciones
    pub rip: u64,
    /// Registro de flags
    pub rflags: u64,
    /// Selector de segmento de código
    pub cs: u16,
    /// Selector de segmento de datos
    pub ds: u16,
    /// Selector de segmento de stack
    pub ss: u16,
    /// Selector de segmento extra
    pub es: u16,
    /// Selector de segmento FS
    pub fs: u16,
    /// Selector de segmento GS
    pub gs: u16,
}

impl Default for CpuContext {
    fn default() -> Self {
        Self {
            rax: 0,
            rbx: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            rbp: 0,
            rsp: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rip: 0,
            rflags: 0x202, // RFLAGS con IF=1
            cs: 0x08,
            ds: 0x10,
            ss: 0x10,
            es: 0x10,
            fs: 0x10,
            gs: 0x10,
        }
    }
}

/// Información de memoria de un proceso
#[derive(Debug, Clone)]
pub struct MemoryInfo {
    /// Dirección base del espacio de direcciones
    pub base_address: u64,
    /// Tamaño del espacio de direcciones
    pub size: u64,
    /// Dirección del stack
    pub stack_pointer: u64,
    /// Tamaño del stack
    pub stack_size: u64,
    /// Dirección de inicio del heap
    pub heap_start: u64,
    /// Dirección actual del break (fin del heap)
    pub heap_break: u64,
    /// Límite máximo del heap
    pub heap_limit: u64,
    /// Dirección física de la tabla PML4 del proceso
    pub pml4_addr: u64,
}

impl Default for MemoryInfo {
    fn default() -> Self {
        // Heap comienza después del código (típicamente en 0x600000)
        let heap_start = 0x600000;
        Self {
            base_address: 0,
            size: 0,
            stack_pointer: 0,
            stack_size: 0x10000, // 64KB de stack
            heap_start,
            heap_break: heap_start, // Heap vacío inicialmente
            heap_limit: heap_start + 0x1000000, // Límite de 16MB de heap
            pml4_addr: 0, // Will be set during process creation/fork
        }
    }
}

/// Bloque de Control de Proceso (PCB)
#[derive(Debug, Clone)]
pub struct ProcessControlBlock {
    /// ID único del proceso
    pub pid: ProcessId,
    /// ID del proceso padre
    pub parent_pid: Option<ProcessId>,
    /// Estado actual del proceso
    pub state: ProcessState,
    /// Prioridad del proceso
    pub priority: ProcessPriority,
    /// Contexto de CPU
    pub cpu_context: CpuContext,
    /// Información de memoria
    pub memory_info: MemoryInfo,
    /// Tiempo de CPU usado
    pub cpu_time: u64,
    /// Tiempo de creación
    pub creation_time: u64,
    /// Tiempo de última ejecución
    pub last_run_time: u64,
    /// Nombre del proceso
    pub name: [u8; 32],
    /// Argumentos del proceso
    pub argc: u32,
    /// Puntero a argumentos
    pub argv: u64,
    /// Variables de entorno
    pub envp: u64,
    /// Código de salida
    pub exit_code: Option<u32>,
    /// Señales pendientes
    pub pending_signals: u32,
    /// Recursos abiertos
    pub open_files: u32,
    /// Directorio de trabajo actual
    pub working_directory: String,
    /// Tabla de file descriptors
    pub fd_table: FileDescriptorTable,
    /// Información del stack del proceso
    pub stack_info: Option<StackInfo>,
    /// Variables de entorno del proceso
    pub environment: HashMap<String, String>,
}

impl ProcessControlBlock {
    /// Crear un nuevo PCB
    pub fn new(pid: ProcessId, name: &str, priority: ProcessPriority) -> Self {
        let mut pcb = Self {
            pid,
            parent_pid: None,
            state: ProcessState::New,
            priority,
            cpu_context: CpuContext::default(),
            memory_info: MemoryInfo::default(),
            cpu_time: 0,
            creation_time: 0, // Se establecerá cuando se cree
            last_run_time: 0,
            name: [0; 32],
            argc: 0,
            argv: 0,
            envp: 0,
            exit_code: None,
            pending_signals: 0,
            open_files: 3, // stdin, stdout, stderr
            working_directory: String::from("/"),
            fd_table: FileDescriptorTable::new(),
            stack_info: None, // Se asignará cuando sea necesario
            environment: {
                let mut env = HashMap::new();
                // Variables de entorno por defecto
                env.insert(String::from("PATH"), String::from("/bin:/usr/bin"));
                env.insert(String::from("HOME"), String::from("/"));
                env.insert(String::from("SHELL"), String::from("/bin/ion"));
                env.insert(String::from("USER"), String::from("root"));
                env.insert(String::from("TERM"), String::from("linux"));
                env
            },
        };

        // Copiar nombre
        let name_bytes = name.as_bytes();
        let copy_len = core::cmp::min(name_bytes.len(), 31);
        pcb.name[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

        pcb
    }

    /// Cambiar estado del proceso
    pub fn set_state(&mut self, new_state: ProcessState) {
        self.state = new_state;
    }
    
    /// Obtener estado del proceso
    pub fn get_state(&self) -> ProcessState {
        self.state
    }

    /// Actualizar contexto de CPU
    pub fn update_cpu_context(&mut self, context: CpuContext) {
        self.cpu_context = context;
    }

    /// Obtener contexto de CPU
    pub fn get_cpu_context(&self) -> CpuContext {
        self.cpu_context
    }

    /// Establecer información de memoria
    pub fn set_memory_info(&mut self, mem_info: MemoryInfo) {
        self.memory_info = mem_info;
    }

    /// Actualizar tiempo de CPU
    pub fn update_cpu_time(&mut self, delta_time: u64) {
        self.cpu_time += delta_time;
        self.last_run_time = delta_time;
    }

    /// Terminar proceso
    pub fn terminate(&mut self, exit_code: u32) {
        self.state = ProcessState::Terminated;
        self.exit_code = Some(exit_code);
    }

    /// Verificar si el proceso está listo para ejecutar
    pub fn is_ready(&self) -> bool {
        self.state == ProcessState::Ready
    }

    /// Verificar si el proceso está ejecutándose
    pub fn is_running(&self) -> bool {
        self.state == ProcessState::Running
    }

    /// Verificar si el proceso está bloqueado
    pub fn is_blocked(&self) -> bool {
        self.state == ProcessState::Blocked
    }

    /// Verificar si el proceso está terminado
    pub fn is_terminated(&self) -> bool {
        self.state == ProcessState::Terminated
    }

    /// Obtener nombre del proceso como string
    pub fn get_name(&self) -> &str {
        let null_pos = self.name.iter().position(|&b| b == 0).unwrap_or(32);
        core::str::from_utf8(&self.name[..null_pos]).unwrap_or("Unknown")
    }

    /// Establecer argumentos del proceso
    pub fn set_arguments(&mut self, argc: u32, argv: u64, envp: u64) {
        self.argc = argc;
        self.argv = argv;
        self.envp = envp;
    }

    /// Agregar señal pendiente
    pub fn add_signal(&mut self, signal: u32) {
        self.pending_signals |= 1 << signal;
    }

    /// Limpiar señal pendiente
    pub fn clear_signal(&mut self, signal: u32) {
        self.pending_signals &= !(1 << signal);
    }

    /// Verificar si hay señales pendientes
    pub fn has_pending_signals(&self) -> bool {
        self.pending_signals != 0
    }
}

/// Información de un thread
#[derive(Debug, Clone)]
pub struct ThreadInfo {
    /// ID del thread
    pub tid: ThreadId,
    /// ID del proceso padre
    pub pid: ProcessId,
    /// Estado del thread
    pub state: ProcessState,
    /// Prioridad del thread
    pub priority: ProcessPriority,
    /// Contexto de CPU del thread
    pub cpu_context: CpuContext,
    /// Stack del thread
    pub stack_pointer: u64,
    /// Tamaño del stack
    pub stack_size: u64,
    /// Tiempo de CPU usado
    pub cpu_time: u64,
    /// Tiempo de creación
    pub creation_time: u64,
    /// Tiempo de última ejecución
    pub last_run_time: u64,
}

impl ThreadInfo {
    /// Crear un nuevo thread
    pub fn new(tid: ThreadId, pid: ProcessId, priority: ProcessPriority) -> Self {
        Self {
            tid,
            pid,
            state: ProcessState::New,
            priority,
            cpu_context: CpuContext::default(),
            stack_pointer: 0,
            stack_size: 0x8000, // 32KB de stack por thread
            cpu_time: 0,
            creation_time: 0,
            last_run_time: 0,
        }
    }

    /// Cambiar estado del thread
    pub fn set_state(&mut self, new_state: ProcessState) {
        self.state = new_state;
    }

    /// Actualizar contexto de CPU
    pub fn update_cpu_context(&mut self, context: CpuContext) {
        self.cpu_context = context;
    }

    /// Obtener contexto de CPU
    pub fn get_cpu_context(&self) -> CpuContext {
        self.cpu_context
    }

    /// Actualizar tiempo de CPU
    pub fn update_cpu_time(&mut self, delta_time: u64) {
        self.cpu_time += delta_time;
        self.last_run_time = delta_time;
    }

    /// Verificar si el thread está listo
    pub fn is_ready(&self) -> bool {
        self.state == ProcessState::Ready
    }

    /// Verificar si el thread está ejecutándose
    pub fn is_running(&self) -> bool {
        self.state == ProcessState::Running
    }

    /// Verificar si el thread está bloqueado
    pub fn is_blocked(&self) -> bool {
        self.state == ProcessState::Blocked
    }
}

/// Contador global de PIDs
static NEXT_PID: AtomicU32 = AtomicU32::new(1);

/// Contador global de TIDs
static NEXT_TID: AtomicU32 = AtomicU32::new(1);

/// Obtener el siguiente PID disponible
pub fn get_next_pid() -> ProcessId {
    NEXT_PID.fetch_add(1, Ordering::SeqCst)
}

/// Obtener el siguiente TID disponible
pub fn get_next_tid() -> ThreadId {
    NEXT_TID.fetch_add(1, Ordering::SeqCst)
}

/// Función para crear un nuevo proceso
pub fn create_process(name: &str, priority: ProcessPriority) -> ProcessControlBlock {
    let pid = get_next_pid();
    ProcessControlBlock::new(pid, name, priority)
}

/// Función para crear un nuevo thread
pub fn create_thread(pid: ProcessId, priority: ProcessPriority) -> ThreadInfo {
    let tid = get_next_tid();
    ThreadInfo::new(tid, pid, priority)
}
