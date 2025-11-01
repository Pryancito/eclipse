//! Sistema de Archivos Virtual para Eclipse OS
//!
//! Este módulo implementa un sistema de archivos virtual completo que incluye:
//! - Sistema de archivos en memoria (RAM FS)
//! - Operaciones básicas de archivos y directorios
//! - API unificada para diferentes tipos de sistemas de archivos
//! - Soporte para archivos temporales y de configuración
//! - Gestión de permisos y metadatos

#![no_std]
#![allow(unused_imports)]

extern crate alloc;
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::vec;
use alloc::collections::BTreeMap;
use core::fmt;

/// Resultado de operaciones del sistema de archivos
pub type FsResult<T> = Result<T, FsError>;

/// Errores del sistema de archivos
#[derive(Debug, Clone)]
pub enum FsError {
    /// Archivo o directorio no encontrado
    NotFound,
    /// Permiso denegado
    PermissionDenied,
    /// Archivo ya existe
    AlreadyExists,
    /// Es un directorio
    IsDirectory,
    /// No es un directorio
    NotDirectory,
    /// Disco lleno
    DiskFull,
    /// Nombre inválido
    InvalidName,
    /// Operación no soportada
    NotSupported,
    /// Error interno
    InternalError(String),
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FsError::NotFound => write!(f, "Archivo o directorio no encontrado"),
            FsError::PermissionDenied => write!(f, "Permiso denegado"),
            FsError::AlreadyExists => write!(f, "Archivo ya existe"),
            FsError::IsDirectory => write!(f, "Es un directorio"),
            FsError::NotDirectory => write!(f, "No es un directorio"),
            FsError::DiskFull => write!(f, "Disco lleno"),
            FsError::InvalidName => write!(f, "Nombre inválido"),
            FsError::NotSupported => write!(f, "Operación no soportada"),
            FsError::InternalError(msg) => write!(f, "Error interno: {}", msg),
        }
    }
}

/// Permisos de archivo
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilePermissions {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}

impl FilePermissions {
    /// Crear permisos por defecto (lectura/escritura)
    pub fn default() -> Self {
        Self {
            read: true,
            write: true,
            execute: false,
        }
    }

    /// Crear permisos de solo lectura
    pub fn read_only() -> Self {
        Self {
            read: true,
            write: false,
            execute: false,
        }
    }

    /// Crear permisos ejecutables
    pub fn executable() -> Self {
        Self {
            read: true,
            write: false,
            execute: true,
        }
    }
}

/// Tipo de entrada del sistema de archivos
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsEntryType {
    /// Archivo regular
    File,
    /// Directorio
    Directory,
}

/// Metadatos de una entrada del sistema de archivos
#[derive(Debug, Clone)]
pub struct FileMetadata {
    /// Nombre de la entrada
    pub name: String,
    /// Tipo de entrada
    pub entry_type: FsEntryType,
    /// Tamaño en bytes (para archivos)
    pub size: u64,
    /// Permisos
    pub permissions: FilePermissions,
    /// Timestamp de creación
    pub created: u64,
    /// Timestamp de modificación
    pub modified: u64,
    /// Timestamp de último acceso
    pub accessed: u64,
}

/// Entrada del sistema de archivos
#[derive(Debug, Clone)]
pub enum FsEntry {
    /// Archivo con contenido
    File {
        metadata: FileMetadata,
        data: Vec<u8>,
    },
    /// Directorio con entradas hijas
    Directory {
        metadata: FileMetadata,
        children: BTreeMap<String, Box<FsEntry>>,
    },
}

impl FsEntry {
    /// Crear un nuevo archivo
    pub fn new_file(name: &str, permissions: FilePermissions) -> Self {
        let now = get_system_time();
        FsEntry::File {
            metadata: FileMetadata {
                name: name.to_string(),
                entry_type: FsEntryType::File,
                size: 0,
                permissions,
                created: now,
                modified: now,
                accessed: now,
            },
            data: Vec::new(),
        }
    }

