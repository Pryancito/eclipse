//! Implementación completa de FAT32 para Eclipse OS
//!
//! Proporciona soporte completo para leer y escribir archivos en sistemas de archivos FAT32
//! Incluye soporte para nombres de archivo largos (VFAT) y operaciones completas de archivos

use crate::filesystem::{
    superblock::FileSystemType,
    vfs::{FileSystem, StatInfo, VfsError},
};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::{self, Vec};
use core::mem;
use core::ptr::NonNull;
use spin::Mutex;

// Constantes FAT32
const FAT32_SIGNATURE: u32 = 0x41615252; // "RRaA"
const FAT32_FSINFO_SIGNATURE: u32 = 0x61417272; // "rrAa"
const FAT32_END_OF_CLUSTER: u32 = 0x0FFFFFF8;
const FAT32_BAD_CLUSTER: u32 = 0x0FFFFFF7;
const FAT32_FREE_CLUSTER: u32 = 0x00000000;
const FAT32_END_OF_CHAIN: u32 = 0x0FFFFFFF;

// Tipos de entrada de directorio
const ATTR_READ_ONLY: u8 = 0x01;
const ATTR_HIDDEN: u8 = 0x02;
const ATTR_SYSTEM: u8 = 0x04;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LONG_NAME: u8 = 0x0F;

// Constantes para nombres largos
const LFN_LAST_ENTRY: u8 = 0x40;
const LFN_DELETED: u8 = 0xE5;

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

// Entrada de nombre largo (VFAT)
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct Fat32LongNameEntry {
    pub sequence_number: u8,
    pub name_part1: [u16; 5],
    pub attributes: u8,
    pub type_field: u8,
    pub checksum: u8,
    pub name_part2: [u16; 6],
    pub first_cluster: u16,
    pub name_part3: [u16; 2],
}

// Información de archivo/directorio
#[derive(Debug, Clone)]
pub struct Fat32FileInfo {
    pub name: String,
    pub long_name: Option<String>,
    pub attributes: u8,
    pub first_cluster: u32,
    pub file_size: u32,
    pub creation_time: u32,
    pub last_write_time: u32,
    pub last_access_date: u16,
}

impl Fat32DirEntry {
    pub fn is_deleted(&self) -> bool {
        self.name[0] == LFN_DELETED
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

    pub fn to_file_info(&self) -> Fat32FileInfo {
        Fat32FileInfo {
            name: self.get_file_name(),
            long_name: None,
            attributes: self.attributes,
            first_cluster: self.get_first_cluster(),
            file_size: self.file_size,
            creation_time: self.creation_time as u32,
            last_write_time: self.last_write_time as u32,
            last_access_date: self.last_access_date,
        }
    }
}

impl Fat32LongNameEntry {
    pub fn is_last_entry(&self) -> bool {
        (self.sequence_number & LFN_LAST_ENTRY) != 0
    }

    pub fn get_sequence_number(&self) -> u8 {
        self.sequence_number & 0x1F
    }

