//! Estructura de inodos para Eclipse OS
//! 
//! Los inodos contienen metadatos de archivos y directorios, incluyendo
//! permisos, tamaño, timestamps y punteros a bloques de datos.

use crate::filesystem::{
    BLOCK_SIZE,
    INODE_TYPE_FILE,
    INODE_TYPE_DIR,
    INODE_TYPE_SYMLINK,
    INODE_TYPE_CHARDEV,
    INODE_TYPE_BLOCKDEV,
    INODE_TYPE_FIFO,
    INODE_TYPE_SOCKET,
    PERM_READ,
    PERM_WRITE,
    PERM_EXECUTE,
};

// Número de punteros directos en el inodo
const DIRECT_POINTERS: usize = 12;
// Número de punteros indirectos simples
const SINGLE_INDIRECT: usize = 1;
// Número de punteros indirectos dobles
const DOUBLE_INDIRECT: usize = 1;
// Número de punteros indirectos triples
const TRIPLE_INDIRECT: usize = 1;

// Tamaño total del inodo
pub const INODE_SIZE: usize = 128;

// Tipos de inodo
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InodeType {
    File,
    Directory,
    Symlink,
    CharDevice,
    BlockDevice,
    Fifo,
    Socket,
}

impl InodeType {
    pub fn to_u16(&self) -> u16 {
        match self {
            InodeType::File => INODE_TYPE_FILE,
            InodeType::Directory => INODE_TYPE_DIR,
            InodeType::Symlink => INODE_TYPE_SYMLINK,
            InodeType::CharDevice => INODE_TYPE_CHARDEV,
            InodeType::BlockDevice => INODE_TYPE_BLOCKDEV,
            InodeType::Fifo => INODE_TYPE_FIFO,
            InodeType::Socket => INODE_TYPE_SOCKET,
        }
    }

    pub fn from_u16(value: u16) -> Self {
        match value & 0xF000 {
            INODE_TYPE_FILE => InodeType::File,
            INODE_TYPE_DIR => InodeType::Directory,
            INODE_TYPE_SYMLINK => InodeType::Symlink,
            INODE_TYPE_CHARDEV => InodeType::CharDevice,
            INODE_TYPE_BLOCKDEV => InodeType::BlockDevice,
            INODE_TYPE_FIFO => InodeType::Fifo,
            INODE_TYPE_SOCKET => InodeType::Socket,
            _ => InodeType::File,
        }
    }
}

// Permisos de inodo
#[derive(Debug, Clone, Copy)]
pub struct InodePermissions {
    pub owner_read: bool,
    pub owner_write: bool,
    pub owner_execute: bool,
    pub group_read: bool,
    pub group_write: bool,
    pub group_execute: bool,
    pub other_read: bool,
    pub other_write: bool,
    pub other_execute: bool,
}

impl InodePermissions {
    pub fn new() -> Self {
        Self {
            owner_read: true,
            owner_write: true,
            owner_execute: false,
            group_read: true,
            group_write: false,
            group_execute: false,
            other_read: true,
            other_write: false,
            other_execute: false,
        }
    }

    pub fn to_u16(&self) -> u16 {
        let mut perms = 0u16;
        
        if self.owner_read { perms |= PERM_READ << 6; }
        if self.owner_write { perms |= PERM_WRITE << 6; }
        if self.owner_execute { perms |= PERM_EXECUTE << 6; }
        
        if self.group_read { perms |= PERM_READ << 3; }
        if self.group_write { perms |= PERM_WRITE << 3; }
        if self.group_execute { perms |= PERM_EXECUTE << 3; }
        
        if self.other_read { perms |= PERM_READ; }
        if self.other_write { perms |= PERM_WRITE; }
        if self.other_execute { perms |= PERM_EXECUTE; }
        
        perms
    }

    pub fn from_u16(value: u16) -> Self {
        Self {
            owner_read: (value & (PERM_READ << 6)) != 0,
            owner_write: (value & (PERM_WRITE << 6)) != 0,
            owner_execute: (value & (PERM_EXECUTE << 6)) != 0,
            group_read: (value & (PERM_READ << 3)) != 0,
            group_write: (value & (PERM_WRITE << 3)) != 0,
            group_execute: (value & (PERM_EXECUTE << 3)) != 0,
            other_read: (value & PERM_READ) != 0,
            other_write: (value & PERM_WRITE) != 0,
            other_execute: (value & PERM_EXECUTE) != 0,
        }
    }
}

