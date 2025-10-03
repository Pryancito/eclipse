//! Virtual File System (VFS) para Eclipse OS
//! 
//! El VFS proporciona una interfaz unificada para diferentes tipos de sistemas de archivos
//! y maneja las operaciones de archivos a nivel del kernel.

use crate::filesystem::{
    superblock::SuperBlock,
    utils::Path,
    FileSystemInfo,
    eclipsefs::{EncryptionType, EncryptionInfo, CompressionType, CompressionInfo, Snapshot, Acl, AclEntry, AclEntryType, TransparentEncryptionConfig, FsckResult, DfResult, FindResult},
};
use alloc::vec::Vec;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::format;

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
    NotASymlink,
    ReadOnlyFileSystem,
    InvalidFileHandle,
    FileBusy,
    NameTooLong,
    PathTooLong,
    InvalidArgument,
    SystemError,
    NoSpaceLeft,
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
            VfsError::NotASymlink => "Not a symbolic link",
            VfsError::ReadOnlyFileSystem => "Read-only file system",
            VfsError::InvalidFileHandle => "Invalid file handle",
            VfsError::FileBusy => "File busy",
            VfsError::NameTooLong => "Name too long",
            VfsError::PathTooLong => "Path too long",
            VfsError::InvalidArgument => "Invalid argument",
            VfsError::SystemError => "System error",
            VfsError::NoSpaceLeft => "No space left on device",
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
#[derive(Debug, Clone)]
pub struct FileHandle {
    pub inode: u32,
    pub position: u64,
    pub mode: u32,
    pub flags: u32,
    // Ruta asociada para backends en memoria
    pub path: Option<String>,
}