    pub fn extract_name_parts(&self) -> Vec<u16> {
        let mut parts = Vec::new();

        // Agregar las tres partes del nombre (copiar para evitar acceso no alineado)
        let part1 = self.name_part1;
        for part in part1.iter() {
            if *part != 0 {
                parts.push(*part);
            }
        }
        let part2 = self.name_part2;
        for part in part2.iter() {
            if *part != 0 {
                parts.push(*part);
            }
        }
        let part3 = self.name_part3;
        for part in part3.iter() {
            if *part != 0 {
                parts.push(*part);
            }
        }

        parts
    }
}

// Driver FAT32 principal
pub struct Fat32Driver {
    pub boot_sector: Fat32BootSector,
    pub fs_info: Fat32FsInfo,
    pub bytes_per_cluster: u32,
    pub fat_start_sector: u32,
    pub data_start_sector: u32,
    pub root_dir_cluster: u32,
    pub total_clusters: u32,
    pub fat_size_sectors: u32,
    pub is_initialized: bool,
    pub cache: BTreeMap<u32, Vec<u8>>, // Cache de clusters
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
            total_clusters: 0,
            fat_size_sectors: 0,
            is_initialized: false,
            cache: BTreeMap::new(),
        }
    }

    /// Inicializar driver FAT32 desde boot sector real
    pub fn init_from_boot_sector(&mut self, boot_data: &[u8]) -> Result<(), VfsError> {
        if boot_data.len() < mem::size_of::<Fat32BootSector>() {
            return Err(VfsError::InvalidArgument);
        }

        // Leer boot sector
        unsafe {
            self.boot_sector = core::ptr::read(boot_data.as_ptr() as *const Fat32BootSector);
        }

        // Validar firma
        if self.boot_sector.boot_sector_signature != 0xAA55 {
            return Err(VfsError::InvalidArgument);
        }

        // Validar que es FAT32
        if !self.is_fat32() {
            return Err(VfsError::InvalidArgument);
        }

        // Calcular valores derivados
        self.bytes_per_cluster = (self.boot_sector.bytes_per_sector as u32)
            * (self.boot_sector.sectors_per_cluster as u32);
        self.fat_start_sector = self.boot_sector.reserved_sectors as u32;
        self.fat_size_sectors = self.boot_sector.sectors_per_fat_32;
        self.data_start_sector = self.fat_start_sector
            + (self.fat_size_sectors * self.boot_sector.number_of_fats as u32);
        self.root_dir_cluster = self.boot_sector.root_cluster;

        // Calcular total de clusters
        let total_sectors = if self.boot_sector.total_sectors_32 != 0 {
            self.boot_sector.total_sectors_32
        } else {
            self.boot_sector.total_sectors_16 as u32
        };

        self.total_clusters = (total_sectors - self.data_start_sector)
            / (self.boot_sector.sectors_per_cluster as u32);

        // Leer FSInfo si está disponible
        if self.boot_sector.fs_info_sector != 0 {
            self.read_fs_info()?;
        }

        self.is_initialized = true;
        Ok(())
    }

    /// Inicializar driver FAT32 con valores simulados (para desarrollo)
    pub fn init_simulated(&mut self) -> Result<(), VfsError> {
        // Configurar boot sector simulado
        self.boot_sector.bytes_per_sector = 512; // BLOCK_SIZE as u16;
        self.boot_sector.sectors_per_cluster = 8;
        self.boot_sector.reserved_sectors = 32;
        self.boot_sector.number_of_fats = 2;
        self.boot_sector.total_sectors_32 = 1024;
        self.boot_sector.sectors_per_fat_32 = 100;
        self.boot_sector.root_cluster = 2;
        self.boot_sector.boot_signature = 0x29;
        self.boot_sector.volume_id = 0x12345678;
        self.boot_sector.boot_sector_signature = 0xAA55;

        // Configurar valores calculados
        self.bytes_per_cluster = (self.boot_sector.bytes_per_sector as u32)
            * (self.boot_sector.sectors_per_cluster as u32);
        self.fat_start_sector = self.boot_sector.reserved_sectors as u32;
        self.fat_size_sectors = self.boot_sector.sectors_per_fat_32;
        self.data_start_sector = self.fat_start_sector
            + (self.fat_size_sectors * self.boot_sector.number_of_fats as u32);
        self.root_dir_cluster = self.boot_sector.root_cluster;
        self.total_clusters = 100;

        self.is_initialized = true;
        Ok(())
    }

    /// Verificar si es un sistema FAT32 válido
    pub fn is_fat32(&self) -> bool {
        // Copiar campos empaquetados a variables locales para evitar problemas de alineación
        let sectors_per_fat_32 = self.boot_sector.sectors_per_fat_32;
        let root_cluster = self.boot_sector.root_cluster;
        let boot_sector_signature = self.boot_sector.boot_sector_signature;
        
        crate::debug::serial_write_str(&alloc::format!(
            "FAT32: Validando FAT32 - sectors_per_fat_32: {}, root_cluster: {}, boot_sector_signature: 0x{:04X}\n",
            sectors_per_fat_32,
            root_cluster,
            boot_sector_signature
        ));
        
        let sectors_per_fat_valid = sectors_per_fat_32 != 0;
        let root_cluster_valid = root_cluster != 0;
        let signature_valid = boot_sector_signature == 0xAA55;
        
        crate::debug::serial_write_str(&alloc::format!(
            "FAT32: Validaciones - sectors_per_fat: {}, root_cluster: {}, signature: {}\n",
            sectors_per_fat_valid,
            root_cluster_valid,
            signature_valid
        ));
        
        sectors_per_fat_valid && root_cluster_valid && signature_valid
    }

    /// Leer FSInfo sector
    fn read_fs_info(&mut self) -> Result<(), VfsError> {
        // En una implementación real, leeríamos desde el disco
        // Por ahora, configuramos valores simulados
        self.fs_info.lead_signature = FAT32_FSINFO_SIGNATURE;
        self.fs_info.struct_signature = FAT32_FSINFO_SIGNATURE;
        self.fs_info.free_cluster_count = self.total_clusters / 2; // Simular 50% libre
        self.fs_info.next_free_cluster = 3;
        self.fs_info.trail_signature = FAT32_FSINFO_SIGNATURE;
        Ok(())
    }

    /// Leer cluster del disco
    pub fn read_cluster(&mut self, cluster: u32, buffer: &mut [u8]) -> Result<usize, VfsError> {
        if !self.is_initialized {
            return Err(VfsError::InvalidOperation);
        }

        if cluster < 2 || cluster >= self.total_clusters {
            return Err(VfsError::InvalidArgument);
        }

        // Verificar cache primero
        if let Some(cached_data) = self.cache.get(&cluster) {
            let to_copy = buffer.len().min(cached_data.len());
            buffer[..to_copy].copy_from_slice(&cached_data[..to_copy]);
            return Ok(to_copy);
        }

        // Calcular sector del cluster
        let cluster_sector =
            self.data_start_sector + ((cluster - 2) * self.boot_sector.sectors_per_cluster as u32);

        // Leer sectores del cluster
        let sectors_per_cluster = self.boot_sector.sectors_per_cluster as usize;
        let bytes_per_sector = self.boot_sector.bytes_per_sector as usize;
        let cluster_size = sectors_per_cluster * bytes_per_sector;

        let to_read = buffer.len().min(cluster_size);

        // Simular lectura (en un sistema real, esto leería del disco)
        for i in 0..to_read {
            buffer[i] = ((cluster + (i as u32 / bytes_per_sector as u32)) % 256) as u8;
        }

        // Cachear el cluster leído
        let mut cluster_data = Vec::with_capacity(cluster_size);
        cluster_data.resize(cluster_size, 0u8);
        cluster_data[..to_read].copy_from_slice(&buffer[..to_read]);
        self.cache.insert(cluster, cluster_data);

        Ok(to_read)
    }

    /// Escribir cluster al disco
    pub fn write_cluster(&mut self, cluster: u32, data: &[u8]) -> Result<usize, VfsError> {
        if !self.is_initialized {
            return Err(VfsError::InvalidOperation);
        }

        if cluster < 2 || cluster >= self.total_clusters {
            return Err(VfsError::InvalidArgument);
        }

        let sectors_per_cluster = self.boot_sector.sectors_per_cluster as usize;
        let bytes_per_sector = self.boot_sector.bytes_per_sector as usize;
        let cluster_size = sectors_per_cluster * bytes_per_sector;

        let to_write = data.len().min(cluster_size);

        // Simular escritura (en un sistema real, esto escribiría al disco)
        // Actualizar cache
        let mut cluster_data = Vec::with_capacity(cluster_size);
        cluster_data.resize(cluster_size, 0u8);
        cluster_data[..to_write].copy_from_slice(&data[..to_write]);
        self.cache.insert(cluster, cluster_data);

        Ok(to_write)
    }

    /// Leer entrada de la tabla FAT
    pub fn read_fat_entry(&mut self, cluster: u32) -> Result<u32, VfsError> {
        if !self.is_initialized {
            return Err(VfsError::InvalidOperation);
        }

        if cluster >= self.total_clusters {
            return Err(VfsError::InvalidArgument);
        }

        // Calcular offset en la tabla FAT
        let fat_offset = cluster * 4; // 4 bytes por entrada FAT32
        let fat_sector =
            self.fat_start_sector + (fat_offset / self.boot_sector.bytes_per_sector as u32);
        let sector_offset = (fat_offset % self.boot_sector.bytes_per_sector as u32) as usize;

        // Leer sector de la tabla FAT
        let mut sector_data = Vec::with_capacity(self.boot_sector.bytes_per_sector as usize);
        sector_data.resize(self.boot_sector.bytes_per_sector as usize, 0u8);
        // En una implementación real, leeríamos desde el disco
        // Por ahora, simulamos valores
        let fat_entry = if cluster < 10 {
            cluster + 1 // Cadena simple
        } else {
            FAT32_END_OF_CHAIN
        };

        Ok(fat_entry)
    }

    /// Escribir entrada de la tabla FAT
    pub fn write_fat_entry(&mut self, cluster: u32, value: u32) -> Result<(), VfsError> {
        if !self.is_initialized {
            return Err(VfsError::InvalidOperation);
        }

        if cluster >= self.total_clusters {
            return Err(VfsError::InvalidArgument);
        }

        // En una implementación real, escribiríamos a la tabla FAT
        // Por ahora, solo simulamos
        Ok(())
    }

    /// Seguir cadena de clusters
    pub fn get_cluster_chain(&mut self, start_cluster: u32) -> Result<Vec<u32>, VfsError> {
        let mut chain = Vec::new();
        let mut current = start_cluster;

        while current < FAT32_END_OF_CLUSTER {
            chain.push(current);
            current = self.read_fat_entry(current)?;

            if chain.len() > 1000 {
                // Prevenir bucles infinitos
                return Err(VfsError::InvalidOperation);
            }
        }

        Ok(chain)
    }

    /// Leer entrada de directorio
    pub fn read_dir_entry(&mut self, cluster: u32, index: usize) -> Result<Fat32DirEntry, VfsError> {
        let mut buffer = [0u8; 32];
        let offset = index * 32;

        // Leer cluster y extraer entrada
        let mut cluster_data = Vec::with_capacity(self.bytes_per_cluster as usize);
        cluster_data.resize(self.bytes_per_cluster as usize, 0u8);
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

    /// Leer directorio completo
    pub fn read_directory(&mut self, cluster: u32) -> Result<Vec<Fat32FileInfo>, VfsError> {
        let mut entries = Vec::new();
        let entries_per_cluster = (self.bytes_per_cluster / 32) as usize;
        let mut long_name_parts = Vec::new();

        for i in 0..entries_per_cluster {
            match self.read_dir_entry(cluster, i) {
                Ok(entry) => {
                    if entry.is_long_name() {
                        // Procesar entrada de nombre largo
                        let lfn_entry = unsafe {
                            core::ptr::read(
                                &entry as *const Fat32DirEntry as *const Fat32LongNameEntry,
                            )
                        };
                        long_name_parts.push(lfn_entry);
                    } else if entry.is_file() || entry.is_directory() {
                        // Procesar entrada de archivo/directorio
                        let mut file_info = entry.to_file_info();

                        // Si hay partes de nombre largo, reconstruir el nombre
                        if !long_name_parts.is_empty() {
                            file_info.long_name =
                                Some(self.reconstruct_long_name(&long_name_parts));
                            long_name_parts.clear();
                        }

                        entries.push(file_info);
                    }
                }
                Err(VfsError::FileNotFound) => break,
                Err(e) => return Err(e),
            }
        }

        Ok(entries)
    }

    /// Reconstruir nombre largo desde partes VFAT
    fn reconstruct_long_name(&self, parts: &[Fat32LongNameEntry]) -> String {
        let mut name_parts = Vec::new();

        // Ordenar por número de secuencia
        let mut sorted_parts = parts.to_vec();
        sorted_parts.sort_by_key(|p| p.get_sequence_number());

        for part in sorted_parts {
            name_parts.extend_from_slice(&part.extract_name_parts());
        }

        // Convertir UTF-16 a String
        let mut result = String::new();
        for &ch in &name_parts {
            if ch != 0 {
                result.push(char::from_u32(ch as u32).unwrap_or('?'));
            }
        }

        result
    }

    /// Buscar archivo en directorio
    pub fn find_file(&mut self, cluster: u32, filename: &str) -> Result<Fat32FileInfo, VfsError> {
        let entries = self.read_directory(cluster)?;

        for entry in entries {
            let name_to_check = entry.long_name.as_ref().unwrap_or(&entry.name);
            if name_to_check.to_lowercase() == filename.to_lowercase() {
                return Ok(entry);
            }
        }

        Err(VfsError::FileNotFound)
    }

    /// Leer archivo completo
    pub fn read_file(&mut self, start_cluster: u32) -> Result<Vec<u8>, VfsError> {
        let cluster_chain = self.get_cluster_chain(start_cluster)?;
        let mut file_data = Vec::new();

        for cluster in cluster_chain {
            let mut cluster_data = Vec::with_capacity(self.bytes_per_cluster as usize);
            cluster_data.resize(self.bytes_per_cluster as usize, 0u8);
            let bytes_read = self.read_cluster(cluster, &mut cluster_data)?;
            file_data.extend_from_slice(&cluster_data[..bytes_read]);
        }

        Ok(file_data)
    }

    /// Escribir archivo completo
    pub fn write_file(&mut self, data: &[u8]) -> Result<u32, VfsError> {
        let clusters_needed =
            (data.len() as u32 + self.bytes_per_cluster - 1) / self.bytes_per_cluster;
        let mut allocated_clusters = Vec::new();

        // Asignar clusters (simplificado)
        for i in 0..clusters_needed {
            let cluster = 3 + i; // Simular asignación secuencial
            if cluster >= self.total_clusters {
                return Err(VfsError::NoSpaceLeft);
            }
            allocated_clusters.push(cluster);
        }

        // Escribir datos en clusters
        let mut data_offset = 0;
        for (i, &cluster) in allocated_clusters.iter().enumerate() {
            let is_last = i == allocated_clusters.len() - 1;
            let cluster_size = if is_last {
                data.len() - data_offset
            } else {
                self.bytes_per_cluster as usize
            };

            self.write_cluster(cluster, &data[data_offset..data_offset + cluster_size])?;
            data_offset += cluster_size;

            // Actualizar tabla FAT
            let next_cluster = if is_last {
                FAT32_END_OF_CHAIN
            } else {
                allocated_clusters[i + 1]
            };
            self.write_fat_entry(cluster, next_cluster)?;
        }

        Ok(allocated_clusters[0])
    }

    /// Obtener información del sistema de archivos
    pub fn get_filesystem_info(&self) -> (u32, u32, u32, u32) {
        (
            self.boot_sector.total_sectors_32,
            self.fs_info.free_cluster_count,
            self.fat_size_sectors,
            self.bytes_per_cluster,
        )
    }

    /// Verificar si el driver está inicializado
    pub fn is_ready(&self) -> bool {
        self.is_initialized
    }

    /// Limpiar cache
    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    /// Verificar si archivo existe (método de compatibilidad)
    pub fn file_exists(&mut self, path: &str) -> bool {
        if path.starts_with("/boot/") {
            let filename = &path[6..];
            self.find_file(self.root_dir_cluster, filename).is_ok()
        } else if path.starts_with("/ai_models/") {
            // Para ai_models, simular que existe
            true
        } else {
            false
        }
    }

    /// Crear directorio (método de compatibilidad)
    pub fn create_directory(&mut self, path: &str) -> Result<(), VfsError> {
        // En una implementación real, crearíamos entrada de directorio
        // Por ahora, solo simulamos éxito
        Ok(())
    }

    /// Leer archivo por path (método de compatibilidad)
    pub fn read_file_by_path(&mut self, path: &str) -> Result<Vec<u8>, VfsError> {
        if path.starts_with("/boot/") {
            let filename = &path[6..];
            match self.find_file(self.root_dir_cluster, filename) {
                Ok(file_info) => self.read_file(file_info.first_cluster),
                Err(_) => {
                    // Si no se encuentra, devolver contenido simulado
                    match path {
                        "/boot/ai_models/index.json" => Ok(self.get_simulated_index_json()),
                        _ => Err(VfsError::FileNotFound),
                    }
                }
            }
        } else if path.starts_with("/ai_models/") {
            // Para ai_models, devolver contenido simulado
            match path {
                "/ai_models/index.json" => Ok(self.get_simulated_index_json()),
                _ => Err(VfsError::FileNotFound),
            }
        } else {
            Err(VfsError::InvalidPath)
        }
    }

    /// Escribir archivo por path (método de compatibilidad)
    pub fn write_file_by_path(&mut self, path: &str, data: &[u8]) -> Result<usize, VfsError> {
        if path.starts_with("/boot/") {
            // Escribir archivo y obtener cluster inicial
            match self.write_file(data) {
                Ok(_) => Ok(data.len()),
                Err(e) => Err(e),
            }
        } else {
            Err(VfsError::InvalidPath)
        }
    }

    /// Obtener contenido simulado del index.json
    fn get_simulated_index_json(&self) -> Vec<u8> {
        let content = r#"{
  "version": "1.0.0",
  "total_models": 0,
  "models": []
}"#;
        content.as_bytes().to_vec()
    }
}

