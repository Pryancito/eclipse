//! ReactOS Rust Kernel - FAT32 File System
//!
//! Implementación del sistema de archivos FAT32.

use core::arch::asm;

/// Boot Sector de FAT32
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct Fat32BootSector {
    pub jump_instruction: [u8; 3],
    pub oem_name: [u8; 8],
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub reserved_sectors: u16,
    pub number_of_fats: u8,
    pub root_entries: u16,
    pub total_sectors_16: u16,
    pub media_type: u8,
    pub sectors_per_fat_16: u16,
    pub sectors_per_track: u16,
    pub number_of_heads: u16,
    pub hidden_sectors: u32,
    pub total_sectors_32: u32,
    pub sectors_per_fat_32: u32,
    pub flags: u16,
    pub version: u16,
    pub root_cluster: u32,
    pub info_sector: u16,
    pub backup_boot_sector: u16,
    pub reserved: [u8; 12],
    pub drive_number: u8,
    pub reserved1: u8,
    pub boot_signature: u8,
    pub volume_id: u32,
    pub volume_label: [u8; 11],
    pub file_system_type: [u8; 8],
    pub boot_code: [u8; 420],
    pub boot_signature_2: u16,
}

/// Entrada de directorio FAT32
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct Fat32DirectoryEntry {
    pub name: [u8; 8],
    pub extension: [u8; 3],
    pub attributes: u8,
    pub reserved: u8,
    pub creation_time_tenths: u8,
    pub creation_time: u16,
    pub creation_date: u16,
    pub last_access_date: u16,
    pub high_cluster: u16,
    pub last_write_time: u16,
    pub last_write_date: u16,
    pub low_cluster: u16,
    pub file_size: u32,
}

/// Atributos de archivo FAT32
pub const FAT32_ATTR_READ_ONLY: u8 = 0x01;
pub const FAT32_ATTR_HIDDEN: u8 = 0x02;
pub const FAT32_ATTR_SYSTEM: u8 = 0x04;
pub const FAT32_ATTR_VOLUME_ID: u8 = 0x08;
pub const FAT32_ATTR_DIRECTORY: u8 = 0x10;
pub const FAT32_ATTR_ARCHIVE: u8 = 0x20;
pub const FAT32_ATTR_LONG_NAME: u8 = 0x0F;

/// Estados de cluster FAT32
pub const FAT32_CLUSTER_FREE: u32 = 0x00000000;
pub const FAT32_CLUSTER_RESERVED: u32 = 0x00000001;
pub const FAT32_CLUSTER_BAD: u32 = 0x0FFFFFF7;
pub const FAT32_CLUSTER_EOF: u32 = 0x0FFFFFF8;

/// Driver FAT32
pub struct Fat32Driver {
    pub boot_sector: Fat32BootSector,
    pub fat_table: *mut u32,
    pub data_start: u32,
    pub cluster_size: u32,
    pub total_clusters: u32,
    pub root_cluster: u32,
    pub device_id: u32,
}

impl Fat32Driver {
    /// Crear un nuevo driver FAT32
    pub fn new(device_id: u32) -> Self {
        Self {
            boot_sector: Fat32BootSector {
                jump_instruction: [0; 3],
                oem_name: [0; 8],
                bytes_per_sector: 512,
                sectors_per_cluster: 1,
                reserved_sectors: 32,
                number_of_fats: 2,
                root_entries: 0,
                total_sectors_16: 0,
                media_type: 0xF8,
                sectors_per_fat_16: 0,
                sectors_per_track: 63,
                number_of_heads: 255,
                hidden_sectors: 0,
                total_sectors_32: 0,
                sectors_per_fat_32: 0,
                flags: 0,
                version: 0,
                root_cluster: 2,
                info_sector: 1,
                backup_boot_sector: 6,
                reserved: [0; 12],
                drive_number: 0x80,
                reserved1: 0,
                boot_signature: 0x29,
                volume_id: 0,
                volume_label: *b"REACTOS    ",
                file_system_type: *b"FAT32   ",
                boot_code: [0; 420],
                boot_signature_2: 0xAA55,
            },
            fat_table: core::ptr::null_mut(),
            data_start: 0,
            cluster_size: 0,
            total_clusters: 0,
            root_cluster: 2,
            device_id,
        }
    }
    
