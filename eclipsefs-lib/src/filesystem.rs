//! Implementación del sistema de archivos EclipseFS

use crate::{
    Acl, AclEntry, AclEntryType, CompressionInfo, CompressionType, DfResult, EclipseFSError,
    EclipseFSHeader, EclipseFSNode, EclipseFSResult, EncryptionInfo, EncryptionType, FindResult,
    FsckResult, InodeTableEntry, NodeKind, Snapshot, TransparentEncryptionConfig,
};

#[cfg(feature = "std")]
use std::collections::HashMap;

#[cfg(not(feature = "std"))]
use heapless::{FnvIndexMap, String, Vec};

#[cfg(not(feature = "std"))]
// Tamaños máximos (menores) para escenarios no_std tempranos
const MAX_NODES: usize = 512; // Capacidad ampliada para imágenes reales
const MAX_DATA_SIZE: usize = 8 * 1024; // 8KB por archivo/symlink en memoria
const MAX_CHILDREN: usize = 256; // Hasta 256 hijos por directorio
const MAX_NAME_LEN: usize = 128; // Nombres hasta 128 caracteres

/// Estructura principal del sistema de archivos EclipseFS
#[cfg(feature = "std")]
pub struct EclipseFS {
    nodes: HashMap<u32, EclipseFSNode>,
    next_inode: u32,
    root_inode: u32,
    umask: u16,
}

#[cfg(not(feature = "std"))]
pub struct EclipseFS {
    nodes: FnvIndexMap<u32, EclipseFSNode, MAX_NODES>,
    next_inode: u32,
    root_inode: u32,
    umask: u16,
}

impl EclipseFS {
    /// Crear un nuevo sistema de archivos EclipseFS
    pub fn new() -> Self {
        let mut fs = Self {
            #[cfg(feature = "std")]
            nodes: HashMap::new(),
            #[cfg(not(feature = "std"))]
            nodes: FnvIndexMap::new(),
            next_inode: 1,
            root_inode: 1,
            umask: 0o022,
        };
        
        // Crear el directorio raíz
        let root_node = EclipseFSNode::new_dir();
        #[cfg(feature = "std")]
        fs.nodes.insert(fs.root_inode, root_node);
        #[cfg(not(feature = "std"))]
        let _ = fs.nodes.insert(fs.root_inode, root_node);
        
        fs
    }
    
    /// Obtener un nodo por su inode
    pub fn get_node(&self, inode: u32) -> Option<&EclipseFSNode> {
        self.nodes.get(&inode)
    }
    
    /// Obtener un nodo mutable por su inode
    pub fn get_node_mut(&mut self, inode: u32) -> Option<&mut EclipseFSNode> {
        self.nodes.get_mut(&inode)
    }
    
    /// Asignar un nuevo inode
    pub fn allocate_inode(&mut self) -> u32 {
        let inode = self.next_inode;
        self.next_inode += 1;
        inode
    }
    
    /// Agregar un nodo al sistema de archivos
    pub fn add_node(&mut self, inode: u32, node: EclipseFSNode) -> EclipseFSResult<()> {
        #[cfg(feature = "std")]
        {
            if self.nodes.contains_key(&inode) {
                return Err(EclipseFSError::DuplicateEntry);
            }
            self.nodes.insert(inode, node);
        }

        #[cfg(not(feature = "std"))]
        {
            if self.nodes.contains_key(&inode) {
                return Err(EclipseFSError::DuplicateEntry);
            }
            self.nodes
                .insert(inode, node)
                .map_err(|_| EclipseFSError::InvalidOperation)?;
        }
        
        Ok(())
    }
    
    /// Crear un archivo
    pub fn create_file(&mut self, parent_inode: u32, name: &str) -> EclipseFSResult<u32> {
        // Verificar que el padre existe y es un directorio
        {
            let parent_node = self
                .get_node(parent_inode)
                .ok_or(EclipseFSError::NotFound)?;
            
            if parent_node.kind != NodeKind::Directory {
                return Err(EclipseFSError::InvalidOperation);
            }
            
            if parent_node.has_child(name) {
                return Err(EclipseFSError::DuplicateEntry);
            }
        }
        
        let inode = self.allocate_inode();
        let file_node = EclipseFSNode::new_file();
        
        self.add_node(inode, file_node)?;
        
        // Agregar el hijo al padre
        let parent_node = self
            .get_node_mut(parent_inode)
            .ok_or(EclipseFSError::NotFound)?;
        parent_node.add_child(name, inode)?;
        
        Ok(inode)
    }
    
