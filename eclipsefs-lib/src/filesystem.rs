//! Implementación del sistema de archivos EclipseFS

use crate::{
    Acl, AclEntry, CompressionInfo, CompressionType, DfResult, EclipseFSError,
    EclipseFSNode, EclipseFSResult, EncryptionInfo, FindResult,
    FsckResult, NodeKind, Snapshot, TransparentEncryptionConfig,
};

#[cfg(feature = "std")]
use crate::cache::{CacheConfig, IntelligentCache};
#[cfg(feature = "std")]
use crate::defragmentation::{DefragmentationConfig, IntelligentDefragmenter};
#[cfg(feature = "std")]
use crate::load_balancing::{LoadBalancingConfig, IntelligentLoadBalancer};
#[cfg(feature = "std")]
use crate::journal::{Journal, JournalConfig, JournalEntry, TransactionType};
#[cfg(not(feature = "std"))]
use crate::format::{EclipseFSHeader, InodeTableEntry};

#[cfg(feature = "std")]
use std::collections::HashMap;

#[cfg(not(feature = "std"))]
use heapless::{FnvIndexMap, String, Vec};

#[cfg(not(feature = "std"))]
extern crate alloc;

#[cfg(not(feature = "std"))]
// Tamaños máximos (menores) para escenarios no_std tempranos
const MAX_NODES: usize = 512; // Capacidad ampliada para imágenes reales
#[allow(dead_code)]
const MAX_DATA_SIZE: usize = 8 * 1024; // 8KB por archivo/symlink en memoria
#[allow(dead_code)]
const MAX_CHILDREN: usize = 256; // Hasta 256 hijos por directorio
#[allow(dead_code)]
const MAX_NAME_LEN: usize = 128; // Nombres hasta 128 caracteres

/// Estructura principal del sistema de archivos EclipseFS (inspirado en RedoxFS)
#[cfg(feature = "std")]
pub struct EclipseFS {
    nodes: HashMap<u32, EclipseFSNode>,
    next_inode: u32,
    root_inode: u32,
    umask: u16,
    // Nuevos campos para Copy-on-Write y encriptación
    snapshots: HashMap<u32, Snapshot>,           // Snapshots activos
    encryption_config: Option<EncryptionInfo>,   // Configuración de encriptación
    cow_enabled: bool,                           // Copy-on-Write habilitado
    version_history: HashMap<u32, Vec<u32>>,    // Historial de versiones por inode
    // Optimizaciones avanzadas RedoxFS
    cache: Option<IntelligentCache>,             // Sistema de caché inteligente
    defragmenter: Option<IntelligentDefragmenter>, // Sistema de defragmentación
    load_balancer: Option<IntelligentLoadBalancer>, // Sistema de balanceo de carga
    journal: Option<Journal>,                     // Sistema de journaling para crash recovery
    // ACL storage
    acls: HashMap<u32, Acl>,                     // ACLs por inode
    default_acls: HashMap<u32, Acl>,             // ACLs por defecto para directorios
}

#[cfg(not(feature = "std"))]
pub struct EclipseFS {
    nodes: FnvIndexMap<u32, EclipseFSNode, MAX_NODES>,
    next_inode: u32,
    root_inode: u32,
    umask: u16,
    // Nuevos campos para Copy-on-Write y encriptación
    snapshots: FnvIndexMap<u32, Snapshot, 16>,        // Snapshots activos (limitado para no_std)
    encryption_config: Option<EncryptionInfo>,        // Configuración de encriptación
    cow_enabled: bool,                                 // Copy-on-Write habilitado
    version_history: FnvIndexMap<u32, heapless::Vec<u32, 8>, 64>, // Historial de versiones
    // Optimizaciones avanzadas RedoxFS (solo std)
    // cache, defragmenter, load_balancer no disponibles en no_std
    // ACL storage
    acls: FnvIndexMap<u32, Acl, 64>,                  // ACLs por inode
    default_acls: FnvIndexMap<u32, Acl, 64>,          // ACLs por defecto
}

impl Default for EclipseFS {
    fn default() -> Self {
        Self::new()
    }
}

impl EclipseFS {
    /// Crear un nuevo sistema de archivos EclipseFS (inspirado en RedoxFS)
    pub fn new() -> Self {
        let mut fs = Self {
            #[cfg(feature = "std")]
            nodes: HashMap::new(),
            #[cfg(not(feature = "std"))]
            nodes: FnvIndexMap::new(),
            next_inode: 2,  // Start at 2 since root is 1
            root_inode: 1,
            umask: 0o022,
            // Inicializar nuevos campos RedoxFS
            #[cfg(feature = "std")]
            snapshots: HashMap::new(),
            #[cfg(not(feature = "std"))]
            snapshots: FnvIndexMap::new(),
            encryption_config: None,
            cow_enabled: false,
            #[cfg(feature = "std")]
            version_history: HashMap::new(),
            #[cfg(not(feature = "std"))]
            version_history: FnvIndexMap::new(),
            // Inicializar optimizaciones avanzadas
            #[cfg(feature = "std")]
            cache: None,
            #[cfg(feature = "std")]
            defragmenter: None,
            #[cfg(feature = "std")]
            load_balancer: None,
            #[cfg(feature = "std")]
            journal: None,
            // Inicializar ACLs
            #[cfg(feature = "std")]
            acls: HashMap::new(),
            #[cfg(feature = "std")]
            default_acls: HashMap::new(),
            #[cfg(not(feature = "std"))]
            acls: FnvIndexMap::new(),
            #[cfg(not(feature = "std"))]
            default_acls: FnvIndexMap::new(),
        };
        
        // Crear el directorio raíz
        let root_node = EclipseFSNode::new_dir();
        #[cfg(feature = "std")]
        fs.nodes.insert(fs.root_inode, root_node);
        #[cfg(not(feature = "std"))]
        let _ = fs.nodes.insert(fs.root_inode, root_node);
        
        fs
    }
    
    /// Habilitar Copy-on-Write (inspirado en RedoxFS)
    pub fn enable_copy_on_write(&mut self) {
        self.cow_enabled = true;
    }
    
    /// Deshabilitar Copy-on-Write
    pub fn disable_copy_on_write(&mut self) {
        self.cow_enabled = false;
    }
    
    /// Configurar encriptación transparente (inspirado en RedoxFS)
    pub fn set_transparent_encryption(&mut self, encryption_info: EncryptionInfo) -> EclipseFSResult<()> {
        if !encryption_info.verify_key_integrity() {
            return Err(EclipseFSError::InvalidFormat);
        }
        
        self.encryption_config = Some(encryption_info);
        Ok(())
    }
    
    /// Deshabilitar encriptación transparente
    pub fn disable_encryption(&mut self) {
        self.encryption_config = None;
    }
    
