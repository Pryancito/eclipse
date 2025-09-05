#![allow(dead_code)]
//! Gestor principal de interrupciones para Eclipse OS
//! 
//! Coordina todos los sistemas de interrupciones y excepciones

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt;

use crate::interrupts::handler::{InterruptHandler, InterruptContext, InterruptResult};
use crate::interrupts::exceptions::{ExceptionHandler, ExceptionInfo, ExceptionResult};

/// Tipos de interrupciones
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InterruptType {
    /// Interrupción hardware (IRQ)
    Hardware,
    /// Excepción del procesador
    Exception,
    /// Interrupción software
    Software,
    /// Interrupción del sistema
    System,
}

/// Prioridades de interrupciones
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub enum InterruptPriority {
    Critical = 0,  // Timer, NMI, errores críticos
    High = 1,      // Teclado, mouse, dispositivos de entrada
    Normal = 2,    // Almacenamiento, red
    Low = 3,       // Audio, video, dispositivos de baja prioridad
}

impl InterruptPriority {
    /// Obtener prioridad como u8
    pub fn as_u8(&self) -> u8 {
        *self as u8
    }
    
    /// Crear prioridad desde u8
    pub fn from_u8(priority: u8) -> Self {
        match priority {
            0 => InterruptPriority::Critical,
            1 => InterruptPriority::High,
            2 => InterruptPriority::Normal,
            3 => InterruptPriority::Low,
            _ => InterruptPriority::Normal,
        }
    }
}

/// Gestor de interrupciones
pub struct InterruptManager {
    /// Handlers de interrupciones hardware
    hardware_handlers: Vec<Option<Box<dyn InterruptHandler>>>,
    /// Handlers de excepciones
    exception_handlers: Vec<Option<Box<dyn ExceptionHandler>>>,
    /// Estado del gestor
    initialized: bool,
    /// Estadísticas
    stats: InterruptStats,
}

impl InterruptManager {
    /// Crear nuevo gestor de interrupciones
    pub fn new() -> Self {
        let mut hardware_handlers = Vec::with_capacity(256);
        for _ in 0..256 {
            hardware_handlers.push(None);
        }
        
        let mut exception_handlers = Vec::with_capacity(32);
        for _ in 0..32 {
            exception_handlers.push(None);
        }
        
        Self {
            hardware_handlers,
            exception_handlers,
            initialized: false,
            stats: InterruptStats::new(),
        }
    }
    
    /// Inicializar el gestor
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        if self.initialized {
            return Ok(());
        }
        
        // Inicializar handlers por defecto
        self.setup_default_handlers()?;
        
        self.initialized = true;
        self.stats.initialization_time = 0; // Se establecería con timestamp real
        
        Ok(())
    }
    
    /// Configurar handlers por defecto
    fn setup_default_handlers(&mut self) -> Result<(), &'static str> {
        // En un sistema real, aquí se configurarían los handlers básicos
        // Por ahora, simulamos la configuración
        
        Ok(())
    }
    
    /// Registrar handler de interrupción hardware
    pub fn register_hardware_handler(&mut self, handler: Box<dyn InterruptHandler>) -> Result<(), &'static str> {
        let interrupt_num = handler.get_interrupt_number() as usize;
        
        if interrupt_num >= 256 {
            return Err("Número de interrupción inválido");
        }
        
        if self.hardware_handlers[interrupt_num].is_some() {
            return Err("Handler ya registrado para esta interrupción");
        }
        
        self.hardware_handlers[interrupt_num] = Some(handler);
        self.stats.registered_handlers += 1;
        
        Ok(())
    }
    
    /// Registrar handler de excepción
    pub fn register_exception_handler(&mut self, handler: Box<dyn ExceptionHandler>) -> Result<(), &'static str> {
        let exception_num = handler.get_exception_type() as u8 as usize;
        
        if exception_num >= 32 {
            return Err("Tipo de excepción inválido");
        }
        
        if self.exception_handlers[exception_num].is_some() {
            return Err("Handler ya registrado para esta excepción");
        }
        
        self.exception_handlers[exception_num] = Some(handler);
        self.stats.registered_exception_handlers += 1;
        
        Ok(())
    }
    
    /// Manejar interrupción hardware
    pub fn handle_hardware_interrupt(&mut self, context: &InterruptContext) -> InterruptResult {
        if !self.initialized {
            return InterruptResult::Error;
        }
        
        let interrupt_num = context.interrupt_number as usize;
        
        if interrupt_num >= 256 {
            return InterruptResult::Error;
        }
        
        self.stats.hardware_interrupts += 1;
        
        if let Some(handler) = self.hardware_handlers[interrupt_num].as_mut() {
            let result = handler.handle_interrupt(context);
            self.stats.successful_handlers += 1;
            result
        } else {
            self.stats.unhandled_interrupts += 1;
            InterruptResult::NotHandled
        }
    }
    
    /// Manejar excepción
    pub fn handle_exception(&mut self, info: &ExceptionInfo) -> ExceptionResult {
        if !self.initialized {
            return ExceptionResult::Fatal;
        }
        
        let exception_num = info.exception_type as u8 as usize;
        
        if exception_num >= 32 {
            return ExceptionResult::Fatal;
        }
        
        self.stats.exceptions += 1;
        
        if let Some(handler) = self.exception_handlers[exception_num].as_mut() {
            let result = handler.handle_exception(info);
            self.stats.successful_exception_handlers += 1;
            result
        } else {
            self.stats.unhandled_exceptions += 1;
            ExceptionResult::NotHandled
        }
    }
    
    /// Obtener estadísticas
    pub fn get_stats(&self) -> &InterruptStats {
        &self.stats
    }
    
    /// Verificar si está inicializado
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
    
    /// Obtener información del sistema
    pub fn get_system_info(&self) -> InterruptSystemInfo {
        InterruptSystemInfo {
            initialized: self.initialized,
            total_handlers: self.stats.registered_handlers,
            total_exception_handlers: self.stats.registered_exception_handlers,
            hardware_interrupts: self.stats.hardware_interrupts,
            exceptions: self.stats.exceptions,
            unhandled_interrupts: self.stats.unhandled_interrupts,
            unhandled_exceptions: self.stats.unhandled_exceptions,
        }
    }
}

