use ipc_common::*;
use binary_driver_loader::{BinaryDriverLoader, DriverMetadata};
use anyhow::Result;

/// Demostración del sistema de carga de drivers binarios
fn main() -> Result<()> {
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║            ECLIPSE OS BINARY DRIVER DEMO                     ║");
    println!("║                Carga de Drivers Binarios                     ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    
    let mut loader = BinaryDriverLoader::new();
    
    println!("\n🔧 CREANDO DRIVERS BINARIOS DE EJEMPLO");
    println!("=======================================");
    
    // Crear drivers binarios de ejemplo
    let nvidia_driver_file = loader.create_example_driver("nvidia_graphics", DriverType::NVIDIA)?;
    let amd_driver_file = loader.create_example_driver("amd_graphics", DriverType::AMD)?;
    let intel_driver_file = loader.create_example_driver("intel_graphics", DriverType::Intel)?;
    let custom_driver_file = loader.create_example_driver("custom_audio", DriverType::Custom("Audio".to_string()))?;
    
    println!("\n📦 CARGANDO DRIVERS BINARIOS");
    println!("=============================");
    
    // Cargar drivers desde archivos binarios
    let nvidia_id = loader.load_driver_from_file(&nvidia_driver_file)?;
    let amd_id = loader.load_driver_from_file(&amd_driver_file)?;
    let intel_id = loader.load_driver_from_file(&intel_driver_file)?;
    let custom_id = loader.load_driver_from_file(&custom_driver_file)?;
    
    println!("\n🎮 PROBANDO DRIVERS BINARIOS");
    println!("=============================");
    
    // Probar comando en driver NVIDIA
    println!("Probando driver NVIDIA (ID: {})", nvidia_id);
    let nvidia_result = loader.execute_binary_driver_command(
        nvidia_id,
        DriverCommandType::ExecuteCommand { command: "get_info".to_string() },
        Vec::new()
    )?;
    println!("  Resultado: {}", String::from_utf8_lossy(&nvidia_result));
    
    // Probar comando en driver AMD
    println!("Probando driver AMD (ID: {})", amd_id);
    let amd_result = loader.execute_binary_driver_command(
        amd_id,
        DriverCommandType::GetStatus,
        Vec::new()
    )?;
    println!("  Estado: {}", String::from_utf8_lossy(&amd_result));
    
    // Probar comando en driver Intel
    println!("Probando driver Intel (ID: {})", intel_id);
    let intel_result = loader.execute_binary_driver_command(
        intel_id,
        DriverCommandType::GetCapabilities,
        Vec::new()
    )?;
    println!("  Capacidades: {}", String::from_utf8_lossy(&intel_result));
    
    // Probar comando en driver personalizado
    println!("Probando driver personalizado (ID: {})", custom_id);
    let custom_result = loader.execute_binary_driver_command(
        custom_id,
        DriverCommandType::ExecuteCommand { command: "get_info".to_string() },
        Vec::new()
    )?;
    println!("  Resultado: {}", String::from_utf8_lossy(&custom_result));
    
    println!("\n📊 INFORMACIÓN DE DRIVERS BINARIOS");
    println!("===================================");
    
    // Listar metadatos de drivers binarios
    let binary_drivers = loader.list_binary_drivers();
    for driver in binary_drivers {
        println!("  - {} v{}", driver.name, driver.version);
        println!("    Autor: {}", driver.author);
        println!("    Tipo: {:?}", driver.driver_type);
        println!("    Tamaño: {} bytes", driver.file_size);
        println!("    Checksum: {}", driver.checksum);
        println!("    Arquitectura: {}", driver.target_arch);
        println!("    OS: {}", driver.target_os);
        println!("    Dependencias: {:?}", driver.dependencies);
        println!("    Capacidades: {:?}", driver.capabilities);
        println!();
    }
    
    println!("\n🔧 CARGANDO DESDE DIRECTORIO");
    println!("=============================");
    
    // Crear directorio de drivers
    std::fs::create_dir_all("drivers")?;
    
    // Mover drivers al directorio
    std::fs::rename(&nvidia_driver_file, "drivers/nvidia_graphics.edriver")?;
    std::fs::rename(&amd_driver_file, "drivers/amd_graphics.edriver")?;
    std::fs::rename(&intel_driver_file, "drivers/intel_graphics.edriver")?;
    std::fs::rename(&custom_driver_file, "drivers/custom_audio.edriver")?;
    
    // Crear nuevo loader para probar carga desde directorio
    let mut dir_loader = BinaryDriverLoader::new();
    let loaded_drivers = dir_loader.load_drivers_from_directory("drivers")?;
    
    println!("Drivers cargados desde directorio: {:?}", loaded_drivers);
    
    println!("\n🧪 PRUEBAS DE COMPATIBILIDAD");
    println!("=============================");
    
    // Crear driver incompatible para probar verificación
    let incompatible_metadata = DriverMetadata {
        name: "Incompatible Driver".to_string(),
        version: "1.0.0".to_string(),
        author: "Test".to_string(),
        description: "Driver incompatible para prueba".to_string(),
        driver_type: DriverType::Custom("Test".to_string()),
        capabilities: vec![DriverCapability::Custom("Test".to_string())],
        dependencies: vec![],
        entry_point: "main".to_string(),
        file_size: 512,
        checksum: "test_checksum".to_string(),
        target_arch: "arm64".to_string(), // Arquitectura incompatible
        target_os: "eclipse".to_string(),
    };
    
    // Serializar y escribir driver incompatible
    let incompatible_data = bincode::serialize(&incompatible_metadata)?;
    let mut incompatible_binary = Vec::new();
    incompatible_binary.extend_from_slice(b"ECLIPSE_DRIVER_METADATA");
    incompatible_binary.extend_from_slice(&incompatible_data);
    incompatible_binary.push(0);
    
    std::fs::write("incompatible.edriver", &incompatible_binary)?;
    
    // Intentar cargar driver incompatible
    match dir_loader.load_driver_from_file("incompatible.edriver") {
        Ok(_) => println!("❌ Error: Driver incompatible fue cargado"),
        Err(e) => println!("✅ Correcto: Driver incompatible rechazado - {}", e),
    }
    
    println!("\n📈 ESTADÍSTICAS FINALES");
    println!("========================");
    
    let final_drivers = loader.list_binary_drivers();
    println!("Total de drivers binarios: {}", final_drivers.len());
    
    let total_size: u64 = final_drivers.iter().map(|d| d.file_size).sum();
    println!("Tamaño total de drivers: {} bytes", total_size);
    
    let nvidia_drivers = final_drivers.iter().filter(|d| matches!(d.driver_type, DriverType::NVIDIA)).count();
    let amd_drivers = final_drivers.iter().filter(|d| matches!(d.driver_type, DriverType::AMD)).count();
    let intel_drivers = final_drivers.iter().filter(|d| matches!(d.driver_type, DriverType::Intel)).count();
    let custom_drivers = final_drivers.iter().filter(|d| matches!(d.driver_type, DriverType::Custom(_))).count();
    
    println!("  - Drivers NVIDIA: {}", nvidia_drivers);
    println!("  - Drivers AMD: {}", amd_drivers);
    println!("  - Drivers Intel: {}", intel_drivers);
    println!("  - Drivers personalizados: {}", custom_drivers);
    
    println!("\n✅ Demostración de drivers binarios completada exitosamente!");
    
    // Limpiar archivos de prueba
    std::fs::remove_dir_all("drivers")?;
    std::fs::remove_file("incompatible.edriver")?;
    
    Ok(())
}
