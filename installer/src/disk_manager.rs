use std::process::Command;
use std::fs;
use std::os::unix::fs::FileTypeExt;
use crate::DiskInfo;

pub struct DiskManager {
    disks: Vec<DiskInfo>,
}

impl DiskManager {
    pub fn new() -> Self {
        Self {
            disks: Vec::new(),
        }
    }
    
    pub fn list_disks(&mut self) -> Vec<DiskInfo> {
        self.scan_disks();
        self.disks.clone()
    }
    
    fn scan_disks(&mut self) {
        self.disks.clear();
        
        // Escanear discos usando lsblk
        let output = Command::new("lsblk")
            .args(&["-d", "-o", "NAME,SIZE,MODEL,TYPE", "-n"])
            .output();
            
        match output {
            Ok(result) => {
                let output_str = String::from_utf8_lossy(&result.stdout);
                for line in output_str.lines() {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 4 && parts[3] == "disk" {
                        let name = format!("/dev/{}", parts[0]);
                        let size = parts[1].to_string();
                        let model = if parts.len() > 2 {
                            parts[2..].join(" ")
                        } else {
                            "Unknown".to_string()
                        };
                        
                        // Verificar que el disco existe y es accesible
                        if self.is_disk_accessible(&name) {
                            self.disks.push(DiskInfo {
                                name: name.clone(),
                                size,
                                model,
                                disk_type: self.get_disk_type(&name),
                            });
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("Error escaneando discos: {}", e);
            }
        }
    }
    
    fn is_disk_accessible(&self, disk_path: &str) -> bool {
        // Verificar que el disco existe y es accesible
        if let Ok(metadata) = fs::metadata(disk_path) {
            metadata.is_file() || metadata.file_type().is_block_device()
        } else {
            false
        }
    }
    
    fn get_disk_type(&self, disk_path: &str) -> String {
        // Determinar el tipo de disco
        if disk_path.contains("nvme") {
            "NVMe SSD".to_string()
        } else if disk_path.contains("sd") {
            "SATA/SCSI".to_string()
        } else if disk_path.contains("hd") {
            "IDE".to_string()
        } else {
            "Unknown".to_string()
        }
    }
    
    pub fn get_disk_info(&self, disk_path: &str) -> Option<&DiskInfo> {
        self.disks.iter().find(|disk| disk.name == disk_path)
    }
    
    pub fn is_disk_mounted(&self, disk_path: &str) -> bool {
        let output = Command::new("mount")
            .output();
            
        match output {
            Ok(result) => {
                let output_str = String::from_utf8_lossy(&result.stdout);
                output_str.contains(disk_path)
            }
            Err(_) => false
        }
    }
    
    pub fn unmount_disk(&self, disk_path: &str) -> Result<(), String> {
        let output = Command::new("umount")
            .arg(disk_path)
            .output();
            
        match output {
            Ok(result) => {
                if result.status.success() {
                    Ok(())
                } else {
                    Err(format!("Error desmontando disco: {}", String::from_utf8_lossy(&result.stderr)))
                }
            }
            Err(e) => Err(format!("Error ejecutando umount: {}", e))
        }
    }
}