    /// Crear un nuevo directorio
    pub fn new_directory(name: &str, permissions: FilePermissions) -> Self {
        let now = get_system_time();
        FsEntry::Directory {
            metadata: FileMetadata {
                name: name.to_string(),
                entry_type: FsEntryType::Directory,
                size: 0,
                permissions,
                created: now,
                modified: now,
                accessed: now,
            },
            children: BTreeMap::new(),
        }
    }

    /// Obtener metadatos
    pub fn metadata(&self) -> &FileMetadata {
        match self {
            FsEntry::File { metadata, .. } => metadata,
            FsEntry::Directory { metadata, .. } => metadata,
        }
    }

    /// Obtener metadatos mutables
    pub fn metadata_mut(&mut self) -> &mut FileMetadata {
        match self {
            FsEntry::File { metadata, .. } => metadata,
            FsEntry::Directory { metadata, .. } => metadata,
        }
    }

    /// Verificar si es un archivo
    pub fn is_file(&self) -> bool {
        matches!(self, FsEntry::File { .. })
    }

    /// Verificar si es un directorio
    pub fn is_directory(&self) -> bool {
        matches!(self, FsEntry::Directory { .. })
    }

    /// Obtener tamaño
    pub fn size(&self) -> u64 {
        match self {
            FsEntry::File { metadata, .. } => metadata.size,
            FsEntry::Directory { children, .. } => children.len() as u64,
        }
    }
}

/// Sistema de archivos virtual en memoria
pub struct VirtualFileSystem {
    /// Raíz del sistema de archivos
    root: FsEntry,
    /// Tamaño máximo del FS (en bytes)
    max_size: u64,
    /// Tamaño actual usado
    used_size: u64,
}

impl VirtualFileSystem {
    /// Crear un nuevo sistema de archivos virtual
    pub fn new(max_size: u64) -> Self {
        let mut root = FsEntry::new_directory("", FilePermissions::default());
        if let FsEntry::Directory { metadata, .. } = &mut root {
            metadata.name = "/".to_string();
        }

        Self {
            root,
            max_size,
            used_size: 4096, // Tamaño base para el directorio raíz
        }
    }

    /// Crear un nuevo sistema de archivos con tamaño por defecto (1MB)
    pub fn new_default() -> Self {
        Self::new(1024 * 1024) // 1MB
    }

    /// Resolver una ruta a una entrada del FS
    fn resolve_path(&self, path: &str) -> FsResult<(&FsEntry, Option<&str>)> {
        let path = path.trim_start_matches('/');
        if path.is_empty() {
            return Ok((&self.root, None));
        }

        let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut current = &self.root;

        for (i, component) in components.iter().enumerate() {
            match current {
                FsEntry::Directory { children, .. } => {
                    if let Some(child) = children.get(*component) {
                        current = child;
                        if i == components.len() - 1 {
                            return Ok((current, None));
                        }
                    } else {
                        return Err(FsError::NotFound);
                    }
                }
                FsEntry::File { .. } => {
                    return Err(FsError::NotDirectory);
                }
            }
        }

        Ok((current, None))
    }

    /// Resolver una ruta a una entrada mutable
    fn resolve_path_mut(&mut self, path: &str) -> FsResult<(&mut FsEntry, Option<String>)> {
        let path = path.trim_start_matches('/');
        if path.is_empty() {
            return Ok((&mut self.root, None));
        }

        let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut current = &mut self.root;

        for (i, component) in components.iter().enumerate() {
            match current {
                FsEntry::Directory { children, .. } => {
                    if let Some(child) = children.get_mut(*component) {
                        current = child;
                        if i == components.len() - 1 {
                            return Ok((current, None));
                        }
                    } else {
                        return Err(FsError::NotFound);
                    }
                }
                FsEntry::File { .. } => {
                    return Err(FsError::NotDirectory);
                }
            }
        }

        Ok((current, None))
    }

