use std::process::Command;
use std::fs;
use std::path::Path;
use crate::DiskInfo;

pub struct BootloaderInstaller {
    efi_mount_point: String,
}

impl BootloaderInstaller {
    pub fn new() -> Self {
        Self {
            efi_mount_point: "/mnt/eclipse-efi".to_string(),
        }
    }
    
    pub fn install_uefi(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("🔧 Instalando bootloader UEFI...");
        
        // 1. Montar partición EFI
        self.mount_efi_partition(disk)?;
        
        // 2. Crear estructura de directorios EFI
        self.create_efi_structure()?;
        
        // 3. Instalar bootloader
        self.install_bootloader_files()?;
        
        // 4. Configurar UEFI
        self.configure_uefi(disk)?;
        
        // 5. Desmontar partición EFI
        self.unmount_efi_partition()?;
        
        println!("✅ Bootloader UEFI instalado exitosamente");
        Ok(())
    }
    
    fn mount_efi_partition(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("   📁 Montando partición EFI...");
        
        let efi_partition = format!("{}1", disk.name);
        
        // Crear directorio de montaje
        if !Path::new(&self.efi_mount_point).exists() {
            fs::create_dir_all(&self.efi_mount_point)
                .map_err(|e| format!("Error creando directorio de montaje: {}", e))?;
        }
        
        // Montar partición
        let output = Command::new("mount")
            .args(&[&efi_partition, &self.efi_mount_point])
            .output();
            
        match output {
            Ok(result) => {
                if result.status.success() {
                    Ok(())
                } else {
                    Err(format!("Error montando partición EFI: {}", String::from_utf8_lossy(&result.stderr)))
                }
            }
            Err(e) => Err(format!("Error ejecutando mount: {}", e))
        }
    }
    
    fn create_efi_structure(&self) -> Result<(), String> {
        println!("   📂 Creando estructura de directorios EFI...");
        
        let efi_boot_dir = format!("{}/EFI/BOOT", self.efi_mount_point);
        let efi_eclipse_dir = format!("{}/EFI/eclipse", self.efi_mount_point);
        
        // Crear directorios
        fs::create_dir_all(&efi_boot_dir)
            .map_err(|e| format!("Error creando EFI/BOOT: {}", e))?;
            
        fs::create_dir_all(&efi_eclipse_dir)
            .map_err(|e| format!("Error creando EFI/eclipse: {}", e))?;
        
        Ok(())
    }
    
    fn install_bootloader_files(&self) -> Result<(), String> {
        println!("   📦 Instalando archivos del bootloader...");
        
        let bootloader_source = "bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader.efi";
        let bootloader_dest = format!("{}/EFI/BOOT/BOOTX64.EFI", self.efi_mount_point);
        
        if !Path::new(bootloader_source).exists() {
            return Err("Bootloader no encontrado. Ejecuta 'cd bootloader-uefi && ./build.sh' primero.".to_string());
        }
        
        fs::copy(bootloader_source, &bootloader_dest)
            .map_err(|e| format!("Error copiando bootloader: {}", e))?;
        
        // También copiar a directorio específico de Eclipse
        let eclipse_bootloader = format!("{}/EFI/eclipse/eclipse-bootloader.efi", self.efi_mount_point);
        fs::copy(bootloader_source, &eclipse_bootloader)
            .map_err(|e| format!("Error copiando bootloader a directorio Eclipse: {}", e))?;
        
        Ok(())
    }
    
    fn configure_uefi(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("   ⚙️  Configurando UEFI...");
        
        // Crear archivo de configuración del bootloader
        self.create_bootloader_config()?;
        
        // Crear entrada de menú UEFI
        self.create_uefi_menu_entry(disk)?;
        
        // Instalar kernel
        self.install_kernel()?;
        
        Ok(())
    }
    