// Estructura de inodo
#[derive(Debug, Clone)]
pub struct Inode {
    /// Tipo de inodo
    pub inode_type: InodeType,
    /// Permisos
    pub permissions: InodePermissions,
    /// Número de enlaces duros
    pub hard_links: u16,
    /// ID del usuario propietario
    pub uid: u16,
    /// ID del grupo propietario
    pub gid: u16,
    /// Tamaño del archivo en bytes
    pub size: u64,
    /// Tamaño en bloques
    pub blocks: u64,
    /// Tiempo de último acceso
    pub atime: u64,
    /// Tiempo de última modificación
    pub mtime: u64,
    /// Tiempo de último cambio de inodo
    pub ctime: u64,
    /// Punteros directos a bloques de datos
    pub direct_blocks: [u64; DIRECT_POINTERS],
    /// Puntero indirecto simple
    pub single_indirect: u64,
    /// Puntero indirecto doble
    pub double_indirect: u64,
    /// Puntero indirecto triple
    pub triple_indirect: u64,
    /// Número de referencias abiertas
    pub open_count: u32,
    /// Flags especiales
    pub flags: u32,
}

impl Inode {
    /// Crear un nuevo inodo
    pub fn new(inode_type: InodeType) -> Self {
        Self {
            inode_type,
            permissions: InodePermissions::new(),
            hard_links: 1,
            uid: 0,
            gid: 0,
            size: 0,
            blocks: 0,
            atime: 0,
            mtime: 0,
            ctime: 0,
            direct_blocks: [0; DIRECT_POINTERS],
            single_indirect: 0,
            double_indirect: 0,
            triple_indirect: 0,
            open_count: 0,
            flags: 0,
        }
    }

    /// Crear inodo de archivo
    pub fn new_file() -> Self {
        Self::new(InodeType::File)
    }

    /// Crear inodo de directorio
    pub fn new_directory() -> Self {
        Self::new(InodeType::Directory)
    }

    /// Crear inodo de enlace simbólico
    pub fn new_symlink() -> Self {
        Self::new(InodeType::Symlink)
    }

    /// Verificar si es un archivo regular
    pub fn is_file(&self) -> bool {
        self.inode_type == InodeType::File
    }

    /// Verificar si es un directorio
    pub fn is_directory(&self) -> bool {
        self.inode_type == InodeType::Directory
    }

    /// Verificar si es un enlace simbólico
    pub fn is_symlink(&self) -> bool {
        self.inode_type == InodeType::Symlink
    }

    /// Verificar si es un dispositivo de caracteres
    pub fn is_char_device(&self) -> bool {
        self.inode_type == InodeType::CharDevice
    }

    /// Verificar si es un dispositivo de bloques
    pub fn is_block_device(&self) -> bool {
        self.inode_type == InodeType::BlockDevice
    }

    /// Verificar si es un FIFO
    pub fn is_fifo(&self) -> bool {
        self.inode_type == InodeType::Fifo
    }

    /// Verificar si es un socket
    pub fn is_socket(&self) -> bool {
        self.inode_type == InodeType::Socket
    }

    /// Obtener el tamaño del archivo
    pub fn get_size(&self) -> u64 {
        self.size
    }

    /// Establecer el tamaño del archivo
    pub fn set_size(&mut self, size: u64) {
        self.size = size;
        self.blocks = (size + (BLOCK_SIZE as u64) - 1) / (BLOCK_SIZE as u64);
    }

    /// Obtener el número de bloques
    pub fn get_blocks(&self) -> u64 {
        self.blocks
    }

    /// Actualizar tiempo de acceso
    pub fn update_atime(&mut self) {
        self.atime = self.get_current_time();
    }

    /// Actualizar tiempo de modificación
    pub fn update_mtime(&mut self) {
        self.mtime = self.get_current_time();
        self.ctime = self.mtime;
    }

    /// Actualizar tiempo de cambio de inodo
    pub fn update_ctime(&mut self) {
        self.ctime = self.get_current_time();
    }

    /// Incrementar contador de enlaces
    pub fn inc_links(&mut self) {
        self.hard_links += 1;
        self.update_ctime();
    }

    /// Decrementar contador de enlaces
    pub fn dec_links(&mut self) {
        if self.hard_links > 0 {
            self.hard_links -= 1;
        }
        self.update_ctime();
    }