/// Estadísticas de interrupciones
#[derive(Debug, Clone, Copy)]
pub struct InterruptStats {
    pub initialization_time: u64,
    pub registered_handlers: u32,
    pub registered_exception_handlers: u32,
    pub hardware_interrupts: u64,
    pub exceptions: u64,
    pub successful_handlers: u64,
    pub successful_exception_handlers: u64,
    pub unhandled_interrupts: u64,
    pub unhandled_exceptions: u64,
}

impl InterruptStats {
    /// Crear nuevas estadísticas
    pub fn new() -> Self {
        Self {
            initialization_time: 0,
            registered_handlers: 0,
            registered_exception_handlers: 0,
            hardware_interrupts: 0,
            exceptions: 0,
            successful_handlers: 0,
            successful_exception_handlers: 0,
            unhandled_interrupts: 0,
            unhandled_exceptions: 0,
        }
    }
}

impl Default for InterruptStats {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for InterruptStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Interrupt Stats: handlers={}, exceptions={}, hw_irq={}, exc={}, unhandled_irq={}, unhandled_exc={}",
               self.registered_handlers, self.registered_exception_handlers,
               self.hardware_interrupts, self.exceptions,
               self.unhandled_interrupts, self.unhandled_exceptions)
    }
}

/// Información del sistema de interrupciones
#[derive(Debug, Clone, Copy)]
pub struct InterruptSystemInfo {
    pub initialized: bool,
    pub total_handlers: u32,
    pub total_exception_handlers: u32,
    pub hardware_interrupts: u64,
    pub exceptions: u64,
    pub unhandled_interrupts: u64,
    pub unhandled_exceptions: u64,
}

impl fmt::Display for InterruptSystemInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Interrupt System: initialized={}, handlers={}/{}, interrupts={}, exceptions={}, unhandled={}/{}",
               self.initialized, self.total_handlers, self.total_exception_handlers,
               self.hardware_interrupts, self.exceptions,
               self.unhandled_interrupts, self.unhandled_exceptions)
    }
}

/// Instancia global del gestor de interrupciones
static mut INTERRUPT_MANAGER: Option<InterruptManager> = None;

/// Inicializar el gestor de interrupciones
pub fn init_interrupt_manager() -> Result<(), &'static str> {
    unsafe {
        if INTERRUPT_MANAGER.is_some() {
            return Ok(());
        }
        
        let mut manager = InterruptManager::new();
        manager.initialize()?;
        INTERRUPT_MANAGER = Some(manager);
    }
    
    Ok(())
}

/// Obtener el gestor de interrupciones
pub fn get_interrupt_manager() -> Option<&'static mut InterruptManager> {
    unsafe { INTERRUPT_MANAGER.as_mut() }
}

/// Manejar interrupción hardware
pub fn handle_hardware_interrupt(context: &InterruptContext) -> InterruptResult {
    if let Some(manager) = get_interrupt_manager() {
        manager.handle_hardware_interrupt(context)
    } else {
        InterruptResult::Error
    }
}

/// Manejar excepción
pub fn handle_exception(info: &ExceptionInfo) -> ExceptionResult {
    if let Some(manager) = get_interrupt_manager() {
        manager.handle_exception(info)
    } else {
        ExceptionResult::Fatal
    }
}

/// Obtener información del sistema de interrupciones
pub fn get_interrupt_system_info() -> Option<InterruptSystemInfo> {
    get_interrupt_manager().map(|manager| manager.get_system_info())
}
