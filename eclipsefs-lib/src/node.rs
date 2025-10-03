//! Definición de nodos de EclipseFS

use crate::error::{EclipseFSError, EclipseFSResult};

#[cfg(feature = "std")]
use std::collections::HashMap;

#[cfg(not(feature = "std"))]
use heapless::{FnvIndexMap, String, Vec};

#[cfg(not(feature = "std"))]
// Tamaños máximos coherentes con escenarios no_std reducidos
pub const MAX_DATA_SIZE: usize = 8 * 1024; // 8KB por archivo/symlink
pub const MAX_CHILDREN: usize = 256; // Hasta 256 entradas por directorio
pub const MAX_NAME_LEN: usize = 128; // Nombres de hasta 128 caracteres

#[derive(Debug, Clone, PartialEq)]
pub enum NodeKind {
    File,
    Directory,
    Symlink,
}

#[cfg(feature = "std")]
#[derive(Debug, Clone)]
pub struct EclipseFSNode {
    pub kind: NodeKind,
    pub data: Vec<u8>,
    pub children: HashMap<String, u32>,
    pub size: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub nlink: u32,
}

#[cfg(not(feature = "std"))]
#[derive(Debug, Clone)]
pub struct EclipseFSNode {
    pub kind: NodeKind,
    pub data: Vec<u8, MAX_DATA_SIZE>,
    pub children: FnvIndexMap<String<MAX_NAME_LEN>, u32, MAX_CHILDREN>,
    pub size: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub nlink: u32,
}

impl EclipseFSNode {
    /// Crear un nuevo directorio
    pub fn new_dir() -> Self {
        Self {
            kind: NodeKind::Directory,
            #[cfg(feature = "std")]
            data: Vec::new(),
            #[cfg(not(feature = "std"))]
            data: Vec::new(),
            #[cfg(feature = "std")]
            children: HashMap::new(),
            #[cfg(not(feature = "std"))]
            children: FnvIndexMap::new(),
            size: 0,
            mode: 0o40755,
            uid: 0,
            gid: 0,
            atime: Self::now(),
            mtime: Self::now(),
            ctime: Self::now(),
            nlink: 2, // . y ..
        }
    }

    /// Crear un nuevo archivo
    pub fn new_file() -> Self {
        Self {
            kind: NodeKind::File,
            #[cfg(feature = "std")]
            data: Vec::new(),
            #[cfg(not(feature = "std"))]
            data: Vec::new(),
            #[cfg(feature = "std")]
            children: HashMap::new(),
            #[cfg(not(feature = "std"))]
            children: FnvIndexMap::new(),
            size: 0,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            atime: Self::now(),
            mtime: Self::now(),
            ctime: Self::now(),
            nlink: 1,
        }
    }

    /// Crear un nuevo enlace simbólico
    pub fn new_symlink(target: &str) -> Self {
        #[cfg(feature = "std")]
        let data = target.as_bytes().to_vec();

        #[cfg(not(feature = "std"))]
        let mut data = Vec::new();
        #[cfg(not(feature = "std"))]
        let target_bytes = target.as_bytes();
        #[cfg(not(feature = "std"))]
        if target_bytes.len() <= MAX_DATA_SIZE {
            data.extend_from_slice(target_bytes).ok();
        }

        Self {
            kind: NodeKind::Symlink,
            data,
            #[cfg(feature = "std")]
            children: HashMap::new(),
            #[cfg(not(feature = "std"))]
            children: FnvIndexMap::new(),
            size: target.len() as u64,
            mode: 0o120777,
            uid: 0,
            gid: 0,
            atime: Self::now(),
            mtime: Self::now(),
            ctime: Self::now(),
            nlink: 1,
        }
    }

    /// Agregar un hijo al directorio
    pub fn add_child(&mut self, name: &str, inode: u32) -> EclipseFSResult<()> {
        if self.kind != NodeKind::Directory {
            return Err(EclipseFSError::InvalidOperation);
        }

        #[cfg(feature = "std")]
        {
            if self.children.contains_key(name) {
                return Err(EclipseFSError::DuplicateEntry);
            }
            self.children.insert(name.to_string(), inode);
        }

        #[cfg(not(feature = "std"))]
        {
            let name_bytes = name.as_bytes();
            if name_bytes.len() > MAX_NAME_LEN {
                return Err(EclipseFSError::InvalidOperation);
            }

            let mut name_str = String::new();
            name_str
                .push_str(name)
                .map_err(|_| EclipseFSError::InvalidOperation)?;

            if self.children.contains_key(&name_str) {
                return Err(EclipseFSError::DuplicateEntry);
            }

            self.children
                .insert(name_str, inode)
                .map_err(|_| EclipseFSError::InvalidOperation)?;
        }

        self.mtime = Self::now();
        Ok(())
    }

