//! # Eclipse Kernel en Rust - Versión Híbrida con Redox

#![no_std]
#![no_main]

extern crate alloc;

#[cfg(feature = "alloc")]
pub mod allocator;

pub mod memory;
pub mod process;
pub mod thread;
pub mod synchronization;  // Sistema de sincronización multihilo
pub mod performance;  // Sistema de optimización de rendimiento multihilo
pub mod math_utils;  // Utilidades matemáticas
pub mod drivers;
pub mod filesystem;
pub mod network;
pub mod gui;
pub mod redox;  // Módulo de integración de Redox
pub mod testing;  // Sistema de pruebas y validación
pub mod init_system;  // Sistema de inicialización con systemd
pub mod process_transfer;  // Transferencia de control del kernel al userland
pub mod elf_loader;  // Cargador de ejecutables ELF64
pub mod process_memory;  // Gestión de memoria para procesos
pub mod paging;  // Sistema de paginación
pub mod gdt;  // Global Descriptor Table
pub mod idt;  // Interrupt Descriptor Table
pub mod interrupts;  // Gestión de interrupciones y timers
// pub mod real_integration;  // Integración real kernel-systemd (deshabilitado temporalmente)
pub mod main_simple;
pub mod main_with_init;  // Main con integración systemd
pub mod vga_centered_display;
pub mod wayland;  // Módulo para mostrar texto centrado en VGA


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelError {
    MemoryError,
    ProcessError,
    ThreadError,
    Unknown,
}

pub type KernelResult<T> = Result<T, KernelError>;

pub const KERNEL_VERSION: &str = "0.4.0";

pub fn initialize() -> KernelResult<()> {
    // Kernel híbrido Eclipse-Redox inicializado
    
    // Inicializar el kernel base de Eclipse
    // Inicializando kernel base de Eclipse
    
    // Inicializar el sistema Redox integrado
    redox::init_redox_system()?;
    
    // Kernel híbrido inicializado correctamente
    Ok(())
}

/// Procesar eventos del sistema híbrido
pub fn process_events() -> KernelResult<()> {
    // Procesar eventos del kernel base de Eclipse
    // (aquí se integrarían los eventos específicos de Eclipse)
    
    // Procesar eventos del sistema Redox integrado
    redox::process_redox_events()?;
    
    Ok(())
}