impl FileHandle {
    pub fn new(inode: u32, mode: u32, flags: u32) -> Self {
        Self {
            inode,
            position: 0,
            mode,
            flags,
            path: None,
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
    pub mounts: Vec<Mount>,
}

impl Vfs {
    /// Crear un nuevo VFS
    pub fn new() -> Self {
        Self {
            superblock: SuperBlock::new(),
            root_inode: 1,
            current_directory: 1,
            open_files: [const { None }; 256],
            next_handle: 0,
            mounts: Vec::new(),
        }
    }

    /// Listar entradas de un directorio
    pub fn readdir(&mut self, path: &Path) -> VfsResult<Vec<String>> {
        let path_str = render_path(path);
        crate::filesystem::eclipsefs::readdir(&path_str)
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
        if path.is_empty() { return Err(VfsError::InvalidPath); }
        let path_str = render_path(path);

        // EclipseFS por defecto
        if (flags & crate::filesystem::O_CREAT) != 0 {
            let _ = crate::filesystem::eclipsefs::create_file(&path_str);
        }
        if crate::filesystem::eclipsefs::stat(&path_str).is_err() {
            return Err(VfsError::FileNotFound);
        }

        let mut handle = FileHandle::new(0, mode, flags);
        handle.path = Some(path_str.clone());

        // Truncar si O_TRUNC
        if (flags & crate::filesystem::O_TRUNC) != 0 {
            let _ = crate::filesystem::eclipsefs::truncate(&path_str, 0);
        }
        // Posicionar al final si O_APPEND
        if (flags & crate::filesystem::O_APPEND) != 0 {
            if let Ok(st) = crate::filesystem::eclipsefs::stat(&path_str) {
                handle.position = st.size;
            }
        }

        for i in 0..self.open_files.len() {
            if self.open_files[i].is_none() {
                self.open_files[i] = Some(handle);
                return Ok(i as u32);
            }
        }
        Err(VfsError::OutOfMemory)
    }

    /// Mover el cursor del archivo (SEEK_SET=0, SEEK_CUR=1, SEEK_END=2)
    pub fn lseek(&mut self, handle: u32, offset: i64, whence: u32) -> VfsResult<u64> {
        let h = self.get_file_handle(handle)?;
        let path = h.path.as_ref().ok_or(VfsError::InvalidFileHandle)?.clone();
        let size = crate::filesystem::eclipsefs::stat(&path).map(|s| s.size).unwrap_or(0);
        let base: i128 = match whence { 0 => 0, 1 => h.position as i128, 2 => size as i128, _ => return Err(VfsError::InvalidArgument) };
        let newpos_i = base + offset as i128;
        if newpos_i < 0 { return Err(VfsError::InvalidArgument); }
        let newpos = newpos_i as u64;
        if let Some(ref mut hh) = self.open_files[handle as usize] { hh.position = newpos; }
        Ok(newpos)
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
        if file_handle.mode & 0x3 == 1 { return Err(VfsError::PermissionDenied); }
        let path = file_handle.path.as_ref().ok_or(VfsError::InvalidFileHandle)?.clone();
        let mut temp = vec![0u8; buffer.len()];
        let n = crate::filesystem::eclipsefs::read(&path, file_handle.position, &mut temp[..])?;
        buffer[..n].copy_from_slice(&temp[..n]);
        if let Some(ref mut h) = self.open_files[handle as usize] { h.position += n as u64; }
        Ok(n)
    }

    /// Escribir a un archivo
    pub fn write(&mut self, handle: u32, buffer: &[u8]) -> VfsResult<usize> {
        let file_handle = self.get_file_handle(handle)?;
        if file_handle.mode & 0x3 == 0 { return Err(VfsError::PermissionDenied); }
        let path = file_handle.path.as_ref().ok_or(VfsError::InvalidFileHandle)?.clone();
        let written = crate::filesystem::eclipsefs::write(&path, file_handle.position, buffer)?;
        if let Some(ref mut h) = self.open_files[handle as usize] { h.position += written as u64; }
        self.superblock.mark_dirty();
        Ok(written)
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
        let path_str = render_path(path);
        let _ = mode; // sin usar por ahora
        crate::filesystem::eclipsefs::create_dir(&path_str).map(|_| ())
    }


    /// Eliminar un directorio
    pub fn rmdir(&mut self, path: &Path) -> VfsResult<()> {
        let path_str = render_path(path);
        crate::filesystem::eclipsefs::rmdir(&path_str)
    }

    /// Cambiar directorio actual
    pub fn chdir(&mut self, path: &Path) -> VfsResult<()> {
        let path_str = render_path(path);
        let st = crate::filesystem::eclipsefs::stat(&path_str)?;
        if !st.is_dir { return Err(VfsError::NotADirectory); }
        // Mantener directorio actual simbólicamente (sin ruta persistente por ahora)
        self.current_directory = 1;
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

    /// stat de una ruta (passthrough a EclipseFS)
    pub fn stat(&self, path: &Path) -> VfsResult<crate::filesystem::eclipsefs::StatInfo> {
        let path_str = render_path(path);
        crate::filesystem::eclipsefs::stat(&path_str)
    }

    /// rename (passthrough a EclipseFS)
    pub fn rename(&mut self, old_path: &Path, new_path: &Path) -> VfsResult<()> {
        let old_s = render_path(old_path);
        let new_s = render_path(new_path);
        crate::filesystem::eclipsefs::rename(&old_s, &new_s)
    }

    /// truncate (passthrough a EclipseFS)
    pub fn truncate(&mut self, path: &Path, new_size: u64) -> VfsResult<()> {
        let path_str = render_path(path);
        crate::filesystem::eclipsefs::truncate(&path_str, new_size)
    }

    /// chmod (passthrough a EclipseFS)
    pub fn chmod(&mut self, path: &Path, mode: u16) -> VfsResult<()> {
        let s = render_path(path);
        crate::filesystem::eclipsefs::chmod(&s, mode)
    }

    /// chown (passthrough a EclipseFS)
    pub fn chown(&mut self, path: &Path, uid: u32, gid: u32) -> VfsResult<()> {
        let s = render_path(path);
        crate::filesystem::eclipsefs::chown(&s, uid, gid)
    }

    /// umask (global de EclipseFS)
    pub fn set_umask(&mut self, mask: u16) { crate::filesystem::eclipsefs::set_umask(mask); }
    pub fn get_umask(&self) -> u16 { crate::filesystem::eclipsefs::get_umask() }
    
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

    /// Registrar un montaje userland (backend IPC)
    pub fn mount_userland(&mut self, mount_point: &str, mount_id: u32) -> VfsResult<()> {
        let m = Mount {
            mount_point: mount_point.to_string(),
            backend: MountBackend::Userland { mount_id },
        };
        self.mounts.push(m);
        Ok(())
    }
}

/// Renderiza un `Path` simplificado a `String`
fn render_path(path: &Path) -> String {
    // Implementación simple basada en los componentes almacenados
    let mut s = String::from("/");
    let mut first = true;
    for i in 0..path.component_count {
        let comp = &path.components[i];
        let name = unsafe { core::str::from_utf8_unchecked(&comp.name[..comp.len]) };
        if !first { s.push('/'); }
        s.push_str(name);
        first = false;
    }
    s
}

/// Backend de montaje: en-kernel o userland vía IPC
#[derive(Debug, Clone)]
pub enum MountBackend {
    InKernel,
    Userland { mount_id: u32 },
}

/// Montaje VFS
#[derive(Debug, Clone)]
pub struct Mount {
    pub mount_point: String,
    pub backend: MountBackend,
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

// Funciones de persistencia para EclipseFS
impl Vfs {
    /// Guardar EclipseFS a buffer
    pub fn dump_eclipsefs(&self) -> VfsResult<Vec<u8>> {
        crate::filesystem::eclipsefs::dump_to_buffer()
    }
    
    /// Cargar EclipseFS desde buffer
    pub fn load_eclipsefs(&mut self, data: &[u8]) -> VfsResult<()> {
        crate::filesystem::eclipsefs::load_from_buffer(data)
    }
    
    /// Guardar EclipseFS a archivo
    pub fn save_eclipsefs(&mut self, path: &str) -> VfsResult<()> {
        crate::filesystem::eclipsefs::save_to_file(path)
    }
    
    /// Cargar EclipseFS desde archivo
    pub fn load_eclipsefs_from_file(&mut self, path: &str) -> VfsResult<()> {
        crate::filesystem::eclipsefs::load_from_file(path)
    }
    
    /// Crear enlace simbólico
    pub fn symlink(&mut self, target: &str, link_path: &str) -> VfsResult<()> {
        crate::filesystem::eclipsefs::symlink(target, link_path)
    }
    
    /// Leer enlace simbólico
    pub fn readlink(&self, path: &str) -> VfsResult<String> {
        crate::filesystem::eclipsefs::readlink(path)
    }
    
    /// Verificar si es enlace simbólico
    pub fn is_symlink(&self, path: &str) -> VfsResult<bool> {
        crate::filesystem::eclipsefs::is_symlink(path)
    }
    
    /// Seguir enlaces simbólicos recursivamente
    pub fn follow_symlinks(&self, path: &str) -> VfsResult<String> {
        crate::filesystem::eclipsefs::follow_symlinks(path)
    }
    
    /// Establecer sticky bit
    pub fn set_sticky_bit(&mut self, path: &str, set: bool) -> VfsResult<()> {
        crate::filesystem::eclipsefs::set_sticky_bit(path, set)
    }
    
    /// Establecer setuid bit
    pub fn set_setuid_bit(&mut self, path: &str, set: bool) -> VfsResult<()> {
        crate::filesystem::eclipsefs::set_setuid_bit(path, set)
    }
    
    /// Establecer setgid bit
    pub fn set_setgid_bit(&mut self, path: &str, set: bool) -> VfsResult<()> {
        crate::filesystem::eclipsefs::set_setgid_bit(path, set)
    }
    
    /// Verificar sticky bit
    pub fn has_sticky_bit(&self, path: &str) -> VfsResult<bool> {
        crate::filesystem::eclipsefs::has_sticky_bit(path)
    }
    
    /// Verificar setuid bit
    pub fn has_setuid_bit(&self, path: &str) -> VfsResult<bool> {
        crate::filesystem::eclipsefs::has_setuid_bit(path)
    }
    
    /// Verificar setgid bit
    pub fn has_setgid_bit(&self, path: &str) -> VfsResult<bool> {
        crate::filesystem::eclipsefs::has_setgid_bit(path)
    }
    
    /// Crear hardlink
    pub fn link(&mut self, target_path: &str, link_path: &str) -> VfsResult<()> {
        crate::filesystem::eclipsefs::link(target_path, link_path)
    }
    
    /// Eliminar hardlink
    pub fn unlink(&mut self, path: &str) -> VfsResult<()> {
        crate::filesystem::eclipsefs::unlink(path)
    }
    
    /// Obtener número de enlaces
    pub fn get_nlink(&self, path: &str) -> VfsResult<u32> {
        crate::filesystem::eclipsefs::get_nlink(path)
    }
    
    /// Encontrar todos los hardlinks de un archivo
    pub fn find_hardlinks(&self, inode: u32) -> VfsResult<Vec<String>> {
        crate::filesystem::eclipsefs::find_hardlinks(inode)
    }
    
    /// Crear directorios recursivamente (mkdir -p)
    pub fn mkdir_p(&mut self, path: &str) -> VfsResult<()> {
        crate::filesystem::eclipsefs::mkdir_p(path)
    }
    
    /// Eliminar directorio recursivamente
    pub fn rmdir_recursive(&mut self, path: &str) -> VfsResult<()> {
        crate::filesystem::eclipsefs::rmdir_recursive(path)
    }
    
    /// Verificar si un directorio está vacío
    pub fn is_dir_empty(&self, path: &str) -> VfsResult<bool> {
        crate::filesystem::eclipsefs::is_dir_empty(path)
    }
    
    /// Obtener tamaño de directorio (recursivo)
    pub fn get_dir_size(&self, path: &str) -> VfsResult<u64> {
        crate::filesystem::eclipsefs::get_dir_size(path)
    }
    
    /// Obtener estadísticas de cache
    pub fn get_cache_stats(&self) -> (u64, u64, u64, usize) {
        crate::filesystem::eclipsefs::get_cache_stats()
    }
    
    /// Obtener tasa de aciertos del cache
    pub fn get_cache_hit_rate(&self) -> f64 {
        crate::filesystem::eclipsefs::get_cache_hit_rate()
    }
    
    /// Resetear estadísticas
    pub fn reset_stats(&mut self) {
        crate::filesystem::eclipsefs::reset_stats();
    }
    
    /// Obtener estadísticas del sistema de archivos
    pub fn get_filesystem_stats(&self) -> (usize, u32, usize) {
        crate::filesystem::eclipsefs::get_filesystem_stats()
    }
    
    /// Optimizar sistema de archivos
    pub fn optimize_filesystem(&mut self) -> VfsResult<()> {
        crate::filesystem::eclipsefs::optimize_filesystem()
    }
    
    // === FUNCIONES DE CIFRADO ===
    
    /// Cifrar un archivo
    pub fn encrypt_file(&mut self, path: &str, encryption_type: EncryptionType, key_id: &str) -> VfsResult<()> {
        crate::filesystem::eclipsefs::encrypt_file(path, encryption_type, key_id)
    }
    
    /// Descifrar un archivo
    pub fn decrypt_file(&mut self, path: &str) -> VfsResult<()> {
        crate::filesystem::eclipsefs::decrypt_file(path)
    }
    
    /// Verificar si un archivo está cifrado
    pub fn is_encrypted(&self, path: &str) -> VfsResult<bool> {
        crate::filesystem::eclipsefs::is_encrypted(path)
    }
    
    /// Obtener información de cifrado de un archivo
    pub fn get_encryption_info(&self, path: &str) -> VfsResult<EncryptionInfo> {
        crate::filesystem::eclipsefs::get_encryption_info(path)
    }
    
    /// Añadir nueva clave de cifrado
    pub fn add_encryption_key(&mut self, key_id: &str, key: Vec<u8>) -> VfsResult<()> {
        crate::filesystem::eclipsefs::add_encryption_key(key_id, key)
    }
    
    /// Cambiar clave de cifrado de un archivo
    pub fn rekey_file(&mut self, path: &str, new_key_id: &str) -> VfsResult<()> {
        crate::filesystem::eclipsefs::rekey_file(path, new_key_id)
    }
    
    // === FUNCIONES DE COMPRESIÓN ===
    
    /// Comprimir un archivo
    pub fn compress_file(&mut self, path: &str, compression_type: CompressionType) -> VfsResult<()> {
        crate::filesystem::eclipsefs::compress_file(path, compression_type)
    }
    
    /// Descomprimir un archivo
    pub fn decompress_file(&mut self, path: &str) -> VfsResult<()> {
        crate::filesystem::eclipsefs::decompress_file(path)
    }
    
    /// Verificar si un archivo está comprimido
    pub fn is_compressed(&self, path: &str) -> VfsResult<bool> {
        crate::filesystem::eclipsefs::is_compressed(path)
    }
    
    /// Obtener información de compresión de un archivo
    pub fn get_compression_info(&self, path: &str) -> VfsResult<CompressionInfo> {
        crate::filesystem::eclipsefs::get_compression_info(path)
    }
    
    /// Comprimir automáticamente archivos grandes
    pub fn auto_compress_large_files(&mut self, threshold: u64) -> VfsResult<usize> {
        crate::filesystem::eclipsefs::auto_compress_large_files(threshold)
    }
    
    /// Obtener estadísticas de compresión
    pub fn get_compression_stats(&self) -> (usize, usize, f32) {
        crate::filesystem::eclipsefs::get_compression_stats()
    }
    
    // === FUNCIONES DE SNAPSHOTS ===
    
    /// Crear snapshot del sistema
    pub fn create_snapshot(&mut self, description: &str) -> VfsResult<u64> {
        crate::filesystem::eclipsefs::create_snapshot(description)
    }
    
    /// Listar todos los snapshots
    pub fn list_snapshots(&self) -> VfsResult<Vec<Snapshot>> {
        crate::filesystem::eclipsefs::list_snapshots()
    }
    
    /// Obtener snapshot específico
    pub fn get_snapshot(&self, snapshot_id: u64) -> VfsResult<Snapshot> {
        crate::filesystem::eclipsefs::get_snapshot(snapshot_id)
    }
    
    /// Restaurar snapshot
    pub fn restore_snapshot(&mut self, snapshot_id: u64) -> VfsResult<()> {
        crate::filesystem::eclipsefs::restore_snapshot(snapshot_id)
    }
    
    /// Eliminar snapshot
    pub fn delete_snapshot(&mut self, snapshot_id: u64) -> VfsResult<()> {
        crate::filesystem::eclipsefs::delete_snapshot(snapshot_id)
    }
    
    /// Obtener estadísticas de snapshots
    pub fn get_snapshot_stats(&self) -> (usize, u64, u64) {
        crate::filesystem::eclipsefs::get_snapshot_stats()
    }
    
    /// Crear snapshot automático
    pub fn auto_snapshot(&mut self) -> VfsResult<u64> {
        crate::filesystem::eclipsefs::auto_snapshot()
    }
    
    /// Limpiar snapshots antiguos
    pub fn cleanup_old_snapshots(&mut self, max_age: u64) -> VfsResult<usize> {
        crate::filesystem::eclipsefs::cleanup_old_snapshots(max_age)
    }
    
    /// Comparar dos snapshots
    pub fn compare_snapshots(&self, snapshot1_id: u64, snapshot2_id: u64) -> VfsResult<(usize, usize, usize)> {
        crate::filesystem::eclipsefs::compare_snapshots(snapshot1_id, snapshot2_id)
    }
    
    /// Exportar snapshot a archivo
    pub fn export_snapshot(&mut self, snapshot_id: u64, file_path: &str) -> VfsResult<()> {
        crate::filesystem::eclipsefs::export_snapshot(snapshot_id, file_path)
    }
    
    /// Importar snapshot desde archivo
    pub fn import_snapshot(&mut self, file_path: &str) -> VfsResult<u64> {
        crate::filesystem::eclipsefs::import_snapshot(file_path)
    }
    
    // ============================================================================
    // FUNCIONES DE ACL (Access Control Lists)
    // ============================================================================
    
    /// Establecer ACL para un archivo o directorio
    pub fn set_acl(&mut self, path: &str, acl: Acl) -> VfsResult<()> {
        crate::filesystem::eclipsefs::set_acl(path, acl)
    }
    
    /// Obtener ACL de un archivo o directorio
    pub fn get_acl(&self, path: &str) -> VfsResult<Acl> {
        crate::filesystem::eclipsefs::get_acl(path)
    }
    
    /// Eliminar ACL de un archivo o directorio
    pub fn remove_acl(&mut self, path: &str) -> VfsResult<()> {
        crate::filesystem::eclipsefs::remove_acl(path)
    }
    
    /// Establecer ACL por defecto para un directorio
    pub fn set_default_acl(&mut self, path: &str, acl: Acl) -> VfsResult<()> {
        crate::filesystem::eclipsefs::set_default_acl(path, acl)
    }
    
    /// Obtener ACL por defecto de un directorio
    pub fn get_default_acl(&self, path: &str) -> VfsResult<Acl> {
        crate::filesystem::eclipsefs::get_default_acl(path)
    }
    
    /// Eliminar ACL por defecto de un directorio
    pub fn remove_default_acl(&mut self, path: &str) -> VfsResult<()> {
        crate::filesystem::eclipsefs::remove_default_acl(path)
    }
    
    /// Verificar permiso ACL para un usuario/grupo
    pub fn check_acl_permission(&self, path: &str, uid: u32, gid: u32, required_permission: u16) -> VfsResult<bool> {
        crate::filesystem::eclipsefs::check_acl_permission(path, uid, gid, required_permission)
    }
    
    /// Copiar ACL de un archivo a otro
    pub fn copy_acl(&mut self, source_path: &str, dest_path: &str) -> VfsResult<()> {
        crate::filesystem::eclipsefs::copy_acl(source_path, dest_path)
    }
    
    /// Heredar ACL por defecto de directorio padre
    pub fn inherit_default_acl(&mut self, parent_path: &str, child_path: &str) -> VfsResult<()> {
        crate::filesystem::eclipsefs::inherit_default_acl(parent_path, child_path)
    }
    
    /// Listar entradas ACL de un archivo
    pub fn list_acl_entries(&self, path: &str) -> VfsResult<Vec<AclEntry>> {
        crate::filesystem::eclipsefs::list_acl_entries(path)
    }
    
    /// Verificar si existe ACL para un archivo
    pub fn acl_exists(&self, path: &str) -> bool {
        crate::filesystem::eclipsefs::acl_exists(path)
    }
    
    /// Obtener estadísticas de ACLs
    pub fn get_acl_stats(&self) -> (usize, usize) {
        crate::filesystem::eclipsefs::get_acl_stats()
    }
    
    /// Limpiar todas las ACLs
    pub fn clear_all_acls(&mut self) {
        crate::filesystem::eclipsefs::clear_all_acls()
    }
    
    // ============================================================================
    // FUNCIONES DE CIFRADO TRANSPARENTE Y CIFRADO DE DIRECTORIOS
    // ============================================================================
    
    /// Habilitar cifrado transparente
    pub fn enable_transparent_encryption(&mut self, config: TransparentEncryptionConfig) -> VfsResult<()> {
        crate::filesystem::eclipsefs::enable_transparent_encryption(config)
    }
    
    /// Deshabilitar cifrado transparente
    pub fn disable_transparent_encryption(&mut self) -> VfsResult<()> {
        crate::filesystem::eclipsefs::disable_transparent_encryption()
    }
    
    /// Verificar si el cifrado transparente está habilitado
    pub fn is_transparent_encryption_enabled(&self) -> bool {
        crate::filesystem::eclipsefs::is_transparent_encryption_enabled()
    }
    
    /// Obtener configuración de cifrado transparente
    pub fn get_transparent_encryption_config(&self) -> TransparentEncryptionConfig {
        crate::filesystem::eclipsefs::get_transparent_encryption_config()
    }
    
    /// Establecer configuración de cifrado transparente
    pub fn set_transparent_encryption_config(&mut self, config: TransparentEncryptionConfig) -> VfsResult<()> {
        crate::filesystem::eclipsefs::set_transparent_encryption_config(config)
    }
    
    /// Cifrar archivo automáticamente
    pub fn auto_encrypt_file(&mut self, path: &str) -> VfsResult<()> {
        crate::filesystem::eclipsefs::auto_encrypt_file(path)
    }
    
    /// Cifrar directorio automáticamente
    pub fn auto_encrypt_directory(&mut self, path: &str) -> VfsResult<()> {
        crate::filesystem::eclipsefs::auto_encrypt_directory(path)
    }
    
    /// Cifrar directorio
    pub fn encrypt_directory(&mut self, path: &str, algorithm: EncryptionType) -> VfsResult<()> {
        crate::filesystem::eclipsefs::encrypt_directory(path, algorithm)
    }
    
    /// Descifrar directorio
    pub fn decrypt_directory(&mut self, path: &str) -> VfsResult<()> {
        crate::filesystem::eclipsefs::decrypt_directory(path)
    }
    
    /// Verificar si un directorio está cifrado
    pub fn is_directory_encrypted(&self, path: &str) -> VfsResult<bool> {
        crate::filesystem::eclipsefs::is_directory_encrypted(path)
    }
    
    /// Obtener información de cifrado de directorio
    pub fn get_directory_encryption_info(&self, path: &str) -> VfsResult<EncryptionInfo> {
        crate::filesystem::eclipsefs::get_directory_encryption_info(path)
    }
    
    /// Generar clave para directorio
    pub fn generate_directory_key(&self, path: &str) -> VfsResult<Vec<u8>> {
        crate::filesystem::eclipsefs::generate_directory_key(path)
    }
    
    /// Obtener clave transparente
    pub fn get_transparent_key(&self, path: &str) -> VfsResult<Vec<u8>> {
        crate::filesystem::eclipsefs::get_transparent_key(path)
    }
    
    /// Establecer clave transparente
    pub fn set_transparent_key(&mut self, path: &str, key: Vec<u8>) -> VfsResult<()> {
        crate::filesystem::eclipsefs::set_transparent_key(path, key)
    }
    
    /// Cifrar datos transparentemente
    pub fn transparent_encrypt_data(&self, data: &[u8], path: &str) -> VfsResult<Vec<u8>> {
        crate::filesystem::eclipsefs::transparent_encrypt_data(data, path)
    }
    
    /// Descifrar datos transparentemente
    pub fn transparent_decrypt_data(&self, data: &[u8], path: &str) -> VfsResult<Vec<u8>> {
        crate::filesystem::eclipsefs::transparent_decrypt_data(data, path)
    }
    
    /// Rotar claves transparentes
    pub fn rotate_transparent_keys(&mut self) -> VfsResult<()> {
        crate::filesystem::eclipsefs::rotate_transparent_keys()
    }
    
    /// Obtener estadísticas de cifrado transparente
    pub fn get_transparent_encryption_stats(&self) -> (usize, usize, usize) {
        crate::filesystem::eclipsefs::get_transparent_encryption_stats()
    }
    
    /// Limpiar cifrado transparente
    pub fn clear_transparent_encryption(&mut self) {
        crate::filesystem::eclipsefs::clear_transparent_encryption()
    }
    
    // ============================================================================
    // HERRAMIENTAS DE ADMINISTRACIÓN (fsck, df, find)
    // ============================================================================
    
    /// Verificar integridad del sistema de archivos (fsck)
    pub fn fsck_verify(&self) -> VfsResult<FsckResult> {
        crate::filesystem::eclipsefs::fsck_verify()
    }
    
    /// Obtener información de uso del disco (df)
    pub fn df_get_usage(&self) -> VfsResult<DfResult> {
        crate::filesystem::eclipsefs::df_get_usage()
    }
    
    /// Buscar archivos con patrón (find)
    pub fn find_files(&self, pattern: &str, search_path: &str) -> VfsResult<FindResult> {
        crate::filesystem::eclipsefs::find_files(pattern, search_path)
    }
    
    /// Obtener salud del sistema de archivos
    pub fn get_filesystem_health(&self) -> VfsResult<(f32, Vec<String>)> {
        crate::filesystem::eclipsefs::get_filesystem_health()
    }
    
    /// Obtener estadísticas detalladas
    pub fn get_detailed_stats(&self) -> VfsResult<(usize, usize, usize, usize, usize, usize)> {
        crate::filesystem::eclipsefs::get_detailed_stats()
    }
    
    // ===== FUNCIONES FAT32 =====
    
    /// Inicializar driver FAT32
    pub fn init_fat32(&mut self) -> VfsResult<()> {
        crate::filesystem::fat32::init_fat32()
    }
    
    /// Inicializar driver FAT32 desde boot sector
    pub fn init_fat32_from_boot(&mut self, boot_data: &[u8]) -> VfsResult<()> {
        crate::filesystem::fat32::init_fat32_from_boot(boot_data)
    }
    
    /// Verificar si FAT32 está disponible
    pub fn is_fat32_available(&self) -> bool {
        crate::filesystem::fat32::is_fat32_available()
    }
    
    /// Leer archivo desde FAT32
    pub fn read_fat32_file(&mut self, path: &str) -> VfsResult<Vec<u8>> {
        if let Some(driver) = crate::filesystem::fat32::get_fat32_driver() {
            // Parsear path y encontrar archivo
            if path.starts_with("/boot/") {
                let filename = &path[6..]; // Remover "/boot/"
                
                // Buscar en directorio raíz
                match driver.find_file(driver.root_dir_cluster, filename) {
                    Ok(file_info) => {
                        driver.read_file(file_info.first_cluster)
                    }
                    Err(_) => {
                        // Si no se encuentra, intentar leer archivo simulado
                        driver.read_file(3) // Cluster simulado
                    }
                }
            } else {
                Err(VfsError::InvalidPath)
            }
        } else {
            Err(VfsError::SystemError)
        }
    }
    
    /// Escribir archivo a FAT32
    pub fn write_fat32_file(&mut self, path: &str, data: &[u8]) -> VfsResult<usize> {
        if let Some(driver) = crate::filesystem::fat32::get_fat32_driver() {
            if path.starts_with("/boot/") {
                // Escribir archivo
                match driver.write_file(data) {
                    Ok(_) => Ok(data.len()),
                    Err(_) => Err(VfsError::SystemError),
                }
            } else {
                Err(VfsError::InvalidPath)
            }
        } else {
            Err(VfsError::SystemError)
        }
    }
    
    /// Listar directorio FAT32
    pub fn list_fat32_directory(&mut self, path: &str) -> VfsResult<Vec<String>> {
        if let Some(driver) = crate::filesystem::fat32::get_fat32_driver() {
            if path == "/boot" || path == "/boot/" {
                // Listar directorio raíz
                match driver.read_directory(driver.root_dir_cluster) {
                    Ok(entries) => {
                        let mut names = Vec::new();
                        for entry in entries {
                            names.push(entry.name);
                        }
                        Ok(names)
                    }
                    Err(_) => Ok(Vec::new()),
                }
            } else {
                Err(VfsError::InvalidPath)
            }
        } else {
            Err(VfsError::SystemError)
        }
    }
    
    /// Verificar si archivo existe en FAT32
    pub fn fat32_file_exists(&mut self, path: &str) -> bool {
        if let Some(driver) = crate::filesystem::fat32::get_fat32_driver() {
            if path.starts_with("/boot/") {
                let filename = &path[6..];
                driver.find_file(driver.root_dir_cluster, filename).is_ok()
            } else {
                false
            }
        } else {
            false
        }
    }
    
    /// Obtener información del sistema de archivos FAT32
    pub fn get_fat32_info(&self) -> VfsResult<(u32, u32, u32, u32)> {
        if let Some(driver) = crate::filesystem::fat32::get_fat32_driver() {
            Ok(driver.get_filesystem_info())
        } else {
            Err(VfsError::SystemError)
        }
    }
    
    /// Montar FAT32 en /boot
    pub fn mount_fat32_boot(&mut self) -> VfsResult<()> {
        // Inicializar FAT32 si no está inicializado
        if !self.is_fat32_available() {
            self.init_fat32()?;
        }
        
        // Crear directorio /boot en EclipseFS si no existe
        let _ = crate::filesystem::eclipsefs::create_dir("/boot");
        
        Ok(())
    }
    
    /// Desmontar FAT32
    pub fn unmount_fat32(&mut self) -> VfsResult<()> {
        // En una implementación real, aquí se limpiarían los recursos
        Ok(())
    }
}