    /// Crear un directorio
    pub fn create_directory(&mut self, parent_inode: u32, name: &str) -> EclipseFSResult<u32> {
        // Verificar que el padre existe y es un directorio
        {
            let parent_node = self
                .get_node(parent_inode)
                .ok_or(EclipseFSError::NotFound)?;
            
            if parent_node.kind != NodeKind::Directory {
                return Err(EclipseFSError::InvalidOperation);
            }
            
            if parent_node.has_child(name) {
                return Err(EclipseFSError::DuplicateEntry);
            }
        }
        
        let inode = self.allocate_inode();
        let dir_node = EclipseFSNode::new_dir();
        
        self.add_node(inode, dir_node)?;
        
        // Agregar el hijo al padre
        let parent_node = self
            .get_node_mut(parent_inode)
            .ok_or(EclipseFSError::NotFound)?;
        parent_node.add_child(name, inode)?;
        
        Ok(inode)
    }
    
    /// Crear un enlace simbólico
    pub fn create_symlink(
        &mut self,
        parent_inode: u32,
        name: &str,
        target: &str,
    ) -> EclipseFSResult<u32> {
        // Verificar que el padre existe y es un directorio
        {
            let parent_node = self
                .get_node(parent_inode)
                .ok_or(EclipseFSError::NotFound)?;
            
            if parent_node.kind != NodeKind::Directory {
                return Err(EclipseFSError::InvalidOperation);
            }
            
            if parent_node.has_child(name) {
                return Err(EclipseFSError::DuplicateEntry);
            }
        }
        
        let inode = self.allocate_inode();
        let symlink_node = EclipseFSNode::new_symlink(target);
        
        self.add_node(inode, symlink_node)?;
        
        // Agregar el hijo al padre
        let parent_node = self
            .get_node_mut(parent_inode)
            .ok_or(EclipseFSError::NotFound)?;
        parent_node.add_child(name, inode)?;
        
        Ok(inode)
    }
    
    /// Buscar un nodo por path
    pub fn lookup_path(&self, path: &str) -> EclipseFSResult<u32> {
        if path.is_empty() || path == "/" {
            return Ok(self.root_inode);
        }
        
        #[cfg(feature = "std")]
        let components: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        #[cfg(not(feature = "std"))]
        let components: Vec<&str, 64> = path.trim_start_matches('/').split('/').collect();
        
        let mut current_inode = self.root_inode;
        
        for component in components.iter() {
            if component.is_empty() {
                continue;
            }
            
            let current_node = self
                .get_node(current_inode)
                .ok_or(EclipseFSError::NotFound)?;
            
            if current_node.kind != NodeKind::Directory {
                return Err(EclipseFSError::InvalidOperation);
            }
            
            current_inode = current_node
                .get_child_inode(component)
                .ok_or(EclipseFSError::NotFound)?;
        }
        
        Ok(current_inode)
    }
    
    /// Leer un archivo
    pub fn read_file(&self, inode: u32) -> EclipseFSResult<&[u8]> {
        let node = self.get_node(inode).ok_or(EclipseFSError::NotFound)?;
        
        if node.kind != NodeKind::File {
            return Err(EclipseFSError::InvalidOperation);
        }
        
        Ok(node.get_data())
    }
    
    /// Escribir en un archivo
    pub fn write_file(&mut self, inode: u32, data: &[u8]) -> EclipseFSResult<()> {
        let node = self.get_node_mut(inode).ok_or(EclipseFSError::NotFound)?;
        
        if node.kind != NodeKind::File {
            return Err(EclipseFSError::InvalidOperation);
        }
        
        node.set_data(data)?;
        Ok(())
    }
    