    /// Resolver ruta para creación de nuevos archivos/directorios
    fn resolve_path_for_create(&mut self, path: &str) -> FsResult<(&mut BTreeMap<String, Box<FsEntry>>, String)> {
        let path = path.trim_start_matches('/');
        let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        if components.is_empty() {
            return Err(FsError::InvalidName);
        }

        let mut current = &mut self.root;

        for (i, component) in components.iter().enumerate() {
            match current {
                FsEntry::Directory { children, .. } => {
                    if i == components.len() - 1 {
                        // Último componente - devolver el mapa de hijos y el nombre
                        return Ok((children, component.to_string()));
                    } else {
                        // Componente intermedio - debe existir y ser directorio
                        if let Some(child) = children.get_mut(*component) {
                            if let FsEntry::Directory { .. } = child.as_ref() {
                                current = child;
                            } else {
                                return Err(FsError::NotDirectory);
                            }
                        } else {
                            return Err(FsError::NotFound);
                        }
                    }
                }
                FsEntry::File { .. } => {
                    return Err(FsError::NotDirectory);
                }
            }
        }

        Err(FsError::InvalidName)
    }

    /// Crear un archivo
    pub fn create_file(&mut self, path: &str, permissions: FilePermissions) -> FsResult<()> {
        // Verificar espacio disponible primero
        let file_size = 4096; // Tamaño base de metadatos
        if self.used_size + file_size > self.max_size {
            return Err(FsError::DiskFull);
        }

        let (parent_children, name) = self.resolve_path_for_create(path)?;

        if parent_children.contains_key(&name) {
            return Err(FsError::AlreadyExists);
        }

        let file = FsEntry::new_file(&name, permissions);
        parent_children.insert(name, Box::new(file));
        self.used_size += file_size;

        Ok(())
    }

    /// Crear un directorio
    pub fn create_directory(&mut self, path: &str, permissions: FilePermissions) -> FsResult<()> {
        // Verificar espacio disponible primero
        let dir_size = 4096; // Tamaño base de metadatos
        if self.used_size + dir_size > self.max_size {
            return Err(FsError::DiskFull);
        }

        let (parent_children, name) = self.resolve_path_for_create(path)?;

        if parent_children.contains_key(&name) {
            return Err(FsError::AlreadyExists);
        }

        let dir = FsEntry::new_directory(&name, permissions);
        parent_children.insert(name, Box::new(dir));
        self.used_size += dir_size;

        Ok(())
    }

    /// Leer un archivo
    pub fn read_file(&mut self, path: &str) -> FsResult<Vec<u8>> {
        let (entry, _) = self.resolve_path_mut(path)?;

        match entry {
            FsEntry::File { data, metadata } => {
                // Actualizar timestamp de acceso
                metadata.accessed = get_system_time();
                Ok(data.clone())
            }
            FsEntry::Directory { .. } => Err(FsError::IsDirectory),
        }
    }

    /// Escribir en un archivo
    pub fn write_file(&mut self, path: &str, data: &[u8]) -> FsResult<()> {
        // Primero obtener el tamaño actual del archivo para calcular la diferencia
        let current_size = if let Ok(metadata) = self.get_metadata(path) {
            metadata.size
        } else {
            0
        };

        let new_size = data.len() as u64;
        let size_diff = new_size as i64 - current_size as i64;

        // Verificar espacio disponible
        if size_diff > 0 && self.used_size + size_diff as u64 > self.max_size {
            return Err(FsError::DiskFull);
        }

        let (entry, _) = self.resolve_path_mut(path)?;

        match entry {
            FsEntry::File { data: file_data, metadata } => {
                file_data.clear();
                file_data.extend_from_slice(data);
                metadata.size = new_size;
                metadata.modified = get_system_time();
                self.used_size = (self.used_size as i64 + size_diff) as u64;

                Ok(())
            }
            FsEntry::Directory { .. } => Err(FsError::IsDirectory),
        }
    }