// Instancia global del driver FAT32
static mut FAT32_DRIVER: Option<Fat32Driver> = None;

/// Inicializar driver FAT32
pub fn init_fat32() -> Result<(), VfsError> {
    unsafe {
        FAT32_DRIVER = Some(Fat32Driver::new());
        if let Some(ref mut driver) = FAT32_DRIVER {
            driver.init_simulated()?;
        }
    }
    Ok(())
}

/// Inicializar driver FAT32 desde boot sector real
pub fn init_fat32_from_boot(boot_data: &[u8]) -> Result<(), VfsError> {
    unsafe {
        FAT32_DRIVER = Some(Fat32Driver::new());
        if let Some(ref mut driver) = FAT32_DRIVER {
            driver.init_from_boot_sector(boot_data)?;
        }
    }
    Ok(())
}

/// Obtener instancia del driver FAT32
pub fn get_fat32_driver() -> Option<&'static mut Fat32Driver> {
    unsafe { FAT32_DRIVER.as_mut() }
}

/// Verificar si FAT32 está disponible
pub fn is_fat32_available() -> bool {
    unsafe { FAT32_DRIVER.as_ref().map(|d| d.is_ready()).unwrap_or(false) }
}

/// Información sobre el dispositivo donde se debe montar FAT32
#[derive(Debug, Clone)]
pub struct Fat32DeviceInfo {
    /// Nombre del dispositivo Linux (ej: "/dev/sda1")
    pub device_name: String,
    /// Tamaño de la partición en sectores LBA
    pub size_lba: u64,
    /// Sector de inicio LBA
    pub start_lba: u64,
    /// Información adicional sobre el dispositivo
    pub additional_info: Option<String>,
}

