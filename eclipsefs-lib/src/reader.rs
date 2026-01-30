//! Lector de imágenes EclipseFS

use crate::{
    format::constants, format::tlv_tags, EclipseFSError, EclipseFSHeader, EclipseFSNode,
    EclipseFSResult, InodeTableEntry, NodeKind,
};
use crate::arc_cache::AdaptiveReplacementCache;
use byteorder::{LittleEndian, ReadBytesExt};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};

/// Buffer size for I/O operations (512KB for better performance)
const BUFFER_SIZE: usize = 512 * 1024;

/// Maximum number of cached nodes (adjust based on memory constraints)
const MAX_CACHED_NODES: usize = 1024;

/// Tipo de cache a utilizar
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CacheType {
    /// LRU simple (Least Recently Used)
    LRU,
    /// ARC (Adaptive Replacement Cache) - Algoritmo "Arquera"
    ARC,
}

/// Lector de imágenes EclipseFS
pub struct EclipseFSReader {
    file: BufReader<File>,
    header: EclipseFSHeader,
    inode_table: Vec<InodeTableEntry>,
    /// Tipo de cache en uso
    cache_type: CacheType,
    /// Simple LRU cache for recently accessed nodes (usado cuando cache_type == LRU)
    lru_cache: HashMap<u32, EclipseFSNode>,
    /// Track access order for LRU eviction
    lru_access_order: Vec<u32>,
    /// ARC cache (usado cuando cache_type == ARC)
    arc_cache: Option<AdaptiveReplacementCache>,
}

impl EclipseFSReader {
    /// Crear un nuevo lector desde un archivo
    pub fn new(file_path: &str) -> EclipseFSResult<Self> {
        Self::new_with_cache(file_path, CacheType::LRU)
    }

    /// Crear un nuevo lector con tipo de cache específico
    pub fn new_with_cache(file_path: &str, cache_type: CacheType) -> EclipseFSResult<Self> {
        let file = File::open(file_path).map_err(|e| {
            // Proporcionar contexto adicional sobre el error
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                eprintln!("Error: Permiso denegado al abrir '{}'. Intenta ejecutar con 'sudo'", file_path);
                EclipseFSError::PermissionDenied
            } else {
                EclipseFSError::IoError
            }
        })?;
        
        // Wrap file with BufReader for much better performance
        let mut buffered_file = BufReader::with_capacity(BUFFER_SIZE, file);
        let header = Self::read_header(&mut buffered_file)?;
        let inode_table = Self::read_inode_table(&mut buffered_file, &header)?;

