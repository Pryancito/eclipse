//! Handler de interrupciones para Eclipse OS
//! 
//! Proporciona el contexto de interrupciones y el trait para handlers

use core::fmt;

/// Contexto de interrupciones que se pasa a los handlers
#[derive(Debug, Clone, Copy)]
pub struct InterruptContext {
    /// Número de interrupción
    pub interrupt_number: u8,
    /// Código de error (si aplica)
    pub error_code: Option<u32>,
    /// Registros del procesador al momento de la interrupción
    pub registers: CpuRegisters,
    /// Timestamp de la interrupción
    pub timestamp: u64,
}

/// Registros del procesador
#[derive(Debug, Clone, Copy)]
pub struct CpuRegisters {
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
    pub rip: u64,
    pub rflags: u64,
    pub cs: u16,
    pub ss: u16,
    pub ds: u16,
    pub es: u16,
    pub fs: u16,
    pub gs: u16,
}

impl CpuRegisters {
    /// Crear un contexto de registros vacío
    pub fn new() -> Self {
        Self {
            rax: 0, rbx: 0, rcx: 0, rdx: 0,
            rsi: 0, rdi: 0, rbp: 0, rsp: 0,
            r8: 0, r9: 0, r10: 0, r11: 0,
            r12: 0, r13: 0, r14: 0, r15: 0,
            rip: 0, rflags: 0,
            cs: 0, ss: 0, ds: 0, es: 0, fs: 0, gs: 0,
        }
    }
}

impl Default for CpuRegisters {
    fn default() -> Self {
        Self::new()
    }
}

/// Resultado de procesar una interrupción
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InterruptResult {
    /// Interrupción manejada exitosamente
    Handled,
    /// Interrupción no manejada, pasar al siguiente handler
    NotHandled,
    /// Error al procesar la interrupción
    Error,
    /// Interrupción crítica que requiere atención inmediata
    Critical,
}

impl fmt::Display for InterruptResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InterruptResult::Handled => write!(f, "Handled"),
            InterruptResult::NotHandled => write!(f, "NotHandled"),
            InterruptResult::Error => write!(f, "Error"),
            InterruptResult::Critical => write!(f, "Critical"),
        }
    }
}

/// Trait que deben implementar todos los handlers de interrupciones
pub trait InterruptHandler {
    /// Manejar una interrupción
    /// 
    /// # Arguments
    /// * `context` - Contexto de la interrupción
    /// 
    /// # Returns
    /// * `InterruptResult` - Resultado del manejo
    fn handle_interrupt(&mut self, context: &InterruptContext) -> InterruptResult;
    
    /// Obtener el número de interrupción que maneja este handler
    fn get_interrupt_number(&self) -> u8;
    
    /// Obtener la prioridad del handler
    fn get_priority(&self) -> u8;
    
    /// Verificar si el handler está habilitado
    fn is_enabled(&self) -> bool;
    
    /// Habilitar el handler
    fn enable(&mut self);
    
    /// Deshabilitar el handler
    fn disable(&mut self);
    
    /// Obtener nombre del handler
    fn get_name(&self) -> &'static str;
}

/// Handler de interrupciones genérico
pub struct GenericInterruptHandler {
    interrupt_number: u8,
    priority: u8,
    enabled: bool,
    name: &'static str,
    handler_func: fn(&InterruptContext) -> InterruptResult,
}

impl GenericInterruptHandler {
    /// Crear un nuevo handler genérico
    pub fn new(
        interrupt_number: u8,
        priority: u8,
        name: &'static str,
        handler_func: fn(&InterruptContext) -> InterruptResult,
    ) -> Self {
        Self {
            interrupt_number,
            priority,
            enabled: true,
            name,
            handler_func,
        }
    }
}

impl InterruptHandler for GenericInterruptHandler {
    fn handle_interrupt(&mut self, context: &InterruptContext) -> InterruptResult {
        if !self.enabled {
            return InterruptResult::NotHandled;
        }
        
        (self.handler_func)(context)
    }
    
    fn get_interrupt_number(&self) -> u8 {
        self.interrupt_number
    }
    
    fn get_priority(&self) -> u8 {
        self.priority
    }
    
    fn is_enabled(&self) -> bool {
        self.enabled
    }
    
    fn enable(&mut self) {
        self.enabled = true;
    }
    
    fn disable(&mut self) {
        self.enabled = false;
    }
    
    fn get_name(&self) -> &'static str {
        self.name
    }
}

/// Handler por defecto para interrupciones no manejadas
pub fn default_interrupt_handler(context: &InterruptContext) -> InterruptResult {
    // En un sistema real, aquí se registraría el error y se tomarían medidas
    InterruptResult::NotHandled
}

/// Handler para interrupciones críticas
pub fn critical_interrupt_handler(context: &InterruptContext) -> InterruptResult {
    // Manejo de interrupciones críticas del sistema
    InterruptResult::Critical
}

/// Handler para interrupciones de timer
pub fn timer_interrupt_handler(context: &InterruptContext) -> InterruptResult {
    // Manejo de interrupciones del timer del sistema
    InterruptResult::Handled
}

/// Handler para interrupciones de teclado
pub fn keyboard_interrupt_handler(context: &InterruptContext) -> InterruptResult {
    // Manejo de interrupciones del teclado
    InterruptResult::Handled
}

/// Handler para interrupciones de mouse
pub fn mouse_interrupt_handler(context: &InterruptContext) -> InterruptResult {
    // Manejo de interrupciones del mouse
    InterruptResult::Handled
}
