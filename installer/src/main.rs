use std::io::{self, Write};
use clap::{Parser, Subcommand};

mod disk_manager;
mod direct_installer;
mod uefi_config;
mod validation;
mod kernel_eclipsefs;
mod eclipsefs_writer;

// Nuevos modulos v0.2.0
pub mod installer_core;
pub mod disk;
pub mod distro;
pub mod config;
pub mod paths;

use disk_manager::DiskManager;
use direct_installer::DirectInstaller;
use validation::{SystemValidator, is_uefi_system, is_secure_boot_enabled};
use crate::installer_core::installer::EclipseInstaller;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Disco destino para instalacion rapida
    #[arg(short, long)]
    disk: Option<String>,

    /// Confirmar automaticamente todas las acciones
    #[arg(short, long)]
    yes: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Instalar Eclipse OS v0.2.0 (Recomendado)
    Install {
        /// Disco donde instalar
        disk: Option<String>,
        
        /// Saltar confirmaciones
        #[arg(short, long)]
        yes: bool,
    },
    /// Listar discos disponibles
    ListDisks,
    /// Validar requisitos del sistema
    Check,
}

fn main() {
    // Verificar permisos de root al inicio de la aplicación
    if !is_root() {
        println!("Error: Este instalador debe ejecutarse como root");
        println!("   Usa: sudo ./eclipse-installer");
        std::process::exit(1);
    }

    let cli = Cli::parse();

    println!("Eclipse OS Installer v0.2.0");
    println!("=============================");
    println!();

    // Si se pasan argumentos, procesarlos y salir
    if cli.disk.is_some() || cli.command.is_some() {
        process_cli(cli);
        return;
    }

    // Modo interactivo (mantener compatibilidad por ahora)
    run_interactive();
}

fn process_cli(cli: Cli) {
    // Si tenemos --disk y --yes en la raiz (modo rapido)
    if let Some(disk_path) = cli.disk {
        if cli.yes || confirm_destructive(&disk_path) {
            run_install_v2(&disk_path);
        }
        return;
    }

    match cli.command {
        Some(Commands::Install { disk, yes }) => {
            if let Some(disk_path) = disk {
                if yes || confirm_destructive(&disk_path) {
                    run_install_v2(&disk_path);
                }
            } else {
                println!("Error: Debes especificar un disco con --disk /dev/sdX");
            }
        }
        Some(Commands::ListDisks) => {
            show_disk_info();
        }
        Some(Commands::Check) => {
            let validator = SystemValidator::new();
            if let Err(e) = validator.validate_system() {
                println!("❌ Error de validacion: {}", e);
            } else {
                println!("✅ Sistema validado correctamente");
            }
        }
        None => run_interactive(),
    }
}

fn confirm_destructive(disk: &str) -> bool {
    let input = read_input(&format!("⚠️  ATENCION: Se borraran TODOS los datos en {}. ¿Continuar? (s/N): ", disk));
    input.trim().to_lowercase() == "s"
}

fn run_install_v2(disk: &str) {
    match EclipseInstaller::new() {
        Ok(installer) => {
            if let Err(e) = installer.run_install(disk) {
                println!("\n❌ Error durante la instalacion: {:?}", e);
            }
        },
        Err(e) => println!("Error inicializando instalador: {:?}", e),
    }
}

fn run_interactive() {
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
    
    // Mostrar menú principal
    loop {
        show_main_menu();
        
        let choice = read_input("Selecciona una opción: ");
        
        match choice.trim() {
            "0" => {
                install_eclipse_os_v2();
            }
            "1" => {
                install_eclipse_os_direct();
            }
            "2" => {
                show_disk_info();
            }
            "3" => {
                show_help();
            }
            "4" => {
                println!("¡Hasta luego!");
                break;
            }
            _ => {
                println!("Opción inválida. Intenta de nuevo.");
            }
        }
        
        println!();
    }
}

fn show_main_menu() {
    println!("Menú Principal");
    println!("===============");
    println!("0. [NUEVO] Instalar Eclipse OS v0.2.0 (Recomendado)");
    println!("1. Instalar Eclipse OS v0.1.0 (Legado)");
    println!("2. Mostrar información de discos");
    println!("3. Ayuda");
    println!("4. Salir");
    println!();
}

fn install_eclipse_os_v2() {
    println!("Instalacion de Eclipse OS v0.2.0");
    println!("===============================");
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
    
    // Confirmacion destructiva
    let confirm = read_input(&format!("⚠️ ATENCION: Se borraran TODOS los datos en {}. ¿Continuar? (s/N): ", selected_disk.name));
    if confirm.trim().to_lowercase() != "s" {
        println!("Operacion cancelada.");
        return;
    }

    // Ejecutar nuevo instalador
    run_install_v2(&selected_disk.name);
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
    println!("Discos soportados:");
    println!("  - NVMe, SATA/AHCI, VirtIO, IDE/ATA");
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