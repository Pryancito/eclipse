use std::io::{self, Write};

mod disk_manager;
mod direct_installer;
mod uefi_config;
mod validation;
mod kernel_eclipsefs;
mod eclipsefs_writer;

use disk_manager::DiskManager;
use direct_installer::DirectInstaller;
use validation::{SystemValidator, is_uefi_system, is_secure_boot_enabled};

fn main() {
    println!("Eclipse OS Installer v0.6.0");
    println!("=============================");
    println!();
    
    // Verificar permisos de root
    if !is_root() {
        println!("Error: Este instalador debe ejecutarse como root");
        println!("   Usa: sudo ./eclipse-installer");
        std::process::exit(1);
    }
    
    // Validar sistema
    let validator = SystemValidator::new();
    if let Err(e) = validator.validate_system() {
        println!("Error de validación: {}", e);
        println!("   Asegúrate de que todos los comandos requeridos estén instalados");
        println!("   y que los archivos del sistema estén compilados");
        std::process::exit(1);
    }
    
    // Verificar sistema UEFI
    if !is_uefi_system() {
        println!("Advertencia: No se detectó un sistema UEFI");
        println!("   Eclipse OS está optimizado para sistemas UEFI");
        println!("   La instalación puede no funcionar correctamente en sistemas BIOS");
    }
    
    // Verificar Secure Boot
    if is_secure_boot_enabled() {
        println!("Advertencia: Secure Boot está habilitado");
        println!("   Puede ser necesario deshabilitar Secure Boot para Eclipse OS");
    }
    
    // Instalar automáticamente a /dev/sda
    println!("Instalando Eclipse OS automáticamente a /dev/sda...");
    
    // Crear un disco virtual para /dev/sda
    let disk = DiskInfo {
        name: "/dev/sda".to_string(),
        size: "250GB".to_string(),
        model: "Virtual Disk".to_string(),
        disk_type: "SSD".to_string(),
    };
    
    // Ejecutar instalación directa
    let direct_installer = DirectInstaller::new();
    match direct_installer.install_eclipse_os(&disk, true) {
        Ok(_) => {
            println!();
            println!("¡Instalación completada exitosamente!");
        }
        Err(e) => {
            println!("Error durante la instalación: {}", e);
        }
    }
}

fn show_main_menu() {
    println!("Menú Principal");
    println!("===============");
    println!("1. Instalar Eclipse OS");
    println!("2. Mostrar información de discos");
    println!("3. Ayuda");
    println!("4. Salir");
    println!();
}


fn install_eclipse_os_direct() {
    println!("Instalacion de Eclipse OS");
    println!("=========================");
    println!();
    
    // Mostrar discos disponibles
    let mut disk_manager = DiskManager::new();
    let disks = disk_manager.list_disks();
    
    if disks.is_empty() {
        println!("No se encontraron discos disponibles");
        return;
    }
    
    println!("Discos disponibles:");
    for (i, disk) in disks.iter().enumerate() {
        println!("  {}. {} ({})", i + 1, disk.name, disk.size);
    }
    println!();
    
    // Seleccionar disco
    let disk_choice = read_input("Selecciona el disco donde instalar (numero): ");
    let disk_index: usize = match disk_choice.trim().parse::<usize>() {
        Ok(n) => n - 1,
        Err(_) => {
            println!("Numero invalido");
            return;
        }
    };
    
    if disk_index >= disks.len() {
        println!("Numero de disco invalido");
        return;
    }
    
    let selected_disk = &disks[disk_index];
    
    // Validar disco seleccionado
    let validator = SystemValidator::new();
    if let Err(e) = validator.validate_disk(&selected_disk.name) {
        println!("Error validando disco: {}", e);
        return;
    }
    
    // Verificar espacio en disco
    if let Err(e) = validator.check_disk_space(&selected_disk.name) {
        println!("Error de espacio en disco: {}", e);
        return;
    }
    
    // Validar módulos userland
    if let Err(e) = validator.validate_userland_modules() {
        println!("Advertencia: {}", e);
    }
    
    // Preguntar si es instalación automática
    let auto_choice = read_input("Instalacion automatica? (s/N): ");
    let auto_install = auto_choice.trim().to_lowercase() == "s";
    
    // Ejecutar instalación directa
    let direct_installer = DirectInstaller::new();
    match direct_installer.install_eclipse_os(selected_disk, auto_install) {
        Ok(_) => {
            println!();
            println!("Instalacion completada exitosamente!");
        }
        Err(e) => {
            println!("Error durante la instalacion: {}", e);
        }
    }
}

fn show_disk_info() {
    println!("Información de Discos");
    println!("======================");
    println!();
    
    let mut disk_manager = DiskManager::new();
    let disks = disk_manager.list_disks();
    
    if disks.is_empty() {
        println!("No se encontraron discos");
        return;
    }
    
    for disk in disks {
        println!("Disco: {}", disk.name);
        println!("   Tamaño: {}", disk.size);
        println!("   Modelo: {}", disk.model);
        println!("   Tipo: {}", disk.disk_type);
        println!();
    }
}

fn show_help() {
    println!("Ayuda del Instalador");
    println!("====================");
    println!();
    println!("Este instalador te permite instalar Eclipse OS en tu disco duro.");
    println!();
    println!("Requisitos:");
    println!("  - Disco duro con al menos 1GB de espacio libre");
    println!("  - Sistema UEFI compatible");
    println!("  - Conexión a internet (para descargar dependencias)");
    println!();
    println!("Advertencias:");
    println!("  - La instalación borrará todos los datos del disco seleccionado");
    println!("  - Haz una copia de seguridad de tus datos importantes");
    println!("  - Asegúrate de seleccionar el disco correcto");
    println!();
    println!("Proceso de instalación:");
    println!("  1. Selección del disco de destino");
    println!("  2. Creación de particiones (EFI + Root)");
    println!("  3. Instalación del bootloader UEFI");
    println!("  4. Configuración del sistema de archivos");
    println!("  5. Instalación de archivos del sistema");
    println!();
}


fn is_root() -> bool {
    unsafe {
        libc::getuid() == 0
    }
}

fn read_input(prompt: &str) -> String {
    print!("{}", prompt);
    io::stdout().flush().unwrap();
    
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    input
}

#[derive(Debug, Clone)]
pub struct DiskInfo {
    pub name: String,
    pub size: String,
    pub model: String,
    pub disk_type: String,
}

#[derive(Debug, Clone)]
pub struct PartitionInfo {
    pub name: String,
    pub mount_point: String,
    pub filesystem: String,
    pub size: String,
}