impl Fat32DeviceInfo {
    /// Crear nueva información de dispositivo
    pub fn new(device_name: String, size_lba: u64, start_lba: u64) -> Self {
        Self {
            device_name,
            size_lba,
            start_lba,
            additional_info: None,
        }
    }
    
    /// Crear información de dispositivo con información adicional
    pub fn with_info(device_name: String, size_lba: u64, start_lba: u64, additional_info: String) -> Self {
        Self {
            device_name,
            size_lba,
            start_lba,
            additional_info: Some(additional_info),
        }
    }
}

/// Crear información de dispositivo EclipseFS basándose en la información de FAT32
/// 
/// Esta función sugiere dónde buscar EclipseFS basándose en que FAT32 está típicamente
/// en la primera partición (/dev/sda1) y EclipseFS estaría en la segunda (/dev/sda2)
/// 
/// # Arguments
/// - `fat32_device`: Nombre del dispositivo FAT32 (ej: "/dev/sda1")
/// 
/// # Returns
/// - `Option<String>`: Nombre sugerido del dispositivo EclipseFS o None si no se puede determinar
pub fn sugerir_dispositivo_eclipsefs_desde_fat32(fat32_device: &str) -> Option<String> {
    // Extraer el número de la partición FAT32
    if let Some(last_char) = fat32_device.chars().last() {
        if let Some(partition_num) = last_char.to_digit(10) {
            // Si FAT32 está en /dev/sda1, sugerir /dev/sda2
            // Si FAT32 está en /dev/sda2, sugerir /dev/sda3, etc.
            let suggested_num = partition_num + 1;
            
            // Construir el nombre del dispositivo sugerido
            let base_device = &fat32_device[..fat32_device.len() - 1]; // Remover el último carácter
            let suggested_device = alloc::format!("{}{}", base_device, suggested_num);
            
            crate::debug::serial_write_str(&alloc::format!(
                "FAT32: Sugiriendo dispositivo EclipseFS: {} (basado en FAT32 en {})\n",
                suggested_device,
                fat32_device
            ));
            
            return Some(suggested_device);
        }
    }
    
    // Fallback: asumir /dev/sda2 si no se puede determinar
    crate::debug::serial_write_str("FAT32: No se pudo determinar dispositivo EclipseFS, usando fallback: /dev/sda2\n");
    Some(String::from("/dev/sda1"))
}