    /// Listar directorio
    #[cfg(feature = "std")]
    pub fn list_directory(&self, inode: u32) -> EclipseFSResult<Vec<String>> {
        let node = self.get_node(inode).ok_or(EclipseFSError::NotFound)?;
        
        if node.kind != NodeKind::Directory {
            return Err(EclipseFSError::InvalidOperation);
        }
        
        let mut entries = Vec::new();
        for (name, _) in node.get_children() {
            entries.push(name.clone());
        }
        
        Ok(entries)
    }

    #[cfg(not(feature = "std"))]
    pub fn list_directory(
        &self,
        inode: u32,
    ) -> EclipseFSResult<Vec<String<MAX_NAME_LEN>, MAX_CHILDREN>> {
        let node = self.get_node(inode).ok_or(EclipseFSError::NotFound)?;
        
        if node.kind != NodeKind::Directory {
            return Err(EclipseFSError::InvalidOperation);
        }
        
        let mut entries = Vec::new();
        for (name, _) in node.get_children() {
            entries
                .push(name.clone())
                .map_err(|_| EclipseFSError::InvalidOperation)?;
        }
        
        Ok(entries)
    }
    
    /// Remover un nodo
    pub fn remove(&mut self, parent_inode: u32, name: &str) -> EclipseFSResult<()> {
        let parent_node = self
            .get_node_mut(parent_inode)
            .ok_or(EclipseFSError::NotFound)?;
        
        if parent_node.kind != NodeKind::Directory {
            return Err(EclipseFSError::InvalidOperation);
        }
        
        let child_inode = parent_node
            .get_child_inode(name)
            .ok_or(EclipseFSError::NotFound)?;
        
        parent_node.remove_child(name)?;
        self.nodes.remove(&child_inode);
        
        Ok(())
    }
    
    /// Obtener estadísticas del sistema de archivos
    pub fn get_stats(&self) -> (u32, u32, u32) {
        let total_nodes = self.nodes.len() as u32;
        let total_files = self
            .nodes
            .values()
            .filter(|n| n.kind == NodeKind::File)
            .count() as u32;
        let total_dirs = self
            .nodes
            .values()
            .filter(|n| n.kind == NodeKind::Directory)
            .count() as u32;
        
        (total_nodes, total_files, total_dirs)
    }
    
    /// Establecer umask
    pub fn set_umask(&mut self, umask: u16) {
        self.umask = umask;
    }
    
    /// Obtener umask
    pub fn get_umask(&self) -> u16 {
        self.umask
    }
}

// Implementaciones stub para funcionalidades avanzadas
impl EclipseFS {
    // Funciones de cifrado
    pub fn encrypt_file(&mut self, _inode: u32, _key: &[u8]) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn decrypt_file(&mut self, _inode: u32, _key: &[u8]) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn is_encrypted(&self, _inode: u32) -> EclipseFSResult<bool> {
        Ok(false)
    }
    pub fn get_encryption_info(&self, _inode: u32) -> EclipseFSResult<EncryptionInfo> {
        Ok(EncryptionInfo::new())
    }
    pub fn add_encryption_key(&mut self, _key_id: u32, _key: &[u8]) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn rekey_file(
        &mut self,
        _inode: u32,
        _old_key: &[u8],
        _new_key: &[u8],
    ) -> EclipseFSResult<()> {
        Ok(())
    }
    
    // Funciones de compresión
    pub fn compress_file(
        &mut self,
        _inode: u32,
        _algorithm: CompressionType,
    ) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn decompress_file(&mut self, _inode: u32) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn is_compressed(&self, _inode: u32) -> EclipseFSResult<bool> {
        Ok(false)
    }
    pub fn get_compression_info(&self, _inode: u32) -> EclipseFSResult<CompressionInfo> { 
        Ok(CompressionInfo {
            compression_type: CompressionType::None,
            original_size: 0,
            compressed_size: 0,
        })
    }
    pub fn auto_compress_large_files(&mut self, _threshold: u64) -> EclipseFSResult<u32> {
        Ok(0)
    }
    pub fn get_compression_stats(&self) -> (u32, u32, f32) {
        (0, 0, 0.0)
    }
    
