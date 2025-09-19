//! Virtual File System (VFS) para Eclipse OS
//! 
//! El VFS proporciona una interfaz unificada para diferentes tipos de sistemas de archivos
//! y maneja las operaciones de archivos a nivel del kernel.

use crate::filesystem::{
    superblock::SuperBlock,
    utils::Path,
    FileSystemInfo,
};

// Resultado de operaciones VFS
pub type VfsResult<T> = Result<T, VfsError>;

// Errores del VFS
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VfsError {
    FileNotFound,
    PermissionDenied,
    FileExists,
    InvalidPath,
    InvalidInode,
    OutOfSpace,
    OutOfMemory,
    InvalidOperation,
    FileTooLarge,
    DirectoryNotEmpty,
    NotADirectory,
    NotAFile,
    ReadOnlyFileSystem,
    InvalidFileHandle,
    FileBusy,
    NameTooLong,
    PathTooLong,
    InvalidArgument,
    SystemError,
}

impl VfsError {
    pub fn as_str(&self) -> &'static str {
        match self {
            VfsError::FileNotFound => "File not found",
            VfsError::PermissionDenied => "Permission denied",
            VfsError::FileExists => "File already exists",
            VfsError::InvalidPath => "Invalid path",
            VfsError::InvalidInode => "Invalid inode",
            VfsError::OutOfSpace => "Out of space",
            VfsError::OutOfMemory => "Out of memory",
            VfsError::InvalidOperation => "Invalid operation",
            VfsError::FileTooLarge => "File too large",
            VfsError::DirectoryNotEmpty => "Directory not empty",
            VfsError::NotADirectory => "Not a directory",
            VfsError::NotAFile => "Not a file",
            VfsError::ReadOnlyFileSystem => "Read-only file system",
            VfsError::InvalidFileHandle => "Invalid file handle",
            VfsError::FileBusy => "File busy",
            VfsError::NameTooLong => "Name too long",
            VfsError::PathTooLong => "Path too long",
            VfsError::InvalidArgument => "Invalid argument",
            VfsError::SystemError => "System error",
        }
    }
}

impl core::fmt::Display for VfsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl From<&str> for VfsError {
    fn from(_s: &str) -> Self {
        VfsError::SystemError
    }
}

// Handle de archivo abierto
#[derive(Debug, Clone, Copy)]
pub struct FileHandle {
    pub inode: u32,
    pub position: u64,
    pub mode: u32,
    pub flags: u32,
}

impl FileHandle {
    pub fn new(inode: u32, mode: u32, flags: u32) -> Self {
        Self {
            inode,
            position: 0,
            mode,
            flags,
        }
    }
}

// Sistema de archivos virtual
pub struct Vfs {
    pub superblock: SuperBlock,
    pub root_inode: u32,
    pub current_directory: u32,
    pub open_files: [Option<FileHandle>; 256], // Máximo 256 archivos abiertos
    pub next_handle: u32,
}

impl Vfs {
    /// Crear un nuevo VFS
    pub fn new() -> Self {
        Self {
            superblock: SuperBlock::new(),
            root_inode: 1,
            current_directory: 1,
            open_files: [None; 256],
            next_handle: 0,
        }
    }

    /// Inicializar el VFS
    pub fn init(&mut self) -> VfsResult<()> {
        // Inicializar superblock
        self.superblock.init()?;
        
        // Crear inodo raíz
        self.root_inode = 1;
        self.current_directory = self.root_inode;
        
        Ok(())
    }

    /// Abrir un archivo
    pub fn open(&mut self, path: &Path, mode: u32, flags: u32) -> VfsResult<u32> {
        // Validar path
        if path.is_empty() {
            return Err(VfsError::InvalidPath);
        }

        // Buscar inodo
        let inode = self.lookup_inode(path)?;
        
        // Verificar permisos
        if !self.check_permissions(inode, mode) {
            return Err(VfsError::PermissionDenied);
        }

        // Crear handle
        let handle = FileHandle::new(inode, mode, flags);
        
        // Asignar slot
        for i in 0..self.open_files.len() {
            if self.open_files[i].is_none() {
                self.open_files[i] = Some(handle);
                return Ok(i as u32);
            }
        }

        Err(VfsError::OutOfMemory)
    }

    /// Cerrar un archivo
    pub fn close(&mut self, handle: u32) -> VfsResult<()> {
        if (handle as usize) >= self.open_files.len() {
            return Err(VfsError::InvalidFileHandle);
        }

        self.open_files[handle as usize] = None;
        Ok(())
    }