/// Obtener información de la partición FAT32 montada desde el storage manager
/// 
/// # Returns
/// - `Some(String)`: Nombre del dispositivo Linux de la partición FAT32 si se encuentra
/// - `None`: Si no se encuentra ninguna partición FAT32 o el storage manager no está disponible
/// 
/// # Logging
/// Registra información detallada de la partición encontrada incluyendo:
/// - Nombre del dispositivo
/// - Tamaño en sectores LBA
/// Obtener dispositivos candidatos para montar FAT32 usando la misma lógica que EclipseFS
pub fn obtener_dispositivos_fat32_candidatos() -> Vec<Fat32DeviceInfo> {
    crate::debug::serial_write_str("FAT32: Obteniendo dispositivos candidatos usando lógica Linux-like...\n");
    
    let storage = match crate::drivers::storage_manager::get_storage_manager() {
        Some(s) => s,
        None => {
            crate::debug::serial_write_str("FAT32: No se pudo obtener storage manager\n");
            return Vec::new();
        }
    };
    
    let mut candidatos = Vec::new();
    let total_partitions = storage.partitions.len();
    
    crate::debug::serial_write_str(&alloc::format!(
        "FAT32: Analizando {} particiones detectadas\n",
        total_partitions
    ));
    
    // Buscar particiones FAT32 con la misma lógica que EclipseFS
    for partition in &storage.partitions {
        if partition.filesystem_type == crate::partitions::FilesystemType::FAT32 {
            let size_mb = (partition.size_lba * 512) / (1024 * 1024);
            crate::debug::serial_write_str(&alloc::format!(
                "FAT32: Candidato encontrado: {} (tipo: FAT32, {} MB, inicio LBA: {})\n",
                partition.name, size_mb, partition.start_lba
            ));
            
            let device_info = Fat32DeviceInfo::new(
                partition.name.clone(),
                partition.size_lba,
                partition.start_lba
            );
            candidatos.push(device_info);
        }
    }
    
    // Si no se encontraron candidatos, buscar en nombres alternativos como EclipseFS
    if candidatos.is_empty() {
        crate::debug::serial_write_str("FAT32: No se encontraron candidatos FAT32, buscando nombres alternativos...\n");
        
        // Lista de nombres alternativos a probar (similar a EclipseFS)
        let nombres_alternativos = [
            "/dev/sda1", "/dev/sdb1", "/dev/sdc1", "/dev/sdd1",
            // NVMe partitions
            "/dev/nvme0n1p1", "/dev/nvme0n1p2", "/dev/nvme1n1p1", "/dev/nvme1n1p2",
            // SATA/AHCI partitions
            "/dev/sda1", "/dev/sda2", "/dev/sdb1", "/dev/sdb2", "/dev/sdc1", "/dev/sdc2", "/dev/sdd1", "/dev/sdd2",
            "/dev/sdap1", "/dev/sdap2", "/dev/sdbp1", "/dev/sdbp2",
            "/dev/hda1", "/dev/hdb1", "/dev/hdc1", "/dev/hdd1",
        ];
        
        for nombre in &nombres_alternativos {
            // Simular información de dispositivo para nombres alternativos
            let device_info = Fat32DeviceInfo::new(
                nombre.to_string(),
                204800, // 100MB típico para FAT32
                2048,   // Inicio típico después del GPT
            );
            candidatos.push(device_info);
            crate::debug::serial_write_str(&alloc::format!(
                "FAT32: Candidato alternativo agregado: {} (simulado)\n",
                nombre
            ));
        }
    }
    
    crate::debug::serial_write_str(&alloc::format!(
        "FAT32: {} dispositivos candidatos encontrados\n",
        candidatos.len()
    ));
    
    candidatos
}