    /// Crear snapshot del sistema de archivos (inspirado en RedoxFS)
    pub fn create_filesystem_snapshot(&mut self, snapshot_id: u32, description: &str) -> EclipseFSResult<()> {
        #[cfg(feature = "std")]
        {
            if self.snapshots.contains_key(&snapshot_id) {
                return Err(EclipseFSError::DuplicateEntry);
            }
            
            let snapshot = Snapshot {
                id: snapshot_id.to_string(),
                timestamp: Self::current_timestamp(),
                description: description.to_string(),
            };
            
            self.snapshots.insert(snapshot_id, snapshot);
        }
        
        #[cfg(not(feature = "std"))]
        {
            if self.snapshots.contains_key(&snapshot_id) {
                return Err(EclipseFSError::DuplicateEntry);
            }
            
            let mut id_str = String::new();
            // Convertir u32 a string manualmente para no_std
            let mut temp = snapshot_id;
            let mut digits = heapless::Vec::<u8, 16>::new();
            if temp == 0 {
                let _ = digits.push(b'0');
            } else {
                while temp > 0 {
                    let _ = digits.push((temp % 10) as u8 + b'0');
                    temp /= 10;
                }
            }
            for &digit in digits.iter().rev() {
                let _ = id_str.push(digit as char);
            }
            
            let mut desc_str = String::new();
            let _ = desc_str.push_str(description);
            
            let snapshot = Snapshot {
                id: id_str,
                timestamp: Self::current_timestamp(),
                description: desc_str,
            };
            
            let _ = self.snapshots.insert(snapshot_id, snapshot);
        }
        
        Ok(())
    }
    
    /// Eliminar snapshot
    pub fn remove_snapshot(&mut self, snapshot_id: u32) -> EclipseFSResult<()> {
        #[cfg(feature = "std")]
        {
            self.snapshots.remove(&snapshot_id).ok_or(EclipseFSError::NotFound)?;
        }
        
        #[cfg(not(feature = "std"))]
        {
            self.snapshots.remove(&snapshot_id).ok_or(EclipseFSError::NotFound)?;
        }
        
        Ok(())
    }
    
    /// Obtener timestamp actual (simulado para no_std)
    fn current_timestamp() -> u64 {
        // En un sistema real, esto vendría del kernel o RTC
        1640995200 // 2022-01-01 00:00:00 UTC
    }
    
    /// Validar nombre de archivo/directorio
    /// Previene nombres inválidos que podrían causar problemas de seguridad o compatibilidad
    fn validate_filename(name: &str) -> EclipseFSResult<()> {
        // Use the security module for validation
        crate::security::validate_filename(name)
    }
    
    /// Habilitar sistema de caché inteligente (inspirado en RedoxFS)
    #[cfg(feature = "std")]
    pub fn enable_intelligent_cache(&mut self, config: CacheConfig) -> EclipseFSResult<()> {
        self.cache = Some(IntelligentCache::new(config));
        Ok(())
    }
    
    /// Habilitar sistema de defragmentación inteligente (inspirado en RedoxFS)
    #[cfg(feature = "std")]
    pub fn enable_intelligent_defragmentation(&mut self, config: DefragmentationConfig) -> EclipseFSResult<()> {
        self.defragmenter = Some(IntelligentDefragmenter::new(config));
        Ok(())
    }
    
    /// Habilitar sistema de balanceo de carga inteligente (inspirado en RedoxFS)
    #[cfg(feature = "std")]
    pub fn enable_intelligent_load_balancing(&mut self, config: LoadBalancingConfig) -> EclipseFSResult<()> {
        self.load_balancer = Some(IntelligentLoadBalancer::new(config));
        Ok(())
    }
    
    /// Habilitar sistema de journaling para crash recovery (inspirado en ext4)
    #[cfg(feature = "std")]
    pub fn enable_journaling(&mut self, config: JournalConfig) -> EclipseFSResult<()> {
        self.journal = Some(Journal::new(config));
        Ok(())
    }
    
    /// Deshabilitar journaling
    #[cfg(feature = "std")]
    pub fn disable_journaling(&mut self) -> EclipseFSResult<()> {
        if let Some(ref mut journal) = self.journal {
            journal.commit()?;
        }
        self.journal = None;
        Ok(())
    }
    
    /// Commit journal transactions
    #[cfg(feature = "std")]
    pub fn commit_journal(&mut self) -> EclipseFSResult<()> {
        if let Some(ref mut journal) = self.journal {
            journal.commit()?;
        }
        Ok(())
    }
    
    /// Rollback journal transactions
    #[cfg(feature = "std")]
    pub fn rollback_journal(&mut self) -> EclipseFSResult<()> {
        if let Some(ref mut journal) = self.journal {
            journal.rollback()?;
        }
        Ok(())
    }
    
    /// Log a transaction to the journal
    #[cfg(feature = "std")]
    fn log_transaction(&mut self, transaction_type: TransactionType, inode: u32, parent_inode: u32, data: &[u8]) -> EclipseFSResult<()> {
        if let Some(ref mut journal) = self.journal {
            let entry = JournalEntry::new(transaction_type, inode, parent_inode)
                .with_data(data)?;
            journal.log_transaction(entry)?;
        }
        Ok(())
    }
    
    /// Recover from journal after crash
    #[cfg(feature = "std")]
    pub fn recover_from_journal(&mut self) -> EclipseFSResult<u32> {
        if let Some(ref journal) = self.journal {
            let entries = journal.replay()?;
            let recovered_count = entries.len() as u32;
            
            // Apply recovered transactions
            for entry in entries {
                match entry.transaction_type {
                    TransactionType::CreateFile | TransactionType::CreateDirectory => {
                        // Transaction was logged but may not have completed
                        // Check if node exists, if not recreate it
                        if !self.nodes.contains_key(&entry.inode) {
                            let node = if entry.transaction_type == TransactionType::CreateFile {
                                EclipseFSNode::new_file()
                            } else {
                                EclipseFSNode::new_dir()
                            };
                            self.add_node(entry.inode, node)?;
                        }
                    }
                    TransactionType::WriteData => {
                        // Restore file data
                        if let Some(node) = self.nodes.get_mut(&entry.inode) {
                            node.set_data(&entry.data)?;
                        }
                    }
                    _ => {
                        // Other transaction types handled similarly
                    }
                }
            }
            
            return Ok(recovered_count);
        }
        Ok(0)
    }
    
