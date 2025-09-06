//! Servidor Wayland para Eclipse OS
//! 
//! Implementa el servidor principal de Wayland.

use super::protocol::*;
use super::display::*;
use core::sync::atomic::{AtomicBool, Ordering};
use alloc::vec::Vec;

/// Servidor Wayland
pub struct WaylandServer {
    pub is_running: AtomicBool,
    pub display: WaylandDisplay,
    pub port: u16,
}

impl WaylandServer {
    pub fn new(port: u16) -> Self {
        Self {
            is_running: AtomicBool::new(false),
            display: WaylandDisplay::new(),
            port,
        }
    }
    
    /// Inicializar servidor
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        self.display.initialize()?;
        self.is_running.store(true, Ordering::Release);
        Ok(())
    }
    
    /// Ejecutar servidor
    pub fn run(&mut self) -> Result<(), &'static str> {
        if !self.is_running.load(Ordering::Acquire) {
            return Err("Server not running");
        }
        
        // Bucle principal del servidor
        loop {
            self.display.process_events()?;
            
            // En un sistema real, aquí habría un sleep o wait
            // Por ahora, simulamos el bucle
            break;
        }
        
        Ok(())
    }
    
    /// Detener servidor
    pub fn stop(&mut self) {
        self.is_running.store(false, Ordering::Release);
    }
}
