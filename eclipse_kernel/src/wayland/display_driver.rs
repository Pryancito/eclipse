//! Driver de display para Wayland en Eclipse OS
//! 
//! Implementa el driver de display específico para Wayland.

use super::protocol::*;
use super::compositor::*;
use super::output::*;
use core::sync::atomic::{AtomicBool, Ordering};
use alloc::vec::Vec;

/// Driver de display Wayland
pub struct WaylandDisplayDriver {
    pub is_initialized: AtomicBool,
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
    pub compositor: WaylandCompositor,
    pub output_manager: OutputManager,
}

impl WaylandDisplayDriver {
    pub fn new() -> Self {
        Self {
            is_initialized: AtomicBool::new(false),
            width: 1920,
            height: 1080,
            refresh_rate: 60,
            compositor: WaylandCompositor::new(),
            output_manager: OutputManager::new(),
        }
    }
    
    /// Inicializar driver
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        if self.is_initialized.load(Ordering::Acquire) {
            return Ok(());
        }
        
        // Inicializar compositor
        self.compositor.initialize()?;
        
        // Crear output principal
        let main_output = WaylandOutput::new(self.width, self.height, self.refresh_rate);
        self.output_manager.add_output(main_output);
        
        self.is_initialized.store(true, Ordering::Release);
        Ok(())
    }
    
    /// Establecer resolución
    pub fn set_resolution(&mut self, width: u32, height: u32) -> Result<(), &'static str> {
        self.width = width;
        self.height = height;
        
        // Actualizar output principal
        if let Some(output) = self.output_manager.get_output_mut(1) {
            output.set_resolution(width, height);
        }
        
        Ok(())
    }
    
    /// Establecer tasa de refresco
    pub fn set_refresh_rate(&mut self, rate: u32) -> Result<(), &'static str> {
        self.refresh_rate = rate;
        
        // Actualizar output principal
        if let Some(output) = self.output_manager.get_output_mut(1) {
            output.set_refresh_rate(rate);
        }
        
        Ok(())
    }
    
    /// Renderizar frame
    pub fn render_frame(&mut self) -> Result<(), &'static str> {
        if !self.is_initialized.load(Ordering::Acquire) {
            return Err("Driver not initialized");
        }
        
        self.compositor.render_frame()
    }
    
    /// Obtener información del display
    pub fn get_display_info(&self) -> DisplayInfo {
        DisplayInfo {
            width: self.width,
            height: self.height,
            refresh_rate: self.refresh_rate,
            is_initialized: self.is_initialized.load(Ordering::Acquire),
            surface_count: self.compositor.surfaces.len(),
        }
    }
}

/// Información del display
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
    pub is_initialized: bool,
    pub surface_count: usize,
}
