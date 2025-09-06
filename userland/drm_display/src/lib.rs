//! Sistema DRM simplificado para control de pantalla en Eclipse OS
//! 
//! Este módulo proporciona una interfaz simplificada para controlar
//! la pantalla usando DRM (Direct Rendering Manager) en userland.

use anyhow::Result;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::fd::AsRawFd;

/// Error types para el sistema DRM
#[derive(thiserror::Error, Debug)]
pub enum DrmError {
    #[error("No se pudo abrir el dispositivo DRM")]
    DeviceOpenFailed,
    #[error("Error de I/O: {0}")]
    IoError(#[from] io::Error),
    #[error("Error de DRM: {0}")]
    DrmError(String),
}

/// Estructura simplificada del sistema DRM
pub struct DrmDisplay {
    device_fd: i32,
    width: u32,
    height: u32,
    is_initialized: bool,
}

impl DrmDisplay {
    /// Crear una nueva instancia del sistema DRM
    pub fn new() -> Result<Self, DrmError> {
        // Intentar abrir diferentes dispositivos DRM
        let device_paths = [
            "/dev/dri/card0",
            "/dev/dri/card1", 
            "/dev/dri/card2",
            "/dev/dri/card3",
        ];
        
        let mut device_fd = None;
        let mut last_error = None;
        
        for path in &device_paths {
            match OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
            {
                Ok(file) => {
                    device_fd = Some(file.as_raw_fd());
                    break;
                }
                Err(e) => {
                    last_error = Some(e);
                }
            }
        }
        
        let device_fd = device_fd
            .ok_or_else(|| {
                DrmError::DrmError(
                    last_error
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "No se encontró ningún dispositivo DRM".to_string())
                )
            })?;
        
        Ok(Self {
            device_fd,
            width: 1920,  // Resolución por defecto
            height: 1080,
            is_initialized: false,
        })
    }
    
    /// Inicializar el sistema DRM
    pub fn initialize(&mut self) -> Result<(), DrmError> {
        // En una implementación real, aquí configuraríamos el modo DRM
        // Por ahora, solo marcamos como inicializado
        self.is_initialized = true;
        Ok(())
    }
    
    /// Mostrar pantalla negra con "Eclipse OS" centrado
    pub fn show_eclipse_os_centered(&mut self) -> Result<(), DrmError> {
        if !self.is_initialized {
            self.initialize()?;
        }
        
        // Limpiar pantalla
        self.clear_screen()?;
        
        // Mostrar "Eclipse OS" centrado
        self.draw_centered_text("Eclipse OS")?;
        
        Ok(())
    }
    
    /// Limpiar pantalla (hacerla completamente negra)
    pub fn clear_screen(&self) -> Result<(), DrmError> {
        // Usar códigos ANSI para limpiar la pantalla
        print!("\x1b[2J\x1b[H"); // Limpiar pantalla y mover cursor al inicio
        print!("\x1b[40m"); // Fondo negro
        print!("\x1b[37m"); // Texto blanco
        io::stdout().flush()?;
        Ok(())
    }
    
    /// Dibujar texto centrado
    fn draw_centered_text(&self, text: &str) -> Result<(), DrmError> {
        // Calcular posición central
        let screen_width = 80; // Ancho de terminal estándar
        let text_len = text.len();
        let start_col = (screen_width - text_len) / 2;
        let start_row = 12; // Fila central aproximada
        
        // Mover cursor a la posición central
        print!("\x1b[{};{}H", start_row, start_col);
        print!("\x1b[1m"); // Texto en negrita
        print!("\x1b[32m"); // Texto verde
        print!("{}", text);
        print!("\x1b[0m"); // Reset atributos
        io::stdout().flush()?;
        
        Ok(())
    }
    
    /// Obtener dimensiones de la pantalla
    pub fn get_dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    
    /// Verificar si está inicializado
    pub fn is_initialized(&self) -> bool {
        self.is_initialized
    }
}

/// Función de conveniencia para mostrar "Eclipse OS" centrado
pub fn show_eclipse_os_centered() -> Result<(), DrmError> {
    let mut display = DrmDisplay::new()?;
    display.show_eclipse_os_centered()?;
    Ok(())
}

/// Función de conveniencia para mostrar pantalla negra
pub fn show_black_screen() -> Result<(), DrmError> {
    let display = DrmDisplay::new()?;
    display.clear_screen()?;
    Ok(())
}

/// Función para mostrar mensaje de bienvenida completo
pub fn show_eclipse_welcome() -> Result<(), DrmError> {
    let display = DrmDisplay::new()?;
    
    // Limpiar pantalla
    display.clear_screen()?;
    
    // Mostrar "Eclipse OS" centrado
    display.draw_centered_text("Eclipse OS")?;
    
    // Mostrar información adicional
    print!("\x1b[14;1H"); // Fila 14
    print!("\x1b[2m"); // Texto tenue
    print!("Sistema Operativo en Rust");
    print!("\x1b[0m");
    
    print!("\x1b[16;1H"); // Fila 16
    print!("\x1b[2m");
    print!("Iniciando...");
    print!("\x1b[0m");
    
    io::stdout().flush()?;
    Ok(())
}