    /// Incrementar contador de referencias abiertas
    pub fn inc_open_count(&mut self) {
        self.open_count += 1;
    }

    /// Decrementar contador de referencias abiertas
    pub fn dec_open_count(&mut self) {
        if self.open_count > 0 {
            self.open_count -= 1;
        }
    }

    /// Obtener tiempo actual (simplificado)
    fn get_current_time(&self) -> u64 {
        // Implementación simplificada - retorna timestamp fijo
        1640995200 // 2022-01-01 00:00:00 UTC
    }

    /// Obtener bloque de datos por índice
    pub fn get_data_block(&self, block_index: u64) -> Option<u64> {
        if block_index < DIRECT_POINTERS as u64 {
            let block = self.direct_blocks[block_index as usize];
            if block != 0 { Some(block) } else { None }
        } else if block_index < (DIRECT_POINTERS + BLOCK_SIZE / 8) as u64 {
            // Implementación simplificada para punteros indirectos
            None
        } else {
            None
        }
    }

    /// Establecer bloque de datos por índice
    pub fn set_data_block(&mut self, block_index: u64, block_number: u64) -> bool {
        if block_index < DIRECT_POINTERS as u64 {
            self.direct_blocks[block_index as usize] = block_number;
            true
        } else {
            // Implementación simplificada para punteros indirectos
            false
        }
    }

    /// Liberar todos los bloques de datos
    pub fn free_data_blocks(&mut self) {
        // Limpiar punteros directos
        for i in 0..DIRECT_POINTERS {
            self.direct_blocks[i] = 0;
        }
        
        // Limpiar punteros indirectos
        self.single_indirect = 0;
        self.double_indirect = 0;
        self.triple_indirect = 0;
        
        self.blocks = 0;
        self.size = 0;
    }

    /// Verificar permisos de lectura
    pub fn can_read(&self, uid: u16, gid: u16) -> bool {
        if uid == self.uid {
            self.permissions.owner_read
        } else if gid == self.gid {
            self.permissions.group_read
        } else {
            self.permissions.other_read
        }
    }

    /// Verificar permisos de escritura
    pub fn can_write(&self, uid: u16, gid: u16) -> bool {
        if uid == self.uid {
            self.permissions.owner_write
        } else if gid == self.gid {
            self.permissions.group_write
        } else {
            self.permissions.other_write
        }
    }

    /// Verificar permisos de ejecución
    pub fn can_execute(&self, uid: u16, gid: u16) -> bool {
        if uid == self.uid {
            self.permissions.owner_execute
        } else if gid == self.gid {
            self.permissions.group_execute
        } else {
            self.permissions.other_execute
        }
    }

    /// Serializar inodo a bytes
    pub fn to_bytes(&self) -> [u8; INODE_SIZE] {
        let mut bytes = [0u8; INODE_SIZE];
        let mut offset = 0;

        // Tipo de inodo (2 bytes)
        let type_bytes = self.inode_type.to_u16().to_le_bytes();
        bytes[offset..offset + 2].copy_from_slice(&type_bytes);
        offset += 2;

        // Permisos (2 bytes)
        let perm_bytes = self.permissions.to_u16().to_le_bytes();
        bytes[offset..offset + 2].copy_from_slice(&perm_bytes);
        offset += 2;

        // Enlaces duros (2 bytes)
        let links_bytes = self.hard_links.to_le_bytes();
        bytes[offset..offset + 2].copy_from_slice(&links_bytes);
        offset += 2;

        // UID (2 bytes)
        let uid_bytes = self.uid.to_le_bytes();
        bytes[offset..offset + 2].copy_from_slice(&uid_bytes);
        offset += 2;

        // GID (2 bytes)
        let gid_bytes = self.gid.to_le_bytes();
        bytes[offset..offset + 2].copy_from_slice(&gid_bytes);
        offset += 2;

        // Tamaño (8 bytes)
        let size_bytes = self.size.to_le_bytes();
        bytes[offset..offset + 8].copy_from_slice(&size_bytes);
        offset += 8;

        // Bloques (8 bytes)
        let blocks_bytes = self.blocks.to_le_bytes();
        bytes[offset..offset + 8].copy_from_slice(&blocks_bytes);
        offset += 8;

        // Timestamps (24 bytes)
        let atime_bytes = self.atime.to_le_bytes();
        bytes[offset..offset + 8].copy_from_slice(&atime_bytes);
        offset += 8;

        let mtime_bytes = self.mtime.to_le_bytes();
        bytes[offset..offset + 8].copy_from_slice(&mtime_bytes);
        offset += 8;

        let ctime_bytes = self.ctime.to_le_bytes();
        bytes[offset..offset + 8].copy_from_slice(&ctime_bytes);
        offset += 8;

        // Punteros directos (96 bytes)
        for i in 0..DIRECT_POINTERS {
            let block_bytes = self.direct_blocks[i].to_le_bytes();
            bytes[offset..offset + 8].copy_from_slice(&block_bytes);
            offset += 8;
        }

        // Punteros indirectos (24 bytes)
        let single_bytes = self.single_indirect.to_le_bytes();
        bytes[offset..offset + 8].copy_from_slice(&single_bytes);
        offset += 8;

        let double_bytes = self.double_indirect.to_le_bytes();
        bytes[offset..offset + 8].copy_from_slice(&double_bytes);
        offset += 8;

        let triple_bytes = self.triple_indirect.to_le_bytes();
        bytes[offset..offset + 8].copy_from_slice(&triple_bytes);
        offset += 8;

        // Flags (4 bytes)
        let flags_bytes = self.flags.to_le_bytes();
        bytes[offset..offset + 4].copy_from_slice(&flags_bytes);

        bytes
    }

