//! Parser para tablas de particiones GPT (GUID Partition Table)

use crate::debug::serial_write_str;
use super::{Partition, PartitionTable, PartitionTableType, FilesystemType};
use alloc::string::ToString;

/// Signature GPT
const GPT_SIGNATURE: &[u8] = b"EFI PART";

/// Tamaño del sector
const SECTOR_SIZE: usize = 512;

/// Header GPT
#[repr(C, packed)]
struct GptHeader {
    signature: [u8; 8],           // "EFI PART"
    revision: u32,                // 0x00010000
    header_size: u32,             // Tamaño del header
    header_crc32: u32,            // CRC32 del header
    reserved: u32,                // Reservado
    current_lba: u64,             // LBA del header actual
    backup_lba: u64,              // LBA del header de respaldo
    first_usable_lba: u64,        // Primer LBA usable
    last_usable_lba: u64,         // Último LBA usable
    disk_guid: [u8; 16],          // GUID del disco
    partition_entry_lba: u64,     // LBA de la primera entrada de partición
    num_partition_entries: u32,   // Número de entradas de partición
    partition_entry_size: u32,    // Tamaño de cada entrada de partición
    partition_array_crc32: u32,   // CRC32 del array de particiones
}

/// Entrada de partición GPT
#[repr(C, packed)]
struct GptPartitionEntry {
    partition_type_guid: [u8; 16], // GUID del tipo de partición
    unique_partition_guid: [u8; 16], // GUID único de la partición
    starting_lba: u64,             // LBA de inicio
    ending_lba: u64,               // LBA de fin
    attributes: u64,               // Atributos de la partición
    partition_name: [u16; 36],     // Nombre de la partición (UTF-16)
}

/// GUIDs de tipos de partición conocidos
const GPT_PARTITION_TYPE_GUID_FAT32: [u8; 16] = [
    0xEB, 0xD0, 0xA0, 0xA2, 0xB9, 0xE5, 0x44, 0x33,
    0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7
];

const GPT_PARTITION_TYPE_GUID_NTFS: [u8; 16] = [
    0xE3, 0x9E, 0xE9, 0x28, 0x32, 0x0B, 0xE3, 0x11,
    0xD0, 0x9D, 0x69, 0x00, 0xA0, 0xC9, 0x3E, 0xC8
];

const GPT_PARTITION_TYPE_GUID_LINUX_FILESYSTEM: [u8; 16] = [
    0x0F, 0xC6, 0x3D, 0xAF, 0x84, 0x83, 0x47, 0x72,
    0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4
];

const GPT_PARTITION_TYPE_GUID_LINUX_SWAP: [u8; 16] = [
    0x06, 0x5D, 0xFD, 0x6D, 0xA4, 0xAB, 0x43, 0xC4,
    0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F
];

const GPT_PARTITION_TYPE_GUID_EFI_SYSTEM: [u8; 16] = [
    0xC1, 0x2A, 0x73, 0x28, 0xF8, 0x1F, 0x11, 0xD2,
    0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B
];

/// Parsear tabla de particiones GPT
pub fn parse_gpt(device: &mut dyn crate::partitions::BlockDevice) -> Result<PartitionTable, &'static str> {
    serial_write_str("GPT: Iniciando parseo de tabla GPT\n");
    
    // Leer el primer sector (MBR con GPT signature)
    let mut mbr_sector = [0u8; SECTOR_SIZE];
    device.read_block(0, &mut mbr_sector)?;
    
    // Verificar que el sector 1 tenga la signature GPT
    let mut gpt_header_sector = [0u8; SECTOR_SIZE];
    device.read_block(1, &mut gpt_header_sector)?;
    
    // Verificar signature GPT
    if &gpt_header_sector[0..8] != GPT_SIGNATURE {
        return Err("Signature GPT no encontrada");
    }
    
    serial_write_str("GPT: Signature GPT encontrada\n");
    
    // Parsear header GPT
    let header = unsafe {
        core::ptr::read(gpt_header_sector.as_ptr() as *const GptHeader)
    };
    
    // Verificar revision
    if header.revision != 0x00010000 {
        serial_write_str("GPT: Advertencia: Revision GPT no estándar\n");
    }
    
    serial_write_str("GPT: Header GPT parseado\n");
    
    // Crear tabla de particiones
    let mut partition_table = PartitionTable::new(PartitionTableType::GPT);
    
    // Leer entradas de partición
    let num_entries = header.num_partition_entries;
    let entry_size = header.partition_entry_size as usize;
    let entries_per_sector = SECTOR_SIZE / entry_size;
    let total_sectors = (num_entries as usize + entries_per_sector - 1) / entries_per_sector;
    
    serial_write_str("GPT: Leyendo entradas de partición\n");
    
        for sector_offset in 0..total_sectors {
            let lba = header.partition_entry_lba + sector_offset as u64;
            let mut sector = [0u8; SECTOR_SIZE];
            device.read_block(lba, &mut sector)?;
            
            for entry_offset in 0..entries_per_sector {
                let entry_index = sector_offset * entries_per_sector + entry_offset;
                if entry_index >= num_entries as usize {
                    break;
                }
                
                let entry_ptr = unsafe { sector.as_ptr().add(entry_offset * entry_size) };
                let entry = unsafe {
                    core::ptr::read(entry_ptr as *const GptPartitionEntry)
                };
            
            // Verificar si la entrada está vacía (GUID tipo cero)
            if is_zero_guid(&entry.partition_type_guid) {
                continue;
            }
            
            // Crear partición
            let start_lba = entry.starting_lba;
            let end_lba = entry.ending_lba;
            let size_lba = if end_lba >= start_lba {
                end_lba - start_lba + 1
            } else {
                continue; // Entrada inválida
            };
            
            let fs_type = determine_filesystem_type(&entry.partition_type_guid);
            // Crear copia local del nombre para evitar problemas de alineación
            let mut name_copy = [0u16; 36];
            unsafe {
                core::ptr::copy_nonoverlapping(
                    core::ptr::addr_of!(entry.partition_name) as *const u16,
                    name_copy.as_mut_ptr(),
                    36
                );
            }
            let descriptive_name = decode_utf16_name(&name_copy);
            
            let mut partition = Partition::new(start_lba, size_lba, fs_type.to_partition_code());
            partition.filesystem_type = fs_type;
            // Generar nombre de dispositivo Linux (será sobrescrito por el storage_manager)
            partition.name = alloc::format!("Partition {}", entry_index + 1);
            partition.guid = Some(entry.unique_partition_guid);
            partition.attributes = entry.attributes;
            
            serial_write_str("GPT: Partición encontrada - ");
            serial_write_str(&partition.name);
            serial_write_str(" (");
            serial_write_str(filesystem_type_to_string(fs_type));
            serial_write_str(")\n");
            
            partition_table.add_partition(partition);
        }
    }
    
    serial_write_str("GPT: Parseo completado - ");
    serial_write_decimal(partition_table.count() as u64);
    serial_write_str(" particiones encontradas\n");
    
    Ok(partition_table)
}