    // Funciones de snapshots
    pub fn create_snapshot(&mut self, _description: &str) -> EclipseFSResult<u64> {
        Ok(0)
    }
    #[cfg(feature = "std")]
    pub fn list_snapshots(&self) -> EclipseFSResult<Vec<Snapshot>> {
        Ok(Vec::new())
    }
    #[cfg(not(feature = "std"))]
    pub fn list_snapshots(&self) -> EclipseFSResult<Vec<Snapshot, 16>> {
        Ok(Vec::new())
    }
    pub fn get_snapshot(&self, _snapshot_id: &str) -> EclipseFSResult<Snapshot> {
        Ok(Snapshot::new())
    }
    pub fn restore_snapshot(&mut self, _snapshot_id: &str) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn delete_snapshot(&mut self, _snapshot_id: &str) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn get_snapshot_stats(&self) -> (u32, u64, u32) {
        (0, 0, 0)
    }
    pub fn auto_snapshot(&mut self, _interval_minutes: u32) -> EclipseFSResult<u64> {
        Ok(0)
    }
    pub fn cleanup_old_snapshots(&mut self, _keep_count: u32) -> EclipseFSResult<u32> {
        Ok(0)
    }
    pub fn compare_snapshots(
        &self,
        _snapshot_id1: &str,
        _snapshot_id2: &str,
    ) -> EclipseFSResult<(u32, u32, u32)> {
        Ok((0, 0, 0))
    }
    pub fn export_snapshot(&self, _snapshot_id: &str, _path: &str) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn import_snapshot(&mut self, _path: &str) -> EclipseFSResult<u64> {
        Ok(0)
    }
    
    // Funciones de ACL
    pub fn set_acl(&mut self, _inode: u32, _acl: Acl) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn get_acl(&self, _inode: u32) -> EclipseFSResult<Acl> { 
        Ok(Acl {
            entries: Vec::new(),
        })
    }
    pub fn remove_acl(&mut self, _inode: u32) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn set_default_acl(&mut self, _inode: u32, _acl: Acl) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn get_default_acl(&self, _inode: u32) -> EclipseFSResult<Acl> { 
        Ok(Acl {
            entries: Vec::new(),
        })
    }
    pub fn remove_default_acl(&mut self, _inode: u32) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn check_acl_permission(
        &self,
        _inode: u32,
        _uid: u32,
        _gid: u32,
        _permission: u32,
    ) -> EclipseFSResult<bool> {
        Ok(true)
    }
    pub fn copy_acl(&mut self, _src_inode: u32, _dst_inode: u32) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn inherit_default_acl(
        &mut self,
        _parent_inode: u32,
        _child_inode: u32,
    ) -> EclipseFSResult<()> {
        Ok(())
    }
    #[cfg(feature = "std")]
    pub fn list_acl_entries(&self, _inode: u32) -> EclipseFSResult<Vec<AclEntry>> {
        Ok(Vec::new())
    }
    #[cfg(not(feature = "std"))]
    pub fn list_acl_entries(&self, _inode: u32) -> EclipseFSResult<Vec<AclEntry, 16>> {
        Ok(Vec::new())
    }
    pub fn acl_exists(&self, _inode: u32) -> EclipseFSResult<bool> {
        Ok(false)
    }
    pub fn get_acl_stats(&self) -> (u32, u32) {
        (0, 0)
    }
    pub fn clear_all_acls(&mut self) -> EclipseFSResult<()> {
        Ok(())
    }
    