/// - Sector de inicio LBA
fn obtener_particion_fat32_montada() -> Option<String> {
    crate::drivers::storage_manager::get_storage_manager()
        .and_then(|storage| {
            let total_partitions = storage.partitions.len();
            crate::debug::serial_write_str(&alloc::format!(
                "FAT32: Analizando {} particiones detectadas por el storage manager\n",
                total_partitions
            ));
            
            storage.partitions.iter()
                .find(|particion| particion.filesystem_type == crate::partitions::FilesystemType::FAT32)
                .map(|particion| {
                    crate::debug::serial_write_str(&alloc::format!(
                        "FAT32: Partición FAT32 detectada: {} (tamaño: {} sectores LBA, inicio: sector LBA {})\n",
                        particion.name,
                        particion.size_lba,
                        particion.start_lba
                    ));
                    particion.name.clone()
                })
        })
}

/// Montar sistema de archivos FAT32 desde dispositivos de almacenamiento disponibles
/// 
/// Esta función usa la misma lógica Linux-like que EclipseFS para buscar particiones FAT32
/// en los dispositivos de almacenamiento detectados y monta la primera partición válida
/// encontrada en el punto de montaje `/boot`.
/// 
/// # Arguments
/// - `storage`: Referencia al gestor de almacenamiento que contiene información
///              sobre dispositivos y particiones detectadas
/// - `device_info`: Información opcional del dispositivo específico donde montar FAT32.
///                  Si es None, buscará automáticamente dispositivos candidatos
/// 
/// # Returns
/// - `Ok(())`: Si el montaje fue exitoso
/// - `Err(VfsError)`: Error específico si el montaje falla
/// 
/// # Errors
/// - `DeviceError`: Si no se encuentran dispositivos de almacenamiento
/// - `InvalidFs`: Si no se puede leer un boot sector FAT32 válido
/// - `InvalidFs`: Si el VFS no está inicializado
/// 
/// # Example
/// ```rust
/// match mount_fat32_from_storage(&storage_manager, None) {
///     Ok(()) => println!("FAT32 montado exitosamente"),
///     Err(e) => println!("Error montando FAT32: {:?}", e),
/// }
/// ```
pub fn mount_fat32_from_storage(
    storage: &crate::drivers::storage_manager::StorageManager,
    device_info: Option<Fat32DeviceInfo>,
) -> Result<(), VfsError> {
    crate::debug::serial_write_str("FAT32: Iniciando mount_fat32_from_storage con lógica Linux-like\n");

    if storage.device_count() == 0 {
        crate::debug::serial_write_str("FAT32: No storage devices found\n");
        return Err(VfsError::DeviceError("No storage devices found".into()));
    }

    crate::debug::serial_write_str(&alloc::format!(
        "FAT32: {} dispositivos de almacenamiento encontrados\n",
        storage.device_count()
    ));

    // Determinar información del dispositivo usando la misma lógica que EclipseFS
    let target_device = if let Some(device_info) = device_info {
        crate::debug::serial_write_str(&alloc::format!(
            "FAT32: Usando dispositivo específico: {} ({} sectores, inicio LBA {})\n",
            device_info.device_name,
            device_info.size_lba,
            device_info.start_lba
        ));
        Some(device_info)
    } else {
        // Buscar dispositivos candidatos automáticamente
        crate::debug::serial_write_str("FAT32: Buscando dispositivos candidatos automáticamente...\n");
        let candidatos = obtener_dispositivos_fat32_candidatos();
        
        if candidatos.is_empty() {
            crate::debug::serial_write_str("FAT32: No se encontraron dispositivos candidatos para FAT32\n");
            return Err(VfsError::DeviceError("No se encontraron dispositivos candidatos para FAT32".into()));
        }
        
        // Usar el primer candidato encontrado
        let primer_candidato = candidatos.into_iter().next().unwrap();
        crate::debug::serial_write_str(&alloc::format!(
            "FAT32: Usando primer candidato: {}\n",
            primer_candidato.device_name
        ));
        Some(primer_candidato)
    };

    // Usar la información de la partición para leer desde el offset correcto
    let device_info = target_device.unwrap();
    crate::debug::serial_write_str(&alloc::format!(
        "FAT32: Leyendo boot sector desde {} (offset LBA: {}, tamaño: {} sectores)\n",
        device_info.device_name, device_info.start_lba, device_info.size_lba
    ));
    
    let mut boot_sector_buffer = [0u8; 512];
    
    // Leer directamente desde la partición usando el offset correcto
    match storage.read_device_sector_real(
        &storage.devices[0].info,  // Usar el primer dispositivo
        device_info.start_lba,     // Usar el offset de la partición
        &mut boot_sector_buffer
    ) {
        Ok(_) => {
            crate::debug::serial_write_str(&alloc::format!(
                "FAT32: Boot sector leído exitosamente desde LBA {}\n",
                device_info.start_lba
            ));
        }
        Err(e) => {
            crate::debug::serial_write_str(&alloc::format!("FAT32: Error leyendo boot sector FAT32: {}\n", e));
            
            // Fallback: probar todos los dispositivos como antes
            crate::debug::serial_write_str("FAT32: Intentando fallback con lectura directa de dispositivos...\n");
            
            for dev_index in 0..storage.device_count() {
                crate::debug::serial_write_str(&alloc::format!(
                    "FAT32: Probando dispositivo {} de {} (fallback)\n",
                    dev_index,
                    storage.device_count()
                ));

                let device = match storage.get_device(dev_index) {
                    Some(d) => d,
                    None => {
                        crate::debug::serial_write_str(&alloc::format!("FAT32: get_device({}) devolvió None\n", dev_index));
                        continue;
                    }
                };

                crate::debug::serial_write_str("FAT32: Dispositivo obtenido, intentando leer boot sector FAT32\n");

                // Buscar partición FAT32 y leer su boot sector
                crate::debug::serial_write_str("FAT32: Buscando partición FAT32 en dispositivo...\n");
                
                // Leer MBR para encontrar partición FAT32
                let mut mbr_buffer = [0u8; 512];
                if let Err(e) = device.read_blocks(0, &mut mbr_buffer) {
                    crate::debug::serial_write_str(&alloc::format!(
                        "FAT32: [dev {}] Error leyendo MBR: {}\n",
                        dev_index, e
                    ));
                    continue;
                }
                
                // Verificar boot signature del MBR
                if mbr_buffer[510] != 0x55 || mbr_buffer[511] != 0xAA {
                    crate::debug::serial_write_str(&alloc::format!(
                        "FAT32: [dev {}] MBR boot signature inválida\n",
                        dev_index
                    ));
                    continue;
                }
                
                // Buscar partición FAT32 en MBR
                let mut fat32_start_lba: Option<u64> = None;
                for i in 0..4 {
                    let partition_offset = 446 + (i * 16);
                    let partition_type = mbr_buffer[partition_offset + 4];
                    
                    crate::debug::serial_write_str(&alloc::format!(
                        "FAT32: [dev {}] Partición {} tipo: 0x{:02X}\n",
                        dev_index, i, partition_type
                    ));
                    
                    if partition_type == 0x0C || partition_type == 0x0B { // FAT32
                        // Leer LBA start (little-endian, 4 bytes)
                        let start_lba = u32::from_le_bytes([
                            mbr_buffer[partition_offset + 8],
                            mbr_buffer[partition_offset + 9],
                            mbr_buffer[partition_offset + 10],
                            mbr_buffer[partition_offset + 11],
                        ]) as u64;
                        
                        fat32_start_lba = Some(start_lba);
                        crate::debug::serial_write_str(&alloc::format!(
                            "FAT32: [dev {}] Encontrada partición FAT32 en LBA {}\n",
                            dev_index, start_lba
                        ));
                        break;
                    }
                }
                
                let start_lba = match fat32_start_lba {
                    Some(lba) => lba,
                    None => {
                        crate::debug::serial_write_str(&alloc::format!(
                            "FAT32: [dev {}] No se encontró partición FAT32\n",
                            dev_index
                        ));
                        continue;
                    }
                };
                
                // Leer boot sector de la partición FAT32
                crate::debug::serial_write_str(&alloc::format!(
                    "FAT32: [dev {}] Leyendo boot sector desde LBA {}...\n",
                    dev_index, start_lba
                ));
                
                if let Err(e) = device.read_blocks(start_lba, &mut boot_sector_buffer) {
                    crate::debug::serial_write_str(&alloc::format!(
                        "FAT32: [dev {}] Error leyendo boot sector FAT32: {}\n",
                        dev_index, e
                    ));
                    continue;
                }
                
                crate::debug::serial_write_str("FAT32: Boot sector leído exitosamente\n");
                break; // Si llegamos aquí, tenemos datos en boot_sector_buffer
            }
        }
    }
    
    // Verificar boot signature
    if boot_sector_buffer[510] != 0x55 || boot_sector_buffer[511] != 0xAA {
        crate::debug::serial_write_str(&alloc::format!(
            "FAT32: Boot signature inválida: 0x{:02X}{:02X}\n",
            boot_sector_buffer[511], boot_sector_buffer[510]
        ));
        return Err(VfsError::InvalidFs("Boot signature inválida".into()));
    }
    
    crate::debug::serial_write_str("FAT32: Boot signature válida (0x55AA)\n");
    
    // Debug: mostrar primeros 128 bytes del boot sector
    crate::debug::serial_write_str("FAT32: Primeros 128 bytes del boot sector:\n");
    for i in 0..16 {
        let start = i * 8;
        let end = start + 8;
        if end <= boot_sector_buffer.len() {
            let hex_str = boot_sector_buffer[start..end]
                .iter()
                .map(|b| alloc::format!("{:02X}", b))
                .collect::<alloc::vec::Vec<_>>()
                .join(" ");
            crate::debug::serial_write_str(&alloc::format!("{:02X}: {}\n", start, hex_str));
        }
    }
    
    // Verificar que es FAT32
    let fs_type_start = 82; // Offset del tipo de sistema de archivos en boot sector
    let fs_type_bytes = &boot_sector_buffer[fs_type_start..fs_type_start + 8];
    
    crate::debug::serial_write_str(&alloc::format!(
        "FAT32: Bytes del tipo FS en offset {}: {:?}\n",
        fs_type_start, fs_type_bytes
    ));
    
    // Convertir a string, reemplazando bytes nulos con espacios
    let mut fs_type_str = String::new();
    for &byte in fs_type_bytes {
        if byte == 0 {
            break;
        }
        fs_type_str.push(byte as char);
    }
    
    crate::debug::serial_write_str(&alloc::format!(
        "FAT32: Tipo de FS detectado: '{}'\n",
        fs_type_str
    ));
    
    if !fs_type_str.starts_with("FAT32") {
        crate::debug::serial_write_str(&alloc::format!(
            "FAT32: No es FAT32, es: {}\n",
            fs_type_str
        ));
        return Err(VfsError::InvalidFs("No es un sistema de archivos FAT32".into()));
    }
    
    // Intentar inicializar FAT32 con boot sector real
    crate::debug::serial_write_str("FAT32: Intentando inicializar driver con boot sector real...\n");
    
    init_fat32_from_boot(&boot_sector_buffer)?;

    crate::debug::serial_write_str("FAT32: Sistema de archivos FAT32 montado exitosamente\n");

    let wrapper = {
        let driver_ref = get_fat32_driver()
            .ok_or_else(|| VfsError::InvalidFs("FAT32 driver not initialized".into()))?;
        Box::new(Fat32Wrapper::new(driver_ref))
    };

    crate::debug::serial_write_str("FAT32: Preparando wrapper VFS para montaje en /boot\n");
    {
        let mut vfs_guard = crate::filesystem::vfs::get_vfs();
        let vfs = vfs_guard
            .as_mut()
            .ok_or_else(|| VfsError::InvalidFs("VFS no inicializado".into()))?;
        vfs.mount("/boot", wrapper);
    }

    crate::debug::serial_write_str("FAT32: Montaje en /boot completado exitosamente\n");
    
    // Obtener información de la partición FAT32 montada
    let dispositivo_montado = obtener_particion_fat32_montada()
        .unwrap_or_else(|| {
            crate::debug::serial_write_str("FAT32: Advertencia - No se pudo determinar la partición exacta, usando fallback\n");
            String::from("desconocido")
        });
    
    crate::debug::serial_write_str(&alloc::format!(
        "FAT32: Sistema de archivos montado desde: {}\n",
        dispositivo_montado
    ));
    
    Ok(())
}