    /// Deserializar inodo desde bytes
    pub fn from_bytes(bytes: &[u8; INODE_SIZE]) -> Self {
        let mut offset = 0;

        // Tipo de inodo
        let type_value = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
        let inode_type = InodeType::from_u16(type_value);
        offset += 2;

        // Permisos
        let perm_value = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
        let permissions = InodePermissions::from_u16(perm_value);
        offset += 2;

        // Enlaces duros
        let hard_links = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
        offset += 2;

        // UID
        let uid = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
        offset += 2;

        // GID
        let gid = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
        offset += 2;

        // Tamaño
        let size = u64::from_le_bytes([
            bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3],
            bytes[offset + 4], bytes[offset + 5], bytes[offset + 6], bytes[offset + 7],
        ]);
        offset += 8;

        // Bloques
        let blocks = u64::from_le_bytes([
            bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3],
            bytes[offset + 4], bytes[offset + 5], bytes[offset + 6], bytes[offset + 7],
        ]);
        offset += 8;

        // Timestamps
        let atime = u64::from_le_bytes([
            bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3],
            bytes[offset + 4], bytes[offset + 5], bytes[offset + 6], bytes[offset + 7],
        ]);
        offset += 8;

        let mtime = u64::from_le_bytes([
            bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3],
            bytes[offset + 4], bytes[offset + 5], bytes[offset + 6], bytes[offset + 7],
        ]);
        offset += 8;

        let ctime = u64::from_le_bytes([
            bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3],
            bytes[offset + 4], bytes[offset + 5], bytes[offset + 6], bytes[offset + 7],
        ]);
        offset += 8;

        // Punteros directos
        let mut direct_blocks = [0u64; DIRECT_POINTERS];
        for i in 0..DIRECT_POINTERS {
            direct_blocks[i] = u64::from_le_bytes([
                bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3],
                bytes[offset + 4], bytes[offset + 5], bytes[offset + 6], bytes[offset + 7],
            ]);
            offset += 8;
        }

        // Punteros indirectos
        let single_indirect = u64::from_le_bytes([
            bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3],
            bytes[offset + 4], bytes[offset + 5], bytes[offset + 6], bytes[offset + 7],
        ]);
        offset += 8;

        let double_indirect = u64::from_le_bytes([
            bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3],
            bytes[offset + 4], bytes[offset + 5], bytes[offset + 6], bytes[offset + 7],
        ]);
        offset += 8;

        let triple_indirect = u64::from_le_bytes([
            bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3],
            bytes[offset + 4], bytes[offset + 5], bytes[offset + 6], bytes[offset + 7],
        ]);
        offset += 8;

        // Flags
        let flags = u32::from_le_bytes([
            bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3],
        ]);

        Self {
            inode_type,
            permissions,
            hard_links,
            uid,
            gid,
            size,
            blocks,
            atime,
            mtime,
            ctime,
            direct_blocks,
            single_indirect,
            double_indirect,
            triple_indirect,
            open_count: 0,
            flags,
        }
    }
}
