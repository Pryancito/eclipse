//! Wrapper VFS para la librería EclipseFS.

use crate::bootloader_data;
use crate::drivers::storage_manager::StorageManager;
use crate::filesystem::vfs::{get_vfs, init_vfs, FileSystem, StatInfo, VfsError};
use eclipsefs_lib::{format::constants as ecfs_constants, EclipseFSError, EclipseFSHeader, InodeTableEntry};
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::any::Any;
use core::cmp;

const HEADER_SIZE_BYTES: usize = 4096; // 8 sectores (header real)
const HEADER_SIZE_BLOCKS: u64 = (HEADER_SIZE_BYTES / 512) as u64;
static mut FS_BUFFER: [u8; HEADER_SIZE_BYTES] = [0u8; HEADER_SIZE_BYTES];
static mut BOOT_SECTOR: [u8; 512] = [0u8; 512];

pub struct EclipseFSWrapper {
    pub fs: Box<eclipsefs_lib::EclipseFS>,
}

impl EclipseFSWrapper {
    pub fn new(fs: eclipsefs_lib::EclipseFS) -> Self {
        Self { fs: Box::new(fs) }
        }

    pub fn as_any(&self) -> &dyn Any {
        self
    }
}

pub fn mount_root_fs_from_storage(storage: &StorageManager) -> Result<(), VfsError> {
    unsafe {
        use x86_64::instructions::port::Port;
        let mut status_port: Port<u8> = Port::new(0x3F8 + 5);
        let mut data_port: Port<u8> = Port::new(0x3F8);
        let msg = b"ECLIPSEFS: entrada directa\n";
        for &byte in msg {
            while status_port.read() & 0x20 == 0 {}
            data_port.write(byte);
        }
    }

    crate::debug::serial_write_str("ECLIPSEFS: mount_root_fs_from_storage() -> inicio\n");
    crate::debug::serial_write_str("ECLIPSEFS: checkpoint A\n");

    let device_count = storage.device_count();
    crate::debug::serial_write_str("ECLIPSEFS: (root) device_count = ");
    serial_write_decimal(device_count as u64);
    crate::debug::serial_write_str("\n");

    crate::debug::serial_write_str("ECLIPSEFS: (root) verificando device_count\n");
    if device_count == 0 {
        crate::debug::serial_write_str("ECLIPSEFS: No storage devices found\n");
        return Err(VfsError::DeviceError("No storage devices found".into()));
    }
    crate::debug::serial_write_str("ECLIPSEFS: (root) device_count OK\n");
    crate::debug::serial_write_str("ECLIPSEFS: dispositivos de almacenamiento encontrados\n");

    crate::debug::serial_write_str("ECLIPSEFS: (root) preparando buffers estaticos\n");
    let fs_buffer = unsafe {
        FS_BUFFER.fill(0);
        &mut FS_BUFFER
    };
    let boot_sector = unsafe {
        BOOT_SECTOR.fill(0);
        &mut BOOT_SECTOR
    };
    crate::debug::serial_write_str("ECLIPSEFS: (root) buffers listos\n");

    crate::debug::serial_write_str("ECLIPSEFS: (root) llamando a read_eclipsefs_boot_sector\n");
    let partition_index = storage
        .read_eclipsefs_boot_sector(boot_sector)
        .map_err(|e| {
            crate::debug::serial_write_str("ECLIPSEFS: Error leyendo boot sector de EclipseFS: ");
            crate::debug::serial_write_str(&alloc::format!("{}", e));
            crate::debug::serial_write_str("\n");
            VfsError::DeviceError(e.into())
        })?;
    crate::debug::serial_write_str("ECLIPSEFS: (root) read_eclipsefs_boot_sector OK\n");

    // Copiar el boot sector al buffer principal del superblock
    fs_buffer[0..512].copy_from_slice(boot_sector);

    crate::debug::serial_write_str("ECLIPSEFS: Boot sector leído desde partición ");
    serial_write_decimal(partition_index as u64);
    crate::debug::serial_write_str("\n");

    crate::debug::serial_write_str("ECLIPSEFS: (root) Boot sector leído. Primeros 32 bytes: ");
    for &byte in &boot_sector[0..32] {
        serial_write_hex_byte(byte);
        crate::debug::serial_write_str(" ");
    }
    crate::debug::serial_write_str("\n");

    crate::debug::serial_write_str("ECLIPSEFS: Solicitando get_partition_info()\n");
    let partition_info = storage
        .get_partition_info(partition_index)
        .map_err(|e| {
            crate::debug::serial_write_str("ECLIPSEFS: No se pudo obtener información de la partición ");
            serial_write_decimal(partition_index as u64);
            crate::debug::serial_write_str(": ");
            crate::debug::serial_write_str(&alloc::format!("{}", e));
            crate::debug::serial_write_str("\n");
            VfsError::InvalidFs(e.into())
        })?;
    crate::debug::serial_write_str("ECLIPSEFS: (root) Información de partición obtenida\n");

    crate::debug::serial_write_str("ECLIPSEFS: Partición offset LBA inicial = ");
    serial_write_decimal(partition_info.start_sector as u64);
    crate::debug::serial_write_str(", tamaño en bloques = ");
    serial_write_decimal(partition_info.size_sectors as u64);
    crate::debug::serial_write_str("\n");
    
    crate::debug::serial_write_str("ECLIPSEFS: Leyendo bloques adicionales del superblock\n");
    for block in 1..HEADER_SIZE_BLOCKS {
        crate::debug::serial_write_str("ECLIPSEFS: Leyendo bloque ");
        serial_write_decimal(block);
        crate::debug::serial_write_str(" de la partición ");
        serial_write_decimal(partition_index as u64);
        crate::debug::serial_write_str(" (LBA ");
        serial_write_decimal(block);
        crate::debug::serial_write_str(")\n");

        let offset = (block as usize) * 512;
            let slice = &mut fs_buffer[offset..offset + 512];
        storage
            .read_from_partition(partition_index, block, slice)
            .map_err(|e| {
                crate::debug::serial_write_str("ECLIPSEFS: Error leyendo bloque ");
                serial_write_decimal(block);
                crate::debug::serial_write_str(" de la partición ");
                serial_write_decimal(partition_index as u64);
                crate::debug::serial_write_str(": ");
                crate::debug::serial_write_str(&alloc::format!("{}", e));
                crate::debug::serial_write_str("\n");
                VfsError::DeviceError(e.into())
            })?;

        crate::debug::serial_write_str("ECLIPSEFS: (root) Superblock adicional leído\n");
    }

    crate::debug::serial_write_str("ECLIPSEFS: Todos los bloques del superblock leídos\n");

    crate::debug::serial_write_str("ECLIPSEFS: Primeros 32 bytes del superblock: ");
    for &byte in &fs_buffer[0..32] {
        serial_write_hex_byte(byte);
        crate::debug::serial_write_str(" ");
    }
    crate::debug::serial_write_str("\n");

    crate::debug::serial_write_str("ECLIPSEFS: (root) Validando número mágico...\n");
    if fs_buffer.len() < 16 {
        crate::debug::serial_write_str("ECLIPSEFS: Header demasiado pequeño, abortando\n");
        return Err(VfsError::InvalidFs("Header EclipseFS demasiado pequeño".into()));
    }

    // Validar el número mágico usando eclipsefs-lib
    let magic_ascii = &fs_buffer[0..9];
    crate::debug::serial_write_str("ECLIPSEFS: Magic leído: ");
    for &byte in magic_ascii {
        serial_write_hex_byte(byte);
        crate::debug::serial_write_str(" ");
    }
    crate::debug::serial_write_str("\n");
    crate::debug::serial_write_str("ECLIPSEFS: Magic esperado: ");
    for &byte in eclipsefs_lib::format::ECLIPSEFS_MAGIC {
        serial_write_hex_byte(byte);
        crate::debug::serial_write_str(" ");
    }
    crate::debug::serial_write_str("\n");
    
    if magic_ascii != eclipsefs_lib::format::ECLIPSEFS_MAGIC {
        crate::debug::serial_write_str("ECLIPSEFS: Magic inválido en superblock (esperado 'ECLIPSEFS')\n");
        return Err(VfsError::InvalidFs("Magic number inválido para EclipseFS".into()));
    }
    
    crate::debug::serial_write_str("ECLIPSEFS: Asegurando inicialización del VFS\n");
    init_vfs();
        crate::debug::serial_write_str("ECLIPSEFS: Obteniendo guardia del VFS\n");
        let mut vfs_guard = get_vfs();
        crate::debug::serial_write_str("ECLIPSEFS: Guardia del VFS obtenido\n");
        let vfs = vfs_guard
            .as_mut()
            .ok_or(VfsError::InvalidFs("VFS not initialized".into()))?;

        crate::debug::serial_write_str("ECLIPSEFS: Creando instancia EclipseFS\n");
    crate::debug::serial_write_str("ECLIPSEFS: antes de new()\n");
        let mut fs_instance = eclipsefs_lib::EclipseFS::new();
    crate::debug::serial_write_str("ECLIPSEFS: después de new()\n");
    crate::debug::serial_write_str("ECLIPSEFS: (root) Instancia de filesystem parseada\n");

    let header = EclipseFSHeader::from_bytes(&fs_buffer[..])
        .map_err(|_| VfsError::InvalidFs("Header EclipseFS inválido".into()))?;

    let inode_table_offset = header.inode_table_offset;
    let inode_table_size = header.inode_table_size;

    let end_table = inode_table_offset
        .checked_add(inode_table_size)
        .ok_or(VfsError::InvalidFs("Tabla de inodos fuera de rango".into()))?;

    if (end_table as usize) > fs_buffer.len() {
        crate::debug::serial_write_str("ECLIPSEFS: Header demasiado grande, se requiere lectura incremental\n");
    }

    // Leer tabla de inodos completa a memoria temporal
    let inode_table_size_usize = inode_table_size as usize;
    let mut inode_table_data: Vec<u8> = Vec::new();
    inode_table_data
        .try_reserve(inode_table_size_usize)
        .map_err(|_| VfsError::InvalidFs("Sin memoria para tabla de inodos".into()))?;
    inode_table_data.resize(inode_table_size_usize, 0);

    let mut bytes_filled = 0usize;
    let mut absolute_offset = inode_table_offset;
    let mut block_buffer = [0u8; 512];

    while bytes_filled < inode_table_size_usize {
        let block = absolute_offset / 512;
        storage
            .read_from_partition(partition_index, block, &mut block_buffer)
            .map_err(|e| {
                crate::debug::serial_write_str("ECLIPSEFS: Error leyendo tabla de inodos\n");
                VfsError::DeviceError(e.into())
            })?;

        let block_offset = (absolute_offset % 512) as usize;
        let to_copy = cmp::min(inode_table_size_usize - bytes_filled, 512 - block_offset);
        inode_table_data[bytes_filled..bytes_filled + to_copy]
            .copy_from_slice(&block_buffer[block_offset..block_offset + to_copy]);

        bytes_filled += to_copy;
        absolute_offset += to_copy as u64;
    }

    let mut inode_entries: Vec<InodeTableEntry> = Vec::with_capacity(header.total_inodes as usize);

    for idx in 0..header.total_inodes {
        let entry_offset = (idx as usize) * (ecfs_constants::INODE_TABLE_ENTRY_SIZE);
        let inode = u32::from_le_bytes([
            inode_table_data[entry_offset],
            inode_table_data[entry_offset + 1],
            inode_table_data[entry_offset + 2],
            inode_table_data[entry_offset + 3],
        ]) as u64;
        let rel_offset = u32::from_le_bytes([
            inode_table_data[entry_offset + 4],
            inode_table_data[entry_offset + 5],
            inode_table_data[entry_offset + 6],
            inode_table_data[entry_offset + 7],
        ]) as u64;
        let node_offset = inode_table_offset + header.inode_table_size + rel_offset;
        inode_entries.push(InodeTableEntry::new(inode, node_offset));
    }

    fs_instance
        .load_from_stream(&header, &inode_entries, |offset, buffer| {
            crate::debug::serial_write_str("ECLIPSEFS: fetch() called - offset: ");
            serial_write_decimal(offset);
            crate::debug::serial_write_str(", buffer_len: ");
            serial_write_decimal(buffer.len() as u64);
            crate::debug::serial_write_str("\n");
            
            let mut current_offset = offset;
            let mut written = 0usize;

            while written < buffer.len() {
                let block = current_offset / 512;
                let mut temp_block = [0u8; 512];
                storage
                    .read_from_partition(partition_index, block, &mut temp_block)
                    .map_err(|e| {
                        crate::debug::serial_write_str("ECLIPSEFS: Error leyendo bloque de nodo\n");
                        crate::debug::serial_write_str(&alloc::format!("{}", e));
                        crate::debug::serial_write_str("\n");
                        EclipseFSError::IoError
                    })?;

                let block_offset = (current_offset % 512) as usize;
                let to_copy = cmp::min(buffer.len() - written, 512 - block_offset);
                buffer[written..written + to_copy]
                    .copy_from_slice(&temp_block[block_offset..block_offset + to_copy]);

                current_offset += to_copy as u64;
                written += to_copy;
            }

            crate::debug::serial_write_str("ECLIPSEFS: fetch() completed successfully\n");
            Ok(())
        })
        .map_err(|e| {
            crate::debug::serial_write_str("ECLIPSEFS: Error parseando superblock: ");
            match e {
                eclipsefs_lib::EclipseFSError::InvalidFormat => crate::debug::serial_write_str("InvalidFormat"),
                eclipsefs_lib::EclipseFSError::NotFound => crate::debug::serial_write_str("NotFound"),
                eclipsefs_lib::EclipseFSError::IoError => crate::debug::serial_write_str("IoError"),
                eclipsefs_lib::EclipseFSError::InvalidOperation => crate::debug::serial_write_str("InvalidOperation"),
                eclipsefs_lib::EclipseFSError::UnsupportedVersion => crate::debug::serial_write_str("UnsupportedVersion"),
                eclipsefs_lib::EclipseFSError::DuplicateEntry => crate::debug::serial_write_str("DuplicateEntry"),
                eclipsefs_lib::EclipseFSError::PermissionDenied => crate::debug::serial_write_str("PermissionDenied"),
                eclipsefs_lib::EclipseFSError::DeviceFull => crate::debug::serial_write_str("DeviceFull"),
                eclipsefs_lib::EclipseFSError::FileTooLarge => crate::debug::serial_write_str("FileTooLarge"),
                eclipsefs_lib::EclipseFSError::InvalidFileName => crate::debug::serial_write_str("InvalidFileName"),
                eclipsefs_lib::EclipseFSError::CorruptedFilesystem => crate::debug::serial_write_str("CorruptedFilesystem"),
                eclipsefs_lib::EclipseFSError::OutOfMemory => crate::debug::serial_write_str("OutOfMemory"),
                eclipsefs_lib::EclipseFSError::CompressionError => crate::debug::serial_write_str("CompressionError"),
                eclipsefs_lib::EclipseFSError::EncryptionError => crate::debug::serial_write_str("EncryptionError"),
                eclipsefs_lib::EclipseFSError::SnapshotError => crate::debug::serial_write_str("SnapshotError"),
                eclipsefs_lib::EclipseFSError::AclError => crate::debug::serial_write_str("AclError"),
            }
            crate::debug::serial_write_str("\n");
            VfsError::InvalidFs("EclipseFS parse error".into())
        })?;

    crate::debug::serial_write_str("ECLIPSEFS: Sistema de archivos EclipseFS parseado exitosamente\n");

    let fs_wrapper = Box::new(EclipseFSWrapper::new(fs_instance));
    vfs.mount("/", fs_wrapper);
    vfs.debug_list_mounts();
    crate::debug::serial_write_str("ECLIPSEFS: (root) Filesystem montado en /\n");

    Ok(())
}

