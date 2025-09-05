use std::process::Command;
use crate::{DiskInfo, PartitionInfo};

pub struct PartitionManager {
    partitions: Vec<PartitionInfo>,
}

impl PartitionManager {
    pub fn new() -> Self {
        Self {
            partitions: Vec::new(),
        }
    }
    
    pub fn create_partitions(&mut self, disk: &DiskInfo) -> Result<Vec<PartitionInfo>, String> {
        self.partitions.clear();
        
        println!("🔧 Creando particiones en {}...", disk.name);
        
        // 1. Limpiar tabla de particiones existente
        self.clear_partition_table(disk)?;
        
        // 2. Crear tabla de particiones GPT
        self.create_gpt_table(disk)?;
        
        // 3. Crear partición EFI (100MB)
        let efi_partition = self.create_efi_partition(disk)?;
        self.partitions.push(efi_partition);
        
        // 4. Crear partición root (resto del disco)
        let root_partition = self.create_root_partition(disk)?;
        self.partitions.push(root_partition);
        
        // 5. Aplicar cambios
        self.apply_changes(disk)?;
        
        println!("✅ Particiones creadas exitosamente");
        Ok(self.partitions.clone())
    }
    
    fn clear_partition_table(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("   🗑️  Limpiando tabla de particiones...");
        
        let output = Command::new("wipefs")
            .args(&["-a", &disk.name])
            .output();
            
        match output {
            Ok(result) => {
                if result.status.success() {
                    Ok(())
                } else {
                    // wipefs puede fallar si no hay tabla de particiones, eso está bien
                    Ok(())
                }
            }
            Err(_) => {
                // wipefs puede no estar disponible, intentar con dd
                self.clear_with_dd(disk)
            }
        }
    }
    
    fn clear_with_dd(&self, disk: &DiskInfo) -> Result<(), String> {
        let output = Command::new("dd")
            .args(&["if=/dev/zero", &format!("of={}", disk.name), "bs=1M", "count=1"])
            .output();
            
        match output {
            Ok(result) => {
                if result.status.success() {
                    Ok(())
                } else {
                    Err(format!("Error limpiando disco: {}", String::from_utf8_lossy(&result.stderr)))
                }
            }
            Err(e) => Err(format!("Error ejecutando dd: {}", e))
        }
    }
    
    fn create_gpt_table(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("   📋 Creando tabla de particiones GPT...");
        
        let output = Command::new("parted")
            .args(&[&disk.name, "mklabel", "gpt"])
            .output();
            
        match output {
            Ok(result) => {
                if result.status.success() {
                    Ok(())
                } else {
                    Err(format!("Error creando tabla GPT: {}", String::from_utf8_lossy(&result.stderr)))
                }
            }
            Err(e) => Err(format!("Error ejecutando parted: {}", e))
        }
    }
    
    fn create_efi_partition(&self, disk: &DiskInfo) -> Result<PartitionInfo, String> {
        println!("   💾 Creando partición EFI (100MB)...");
        
        let output = Command::new("parted")
            .args(&[&disk.name, "mkpart", "EFI", "fat32", "1MiB", "101MiB"])
            .output();
            
        match output {
            Ok(result) => {
                if result.status.success() {
                    // Marcar como partición EFI
                    let _ = Command::new("parted")
                        .args(&[&disk.name, "set", "1", "esp", "on"])
                        .output();
                    
                    Ok(PartitionInfo {
                        name: format!("{}1", disk.name),
                        mount_point: "/boot/efi".to_string(),
                        filesystem: "fat32".to_string(),
                        size: "100MB".to_string(),
                    })
                } else {
                    Err(format!("Error creando partición EFI: {}", String::from_utf8_lossy(&result.stderr)))
                }
            }
            Err(e) => Err(format!("Error ejecutando parted: {}", e))
        }
    }
    
    fn create_root_partition(&self, disk: &DiskInfo) -> Result<PartitionInfo, String> {
        println!("   🗂️  Creando partición root (resto del disco)...");
        
        let output = Command::new("parted")
            .args(&[&disk.name, "mkpart", "ROOT", "ext4", "101MiB", "100%"])
            .output();
            
        match output {
            Ok(result) => {
                if result.status.success() {
                    Ok(PartitionInfo {
                        name: format!("{}2", disk.name),
                        mount_point: "/".to_string(),
                        filesystem: "ext4".to_string(),
                        size: "Resto del disco".to_string(),
                    })
                } else {
                    Err(format!("Error creando partición root: {}", String::from_utf8_lossy(&result.stderr)))
                }
            }
            Err(e) => Err(format!("Error ejecutando parted: {}", e))
        }
    }
    
    fn apply_changes(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("   ⚡ Aplicando cambios...");
        
        // Sincronizar cambios
        let _ = Command::new("sync").output();
        
        // Recargar tabla de particiones
        let output = Command::new("partprobe")
            .arg(&disk.name)
            .output();
            
        match output {
            Ok(result) => {
                if result.status.success() {
                    Ok(())
                } else {
                    // partprobe puede fallar, pero los cambios ya están aplicados
                    Ok(())
                }
            }
            Err(_) => {
                // partprobe puede no estar disponible
                Ok(())
            }
        }
    }
    
    pub fn format_partitions(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("🔧 Formateando particiones...");
        
        // Formatear partición EFI
        let efi_partition = format!("{}1", disk.name);
        self.format_efi_partition(&efi_partition)?;
        
        // Formatear partición root
        let root_partition = format!("{}2", disk.name);
        self.format_root_partition(&root_partition)?;
        
        Ok(())
    }
    
    fn format_efi_partition(&self, partition: &str) -> Result<(), String> {
        println!("   💾 Formateando partición EFI como FAT32...");
        
        let output = Command::new("mkfs.fat")
            .args(&["-F32", partition])
            .output();
            
        match output {
            Ok(result) => {
                if result.status.success() {
                    Ok(())
                } else {
                    Err(format!("Error formateando EFI: {}", String::from_utf8_lossy(&result.stderr)))
                }
            }
            Err(e) => Err(format!("Error ejecutando mkfs.fat: {}", e))
        }
    }
    
    fn format_root_partition(&self, partition: &str) -> Result<(), String> {
        println!("   🗂️  Formateando partición root como EXT4...");
        
        let output = Command::new("mkfs.ext4")
            .args(&["-F", partition])
            .output();
            
        match output {
            Ok(result) => {
                if result.status.success() {
                    Ok(())
                } else {
                    Err(format!("Error formateando root: {}", String::from_utf8_lossy(&result.stderr)))
                }
            }
            Err(e) => Err(format!("Error ejecutando mkfs.ext4: {}", e))
        }
    }
}