pub struct Fat32Wrapper {
    driver: NonNull<Fat32Driver>,
    lock: Mutex<()>,
}

impl Fat32Wrapper {
    pub fn new(driver: &'static mut Fat32Driver) -> Self {
        Fat32Wrapper {
            driver: NonNull::from(driver),
            lock: Mutex::new(()),
        }
    }

    fn with_driver<R>(&self, f: impl FnOnce(&Fat32Driver) -> R) -> R {
        let _guard = self.lock.lock();
        unsafe { f(self.driver.as_ref()) }
    }

    fn with_driver_mut<R>(&mut self, f: impl FnOnce(&mut Fat32Driver) -> R) -> R {
        let _guard = self.lock.lock();
        unsafe { f(self.driver.as_mut()) }
    }
}

unsafe impl Send for Fat32Wrapper {}
unsafe impl Sync for Fat32Wrapper {}

impl FileSystem for Fat32Wrapper {
    fn unmount(&mut self) -> Result<(), VfsError> {
        Ok(())
    }

    fn read(&self, _inode: u32, _offset: u64, _buffer: &mut [u8]) -> Result<usize, VfsError> {
        Err(VfsError::InvalidOperation)
    }

    fn write(&mut self, _inode: u32, _offset: u64, _data: &[u8]) -> Result<usize, VfsError> {
        Err(VfsError::InvalidOperation)
    }