    // Funciones de cifrado transparente
    pub fn enable_transparent_encryption(
        &mut self,
        _config: TransparentEncryptionConfig,
    ) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn disable_transparent_encryption(&mut self) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn get_transparent_encryption_config(
        &self,
    ) -> EclipseFSResult<TransparentEncryptionConfig> {
        Ok(TransparentEncryptionConfig::new())
    }
    pub fn is_transparent_encryption_enabled(&self) -> EclipseFSResult<bool> {
        Ok(false)
    }
    pub fn set_transparent_encryption_config(
        &mut self,
        _config: TransparentEncryptionConfig,
    ) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn auto_encrypt_file(&mut self, _inode: u32) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn auto_encrypt_directory(&mut self, _inode: u32) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn encrypt_directory(&mut self, _inode: u32, _key: &[u8]) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn decrypt_directory(&mut self, _inode: u32, _key: &[u8]) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn is_directory_encrypted(&self, _inode: u32) -> EclipseFSResult<bool> {
        Ok(false)
    }
    pub fn get_directory_encryption_info(&self, _inode: u32) -> EclipseFSResult<EncryptionInfo> {
        Ok(EncryptionInfo::new())
    }
    #[cfg(feature = "std")]
    pub fn generate_directory_key(&mut self, _inode: u32) -> EclipseFSResult<Vec<u8>> {
        Ok(Vec::new())
    }
    #[cfg(not(feature = "std"))]
    pub fn generate_directory_key(&mut self, _inode: u32) -> EclipseFSResult<Vec<u8, 32>> {
        Ok(Vec::new())
    }
    #[cfg(feature = "std")]
    pub fn get_transparent_key(&self, _key_id: u32) -> EclipseFSResult<Vec<u8>> {
        Ok(Vec::new())
    }
    #[cfg(not(feature = "std"))]
    pub fn get_transparent_key(&self, _key_id: u32) -> EclipseFSResult<Vec<u8, 32>> {
        Ok(Vec::new())
    }
    pub fn set_transparent_key(&mut self, _key_id: u32, _key: &[u8]) -> EclipseFSResult<()> {
        Ok(())
    }
    #[cfg(feature = "std")]
    pub fn transparent_encrypt_data(&mut self, data: &[u8]) -> EclipseFSResult<Vec<u8>> {
        Ok(data.to_vec())
    }
    #[cfg(not(feature = "std"))]
    pub fn transparent_encrypt_data(
        &mut self,
        data: &[u8],
    ) -> EclipseFSResult<Vec<u8, MAX_DATA_SIZE>> {
        let mut result = Vec::new();
        result.extend_from_slice(data).ok();
        Ok(result)
    }
    #[cfg(feature = "std")]
    pub fn transparent_decrypt_data(&mut self, data: &[u8]) -> EclipseFSResult<Vec<u8>> {
        Ok(data.to_vec())
    }
    #[cfg(not(feature = "std"))]
    pub fn transparent_decrypt_data(
        &mut self,
        data: &[u8],
    ) -> EclipseFSResult<Vec<u8, MAX_DATA_SIZE>> {
        let mut result = Vec::new();
        result.extend_from_slice(data).ok();
        Ok(result)
    }
    pub fn rotate_transparent_keys(&mut self) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn get_transparent_encryption_stats(&self) -> (u32, u32, u32) {
        (0, 0, 0)
    }
    pub fn clear_transparent_encryption(&mut self) -> EclipseFSResult<()> {
        Ok(())
    }
    
    // Funciones de mantenimiento
    pub fn fsck(&self) -> EclipseFSResult<FsckResult> { 
        Ok(FsckResult {
            errors_found: 0,
            errors_fixed: 0,
            warnings: 0,
        })
    }
    pub fn df(&self) -> EclipseFSResult<DfResult> { 
        Ok(DfResult {
            total_blocks: 1000000,
            used_blocks: 100000,
            free_blocks: 900000,
        })
    }
    pub fn find_files(&self, _pattern: &str, _start_path: &str) -> EclipseFSResult<FindResult> {
        Ok(FindResult::new())
    }
    #[cfg(feature = "std")]
    pub fn get_filesystem_health(&self) -> EclipseFSResult<(f32, Vec<String>)> {
        Ok((1.0, Vec::new()))
    }
    #[cfg(not(feature = "std"))]
    pub fn get_filesystem_health(&self) -> EclipseFSResult<(f32, Vec<String<128>, 16>)> {
        Ok((1.0, Vec::new()))
    }
    pub fn get_detailed_stats(
        &self,
    ) -> EclipseFSResult<(usize, usize, usize, usize, usize, usize)> {
        Ok((0, 0, 0, 0, 0, 0))
    }
    
