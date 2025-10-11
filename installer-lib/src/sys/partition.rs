//! Funciones nativas para particionado (sin usar parted)

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write, Seek, SeekFrom};
use std::path::Path;
use super::disk::{DiskInfo, read_sys_file};

/// Tabla de particiones GPT
pub struct GptTable {
    pub disk_size_bytes: u64,
    pub partitions: Vec<GptPartitionEntry>,
}

/// Entrada de partición GPT
#[derive(Debug, Clone)]
pub struct GptPartitionEntry {
    pub partition_number: u32,
    pub start_lba: u64,
    pub end_lba: u64,
    pub size_bytes: u64,
    pub partition_type_guid: [u8; 16],
    pub partition_guid: [u8; 16],
    pub name: String,
}

impl GptTable {
    /// Crear tabla GPT vacía
    pub fn new(disk_size_bytes: u64) -> Self {
        Self {
            disk_size_bytes,
            partitions: Vec::new(),
        }
    }
    
    /// Añadir partición
    pub fn add_partition(
        &mut self,
        start_lba: u64,
        size_bytes: u64,
        partition_type_guid: [u8; 16],
        name: &str,
    ) -> Result<(), &'static str> {
        let end_lba = start_lba + (size_bytes / 512) - 1;
        
        let partition_number = (self.partitions.len() + 1) as u32;
        
        self.partitions.push(GptPartitionEntry {
            partition_number,
            start_lba,
            end_lba,
            size_bytes,
            partition_type_guid,
            partition_guid: generate_random_guid(),
            name: name.to_string(),
        });
        
        Ok(())
    }
    
    /// Escribir tabla GPT al disco
    pub fn write_to_disk<P: AsRef<Path>>(&self, disk_path: P) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(disk_path)?;
        
        // Escribir protective MBR (sector 0)
        file.seek(SeekFrom::Start(0))?;
        let mbr = self.create_protective_mbr();
        file.write_all(&mbr)?;
        
        // Escribir GPT header (sector 1)
        file.seek(SeekFrom::Start(512))?;
        let header = self.create_gpt_header();
        file.write_all(&header)?;
        
        // Escribir partition array (sectores 2-33)
        file.seek(SeekFrom::Start(1024))?;
        let partition_array = self.create_partition_array();
        file.write_all(&partition_array)?;
        
        file.sync_all()?;
        
        Ok(())
    }
    
    fn create_protective_mbr(&self) -> Vec<u8> {
        let mut mbr = vec![0u8; 512];
        
        // Boot signature
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        
        // Protective MBR partition entry (offset 446)
        mbr[446] = 0x00; // Status (not bootable)
        mbr[447] = 0x00; // CHS start (head)
        mbr[448] = 0x02; // CHS start (sector)
        mbr[449] = 0x00; // CHS start (cylinder)
        mbr[450] = 0xEE; // Partition type (GPT protective)
        mbr[451] = 0xFF; // CHS end
        mbr[452] = 0xFF;
        mbr[453] = 0xFF;
        // LBA start (1)
        mbr[454] = 0x01;
        mbr[455] = 0x00;
        mbr[456] = 0x00;
        mbr[457] = 0x00;
        // LBA size (disk size - 1)
        let disk_sectors = (self.disk_size_bytes / 512) as u32;
        mbr[458..462].copy_from_slice(&disk_sectors.to_le_bytes());
        
        mbr
    }
    
    fn create_gpt_header(&self) -> Vec<u8> {
        let mut header = vec![0u8; 512];
        
        // Signature "EFI PART"
        header[0..8].copy_from_slice(b"EFI PART");
        
        // Revision (1.0)
        header[8..12].copy_from_slice(&0x00010000u32.to_le_bytes());
        
        // Header size (92 bytes)
        header[12..16].copy_from_slice(&92u32.to_le_bytes());
        
        // CRC32 (se calcularía aquí, por ahora 0)
        header[16..20].copy_from_slice(&0u32.to_le_bytes());
        
        // Reserved
        header[20..24].copy_from_slice(&0u32.to_le_bytes());
        
        // Current LBA (1)
        header[24..32].copy_from_slice(&1u64.to_le_bytes());
        
        // Backup LBA (último sector)
        let backup_lba = (self.disk_size_bytes / 512) - 1;
        header[32..40].copy_from_slice(&backup_lba.to_le_bytes());
        
        // First usable LBA (34)
        header[40..48].copy_from_slice(&34u64.to_le_bytes());
        
        // Last usable LBA
        let last_usable = backup_lba - 33;
        header[48..56].copy_from_slice(&last_usable.to_le_bytes());
        
        // Disk GUID (random)
        let disk_guid = generate_random_guid();
        header[56..72].copy_from_slice(&disk_guid);
        
        // Partition array LBA (2)
        header[72..80].copy_from_slice(&2u64.to_le_bytes());
        
        // Number of partition entries (128)
        header[80..84].copy_from_slice(&128u32.to_le_bytes());
        
        // Size of partition entry (128 bytes)
        header[84..88].copy_from_slice(&128u32.to_le_bytes());
        
        // CRC32 of partition array (se calcularía aquí)
        header[88..92].copy_from_slice(&0u32.to_le_bytes());
        
        header
    }
    
    fn create_partition_array(&self) -> Vec<u8> {
        let mut array = vec![0u8; 128 * 128]; // 128 entradas de 128 bytes
        
        for (i, partition) in self.partitions.iter().enumerate() {
            let offset = i * 128;
            let entry = &mut array[offset..offset + 128];
            
            // Partition type GUID
            entry[0..16].copy_from_slice(&partition.partition_type_guid);
            
            // Partition GUID
            entry[16..32].copy_from_slice(&partition.partition_guid);
            
            // Start LBA
            entry[32..40].copy_from_slice(&partition.start_lba.to_le_bytes());
            
            // End LBA
            entry[40..48].copy_from_slice(&partition.end_lba.to_le_bytes());
            
            // Attributes (0)
            entry[48..56].copy_from_slice(&0u64.to_le_bytes());
            
            // Partition name (UTF-16LE, max 36 chars)
            let name_bytes = string_to_utf16le(&partition.name, 36);
            entry[56..56 + name_bytes.len()].copy_from_slice(&name_bytes);
        }
        
        array
    }
}