    fn create_bootloader_config(&self) -> Result<(), String> {
        let config_content = r#"# Eclipse OS Boot Configuration
# =============================

# Configuración del bootloader
TIMEOUT=5
DEFAULT_ENTRY=eclipse
SHOW_MENU=true

# Entrada principal de Eclipse OS
[entry:eclipse]
title=Eclipse OS
description=Sistema Operativo Eclipse v1.0
kernel=/eclipse_kernel
initrd=
args=quiet splash
"#;
        
        let config_path = format!("{}/boot.conf", self.efi_mount_point);
        fs::write(&config_path, config_content)
            .map_err(|e| format!("Error creando configuración del bootloader: {}", e))?;
        
        Ok(())
    }
    
    fn create_uefi_menu_entry(&self, disk: &DiskInfo) -> Result<(), String> {
        let menu_entry = format!(r#"# Eclipse OS UEFI Menu Entry
# ==========================

title Eclipse OS
description Sistema Operativo Eclipse v1.0
kernel /eclipse_kernel
args quiet splash
"#);
        
        let menu_path = format!("{}/EFI/eclipse/eclipse.conf", self.efi_mount_point);
        fs::write(&menu_path, menu_entry)
            .map_err(|e| format!("Error creando entrada de menú UEFI: {}", e))?;
        
        Ok(())
    }
    
    fn install_kernel(&self) -> Result<(), String> {
        println!("   🧠 Instalando kernel Eclipse...");
        
        let kernel_source = "eclipse_kernel/target/x86_64-unknown-none/release/eclipse_kernel";
        let kernel_dest = format!("{}/eclipse_kernel", self.efi_mount_point);
        
        if !Path::new(kernel_source).exists() {
            return Err("Kernel no encontrado. Ejecuta 'cargo build --release' primero.".to_string());
        }
        
        fs::copy(kernel_source, &kernel_dest)
            .map_err(|e| format!("Error copiando kernel: {}", e))?;
        
        Ok(())
    }
    
    fn unmount_efi_partition(&self) -> Result<(), String> {
        println!("   📤 Desmontando partición EFI...");
        
        let output = Command::new("umount")
            .arg(&self.efi_mount_point)
            .output();
            
        match output {
            Ok(result) => {
                if result.status.success() {
                    Ok(())
                } else {
                    Err(format!("Error desmontando partición EFI: {}", String::from_utf8_lossy(&result.stderr)))
                }
            }
            Err(e) => Err(format!("Error ejecutando umount: {}", e))
        }
    }
    
    pub fn install_grub(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("🔧 Instalando GRUB como bootloader alternativo...");
        
        // Montar partición root
        let root_mount = "/mnt/eclipse-root";
        let root_partition = format!("{}2", disk.name);
        
        // Crear directorio de montaje
        if !Path::new(root_mount).exists() {
            fs::create_dir_all(root_mount)
                .map_err(|e| format!("Error creando directorio de montaje root: {}", e))?;
        }
        
        // Montar partición root
        let output = Command::new("mount")
            .args(&[&root_partition, root_mount])
            .output();
            
        match output {
            Ok(result) => {
                if !result.status.success() {
                    return Err(format!("Error montando partición root: {}", String::from_utf8_lossy(&result.stderr)));
                }
            }
            Err(e) => return Err(format!("Error ejecutando mount: {}", e))
        }
        
        // Instalar GRUB
        let grub_output = Command::new("grub-install")
            .args(&["--target=x86_64-efi", "--efi-directory=/mnt/eclipse-efi", "--boot-directory=/mnt/eclipse-root/boot", &disk.name])
            .output();
            
        match grub_output {
            Ok(result) => {
                if result.status.success() {
                    println!("✅ GRUB instalado exitosamente");
                    Ok(())
                } else {
                    Err(format!("Error instalando GRUB: {}", String::from_utf8_lossy(&result.stderr)))
                }
            }
            Err(e) => Err(format!("Error ejecutando grub-install: {}", e))
        }
    }
}