    /// Eliminar una entrada (archivo o directorio)
    pub fn remove(&mut self, path: &str) -> FsResult<()> {
        let path = path.trim_start_matches('/');
        let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        if components.is_empty() {
            return Err(FsError::InvalidName);
        }

        let mut current = &mut self.root;

        for (i, component) in components.iter().enumerate() {
            match current {
                FsEntry::Directory { children, .. } => {
                    if i == components.len() - 1 {
                        // Último componente - eliminarlo
                        if let Some(removed_entry) = children.remove(*component) {
                            // Liberar espacio
                            self.used_size -= removed_entry.size() * 4096; // Estimación simple
                            return Ok(());
                        } else {
                            return Err(FsError::NotFound);
                        }
                    } else {
                        // Componente intermedio
                        if let Some(child) = children.get_mut(*component) {
                            if let FsEntry::Directory { .. } = child.as_ref() {
                                current = child;
                            } else {
                                return Err(FsError::NotDirectory);
                            }
                        } else {
                            return Err(FsError::NotFound);
                        }
                    }
                }
                FsEntry::File { .. } => {
                    return Err(FsError::NotDirectory);
                }
            }
        }

        Err(FsError::NotFound)
    }

    /// Listar contenido de un directorio
    pub fn list_directory(&self, path: &str) -> FsResult<Vec<FileMetadata>> {
        let (entry, _) = self.resolve_path(path)?;

        match entry {
            FsEntry::Directory { children, .. } => {
                let mut result = Vec::new();
                for child in children.values() {
                    result.push(child.metadata().clone());
                }
                Ok(result)
            }
            FsEntry::File { .. } => Err(FsError::NotDirectory),
        }
    }

    /// Obtener metadatos de una entrada
    pub fn get_metadata(&self, path: &str) -> FsResult<FileMetadata> {
        let (entry, _) = self.resolve_path(path)?;
        Ok(entry.metadata().clone())
    }

    /// Verificar si una ruta existe
    pub fn exists(&self, path: &str) -> bool {
        self.resolve_path(path).is_ok()
    }

    /// Obtener estadísticas del sistema de archivos
    pub fn get_stats(&self) -> FsStats {
        FsStats {
            total_size: self.max_size,
            used_size: self.used_size,
            free_size: self.max_size - self.used_size,
        }
    }

    /// Copiar un archivo
    pub fn copy_file(&mut self, from_path: &str, to_path: &str) -> FsResult<()> {
        let data = self.read_file(from_path)?;
        let metadata = self.get_metadata(from_path)?;

        self.create_file(to_path, metadata.permissions)?;
        self.write_file(to_path, &data)
    }

    /// Mover/renombrar una entrada
    pub fn move_entry(&mut self, from_path: &str, to_path: &str) -> FsResult<()> {
        // Leer el contenido si es un archivo
        let content = if let Ok(data) = self.read_file(from_path) {
            Some(data)
        } else {
            None
        };

        let metadata = self.get_metadata(from_path)?;

        // Crear la nueva entrada
        match metadata.entry_type {
            FsEntryType::File => {
                self.create_file(to_path, metadata.permissions)?;
                if let Some(data) = content {
                    self.write_file(to_path, &data)?;
                }
            }
            FsEntryType::Directory => {
                self.create_directory(to_path, metadata.permissions)?;
            }
        }

        // Eliminar la entrada original
        self.remove(from_path)
    }
}

/// Estadísticas del sistema de archivos
#[derive(Debug, Clone)]
pub struct FsStats {
    /// Tamaño total en bytes
    pub total_size: u64,
    /// Espacio usado en bytes
    pub used_size: u64,
    /// Espacio libre en bytes
    pub free_size: u64,
}

impl FsStats {
    /// Calcular porcentaje de uso
    pub fn usage_percentage(&self) -> f64 {
        if self.total_size == 0 {
            0.0
        } else {
            (self.used_size as f64 / self.total_size as f64) * 100.0
        }
    }
}