/// Verificar si un GUID está vacío (todos ceros)
fn is_zero_guid(guid: &[u8; 16]) -> bool {
    guid.iter().all(|&b| b == 0)
}

/// Determinar tipo de sistema de archivos desde GUID de tipo de partición
fn determine_filesystem_type(type_guid: &[u8; 16]) -> FilesystemType {
    if type_guid == &GPT_PARTITION_TYPE_GUID_FAT32 {
        FilesystemType::FAT32
    } else if type_guid == &GPT_PARTITION_TYPE_GUID_NTFS {
        FilesystemType::NTFS
    } else if type_guid == &GPT_PARTITION_TYPE_GUID_LINUX_FILESYSTEM {
        FilesystemType::Ext4 // Asumir Ext4 para Linux
    } else if type_guid == &GPT_PARTITION_TYPE_GUID_LINUX_SWAP {
        FilesystemType::LinuxSwap
    } else if type_guid == &GPT_PARTITION_TYPE_GUID_EFI_SYSTEM {
        FilesystemType::EFISystem
    } else {
        // Verificar si podría ser EclipseFS (usar heurística)
        if is_potential_eclipsefs_guid(type_guid) {
            FilesystemType::EclipseFS
        } else {
            FilesystemType::Unknown
        }
    }
}

/// Verificar si un GUID podría ser EclipseFS
fn is_potential_eclipsefs_guid(type_guid: &[u8; 16]) -> bool {
    // EclipseFS podría usar un GUID personalizado
    // Por ahora, asumir que cualquier GUID no reconocido podría ser EclipseFS
    // En el futuro, se podría definir un GUID específico para EclipseFS
    !is_zero_guid(type_guid) && 
    type_guid != &GPT_PARTITION_TYPE_GUID_FAT32 &&
    type_guid != &GPT_PARTITION_TYPE_GUID_NTFS &&
    type_guid != &GPT_PARTITION_TYPE_GUID_LINUX_FILESYSTEM &&
    type_guid != &GPT_PARTITION_TYPE_GUID_LINUX_SWAP &&
    type_guid != &GPT_PARTITION_TYPE_GUID_EFI_SYSTEM
}

/// Decodificar nombre UTF-16 de la partición
fn decode_utf16_name(utf16_name: &[u16; 36]) -> alloc::string::String {
    // Crear una copia local para evitar problemas de alineación
    let mut name_copy = [0u16; 36];
    unsafe {
        core::ptr::copy_nonoverlapping(utf16_name.as_ptr(), name_copy.as_mut_ptr(), 36);
    }
    let mut result = alloc::string::String::new();
    
    for &ch in name_copy.iter() {
        if ch == 0 {
            break; // Fin de string
        }
        
        // Convertir UTF-16 a UTF-8 (simplificado)
        if ch <= 0x7F {
            result.push(ch as u8 as char);
        } else if ch <= 0x7FF {
            result.push(((ch >> 6) | 0xC0) as u8 as char);
            result.push(((ch & 0x3F) | 0x80) as u8 as char);
        } else {
            result.push(((ch >> 12) | 0xE0) as u8 as char);
            result.push((((ch >> 6) & 0x3F) | 0x80) as u8 as char);
            result.push(((ch & 0x3F) | 0x80) as u8 as char);
        }
    }
    
    if result.is_empty() {
        result = "Sin nombre".to_string();
    }
    
    result
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