/// Montar EclipseFS desde la partición específica usando StorageManager
pub fn mount_eclipsefs_from_storage(storage: &StorageManager) -> Result<(), VfsError> {
    crate::debug::serial_write_str("ECLIPSEFS: Iniciando mount_eclipsefs_from_storage\n");
    crate::debug::serial_write_str("ECLIPSEFS: device_count = ");
    serial_write_decimal(storage.device_count() as u64);
    crate::debug::serial_write_str("\n");

    if storage.device_count() == 0 {
        crate::debug::serial_write_str("ECLIPSEFS: No storage devices found\n");
        return Err(VfsError::DeviceError("No storage devices found".into()));
    }

    crate::debug::serial_write_str("ECLIPSEFS: llamando a mount_root_fs_from_storage()\n");
    crate::debug::serial_write_str("ECLIPSEFS: checkpoint before root mount\n");

    match mount_root_fs_from_storage(storage) {
        Ok(()) => {
            crate::debug::serial_write_str("ECLIPSEFS: checkpoint after root mount\n");
            crate::debug::serial_write_str("ECLIPSEFS: mount_root_fs_from_storage completado con éxito\n");
            Ok(())
        }
        Err(e) => {
            crate::debug::serial_write_str("ECLIPSEFS: mount_root_fs_from_storage falló\n");
            Err(e)
        }
    }
}

