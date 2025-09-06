//! Ejemplo de uso del sistema DRM para mostrar "Eclipse OS" centrado

use drm_display::{DrmDisplay, show_eclipse_os_centered, show_black_screen};
use std::thread;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Iniciando sistema DRM para Eclipse OS...");
    
    // Mostrar "Eclipse OS" centrado
    match show_eclipse_os_centered() {
        Ok(()) => {
            println!("Pantalla configurada correctamente");
            
            // Mantener la pantalla visible por 10 segundos
            thread::sleep(Duration::from_secs(10));
            
            // Limpiar pantalla
            show_black_screen()?;
            println!("Pantalla limpiada");
        }
        Err(e) => {
            eprintln!("Error configurando pantalla: {}", e);
            return Err(e.into());
        }
    }
    
    Ok(())
}
