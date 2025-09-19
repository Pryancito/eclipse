//! Implementación básica de FAT32 para Eclipse OS
//! 
//! Proporciona soporte para leer y escribir archivos en sistemas de archivos FAT32

use crate::filesystem::{
    VfsResult, BLOCK_SIZE,
    superblock::FileSystemType,
};
use crate::filesystem::vfs::VfsError;
use alloc::vec;
use alloc::string::String;

// Constantes FAT32
const FAT32_SIGNATURE: u32 = 0x41615252; // "RRaA"
const FAT32_FSINFO_SIGNATURE: u32 = 0x61417272; // "rrAa"
const FAT32_END_OF_CLUSTER: u32 = 0x0FFFFFF8;
const FAT32_BAD_CLUSTER: u32 = 0x0FFFFFF7;
const FAT32_FREE_CLUSTER: u32 = 0x00000000;

// Tipos de entrada de directorio
const ATTR_READ_ONLY: u8 = 0x01;
const ATTR_HIDDEN: u8 = 0x02;
const ATTR_SYSTEM: u8 = 0x04;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LONG_NAME: u8 = 0x0F;

// Boot Sector FAT32
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
    pub heads: u16,
    pub hidden_sectors: u32,
    pub total_sectors_32: u32,
    pub sectors_per_fat_32: u32,
    pub flags: u16,
    pub version: u16,
    pub root_cluster: u32,
    pub fs_info_sector: u16,
    pub backup_boot_sector: u16,
    pub reserved: [u8; 12],
    pub drive_number: u8,
    pub reserved1: u8,
    pub boot_signature: u8,
    pub volume_id: u32,
    pub volume_label: [u8; 11],
    pub file_system_type: [u8; 8],
    pub boot_code: [u8; 420],
    pub boot_sector_signature: u16,
}

// FSInfo Sector FAT32
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct Fat32FsInfo {
    pub lead_signature: u32,
    pub reserved1: [u8; 480],
    pub struct_signature: u32,
    pub free_cluster_count: u32,
    pub next_free_cluster: u32,
    pub reserved2: [u8; 12],
    pub trail_signature: u32,
}

// Entrada de directorio FAT32
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct Fat32DirEntry {
    pub name: [u8; 8],
    pub extension: [u8; 3],
    pub attributes: u8,
    pub reserved: u8,
    pub creation_time_tenths: u8,
    pub creation_time: u16,
    pub creation_date: u16,
    pub last_access_date: u16,
    pub first_cluster_high: u16,
    pub last_write_time: u16,
    pub last_write_date: u16,
    pub first_cluster_low: u16,
    pub file_size: u32,
}

impl Fat32DirEntry {
    pub fn is_deleted(&self) -> bool {
        self.name[0] == 0xE5
    }
    
    pub fn is_end(&self) -> bool {
        self.name[0] == 0x00
    }
    
    pub fn is_long_name(&self) -> bool {
        (self.attributes & ATTR_LONG_NAME) == ATTR_LONG_NAME
    }
    
    pub fn is_directory(&self) -> bool {
        (self.attributes & ATTR_DIRECTORY) == ATTR_DIRECTORY
    }
    
    pub fn is_file(&self) -> bool {
        !self.is_directory() && !self.is_long_name() && !self.is_deleted() && !self.is_end()
    }
    
    pub fn get_first_cluster(&self) -> u32 {
        ((self.first_cluster_high as u32) << 16) | (self.first_cluster_low as u32)
    }
    
    pub fn get_file_name(&self) -> String {
        let mut name = String::new();
        
        // Nombre base
        for &byte in &self.name {
            if byte != b' ' && byte != 0 {
                name.push(byte as char);
            }
        }
        
        // Extensión
        if self.extension[0] != b' ' {
            name.push('.');
            for &byte in &self.extension {
                if byte != b' ' && byte != 0 {
                    name.push(byte as char);
                }
            }
        }
        
        name
    }
}

// Driver FAT32
pub struct Fat32Driver {
    pub boot_sector: Fat32BootSector,
    pub fs_info: Fat32FsInfo,
    pub bytes_per_cluster: u32,
    pub fat_start_sector: u32,
    pub data_start_sector: u32,
    pub root_dir_cluster: u32,
}

impl Fat32Driver {
    pub fn new() -> Self {
        Self {
            boot_sector: Fat32BootSector {
                jump_instruction: [0; 3],
                oem_name: [0; 8],
                bytes_per_sector: 0,
                sectors_per_cluster: 0,
                reserved_sectors: 0,
                number_of_fats: 0,
                root_entries: 0,
                total_sectors_16: 0,
                media_type: 0,
                sectors_per_fat_16: 0,
                sectors_per_track: 0,
                heads: 0,
                hidden_sectors: 0,
                total_sectors_32: 0,
                sectors_per_fat_32: 0,
                flags: 0,
                version: 0,
                root_cluster: 0,
                fs_info_sector: 0,
                backup_boot_sector: 0,
                reserved: [0; 12],
                drive_number: 0,
                reserved1: 0,
                boot_signature: 0,
                volume_id: 0,
                volume_label: [0; 11],
                file_system_type: [0; 8],
                boot_code: [0; 420],
                boot_sector_signature: 0,
            },
            fs_info: Fat32FsInfo {
                lead_signature: 0,
                reserved1: [0; 480],
                struct_signature: 0,
                free_cluster_count: 0,
                next_free_cluster: 0,
                reserved2: [0; 12],
                trail_signature: 0,
            },
            bytes_per_cluster: 0,
            fat_start_sector: 0,
            data_start_sector: 0,
            root_dir_cluster: 0,
        }
    }
    
