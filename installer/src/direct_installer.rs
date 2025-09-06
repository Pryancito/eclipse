use std::fs;
use std::path::Path;
use std::io::{self, Write};
use crate::DiskInfo;
use crate::uefi_config::UefiConfigManager;

pub struct DirectInstaller {
    efi_mount_point: String,
    root_mount_point: String,
}

impl DirectInstaller {
    pub fn new() -> Self {
        Self {
            efi_mount_point: "/mnt/eclipse-efi".to_string(),
            root_mount_point: "/mnt/eclipse-root".to_string(),
        }
    }

    pub fn install_eclipse_os(&self, disk: &DiskInfo, auto_install: bool) -> Result<(), String> {
        println!("Instalador de Eclipse OS v0.5.0");
        println!("================================");
        println!();

        // Verificar disco
        self.verify_disk(disk)?;

        // Mostrar información del disco
        println!("Disco seleccionado: {}", disk.name);
        println!();

        // Confirmar instalación (si no es automática)
        if !auto_install {
            println!("ADVERTENCIA: Esto borrara TODOS los datos en {}", disk.name);
            print!("Estas seguro de que quieres continuar? (escribe 'SI' para confirmar): ");
            io::stdout().flush().map_err(|e| format!("Error escribiendo: {}", e))?;
            
            let mut input = String::new();
            io::stdin().read_line(&mut input).map_err(|e| format!("Error leyendo entrada: {}", e))?;
            
            if input.trim() != "SI" {
                return Err("Instalacion cancelada".to_string());
            }
        }

        println!("Iniciando instalacion de Eclipse OS...");
        println!("=====================================");
        println!();

        // Crear particiones
        self.create_partitions(disk)?;

        // Formatear particiones
        self.format_partitions(disk)?;

        // Instalar bootloader
        self.install_bootloader(disk)?;

        // Instalar userland
        self.install_userland(disk)?;

        // Crear archivos de configuración
        self.create_config_files(disk)?;

        println!();
        println!("Instalacion completada exitosamente!");
        println!("===================================");
        println!();
        println!("Resumen de la instalacion:");
        println!("  - Disco: {}", disk.name);
        println!("  - Particion EFI: {}1 (FAT32)", disk.name);
        println!("  - Particion root: {}2 (EXT4)", disk.name);
        println!("  - Bootloader: UEFI");
        println!("  - Kernel: Eclipse OS v0.5.0");
        println!("  - Userland: Modulos compilados e instalados");
        println!();
        println!("Reinicia el sistema para usar Eclipse OS");
        println!();
        println!("Consejos:");
        println!("  - Asegurate de que UEFI este habilitado en tu BIOS");
        println!("  - Selecciona el disco como dispositivo de arranque");
        println!("  - Si no arranca, verifica la configuracion UEFI");

        Ok(())
    }

    fn verify_disk(&self, disk: &DiskInfo) -> Result<(), String> {
        if !Path::new(&disk.name).exists() {
            return Err(format!("{} no es un dispositivo de bloque valido", disk.name));
        }

        // Verificar que no esté montado
        let mount_output = std::process::Command::new("mount")
            .output()
            .map_err(|e| format!("Error ejecutando mount: {}", e))?;
        
        let mount_str = String::from_utf8_lossy(&mount_output.stdout);
        if mount_str.contains(&disk.name) {
            return Err(format!("{} tiene particiones montadas. Desmonta las particiones antes de continuar", disk.name));
        }

        Ok(())
    }

    fn create_partitions(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("Creando particiones en {}...", disk.name);

        // Limpiar tabla de particiones
        println!("   Limpiando tabla de particiones...");
        let _ = std::process::Command::new("wipefs")
            .args(&["-a", &disk.name])
            .output();

        // Crear tabla GPT
        println!("   Creando tabla de particiones GPT...");
        let output = std::process::Command::new("parted")
            .args(&[&disk.name, "mklabel", "gpt"])
            .output()
            .map_err(|e| format!("Error ejecutando parted: {}", e))?;

        if !output.status.success() {
            return Err(format!("No se pudo crear tabla GPT en {}: {}", disk.name, String::from_utf8_lossy(&output.stderr)));
        }

        // Crear partición EFI (100MB)
        println!("   Creando particion EFI (100MB)...");
        let output = std::process::Command::new("parted")
            .args(&[&disk.name, "mkpart", "EFI", "fat32", "1MiB", "101MiB"])
            .output()
            .map_err(|e| format!("Error ejecutando parted: {}", e))?;

        if !output.status.success() {
            return Err(format!("No se pudo crear particion EFI: {}", String::from_utf8_lossy(&output.stderr)));
        }

        // Marcar partición EFI como ESP
        let output = std::process::Command::new("parted")
            .args(&[&disk.name, "set", "1", "esp", "on"])
            .output()
            .map_err(|e| format!("Error ejecutando parted: {}", e))?;

        if !output.status.success() {
            return Err(format!("No se pudo marcar particion EFI como ESP: {}", String::from_utf8_lossy(&output.stderr)));
        }

        // Crear partición root (resto del disco)
        println!("   Creando particion root (resto del disco)...");
        let output = std::process::Command::new("parted")
            .args(&[&disk.name, "mkpart", "ROOT", "ext4", "101MiB", "100%"])
            .output()
            .map_err(|e| format!("Error ejecutando parted: {}", e))?;

        if !output.status.success() {
            return Err(format!("No se pudo crear particion root: {}", String::from_utf8_lossy(&output.stderr)));
        }

        // Sincronizar cambios
        println!("   Sincronizando cambios...");
        let _ = std::process::Command::new("sync").output();
        let _ = std::process::Command::new("partprobe").arg(&disk.name).output();

        // Verificar que las particiones existen
        std::thread::sleep(std::time::Duration::from_secs(2));
        
        let part1 = format!("{}1", disk.name);
        let part2 = format!("{}2", disk.name);

        if !Path::new(&part1).exists() || !Path::new(&part2).exists() {
            return Err("Las particiones no se crearon correctamente".to_string());
        }

        println!("Particiones creadas exitosamente");
        Ok(())
    }