impl FileSystem for EclipseFSWrapper {
    fn unmount(&mut self) -> Result<(), VfsError> { Ok(()) }
    fn read(&self, _inode: u32, _offset: u64, _buffer: &mut [u8]) -> Result<usize, VfsError> { Ok(0) }
    fn write(&mut self, _inode: u32, _offset: u64, _data: &[u8]) -> Result<usize, VfsError> { Ok(0) }

    fn stat(&self, inode: u32) -> Result<StatInfo, VfsError> {
        let node = self.fs.get_node(inode).ok_or(VfsError::FileNotFound)?;
    Ok(StatInfo {
            inode,
        size: node.size,
            mode: node.mode as u16,
        uid: node.uid,
        gid: node.gid,
        atime: node.atime,
        mtime: node.mtime,
        ctime: node.ctime,
            nlink: node.nlink,
        })
    }

    fn readdir(&self, inode: u32) -> Result<Vec<String>, VfsError> {
        let entries = self.fs.list_directory(inode).map_err(|_| VfsError::InvalidFs("Error listing directory".into()))?;
        Ok(entries.iter().map(|name_str| name_str.to_string()).collect())
    }
    
    fn truncate(&mut self, _inode: u32, _size: u64) -> Result<(), VfsError> { Ok(()) }
    fn rmdir(&mut self, _parent_inode: u32, _name: &str) -> Result<(), VfsError> { Ok(()) }
    fn rename(&mut self, _parent_inode: u32, _old_name: &str, _new_parent_inode: u32, _new_name: &str) -> Result<(), VfsError> { Ok(()) }
    fn unlink(&mut self, _parent_inode: u32, _name: &str) -> Result<(), VfsError> { Ok(()) }
    fn chmod(&mut self, _inode: u32, _mode: u16) -> Result<(), VfsError> { Ok(()) }
    fn chown(&mut self, _inode: u32, _uid: u32, _gid: u32) -> Result<(), VfsError> { Ok(()) }

