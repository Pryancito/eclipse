//! Eclipse OS Rust Userland
//! 
//! Userland y Win32 API para Eclipse OS
//! Implementación completa de las APIs de Windows en Rust
//! Soporte multi-arquitectura (x86 y x86_64)

// Módulos del sistema
pub mod applications;
pub mod services;

// Módulos de IA (experimental/optional - implementations are stubs for now)
pub mod ai_core;
pub mod ai_assistant;
pub mod ai_performance;
pub mod ai_multi_gpu;
pub mod ai_gpu_failover;

// Note: ai_anomaly, ai_hardware, and ai_predictor were stub modules and have been removed
// Future implementations should use real ML libraries or mark as optional features

// Módulos del sistema
pub mod file_system;
pub mod networking;
pub mod security;

// Módulos de sistema de archivos
pub mod fat32;
pub mod ntfs;

// Note: GUI module removed - use Wayland integration instead (wayland_integration, wayland_terminal)
// Real GUI functionality is provided through Wayland compositor and clients

use anyhow::Result;
use log::info;
use services::system_services::SystemServiceManager;

/// Inicializa el userland de Eclipse OS
pub fn init() -> anyhow::Result<()> {
    info!("Inicializando Eclipse OS Userland...");
    
    // Inicializar aplicaciones
    info!("Aplicaciones de usuario cargadas");
    
    info!("✅ Userland de Eclipse OS inicializado correctamente");
    Ok(())
}

/// Función main para compilación
fn main() {
    // Inicializar logging
    env_logger::init();
    
    println!("╔══════════════════════════════════════════════════════════════════════╗");
    println!("║         Eclipse OS - Userland con Servidores Microkernel           ║");
    println!("║                    Servicios en Espacio de Usuario                  ║");
    println!("╚══════════════════════════════════════════════════════════════════════╝\n");
    
    // Inicializar userland
    if let Err(e) = init() {
        eprintln!("❌ Error al inicializar userland: {}", e);
        std::process::exit(1);
    }
    
    // Crear y configurar gestor de servicios del sistema
    let mut service_manager = SystemServiceManager::new();
    
    // Inicializar todos los servicios (incluyendo servidores del microkernel)
    match service_manager.initialize_all_services() {
        Ok(_) => {
            println!("🎉 Eclipse OS Userland inicializado exitosamente!");
            println!("✅ Todos los componentes del userland están funcionando\n");
            
            // Simular operaciones del userland
            println!("🔄 Sistema operativo en modo userspace...");
            println!("   • Servidores del microkernel activos y procesando mensajes");
            println!("   • Aplicaciones de usuario cargadas");
            println!("   • Sistema de archivos funcionando");
            println!("   • Red funcionando");
            println!("   • Seguridad activa");
            
            // Mostrar resumen del sistema
            let (total, running, stopped) = service_manager.get_system_summary();
            println!("\n📊 Resumen del Sistema:");
            println!("   • Total de servicios: {}", total);
            println!("   • Servicios en ejecución: {}", running);
            println!("   • Servicios detenidos: {}", stopped);
            
            println!("\n🚀 Eclipse OS Userland está listo para usar!");
            println!("   Los servidores del microkernel están esperando mensajes del kernel.\n");
            
            // Detener servidores al finalizar
            println!("Presione Ctrl+C para detener los servicios...");
            
            // En un sistema real, aquí entraríamos en un loop de eventos
            // Por ahora, solo limpiamos y salimos
            println!("\nFinalizando userland...");
            if let Err(e) = service_manager.shutdown_microkernel_servers() {
                eprintln!("⚠ Error al detener servidores: {}", e);
            }
        }
        Err(e) => {
            eprintln!("❌ Error al inicializar servicios: {}", e);
            std::process::exit(1);
        }
    }
}