    fn format_partitions(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("Formateando particiones...");

        let efi_partition = format!("{}1", disk.name);
        let root_partition = format!("{}2", disk.name);

        // Formatear partición EFI
        println!("   Formateando particion EFI como FAT32...");
        let output = std::process::Command::new("mkfs.fat")
            .args(&["-F32", "-n", "ECLIPSE_EFI", &efi_partition])
            .output()
            .map_err(|e| format!("Error ejecutando mkfs.fat: {}", e))?;

        if !output.status.success() {
            return Err(format!("No se pudo formatear particion EFI: {}", String::from_utf8_lossy(&output.stderr)));
        }

        // Formatear partición root
        println!("   Formateando particion root como EXT4...");
        let output = std::process::Command::new("mkfs.ext4")
            .args(&["-F", "-L", "ECLIPSE_ROOT", &root_partition])
            .output()
            .map_err(|e| format!("Error ejecutando mkfs.ext4: {}", e))?;

        if !output.status.success() {
            return Err(format!("No se pudo formatear particion root: {}", String::from_utf8_lossy(&output.stderr)));
        }

        println!("Particiones formateadas exitosamente");
        Ok(())
    }

    fn install_bootloader(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("Instalando bootloader UEFI...");

        let efi_partition = format!("{}1", disk.name);

        // Crear directorios de montaje
        fs::create_dir_all(&self.efi_mount_point)
            .map_err(|e| format!("Error creando directorio EFI: {}", e))?;

        // Montar partición EFI
        println!("   Montando particion EFI...");
        let output = std::process::Command::new("mount")
            .args(&[&efi_partition, &self.efi_mount_point])
            .output()
            .map_err(|e| format!("Error ejecutando mount: {}", e))?;

        if !output.status.success() {
            return Err(format!("No se pudo montar particion EFI: {}", String::from_utf8_lossy(&output.stderr)));
        }

        // Crear estructura EFI
        println!("   Creando estructura EFI...");
        fs::create_dir_all(format!("{}/EFI/BOOT", self.efi_mount_point))
            .map_err(|e| format!("Error creando directorio EFI/BOOT: {}", e))?;
        fs::create_dir_all(format!("{}/EFI/eclipse", self.efi_mount_point))
            .map_err(|e| format!("Error creando directorio EFI/eclipse: {}", e))?;

        // Copiar bootloader
        println!("   Instalando bootloader...");
        let bootloader_source = "../bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader.efi";
        
        if !Path::new(bootloader_source).exists() {
            return Err("Bootloader no encontrado. Ejecuta 'cd bootloader-uefi && ./build.sh' primero".to_string());
        }

        fs::copy(bootloader_source, format!("{}/EFI/BOOT/BOOTX64.EFI", self.efi_mount_point))
            .map_err(|e| format!("Error copiando bootloader a EFI/BOOT/: {}", e))?;
        
        fs::copy(bootloader_source, format!("{}/EFI/eclipse/eclipse-bootloader.efi", self.efi_mount_point))
            .map_err(|e| format!("Error copiando bootloader a EFI/eclipse/: {}", e))?;

        // Copiar kernel
        println!("   Instalando kernel...");
        let kernel_source = "../eclipse_kernel/target/x86_64-unknown-none/release/eclipse_kernel";
        
        if !Path::new(kernel_source).exists() {
            return Err("Kernel no encontrado. Ejecuta 'cd eclipse_kernel && cargo build --release' primero".to_string());
        }

        fs::copy(kernel_source, format!("{}/eclipse_kernel", self.efi_mount_point))
            .map_err(|e| format!("Error copiando kernel: {}", e))?;

        Ok(())
    }