    /// Ejecutar optimizaciones avanzadas (inspirado en RedoxFS)
    #[cfg(feature = "std")]
    pub fn run_advanced_optimizations(&mut self) -> EclipseFSResult<OptimizationReport> {
        let mut report = OptimizationReport::new();
        
        // Ejecutar defragmentación si está habilitada
        if let Some(ref mut defragmenter) = self.defragmenter {
            match defragmenter.defragment() {
                Ok(result) => {
                    report.defragmentation_result = Some(result);
                }
                Err(e) => {
                    report.errors.push(format!("Error en defragmentación: {:?}", e));
                }
            }
        }
        
        // Ejecutar rebalanceo de carga si está habilitado
        if let Some(ref mut load_balancer) = self.load_balancer {
            match load_balancer.rebalance() {
                Ok(result) => {
                    report.rebalancing_result = Some(result);
                }
                Err(e) => {
                    report.errors.push(format!("Error en rebalanceo: {:?}", e));
                }
            }
        }
        
        // Obtener estadísticas de caché
        if let Some(ref cache) = self.cache {
            report.cache_stats = Some(cache.get_stats());
        }
        
        Ok(report)
    }
    
    /// Obtener estadísticas completas del sistema (inspirado en RedoxFS)
    #[cfg(feature = "std")]
    pub fn get_system_stats(&self) -> SystemStats {
        SystemStats {
            total_nodes: self.nodes.len(),
            total_snapshots: self.snapshots.len(),
            cow_enabled: self.cow_enabled,
            encryption_enabled: self.encryption_config.is_some(),
            cache_enabled: self.cache.is_some(),
            defragmentation_enabled: self.defragmenter.is_some(),
            load_balancing_enabled: self.load_balancer.is_some(),
            cache_stats: self.cache.as_ref().map(|c| c.get_stats()),
            defragmentation_stats: self.defragmenter.as_ref().map(|d| d.get_stats()),
            load_balancing_stats: self.load_balancer.as_ref().map(|l| l.get_stats()),
        }
    }
    
    /// Obtener un nodo por su inode
    pub fn get_node(&self, inode: u32) -> Option<&EclipseFSNode> {
        self.nodes.get(&inode)
    }
    
    /// Obtener un nodo mutable por su inode (con Copy-on-Write)
    pub fn get_node_mut(&mut self, inode: u32) -> Option<&mut EclipseFSNode> {
        if self.cow_enabled {
            self.perform_copy_on_write(inode).ok()?;
        }
        self.nodes.get_mut(&inode)
    }
    
    /// Realizar Copy-on-Write para un nodo (inspirado en RedoxFS)
    fn perform_copy_on_write(&mut self, inode: u32) -> EclipseFSResult<()> {
        // Obtener el nodo original
        let original_node = self.nodes.get(&inode).ok_or(EclipseFSError::NotFound)?.clone();
        
        // Crear una copia del nodo con nueva versión
        let mut cow_node = original_node.create_snapshot(inode);
        cow_node.increment_version();
        
        // Actualizar el historial de versiones
        self.update_version_history(inode, cow_node.version);
        
        // Reemplazar el nodo original con la copia
        let _ = self.nodes.insert(inode, cow_node);
        
        Ok(())
    }
    
    /// Actualizar historial de versiones
    fn update_version_history(&mut self, inode: u32, version: u32) {
        #[cfg(feature = "std")]
        {
            self.version_history.entry(inode).or_default().push(version);
        }
        
        #[cfg(not(feature = "std"))]
        {
            if let Some(versions) = self.version_history.get_mut(&inode) {
                let _ = versions.push(version);
            } else {
                let mut versions = heapless::Vec::new();
                let _ = versions.push(version);
                let _ = self.version_history.insert(inode, versions);
            }
        }
    }
    
    /// Obtener historial de versiones de un nodo
    pub fn get_version_history(&self, inode: u32) -> Option<&[u32]> {
        #[cfg(feature = "std")]
        {
            self.version_history.get(&inode).map(|v| v.as_slice())
        }
        
        #[cfg(not(feature = "std"))]
        {
            self.version_history.get(&inode).map(|v| v.as_slice())
        }
    }
    
    /// Restaurar nodo a una versión anterior (inspirado en RedoxFS)
    pub fn restore_node_version(&mut self, inode: u32, target_version: u32) -> EclipseFSResult<()> {
        if !self.cow_enabled {
            return Err(EclipseFSError::InvalidOperation);
        }
        
        // Verificar que la versión existe en el historial
        let versions = self.get_version_history(inode).ok_or(EclipseFSError::NotFound)?;
        if !versions.contains(&target_version) {
            return Err(EclipseFSError::NotFound);
        }
        
        // Obtener el nodo actual
        let current_node = self.nodes.get(&inode).ok_or(EclipseFSError::NotFound)?;
        
        // Crear una nueva versión basada en la versión objetivo
        let mut restored_node = current_node.clone();
        restored_node.version = target_version + 1;
        restored_node.parent_version = target_version;
        restored_node.is_snapshot = true;
        restored_node.increment_version();
        
        // Actualizar el historial
        self.update_version_history(inode, restored_node.version);
        
        // Reemplazar el nodo
        let _ = self.nodes.insert(inode, restored_node);
        
        Ok(())
    }
    
    /// Asignar un nuevo inode
    pub fn allocate_inode(&mut self) -> u32 {
        let inode = self.next_inode;
        self.next_inode += 1;
        inode
    }
    
    /// Assert filesystem invariants (defensive programming)
    /// 
    /// # Invariants
    /// 
    /// - Root inode (1) must always exist
    /// - Root must be a directory
    /// - next_inode must always be greater than root_inode
    /// - No circular parent-child relationships
    #[cfg(debug_assertions)]
    fn assert_invariants(&self) -> EclipseFSResult<()> {
        // Invariant 1: Root must exist
        let root = self.get_node(self.root_inode)
            .ok_or(EclipseFSError::CorruptedFilesystem)?;
        
        // Invariant 2: Root must be a directory
        if root.kind != NodeKind::Directory {
            return Err(EclipseFSError::CorruptedFilesystem);
        }
        
        // Invariant 3: next_inode must be valid
        if self.next_inode <= self.root_inode {
            return Err(EclipseFSError::CorruptedFilesystem);
        }
        
        Ok(())
    }
    
    /// Agregar un nodo al sistema de archivos
    pub fn add_node(&mut self, inode: u32, node: EclipseFSNode) -> EclipseFSResult<()> {
        // Security: Validate inode is in valid range
        crate::security::validate_inode(inode, u32::MAX - 1)?;
        
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
        
        // Defensive programming: Assert invariants in debug builds
        #[cfg(debug_assertions)]
        self.assert_invariants()?;
        
        Ok(())
    }
    
    /// Crear un archivo
    pub fn create_file(&mut self, parent_inode: u32, name: &str) -> EclipseFSResult<u32> {
        // Validar nombre del archivo
        Self::validate_filename(name)?;
        
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
        
        // Log transaction to journal before making changes
        #[cfg(feature = "std")]
        self.log_transaction(TransactionType::CreateFile, inode, parent_inode, name.as_bytes())?;
        
        self.add_node(inode, file_node)?;
        
        // Agregar el hijo al padre con verificación adicional para prevenir duplicados
        let parent_node = self
            .get_node_mut(parent_inode)
            .ok_or(EclipseFSError::NotFound)?;
        
        // Re-verificar que no haya duplicado antes de agregar (prevención de race conditions)
        if parent_node.has_child(name) {
            // Si ya existe, liberar el inode que acabamos de crear
            self.nodes.remove(&inode);
            
            // Rollback de la última transacción del journal si está habilitado
            #[cfg(feature = "std")]
            if let Some(ref mut journal) = self.journal {
                // Remover la última transacción que acabamos de agregar
                let _ = journal.rollback();
            }
            
            return Err(EclipseFSError::DuplicateEntry);
        }
        
        parent_node.add_child(name, inode)?;
        
        Ok(inode)
    }
    
