//! Funciones para listar y obtener información de discos sin usar lsblk

use std::fs;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::collections::HashMap;

/// Información de un disco
#[derive(Debug, Clone)]
pub struct DiskInfo {
    pub name: String,
    pub size_bytes: u64,
    pub is_removable: bool,
    pub model: Option<String>,
    pub vendor: Option<String>,
    pub serial: Option<String>,
}

/// Información de una partición
#[derive(Debug, Clone)]
pub struct PartitionInfo {
    pub name: String,
    pub disk_name: String,
    pub size_bytes: u64,
    pub start_sector: u64,
    pub partition_number: u32,
}

/// Leer /proc/partitions para obtener lista de discos y particiones
pub fn read_proc_partitions() -> io::Result<Vec<PartitionInfo>> {
    let content = fs::read_to_string("/proc/partitions")?;
    let mut partitions = Vec::new();
    
    for line in content.lines().skip(2) { // Saltar cabecera
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 4 {
            let major = parts[0].parse::<u32>().ok();
            let minor = parts[1].parse::<u32>().ok();
            let blocks = parts[2].parse::<u64>().ok();
            let name = parts[3];
            
            if let (Some(_major), Some(_minor), Some(blocks)) = (major, minor, blocks) {
                // Determinar si es disco o partición
                let is_partition = name.chars().last().map_or(false, |c| c.is_numeric());
                
                if is_partition {
                    // Extraer nombre del disco y número de partición
                    let disk_name: String = name.chars()
                        .take_while(|c| !c.is_numeric())
                        .collect();
                    let partition_num: String = name.chars()
                        .skip_while(|c| !c.is_numeric())
                        .collect();
                    
                    partitions.push(PartitionInfo {
                        name: name.to_string(),
                        disk_name,
                        size_bytes: blocks * 1024, // blocks son de 1KB
                        start_sector: 0, // Se debe leer de /sys/block
                        partition_number: partition_num.parse().unwrap_or(0),
                    });
                }
            }
        }
    }
    
    Ok(partitions)
}

/// Leer información de un disco desde /sys/block
pub fn read_disk_info(disk_name: &str) -> io::Result<DiskInfo> {
    let sys_path = PathBuf::from(format!("/sys/block/{}", disk_name));
    
    // Leer tamaño
    let size_path = sys_path.join("size");
    let size_sectors = fs::read_to_string(&size_path)?
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
    let size_bytes = size_sectors * 512; // Sectores de 512 bytes
    
    // Leer si es removible
    let removable_path = sys_path.join("removable");
    let is_removable = fs::read_to_string(&removable_path)?
        .trim()
        .parse::<u32>()
        .unwrap_or(0) == 1;
    
    // Leer modelo
    let model = read_sys_file(&sys_path.join("device/model"));
    
    // Leer vendor
    let vendor = read_sys_file(&sys_path.join("device/vendor"));
    
    // Leer serial (si existe)
    let serial = read_sys_file(&sys_path.join("device/serial"));
    
    Ok(DiskInfo {
        name: disk_name.to_string(),
        size_bytes,
        is_removable,
        model,
        vendor,
        serial,
    })
}

/// Helper para leer archivos de /sys y manejar errores
pub fn read_sys_file(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Listar todos los discos del sistema
pub fn list_disks() -> io::Result<Vec<DiskInfo>> {
    let sys_block = PathBuf::from("/sys/block");
    let mut disks = Vec::new();
    
    for entry in fs::read_dir(&sys_block)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        
        // Filtrar solo discos reales (sd*, nvme*, vd*, hd*)
        if name.starts_with("sd") || 
           name.starts_with("nvme") || 
           name.starts_with("vd") || 
           name.starts_with("hd") {
            if let Ok(info) = read_disk_info(&name) {
                disks.push(info);
            }
        }
    }
    
    Ok(disks)
}

/// Obtener información completa de particiones con start sector
pub fn get_partition_info(partition_name: &str) -> io::Result<PartitionInfo> {
    // Extraer nombre del disco
    let disk_name: String = partition_name.chars()
        .take_while(|c| !c.is_numeric())
        .collect();
    
    let partition_num: String = partition_name.chars()
        .skip_while(|c| !c.is_numeric())
        .collect();
    
    // Leer tamaño desde /sys/block/{disk}/{partition}/size
    let size_path = PathBuf::from(format!("/sys/block/{}/{}/size", disk_name, partition_name));
    let size_sectors = fs::read_to_string(&size_path)?
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
    
    // Leer sector de inicio desde /sys/block/{disk}/{partition}/start
    let start_path = PathBuf::from(format!("/sys/block/{}/{}/start", disk_name, partition_name));
    let start_sector = fs::read_to_string(&start_path)?
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
    
    Ok(PartitionInfo {
        name: partition_name.to_string(),
        disk_name,
        size_bytes: size_sectors * 512,
        start_sector,
        partition_number: partition_num.parse().unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_list_disks_compiles() {
        // Solo verificar que compila
        let _ = list_disks();
    }
    
    #[test]
    fn test_read_proc_partitions_compiles() {
        let _ = read_proc_partitions();
    }
}

