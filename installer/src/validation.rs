use std::path::Path;
use std::process::Command;
use std::fs;
use std::os::unix::fs::FileTypeExt;

pub struct SystemValidator {
    required_commands: Vec<&'static str>,
    required_files: Vec<&'static str>,
}

impl SystemValidator {
    pub fn new() -> Self {
        Self {
            required_commands: vec![
                "parted",
                "mkfs.fat", 
                "mkfs.ext4",
                "mount",
                "umount",
                "lsblk",
            ],
            required_files: vec![
                "eclipse_kernel/target/x86_64-unknown-none/release/eclipse_kernel",
                "bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader.efi",
            ],
        }
    }

    pub fn validate_system(&self) -> Result<(), String> {
        println!("Validando sistema...");
        
        // Verificar comandos requeridos
        self.validate_commands()?;
        
        // Verificar archivos requeridos
        self.validate_files()?;
        
        // Verificar permisos
        self.validate_permissions()?;
        
        println!("Sistema validado correctamente");
        Ok(())
    }

    fn validate_commands(&self) -> Result<(), String> {
        println!("   Verificando comandos del sistema...");
        
        for command in &self.required_commands {
            let output = Command::new("which")
                .arg(command)
                .output();
                
            match output {
                Ok(result) => {
                    if !result.status.success() {
                        return Err(format!("Comando requerido no encontrado: {}", command));
                    }
                }
                Err(_) => {
                    return Err(format!("Error verificando comando: {}", command));
                }
            }
        }
        
        println!("   Comandos del sistema verificados");
        Ok(())
    }

    fn validate_files(&self) -> Result<(), String> {
        println!("   Verificando archivos del sistema...");
        
        for file_path in &self.required_files {
            if !Path::new(file_path).exists() {
                return Err(format!("Archivo requerido no encontrado: {}", file_path));
            }
        }
        
        println!("   Archivos del sistema verificados");
        Ok(())
    }

    fn validate_permissions(&self) -> Result<(), String> {
        println!("   Verificando permisos...");
        
        // Verificar que tenemos permisos de root
        if unsafe { libc::getuid() != 0 } {
            return Err("Se requieren permisos de root para la instalación".to_string());
        }
        
        // Verificar que podemos escribir en /tmp
        let test_file = "/tmp/eclipse_installer_test";
        match fs::write(test_file, "test") {
            Ok(_) => {
                let _ = fs::remove_file(test_file);
                println!("   Permisos verificados");
                Ok(())
            }
            Err(e) => Err(format!("Error de permisos: {}", e))
        }
    }

    pub fn validate_disk(&self, disk_path: &str) -> Result<(), String> {
        println!("   Validando disco: {}", disk_path);
        
        // Verificar que el disco existe
        if !Path::new(disk_path).exists() {
            return Err(format!("Disco no encontrado: {}", disk_path));
        }
        
        // Verificar que es un dispositivo de bloque
        match fs::metadata(disk_path) {
            Ok(metadata) => {
                if !metadata.file_type().is_block_device() {
                    return Err(format!("No es un dispositivo de bloque: {}", disk_path));
                }
            }
            Err(e) => return Err(format!("Error accediendo al disco: {}", e))
        }
        
        // Verificar que no está montado
        let output = Command::new("mount")
            .output()
            .map_err(|e| format!("Error ejecutando mount: {}", e))?;
            
        let mount_str = String::from_utf8_lossy(&output.stdout);
        if mount_str.contains(disk_path) {
            return Err(format!("El disco {} tiene particiones montadas. Desmonta las particiones antes de continuar", disk_path));
        }
        
        println!("   Disco validado correctamente");
        Ok(())
    }

    pub fn validate_userland_modules(&self) -> Result<(), String> {
        println!("   Validando módulos userland...");
        
        let userland_modules = vec![
            "userland/module_loader/target/release/module_loader",
            "userland/graphics_module/target/release/graphics_module", 
            "userland/app_framework/target/release/app_framework",
            "userland/target/release/eclipse_userland",
        ];
        
        let mut missing_modules = Vec::new();
        
        for module in userland_modules {
            if !Path::new(module).exists() {
                missing_modules.push(module);
            }
        }
        
        if !missing_modules.is_empty() {
            println!("   Advertencia: Algunos módulos userland no están disponibles:");
            for module in missing_modules {
                println!("     - {}", module);
            }
            println!("   La instalación continuará sin estos módulos");
        } else {
            println!("   Todos los módulos userland están disponibles");
        }
        
        Ok(())
    }

    pub fn check_disk_space(&self, disk_path: &str) -> Result<(), String> {
        println!("   Verificando espacio en disco...");
        
        let output = Command::new("lsblk")
            .args(&["-b", "-d", "-o", "SIZE", "-n", disk_path])
            .output()
            .map_err(|e| format!("Error verificando espacio en disco: {}", e))?;
            
        if !output.status.success() {
            return Err("No se pudo verificar el espacio en disco".to_string());
        }
        
        let size_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let size_bytes: u64 = size_str.parse()
            .map_err(|_| "Error parseando tamaño del disco".to_string())?;
            
        // Requerir al menos 1GB (1,073,741,824 bytes)
        let min_size = 1_073_741_824;
        if size_bytes < min_size {
            return Err(format!("El disco es demasiado pequeño. Se requieren al menos 1GB, encontrados {} bytes", size_bytes));
        }
        
        let size_gb = size_bytes / 1_073_741_824;
        println!("   Espacio disponible: {}GB", size_gb);
        
        Ok(())
    }
}

// Función auxiliar para verificar si estamos en un sistema UEFI
pub fn is_uefi_system() -> bool {
    Path::new("/sys/firmware/efi").exists()
}

// Función auxiliar para verificar si el sistema tiene Secure Boot habilitado
pub fn is_secure_boot_enabled() -> bool {
    if let Ok(output) = Command::new("mokutil").args(&["--sb-state"]).output() {
        let output_str = String::from_utf8_lossy(&output.stdout);
        output_str.contains("enabled")
    } else {
        false
    }
}