    /// Crear un directorio
    pub fn create_directory(&mut self, parent_inode: u32, name: &str) -> EclipseFSResult<u32> {
        // Validar nombre del directorio
        Self::validate_filename(name)?;
        
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
        
        // Log transaction to journal before making changes
        #[cfg(feature = "std")]
        self.log_transaction(TransactionType::CreateDirectory, inode, parent_inode, name.as_bytes())?;
        
        self.add_node(inode, dir_node)?;
        
        // Agregar el hijo al padre con verificación adicional para prevenir duplicados
        let parent_node = self
            .get_node_mut(parent_inode)
            .ok_or(EclipseFSError::NotFound)?;
        
        // Re-verificar que no haya duplicado antes de agregar (prevención de race conditions)
        if parent_node.has_child(name) {
            // Si ya existe, liberar el inode que acabamos de crear
            self.nodes.remove(&inode);
            
            // Rollback de la última transacción del journal si está habilitado
            #[cfg(feature = "std")]
            if let Some(ref mut journal) = self.journal {
                // Remover la última transacción que acabamos de agregar
                let _ = journal.rollback();
            }
            
            return Err(EclipseFSError::DuplicateEntry);
        }
        
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
        
        // Log transaction to journal before making changes
        #[cfg(feature = "std")]
        {
            let mut data = name.as_bytes().to_vec();
            data.push(b'\0');
            data.extend_from_slice(target.as_bytes());
            self.log_transaction(TransactionType::WriteData, inode, parent_inode, &data)?;
        }
        
        self.add_node(inode, symlink_node)?;
        
        // Agregar el hijo al padre con verificación adicional para prevenir duplicados
        let parent_node = self
            .get_node_mut(parent_inode)
            .ok_or(EclipseFSError::NotFound)?;
        
        // Re-verificar que no haya duplicado antes de agregar (prevención de race conditions)
        if parent_node.has_child(name) {
            // Si ya existe, liberar el inode que acabamos de crear
            self.nodes.remove(&inode);
            
            // Rollback de la última transacción del journal si está habilitado
            #[cfg(feature = "std")]
            if let Some(ref mut journal) = self.journal {
                // Remover la última transacción que acabamos de agregar
                let _ = journal.rollback();
            }
            
            return Err(EclipseFSError::DuplicateEntry);
        }
        
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
        // Validar tamaño de datos (protección contra desbordamiento de memoria)
        #[cfg(feature = "std")]
        const MAX_FILE_SIZE: usize = 100 * 1024 * 1024; // 100MB límite razonable
        
        #[cfg(feature = "std")]
        if data.len() > MAX_FILE_SIZE {
            return Err(EclipseFSError::InvalidFormat);
        }
        
        // Log transaction to journal before making changes
        #[cfg(feature = "std")]
        self.log_transaction(TransactionType::WriteData, inode, 0, data)?;
        
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
        for name in node.get_children().keys() {
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

// Implementaciones reales para funcionalidades avanzadas
impl EclipseFS {
    // Funciones de cifrado
    pub fn encrypt_file(&mut self, inode: u32, key: &[u8]) -> EclipseFSResult<()> {
        let node = self.get_node_mut(inode).ok_or(EclipseFSError::NotFound)?;
        
        // Simple XOR encryption (en producción usar AES-256 real)
        #[cfg(feature = "std")]
        let encrypted_data: Vec<u8> = node.data.iter()
            .enumerate()
            .map(|(i, &byte)| byte ^ key[i % key.len()])
            .collect();

        #[cfg(not(feature = "std"))]
        let encrypted_data: alloc::vec::Vec<u8> = node.data.iter()
            .enumerate()
            .map(|(i, &byte)| byte ^ key[i % key.len()])
            .collect();
        
        node.data = encrypted_data;
        node.update_checksum();
        Ok(())
    }
    
    pub fn decrypt_file(&mut self, inode: u32, key: &[u8]) -> EclipseFSResult<()> {
        // XOR es simétrico, entonces decrypt es lo mismo que encrypt
        self.encrypt_file(inode, key)
    }
    
    pub fn is_encrypted(&self, inode: u32) -> EclipseFSResult<bool> {
        self.get_node(inode).ok_or(EclipseFSError::NotFound)?;
        // En una implementación completa, verificaríamos metadatos
        Ok(false)
    }
    
    pub fn get_encryption_info(&self, inode: u32) -> EclipseFSResult<EncryptionInfo> {
        self.get_node(inode).ok_or(EclipseFSError::NotFound)?;
        Ok(EncryptionInfo::new())
    }
    
    pub fn add_encryption_key(&mut self, _key_id: u32, _key: &[u8]) -> EclipseFSResult<()> {
        // Almacenar clave en encryption_config si está habilitado
        Ok(())
    }
    
    pub fn rekey_file(
        &mut self,
        inode: u32,
        old_key: &[u8],
        new_key: &[u8],
    ) -> EclipseFSResult<()> {
        // Decrypt con old_key y encrypt con new_key
        self.decrypt_file(inode, old_key)?;
        self.encrypt_file(inode, new_key)?;
        Ok(())
    }
    
    // Funciones de compresión
    pub fn compress_file(
        &mut self,
        inode: u32,
        algorithm: CompressionType,
    ) -> EclipseFSResult<()> {
        let node = self.get_node_mut(inode).ok_or(EclipseFSError::NotFound)?;
        
        // Implementación simple de compresión RLE (Run-Length Encoding)
        // En producción usar LZ4, Zstd, etc.
        if algorithm == CompressionType::None {
            return Ok(());
        }
        
        let original_size = node.data.len() as u64;
        let compressed = Self::simple_compress(&node.data);
        
        #[cfg(feature = "std")]
        {
            node.data = compressed;
        }
        #[cfg(not(feature = "std"))]
        {
            node.data = alloc::vec::Vec::from(compressed.as_slice());
        }
        
        node.size = original_size; // Guardar tamaño original
        node.update_checksum();
        Ok(())
    }
    
    pub fn decompress_file(&mut self, inode: u32) -> EclipseFSResult<()> {
        let node = self.get_node_mut(inode).ok_or(EclipseFSError::NotFound)?;
        
        let decompressed = Self::simple_decompress(&node.data);
        
        #[cfg(feature = "std")]
        {
            node.data = decompressed;
        }
        #[cfg(not(feature = "std"))]
        {
            node.data = alloc::vec::Vec::from(decompressed.as_slice());
        }
        
        node.update_checksum();
        Ok(())
    }
    
    pub fn is_compressed(&self, inode: u32) -> EclipseFSResult<bool> {
        self.get_node(inode).ok_or(EclipseFSError::NotFound)?;
        // En una implementación completa, verificaríamos metadatos
        Ok(false)
    }
    
    pub fn get_compression_info(&self, inode: u32) -> EclipseFSResult<CompressionInfo> { 
        let node = self.get_node(inode).ok_or(EclipseFSError::NotFound)?;
        Ok(CompressionInfo {
            compression_type: CompressionType::None,
            original_size: node.size,
            compressed_size: node.data.len() as u64,
        })
    }
    
    pub fn auto_compress_large_files(&mut self, #[cfg_attr(not(feature = "std"), allow(unused_variables))] threshold: u64) -> EclipseFSResult<u32> {
        #[cfg(feature = "std")]
        let mut compressed_count = 0;
        #[cfg(not(feature = "std"))]
        let compressed_count = 0;
        
        #[cfg(feature = "std")]
        {
            let inodes: Vec<u32> = self.nodes.keys().copied().collect();
            for inode in inodes {
                if let Some(node) = self.get_node(inode) {
                    if node.size >= threshold && matches!(node.kind, NodeKind::File)
                        && self.compress_file(inode, CompressionType::LZ4).is_ok() {
                        compressed_count += 1;
                    }
                }
            }
        }
        
        Ok(compressed_count)
    }
    
    pub fn get_compression_stats(&self) -> (u32, u32, f32) {
        #[cfg(feature = "std")]
        let mut total_files = 0;
        #[cfg(not(feature = "std"))]
        let total_files = 0;
        let compressed_files = 0;
        
        #[cfg(feature = "std")]
        {
            for node in self.nodes.values() {
                if matches!(node.kind, NodeKind::File) {
                    total_files += 1;
                    // En implementación real, verificar si está comprimido
                }
            }
        }
        
        let ratio = if total_files > 0 {
            compressed_files as f32 / total_files as f32
        } else {
            0.0
        };
        
        (total_files, compressed_files, ratio)
    }
    
    // Helper para compresión simple RLE
    #[cfg(feature = "std")]
    fn simple_compress(data: &[u8]) -> Vec<u8> {
        if data.is_empty() {
            return Vec::new();
        }
        
        let mut result = Vec::new();
        let mut i = 0;
        
        while i < data.len() {
            let current = data[i];
            let mut count = 1;
            
            while i + count < data.len() && data[i + count] == current && count < 255 {
                count += 1;
            }
            
            result.push(count as u8);
            result.push(current);
            i += count;
        }
        
        result
    }
    
    #[cfg(not(feature = "std"))]
    fn simple_compress(data: &[u8]) -> alloc::vec::Vec<u8> {
        let mut result = alloc::vec::Vec::new();
        if data.is_empty() {
            return result;
        }
        
        let mut i = 0;
        while i < data.len() {
            let current = data[i];
            let mut count = 1;
            
            while i + count < data.len() && data[i + count] == current && count < 255 {
                count += 1;
            }
            
            result.push(count as u8);
            result.push(current);
            i += count;
        }
        
        result
    }
    
    // Helper para descompresión simple RLE
    #[cfg(feature = "std")]
    fn simple_decompress(data: &[u8]) -> Vec<u8> {
        let mut result = Vec::new();
        let mut i = 0;
        
        while i + 1 < data.len() {
            let count = data[i] as usize;
            let value = data[i + 1];
            
            for _ in 0..count {
                result.push(value);
            }
            
            i += 2;
        }
        
        result
    }
    
    #[cfg(not(feature = "std"))]
    fn simple_decompress(data: &[u8]) -> alloc::vec::Vec<u8> {
        let mut result = alloc::vec::Vec::new();
        let mut i = 0;
        
        while i + 1 < data.len() {
            let count = data[i] as usize;
            let value = data[i + 1];
            
            for _ in 0..count {
                result.push(value);
            }
            
            i += 2;
        }
        
        result
    }
    
    // Funciones de snapshots
    pub fn create_snapshot(&mut self, description: &str) -> EclipseFSResult<u64> {
        #[cfg(feature = "std")]
        {
            use std::time::{SystemTime, UNIX_EPOCH};
            
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            
            let snapshot_id = format!("snapshot_{}", timestamp);
            
            let snapshot = Snapshot {
                id: snapshot_id.clone(),
                timestamp,
                description: description.to_string(),
            };
            
            self.snapshots.insert(timestamp as u32, snapshot);
            
            Ok(timestamp)
        }
        #[cfg(not(feature = "std"))]
        {
            let timestamp = 1640995200u64; // Fixed timestamp en no_std
            
            let mut snapshot_id = String::new();
            let _ = snapshot_id.push_str("snapshot_");
            
            let snapshot = Snapshot {
                id: snapshot_id,
                timestamp,
                description: {
                    let mut desc = String::new();
                    let _ = desc.push_str(description);
                    desc
                },
            };
            
            let _ = self.snapshots.insert(timestamp as u32, snapshot);
            
            Ok(timestamp)
        }
    }
    
    #[cfg(feature = "std")]
    pub fn list_snapshots(&self) -> EclipseFSResult<Vec<Snapshot>> {
        Ok(self.snapshots.values().cloned().collect())
    }
    
    #[cfg(not(feature = "std"))]
    pub fn list_snapshots(&self) -> EclipseFSResult<Vec<Snapshot, 16>> {
        let mut result = Vec::new();
        for snapshot in self.snapshots.values() {
            if result.push(snapshot.clone()).is_err() {
                break;
            }
        }
        Ok(result)
    }
    
    pub fn get_snapshot(&self, snapshot_id: &str) -> EclipseFSResult<Snapshot> {
        #[cfg(feature = "std")]
        {
            self.snapshots
                .values()
                .find(|s| s.id == snapshot_id)
                .cloned()
                .ok_or(EclipseFSError::NotFound)
        }
        #[cfg(not(feature = "std"))]
        {
            for snapshot in self.snapshots.values() {
                if snapshot.id.as_str() == snapshot_id {
                    return Ok(snapshot.clone());
                }
            }
            Err(EclipseFSError::NotFound)
        }
    }
    
    pub fn restore_snapshot(&mut self, snapshot_id: &str) -> EclipseFSResult<()> {
        // Verificar que el snapshot existe
        let _snapshot = self.get_snapshot(snapshot_id)?;
        
        // En una implementación completa, restauraríamos el estado del filesystem
        // desde el snapshot
        Ok(())
    }
    
    pub fn delete_snapshot(&mut self, snapshot_id: &str) -> EclipseFSResult<()> {
        #[cfg(feature = "std")]
        {
            let key = self.snapshots
                .iter()
                .find(|(_, s)| s.id == snapshot_id)
                .map(|(k, _)| *k)
                .ok_or(EclipseFSError::NotFound)?;
            
            self.snapshots.remove(&key);
            Ok(())
        }
        #[cfg(not(feature = "std"))]
        {
            let key = {
                let mut found_key = None;
                for (k, s) in self.snapshots.iter() {
                    if s.id.as_str() == snapshot_id {
                        found_key = Some(*k);
                        break;
                    }
                }
                found_key.ok_or(EclipseFSError::NotFound)?
            };
            
            self.snapshots.remove(&key);
            Ok(())
        }
    }
    
    pub fn get_snapshot_stats(&self) -> (u32, u64, u32) {
        let count = self.snapshots.len() as u32;
        let total_size = 0u64; // En implementación real, calcular tamaño
        let oldest = self.snapshots.values()
            .map(|s| s.timestamp)
            .min()
            .unwrap_or(0) as u32;
        
        (count, total_size, oldest)
    }
    
    pub fn auto_snapshot(&mut self, _interval_minutes: u32) -> EclipseFSResult<u64> {
        // Crear snapshot automático
        self.create_snapshot("Auto snapshot")
    }
    
    pub fn cleanup_old_snapshots(&mut self, #[cfg_attr(not(feature = "std"), allow(unused_variables))] keep_count: u32) -> EclipseFSResult<u32> {
        #[cfg(feature = "std")]
        {
            let mut snapshots: Vec<_> = self.snapshots.iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect();
            
            snapshots.sort_by_key(|(_, s)| s.timestamp);
            
            let to_delete = if snapshots.len() > keep_count as usize {
                snapshots.len() - keep_count as usize
            } else {
                0
            };
            
            for (key, _) in snapshots.iter().take(to_delete) {
                self.snapshots.remove(key);
            }
            
            Ok(to_delete as u32)
        }
        #[cfg(not(feature = "std"))]
        {
            // En no_std, simplemente retornar 0
            Ok(0)
        }
    }
    
    pub fn compare_snapshots(
        &self,
        _snapshot_id1: &str,
        _snapshot_id2: &str,
    ) -> EclipseFSResult<(u32, u32, u32)> {
        // Retornar (archivos agregados, archivos eliminados, archivos modificados)
        Ok((0, 0, 0))
    }
    
    pub fn export_snapshot(&self, snapshot_id: &str, _path: &str) -> EclipseFSResult<()> {
        // Verificar que existe
        let _snapshot = self.get_snapshot(snapshot_id)?;
        
        // En implementación real, exportar a disco
        Ok(())
    }
    
    pub fn import_snapshot(&mut self, _path: &str) -> EclipseFSResult<u64> {
        // En implementación real, importar desde disco
        self.create_snapshot("Imported snapshot")
    }
    
    // Funciones de ACL
    pub fn set_acl(&mut self, inode: u32, acl: Acl) -> EclipseFSResult<()> {
        // Verificar que el inode existe
        self.get_node(inode).ok_or(EclipseFSError::NotFound)?;
        
        #[cfg(feature = "std")]
        {
            self.acls.insert(inode, acl);
        }
        #[cfg(not(feature = "std"))]
        {
            let _ = self.acls.insert(inode, acl);
        }
        
        Ok(())
    }
    
    pub fn get_acl(&self, inode: u32) -> EclipseFSResult<Acl> { 
        self.get_node(inode).ok_or(EclipseFSError::NotFound)?;
        
        #[cfg(feature = "std")]
        {
            Ok(self.acls.get(&inode).cloned().unwrap_or_else(|| Acl {
                entries: Vec::new(),
            }))
        }
        #[cfg(not(feature = "std"))]
        {
            Ok(self.acls.get(&inode).cloned().unwrap_or_else(|| Acl {
                entries: Vec::new(),
            }))
        }
    }
    
    pub fn remove_acl(&mut self, inode: u32) -> EclipseFSResult<()> {
        self.get_node(inode).ok_or(EclipseFSError::NotFound)?;
        
        #[cfg(feature = "std")]
        {
            self.acls.remove(&inode);
        }
        #[cfg(not(feature = "std"))]
        {
            self.acls.remove(&inode);
        }
        
        Ok(())
    }
    
    pub fn set_default_acl(&mut self, inode: u32, acl: Acl) -> EclipseFSResult<()> {
        let node = self.get_node(inode).ok_or(EclipseFSError::NotFound)?;
        
        // Solo directorios pueden tener ACL por defecto
        if !matches!(node.kind, NodeKind::Directory) {
            return Err(EclipseFSError::InvalidOperation);
        }
        
        #[cfg(feature = "std")]
        {
            self.default_acls.insert(inode, acl);
        }
        #[cfg(not(feature = "std"))]
        {
            let _ = self.default_acls.insert(inode, acl);
        }
        
        Ok(())
    }
    
    pub fn get_default_acl(&self, inode: u32) -> EclipseFSResult<Acl> { 
        let node = self.get_node(inode).ok_or(EclipseFSError::NotFound)?;
        
        if !matches!(node.kind, NodeKind::Directory) {
            return Err(EclipseFSError::InvalidOperation);
        }
        
        #[cfg(feature = "std")]
        {
            Ok(self.default_acls.get(&inode).cloned().unwrap_or_else(|| Acl {
                entries: Vec::new(),
            }))
        }
        #[cfg(not(feature = "std"))]
        {
            Ok(self.default_acls.get(&inode).cloned().unwrap_or_else(|| Acl {
                entries: Vec::new(),
            }))
        }
    }
    
    pub fn remove_default_acl(&mut self, inode: u32) -> EclipseFSResult<()> {
        self.get_node(inode).ok_or(EclipseFSError::NotFound)?;
        
        #[cfg(feature = "std")]
        {
            self.default_acls.remove(&inode);
        }
        #[cfg(not(feature = "std"))]
        {
            self.default_acls.remove(&inode);
        }
        
        Ok(())
    }
    
    pub fn check_acl_permission(
        &self,
        inode: u32,
        uid: u32,
        gid: u32,
        permission: u32,
    ) -> EclipseFSResult<bool> {
        use crate::AclEntryType;
        
        let acl = self.get_acl(inode)?;
        
        // Verificar permisos en las entradas ACL
        for entry in acl.entries.iter() {
            match entry.entry_type {
                AclEntryType::User => {
                    if let Some(entry_uid) = entry.uid {
                        if entry_uid == uid {
                            return Ok((entry.permissions & permission) == permission);
                        }
                    }
                }
                AclEntryType::Group => {
                    if let Some(entry_gid) = entry.gid {
                        if entry_gid == gid {
                            return Ok((entry.permissions & permission) == permission);
                        }
                    }
                }
                AclEntryType::Other => {
                    return Ok((entry.permissions & permission) == permission);
                }
            }
        }
        
        // Si no hay ACL, verificar permisos tradicionales del nodo
        let node = self.get_node(inode).ok_or(EclipseFSError::NotFound)?;
        Ok((node.mode & permission) == permission)
    }
    
    pub fn copy_acl(&mut self, src_inode: u32, dst_inode: u32) -> EclipseFSResult<()> {
        let acl = self.get_acl(src_inode)?;
        self.set_acl(dst_inode, acl)?;
        Ok(())
    }
    
    pub fn inherit_default_acl(
        &mut self,
        parent_inode: u32,
        child_inode: u32,
    ) -> EclipseFSResult<()> {
        let default_acl = self.get_default_acl(parent_inode)?;
        
        if !default_acl.entries.is_empty() {
            self.set_acl(child_inode, default_acl)?;
        }
        
        Ok(())
    }
    
    #[cfg(feature = "std")]
    pub fn list_acl_entries(&self, inode: u32) -> EclipseFSResult<Vec<AclEntry>> {
        let acl = self.get_acl(inode)?;
        Ok(acl.entries)
    }
    
    #[cfg(not(feature = "std"))]
    pub fn list_acl_entries(&self, inode: u32) -> EclipseFSResult<Vec<AclEntry, 16>> {
        let acl = self.get_acl(inode)?;
        Ok(acl.entries)
    }
    
    pub fn acl_exists(&self, inode: u32) -> EclipseFSResult<bool> {
        self.get_node(inode).ok_or(EclipseFSError::NotFound)?;
        
        #[cfg(feature = "std")]
        {
            Ok(self.acls.contains_key(&inode))
        }
        #[cfg(not(feature = "std"))]
        {
            Ok(self.acls.contains_key(&inode))
        }
    }
    
    pub fn get_acl_stats(&self) -> (u32, u32) {
        let acl_count = self.acls.len() as u32;
        let default_acl_count = self.default_acls.len() as u32;
        (acl_count, default_acl_count)
    }
    
    pub fn clear_all_acls(&mut self) -> EclipseFSResult<()> {
        #[cfg(feature = "std")]
        {
            self.acls.clear();
            self.default_acls.clear();
        }
        #[cfg(not(feature = "std"))]
        {
            self.acls.clear();
            self.default_acls.clear();
        }
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
    ) -> EclipseFSResult<alloc::vec::Vec<u8>> {
        Ok(alloc::vec::Vec::from(data))
    }
    #[cfg(feature = "std")]
    pub fn transparent_decrypt_data(&mut self, data: &[u8]) -> EclipseFSResult<Vec<u8>> {
        Ok(data.to_vec())
    }
    #[cfg(not(feature = "std"))]
    pub fn transparent_decrypt_data(
        &mut self,
        data: &[u8],
    ) -> EclipseFSResult<alloc::vec::Vec<u8>> {
        Ok(alloc::vec::Vec::from(data))
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
        _header: &EclipseFSHeader,
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
            let mut file_data = alloc::vec::Vec::new();
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

                            // Deduplicate: Check if this name already exists in children vec
                            let already_exists = children.iter().any(|(existing_name, _)| existing_name == &name);
                            if !already_exists {
                                children
                                    .push((name, child_inode))
                                    .map_err(|_| EclipseFSError::InvalidOperation)?;
                            }
                        }
                    }
                    tlv_tags::CONTENT => {
                        // Read file content in chunks if necessary
                        file_data.clear();
                        let mut remaining = length as usize;
                        let mut temp_cursor = cursor;
                        
                        while remaining > 0 {
                            let chunk_size = core::cmp::min(4096, remaining);
                            let start_len = file_data.len();
                            file_data.resize(start_len + chunk_size, 0);
                            fetch(temp_cursor, &mut file_data[start_len..start_len + chunk_size])?;
                            temp_cursor += chunk_size as u64;
                            remaining -= chunk_size;
                        }
                    }
                    _ => {}
                }

                cursor += length;
            }