        Ok(Self {
            file: buffered_file,
            header,
            inode_table,
            cache_type,
            lru_cache: HashMap::new(),
            lru_access_order: Vec::new(),
            arc_cache: if cache_type == CacheType::ARC {
                Some(AdaptiveReplacementCache::new())
            } else {
                None
            },
        })
    }

    /// Crear un nuevo lector desde un File existente
    pub fn from_file(file: File) -> EclipseFSResult<Self> {
        Self::from_file_with_cache(file, CacheType::LRU)
    }

    /// Crear un nuevo lector desde un File con tipo de cache específico
    pub fn from_file_with_cache(file: File, cache_type: CacheType) -> EclipseFSResult<Self> {
        // Wrap file with BufReader for much better performance
        let mut buffered_file = BufReader::with_capacity(BUFFER_SIZE, file);
        let header = Self::read_header(&mut buffered_file)?;
        let inode_table = Self::read_inode_table(&mut buffered_file, &header)?;

        Ok(Self {
            file: buffered_file,
            header,
            inode_table,
            cache_type,
            lru_cache: HashMap::new(),
            lru_access_order: Vec::new(),
            arc_cache: if cache_type == CacheType::ARC {
                Some(AdaptiveReplacementCache::new())
            } else {
                None
            },
        })
    }

    /// Leer el header del sistema de archivos
    fn read_header(file: &mut BufReader<File>) -> EclipseFSResult<EclipseFSHeader> {
        let mut magic = [0u8; 9];
        file.read_exact(&mut magic)?;

        let version = file.read_u32::<LittleEndian>()?;
        let inode_table_offset = file.read_u64::<LittleEndian>()?;
        let inode_table_size = file.read_u64::<LittleEndian>()?;
        let total_inodes = file.read_u32::<LittleEndian>()?;

        let header = EclipseFSHeader {
            magic,
            version,
            inode_table_offset,
            inode_table_size,
            total_inodes,
            // Nuevos campos RedoxFS
            header_checksum: 0,
            metadata_checksum: 0,
            data_checksum: 0,
            creation_time: 0,
            last_check: 0,
            flags: 0,
        };

        header.validate()?;
        Ok(header)
    }

    /// Leer la tabla de inodos
    fn read_inode_table(
        file: &mut BufReader<File>,
        header: &EclipseFSHeader,
    ) -> EclipseFSResult<Vec<InodeTableEntry>> {
        file.seek(SeekFrom::Start(header.inode_table_offset))?;

        let mut entries = Vec::new();
        for _ in 0..header.total_inodes {
            let inode = file.read_u32::<LittleEndian>()? as u64;
            let rel_offset = file.read_u32::<LittleEndian>()? as u64;
            let absolute_offset = header.inode_table_offset + header.inode_table_size + rel_offset;
            entries.push(InodeTableEntry::new(inode, absolute_offset));
        }

        Ok(entries)
    }

    /// Leer un nodo por su inode
    pub fn read_node(&mut self, inode: u32) -> EclipseFSResult<EclipseFSNode> {
        // Check cache first based on cache type
        match self.cache_type {
            CacheType::ARC => {
                if let Some(ref mut arc) = self.arc_cache {
                    if let Some(node) = arc.get(inode) {
                        return Ok(node);
                    }
                }
            }
            CacheType::LRU => {
                if let Some(cached_node) = self.lru_cache.get(&inode) {
                    // Update LRU access order
                    self.lru_access_order.retain(|&i| i != inode);
                    self.lru_access_order.push(inode);
                    return Ok(cached_node.clone());
                }
            }
        }

        let entry = self
            .inode_table
            .get(inode as usize - 1)
            .ok_or(EclipseFSError::NotFound)?;

        self.file.seek(SeekFrom::Start(entry.offset))?;

        let mut header = [0u8; constants::NODE_RECORD_HEADER_SIZE];
        self.file.read_exact(&mut header)?;

        let recorded_inode = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let record_size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;

        if recorded_inode != inode {
            return Err(EclipseFSError::InvalidFormat);
        }

        if record_size < constants::NODE_RECORD_HEADER_SIZE {
            return Err(EclipseFSError::InvalidFormat);
        }

        let tlv_size = record_size - constants::NODE_RECORD_HEADER_SIZE;
        let mut tlv_data = vec![0u8; tlv_size];
        self.file.read_exact(&mut tlv_data)?;

        // Leer TLV entries
        let mut node_type = NodeKind::File;
        let mut mode = 0o100644;
        let mut uid = 0;
        let mut gid = 0;
        let mut size = 0u64;
        let mut atime = 0u64;
        let mut mtime = 0u64;
        let mut ctime = 0u64;
        let mut nlink = 1u32;
        let mut data = Vec::new();
        let mut children = std::collections::HashMap::new();

        let mut offset = 0;

        loop {
            if offset + 6 > tlv_data.len() {
                break;
            }

            let tag = u16::from_le_bytes([tlv_data[offset], tlv_data[offset + 1]]);
            let length = u32::from_le_bytes([
                tlv_data[offset + 2],
                tlv_data[offset + 3],
                tlv_data[offset + 4],
                tlv_data[offset + 5],
            ]) as usize;
            offset += 6;

            if offset + length > tlv_data.len() {
                break;
            }

            let value = &tlv_data[offset..offset + length];
            offset += length;

            match tag {
                tlv_tags::NODE_TYPE => {
                    if !value.is_empty() {
                        node_type = match value[0] {
                            1 => NodeKind::File,
                            2 => NodeKind::Directory,
                            3 => NodeKind::Symlink,
                            _ => return Err(EclipseFSError::InvalidFormat),
                        };
                    }
                }
                tlv_tags::MODE => {
                    if value.len() >= 4 {
                        mode = u32::from_le_bytes([value[0], value[1], value[2], value[3]]);
                    }
                }
                tlv_tags::UID => {
                    if value.len() >= 4 {
                        uid = u32::from_le_bytes([value[0], value[1], value[2], value[3]]);
                    }
                }
                tlv_tags::GID => {
                    if value.len() >= 4 {
                        gid = u32::from_le_bytes([value[0], value[1], value[2], value[3]]);
                    }
                }
                tlv_tags::SIZE => {
                    if value.len() >= 8 {
                        size = u64::from_le_bytes([
                            value[0], value[1], value[2], value[3], value[4], value[5], value[6],
                            value[7],
                        ]);
                    }
                }
                tlv_tags::ATIME => {
                    if value.len() >= 8 {
                        atime = u64::from_le_bytes([
                            value[0], value[1], value[2], value[3], value[4], value[5], value[6],
                            value[7],
                        ]);
                    }
                }
                tlv_tags::MTIME => {
                    if value.len() >= 8 {
                        mtime = u64::from_le_bytes([
                            value[0], value[1], value[2], value[3], value[4], value[5], value[6],
                            value[7],
                        ]);
                    }
                }
                tlv_tags::CTIME => {
                    if value.len() >= 8 {
                        ctime = u64::from_le_bytes([
                            value[0], value[1], value[2], value[3], value[4], value[5], value[6],
                            value[7],
                        ]);
                    }
                }
                tlv_tags::NLINK => {
                    if value.len() >= 4 {
                        nlink = u32::from_le_bytes([value[0], value[1], value[2], value[3]]);
                    }
                }
                tlv_tags::CONTENT => {
                    data = value.to_vec();
                }
                tlv_tags::DIRECTORY_ENTRIES => {
                    children = Self::deserialize_directory_entries(&value)?;
                }
                _ => {
                    // Ignorar tags desconocidos
                }
            }
        }

        let node = EclipseFSNode {
            kind: node_type,
            data,
            children,
            size,
            mode,
            uid,
            gid,
            atime,
            mtime,
            ctime,
            nlink,
            // Nuevos campos RedoxFS
            version: 1,
            parent_version: 0,
            is_snapshot: false,
            original_inode: 0,
            checksum: 0,
            // Extent-based allocation
            extent_tree: crate::extent::ExtentTree::new(),
            use_extents: false,
        };

        // Cache the node for future reads
        self.cache_node(inode, node.clone());

        Ok(node)
    }

    /// Cache a node and manage eviction based on cache type
    fn cache_node(&mut self, inode: u32, node: EclipseFSNode) {
        match self.cache_type {
            CacheType::ARC => {
                if let Some(ref mut arc) = self.arc_cache {
                    arc.put(inode, node);
                }
            }
            CacheType::LRU => {
                // Evict oldest entry if cache is full
                if self.lru_cache.len() >= MAX_CACHED_NODES {
                    if let Some(oldest_inode) = self.lru_access_order.first().copied() {
                        self.lru_cache.remove(&oldest_inode);
                        self.lru_access_order.remove(0);
                    }
                }

                // Add to cache
                self.lru_cache.insert(inode, node);
                self.lru_access_order.push(inode);
            }
        }
    }

    /// Deserializar entradas de directorio
    fn deserialize_directory_entries(
        data: &[u8],
    ) -> EclipseFSResult<std::collections::HashMap<String, u32>> {
        let mut entries = std::collections::HashMap::new();
        let mut offset = 0;

        while offset < data.len() {
            if offset + 4 > data.len() {
                break;
            }

            let name_len = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize;
            offset += 4;

            if offset + 4 > data.len() {
                break;
            }

            let child_inode = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]);
            offset += 4;

            if offset + name_len > data.len() {
                break;
            }

            let name = String::from_utf8(data[offset..offset + name_len].to_vec())
                .map_err(|_| EclipseFSError::InvalidFormat)?;
            offset += name_len;

            // Deduplicate: Only insert if not already present
            // HashMap::insert would overwrite, but we explicitly check to detect issues
            if entries.contains_key(&name) {
                eprintln!("WARNING: Duplicate directory entry '{}' found during deserialization, skipping", name);
                continue;
            }
            entries.insert(name, child_inode);
        }

        Ok(entries)
    }

    /// Resolver path a inode
    pub fn lookup_path(&mut self, path: &str) -> EclipseFSResult<u32> {
        if path.is_empty() || path == "/" {
            return Ok(constants::ROOT_INODE);
        }

        let components: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let mut current_inode = constants::ROOT_INODE;

        for component in components {
            if component.is_empty() {
                continue;
            }

            let current_node = self.read_node(current_inode)?;

            if current_node.kind != NodeKind::Directory {
                return Err(EclipseFSError::InvalidOperation);
            }

            current_inode = current_node
                .get_child_inode(component)
                .ok_or(EclipseFSError::NotFound)?;
        }

        Ok(current_inode)
    }

    /// Obtener el header
    pub fn get_header(&self) -> &EclipseFSHeader {
        &self.header
    }

    /// Obtener la tabla de inodos
    pub fn get_inode_table(&self) -> &[InodeTableEntry] {
        &self.inode_table
    }

    /// Obtener el nodo raíz
    pub fn get_root(&mut self) -> EclipseFSResult<EclipseFSNode> {
        self.read_node(constants::ROOT_INODE)
    }

    /// Buscar un hijo en un directorio
    pub fn lookup(&mut self, parent_inode: u64, name: &str) -> EclipseFSResult<u64> {
        let parent = self.read_node(parent_inode as u32)?;
        
        if parent.kind != NodeKind::Directory {
            return Err(EclipseFSError::InvalidOperation);
        }
        
        parent.get_child_inode(name)
            .map(|inode| inode as u64)
            .ok_or(EclipseFSError::NotFound)
    }

    /// Obtener un nodo por su inode
    pub fn get_node(&mut self, inode: u64) -> EclipseFSResult<EclipseFSNode> {
        self.read_node(inode as u32)
    }

    /// Prefetch multiple nodes at once for better performance
    /// This is especially useful for directory listings
    pub fn prefetch_nodes(&mut self, inodes: &[u32]) -> EclipseFSResult<()> {
        for &inode in inodes {
            // Only prefetch if not already cached
            let already_cached = match self.cache_type {
                CacheType::ARC => {
                    // For ARC, we check by attempting a get (which updates internal state anyway)
                    // If it's cached, we'll get a hit; if not, we'll load it below
                    false // Always try to load, ARC will handle efficiently
                }
                CacheType::LRU => self.lru_cache.contains_key(&inode),
            };
            
            if !already_cached {
                // Ignore errors during prefetch - best effort
                let _ = self.read_node(inode);
            }
        }
        Ok(())
    }

    /// Get cache statistics
    pub fn get_cache_stats(&self) -> CacheStats {
        match self.cache_type {
            CacheType::LRU => CacheStats::LRU {
                cached_nodes: self.lru_cache.len(),
                cache_capacity: MAX_CACHED_NODES,
            },
            CacheType::ARC => {
                if let Some(ref arc) = self.arc_cache {
                    CacheStats::ARC(arc.stats())
                } else {
                    CacheStats::LRU {
                        cached_nodes: 0,
                        cache_capacity: MAX_CACHED_NODES,
                    }
                }
            }
        }
    }

    /// Get current cache type
    pub fn get_cache_type(&self) -> CacheType {
        self.cache_type
    }

    /// Read a directory node and automatically prefetch all its children
    /// This is optimized for directory listing operations
    pub fn read_directory_with_children(&mut self, inode: u32) -> EclipseFSResult<EclipseFSNode> {
        let dir_node = self.read_node(inode)?;
        
        // Only prefetch for directories
        if dir_node.kind == NodeKind::Directory {
            let child_inodes: Vec<u32> = dir_node.get_children().values().copied().collect();
            let _ = self.prefetch_nodes(&child_inodes);
        }
        
        Ok(dir_node)
    }
}

/// Cache statistics for monitoring
#[derive(Debug, Clone)]
pub enum CacheStats {
    LRU {
        cached_nodes: usize,
        cache_capacity: usize,
    },
    ARC(crate::arc_cache::ARCStats),
}

impl CacheStats {
    pub fn print(&self) {
        match self {
            CacheStats::LRU { cached_nodes, cache_capacity } => {
                println!("=== LRU Cache Statistics ===");
                println!("Cached nodes: {}/{}", cached_nodes, cache_capacity);
            }
            CacheStats::ARC(stats) => {
                stats.print();
            }
        }
    }
}
