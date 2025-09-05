use std::io::{self, Write};
use std::process::Command;
use std::fs;
use std::path::Path;

mod disk_manager;
mod partition_manager;
mod bootloader_installer;
mod filesystem_manager;

use disk_manager::DiskManager;
use partition_manager::PartitionManager;
use bootloader_installer::BootloaderInstaller;
use filesystem_manager::FilesystemManager;

fn main() {
    println!("🌙 Eclipse OS Installer v1.0");
    println!("=============================");
    println!();
    
    // Verificar permisos de root
    if !is_root() {
        println!("❌ Error: Este instalador debe ejecutarse como root");
        println!("   Usa: sudo ./eclipse-installer");
        std::process::exit(1);
    }
    
    // Mostrar menú principal
    loop {
        show_main_menu();
        
        let choice = read_input("Selecciona una opción: ");
        
        match choice.trim() {
            "1" => {
                install_eclipse_os();
            }
            "2" => {
                show_disk_info();
            }
            "3" => {
                show_help();
            }
            "4" => {
                println!("👋 ¡Hasta luego!");
                break;
            }
            _ => {
                println!("❌ Opción inválida. Intenta de nuevo.");
            }
        }
        
        println!();
    }
}

fn show_main_menu() {
    println!("📋 Menú Principal");
    println!("=================");
    println!("1. Instalar Eclipse OS");
    println!("2. Mostrar información de discos");
    println!("3. Ayuda");
    println!("4. Salir");
    println!();
}

fn install_eclipse_os() {
    println!("🔧 Instalación de Eclipse OS");
    println!("=============================");
    println!();
    
    // 1. Mostrar discos disponibles
    let mut disk_manager = DiskManager::new();
    let disks = disk_manager.list_disks();
    
    if disks.is_empty() {
        println!("❌ No se encontraron discos disponibles");
        return;
    }
    
    println!("💾 Discos disponibles:");
    for (i, disk) in disks.iter().enumerate() {
        println!("  {}. {} ({})", i + 1, disk.name, disk.size);
    }
    println!();
    
    // 2. Seleccionar disco
    let disk_choice = read_input("Selecciona el disco donde instalar (número): ");
    let disk_index: usize = match disk_choice.trim().parse::<usize>() {
        Ok(n) => n - 1,
        Err(_) => {
            println!("❌ Número inválido");
            return;
        }
    };
    
    if disk_index >= disks.len() {
        println!("❌ Número de disco inválido");
        return;
    }
    
    let selected_disk = &disks[disk_index];
    println!("✅ Disco seleccionado: {}", selected_disk.name);
    println!();
    
    // 3. Confirmar instalación
    let confirm = read_input(&format!(
        "⚠️  ADVERTENCIA: Esto borrará todos los datos en {}. ¿Continuar? (s/N): ",
        selected_disk.name
    ));
    
    if confirm.trim().to_lowercase() != "s" {
        println!("❌ Instalación cancelada");
        return;
    }
    
    // 4. Iniciar instalación
    println!("🚀 Iniciando instalación...");
    println!();
    
    // Crear particiones
    let mut partition_manager = PartitionManager::new();
    match partition_manager.create_partitions(selected_disk) {
        Ok(partitions) => {
            println!("✅ Particiones creadas exitosamente");
            for partition in &partitions {
                println!("   - {}: {} ({})", partition.name, partition.mount_point, partition.filesystem);
            }
        }
        Err(e) => {
            println!("❌ Error creando particiones: {}", e);
            return;
        }
    }
    
    // Instalar bootloader
    let bootloader_installer = BootloaderInstaller::new();
    match bootloader_installer.install_uefi(selected_disk) {
        Ok(_) => println!("✅ Bootloader UEFI instalado"),
        Err(e) => {
            println!("❌ Error instalando bootloader: {}", e);
            return;
        }
    }
    
    // Configurar sistema de archivos
    let fs_manager = FilesystemManager::new();
    match fs_manager.setup_filesystem(selected_disk) {
        Ok(_) => println!("✅ Sistema de archivos configurado"),
        Err(e) => {
            println!("❌ Error configurando sistema de archivos: {}", e);
            return;
        }
    }
    
    // Instalar kernel y archivos del sistema
    match install_system_files(selected_disk) {
        Ok(_) => println!("✅ Archivos del sistema instalados"),
        Err(e) => {
            println!("❌ Error instalando archivos del sistema: {}", e);
            return;
        }
    }
    
    println!();
    println!("🎉 ¡Instalación completada exitosamente!");
    println!("========================================");
    println!();
    println!("📋 Resumen de la instalación:");
    println!("  - Disco: {}", selected_disk.name);
    println!("  - Bootloader: UEFI");
    println!("  - Sistema de archivos: FAT32 (EFI) + EXT4 (root)");
    println!("  - Kernel: Eclipse OS v1.0");
    println!();
    println!("🔄 Reinicia el sistema para usar Eclipse OS");
}