/// Instancia global del sistema de archivos virtual
static mut VIRTUAL_FS: Option<VirtualFileSystem> = None;

/// Inicializar el sistema de archivos virtual
pub fn init_virtual_fs() -> FsResult<()> {
    unsafe {
        VIRTUAL_FS = Some(VirtualFileSystem::new_default());
    }

    // Crear estructura básica de directorios
    if let Some(fs) = get_virtual_fs() {
        fs.create_directory("/tmp", FilePermissions::default())?;
        fs.create_directory("/etc", FilePermissions::default())?;
        fs.create_directory("/var", FilePermissions::default())?;
        fs.create_directory("/home", FilePermissions::default())?;
    }

        // Logging disabled
    Ok(())
}

/// Obtener referencia al sistema de archivos virtual
pub fn get_virtual_fs() -> Option<&'static mut VirtualFileSystem> {
    unsafe {
        VIRTUAL_FS.as_mut()
    }
}

/// Crear un archivo con contenido
pub fn create_file_with_content(path: &str, content: &str) -> FsResult<()> {
    if let Some(fs) = get_virtual_fs() {
        fs.create_file(path, FilePermissions::default())?;
        fs.write_file(path, content.as_bytes())
    } else {
        Err(FsError::InternalError("Sistema de archivos no inicializado".to_string()))
    }
}

/// Leer archivo como string
pub fn read_file_as_string(path: &str) -> FsResult<String> {
    if let Some(fs) = get_virtual_fs() {
        let data = fs.read_file(path)?;
        String::from_utf8(data).map_err(|_| FsError::InternalError("Contenido no válido UTF-8".to_string()))
    } else {
        Err(FsError::InternalError("Sistema de archivos no inicializado".to_string()))
    }
}

/// Función helper para obtener tiempo del sistema (simulado)
fn get_system_time() -> u64 {
    // Contador simulado de tiempo
    static mut SYSTEM_TIME: u64 = 0;
    unsafe {
        SYSTEM_TIME += 1;
        SYSTEM_TIME
    }
}

// funciones de logging removidas

/// Demostración del sistema de archivos virtual
pub fn demo_virtual_filesystem() -> FsResult<()> {
        // Logging disabled

    if let Some(fs) = get_virtual_fs() {
        // Crear algunos archivos de ejemplo
        fs.create_file("/etc/hostname", FilePermissions::read_only())?;
        fs.write_file("/etc/hostname", b"eclipse-os")?;

        fs.create_file("/etc/passwd", FilePermissions::read_only())?;
        fs.write_file("/etc/passwd", b"root:x:0:0:root:/root:/bin/sh\nuser:x:1000:1000:user:/home/user:/bin/sh")?;

        fs.create_file("/tmp/demo.txt", FilePermissions::default())?;
        fs.write_file("/tmp/demo.txt", b"Este es un archivo de demostracion en el sistema de archivos virtual.")?;

        // Crear un directorio de usuario
        fs.create_directory("/home/user", FilePermissions::default())?;
        fs.create_file("/home/user/.bashrc", FilePermissions::default())?;
        fs.write_file("/home/user/.bashrc", b"export PATH=/bin:/usr/bin\nexport HOME=/home/user")?;

        // Listar directorios
        // Logging disabled
        let root_entries = fs.list_directory("/")?;
        for entry in root_entries {
        // Logging disabled
        }

        // Logging disabled
        let etc_entries = fs.list_directory("/etc")?;
        for entry in etc_entries {
        // Logging disabled
        }

        // Leer algunos archivos
        if let Ok(hostname) = read_file_as_string("/etc/hostname") {
        // Logging disabled
        }

        if let Ok(demo_content) = read_file_as_string("/tmp/demo.txt") {
        // Logging disabled
        }

        // Mostrar estadísticas
        let stats = fs.get_stats();
        // Logging disabled
        // Logging disabled
        // Logging disabled
        // Logging disabled
        // Logging disabled

        // Logging disabled
    }

    Ok(())
}
