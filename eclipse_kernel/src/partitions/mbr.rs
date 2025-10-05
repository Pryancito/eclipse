//! Parser para tablas de particiones MBR (Master Boot Record)

use crate::debug::serial_write_str;
use super::{Partition, PartitionTable, PartitionTableType, FilesystemType};

/// Tamaño del sector
const SECTOR_SIZE: usize = 512;

/// Signature MBR
const MBR_SIGNATURE: u16 = 0xAA55;

/// Entrada de partición MBR
#[repr(C, packed)]
struct MbrPartitionEntry {
    status: u8,                   // Estado de la partición
    first_chs: [u8; 3],          // Primer sector CHS
    partition_type: u8,           // Tipo de partición
    last_chs: [u8; 3],           // Último sector CHS
    first_lba: u32,              // Primer LBA
    num_sectors: u32,            // Número de sectores
}

/// Parsear tabla de particiones MBR
pub fn parse_mbr(device: &mut dyn crate::partitions::BlockDevice) -> Result<PartitionTable, &'static str> {
    serial_write_str("MBR: Iniciando parseo de tabla MBR\n");
    
    // Leer el primer sector (MBR)
    let mut mbr_sector = [0u8; SECTOR_SIZE];
    device.read_block(0, &mut mbr_sector)?;
    
    // Verificar signature MBR
    let signature = u16::from_le_bytes([mbr_sector[510], mbr_sector[511]]);
    if signature != MBR_SIGNATURE {
        return Err("Signature MBR no encontrada");
    }
    
    serial_write_str("MBR: Signature MBR encontrada\n");
    
    // Crear tabla de particiones
    let mut partition_table = PartitionTable::new(PartitionTableType::MBR);
    
    // Parsear entradas de partición (4 entradas en MBR)
    for i in 0..4 {
        let entry_offset = 446 + (i * 16); // Offset de la entrada en el MBR
        let entry_ptr = unsafe { mbr_sector.as_ptr().add(entry_offset) };
        let entry = unsafe {
            core::ptr::read(entry_ptr as *const MbrPartitionEntry)
        };
        
        // Verificar si la entrada está vacía (tipo 0)
        if entry.partition_type == 0 {
            continue;
        }
        
        // Verificar si la entrada es válida (status 0x80 o 0x00)
        if entry.status != 0x80 && entry.status != 0x00 {
            continue;
        }
        
        let start_lba = entry.first_lba as u64;
        let size_lba = entry.num_sectors as u64;
        
        // Verificar que la partición sea válida
        if size_lba == 0 || start_lba == 0 {
            continue;
        }
        
        // Crear partición
        let fs_type = FilesystemType::from_partition_code(entry.partition_type);
        let mut partition = Partition::new(start_lba, size_lba, entry.partition_type);
        partition.filesystem_type = fs_type;
        partition.name = alloc::format!("Partition {}", i + 1);
        
        let partition_name = partition.name.clone();
        
        serial_write_str("MBR: Partición ");
        serial_write_decimal((i + 1) as u64);
        serial_write_str(" encontrada - ");
        serial_write_str(&partition_name);
        serial_write_str(" (");
        serial_write_str(filesystem_type_to_string(fs_type));
        serial_write_str(")\n");
        
        partition_table.add_partition(partition);
    }
    
    serial_write_str("MBR: Parseo completado - ");
    serial_write_decimal(partition_table.count() as u64);
    serial_write_str(" particiones encontradas\n");
    
    Ok(partition_table)
}

/// Convertir tipo de sistema de archivos a string
fn filesystem_type_to_string(fs_type: FilesystemType) -> &'static str {
    match fs_type {
        FilesystemType::FAT12 => "FAT12",
        FilesystemType::FAT16 => "FAT16",
        FilesystemType::FAT32 => "FAT32",
        FilesystemType::NTFS => "NTFS",
        FilesystemType::Ext2 => "Ext2",
        FilesystemType::Ext3 => "Ext3",
        FilesystemType::Ext4 => "Ext4",
        FilesystemType::EclipseFS => "EclipseFS",
        FilesystemType::LinuxSwap => "Linux Swap",
        FilesystemType::EFISystem => "EFI System",
        FilesystemType::Unknown => "Unknown",
    }
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