fn show_disk_info() {
    println!("💾 Información de Discos");
    println!("========================");
    println!();
    
    let mut disk_manager = DiskManager::new();
    let disks = disk_manager.list_disks();
    
    if disks.is_empty() {
        println!("❌ No se encontraron discos");
        return;
    }
    
    for disk in disks {
        println!("📀 Disco: {}", disk.name);
        println!("   Tamaño: {}", disk.size);
        println!("   Modelo: {}", disk.model);
        println!("   Tipo: {}", disk.disk_type);
        println!();
    }
}

fn show_help() {
    println!("❓ Ayuda del Instalador");
    println!("=======================");
    println!();
    println!("Este instalador te permite instalar Eclipse OS en tu disco duro.");
    println!();
    println!("📋 Requisitos:");
    println!("  - Disco duro con al menos 1GB de espacio libre");
    println!("  - Sistema UEFI compatible");
    println!("  - Conexión a internet (para descargar dependencias)");
    println!();
    println!("⚠️  Advertencias:");
    println!("  - La instalación borrará todos los datos del disco seleccionado");
    println!("  - Haz una copia de seguridad de tus datos importantes");
    println!("  - Asegúrate de seleccionar el disco correcto");
    println!();
    println!("🔧 Proceso de instalación:");
    println!("  1. Selección del disco de destino");
    println!("  2. Creación de particiones (EFI + Root)");
    println!("  3. Instalación del bootloader UEFI");
    println!("  4. Configuración del sistema de archivos");
    println!("  5. Instalación de archivos del sistema");
    println!();
}

fn install_system_files(disk: &DiskInfo) -> Result<(), String> {
    println!("📦 Instalando archivos del sistema...");
    
    // Montar partición EFI
    let efi_mount = "/mnt/eclipse-efi";
    if !Path::new(efi_mount).exists() {
        fs::create_dir_all(efi_mount).map_err(|e| format!("Error creando directorio EFI: {}", e))?;
    }
    
    // Montar partición root
    let root_mount = "/mnt/eclipse-root";
    if !Path::new(root_mount).exists() {
        fs::create_dir_all(root_mount).map_err(|e| format!("Error creando directorio root: {}", e))?;
    }
    
    // Copiar kernel
    let kernel_source = "target_hardware/x86_64-unknown-none/release/eclipse_kernel";
    let kernel_dest = format!("{}/eclipse_kernel", efi_mount);
    
    if Path::new(kernel_source).exists() {
        fs::copy(kernel_source, &kernel_dest)
            .map_err(|e| format!("Error copiando kernel: {}", e))?;
        println!("   ✅ Kernel copiado");
    } else {
        return Err("Kernel no encontrado. Ejecuta 'cargo build --release' primero.".to_string());
    }
    
    // Copiar bootloader
    let bootloader_source = "bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader-main.efi";
    let bootloader_dest = format!("{}/EFI/BOOT/BOOTX64.EFI", efi_mount);
    
    if Path::new(bootloader_source).exists() {
        fs::create_dir_all(format!("{}/EFI/BOOT", efi_mount))
            .map_err(|e| format!("Error creando directorio EFI/BOOT: {}", e))?;
        
        fs::copy(bootloader_source, &bootloader_dest)
            .map_err(|e| format!("Error copiando bootloader: {}", e))?;
        println!("   ✅ Bootloader copiado");
    } else {
        return Err("Bootloader no encontrado. Ejecuta 'cd bootloader-uefi && ./build.sh' primero.".to_string());
    }
    
    // Crear archivos de configuración
    create_config_files(efi_mount)?;
    
    Ok(())
}

fn create_config_files(efi_mount: &str) -> Result<(), String> {
    // Crear README
    let readme_content = r#"🌙 Eclipse OS - Sistema Operativo en Rust
=========================================

Versión: 1.0
Arquitectura: x86_64
Tipo: Instalación en disco

Características:
- Kernel microkernel en Rust
- Bootloader UEFI personalizado
- Sistema de archivos optimizado
- Interfaz gráfica moderna

Desarrollado con ❤️ en Rust
"#;
    
    fs::write(format!("{}/README.txt", efi_mount), readme_content)
        .map_err(|e| format!("Error creando README: {}", e))?;
    
    // Crear archivo de configuración del bootloader
    let boot_config = r#"# Eclipse OS Boot Configuration
# =============================

KERNEL_PATH=/eclipse_kernel
INITRD_PATH=
BOOT_ARGS=quiet splash
TIMEOUT=5
DEFAULT_ENTRY=eclipse

[entry:eclipse]
title=Eclipse OS
kernel=/eclipse_kernel
args=quiet splash
"#;
    
    fs::write(format!("{}/boot.conf", efi_mount), boot_config)
        .map_err(|e| format!("Error creando configuración de boot: {}", e))?;
    
    println!("   ✅ Archivos de configuración creados");
    Ok(())
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