    // Funciones de serialización
    #[cfg(feature = "std")]
    pub fn dump_to_buffer(&self) -> EclipseFSResult<Vec<u8>> {
        Ok(Vec::new())
    }
    #[cfg(not(feature = "std"))]
    pub fn dump_to_buffer(&self) -> EclipseFSResult<Vec<u8, MAX_DATA_SIZE>> {
        Ok(Vec::new())
    }

#[cfg(not(feature = "std"))]
pub fn load_from_buffer(&mut self, data: &[u8]) -> EclipseFSResult<()> {
        let header = EclipseFSHeader::from_bytes(data)?;

        let table_offset = header.inode_table_offset as usize;
        let table_size = header.inode_table_size as usize;
        let end_table = table_offset
            .checked_add(table_size)
            .ok_or(EclipseFSError::InvalidFormat)?;

        if data.len() < end_table {
            return Err(EclipseFSError::InvalidFormat);
        }

        let mut inode_entries: Vec<InodeTableEntry, MAX_NODES> = Vec::new();

        for idx in 0..header.total_inodes {
            let entry_offset = table_offset
                .checked_add(idx as usize * crate::format::constants::INODE_TABLE_ENTRY_SIZE)
                .ok_or(EclipseFSError::InvalidFormat)?;

            if entry_offset + 8 > end_table {
                return Err(EclipseFSError::InvalidFormat);
            }

            let inode = u32::from_le_bytes([
                data[entry_offset],
                data[entry_offset + 1],
                data[entry_offset + 2],
                data[entry_offset + 3],
            ]) as u64;

            let rel_offset = u32::from_le_bytes([
                data[entry_offset + 4],
                data[entry_offset + 5],
                data[entry_offset + 6],
                data[entry_offset + 7],
            ]) as u64;

            let node_offset = header.inode_table_offset + header.inode_table_size + rel_offset;
            inode_entries
                .push(InodeTableEntry::new(inode, node_offset))
                .map_err(|_| EclipseFSError::InvalidOperation)?;
        }

        self.load_from_stream(&header, &inode_entries, |offset, buffer| {
            let start = offset as usize;
            let end = start
                .checked_add(buffer.len())
                .ok_or(EclipseFSError::InvalidFormat)?;

            if end > data.len() {
                return Err(EclipseFSError::InvalidFormat);
            }

            buffer.copy_from_slice(&data[start..end]);
            Ok(())
        })
    }

