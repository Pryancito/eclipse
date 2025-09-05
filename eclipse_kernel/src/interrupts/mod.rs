//! Sistema de manejo de interrupciones para Eclipse OS
//! 
//! Este módulo proporciona:
//! - Manejo de interrupciones hardware (IRQ)
//! - Manejo de excepciones del procesador
//! - Sistema de interrupciones programables (PIC/APIC)
//! - Gestión de interrupciones por prioridades
//! - Handlers de interrupciones específicas

pub mod handler;
pub mod pic;
pub mod apic;
pub mod exceptions;
pub mod manager;

// Re-exportar tipos principales

// Constantes del sistema de interrupciones
pub const MAX_INTERRUPTS: usize = 256;
pub const IRQ_BASE: u8 = 32;
pub const IRQ_TIMER: u8 = 0;
pub const IRQ_KEYBOARD: u8 = 1;
pub const IRQ_MOUSE: u8 = 12;
pub const IRQ_ATA_PRIMARY: u8 = 14;
pub const IRQ_ATA_SECONDARY: u8 = 15;

// Prioridades de interrupciones
pub const PRIORITY_CRITICAL: u8 = 0;  // Timer, NMI
pub const PRIORITY_HIGH: u8 = 1;      // Keyboard, Mouse
pub const PRIORITY_NORMAL: u8 = 2;    // Storage, Network
pub const PRIORITY_LOW: u8 = 3;       // Audio, Video

/// Inicializar el sistema de interrupciones
pub fn init_interrupt_system() -> Result<(), &'static str> {
    manager::init_interrupt_manager()?;
    pic::init_pic()?;
    apic::init_apic()?;
    exceptions::init_exception_handlers()?;
    Ok(())
}

/// Obtener información del sistema de interrupciones
pub fn get_interrupt_system_info() -> &'static str {
    "Sistema de interrupciones Eclipse OS v1.0 - IRQ/Exception Handler"
}