    /// Leer de un archivo
    pub fn read(&mut self, handle: u32, buffer: &mut [u8]) -> VfsResult<usize> {
        let file_handle = self.get_file_handle(handle)?;
        
        // Verificar permisos de lectura
        if file_handle.mode & 0x3 == 1 { // Solo escritura
            return Err(VfsError::PermissionDenied);
        }
        
        // Obtener inodo
        let inode = self.get_inode(file_handle.inode)?;
        
        // Verificar que no exceda el tamaño del archivo
        if file_handle.position >= inode.size {
            return Ok(0); // EOF
        }
        
        // Calcular cuántos bytes leer
        let remaining = inode.size - file_handle.position;
        let to_read = buffer.len().min(remaining as usize);
        
        // Leer datos del cache o disco
        let bytes_read = self.read_inode_data(file_handle.inode, file_handle.position, &mut buffer[..to_read])?;
        
        // Actualizar posición
        if let Some(ref mut handle) = self.open_files[handle as usize] {
            handle.position += bytes_read as u64;
        }
        
        Ok(bytes_read)
    }

    /// Escribir a un archivo
    pub fn write(&mut self, handle: u32, buffer: &[u8]) -> VfsResult<usize> {
        let file_handle = self.get_file_handle(handle)?;
        
        // Verificar modo de escritura
        if file_handle.mode & 0x3 == 0 { // Solo lectura
            return Err(VfsError::PermissionDenied);
        }
        
        // Guardar valores necesarios antes del borrow mutable
        let inode_num = file_handle.inode;
        let position = file_handle.position;
        
        // Escribir datos al cache o disco
        let bytes_written = self.write_inode_data(inode_num, position, buffer)?;
        
        // Actualizar tamaño del archivo si es necesario
        let new_position = position + bytes_written as u64;
        
        // Obtener inodo y actualizar tamaño si es necesario
        let mut inode = self.get_inode(inode_num)?;
        if new_position > inode.size {
            inode.set_size(new_position);
            self.update_inode(inode_num, &inode)?;
        }
        
        // Actualizar posición
        if let Some(ref mut handle) = self.open_files[handle as usize] {
            handle.position = new_position;
        }
        
        // Marcar sistema de archivos como sucio
        self.superblock.mark_dirty();
        
        Ok(bytes_written)
    }

    /// Crear un archivo
    pub fn create(&mut self, path: &Path, mode: u32) -> VfsResult<u32> {
        // Verificar si el archivo ya existe
        if self.lookup_inode(path).is_ok() {
            return Err(VfsError::FileExists);
        }

        // Crear inodo
        let inode = self.allocate_inode()?;
        
        // Configurar inodo
        // (implementación simplificada)

        // Abrir archivo
        self.open(path, mode, 0)
    }

    /// Crear un directorio
    pub fn mkdir(&mut self, path: &Path, mode: u32) -> VfsResult<()> {
        // Verificar si el directorio ya existe
        if self.lookup_inode(path).is_ok() {
            return Err(VfsError::FileExists);
        }

        // Crear inodo de directorio
        let inode = self.allocate_inode()?;
        
        // Configurar como directorio
        // (implementación simplificada)

        Ok(())
    }

    /// Eliminar un archivo
    pub fn unlink(&mut self, path: &Path) -> VfsResult<()> {
        let inode = self.lookup_inode(path)?;
        
        // Verificar que no sea un directorio
        // (implementación simplificada)

        // Liberar inodo
        self.free_inode(inode)?;
        
        Ok(())
    }

    /// Eliminar un directorio
    pub fn rmdir(&mut self, path: &Path) -> VfsResult<()> {
        let inode = self.lookup_inode(path)?;
        
        // Verificar que sea un directorio
        // Verificar que esté vacío
        // (implementación simplificada)

        // Liberar inodo
        self.free_inode(inode)?;
        
        Ok(())
    }

    /// Cambiar directorio actual
    pub fn chdir(&mut self, path: &Path) -> VfsResult<()> {
        let inode = self.lookup_inode(path)?;
        
        // Verificar que sea un directorio
        // (implementación simplificada)

        self.current_directory = inode;
        Ok(())
    }

    /// Obtener directorio actual
    pub fn getcwd(&self) -> Path {
        // Implementación simplificada
        Path::new()
    }

    /// Buscar inodo por path
    fn lookup_inode(&self, path: &Path) -> VfsResult<u32> {
        // Implementación simplificada - siempre retorna inodo raíz
        if path.as_str() == "/" {
            Ok(self.root_inode)
        } else {
            Err(VfsError::FileNotFound)
        }
    }

    /// Verificar permisos
    fn check_permissions(&self, _inode: u32, _mode: u32) -> bool {
        // Implementación simplificada - siempre permite
        true
    }