    #[cfg(not(feature = "std"))]
    pub fn load_from_stream<F>(
        &mut self,
        header: &EclipseFSHeader,
        inode_entries: &[InodeTableEntry],
        mut fetch: F,
    ) -> EclipseFSResult<()>
    where
        F: FnMut(u64, &mut [u8]) -> EclipseFSResult<()>,
    {
        use crate::format::{constants, tlv_tags};

        self.nodes.clear();
        self.next_inode = constants::ROOT_INODE + 1;
        self.root_inode = constants::ROOT_INODE;

        let mut max_inode = self.root_inode;

        let mut header_buf = [0u8; constants::NODE_RECORD_HEADER_SIZE];
        let mut tlv_header = [0u8; 6];
        let mut small_buf = [0u8; 16];
        let mut dir_buf = [0u8; 4096];
        let mut data_buf = [0u8; MAX_DATA_SIZE];

        for entry in inode_entries.iter() {
            let inode = entry.inode as u32;
            if inode == 0 {
                return Err(EclipseFSError::InvalidFormat);
            }

            if inode > max_inode {
                max_inode = inode;
            }

            fetch(entry.offset, &mut header_buf)?;

            let recorded_inode = u32::from_le_bytes([
                header_buf[0],
                header_buf[1],
                header_buf[2],
                header_buf[3],
            ]);

            let record_size = u32::from_le_bytes([
                header_buf[4],
                header_buf[5],
                header_buf[6],
                header_buf[7],
            ]) as u64;

            if recorded_inode != inode || record_size < constants::NODE_RECORD_HEADER_SIZE as u64 {
                return Err(EclipseFSError::InvalidFormat);
            }

            let mut node_type = NodeKind::File;
            let mut mode = 0o100644u32;
            let mut uid = 0u32;
            let mut gid = 0u32;
            let mut size = 0u64;
            let mut atime = 0u64;
            let mut mtime = 0u64;
            let mut ctime = 0u64;
            let mut nlink = 1u32;
            let mut data_len = 0usize;
            let mut children: Vec<(String<MAX_NAME_LEN>, u32), MAX_CHILDREN> = Vec::new();

            let mut cursor = entry.offset + constants::NODE_RECORD_HEADER_SIZE as u64;
            let end = entry.offset + record_size;

            while cursor < end {
                if end - cursor < 6 {
                    return Err(EclipseFSError::InvalidFormat);
                }

                fetch(cursor, &mut tlv_header)?;
                cursor += 6;

                let tag = u16::from_le_bytes([tlv_header[0], tlv_header[1]]);
                let length = u32::from_le_bytes([
                    tlv_header[2],
                    tlv_header[3],
                    tlv_header[4],
                    tlv_header[5],
                ]) as u64;

                if cursor + length > end {
                    return Err(EclipseFSError::InvalidFormat);
                }

                match tag {
                    tlv_tags::NODE_TYPE => {
                        if length == 0 || length > small_buf.len() as u64 {
                            return Err(EclipseFSError::InvalidFormat);
                        }
                        fetch(cursor, &mut small_buf[..length as usize])?;
                        node_type = match small_buf[0] {
                            1 => NodeKind::File,
                            2 => NodeKind::Directory,
                            3 => NodeKind::Symlink,
                            _ => return Err(EclipseFSError::InvalidFormat),
                        };
                    }
                    tlv_tags::MODE => {
                        if length != 4 {
                            return Err(EclipseFSError::InvalidFormat);
                        }
                        fetch(cursor, &mut small_buf[..4])?;
                        mode = u32::from_le_bytes([small_buf[0], small_buf[1], small_buf[2], small_buf[3]]);
                    }
                    tlv_tags::UID => {
                        if length != 4 {
                            return Err(EclipseFSError::InvalidFormat);
                        }
                        fetch(cursor, &mut small_buf[..4])?;
                        uid = u32::from_le_bytes([small_buf[0], small_buf[1], small_buf[2], small_buf[3]]);
                    }
                    tlv_tags::GID => {
                        if length != 4 {
                            return Err(EclipseFSError::InvalidFormat);
                        }
                        fetch(cursor, &mut small_buf[..4])?;
                        gid = u32::from_le_bytes([small_buf[0], small_buf[1], small_buf[2], small_buf[3]]);
                    }
                    tlv_tags::SIZE => {
                        if length != 8 {
                            return Err(EclipseFSError::InvalidFormat);
                        }
                        fetch(cursor, &mut small_buf[..8])?;
                        size = u64::from_le_bytes([
                            small_buf[0],
                            small_buf[1],
                            small_buf[2],
                            small_buf[3],
                            small_buf[4],
                            small_buf[5],
                            small_buf[6],
                            small_buf[7],
                        ]);
                    }
                    tlv_tags::ATIME => {
                        if length != 8 {
                            return Err(EclipseFSError::InvalidFormat);
                        }
                        fetch(cursor, &mut small_buf[..8])?;
                        atime = u64::from_le_bytes([
                            small_buf[0],
                            small_buf[1],
                            small_buf[2],
                            small_buf[3],
                            small_buf[4],
                            small_buf[5],
                            small_buf[6],
                            small_buf[7],
                        ]);
                    }
                    tlv_tags::MTIME => {
                        if length != 8 {
                            return Err(EclipseFSError::InvalidFormat);
                        }
                        fetch(cursor, &mut small_buf[..8])?;
                        mtime = u64::from_le_bytes([
                            small_buf[0],
                            small_buf[1],
                            small_buf[2],
                            small_buf[3],
                            small_buf[4],
                            small_buf[5],
                            small_buf[6],
                            small_buf[7],
                        ]);
                    }
                    tlv_tags::CTIME => {
                        if length != 8 {
                            return Err(EclipseFSError::InvalidFormat);
                        }
                        fetch(cursor, &mut small_buf[..8])?;
                        ctime = u64::from_le_bytes([
                            small_buf[0],
                            small_buf[1],
                            small_buf[2],
                            small_buf[3],
                            small_buf[4],
                            small_buf[5],
                            small_buf[6],
                            small_buf[7],
                        ]);
                    }
                    tlv_tags::NLINK => {
                        if length != 4 {
                            return Err(EclipseFSError::InvalidFormat);
                        }
                        fetch(cursor, &mut small_buf[..4])?;
                        nlink = u32::from_le_bytes([small_buf[0], small_buf[1], small_buf[2], small_buf[3]]);
                    }
                    tlv_tags::DIRECTORY_ENTRIES => {
                        let len = length as usize;
                        if len > dir_buf.len() {
                            return Err(EclipseFSError::InvalidOperation);
                        }
                        fetch(cursor, &mut dir_buf[..len])?;
                        let mut offset = 0usize;
                        while offset < len {
                            if offset + 8 > len {
                                return Err(EclipseFSError::InvalidFormat);
                            }
                            let name_len = u32::from_le_bytes([
                                dir_buf[offset],
                                dir_buf[offset + 1],
                                dir_buf[offset + 2],
                                dir_buf[offset + 3],
                            ]) as usize;
                            offset += 4;

                            let child_inode = u32::from_le_bytes([
                                dir_buf[offset],
                                dir_buf[offset + 1],
                                dir_buf[offset + 2],
                                dir_buf[offset + 3],
                            ]);
                            offset += 4;

                            if offset + name_len > len {
                                return Err(EclipseFSError::InvalidFormat);
                            }

                            let name_slice = &dir_buf[offset..offset + name_len];
                            offset += name_len;

                            let mut name = String::<MAX_NAME_LEN>::new();
                            name.push_str(core::str::from_utf8(name_slice).map_err(|_| EclipseFSError::InvalidFormat)?)
                                .map_err(|_| EclipseFSError::InvalidOperation)?;

                            children
                                .push((name, child_inode))
                                .map_err(|_| EclipseFSError::InvalidOperation)?;
                        }
                    }
                    tlv_tags::CONTENT => {
                        if length as usize <= data_buf.len() {
                            fetch(cursor, &mut data_buf[..length as usize])?;
                            data_len = length as usize;
                        } else {
                            let mut remaining = length;
                            let mut temp_offset = cursor;
                            while remaining > 0 {
                                let chunk = core::cmp::min(remaining, dir_buf.len() as u64);
                                fetch(temp_offset, &mut dir_buf[..chunk as usize])?;
                                temp_offset += chunk;
                                remaining -= chunk;
                            }
                        }
                    }
                    _ => {}
                }

                cursor += length;
            }

            let mut node = match node_type {
                NodeKind::File => {
                    let mut n = EclipseFSNode::new_file();
                    if data_len > 0 {
                        n.set_data(&data_buf[..core::cmp::min(data_len, data_buf.len())])?;
                    }
                    n
                }
                NodeKind::Directory => EclipseFSNode::new_dir(),
                NodeKind::Symlink => {
                    let target = if data_len > 0 {
                        core::str::from_utf8(&data_buf[..data_len]).unwrap_or("")
                    } else {
                        ""
                    };
                    EclipseFSNode::new_symlink(target)
                }
            };

            node.mode = mode;
            node.uid = uid;
            node.gid = gid;
            node.size = size;
            node.atime = atime;
            node.mtime = mtime;
            node.ctime = ctime;
            node.nlink = nlink;

            if matches!(node_type, NodeKind::Directory) {
                for (name, child_inode) in children.iter() {
                    node.add_child(name.as_str(), *child_inode)?;
                }
            }

            self.add_node(inode, node)?;
        }

        self.next_inode = max_inode + 1;
        Ok(())
    }
    pub fn save_to_file(&self, _path: &str) -> EclipseFSResult<()> {
        Ok(())
    }
    pub fn load_from_file(&mut self, _path: &str) -> EclipseFSResult<()> {
        Ok(())
    }
}