    /// Remover un hijo del directorio
    pub fn remove_child(&mut self, name: &str) -> EclipseFSResult<()> {
        if self.kind != NodeKind::Directory {
            return Err(EclipseFSError::InvalidOperation);
        }

        #[cfg(feature = "std")]
        {
            if self.children.remove(name).is_none() {
                return Err(EclipseFSError::NotFound);
            }
        }

        #[cfg(not(feature = "std"))]
        {
            let name_bytes = name.as_bytes();
            if name_bytes.len() > MAX_NAME_LEN {
                return Err(EclipseFSError::InvalidOperation);
            }

            let mut name_str = String::new();
            name_str
                .push_str(name)
                .map_err(|_| EclipseFSError::InvalidOperation)?;

            if self.children.remove(&name_str).is_none() {
                return Err(EclipseFSError::NotFound);
            }
        }

        self.mtime = Self::now();
        Ok(())
    }

    /// Establecer los datos del archivo
    pub fn set_data(&mut self, data: &[u8]) -> EclipseFSResult<()> {
        if self.kind != NodeKind::File && self.kind != NodeKind::Symlink {
            return Err(EclipseFSError::InvalidOperation);
        }

        #[cfg(feature = "std")]
        {
            self.data = data.to_vec();
        }

        #[cfg(not(feature = "std"))]
        {
            if data.len() > MAX_DATA_SIZE {
                return Err(EclipseFSError::InvalidOperation);
            }

            self.data.clear();
            self.data
                .extend_from_slice(data)
                .map_err(|_| EclipseFSError::InvalidOperation)?;
        }

        self.size = data.len() as u64;
        self.mtime = Self::now();
        Ok(())
    }

    /// Obtener el tamaño de los datos
    pub fn get_data_size(&self) -> usize {
        self.data.len()
    }

    /// Obtener los datos del archivo
    pub fn get_data(&self) -> &[u8] {
        &self.data
    }

    /// Obtener el número de hijos
    pub fn get_child_count(&self) -> usize {
        self.children.len()
    }

    /// Verificar si tiene un hijo específico
    pub fn has_child(&self, name: &str) -> bool {
        #[cfg(feature = "std")]
        {
            self.children.contains_key(name)
        }

        #[cfg(not(feature = "std"))]
        {
            let name_bytes = name.as_bytes();
            if name_bytes.len() > MAX_NAME_LEN {
                return false;
            }

            let mut name_str = String::new();
            if name_str.push_str(name).is_err() {
                return false;
            }

            self.children.contains_key(&name_str)
        }
    }

    /// Obtener el inode de un hijo
    pub fn get_child_inode(&self, name: &str) -> Option<u32> {
        #[cfg(feature = "std")]
        {
            self.children.get(name).copied()
        }

        #[cfg(not(feature = "std"))]
        {
            let name_bytes = name.as_bytes();
            if name_bytes.len() > MAX_NAME_LEN {
                return None;
            }

            let mut name_str = String::new();
            if name_str.push_str(name).is_err() {
                return None;
            }

            self.children.get(&name_str).copied()
        }
    }

    /// Obtener todos los hijos
    #[cfg(feature = "std")]
    pub fn get_children(&self) -> &HashMap<String, u32> {
        &self.children
    }

    #[cfg(not(feature = "std"))]
    pub fn get_children(&self) -> &FnvIndexMap<String<MAX_NAME_LEN>, u32, MAX_CHILDREN> {
        &self.children
    }

    /// Actualizar timestamp de acceso
    pub fn touch_access(&mut self) {
        self.atime = Self::now();
    }

    /// Actualizar timestamp de modificación
    pub fn touch_modification(&mut self) {
        self.mtime = Self::now();
    }

    /// Actualizar timestamp de cambio
    pub fn touch_change(&mut self) {
        self.ctime = Self::now();
    }

    /// Función auxiliar para obtener timestamp actual (stub)
    fn now() -> u64 {
        // En un sistema real, esto debería obtener el timestamp actual
        // Para ahora, retornamos un valor fijo
        1640995200 // 2022-01-01 00:00:00 UTC
    }
}