    /// Asignar nuevo inodo
    fn allocate_inode(&mut self) -> VfsResult<u32> {
        // Implementación simplificada
        Ok(2) // Retorna inodo 2 como ejemplo
    }

    /// Liberar inodo
    fn free_inode(&mut self, _inode: u32) -> VfsResult<()> {
        // Implementación simplificada
        Ok(())
    }

    /// Obtener handle de archivo
    fn get_file_handle(&self, handle: u32) -> VfsResult<&FileHandle> {
        if (handle as usize) >= self.open_files.len() {
            return Err(VfsError::InvalidFileHandle);
        }

        self.open_files[handle as usize]
            .as_ref()
            .ok_or(VfsError::InvalidFileHandle)
    }

    /// Obtener información del sistema de archivos
    pub fn get_filesystem_info(&self) -> FileSystemInfo {
        FileSystemInfo::new()
    }
    
    /// Obtener inodo por número
    fn get_inode(&self, inode_num: u32) -> VfsResult<crate::filesystem::inode::Inode> {
        // Implementación simplificada - crear inodo básico
        if inode_num == 1 {
            Ok(crate::filesystem::inode::Inode::new_directory())
        } else {
            Ok(crate::filesystem::inode::Inode::new_file())
        }
    }
    
    /// Actualizar inodo
    fn update_inode(&mut self, _inode_num: u32, _inode: &crate::filesystem::inode::Inode) -> VfsResult<()> {
        // Implementación simplificada
        Ok(())
    }
    
    /// Leer datos de un inodo
    fn read_inode_data(&mut self, inode_num: u32, offset: u64, buffer: &mut [u8]) -> VfsResult<usize> {
        // Intentar leer del cache de archivos primero
        if let Some(cache) = crate::filesystem::cache::get_file_cache() {
            if let Some(cache_entry) = cache.get(inode_num) {
                let start = offset as usize;
                let end = (start + buffer.len()).min(cache_entry.data.len());
                if start < cache_entry.data.len() {
                    let len = end - start;
                    buffer[..len].copy_from_slice(&cache_entry.data[start..end]);
                    return Ok(len);
                }
            }
        }
        
        // Si no está en cache de archivos, leer del sistema de bloques
        if let Some(block_cache) = crate::filesystem::block::get_block_cache() {
            // Calcular bloque inicial
            let block_size = crate::filesystem::BLOCK_SIZE as u64;
            let start_block = offset / block_size;
            let block_offset = (offset % block_size) as usize;
            
            // Leer bloques necesarios
            let mut bytes_read = 0;
            let mut remaining = buffer.len();
            let mut current_offset = block_offset;
            let mut current_block = start_block;
            
            while remaining > 0 && bytes_read < buffer.len() {
                match block_cache.get_or_load_block(current_block) {
                    Ok(block_data) => {
                        let available = block_data.len() - current_offset;
                        let to_copy = remaining.min(available);
                        
                        buffer[bytes_read..bytes_read + to_copy]
                            .copy_from_slice(&block_data[current_offset..current_offset + to_copy]);
                        
                        bytes_read += to_copy;
                        remaining -= to_copy;
                        current_offset = 0;
                        current_block += 1;
                    }
                    Err(_) => break,
                }
            }
            
            Ok(bytes_read)
        } else {
            // Fallback: datos de ejemplo
            let data = b"Eclipse OS File System - Archivo de ejemplo\n";
            let start = offset as usize;
            let end = (start + buffer.len()).min(data.len());
            if start < data.len() {
                let len = end - start;
                buffer[..len].copy_from_slice(&data[start..end]);
                Ok(len)
            } else {
                Ok(0)
            }
        }
    }
    
    /// Escribir datos a un inodo
    fn write_inode_data(&mut self, inode_num: u32, offset: u64, data: &[u8]) -> VfsResult<usize> {
        // Escribir al cache de archivos
        if let Some(cache) = crate::filesystem::cache::get_file_cache() {
            let cache_entry = cache.put(inode_num);
            let start = offset as usize;
            let end = (start + data.len()).min(cache_entry.data.len());
            if start < cache_entry.data.len() {
                let len = end - start;
                cache_entry.data[start..end].copy_from_slice(&data[..len]);
                cache_entry.dirty = true;
            }
        }
        
        // También escribir al sistema de bloques
        if let Some(block_cache) = crate::filesystem::block::get_block_cache() {
            let block_size = crate::filesystem::BLOCK_SIZE as u64;
            let start_block = offset / block_size;
            let block_offset = (offset % block_size) as usize;
            
            let mut bytes_written = 0;
            let mut remaining = data.len();
            let mut current_offset = block_offset;
            let mut current_block = start_block;
            
            while remaining > 0 && bytes_written < data.len() {
                match block_cache.get_or_load_block(current_block) {
                    Ok(block_data) => {
                        let available = block_data.len() - current_offset;
                        let to_write = remaining.min(available);
                        
                        block_data[current_offset..current_offset + to_write]
                            .copy_from_slice(&data[bytes_written..bytes_written + to_write]);
                        
                        // Marcar bloque como sucio
                        block_cache.mark_dirty(current_block);
                        
                        bytes_written += to_write;
                        remaining -= to_write;
                        current_offset = 0;
                        current_block += 1;
                    }
                    Err(_) => break,
                }
            }
            
            Ok(bytes_written)
        } else {
            // Fallback: solo retornar el tamaño
            Ok(data.len())
        }
    }
}

