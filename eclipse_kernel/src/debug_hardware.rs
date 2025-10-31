#![allow(dead_code)]
//! Sistema de debug para hardware real
//! 
//! Este módulo proporciona logging detallado para diagnosticar problemas
//! en hardware real que no ocurren en emulación.

use core::sync::atomic::{AtomicU32, Ordering};
use alloc::format;
use alloc::string::String;

static DEBUG_LEVEL: AtomicU32 = AtomicU32::new(1); // 0=off, 1=basic, 2=detailed, 3=verbose

/// Nivel de debug
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DebugLevel {
    Off = 0,
    Basic = 1,
    Detailed = 2,
    Verbose = 3,
}

/// Configurar nivel de debug
pub fn set_debug_level(level: DebugLevel) {
    DEBUG_LEVEL.store(level as u32, Ordering::Relaxed);
}

/// Obtener nivel de debug actual
pub fn get_debug_level() -> DebugLevel {
    match DEBUG_LEVEL.load(Ordering::Relaxed) {
        0 => DebugLevel::Off,
        1 => DebugLevel::Basic,
        2 => DebugLevel::Detailed,
        3 => DebugLevel::Verbose,
        _ => DebugLevel::Basic,
    }
}

/// Log de debug básico
pub fn debug_basic(component: &str, message: &str) {
    if get_debug_level() as u32 >= DebugLevel::Basic as u32 {
        debug_print(format!("[DEBUG] {}: {}", component, message));
    }
}

/// Log de debug detallado
pub fn debug_detailed(component: &str, message: &str) {
    if get_debug_level() as u32 >= DebugLevel::Detailed as u32 {
        debug_print(format!("[DETAILED] {}: {}", component, message));
    }
}

/// Log de debug verbose
pub fn debug_verbose(component: &str, message: &str) {
    if get_debug_level() as u32 >= DebugLevel::Verbose as u32 {
        debug_print(format!("[VERBOSE] {}: {}", component, message));
    }
}

/// Log de error crítico
pub fn debug_error(component: &str, message: &str) {
    debug_print(format!("[ERROR] {}: {}", component, message));
}

/// Log de advertencia
pub fn debug_warning(component: &str, message: &str) {
    debug_print(format!("[WARNING] {}: {}", component, message));
}

/// Función de impresión de debug
fn debug_print(message: String) {
    // En hardware real, esto debería escribir a puerto serie o VGA
    // Por ahora, simulamos la salida
    unsafe {
        // Escribir a VGA buffer para debug
        let vga_buffer = 0xb8000 as *mut u8;
        let msg_bytes = message.as_bytes();
        
        // Limpiar línea actual
        for i in 0..80 {
            *vga_buffer.add(i * 2) = b' ';
            *vga_buffer.add(i * 2 + 1) = 0x07; // Gris sobre negro
        }
        
        // Escribir mensaje
        for (i, &byte) in msg_bytes.iter().enumerate() {
            if i < 80 {
                *vga_buffer.add(i * 2) = byte;
                *vga_buffer.add(i * 2 + 1) = 0x0F; // Blanco sobre negro
            }
        }
    }
}

/// Función para pausar el sistema y permitir debug
pub fn debug_pause(component: &str, message: &str) {
    debug_error(component, message);
    debug_error("DEBUG", "Sistema pausado para debug. Presiona cualquier tecla...");
    
    // Bucle de espera para permitir leer el mensaje
    let mut counter = 0;
    loop {
        unsafe {
            core::arch::asm!("nop");
        }
        counter += 1;
        
        // Pausa por un tiempo para permitir leer
        if counter > 10000000 {
            break;
        }
    }
}

/// Función para reinicio controlado con logging
pub fn debug_reboot(reason: &str) {
    debug_error("REBOOT", reason);
    debug_error("REBOOT", "Reiniciando sistema en 5 segundos...");
    
    // Esperar 5 segundos
    for i in (1..=5).rev() {
        debug_error("REBOOT", &format!("Reiniciando en {}...", i));
        
        // Espera aproximada de 1 segundo
        let mut counter = 0;
        while counter < 1000000 {
            unsafe {
                core::arch::asm!("nop");
            }
            counter += 1;
        }
    }
    
    // Reiniciar
    unsafe {
        core::arch::asm!("hlt");
    }
}
