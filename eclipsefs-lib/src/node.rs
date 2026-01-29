//! Definición de nodos de EclipseFS

use crate::error::{EclipseFSError, EclipseFSResult};
use crate::extent::ExtentTree;

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
    // Mejoras inspiradas en RedoxFS
    pub version: u32,              // Versión del nodo para Copy-on-Write
    pub parent_version: u32,       // Versión del padre cuando se creó
    pub is_snapshot: bool,         // Si es una copia CoW
    pub original_inode: u32,       // Inode original (para snapshots)
    pub checksum: u32,             // CRC32 del contenido del nodo
    // Nuevos campos para extent-based allocation (ext4/XFS)
    pub extent_tree: ExtentTree,   // Árbol de extents para archivos grandes
    pub use_extents: bool,         // Si este nodo usa extents o datos inline
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
    // Mejoras inspiradas en RedoxFS
    pub version: u32,              // Versión del nodo para Copy-on-Write
    pub parent_version: u32,       // Versión del padre cuando se creó
    pub is_snapshot: bool,         // Si es una copia CoW
    pub original_inode: u32,       // Inode original (para snapshots)
    pub checksum: u32,             // CRC32 del contenido del nodo
    // Nuevos campos para extent-based allocation (ext4/XFS)
    pub extent_tree: ExtentTree,   // Árbol de extents para archivos grandes
    pub use_extents: bool,         // Si este nodo usa extents o datos inline
}

impl EclipseFSNode {
    /// Crear un nuevo directorio (inspirado en RedoxFS)
    pub fn new_dir() -> Self {
        let now = Self::now();
        let mut node = Self {
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
            atime: now,
            mtime: now,
            ctime: now,
            nlink: 2, // . y ..
            // Nuevos campos RedoxFS
            version: 1,
            parent_version: 0,
            is_snapshot: false,
            original_inode: 0,
            // Extent-based allocation
            extent_tree: ExtentTree::new(),
            use_extents: false,
            checksum: 0,
        };
        node.update_checksum();
        node
    }

    /// Crear un nuevo archivo (inspirado en RedoxFS)
    pub fn new_file() -> Self {
        let now = Self::now();
        let mut node = Self {
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
            atime: now,
            mtime: now,
            ctime: now,
            nlink: 1,
            // Nuevos campos RedoxFS
            version: 1,
            parent_version: 0,
            is_snapshot: false,
            original_inode: 0,
            // Extent-based allocation
            extent_tree: ExtentTree::new(),
            use_extents: false,
            checksum: 0,
        };
        node.update_checksum();
        node
    }

    /// Crear un nuevo enlace simbólico (inspirado en RedoxFS)
    pub fn new_symlink(target: &str) -> Self {
        let now = Self::now();
        
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

        let mut node = Self {
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
            atime: now,
            mtime: now,
            ctime: now,
            nlink: 1,
            // Nuevos campos RedoxFS
            version: 1,
            parent_version: 0,
            is_snapshot: false,
            original_inode: 0,
            // Extent-based allocation
            extent_tree: ExtentTree::new(),
            use_extents: false,
            checksum: 0,
        };
        node.update_checksum();
        node
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
        self.update_checksum(); // Update checksum after data changes
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

    /// Función auxiliar para obtener timestamp actual
    fn now() -> u64 {
        #[cfg(feature = "std")]
        {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        }
        #[cfg(not(feature = "std"))]
        {
            // En no_std sin reloj del sistema, usar un contador incremental
            // o valor fijo. Este es un valor razonable por defecto.
            1640995200 // 2022-01-01 00:00:00 UTC
        }
    }
    
    /// Calcular checksum CRC32 del nodo (inspirado en RedoxFS)
    fn calculate_crc32(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFFFFFF;
        for &byte in data {
            crc ^= byte as u32;
            for _ in 0..8 {
                if crc & 1 != 0 {
                    crc = (crc >> 1) ^ 0xEDB88320;
                } else {
                    crc >>= 1;
                }
            }
        }
        crc ^ 0xFFFFFFFF
    }
    
    /// Actualizar checksum del nodo
    pub fn update_checksum(&mut self) {
        let node_data = self.serialize_for_checksum();
        self.checksum = Self::calculate_crc32(&node_data);
    }
    
    /// Serializar nodo para cálculo de checksum
    fn serialize_for_checksum(&self) -> heapless::Vec<u8, 1024> {
        let mut data = heapless::Vec::new();
        
        // Serializar campos críticos para checksum
        let _ = data.extend_from_slice(&(self.kind.clone() as u8).to_le_bytes());
        let _ = data.extend_from_slice(&self.size.to_le_bytes());
        let _ = data.extend_from_slice(&self.mode.to_le_bytes());
        let _ = data.extend_from_slice(&self.uid.to_le_bytes());
        let _ = data.extend_from_slice(&self.gid.to_le_bytes());
        let _ = data.extend_from_slice(&self.mtime.to_le_bytes());
        let _ = data.extend_from_slice(&self.version.to_le_bytes());
        let _ = data.extend_from_slice(&self.data);
        
        // Serializar children para directorios
        if self.kind == NodeKind::Directory {
            #[cfg(feature = "std")]
            {
                for (name, inode) in &self.children {
                    let _ = data.extend_from_slice(name.as_bytes());
                    let _ = data.extend_from_slice(&inode.to_le_bytes());
                }
            }
            #[cfg(not(feature = "std"))]
            {
                for (name, inode) in &self.children {
                    let _ = data.extend_from_slice(name.as_bytes());
                    let _ = data.extend_from_slice(&inode.to_le_bytes());
                }
            }
        }
        
        data
    }
    
    /// Verificar integridad del nodo
    pub fn verify_integrity(&self) -> EclipseFSResult<()> {
        let expected_checksum = Self::calculate_crc32(&self.serialize_for_checksum());
        if self.checksum != expected_checksum {
            return Err(EclipseFSError::InvalidFormat);
        }
        Ok(())
    }
    
    /// Crear snapshot Copy-on-Write del nodo (inspirado en RedoxFS)
    pub fn create_snapshot(&self, new_inode: u32) -> Self {
        let mut snapshot = self.clone();
        snapshot.version += 1;
        snapshot.parent_version = self.version;
        snapshot.is_snapshot = true;
        snapshot.original_inode = new_inode; // Se actualizará con el inode real
        snapshot.ctime = Self::now();
        snapshot.update_checksum();
        snapshot
    }
    
    /// Incrementar versión del nodo (para Copy-on-Write)
    pub fn increment_version(&mut self) {
        self.version += 1;
        self.ctime = Self::now();
        self.update_checksum();
    }
    
    /// Verificar si el nodo es una versión más reciente que otro
    pub fn is_newer_than(&self, other: &Self) -> bool {
        self.version > other.version
    }
}