    /// Inicializar driver FAT32
    pub fn init(&mut self) -> Result<(), &'static str> {
        // Leer boot sector
        self.read_boot_sector()?;
        
        // Calcular parámetros
        self.cluster_size = self.boot_sector.bytes_per_sector as u32 * self.boot_sector.sectors_per_cluster as u32;
        self.data_start = self.boot_sector.reserved_sectors as u32 + 
                         (self.boot_sector.number_of_fats as u32 * self.boot_sector.sectors_per_fat_32);
        self.total_clusters = (self.boot_sector.total_sectors_32 - self.data_start) / self.boot_sector.sectors_per_cluster as u32;
        self.root_cluster = self.boot_sector.root_cluster;
        
        // Asignar memoria para FAT table
        let fat_size = self.boot_sector.sectors_per_fat_32 * self.boot_sector.bytes_per_sector as u32;
        // TODO: Implementar asignación de memoria para FAT table
        
        Ok(())
    }
    
    /// Leer boot sector
    fn read_boot_sector(&mut self) -> Result<(), &'static str> {
        // TODO: Implementar lectura del boot sector desde el dispositivo
        // Por ahora, usar valores por defecto
        self.boot_sector.bytes_per_sector = 512;
        self.boot_sector.sectors_per_cluster = 8;
        self.boot_sector.reserved_sectors = 32;
        self.boot_sector.number_of_fats = 2;
        self.boot_sector.sectors_per_fat_32 = 1024;
        self.boot_sector.total_sectors_32 = 1048576; // 512MB
        self.boot_sector.root_cluster = 2;
        
        Ok(())
    }
    
    /// Leer cluster
    pub fn read_cluster(&self, cluster: u32, buffer: &mut [u8]) -> Result<(), &'static str> {
        if cluster < 2 || cluster >= self.total_clusters {
            return Err("Invalid cluster number");
        }
        
        let sector = self.data_start + (cluster - 2) * self.boot_sector.sectors_per_cluster as u32;
        let bytes_to_read = core::cmp::min(buffer.len(), self.cluster_size as usize);
        
        // TODO: Implementar lectura real del dispositivo
        // Por ahora, llenar con datos simulados
        for i in 0..bytes_to_read {
            buffer[i] = (cluster as u8).wrapping_add(i as u8);
        }
        
        Ok(())
    }
    
    /// Escribir cluster
    pub fn write_cluster(&self, cluster: u32, buffer: &[u8]) -> Result<(), &'static str> {
        if cluster < 2 || cluster >= self.total_clusters {
            return Err("Invalid cluster number");
        }
        
        let sector = self.data_start + (cluster - 2) * self.boot_sector.sectors_per_cluster as u32;
        let bytes_to_write = core::cmp::min(buffer.len(), self.cluster_size as usize);
        
        // TODO: Implementar escritura real al dispositivo
        // Por ahora, simular escritura exitosa
        
        Ok(())
    }
    
    /// Obtener siguiente cluster en la cadena
    pub fn get_next_cluster(&self, cluster: u32) -> Result<u32, &'static str> {
        if cluster < 2 || cluster >= self.total_clusters {
            return Err("Invalid cluster number");
        }
        
        // TODO: Implementar lectura de FAT table
        // Por ahora, simular que es el último cluster
        Ok(FAT32_CLUSTER_EOF)
    }
    
    /// Asignar nuevo cluster
    pub fn allocate_cluster(&mut self) -> Result<u32, &'static str> {
        // TODO: Implementar búsqueda de cluster libre en FAT
        // Por ahora, simular asignación
        Ok(3)
    }
    
    /// Liberar cluster
    pub fn free_cluster(&mut self, cluster: u32) -> Result<(), &'static str> {
        if cluster < 2 || cluster >= self.total_clusters {
            return Err("Invalid cluster number");
        }
        
        // TODO: Implementar liberación de cluster en FAT
        Ok(())
    }
    
    /// Leer entrada de directorio
    pub fn read_directory_entry(&self, cluster: u32, index: u32) -> Result<Fat32DirectoryEntry, &'static str> {
        let mut buffer = [0u8; 512];
        self.read_cluster(cluster, &mut buffer)?;
        
        let entry_offset = (index * 32) as usize;
        if entry_offset + 32 > buffer.len() {
            return Err("Directory entry out of bounds");
        }
        
        let mut entry = Fat32DirectoryEntry {
            name: [0; 8],
            extension: [0; 3],
            attributes: 0,
            reserved: 0,
            creation_time_tenths: 0,
            creation_time: 0,
            creation_date: 0,
            last_access_date: 0,
            high_cluster: 0,
            last_write_time: 0,
            last_write_date: 0,
            low_cluster: 0,
            file_size: 0,
        };
        
        // Copiar datos del buffer
        entry.name.copy_from_slice(&buffer[entry_offset..entry_offset + 8]);
        entry.extension.copy_from_slice(&buffer[entry_offset + 8..entry_offset + 11]);
        entry.attributes = buffer[entry_offset + 11];
        entry.reserved = buffer[entry_offset + 12];
        entry.creation_time_tenths = buffer[entry_offset + 13];
        entry.creation_time = u16::from_le_bytes([buffer[entry_offset + 14], buffer[entry_offset + 15]]);
        entry.creation_date = u16::from_le_bytes([buffer[entry_offset + 16], buffer[entry_offset + 17]]);
        entry.last_access_date = u16::from_le_bytes([buffer[entry_offset + 18], buffer[entry_offset + 19]]);
        entry.high_cluster = u16::from_le_bytes([buffer[entry_offset + 20], buffer[entry_offset + 21]]);
        entry.last_write_time = u16::from_le_bytes([buffer[entry_offset + 22], buffer[entry_offset + 23]]);
        entry.last_write_date = u16::from_le_bytes([buffer[entry_offset + 24], buffer[entry_offset + 25]]);
        entry.low_cluster = u16::from_le_bytes([buffer[entry_offset + 26], buffer[entry_offset + 27]]);
        entry.file_size = u32::from_le_bytes([
            buffer[entry_offset + 28],
            buffer[entry_offset + 29],
            buffer[entry_offset + 30],
            buffer[entry_offset + 31],
        ]);
        
        Ok(entry)
    }
    
    /// Escribir entrada de directorio
    pub fn write_directory_entry(&self, cluster: u32, index: u32, entry: &Fat32DirectoryEntry) -> Result<(), &'static str> {
        let mut buffer = [0u8; 512];
        self.read_cluster(cluster, &mut buffer)?;
        
        let entry_offset = (index * 32) as usize;
        if entry_offset + 32 > buffer.len() {
            return Err("Directory entry out of bounds");
        }
        
        // Copiar datos al buffer
        buffer[entry_offset..entry_offset + 8].copy_from_slice(&entry.name);
        buffer[entry_offset + 8..entry_offset + 11].copy_from_slice(&entry.extension);
        buffer[entry_offset + 11] = entry.attributes;
        buffer[entry_offset + 12] = entry.reserved;
        buffer[entry_offset + 13] = entry.creation_time_tenths;
        buffer[entry_offset + 14..entry_offset + 16].copy_from_slice(&entry.creation_time.to_le_bytes());
        buffer[entry_offset + 16..entry_offset + 18].copy_from_slice(&entry.creation_date.to_le_bytes());
        buffer[entry_offset + 18..entry_offset + 20].copy_from_slice(&entry.last_access_date.to_le_bytes());
        buffer[entry_offset + 20..entry_offset + 22].copy_from_slice(&entry.high_cluster.to_le_bytes());
        buffer[entry_offset + 22..entry_offset + 24].copy_from_slice(&entry.last_write_time.to_le_bytes());
        buffer[entry_offset + 24..entry_offset + 26].copy_from_slice(&entry.last_write_date.to_le_bytes());
        buffer[entry_offset + 26..entry_offset + 28].copy_from_slice(&entry.low_cluster.to_le_bytes());
        buffer[entry_offset + 28..entry_offset + 32].copy_from_slice(&entry.file_size.to_le_bytes());
        
        self.write_cluster(cluster, &buffer)?;
        Ok(())
    }
    
    /// Buscar archivo en directorio
    pub fn find_file(&self, directory_cluster: u32, filename: &str) -> Result<Fat32DirectoryEntry, &'static str> {
        let mut cluster = directory_cluster;
        let mut entry_index = 0;
        
        loop {
            let entry = self.read_directory_entry(cluster, entry_index)?;
            
            // Verificar si es entrada válida
            if entry.name[0] == 0x00 {
                return Err("File not found");
            }
            
            if entry.name[0] != 0xE5 { // No es entrada eliminada
                // Construir nombre completo
                let mut full_name = [0u8; 12];
                full_name[..8].copy_from_slice(&entry.name);
                if entry.extension[0] != b' ' {
                    full_name[8] = b'.';
                    full_name[9..12].copy_from_slice(&entry.extension);
                }
                
                // Comparar nombres
                let name_str = core::str::from_utf8(&full_name).unwrap_or("");
                if name_str.trim_end_matches('\0') == filename {
                    return Ok(entry);
                }
            }
            
            entry_index += 1;
            if entry_index >= 16 { // 16 entradas por cluster (512/32)
                entry_index = 0;
                cluster = self.get_next_cluster(cluster)?;
                if cluster >= FAT32_CLUSTER_EOF {
                    return Err("File not found");
                }
            }
        }
    }
    
    /// Crear archivo
    pub fn create_file(&mut self, directory_cluster: u32, filename: &str) -> Result<Fat32DirectoryEntry, &'static str> {
        // Buscar entrada libre
        let mut cluster = directory_cluster;
        let mut entry_index = 0;
        
        loop {
            let entry = self.read_directory_entry(cluster, entry_index)?;
            
            if entry.name[0] == 0x00 || entry.name[0] == 0xE5 {
                // Entrada libre encontrada
                let mut new_entry = Fat32DirectoryEntry {
                    name: [0x20; 8],
                    extension: [0x20; 3],
                    attributes: 0,
                    reserved: 0,
                    creation_time_tenths: 0,
                    creation_time: 0,
                    creation_date: 0,
                    last_access_date: 0,
                    high_cluster: 0,
                    last_write_time: 0,
                    last_write_date: 0,
                    low_cluster: 0,
                    file_size: 0,
                };
                
                // Parsear nombre y extensión
                if let Some(dot_pos) = filename.find('.') {
                    let name_part = &filename[..dot_pos];
                    let ext_part = &filename[dot_pos + 1..];
                    let name_bytes = name_part.as_bytes();
                    let ext_bytes = ext_part.as_bytes();
                    let name_len = core::cmp::min(name_bytes.len(), 8);
                    let ext_len = core::cmp::min(ext_bytes.len(), 3);
                    new_entry.name[..name_len].copy_from_slice(&name_bytes[..name_len]);
                    new_entry.extension[..ext_len].copy_from_slice(&ext_bytes[..ext_len]);
                } else {
                    let name_bytes = filename.as_bytes();
                    let name_len = core::cmp::min(name_bytes.len(), 8);
                    new_entry.name[..name_len].copy_from_slice(&name_bytes[..name_len]);
                }
                
                // Asignar cluster
                new_entry.low_cluster = self.allocate_cluster()? as u16;
                
                self.write_directory_entry(cluster, entry_index, &new_entry)?;
                return Ok(new_entry);
            }
            
            entry_index += 1;
            if entry_index >= 16 {
                entry_index = 0;
                cluster = self.get_next_cluster(cluster)?;
                if cluster >= FAT32_CLUSTER_EOF {
                    return Err("Directory full");
                }
            }
        }
    }
}

/// Instancia global del driver FAT32
static mut FAT32_DRIVER: Option<Fat32Driver> = None;

/// Inicializar driver FAT32
pub fn init_fat32(device_id: u32) -> Result<(), &'static str> {
    unsafe {
        FAT32_DRIVER = Some(Fat32Driver::new(device_id));
        if let Some(ref mut driver) = FAT32_DRIVER {
            driver.init()?;
        }
    }
    Ok(())
}

/// Obtener driver FAT32
pub fn get_fat32_driver() -> Option<&'static mut Fat32Driver> {
    unsafe {
        FAT32_DRIVER.as_mut()
    }
}