    /// Inicializar driver FAT32
    pub fn init(&mut self) -> VfsResult<()> {
        // Leer boot sector (simplificado)
        self.boot_sector.bytes_per_sector = BLOCK_SIZE as u16;
        self.boot_sector.sectors_per_cluster = 8; // 8 sectores por cluster
        self.boot_sector.reserved_sectors = 32;
        self.boot_sector.number_of_fats = 2;
        self.boot_sector.total_sectors_32 = 1024;
        self.boot_sector.sectors_per_fat_32 = 100;
        self.boot_sector.root_cluster = 2;
        self.boot_sector.boot_signature = 0x29;
        self.boot_sector.volume_id = 0x12345678;
        self.boot_sector.boot_sector_signature = 0xAA55;
        
        // Configurar valores calculados
        self.bytes_per_cluster = (self.boot_sector.bytes_per_sector as u32) * (self.boot_sector.sectors_per_cluster as u32);
        self.fat_start_sector = self.boot_sector.reserved_sectors as u32;
        self.data_start_sector = self.fat_start_sector + (self.boot_sector.sectors_per_fat_32 * self.boot_sector.number_of_fats as u32);
        self.root_dir_cluster = self.boot_sector.root_cluster;
        
        Ok(())
    }
    
    /// Leer cluster del disco
    pub fn read_cluster(&mut self, cluster: u32, buffer: &mut [u8]) -> VfsResult<usize> {
        if cluster < 2 {
            return Err(VfsError::InvalidArgument);
        }
        
        // Calcular sector del cluster
        let cluster_sector = self.data_start_sector + ((cluster - 2) * self.boot_sector.sectors_per_cluster as u32);
        
        // Leer sectores del cluster
        let sectors_per_cluster = self.boot_sector.sectors_per_cluster as usize;
        let bytes_per_sector = self.boot_sector.bytes_per_sector as usize;
        let cluster_size = sectors_per_cluster * bytes_per_sector;
        
        let to_read = buffer.len().min(cluster_size);
        
        // Simular lectura (en un sistema real, esto leería del disco)
        for i in 0..to_read {
            buffer[i] = ((cluster + (i as u32 / bytes_per_sector as u32)) % 256) as u8;
        }
        
        Ok(to_read)
    }
    
    /// Escribir cluster al disco
    pub fn write_cluster(&mut self, cluster: u32, data: &[u8]) -> VfsResult<usize> {
        if cluster < 2 {
            return Err(VfsError::InvalidArgument);
        }
        
        // Simular escritura (en un sistema real, esto escribiría al disco)
        let sectors_per_cluster = self.boot_sector.sectors_per_cluster as usize;
        let bytes_per_sector = self.boot_sector.bytes_per_sector as usize;
        let cluster_size = sectors_per_cluster * bytes_per_sector;
        
        let to_write = data.len().min(cluster_size);
        Ok(to_write)
    }
    
    /// Leer entrada de directorio
    pub fn read_dir_entry(&mut self, cluster: u32, index: usize) -> VfsResult<Fat32DirEntry> {
        let mut buffer = [0u8; 32];
        let offset = index * 32;
        
        // Leer cluster y extraer entrada
        let mut cluster_data = vec![0u8; self.bytes_per_cluster as usize];
        self.read_cluster(cluster, &mut cluster_data)?;
        
        if offset + 32 > cluster_data.len() {
            return Err(VfsError::FileNotFound);
        }
        
        buffer.copy_from_slice(&cluster_data[offset..offset + 32]);
        
        // Convertir bytes a estructura
        let entry = unsafe { core::ptr::read(buffer.as_ptr() as *const Fat32DirEntry) };
        
        if entry.is_end() {
            Err(VfsError::FileNotFound)
        } else {
            Ok(entry)
        }
    }
    
    /// Buscar archivo en directorio
    pub fn find_file(&mut self, cluster: u32, filename: &str) -> VfsResult<Fat32DirEntry> {
        let entries_per_cluster = (self.bytes_per_cluster / 32) as usize;
        
        for i in 0..entries_per_cluster {
            match self.read_dir_entry(cluster, i) {
                Ok(entry) => {
                    if entry.is_file() || entry.is_directory() {
                        let entry_name = entry.get_file_name();
                        if entry_name.to_lowercase() == filename.to_lowercase() {
                            return Ok(entry);
                        }
                    }
                }
                Err(VfsError::FileNotFound) => break,
                Err(e) => return Err(e),
            }
        }
        
        Err(VfsError::FileNotFound)
    }
    
    /// Obtener información del sistema de archivos
    pub fn get_filesystem_info(&self) -> (u32, u32, u32, u32) {
        (
            self.boot_sector.total_sectors_32,
            self.fs_info.free_cluster_count,
            self.boot_sector.sectors_per_fat_32,
            self.bytes_per_cluster
        )
    }
}

// Instancia global del driver FAT32
static mut FAT32_DRIVER: Option<Fat32Driver> = None;

/// Inicializar driver FAT32
pub fn init_fat32() -> VfsResult<()> {
    unsafe {
        FAT32_DRIVER = Some(Fat32Driver::new());
        if let Some(ref mut driver) = FAT32_DRIVER {
            driver.init()?;
        }
    }
    Ok(())
}

/// Obtener instancia del driver FAT32
pub fn get_fat32_driver() -> Option<&'static mut Fat32Driver> {
    unsafe { FAT32_DRIVER.as_mut() }
}
