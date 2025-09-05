//! Bootloader UEFI minimalista para Eclipse OS
//! 
//! Versión ultra-simplificada que solo muestra mensajes

#![no_std]
#![no_main]

use uefi::prelude::*;
use uefi::table::boot::BootServices;
use uefi::table::runtime::ResetType;
use uefi::helpers::init;

#[entry]
fn main(_handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    // Inicializar servicios UEFI
    init(&mut system_table).unwrap();
    
    // Mostrar banner simple
    println!("🌙 Eclipse OS Bootloader");
    println!("   Versión minimalista");
    println!("   Kernel Eclipse listo para cargar");
    
    // Simular carga del kernel
    println!("   Cargando kernel Eclipse...");
    println!("   ✅ Kernel cargado exitosamente");
    println!("   🚀 Iniciando sistema Eclipse...");
    
    // En un bootloader real, aquí se cargaría y ejecutaría el kernel
    // Por ahora, simplemente mostramos que todo está listo
    println!("   ✅ Sistema Eclipse iniciado");
    
    // Reiniciar después de un tiempo
    println!("   🔄 Reiniciando en 3 segundos...");
    
    // Esperar un poco (en un bootloader real esto sería más sofisticado)
    for i in (1..=3).rev() {
        println!("   {}...", i);
    }
    
    // Reiniciar sistema
    unsafe {
        let rt = system_table.runtime_services();
        rt.reset(ResetType::Cold, uefi::Status::SUCCESS, None);
    }
}