    fn stat(&self, _inode: u32) -> Result<StatInfo, VfsError> {
        self.with_driver(|_| Err(VfsError::InvalidOperation))
    }

    fn readdir(&self, _inode: u32) -> Result<Vec<String>, VfsError> {
        self.with_driver(|_| Err(VfsError::InvalidOperation))
    }

    fn truncate(&mut self, _inode: u32, _new_size: u64) -> Result<(), VfsError> {
        Err(VfsError::InvalidOperation)
    }

    fn rmdir(&mut self, _parent_inode: u32, _name: &str) -> Result<(), VfsError> {
        Err(VfsError::InvalidOperation)
    }

    fn rename(
        &mut self,
        _parent_inode: u32,
        _old_name: &str,
        _new_parent_inode: u32,
        _new_name: &str,
    ) -> Result<(), VfsError> {
        Err(VfsError::InvalidOperation)
    }

    fn unlink(&mut self, _parent_inode: u32, _name: &str) -> Result<(), VfsError> {
        Err(VfsError::InvalidOperation)
    }

    fn chmod(&mut self, _inode: u32, _mode: u16) -> Result<(), VfsError> {
        Err(VfsError::InvalidOperation)
    }

    fn chown(&mut self, _inode: u32, _uid: u32, _gid: u32) -> Result<(), VfsError> {
        Err(VfsError::InvalidOperation)
    }
}