    fn install_userland(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("Instalando modulos userland...");

        // Crear directorio para userland
        fs::create_dir_all(format!("{}/userland/bin", self.efi_mount_point))
            .map_err(|e| format!("Error creando directorio userland/bin: {}", e))?;
        fs::create_dir_all(format!("{}/userland/lib", self.efi_mount_point))
            .map_err(|e| format!("Error creando directorio userland/lib: {}", e))?;
        fs::create_dir_all(format!("{}/userland/config", self.efi_mount_point))
            .map_err(|e| format!("Error creando directorio userland/config: {}", e))?;

        // Copiar binarios userland
        let userland_modules = vec![
            ("../userland/module_loader/target/release/module_loader", "module_loader"),
            ("../userland/graphics_module/target/release/graphics_module", "graphics_module"),
            ("../userland/app_framework/target/release/app_framework", "app_framework"),
            ("../userland/target/release/eclipse-userland", "eclipse-userland"),
        ];

        for (source, name) in userland_modules {
            if Path::new(source).exists() {
                fs::copy(source, format!("{}/userland/bin/{}", self.efi_mount_point, name))
                    .map_err(|e| format!("Error copiando {}: {}", name, e))?;
                println!("     {} instalado", name);
            }
        }

        // Crear configuración de userland
        let config_content = r#"# Eclipse OS Userland Configuration v0.5.0
# =========================================

[system]
name = "Eclipse OS"
version = "0.5.0"
kernel = "/eclipse_kernel"

[modules]
module_loader = "/userland/bin/module_loader"
graphics_module = "/userland/bin/graphics_module"
app_framework = "/userland/bin/app_framework"
eclipse_userland = "/userland/bin/eclipse-userland"

[ipc]
socket_path = "/tmp/eclipse_ipc.sock"
timeout = 5000

[graphics]
graphics_mode = "1920x1080x32"
vga_fallback = true

[memory]
kernel_memory = "64M"
userland_memory = "256M"
"#;

        fs::write(format!("{}/userland/config/system.conf", self.efi_mount_point), config_content)
            .map_err(|e| format!("Error creando configuracion de userland: {}", e))?;

        println!("     Configuracion de userland creada");
        println!("   Modulos userland instalados");

        Ok(())
    }

    fn create_config_files(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("Creando archivos de configuracion...");

        // Crear configuración UEFI personalizada
        let uefi_config = UefiConfigManager::new();
        uefi_config.create_uefi_config(&self.efi_mount_point)?;
        uefi_config.create_boot_entries(&self.efi_mount_point)?;
        uefi_config.create_module_config(&self.efi_mount_point)?;
        uefi_config.create_system_info(&self.efi_mount_point)?;

        // Configuración del bootloader (compatibilidad)
        let boot_conf = r#"# Eclipse OS Boot Configuration v0.5.0
# ===================================

TIMEOUT=5
DEFAULT_ENTRY=eclipse
SHOW_MENU=true

[entry:eclipse]
title=Eclipse OS
description=Sistema Operativo Eclipse v0.5.0
kernel=/eclipse_kernel
initrd=
args=quiet splash
"#;

        fs::write(format!("{}/boot.conf", self.efi_mount_point), boot_conf)
            .map_err(|e| format!("Error creando boot.conf: {}", e))?;

        // README
        let readme_content = r#"Eclipse OS - Sistema Operativo en Rust
=====================================

Version: 0.5.0
Arquitectura: x86_64
Tipo: Instalacion en disco

Caracteristicas:
- Kernel microkernel en Rust
- Bootloader UEFI personalizado
- Sistema de archivos optimizado
- Interfaz grafica moderna

Desarrollado con amor en Rust
"#;

        fs::write(format!("{}/README.txt", self.efi_mount_point), readme_content)
            .map_err(|e| format!("Error creando README.txt: {}", e))?;

        // Desmontar partición EFI
        let output = std::process::Command::new("umount")
            .arg(&self.efi_mount_point)
            .output()
            .map_err(|e| format!("Error ejecutando umount: {}", e))?;

        if !output.status.success() {
            eprintln!("Advertencia: Error desmontando particion EFI: {}", String::from_utf8_lossy(&output.stderr));
        }

        // Limpiar directorio de montaje
        let _ = fs::remove_dir(&self.efi_mount_point);

        println!("Configuracion UEFI instalada exitosamente");
        Ok(())
    }

    pub fn show_disks(&self) -> Result<(), String> {
        println!("Discos disponibles:");
        println!("==================");
        
        let output = std::process::Command::new("lsblk")
            .args(&["-d", "-o", "NAME,SIZE,MODEL,TYPE"])
            .output()
            .map_err(|e| format!("Error ejecutando lsblk: {}", e))?;

        if !output.status.success() {
            return Err(format!("Error listando discos: {}", String::from_utf8_lossy(&output.stderr)));
        }

        let output_str = String::from_utf8_lossy(&output.stdout);
        let mut count = 1;
        
        for line in output_str.lines() {
            if line.contains("disk") {
                println!("  {}. {}", count, line);
                count += 1;
            }
        }
        
        println!();
        Ok(())
    }
}



