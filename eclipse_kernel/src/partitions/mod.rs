//! Sistema de detección y manejo de particiones

use crate::debug::serial_write_str;
use alloc::vec::Vec;

pub mod gpt;
pub mod mbr;

/// Tipo de sistema de archivos
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FilesystemType {
    Unknown,
    FAT12,
    FAT16,
    FAT32,
    NTFS,
    Ext2,
    Ext3,
    Ext4,
    EclipseFS,
    LinuxSwap,
    EFISystem,
}

impl FilesystemType {
    /// Obtener tipo de sistema de archivos desde código de partición
    pub fn from_partition_code(code: u8) -> Self {
        match code {
            0x01 => FilesystemType::FAT12,
            0x04 => FilesystemType::FAT16,
            0x06 => FilesystemType::FAT16,
            0x07 => FilesystemType::NTFS,
            0x0B => FilesystemType::FAT32,
            0x0C => FilesystemType::FAT32,
            0x0E => FilesystemType::FAT16,
            0x0F => FilesystemType::FAT32,
            0x82 => FilesystemType::LinuxSwap,
            0x83 => FilesystemType::Ext2, // Podría ser Ext2/3/4
            0xEF => FilesystemType::EFISystem,
            _ => FilesystemType::Unknown,
        }
    }

    /// Obtener código de partición desde tipo de sistema de archivos
    pub fn to_partition_code(self) -> u8 {
        match self {
            FilesystemType::FAT12 => 0x01,
            FilesystemType::FAT16 => 0x06,
            FilesystemType::FAT32 => 0x0C,
            FilesystemType::NTFS => 0x07,
            FilesystemType::Ext2 | FilesystemType::Ext3 | FilesystemType::Ext4 => 0x83,
            FilesystemType::LinuxSwap => 0x82,
            FilesystemType::EFISystem => 0xEF,
            FilesystemType::EclipseFS => 0x83, // Usar código Linux como fallback
            FilesystemType::Unknown => 0x00,
        }
    }
}

/// Información de una partición
#[derive(Debug, Clone)]
pub struct Partition {
    pub start_lba: u64,
    pub size_lba: u64,
    pub partition_type: u8,
    pub filesystem_type: FilesystemType,
    pub name: alloc::string::String,
    pub guid: Option<[u8; 16]>, // Para GPT
    pub attributes: u64, // Para GPT
}

impl Partition {
    pub fn new(start_lba: u64, size_lba: u64, partition_type: u8) -> Self {
        Self {
            start_lba,
            size_lba,
            partition_type,
            filesystem_type: FilesystemType::from_partition_code(partition_type),
            name: alloc::string::String::new(),
            guid: None,
            attributes: 0,
        }
    }

    /// Verificar si la partición es válida
    pub fn is_valid(&self) -> bool {
        self.size_lba > 0 && self.start_lba > 0
    }

    /// Obtener el último LBA de la partición
    pub fn end_lba(&self) -> u64 {
        self.start_lba + self.size_lba - 1
    }
}

/// Tabla de particiones
#[derive(Debug, Clone)]
pub struct PartitionTable {
    pub partitions: Vec<Partition>,
    pub table_type: PartitionTableType,
}

/// Tipo de tabla de particiones
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PartitionTableType {
    MBR,
    GPT,
    Unknown,
}

impl PartitionTable {
    pub fn new(table_type: PartitionTableType) -> Self {
        Self {
            partitions: Vec::new(),
            table_type,
        }
    }

    /// Agregar una partición a la tabla
    pub fn add_partition(&mut self, partition: Partition) {
        if partition.is_valid() {
            self.partitions.push(partition);
        }
    }

    /// Buscar particiones por tipo de sistema de archivos
    pub fn find_partitions_by_fs_type(&self, fs_type: FilesystemType) -> Vec<&Partition> {
        self.partitions
            .iter()
            .filter(|p| p.filesystem_type == fs_type)
            .collect()
    }

    /// Obtener la primera partición de un tipo específico
    pub fn find_first_partition_by_fs_type(&self, fs_type: FilesystemType) -> Option<&Partition> {
        self.partitions
            .iter()
            .find(|p| p.filesystem_type == fs_type)
    }

    /// Obtener el número total de particiones
    pub fn count(&self) -> usize {
        self.partitions.len()
    }

    /// Verificar si hay particiones
    pub fn is_empty(&self) -> bool {
        self.partitions.is_empty()
    }
}

/// Trait para dispositivos de bloque
pub trait BlockDevice {
    /// Leer un bloque del dispositivo
    fn read_block(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), &'static str>;
    
    /// Escribir un bloque al dispositivo
    fn write_block(&mut self, lba: u64, buffer: &[u8]) -> Result<(), &'static str>;
    
    /// Obtener el tamaño del bloque
    fn block_size(&self) -> usize;
    
    /// Obtener el número total de bloques
    fn total_blocks(&self) -> u64;
}

/// Parsear tabla de particiones desde un dispositivo
pub fn parse_partition_table(device: &mut dyn BlockDevice) -> Result<PartitionTable, &'static str> {
    serial_write_str("PARTITIONS: Iniciando detección de tabla de particiones\n");
    
    // Intentar primero GPT
    match gpt::parse_gpt(device) {
        Ok(gpt_table) => {
            serial_write_str("PARTITIONS: Tabla GPT detectada\n");
            return Ok(gpt_table);
        }
        Err(e) => {
            serial_write_str("PARTITIONS: Error parseando GPT: ");
            serial_write_str(e);
            serial_write_str("\n");
        }
    }
    
    // Fallback a MBR
    match mbr::parse_mbr(device) {
        Ok(mbr_table) => {
            serial_write_str("PARTITIONS: Tabla MBR detectada\n");
            return Ok(mbr_table);
        }
        Err(e) => {
            serial_write_str("PARTITIONS: Error parseando MBR: ");
            serial_write_str(e);
            serial_write_str("\n");
        }
    }
    
    Err("No se pudo detectar tabla de particiones válida")
}

/// Buscar particiones EclipseFS en un dispositivo
pub fn find_eclipsefs_partitions(device: &mut dyn BlockDevice) -> Result<Vec<Partition>, &'static str> {
    serial_write_str("PARTITIONS: Buscando particiones EclipseFS\n");
    
    let partition_table = parse_partition_table(device)?;
    let eclipsefs_partitions = partition_table.find_partitions_by_fs_type(FilesystemType::EclipseFS);
    
    if eclipsefs_partitions.is_empty() {
        serial_write_str("PARTITIONS: No se encontraron particiones EclipseFS\n");
        return Ok(Vec::new());
    }
    
    serial_write_str("PARTITIONS: Particiones EclipseFS encontradas: ");
    serial_write_decimal(eclipsefs_partitions.len() as u64);
    serial_write_str("\n");
    
    Ok(eclipsefs_partitions.into_iter().cloned().collect())
}

/// Función auxiliar para escribir números decimales
fn serial_write_decimal(mut num: u64) {
    if num == 0 {
        serial_write_str("0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 0;
    while num > 0 {
        buf[i] = b'0' + (num % 10) as u8;
        num /= 10;
        i += 1;
    }
    for j in (0..i).rev() {
        unsafe {
            while x86_64::instructions::port::Port::<u8>::new(0x3F8 + 5).read() & 0x20 == 0 {}
            x86_64::instructions::port::Port::<u8>::new(0x3F8).write(buf[j]);
        }
    }
}