    fn resolve_path(&self, path: &str) -> Result<u32, VfsError> {
        let normalized = normalize_path(path);

        match self.fs.lookup_path(&normalized) {
            Ok(inode) => Ok(inode),
            Err(EclipseFSError::NotFound) => Err(VfsError::FileNotFound),
            Err(EclipseFSError::InvalidOperation) => Err(VfsError::InvalidPath),
            Err(err) => Err(VfsError::InvalidFs(alloc::format!("Error resolviendo ruta: {:?}", err))),
        }
    }

    fn readdir_path(&self, path: &str) -> Result<Vec<String>, VfsError> {
        let inode = self.resolve_path(path)?;
        self.readdir(inode)
    }

    fn read_file_path(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        let inode = self.resolve_path(path)?;
        let data = self
            .fs
            .read_file(inode)
            .map_err(|err| match err {
                EclipseFSError::NotFound => VfsError::FileNotFound,
                _ => VfsError::InvalidFs("Error leyendo archivo".into()),
            })?;
        Ok(data.to_vec())
    }
}

pub fn serial_write_decimal(mut num: u64) {
    if num == 0 {
        crate::debug::serial_write_str("0");
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
        serial_write_byte(buf[j]);
    }
}

pub fn serial_write_hex_byte(byte: u8) {
    let hex = b"0123456789ABCDEF";
    serial_write_byte(hex[(byte >> 4) as usize]);
    serial_write_byte(hex[(byte & 0xF) as usize]);
}

pub fn serial_write_byte(byte: u8) {
    // Implementación para escribir un byte al puerto serial
    unsafe {
        while x86_64::instructions::port::Port::<u8>::new(0x3F8 + 5).read() & 0x20 == 0 {}
        x86_64::instructions::port::Port::<u8>::new(0x3F8).write(byte);
    }
}

fn normalize_path(path: &str) -> alloc::string::String {
    if path.is_empty() {
        return "/".to_string();
    }

    let trimmed = path.trim();
    if trimmed == "/" {
        return "/".to_string();
    }

    let mut buffer = alloc::string::String::new();
    let mut prev_was_slash = false;
    for ch in trimmed.chars() {
        if ch == '/' {
            if !prev_was_slash {
                buffer.push(ch);
                prev_was_slash = true;
            }
        } else {
            buffer.push(ch);
            prev_was_slash = false;
        }
    }

    if buffer.is_empty() {
        "/".to_string()
    } else if buffer.starts_with('/') {
        buffer
    } else {
        alloc::format!("/{}", buffer)
    }
}