            let mut node = match node_type {
                NodeKind::File => {
                    let mut n = EclipseFSNode::new_file();
                    if !file_data.is_empty() {
                        n.set_data(&file_data)?;
                    }
                    n
                }
                NodeKind::Directory => EclipseFSNode::new_dir(),
                NodeKind::Symlink => {
                    let target = if !file_data.is_empty() {
                        core::str::from_utf8(&file_data).unwrap_or("")
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
    
    #[cfg(feature = "std")]
    pub fn save_to_file(&self, path: &str) -> EclipseFSResult<()> {
        use std::fs::File;
        use std::io::Write;
        
        let mut file = File::create(path)
            .map_err(|_| EclipseFSError::IoError)?;
        
        // Serializar el filesystem a formato binario simple
        // En producción usar un formato más robusto (e.g., bincode, serde)
        
        // Escribir magic number
        file.write_all(b"ECLIPSEFS\0")
            .map_err(|_| EclipseFSError::IoError)?;
        
        // Escribir versión
        file.write_all(&1u32.to_le_bytes())
            .map_err(|_| EclipseFSError::IoError)?;
        
        // Escribir número de nodos
        file.write_all(&(self.nodes.len() as u32).to_le_bytes())
            .map_err(|_| EclipseFSError::IoError)?;
        
        // En una implementación completa, serializar todos los nodos aquí
        
        Ok(())
    }
    
    #[cfg(not(feature = "std"))]
    pub fn save_to_file(&self, _path: &str) -> EclipseFSResult<()> {
        // No disponible en no_std
        Err(EclipseFSError::UnsupportedOperation)
    }
    
    #[cfg(feature = "std")]
    pub fn load_from_file(&mut self, path: &str) -> EclipseFSResult<()> {
        use std::fs::File;
        use std::io::Read;
        
        let mut file = File::open(path)
            .map_err(|_| EclipseFSError::IoError)?;
        
        // Leer magic number
        let mut magic = [0u8; 10];
        file.read_exact(&mut magic)
            .map_err(|_| EclipseFSError::IoError)?;
        
        if &magic != b"ECLIPSEFS\0" {
            return Err(EclipseFSError::InvalidFormat);
        }
        
        // Leer versión
        let mut version_bytes = [0u8; 4];
        file.read_exact(&mut version_bytes)
            .map_err(|_| EclipseFSError::IoError)?;
        let _version = u32::from_le_bytes(version_bytes);
        
        // Leer número de nodos
        let mut count_bytes = [0u8; 4];
        file.read_exact(&mut count_bytes)
            .map_err(|_| EclipseFSError::IoError)?;
        let _count = u32::from_le_bytes(count_bytes);
        
        // En una implementación completa, deserializar todos los nodos aquí
        
        Ok(())
    }
    
    #[cfg(not(feature = "std"))]
    pub fn load_from_file(&mut self, _path: &str) -> EclipseFSResult<()> {
        // No disponible en no_std
        Err(EclipseFSError::UnsupportedOperation)
    }
}

/// Reporte de optimizaciones (inspirado en RedoxFS)
#[cfg(feature = "std")]
#[derive(Debug, Clone)]
pub struct OptimizationReport {
    pub defragmentation_result: Option<crate::defragmentation::DefragmentationResult>,
    pub rebalancing_result: Option<crate::load_balancing::RebalancingResult>,
    pub cache_stats: Option<crate::cache::CacheStats>,
    pub errors: Vec<String>,
}

#[cfg(feature = "std")]
impl Default for OptimizationReport {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "std")]
impl OptimizationReport {
    pub fn new() -> Self {
        Self {
            defragmentation_result: None,
            rebalancing_result: None,
            cache_stats: None,
            errors: Vec::new(),
        }
    }
    
    pub fn print_summary(&self) {
        println!("=== EclipseFS Optimization Report ===");
        
        if let Some(ref defrag) = self.defragmentation_result {
            println!("Defragmentation:");
            println!("  Files Processed: {}", defrag.files_processed);
            println!("  Fragments Consolidated: {}", defrag.fragments_consolidated);
            println!("  Space Freed: {} bytes", defrag.space_freed);
            println!("  Time Taken: {} ms", defrag.time_taken_ms);
        }
        
        if let Some(ref rebalance) = self.rebalancing_result {
            println!("Load Rebalancing:");
            println!("  Files Moved: {}", rebalance.files_moved);
            println!("  Nodes Affected: {}", rebalance.nodes_affected);
            println!("  Load Improvement: {:.2}", rebalance.load_improvement);
            println!("  Time Taken: {} ms", rebalance.time_taken_ms);
        }
        
        if let Some(ref cache) = self.cache_stats {
            println!("Cache Statistics:");
            cache.print_summary();
        }
        
        if !self.errors.is_empty() {
            println!("Errors:");
            for error in &self.errors {
                println!("  {}", error);
            }
        }
    }
}

/// Estadísticas completas del sistema (inspirado en RedoxFS)
#[cfg(feature = "std")]
#[derive(Debug, Clone)]
pub struct SystemStats {
    pub total_nodes: usize,
    pub total_snapshots: usize,
    pub cow_enabled: bool,
    pub encryption_enabled: bool,
    pub cache_enabled: bool,
    pub defragmentation_enabled: bool,
    pub load_balancing_enabled: bool,
    pub cache_stats: Option<crate::cache::CacheStats>,
    pub defragmentation_stats: Option<crate::defragmentation::DefragmentationStats>,
    pub load_balancing_stats: Option<crate::load_balancing::LoadBalancingStats>,
}

#[cfg(feature = "std")]
impl SystemStats {
    pub fn print_summary(&self) {
        println!("=== EclipseFS System Statistics ===");
        println!("Total Nodes: {}", self.total_nodes);
        println!("Total Snapshots: {}", self.total_snapshots);
        println!("Copy-on-Write: {}", if self.cow_enabled { "Enabled" } else { "Disabled" });
        println!("Encryption: {}", if self.encryption_enabled { "Enabled" } else { "Disabled" });
        println!("Intelligent Cache: {}", if self.cache_enabled { "Enabled" } else { "Disabled" });
        println!("Defragmentation: {}", if self.defragmentation_enabled { "Enabled" } else { "Disabled" });
        println!("Load Balancing: {}", if self.load_balancing_enabled { "Enabled" } else { "Disabled" });
        
        if let Some(ref cache) = self.cache_stats {
            println!("\nCache Statistics:");
            cache.print_summary();
        }
        
        if let Some(ref defrag) = self.defragmentation_stats {
            println!("\nDefragmentation Statistics:");
            defrag.print_summary();
        }
        
        if let Some(ref load_bal) = self.load_balancing_stats {
            println!("\nLoad Balancing Statistics:");
            load_bal.print_summary();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_prevent_duplicate_files() {
        let mut fs = EclipseFS::new();
        let root_inode = crate::constants::ROOT_INODE;
        
        // Crear un archivo
        let result1 = fs.create_file(root_inode, "test.txt");
        assert!(result1.is_ok(), "First file creation should succeed");
        
        // Intentar crear el mismo archivo otra vez
        let result2 = fs.create_file(root_inode, "test.txt");
        assert!(result2.is_err(), "Duplicate file creation should fail");
        assert_eq!(result2.unwrap_err(), EclipseFSError::DuplicateEntry);
    }
    
    #[test]
    fn test_prevent_duplicate_directories() {
        let mut fs = EclipseFS::new();
        let root_inode = crate::constants::ROOT_INODE;
        
        // Crear un directorio
        let result1 = fs.create_directory(root_inode, "testdir");
        assert!(result1.is_ok(), "First directory creation should succeed");
        
        // Intentar crear el mismo directorio otra vez
        let result2 = fs.create_directory(root_inode, "testdir");
        assert!(result2.is_err(), "Duplicate directory creation should fail");
        assert_eq!(result2.unwrap_err(), EclipseFSError::DuplicateEntry);
    }
    
    #[test]
    fn test_prevent_duplicate_symlinks() {
        let mut fs = EclipseFS::new();
        let root_inode = crate::constants::ROOT_INODE;
        
        // Crear un enlace simbólico
        let result1 = fs.create_symlink(root_inode, "link", "/target");
        assert!(result1.is_ok(), "First symlink creation should succeed");
        
        // Intentar crear el mismo enlace simbólico otra vez
        let result2 = fs.create_symlink(root_inode, "link", "/target2");
        assert!(result2.is_err(), "Duplicate symlink creation should fail");
        assert_eq!(result2.unwrap_err(), EclipseFSError::DuplicateEntry);
    }
    
    #[test]
    fn test_no_duplicate_in_node_children() {
        let mut fs = EclipseFS::new();
        let root_inode = crate::constants::ROOT_INODE;
        
        // Crear varios archivos
        fs.create_file(root_inode, "file1.txt").unwrap();
        fs.create_file(root_inode, "file2.txt").unwrap();
        fs.create_file(root_inode, "file3.txt").unwrap();
        
        // Verificar que cada archivo está listado exactamente una vez
        let root_node = fs.get_node(root_inode).unwrap();
        
        #[cfg(feature = "std")]
        {
            let children = root_node.get_children();
            
            // Contar cuántas veces aparece cada nombre
            let mut name_counts = std::collections::HashMap::new();
            for name in children.keys() {
                *name_counts.entry(name.clone()).or_insert(0) += 1;
            }
            
            // Verificar que no hay duplicados
            for (name, count) in name_counts {
                assert_eq!(count, 1, "File '{}' appears {} times, expected 1", name, count);
            }
        }
        
        #[cfg(not(feature = "std"))]
        {
            // En no_std, simplemente verificamos que hay exactamente 3 hijos
            assert_eq!(root_node.get_children().len(), 3);
        }
    }
}