/// Listar todos los discos disponibles
pub fn list_all_disks() -> io::Result<Vec<DiskInfo>> {
    let mut disks = Vec::new();
    let sys_block = Path::new("/sys/block");
    
    for entry in fs::read_dir(sys_block)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        
        // Filtrar solo discos (sd*, nvme*, vd*, hd*)
        if name.starts_with("sd") || 
           name.starts_with("nvme") || 
           name.starts_with("vd") || 
           name.starts_with("hd") {
            // Leer información del disco desde /sys/block/{name}
            let disk_path = sys_block.join(&name);
            
            // Leer tamaño
            let size_path = disk_path.join("size");
            let size_sectors = fs::read_to_string(&size_path)
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);
            
            // Leer si es removible
            let removable_path = disk_path.join("removable");
            let is_removable = fs::read_to_string(&removable_path)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(0) == 1;
            
            // Leer modelo
            let model = read_sys_file(&disk_path.join("device/model"));
            
            // Leer vendor
            let vendor = read_sys_file(&disk_path.join("device/vendor"));
            
            disks.push(DiskInfo {
                name,
                size_bytes: size_sectors * 512,
                is_removable,
                model,
                vendor,
                serial: None,
            });
        }
    }
    
    Ok(disks)
}

/// Generar GUID aleatorio (simplificado)
fn generate_random_guid() -> [u8; 16] {
    use std::time::{SystemTime, UNIX_EPOCH};
    
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    
    let mut guid = [0u8; 16];
    guid[0..8].copy_from_slice(&timestamp.to_le_bytes());
    guid[8..16].copy_from_slice(&(!timestamp).to_le_bytes());
    
    guid
}

/// Convertir string a UTF-16LE
fn string_to_utf16le(s: &str, max_chars: usize) -> Vec<u8> {
    let utf16: Vec<u16> = s.chars()
        .take(max_chars)
        .map(|c| c as u16)
        .collect();
    
    let mut bytes = Vec::with_capacity(utf16.len() * 2);
    for code_unit in utf16 {
        bytes.extend_from_slice(&code_unit.to_le_bytes());
    }
    
    bytes
}

// GUIDs de tipo de partición comunes
pub const EFI_SYSTEM_PARTITION_GUID: [u8; 16] = [
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11,
    0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];

pub const LINUX_FILESYSTEM_GUID: [u8; 16] = [
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47,
    0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
];

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_gpt_table_creation() {
        let mut gpt = GptTable::new(1024 * 1024 * 1024); // 1GB
        gpt.add_partition(2048, 100 * 1024 * 1024, EFI_SYSTEM_PARTITION_GUID, "EFI").unwrap();
        assert_eq!(gpt.partitions.len(), 1);
    }
}

