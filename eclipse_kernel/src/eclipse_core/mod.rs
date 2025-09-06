//! Módulo core de Eclipse OS
//! Contiene las funcionalidades nativas del kernel de Eclipse

pub mod memory;
pub mod process;
pub mod filesystem;
pub mod network;
pub mod security;

use crate::KernelResult;

/// Inicializar el sistema core de Eclipse
pub fn init_eclipse_core() -> KernelResult<()> {
    // Inicializando sistema core de Eclipse
    
    // Inicializar subsistemas core de Eclipse
    memory::init_eclipse_memory()?;
    process::init_eclipse_process()?;
    filesystem::init_eclipse_filesystem()?;
    network::init_eclipse_network()?;
    security::init_eclipse_security()?;
    
    // Sistema core de Eclipse inicializado correctamente
    Ok(())
}

/// Procesar eventos del sistema core de Eclipse
pub fn process_eclipse_events() -> KernelResult<()> {
    // Procesar eventos de memoria
    memory::process_memory_events()?;
    
    // Procesar eventos de procesos
    process::process_process_events()?;
    
    // Procesar eventos de filesystem
    filesystem::process_filesystem_events()?;
    
    // Procesar eventos de red
    network::process_network_events()?;
    
    // Procesar eventos de seguridad
    security::process_security_events()?;
    
    Ok(())
}