// Instancia global del VFS
static mut VFS_INSTANCE: Option<Vfs> = None;

/// Inicializar VFS
pub fn init_vfs() -> VfsResult<()> {
    unsafe {
        VFS_INSTANCE = Some(Vfs::new());
        if let Some(ref mut vfs) = VFS_INSTANCE {
            vfs.init()?;
        }
    }
    Ok(())
}

/// Obtener instancia del VFS
pub fn get_vfs() -> Option<&'static mut Vfs> {
    unsafe { VFS_INSTANCE.as_mut() }
}

/// Obtener estadísticas del VFS (compatible con main.rs)
pub fn get_vfs_statistics() -> (usize, usize, usize, usize) {
    unsafe {
        if let Some(ref vfs) = VFS_INSTANCE {
            let total_mounts = 1; // Un sistema de archivos montado
            let mounted_fs = 1; // Un sistema de archivos montado
            let open_files = vfs.open_files.iter().filter(|f| f.is_some()).count();
            let total_files = 100; // Simplificado
            (total_mounts, mounted_fs, open_files, total_files)
        } else {
            (0, 0, 0, 0)
        }
    }
}

/// Crear sistema de archivos de demostración
pub fn create_demo_filesystem() -> VfsResult<()> {
    unsafe {
        if let Some(ref mut vfs) = VFS_INSTANCE {
            // Crear directorio raíz con algunos archivos de ejemplo
            let mut root_dir = crate::filesystem::directory::Directory::new();
            
            // Crear archivo de bienvenida
            let mut welcome_entry = crate::filesystem::directory::DirectoryEntry::new();
            welcome_entry.inode = 2;
            welcome_entry.set_name("welcome.txt");
            welcome_entry.entry_type = crate::filesystem::INODE_TYPE_FILE as u8;
            root_dir.add_entry(welcome_entry);
            
            // Crear directorio de sistema
            let mut system_entry = crate::filesystem::directory::DirectoryEntry::new();
            system_entry.inode = 3;
            system_entry.set_name("system");
            system_entry.entry_type = crate::filesystem::INODE_TYPE_DIR as u8;
            root_dir.add_entry(system_entry);
            
            // Crear archivo de configuración
            let mut config_entry = crate::filesystem::directory::DirectoryEntry::new();
            config_entry.inode = 4;
            config_entry.set_name("config.ini");
            config_entry.entry_type = crate::filesystem::INODE_TYPE_FILE as u8;
            root_dir.add_entry(config_entry);
            
            // Crear directorio de usuarios
            let mut users_entry = crate::filesystem::directory::DirectoryEntry::new();
            users_entry.inode = 5;
            users_entry.set_name("users");
            users_entry.entry_type = crate::filesystem::INODE_TYPE_DIR as u8;
            root_dir.add_entry(users_entry);
            
            // Crear archivo de log
            let mut log_entry = crate::filesystem::directory::DirectoryEntry::new();
            log_entry.inode = 6;
            log_entry.set_name("system.log");
            log_entry.entry_type = crate::filesystem::INODE_TYPE_FILE as u8;
            root_dir.add_entry(log_entry);
            
            Ok(())
        } else {
            Err(VfsError::SystemError)
        }
    }
}

/// Escribir contenido de demostración a un archivo
pub fn write_demo_content(inode: u32, content: &[u8]) -> VfsResult<()> {
    unsafe {
        if let Some(cache) = crate::filesystem::cache::get_file_cache() {
            let cache_entry = cache.put(inode);
            let len = content.len().min(cache_entry.data.len());
            cache_entry.data[..len].copy_from_slice(&content[..len]);
            cache_entry.dirty = true;
            Ok(())
        } else {
            Err(VfsError::SystemError)
        }
    }
}