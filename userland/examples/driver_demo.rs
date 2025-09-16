use ipc_common::*;
use driver_loader::{DriverLoader, create_driver_loader};
use anyhow::Result;

/// Demostración del sistema IPC de drivers
fn main() -> Result<()> {
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║                ECLIPSE OS DRIVER DEMO                        ║");
    println!("║                    Sistema IPC de Drivers                    ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    
    let mut loader = create_driver_loader();
    
    println!("\n🔧 DRIVERS PREDEFINIDOS");
    println!("========================");
    
    // Listar drivers predefinidos
    let drivers = loader.list_drivers();
    for driver in &drivers {
        println!("  - {} (ID: {}) - {:?}", driver.config.name, driver.id, driver.status);
        println!("    Versión: {} | Autor: {}", driver.config.version, driver.config.author);
        println!("    Capacidades: {:?}", driver.config.capabilities);
        println!();
    }
    
    println!("🎮 PROBANDO DRIVER NVIDIA");
    println!("=========================");
    
    // Buscar driver NVIDIA
    if let Some(nvidia_driver) = drivers.iter().find(|d| d.config.name.contains("NVIDIA")) {
        println!("Driver NVIDIA encontrado (ID: {})", nvidia_driver.id);
        
        // Inicializar driver
        let init_result = loader.execute_command(nvidia_driver.id, DriverCommandType::Initialize, vec![])?;
        println!("  Inicialización: {}", String::from_utf8_lossy(&init_result));
        
        // Obtener conteo de GPUs
        let gpu_count_result = loader.execute_command(
            nvidia_driver.id, 
            DriverCommandType::ExecuteCommand { command: "get_gpu_count".to_string() }, 
            vec![]
        )?;
        let gpu_count = u32::from_le_bytes([gpu_count_result[0], gpu_count_result[1], gpu_count_result[2], gpu_count_result[3]]);
        println!("  GPUs detectadas: {}", gpu_count);
        
        // Obtener información de memoria
        let memory_result = loader.execute_command(
            nvidia_driver.id, 
            DriverCommandType::ExecuteCommand { command: "get_memory_info".to_string() }, 
            vec![]
        )?;
        println!("  Memoria: {}", String::from_utf8_lossy(&memory_result));
        
        // Obtener estado del driver
        let status_result = loader.execute_command(nvidia_driver.id, DriverCommandType::GetStatus, vec![])?;
        println!("  Estado: {}", String::from_utf8_lossy(&status_result));
        
        // Obtener capacidades
        let caps_result = loader.execute_command(nvidia_driver.id, DriverCommandType::GetCapabilities, vec![])?;
        println!("  Capacidades: {}", String::from_utf8_lossy(&caps_result));
    }
    
    println!("\n🔧 CARGANDO DRIVER PERSONALIZADO");
    println!("=================================");
    
    // Cargar driver personalizado
    let custom_config = DriverConfig {
        name: "Mi Driver Personalizado".to_string(),
        version: "2.0.0".to_string(),
        author: "Usuario Eclipse OS".to_string(),
        description: "Driver de ejemplo para demostración".to_string(),
        priority: 5,
        auto_load: false,
        memory_limit: 4 * 1024 * 1024, // 4MB
        dependencies: vec!["PCI Driver".to_string()],
        capabilities: vec![
            DriverCapability::Graphics,
            DriverCapability::Custom("MiFuncionalidad".to_string()),
        ],
    };
    
    let custom_driver_id = loader.load_driver(
        DriverType::Custom("Personalizado".to_string()),
        "Mi Driver Personalizado".to_string(),
        vec![], // Datos binarios del driver
        custom_config,
    )?;
    
    println!("✓ Driver personalizado cargado con ID: {}", custom_driver_id);
    
    // Probar comando personalizado
    let custom_result = loader.execute_command(
        custom_driver_id,
        DriverCommandType::ExecuteCommand { command: "mi_comando".to_string() },
        vec![]
    )?;
    println!("  Resultado comando personalizado: {}", String::from_utf8_lossy(&custom_result));
    
    println!("\n📊 ESTADÍSTICAS FINALES");
    println!("========================");
    
    let final_drivers = loader.list_drivers();
    println!("Total de drivers: {}", final_drivers.len());
    
    let ready_drivers = final_drivers.iter().filter(|d| matches!(d.status, DriverStatus::Ready)).count();
    println!("Drivers listos: {}", ready_drivers);
    
    let total_memory: u64 = final_drivers.iter().map(|d| d.memory_usage).sum();
    println!("Memoria total utilizada: {} bytes", total_memory);
    
    println!("\n✅ Demostración completada exitosamente!");
    
    Ok(())
}
