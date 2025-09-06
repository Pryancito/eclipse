//! Ejemplo de uso del sistema DRM en Eclipse OS

use eclipse_userland::drm_display::{show_eclipse_os_centered, show_eclipse_welcome, EclipseDrmDisplay};
use std::thread;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Iniciando demostración DRM de Eclipse OS...");
    
    // Mostrar pantalla de bienvenida
    show_eclipse_welcome()?;
    
    // Esperar 3 segundos
    thread::sleep(Duration::from_secs(3));
    
    // Crear instancia DRM personalizada
    let mut display = EclipseDrmDisplay::new()?;
    display.show_eclipse_os_centered()?;
    
    // Esperar 5 segundos más
    thread::sleep(Duration::from_secs(5));
    
    // Limpiar pantalla
    display.clear_screen()?;
    
    println!("Demostración DRM completada");
    Ok(())
}
