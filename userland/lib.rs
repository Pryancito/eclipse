//! Biblioteca del userland de Eclipse OS
//! 
//! Esta biblioteca proporciona las funciones básicas para las aplicaciones
//! del userland que se ejecutan sobre el kernel Eclipse OS.

#![no_std]

extern crate alloc;


#[cfg(feature = "alloc")]
use alloc::string::String;

/// Convierte un &str a String
#[cfg(feature = "alloc")]
pub fn str_to_string(s: &str) -> String {
    s.to_string()
}

/// Inicializa el userland de Eclipse OS
pub fn init_userland() -> Result<(), &'static str> {
    // Inicializar el sistema de userland
    // TODO: Implementar inicialización del sistema
    
    Ok(())
}

/// Obtiene información del sistema
pub fn get_system_info() -> SystemInfo {
    SystemInfo {
        kernel_version: "Eclipse OS 0.2.0",
        wayland_version: "1.0",
        userland_version: "0.2.0",
        architecture: "x86_64",
    }
}

/// Información del sistema
#[derive(Debug, Clone)]
pub struct SystemInfo {
    pub kernel_version: &'static str,
    pub wayland_version: &'static str,
    pub userland_version: &'static str,
    pub architecture: &'static str,
}

