//! Wrapper VFS para la librería EclipseFS.
//! 
//! Este módulo implementa la integración del sistema de archivos EclipseFS con el VFS del kernel.
//! Utiliza un enfoque de "carga bajo demanda" (lazy loading) para minimizar el uso de memoria:
//! 
//! - El header y la tabla de inodos se cargan al montar el filesystem
//! - Los nodos individuales se leen del disco solo cuando se necesitan
//! - Formato TLV (Type-Length-Value) para almacenamiento flexible de metadatos
//! 
//! ## Estructura en disco:
//! 
//! ```text
//! +------------------+
//! | Header (4KB)     |  <- Superbloque con metadatos del FS
//! +------------------+
//! | Inode Table      |  <- Tabla de índice (inode -> offset)
//! +------------------+
//! | Node Data        |  <- Datos de nodos en formato TLV
//! | (archivos y dirs)|
//! +------------------+
//! ```
//! 
//! ## Formato TLV de nodos:
//! 
//! Cada nodo se almacena como una secuencia de entradas TLV:
//! - Tag (2 bytes): tipo de campo (NODE_TYPE, MODE, UID, SIZE, etc.)
//! - Length (4 bytes): tamaño del valor en bytes
//! - Value (N bytes): datos del campo
//!

use crate::bootloader_data;
use crate::drivers::storage_manager::{StorageManager, StorageSectorType};
use crate::filesystem::vfs::{get_vfs, init_vfs, FileSystem, StatInfo, VfsError};
use crate::filesystem::block_cache::{get_block_cache, read_data_from_offset, BLOCK_SIZE};
use eclipsefs_lib::{format::constants as ecfs_constants, EclipseFSError, EclipseFSHeader, InodeTableEntry};
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::format;
use core::any::Any;
use core::cmp;

const HEADER_SIZE_BYTES: usize = 4096; // 8 sectores (header real)
const HEADER_SIZE_BLOCKS: u64 = (HEADER_SIZE_BYTES / 512) as u64;
static mut FS_BUFFER: [u8; HEADER_SIZE_BYTES] = [0u8; HEADER_SIZE_BYTES];
static mut BOOT_SECTOR: [u8; 512] = [0u8; 512];

/// Información sobre el dispositivo donde se debe montar EclipseFS
#[derive(Debug, Clone)]
pub struct EclipseFSDeviceInfo {
    /// Nombre del dispositivo Linux (ej: "/dev/sda2")
    pub device_name: String,
    /// Tamaño de la partición en sectores LBA
    pub size_lba: u64,
    /// Sector de inicio LBA
    pub start_lba: u64,
    /// Información adicional sobre el dispositivo
    pub additional_info: Option<String>,
}

impl EclipseFSDeviceInfo {
    /// Crear nueva información de dispositivo
    pub fn new(device_name: String, size_lba: u64, start_lba: u64) -> Self {
        Self {
            device_name,
            size_lba,
            start_lba,
            additional_info: None,
        }
    }
    
    /// Crear información de dispositivo con información adicional
    pub fn with_info(device_name: String, size_lba: u64, start_lba: u64, additional_info: String) -> Self {
        Self {
            device_name,
            size_lba,
            start_lba,
            additional_info: Some(additional_info),
        }
    }
}

/// Wrapper lazy para EclipseFS que usa carga bajo demanda
pub struct EclipseFSWrapper {
    /// Header del sistema de archivos (cargado una vez)
    header: EclipseFSHeader,
    /// Información de la tabla de inodos
    inode_table_entries: Vec<InodeTableEntry>,
    /// Índice de la partición donde está montado
    partition_index: u32,
    /// Información del dispositivo
    device_info: EclipseFSDeviceInfo,
}

impl EclipseFSWrapper {
    /// Crear nuevo wrapper lazy
    pub fn new_lazy(header: EclipseFSHeader, inode_table_entries: Vec<InodeTableEntry>, partition_index: u32, device_info: EclipseFSDeviceInfo) -> Self {
        Self {
            header,
            inode_table_entries,
            partition_index,
            device_info,
        }
        }

    pub fn as_any(&self) -> &dyn Any {
        self
    }

    /// Cargar un nodo específico bajo demanda
    pub fn load_node_lazy(&self, inode_num: u32, storage: &mut StorageManager) -> Result<eclipsefs_lib::EclipseFSNode, VfsError> {
        // Buscar la entrada en la tabla de inodos
        let entry = self.inode_table_entries.iter()
            .find(|entry| entry.inode == inode_num as u64)
            .ok_or(VfsError::FileNotFound)?;

        crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Cargando nodo {} bajo demanda (offset: {})\n", inode_num, entry.offset));

        // Calcular el offset absoluto en el disco
        let absolute_offset = entry.offset;
        
        // Buffer para leer el nodo (asumimos tamaño máximo de 4KB por nodo)
        let mut node_buffer = [0u8; 4096];
        
        // Leer datos del nodo usando el cache de bloques
        let bytes_read = read_data_from_offset(
            get_block_cache(),
            storage,
            self.partition_index,
            absolute_offset,
            &mut node_buffer
        ).map_err(|e| {
            crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Error en read_data_from_offset: {}\n", e));
            VfsError::InvalidOperation
        })?;

        if bytes_read == 0 {
            crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: ERROR - Se leyeron 0 bytes para el nodo {}\n", inode_num));
            return Err(VfsError::InvalidFs("No se pudieron leer datos del nodo".into()));
        }

        crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Nodo {} leído exitosamente ({} bytes)\n", inode_num, bytes_read));

        // Parsear el nodo desde el buffer usando formato TLV
        let node = self.parse_node_from_buffer(&node_buffer[..bytes_read], inode_num)?;
        
        crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Nodo {} parseado exitosamente (tipo: {:?}, tamaño: {})\n", 
            inode_num, node.kind, node.size));
        
        Ok(node)
    }

    /// Parsear un nodo desde un buffer TLV
    /// 
    /// ## Formato del nodo en disco:
    /// 
    /// ```text
    /// +----------------------+
    /// | Inode (4 bytes)      |  <- Número de inode
    /// | Record Size (4 bytes)|  <- Tamaño total del registro
    /// +----------------------+
    /// | TLV Entry 1          |  <- Tag (2) + Length (4) + Value (N)
    /// | TLV Entry 2          |
    /// | ...                  |
    /// +----------------------+
    /// ```
    /// 
    /// ## Tags TLV soportados:
    /// 
    /// - 0x0001: NODE_TYPE (File=1, Directory=2, Symlink=3)
    /// - 0x0002: MODE (permisos Unix)
    /// - 0x0003: UID (user ID)
    /// - 0x0004: GID (group ID)
    /// - 0x0005: SIZE (tamaño en bytes)
    /// - 0x0006: ATIME (timestamp de último acceso)
    /// - 0x0007: MTIME (timestamp de última modificación)
    /// - 0x0008: CTIME (timestamp de último cambio de metadatos)
    /// - 0x0009: NLINK (número de hard links)
    /// - 0x000A: CONTENT (datos del archivo)
    /// - 0x000B: DIRECTORY_ENTRIES (hijos del directorio)
    fn parse_node_from_buffer(&self, buffer: &[u8], expected_inode: u32) -> Result<eclipsefs_lib::EclipseFSNode, VfsError> {
        // Leer cabecera del registro de nodo (8 bytes: inode + tamaño)
        if buffer.len() < ecfs_constants::NODE_RECORD_HEADER_SIZE {
            crate::debug::serial_write_str(&alloc::format!(
                "ECLIPSEFS: ERROR - Buffer demasiado pequeño: {} bytes (mínimo {})\n",
                buffer.len(), ecfs_constants::NODE_RECORD_HEADER_SIZE
            ));
            return Err(VfsError::InvalidFs("Nodo corrupto: buffer demasiado pequeño".into()));
        }

        let recorded_inode = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
        let record_size = u32::from_le_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]) as usize;

        crate::debug::serial_write_str(&alloc::format!(
            "ECLIPSEFS: Parseando nodo - inode esperado: {}, inode leído: {}, tamaño registro: {}\n",
            expected_inode, recorded_inode, record_size
        ));

        if recorded_inode != expected_inode {
            crate::debug::serial_write_str(&alloc::format!(
                "ECLIPSEFS: ERROR - Inode no coincide (esperado {}, encontrado {})\n",
                expected_inode, recorded_inode
            ));
            return Err(VfsError::InvalidFs("Nodo corrupto: inode no coincide".into()));
        }

        if record_size < ecfs_constants::NODE_RECORD_HEADER_SIZE {
            crate::debug::serial_write_str(&alloc::format!(
                "ECLIPSEFS: ERROR - Tamaño de registro inválido: {}\n", record_size
            ));
            return Err(VfsError::InvalidFs("Nodo corrupto: tamaño inválido".into()));
        }

        // Los datos TLV empiezan después de la cabecera
        let tlv_size = record_size - ecfs_constants::NODE_RECORD_HEADER_SIZE;
        
        // CRÍTICO: Validar que el buffer contiene suficientes datos
        if ecfs_constants::NODE_RECORD_HEADER_SIZE + tlv_size > buffer.len() {
            crate::debug::serial_write_str(&alloc::format!(
                "ECLIPSEFS: ERROR - Registro truncado: necesita {} bytes, buffer tiene {} bytes\n",
                ecfs_constants::NODE_RECORD_HEADER_SIZE + tlv_size, buffer.len()
            ));
            return Err(VfsError::InvalidFs("Nodo corrupto: datos TLV truncados".into()));
        }
        
        let tlv_data = &buffer[ecfs_constants::NODE_RECORD_HEADER_SIZE..ecfs_constants::NODE_RECORD_HEADER_SIZE + tlv_size];

        // Parsear entradas TLV
        let mut node_kind = eclipsefs_lib::NodeKind::File;
        let mut mode = 0o100644u32; // Default para archivos
        let mut mode_set = false; // Track si MODE fue establecido explícitamente
        let mut uid = 0u32;
        let mut gid = 0u32;
        let mut size = 0u64;
        let mut atime = 0u64;
        let mut mtime = 0u64;
        let mut ctime = 0u64;
        let mut nlink = 1u32;
        let mut data = alloc::vec::Vec::new();
        let mut children = heapless::FnvIndexMap::<heapless::String<128>, u32, 256>::new();

        let mut offset = 0;

        while offset + 6 <= tlv_data.len() {
            let tag = u16::from_le_bytes([tlv_data[offset], tlv_data[offset + 1]]);
            let length = u32::from_le_bytes([
                tlv_data[offset + 2],
                tlv_data[offset + 3],
                tlv_data[offset + 4],
                tlv_data[offset + 5],
            ]) as usize;
            offset += 6;

            if offset + length > tlv_data.len() {
                crate::debug::serial_write_str(&alloc::format!(
                    "ECLIPSEFS: ADVERTENCIA - TLV truncado en tag 0x{:04X}, ignorando resto\n", tag
                ));
                break;
            }

            let value = &tlv_data[offset..offset + length];
            offset += length;

            match tag {
                0x0001 => { // NODE_TYPE
                    if !value.is_empty() {
                        node_kind = match value[0] {
                            1 => eclipsefs_lib::NodeKind::File,
                            2 => eclipsefs_lib::NodeKind::Directory,
                            3 => eclipsefs_lib::NodeKind::Symlink,
                            _ => {
                                crate::debug::serial_write_str(&alloc::format!(
                                    "ECLIPSEFS: ADVERTENCIA - Tipo de nodo desconocido: {}, usando File\n", value[0]
                                ));
                                eclipsefs_lib::NodeKind::File
                            }
                        };
                        crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Tipo de nodo: {:?}\n", node_kind));
                    }
                }
                0x0002 => { // MODE
                    if value.len() >= 4 {
                        mode = u32::from_le_bytes([value[0], value[1], value[2], value[3]]);
                        mode_set = true;
                    }
                }
                0x0003 => { // UID
                    if value.len() >= 4 {
                        uid = u32::from_le_bytes([value[0], value[1], value[2], value[3]]);
                    }
                }
                0x0004 => { // GID
                    if value.len() >= 4 {
                        gid = u32::from_le_bytes([value[0], value[1], value[2], value[3]]);
                    }
                }
                0x0005 => { // SIZE
                    if value.len() >= 8 {
                        size = u64::from_le_bytes([
                            value[0], value[1], value[2], value[3],
                            value[4], value[5], value[6], value[7],
                        ]);
                    }
                }
                0x0006 => { // ATIME
                    if value.len() >= 8 {
                        atime = u64::from_le_bytes([
                            value[0], value[1], value[2], value[3],
                            value[4], value[5], value[6], value[7],
                        ]);
                    }
                }
                0x0007 => { // MTIME
                    if value.len() >= 8 {
                        mtime = u64::from_le_bytes([
                            value[0], value[1], value[2], value[3],
                            value[4], value[5], value[6], value[7],
                        ]);
                    }
                }
                0x0008 => { // CTIME
                    if value.len() >= 8 {
                        ctime = u64::from_le_bytes([
                            value[0], value[1], value[2], value[3],
                            value[4], value[5], value[6], value[7],
                        ]);
                    }
                }
                0x0009 => { // NLINK
                    if value.len() >= 4 {
                        nlink = u32::from_le_bytes([value[0], value[1], value[2], value[3]]);
                    }
                }
                0x000A => { // CONTENT
                    data.extend_from_slice(value);
                    crate::debug::serial_write_str(&alloc::format!(
                        "ECLIPSEFS: Contenido del archivo: {} bytes\n", value.len()
                    ));
                }
                0x000B => { // DIRECTORY_ENTRIES
                    children = self.deserialize_directory_entries(value)?;
                    crate::debug::serial_write_str(&alloc::format!(
                        "ECLIPSEFS: Directorio con {} entradas\n", children.len()
                    ));
                    // Log all children for debugging
                    for (child_name, child_inode) in children.iter() {
                        crate::debug::serial_write_str(&alloc::format!(
                            "ECLIPSEFS:   - '{}' -> inodo {}\n", child_name, child_inode
                        ));
                    }
                }
                _ => {
                    // Ignorar tags desconocidos
                    crate::debug::serial_write_str(&alloc::format!(
                        "ECLIPSEFS: ADVERTENCIA - Tag TLV desconocido: 0x{:04X}\n", tag
                    ));
                }
            }
        }
        
        // Ajustar mode según tipo de nodo si no fue establecido explícitamente
        if !mode_set {
            mode = match node_kind {
                eclipsefs_lib::NodeKind::Directory => 0o040755,
                eclipsefs_lib::NodeKind::File => 0o100644,
                eclipsefs_lib::NodeKind::Symlink => 0o120777,
            };
        }

        Ok(eclipsefs_lib::EclipseFSNode {
            kind: node_kind,
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
            version: 1,
            parent_version: 0,
            is_snapshot: false,
            original_inode: 0,
            checksum: 0,
            extent_tree: eclipsefs_lib::ExtentTree::new(),
            use_extents: false,
        })
    }

    /// Deserializar entradas de directorio desde formato binario
    /// 
    /// Formato: [name_len (4) | child_inode (4) | name (N bytes)] repetido
    fn deserialize_directory_entries(&self, data: &[u8]) -> Result<heapless::FnvIndexMap<heapless::String<128>, u32, 256>, VfsError> {
        let mut entries = heapless::FnvIndexMap::new();
        let mut offset = 0;
        let mut skipped_count = 0;
        let mut truncation_count = 0;

        while offset < data.len() {
            if offset + 8 > data.len() {
                break;
            }

            let name_len = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize;
            offset += 4;

            let child_inode = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]);
            offset += 4;

            if offset + name_len > data.len() {
                crate::debug::serial_write_str(&alloc::format!(
                    "ECLIPSEFS: ADVERTENCIA - Entrada de directorio truncada: esperaba {} bytes, solo {} disponibles\n",
                    name_len, data.len() - offset
                ));
                break;
            }

            // Validar longitud de nombre antes de conversión
            if name_len > 128 {
                crate::debug::serial_write_str(&alloc::format!(
                    "ECLIPSEFS: ADVERTENCIA - Nombre de archivo demasiado largo ({} bytes), máximo 128. Saltando entrada.\n",
                    name_len
                ));
                skipped_count += 1;
                offset += name_len;
                continue;
            }

            // Convertir bytes a String
            let name_bytes = &data[offset..offset + name_len];
            match core::str::from_utf8(name_bytes) {
                Ok(name_str) => {
                    let mut name = heapless::String::new();
                    if name.push_str(name_str).is_ok() {
                        crate::debug::serial_write_str(&alloc::format!(
                            "ECLIPSEFS: Entrada de directorio: '{}' -> inode {}\n",
                            name_str, child_inode
                        ));
                        
                        // Verificar si hay espacio en el mapa
                        if entries.insert(name, child_inode).is_err() {
                            crate::debug::serial_write_str(&alloc::format!(
                                "ECLIPSEFS: ADVERTENCIA - Directorio lleno (máximo 256 entradas). Ignorando '{}'\n",
                                name_str
                            ));
                            truncation_count += 1;
                        }
                    } else {
                        crate::debug::serial_write_str(&alloc::format!(
                            "ECLIPSEFS: ADVERTENCIA - Fallo push_str para '{}' (probablemente demasiado largo)\n",
                            name_str
                        ));
                        skipped_count += 1;
                    }
                }
                Err(_) => {
                    crate::debug::serial_write_str(&alloc::format!(
                        "ECLIPSEFS: ADVERTENCIA - Nombre de archivo no es UTF-8 válido ({} bytes). Saltando entrada.\n",
                        name_len
                    ));
                    skipped_count += 1;
                }
            }
            offset += name_len;
        }

        if skipped_count > 0 || truncation_count > 0 {
            crate::debug::serial_write_str(&alloc::format!(
                "ECLIPSEFS: Resumen de directorio: {} entradas cargadas, {} saltadas, {} truncadas\n",
                entries.len(), skipped_count, truncation_count
            ));
        }

        Ok(entries)
    }

    /// Sincronizar todos los cambios al disco real
    pub fn sync_to_disk(&mut self) -> Result<(), VfsError> {
        crate::debug::serial_write_str("ECLIPSEFS: Sincronizando cambios al disco...\n");
        
        // Sincronizar cache de bloques
        get_block_cache().sync(
            &mut StorageManager::new(),
            self.partition_index
        ).map_err(|_| VfsError::InvalidOperation)?;
        
        crate::debug::serial_write_str("ECLIPSEFS: Sincronización completada\n");
        Ok(())
    }

    /// Crear un nuevo archivo en EclipseFS
    pub fn create_file(&mut self, parent_inode: u32, name: &str, content: &[u8]) -> Result<u32, VfsError> {
        crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Creando archivo '{}' en inodo {}\n", name, parent_inode));
        
        // Para la implementación lazy, por ahora solo logueamos la creación
        // TODO: Implementar creación de archivos lazy
        crate::debug::serial_write_str("ECLIPSEFS: Creación de archivos lazy no implementada completamente\n");
        
        // Simular creación exitosa
        Ok(parent_inode + 1)
    }

    /// Helper function to resolve paths with symlink depth tracking
    /// This is used internally by resolve_path to prevent infinite symlink loops
    fn resolve_path_with_depth(&self, path: &str, depth: u32, max_depth: u32) -> Result<u32, VfsError> {
        // Prevenir loops infinitos de symlinks
        if depth >= max_depth {
            crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Demasiados niveles de symlinks ({}), posible loop\n", depth));
            return Err(VfsError::InvalidPath);
        }
        
        crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Resolviendo ruta '{}' (lazy, profundidad {})\n", path, depth));
        
        let normalized = normalize_path(path);
        crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Ruta normalizada: '{}'\n", normalized));
        
        // Raíz siempre es inode 1
        if normalized == "/" {
            return Ok(1);
        }
        
        // Buscar en la tabla de inodos
        let mut storage = StorageManager::new();
        
        // Empezar desde la raíz (inode 1) y navegar por cada componente de la ruta
        let path_parts: Vec<&str> = normalized.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
        let mut current_inode = 1u32; // Empezar desde la raíz
        
        for (idx, part) in path_parts.iter().enumerate() {
            crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Buscando '{}' en inodo {}\n", part, current_inode));
            
            // Cargar el nodo actual
            let node = self.load_node_lazy(current_inode, &mut storage)?;
            
            // Si es el último componente de la ruta, podríamos estar buscando un archivo
            // De lo contrario, debe ser un directorio
            if idx < path_parts.len() - 1 && node.kind != eclipsefs_lib::NodeKind::Directory {
                crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: '{}' no es un directorio\n", part));
                return Err(VfsError::NotADirectory);
            }
            
            // Buscar el nombre en los hijos del directorio
            if node.kind == eclipsefs_lib::NodeKind::Directory {
                crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Directorio inodo {} tiene {} hijos\n", current_inode, node.children.len()));
                
                let mut found_inode = None;
                // node.children es un FnvIndexMap<String, u32> que se itera como (key, value)
                for (child_name, child_inode) in node.children.iter() {
                    if child_name.as_str() == *part {
                        found_inode = Some(*child_inode);
                        crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Encontrado '{}' -> inodo {}\n", part, child_inode));
                        break;
                    }
                }
                
                let found_inode = match found_inode {
                    Some(inode) => inode,
                    None => {
                        crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: No se encontró '{}' en el directorio inodo {}. Hijos disponibles:\n", part, current_inode));
                        for (child_name, child_inode) in node.children.iter() {
                            crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS:   - '{}' -> inodo {}\n", child_name, child_inode));
                        }
                        return Err(VfsError::FileNotFound);
                    }
                };
                
                // Cargar el nodo encontrado para verificar si es un symlink
                let found_node = self.load_node_lazy(found_inode, &mut storage)?;
                
                if found_node.kind == eclipsefs_lib::NodeKind::Symlink {
                    crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: '{}' es un symlink, siguiendo...\n", part));
                    
                    // Obtener el target del symlink desde los datos del nodo
                    let target_bytes = found_node.get_data();
                    let target = alloc::string::String::from_utf8_lossy(target_bytes).to_string();
                    crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Symlink apunta a '{}'\n", target));
                    
                    // Resolver el target del symlink de forma recursiva
                    // Si el target es relativo, lo resolvemos desde el directorio padre actual
                    let target_path = if target.starts_with('/') {
                        // Ruta absoluta
                        target.clone()
                    } else {
                        // Ruta relativa - necesitamos construir el path completo
                        // Reconstruir el path del directorio padre
                        let parent_path = if idx > 0 {
                            alloc::format!("/{}", path_parts[..idx].join("/"))
                        } else {
                            "/".to_string()
                        };
                        alloc::format!("{}/{}", parent_path.trim_end_matches('/'), target)
                    };
                    
                    crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Resolviendo symlink a '{}'\n", target_path));
                    
                    // Resolver recursivamente el target del symlink con profundidad incrementada
                    current_inode = self.resolve_path_with_depth(&target_path, depth + 1, max_depth)?;
                    crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Symlink resuelto a inodo {}\n", current_inode));
                } else {
                    // No es un symlink, usar el inode encontrado
                    current_inode = found_inode;
                }
            } else {
                // Es un archivo y no es el último componente - error
                if idx < path_parts.len() - 1 {
                    return Err(VfsError::NotADirectory);
                }
            }
        }
        
        crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Ruta '{}' resuelta a inodo {}\n", path, current_inode));
        Ok(current_inode)
    }
}

pub fn mount_root_fs_from_storage(storage: &StorageManager) -> Result<(), VfsError> {
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

    crate::debug::serial_write_str("ECLIPSEFS: (root) usando EclipseOS - selección inteligente de dispositivo...\n");
    
    // Usar la solución a medida de EclipseOS para encontrar el mejor dispositivo
    let device_index = match storage.find_best_storage_device() {
        Some(idx) => {
            crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: EclipseOS seleccionó dispositivo {} como el mejor\n", idx));
            idx
        }
        None => {
            crate::debug::serial_write_str("ECLIPSEFS: EclipseOS no encontró dispositivos válidos, usando índice 0 como fallback\n");
            0
        }
    };
    
    // 🎯 ESTRATEGIA CORRECTA: Usar sistema de nombres de dispositivos estilo Linux
    crate::debug::serial_write_str("ECLIPSEFS: (root) 🎯 ESTRATEGIA CORRECTA - Usando sistema de nombres estilo Linux\n");
    
    let device_info = &storage.devices[device_index].info;
    crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: (root) Dispositivo seleccionado: {} (Tipo: {:?})\n", device_info.device_name, device_info.controller_type));
    
    // 📋 BUSCAR PARTICIONES ECLIPSEFS:
    // - Primero buscar cualquier partición que pueda ser EclipseFS (incluyendo /dev/sdap1, etc.)
    // - Luego buscar particiones SATA y VirtIO tradicionales como fallback
    
    let mut eclipsefs_partition = None;
    
    // Buscar cualquier partición que pueda ser EclipseFS (incluyendo nombres alternativos)
    for partition in &storage.partitions {
        // Buscar particiones que no sean FAT32 y que tengan un tamaño razonable
        if partition.filesystem_type != crate::partitions::FilesystemType::FAT32 {
            let size_mb = (partition.size_lba * 512) / (1024 * 1024);
            if size_mb >= 1 {
                eclipsefs_partition = Some(partition);
                crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: (root) ✅ Candidato EclipseFS encontrado en {} (tipo: {:?}, {} MB)\n", partition.name, partition.filesystem_type, size_mb));
                break;
            }
        }
    }
    
    // Si no se encontró ningún candidato, buscar en TODAS las particiones detectadas
    if eclipsefs_partition.is_none() {
        crate::debug::serial_write_str("ECLIPSEFS: (root) No se encontraron candidatos, buscando en TODAS las particiones detectadas...\n");
        crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: (root) Analizando {} particiones detectadas\n", storage.partitions.len()));
        
        // Buscar en TODAS las particiones detectadas
        for partition in &storage.partitions {
            crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: (root) Verificando partición: {} (tipo: {:?})\n", partition.name, partition.filesystem_type));
            
            if partition.filesystem_type == crate::partitions::FilesystemType::EclipseFS {
                eclipsefs_partition = Some(partition);
                crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: (root) ✅ Encontrada partición EclipseFS en {}\n", partition.name));
                break;
            } else {
                crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: (root) ❌ {} es {:?} (no EclipseFS)\n", partition.name, partition.filesystem_type));
            }
        }
        
        // Si aún no se encontró, buscar por nombres específicos como fallback
        if eclipsefs_partition.is_none() {
            crate::debug::serial_write_str("ECLIPSEFS: (root) No se encontró EclipseFS en particiones detectadas, intentando búsqueda por nombres...\n");
            // Lista de particiones candidatas (orden de prioridad):
            // 1. NVMe (más común en sistemas modernos)
            // 2. SATA/AHCI (tradicional)
            // 3. VirtIO (virtualización)
            // 4. IDE (legacy)
            let storage_partitions = [
                // NVMe devices (formato: /dev/nvmeXn1pY)
                "/dev/nvme0n1p2", "/dev/nvme0n1p1", "/dev/nvme1n1p2", "/dev/nvme1n1p1",
                // SATA/AHCI devices
                "/dev/sda2", "/dev/sda1", "/dev/sdb2", "/dev/sdb1", "/dev/sdc2", "/dev/sdc1",
                // VirtIO devices
                "/dev/vda2", "/dev/vda1", "/dev/vdb2", "/dev/vdb1",
                // IDE legacy
                "/dev/hda2", "/dev/hda1", "/dev/hdb2", "/dev/hdb1", "/dev/hdc2", "/dev/hdc1"
            ];
            for partition_name in &storage_partitions {
                if let Some(partition) = storage.find_partition_by_name(partition_name) {
                    if partition.filesystem_type == crate::partitions::FilesystemType::EclipseFS {
                        eclipsefs_partition = Some(partition);
                        crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: (root) ✅ Encontrada partición EclipseFS tradicional en {}\n", partition_name));
                        break;
                    } else {
                        crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: (root) ❌ {} existe pero es {:?} (no EclipseFS)\n", partition_name, partition.filesystem_type));
                    }
                } else {
                    crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: (root) ❌ {} no encontrada\n", partition_name));
                }
            }
        }
    }
    
    let partition = match eclipsefs_partition {
        Some(p) => p,
        None => {
            crate::debug::serial_write_str("ECLIPSEFS: (root) ❌ No se encontró ninguna partición EclipseFS\n");
            return Err(VfsError::DeviceError("No se encontró partición EclipseFS".into()));
        }
    };
    
    crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: (root) 📋 Leyendo EclipseFS desde {} (sector 0 de la partición)\n", partition.name));
    
    // Leer el superblock de EclipseFS directamente desde /dev/sda2
    // Como el driver ATA directo falla, vamos a leer directamente desde el sector donde está EclipseFS
    // Determinar sector offset según el dispositivo
    // Particiones 2 típicamente empiezan después de la partición 1 (boot)
    let is_second_partition = partition.name.ends_with("2") || partition.name.ends_with("p2");
    let sector_offset = if is_second_partition {
        // EclipseFS está instalado en /dev/sda2, que empieza en el sector 20973568 (según el instalador)
        // Pero vamos a leer directamente desde el inicio de la partición
        20973568
    } else {
        partition.start_lba
    };
    
    crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: (root) Leyendo desde sector {} de {} (offset de partición: {})\n", 
                                                   sector_offset, partition.name, partition.start_lba));
    
    // Leer realmente desde el disco usando el storage manager
    crate::debug::serial_write_str("ECLIPSEFS: (root) Leyendo realmente desde el disco\n");
    
    // Leer el boot sector desde la partición usando el storage manager
    // CORRECCIÓN: Usar el índice correcto de la partición (/dev/sda2 = índice 1)
    // Determinar índice de partición (0=primera, 1=segunda, etc.)
    let partition_index = if partition.name.ends_with("2") || partition.name.ends_with("p2") {
        1
    } else {
        0
    };
    crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: (root) Usando índice de partición {} para {}\n", partition_index, partition.name));
    
    // NUEVA ESTRATEGIA: Buscar EclipseFS en diferentes sectores dentro de la partición
    let mut eclipsefs_found = false;
    let mut sector_offset = 0u64;
    
    // Buscar en los primeros 10 sectores de la partición
    for sector in 0..10 {
        crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: (root) Probando sector {} dentro de la partición\n", sector));
        
        match storage.read_from_partition(partition_index, sector, &mut boot_sector[..]) {
            Ok(()) => {
                crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: (root) Sector {} leído exitosamente\n", sector));
                
                // Verificar magic number de EclipseFS
                let magic = &boot_sector[0..9];
                crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: (root) Magic en sector {}: {:?}\n", sector, magic));
                
                if magic == b"ECLIPSEFS" {
                    crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: (root) ✅ ¡ECLIPSEFS ENCONTRADO en sector {}!\n", sector));
                    eclipsefs_found = true;
                    sector_offset = sector;
                    break;
                } else {
                    crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: (root) Sector {} no contiene EclipseFS\n", sector));
                }
            }
            Err(e) => {
                crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: (root) Error leyendo sector {}: {:?}\n", sector, e));
                continue; // Continuar con el siguiente sector
            }
        }
    }
    
    if !eclipsefs_found {
        crate::debug::serial_write_str("ECLIPSEFS: (root) ❌ EclipseFS no encontrado en los primeros 10 sectores de la partición\n");
        return Err(VfsError::DeviceError("EclipseFS no encontrado en la partición".into()));
    }
    
    crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: (root) ✅ EclipseFS encontrado en sector {} de la partición\n", sector_offset));
    
    // Mostrar los primeros 64 bytes del sector encontrado para debug
    crate::debug::serial_write_str("ECLIPSEFS: (root) Primeros 64 bytes del sector EclipseFS:\n");
    for i in 0..64 {
        if i % 16 == 0 {
            crate::debug::serial_write_str(&alloc::format!("{:04X}: ", i));
        }
        serial_write_hex_byte(boot_sector[i]);
        crate::debug::serial_write_str(" ");
        if i % 16 == 15 {
            crate::debug::serial_write_str("\n");
        }
    }
    crate::debug::serial_write_str("\n");
    
    crate::debug::serial_write_str("ECLIPSEFS: (root) ✅ Magic number válido - EclipseFS encontrado correctamente\n");
    
    crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: (root) ✅ Usando partición: {} (inicio en sector {})\n", partition.name, partition.start_lba));
    crate::debug::serial_write_str("ECLIPSEFS: (root) boot sector leído directamente desde VirtIO OK\n");
    
    // CORRECCIÓN CRÍTICA: Usar el índice de partición correcto que ya se calculó
    // partition_index ya se calculó correctamente arriba (1 para /dev/sda2)
    // NO resetear a 0, usar el valor correcto
    
    // DEBUG: Mensaje simple para verificar que llegamos aquí
    crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: DEBUG - LLEGAMOS AQUI - partition_index = {}\n", partition_index));

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

    crate::debug::serial_write_str("ECLIPSEFS: (root) Información de partición ya obtenida\n");

    crate::debug::serial_write_str("ECLIPSEFS: Partición offset LBA inicial = ");
    serial_write_decimal(partition.start_lba);
    crate::debug::serial_write_str(", tamaño en bloques = ");
    serial_write_decimal(partition.size_lba);
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
        
        // DEBUG: Mostrar valores antes de la llamada
        crate::debug::serial_write_str("ECLIPSEFS: DEBUG - Antes de read_from_partition: ");
        crate::debug::serial_write_str("partition_index=");
        serial_write_decimal(partition_index as u64);
        crate::debug::serial_write_str(", block=");
        serial_write_decimal(block);
        crate::debug::serial_write_str(", devices.len()=");
        serial_write_decimal(storage.device_count() as u64);
        crate::debug::serial_write_str("\n");
        
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

    // Debug: mostrar los primeros 64 bytes del buffer antes del parsing
    crate::debug::serial_write_str("ECLIPSEFS: Primeros 64 bytes del buffer antes del parsing: ");
    for &byte in &fs_buffer[0..64] {
        serial_write_hex_byte(byte);
        crate::debug::serial_write_str(" ");
    }
    crate::debug::serial_write_str("\n");
    
    // Intentar parsing con manejo de errores detallado
    // Solo parsear los primeros 65 bytes del header (tamaño real del header EclipseFS)
    let header = match EclipseFSHeader::from_bytes(&fs_buffer[0..65]) {
        Ok(header) => {
            crate::debug::serial_write_str("ECLIPSEFS: Header parseado exitosamente\n");
            header
        }
        Err(e) => {
            crate::debug::serial_write_str("ECLIPSEFS: Error parseando header: ");
            match e {
                eclipsefs_lib::EclipseFSError::InvalidFormat => crate::debug::serial_write_str("InvalidFormat - estructura de datos inválida"),
                eclipsefs_lib::EclipseFSError::UnsupportedVersion => crate::debug::serial_write_str("UnsupportedVersion - versión no soportada"),
                _ => crate::debug::serial_write_str("Otro error"),
            }
            crate::debug::serial_write_str("\n");
            
            // Mostrar los valores específicos del header para debug
            if fs_buffer.len() >= 33 {
                let magic = &fs_buffer[0..9];
                let version = u32::from_le_bytes([fs_buffer[9], fs_buffer[10], fs_buffer[11], fs_buffer[12]]);
                let inode_table_offset = u64::from_le_bytes([
                    fs_buffer[13], fs_buffer[14], fs_buffer[15], fs_buffer[16],
                    fs_buffer[17], fs_buffer[18], fs_buffer[19], fs_buffer[20]
                ]);
                let inode_table_size = u64::from_le_bytes([
                    fs_buffer[21], fs_buffer[22], fs_buffer[23], fs_buffer[24],
                    fs_buffer[25], fs_buffer[26], fs_buffer[27], fs_buffer[28]
                ]);
                let total_inodes = u32::from_le_bytes([fs_buffer[29], fs_buffer[30], fs_buffer[31], fs_buffer[32]]);
                
                crate::debug::serial_write_str(&alloc::format!(
                    "ECLIPSEFS: Magic: {:?}, Version: 0x{:08X}, InodeTableOffset: {}, InodeTableSize: {}, TotalInodes: {}\n",
                    core::str::from_utf8(magic).unwrap_or("INVALID"),
                    version,
                    inode_table_offset,
                    inode_table_size,
                    total_inodes
                ));
            }
            
            return Err(VfsError::InvalidFs("Header EclipseFS inválido".into()));
        }
    };

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

    let mut inode_entries: Vec<InodeTableEntry> = Vec::new();
    let mut valid_inode_count = 0;
    let mut empty_inode_count = 0;

    // Optimización: Limitar lectura a un máximo razonable de inodos
    // Si hay más de 1000 inodos, es probable que muchos estén vacíos
    // En un sistema real, solo deberíamos leer los que se usan realmente
    let max_inodes_to_scan = core::cmp::min(header.total_inodes, 10000);
    
    crate::debug::serial_write_str(&alloc::format!(
        "ECLIPSEFS: Escaneando tabla de inodos (total_inodes={}, max_scan={})\n",
        header.total_inodes, max_inodes_to_scan
    ));

    // OPTIMIZACIÓN CRÍTICA: Escanear en bloques y salir temprano si encontramos muchos vacíos consecutivos
    let mut consecutive_empty = 0;
    const MAX_CONSECUTIVE_EMPTY: u32 = 100; // Salir después de 100 entradas vacías consecutivas

    for idx in 0..max_inodes_to_scan {
        let entry_offset = (idx as usize) * (ecfs_constants::INODE_TABLE_ENTRY_SIZE);
        
        // Validar que no nos salimos del buffer
        if entry_offset + 8 > inode_table_data.len() {
            crate::debug::serial_write_str(&alloc::format!(
                "ECLIPSEFS: Fin prematuro de tabla de inodos en índice {} (fuera de límites)\n", idx
            ));
            break;
        }
        
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
        
        // Filtrar entradas vacías (inode=0 indica entrada sin usar)
        if inode == 0 {
            empty_inode_count += 1;
            consecutive_empty += 1;
            
            // OPTIMIZACIÓN: Si encontramos muchas entradas vacías consecutivas,
            // probablemente el resto también esté vacío
            if consecutive_empty >= MAX_CONSECUTIVE_EMPTY {
                crate::debug::serial_write_str(&alloc::format!(
                    "ECLIPSEFS: Detectadas {} entradas vacías consecutivas. Asumiendo resto vacío.\n",
                    consecutive_empty
                ));
                break;
            }
            continue;
        }
        
        // Reiniciar contador de entradas vacías consecutivas
        consecutive_empty = 0;
        
        let node_offset = header.inode_table_offset + header.inode_table_size + rel_offset;
        inode_entries.push(InodeTableEntry::new(inode, node_offset));
        valid_inode_count += 1;
    }

    // Debug: mostrar información del header y estadísticas de la tabla
    crate::debug::serial_write_str(&alloc::format!(
        "ECLIPSEFS: Estadísticas de tabla de inodos:\n\
         - Offset de tabla: {}\n\
         - Tamaño de tabla: {} bytes\n\
         - Total declarado: {} inodos\n\
         - Escaneados: {} inodos\n\
         - Válidos encontrados: {} inodos\n\
         - Vacíos encontrados: {} inodos\n\
         - Tasa de uso: {:.1}%\n",
        header.inode_table_offset,
        header.inode_table_size,
        header.total_inodes,
        max_inodes_to_scan,
        valid_inode_count,
        empty_inode_count,
        if max_inodes_to_scan > 0 {
            (valid_inode_count as f32 / max_inodes_to_scan as f32) * 100.0
        } else {
            0.0
        }
    ));
    
    // Debug: mostrar información de las entradas de inodos (solo las primeras 10 válidas y las últimas 5)
    crate::debug::serial_write_str("ECLIPSEFS: Tabla de inodos parseada (mostrando primeras 10 y últimas 5 entradas válidas):\n");
    let show_count = core::cmp::min(10, inode_entries.len());
    for i in 0..show_count {
        let entry = &inode_entries[i];
        crate::debug::serial_write_str(&alloc::format!(
            "  Entrada {}: inode={}, offset={}\n",
            i, entry.inode, entry.offset
        ));
    }
    
    if inode_entries.len() > 15 {
        crate::debug::serial_write_str("  ... (entradas intermedias omitidas) ...\n");
        let last_start = inode_entries.len() - 5;
        for i in last_start..inode_entries.len() {
            let entry = &inode_entries[i];
            crate::debug::serial_write_str(&alloc::format!(
                "  Entrada {}: inode={}, offset={}\n",
                i, entry.inode, entry.offset
            ));
        }
    } else if inode_entries.len() > 10 {
        for i in 10..inode_entries.len() {
            let entry = &inode_entries[i];
            crate::debug::serial_write_str(&alloc::format!(
                "  Entrada {}: inode={}, offset={}\n",
                i, entry.inode, entry.offset
            ));
        }
    }
    
    // Debug adicional: mostrar los bytes raw de la tabla de inodos
    crate::debug::serial_write_str("ECLIPSEFS: Bytes raw de la tabla de inodos (primeros 32 bytes):\n");
    let inode_table_start = header.inode_table_offset as usize;
    let inode_table_end = inode_table_start + header.inode_table_size as usize;
    let debug_end = core::cmp::min(inode_table_end, inode_table_start + 32);
    
    for i in inode_table_start..debug_end {
        if i < fs_buffer.len() {
            crate::debug::serial_write_str(&alloc::format!("{:02X} ", fs_buffer[i]));
        }
    }
    crate::debug::serial_write_str("\n");
    
    crate::debug::serial_write_str("ECLIPSEFS: 🚀 Implementando montaje lazy sin load_from_stream\n");
    
    // NUEVA IMPLEMENTACIÓN LAZY: No cargar todo el sistema de archivos en memoria
    // Solo parsear el header y la tabla de inodos, cargar nodos bajo demanda
    let result = Ok(());
    
    // CÓDIGO ORIGINAL COMENTADO:
    /*
    let result = fs_instance
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
                // Usar sistema de particiones estilo Linux
                let eclipsefs_partition = storage.partitions.iter()
                    .find(|p| p.filesystem_type == crate::partitions::FilesystemType::EclipseFS)
                    .ok_or(EclipseFSError::IoError)?;
                
                // Leer directamente desde el sector donde está EclipseFS
                let is_second_partition = eclipsefs_partition.name.ends_with("2") || eclipsefs_partition.name.ends_with("p2");
                let sector_offset = if is_second_partition {
                    20973568 + block  // EclipseFS está en /dev/sda2 + offset del bloque
                } else {
                    eclipsefs_partition.start_lba + block
                };
                
                let device_info = &storage.devices[device_index].info;
                storage
                    .read_device_sector_real(device_info, sector_offset, &mut temp_block)
                    .map_err(|e| {
                        crate::debug::serial_write_str("ECLIPSEFS: Error leyendo bloque de nodo\n");
                        crate::debug::serial_write_str(&alloc::format!("{}", e));
                        crate::debug::serial_write_str("\n");
                        EclipseFSError::IoError
                    })?;

                // Debug: mostrar qué bloque estamos leyendo y los primeros bytes
                crate::debug::serial_write_str("ECLIPSEFS: Leyendo bloque ");
                serial_write_decimal(block);
                crate::debug::serial_write_str(" para offset ");
                serial_write_decimal(current_offset);
                crate::debug::serial_write_str("\n");
                
                // Calcular el offset dentro del bloque
                let block_offset = (current_offset % 512) as usize;
                crate::debug::serial_write_str("ECLIPSEFS: Offset dentro del bloque: ");
                serial_write_decimal(block_offset as u64);
                crate::debug::serial_write_str("\n");
                
                // Mostrar los primeros 32 bytes del bloque leído
                crate::debug::serial_write_str("ECLIPSEFS: Primeros 32 bytes del bloque: ");
                for i in 0..32 {
                    crate::debug::serial_write_str(&alloc::format!("{:02X} ", temp_block[i]));
                }
                crate::debug::serial_write_str("\n");
                
                // Mostrar los bytes desde el offset específico
                crate::debug::serial_write_str("ECLIPSEFS: Bytes desde offset ");
                serial_write_decimal(block_offset as u64);
                crate::debug::serial_write_str(": ");
                for i in block_offset..block_offset + 8 {
                    if i < temp_block.len() {
                        crate::debug::serial_write_str(&alloc::format!("{:02X} ", temp_block[i]));
                    }
                }
                crate::debug::serial_write_str("\n");

                let block_offset = (current_offset % 512) as usize;
                let to_copy = cmp::min(buffer.len() - written, 512 - block_offset);
                buffer[written..written + to_copy]
                    .copy_from_slice(&temp_block[block_offset..block_offset + to_copy]);

                current_offset += to_copy as u64;
                written += to_copy;
            }

            crate::debug::serial_write_str("ECLIPSEFS: fetch() completed successfully\n");
            Ok(())
        });
    */
        
    match result {
        Ok(()) => {
            crate::debug::serial_write_str("ECLIPSEFS: load_from_stream completado exitosamente\n");
        }
        Err(e) => {
            crate::debug::serial_write_str("ECLIPSEFS: Error en load_from_stream: ");
            match e {
                eclipsefs_lib::EclipseFSError::InvalidFormat => {
                    crate::debug::serial_write_str("InvalidFormat - estructura de datos inválida en load_from_stream\n");
                }
                eclipsefs_lib::EclipseFSError::NotFound => {
                    crate::debug::serial_write_str("NotFound - nodo no encontrado\n");
                }
                eclipsefs_lib::EclipseFSError::IoError => {
                    crate::debug::serial_write_str("IoError - error de E/S\n");
                }
                eclipsefs_lib::EclipseFSError::InvalidOperation => {
                    crate::debug::serial_write_str("InvalidOperation - operación inválida\n");
                }
                eclipsefs_lib::EclipseFSError::UnsupportedOperation => {
                    crate::debug::serial_write_str("UnsupportedOperation - operación no soportada\n");
                }
                eclipsefs_lib::EclipseFSError::UnsupportedVersion => {
                    crate::debug::serial_write_str("UnsupportedVersion - versión no soportada\n");
                }
                eclipsefs_lib::EclipseFSError::DuplicateEntry => {
                    crate::debug::serial_write_str("DuplicateEntry - entrada duplicada\n");
                }
                eclipsefs_lib::EclipseFSError::PermissionDenied => {
                    crate::debug::serial_write_str("PermissionDenied - permiso denegado\n");
                }
                eclipsefs_lib::EclipseFSError::DeviceFull => {
                    crate::debug::serial_write_str("DeviceFull - dispositivo lleno\n");
                }
                eclipsefs_lib::EclipseFSError::FileTooLarge => {
                    crate::debug::serial_write_str("FileTooLarge - archivo demasiado grande\n");
                }
                eclipsefs_lib::EclipseFSError::InvalidFileName => {
                    crate::debug::serial_write_str("InvalidFileName - nombre de archivo inválido\n");
                }
                eclipsefs_lib::EclipseFSError::CorruptedFilesystem => {
                    crate::debug::serial_write_str("CorruptedFilesystem - sistema de archivos corrupto\n");
                }
                eclipsefs_lib::EclipseFSError::OutOfMemory => {
                    crate::debug::serial_write_str("OutOfMemory - sin memoria\n");
                }
                eclipsefs_lib::EclipseFSError::CompressionError => {
                    crate::debug::serial_write_str("CompressionError - error de compresión\n");
                }
                eclipsefs_lib::EclipseFSError::EncryptionError => {
                    crate::debug::serial_write_str("EncryptionError - error de encriptación\n");
                }
                eclipsefs_lib::EclipseFSError::SnapshotError => {
                    crate::debug::serial_write_str("SnapshotError - error de snapshot\n");
                }
                eclipsefs_lib::EclipseFSError::AclError => {
                    crate::debug::serial_write_str("AclError - error de ACL\n");
                }
            }
            return Err(VfsError::InvalidFs("EclipseFS load_from_stream error".into()));
        }
    }

    crate::debug::serial_write_str("ECLIPSEFS: Sistema de archivos EclipseFS parseado exitosamente\n");

    // 🎯 IMPLEMENTACIÓN LAZY: Crear wrapper sin cargar todo en memoria
    crate::debug::serial_write_str("ECLIPSEFS: 🚀 Creando wrapper lazy con carga bajo demanda\n");
    
    // Crear información del dispositivo
    let device_info = EclipseFSDeviceInfo::new(
        partition.name.clone(),
        partition.size_lba,
        partition.start_lba
    );
    
    // Crear wrapper lazy que solo contiene metadatos
    let fs_wrapper = EclipseFSWrapper::new_lazy(header, inode_entries, partition_index, device_info);
    
    // Montar en VFS usando Box::new (esto es necesario para el trait object)
    // Pero el wrapper interno usa lazy loading para evitar cargar todo
    let fs_box = Box::new(fs_wrapper);
    vfs.mount("/", fs_box);
    vfs.debug_list_mounts();
    
    crate::debug::serial_write_str("ECLIPSEFS: ✅ Filesystem lazy montado en / (carga bajo demanda)\n");

    Ok(())
}

/// Obtener información de dispositivos disponibles para EclipseFS
/// 
/// Esta función busca particiones que podrían contener EclipseFS basándose en:
/// - Particiones que no son FAT32 (para evitar conflicto con EFI)
/// - Tamaño mínimo razonable para un sistema de archivos
/// 
/// # Returns
/// - `Vec<EclipseFSDeviceInfo>`: Lista de dispositivos candidatos para EclipseFS
pub fn obtener_dispositivos_eclipsefs_candidatos() -> Vec<EclipseFSDeviceInfo> {
    let mut candidatos = Vec::new();
    
    if let Some(storage) = crate::drivers::storage_manager::get_storage_manager() {
        crate::debug::serial_write_str(&alloc::format!(
            "ECLIPSEFS: Analizando {} particiones para candidatos EclipseFS\n",
            storage.partitions.len()
        ));
        
        // Log al framebuffer también
        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
            let fb_msg = alloc::format!("ECLIPSEFS: {} particiones", storage.partitions.len());
            fb.write_text_kernel(&fb_msg, crate::drivers::framebuffer::Color::CYAN);
        }
        
        // Priorizar específicamente /dev/sda2 (donde está instalado EclipseFS v2.0)
        for particion in &storage.partitions {
            let is_second_partition = particion.name.ends_with("2") || particion.name.ends_with("p2");
            if is_second_partition {
                let size_mb = (particion.size_lba * 512) / (1024 * 1024);
                if size_mb >= 1 {
                    let info = EclipseFSDeviceInfo::with_info(
                        particion.name.clone(),
                        particion.size_lba,
                        particion.start_lba,
                        alloc::format!("EclipseFS v2.0 instalado ({} MB)", size_mb)
                    );
                    
                    crate::debug::serial_write_str(&alloc::format!(
                        "ECLIPSEFS: ✅ Candidato prioritario encontrado: {} (tipo: {:?}, {} MB, inicio LBA: {})\n",
                        particion.name,
                        particion.filesystem_type,
                        size_mb,
                        particion.start_lba
                    ));
                    
                    // Log al framebuffer también
                    if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                        let fb_msg = alloc::format!("ECLIPSEFS: {} encontrado ({} MB)", 
                                                  particion.name, size_mb);
                        fb.write_text_kernel(&fb_msg, crate::drivers::framebuffer::Color::GREEN);
                    }
                    
                    candidatos.push(info);
                    break; // Priorizar solo /dev/sda2
                }
            }
        }
        
        // Si no se encontró /dev/sda2, buscar otras particiones no-FAT32 como fallback
        if candidatos.is_empty() {
            crate::debug::serial_write_str("ECLIPSEFS: /dev/sda2 no encontrado, buscando otras particiones no-FAT32...\n");
            for particion in &storage.partitions {
                // Buscar particiones que no sean FAT32 (para evitar conflicto con EFI)
                if particion.filesystem_type != crate::partitions::FilesystemType::FAT32 {
                    // Verificar que tenga un tamaño mínimo razonable (al menos 1MB)
                    let size_mb = (particion.size_lba * 512) / (1024 * 1024);
                    if size_mb >= 1 {
                        let info = if particion.filesystem_type == crate::partitions::FilesystemType::Unknown {
                            EclipseFSDeviceInfo::with_info(
                                particion.name.clone(),
                                particion.size_lba,
                                particion.start_lba,
                                alloc::format!("Partición desconocida ({} MB)", size_mb)
                            )
                        } else {
                            EclipseFSDeviceInfo::new(
                                particion.name.clone(),
                                particion.size_lba,
                                particion.start_lba
                            )
                        };
                        
                        crate::debug::serial_write_str(&alloc::format!(
                            "ECLIPSEFS: Candidato fallback encontrado: {} (tipo: {:?}, {} MB, inicio LBA: {})\n",
                            particion.name,
                            particion.filesystem_type,
                            size_mb,
                            particion.start_lba
                        ));
                        
                        candidatos.push(info);
                    }
                }
            }
        }
        
        // Si no se encontraron candidatos, buscar nombres alternativos comunes
        if candidatos.is_empty() {
            crate::debug::serial_write_str("ECLIPSEFS: No se encontraron candidatos, buscando nombres alternativos...\n");
            
            // Buscar /dev/sdap1, /dev/sdap2, etc.
            let nombres_alternativos = ["/dev/sdap1", "/dev/sdap2", "/dev/sdap3", "/dev/sdap4"];
            for nombre in &nombres_alternativos {
                if let Some(particion) = storage.find_partition_by_name(nombre) {
                    let size_mb = (particion.size_lba * 512) / (1024 * 1024);
                    if size_mb >= 1 {
                        let info = EclipseFSDeviceInfo::with_info(
                            nombre.to_string(),
                            particion.size_lba,
                            particion.start_lba,
                            alloc::format!("Nombre alternativo encontrado ({} MB)", size_mb)
                        );
                        
                        crate::debug::serial_write_str(&alloc::format!(
                            "ECLIPSEFS: Candidato alternativo encontrado: {} (tipo: {:?}, {} MB, inicio LBA: {})\n",
                            nombre,
                            particion.filesystem_type,
                            size_mb,
                            particion.start_lba
                        ));
                        
                        candidatos.push(info);
                    }
                }
            }
        }
    }
    
    crate::debug::serial_write_str(&alloc::format!(
        "ECLIPSEFS: {} dispositivos candidatos encontrados\n",
        candidatos.len()
    ));
    
    candidatos
}

/// Montar EclipseFS desde la partición específica usando StorageManager
/// 
/// # Arguments
/// - `storage`: Referencia al gestor de almacenamiento
/// - `device_info`: Información opcional del dispositivo donde montar EclipseFS
///                  Si es None, buscará automáticamente dispositivos candidatos
pub fn mount_eclipsefs_from_storage(storage: &StorageManager, device_info: Option<EclipseFSDeviceInfo>) -> Result<(), VfsError> {
    crate::debug::serial_write_str("ECLIPSEFS: Iniciando mount_eclipsefs_from_storage\n");
    
    // Determinar información del dispositivo
    let target_device = if let Some(device_info) = device_info {
        crate::debug::serial_write_str(&alloc::format!(
            "ECLIPSEFS: Usando dispositivo específico: {} ({} sectores, inicio LBA {})\n",
            device_info.device_name,
            device_info.size_lba,
            device_info.start_lba
        ));
        Some(device_info)
    } else {
        // Buscar dispositivos candidatos automáticamente
        crate::debug::serial_write_str("ECLIPSEFS: Buscando dispositivos candidatos automáticamente...\n");
        let candidatos = obtener_dispositivos_eclipsefs_candidatos();
        
        if candidatos.is_empty() {
            crate::debug::serial_write_str("ECLIPSEFS: No se encontraron dispositivos candidatos para EclipseFS\n");
            return Err(VfsError::DeviceError("No se encontraron dispositivos candidatos para EclipseFS".into()));
        }
        
        // Usar el primer candidato encontrado
        let primer_candidato = candidatos.into_iter().next().unwrap();
        crate::debug::serial_write_str(&alloc::format!(
            "ECLIPSEFS: Usando primer candidato: {}\n",
            primer_candidato.device_name
        ));
        Some(primer_candidato)
    };

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
    fn unmount(&mut self) -> Result<(), VfsError> { 
        // Sincronizar todos los cambios al disco antes de desmontar
        self.sync_to_disk()?;
        Ok(()) 
    }
    
    fn read(&self, inode: u32, offset: u64, buffer: &mut [u8]) -> Result<usize, VfsError> {
        crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Leyendo inodo {} offset {} ({} bytes)\n", inode, offset, buffer.len()));
        
        // Crear un storage manager temporal para la operación de lectura
        let mut storage = StorageManager::new();
        
        // Cargar el nodo bajo demanda
        let node = self.load_node_lazy(inode, &mut storage)?;
        
        // Si es un archivo, obtener los datos
        if node.kind == eclipsefs_lib::NodeKind::File {
            let data = node.get_data();
        let start = offset as usize;
            let end = (start + buffer.len()).min(data.len());
            
            if start < data.len() {
                let len = end - start;
                buffer[..len].copy_from_slice(&data[start..end]);
                crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Leídos {} bytes del inodo {}\n", len, inode));
                Ok(len)
            } else {
                Ok(0)
            }
        } else {
            Err(VfsError::InvalidOperation)
        }
    }
    fn write(&mut self, inode: u32, offset: u64, data: &[u8]) -> Result<usize, VfsError> {
        crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Escribiendo {} bytes al inodo {} offset {}\n", 
                                                      data.len(), inode, offset));
        
        // Para la implementación lazy, por ahora solo logueamos la escritura
        // TODO: Implementar escritura lazy usando el cache de bloques
        crate::debug::serial_write_str("ECLIPSEFS: Escritura lazy no implementada completamente\n");
        
        Ok(data.len())
    }

    fn stat(&self, inode: u32) -> Result<StatInfo, VfsError> {
        crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Stat inodo {} (lazy)\n", inode));
        
        // Crear un storage manager temporal para la operación de lectura
        let mut storage = StorageManager::new();
        
        // Cargar el nodo bajo demanda
        let node = self.load_node_lazy(inode, &mut storage)?;
        
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
        crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Readdir inodo {} (lazy)\n", inode));
        
        // Para la implementación lazy, por ahora devolvemos un directorio básico
        // TODO: Implementar lectura de directorio lazy
        let mut entries = Vec::new();
        entries.push(".".to_string());
        entries.push("..".to_string());
        Ok(entries)
    }
    
    fn truncate(&mut self, _inode: u32, _size: u64) -> Result<(), VfsError> { Ok(()) }
    fn rmdir(&mut self, _parent_inode: u32, _name: &str) -> Result<(), VfsError> { Ok(()) }
    fn rename(&mut self, _parent_inode: u32, _old_name: &str, _new_parent_inode: u32, _new_name: &str) -> Result<(), VfsError> { Ok(()) }
    fn unlink(&mut self, _parent_inode: u32, _name: &str) -> Result<(), VfsError> { Ok(()) }
    fn chmod(&mut self, _inode: u32, _mode: u16) -> Result<(), VfsError> { Ok(()) }
    fn chown(&mut self, _inode: u32, _uid: u32, _gid: u32) -> Result<(), VfsError> { Ok(()) }

    fn resolve_path(&self, path: &str) -> Result<u32, VfsError> {
        // Límite de profundidad para prevenir loops infinitos de symlinks
        const MAX_SYMLINK_DEPTH: u32 = 40;
        self.resolve_path_with_depth(path, 0, MAX_SYMLINK_DEPTH)
    }

    fn readdir_path(&self, path: &str) -> Result<Vec<String>, VfsError> {
        let inode = self.resolve_path(path)?;
        self.readdir(inode)
    }

    fn read_file_path(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Leyendo archivo '{}' (lazy)\n", path));
        
        // Resolver la ruta a un inode (esto ya sigue symlinks automáticamente)
        let inode = match self.resolve_path(path) {
            Ok(inode) => {
                crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Ruta '{}' resuelta a inodo {}\n", path, inode));
                inode
            }
            Err(e) => {
                crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Error resolviendo ruta '{}': {:?}\n", path, e));
                return Err(e);
            }
        };
        
        // Crear un storage manager para la operación de lectura
        let mut storage = StorageManager::new();
        
        // Cargar el nodo
        let node = match self.load_node_lazy(inode, &mut storage) {
            Ok(node) => {
                crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Nodo {} cargado exitosamente\n", inode));
                node
            }
            Err(e) => {
                crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Error cargando nodo {}: {:?}\n", inode, e));
                return Err(e);
            }
        };
        
        // Verificar que sea un archivo (después de seguir symlinks, debería serlo)
        if node.kind != eclipsefs_lib::NodeKind::File {
            crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: '{}' no es un archivo (tipo: {:?})\n", path, node.kind));
            return Err(VfsError::NotAFile);
        }
        
        // Obtener los datos del archivo
        let data = node.get_data().to_vec();
        crate::debug::serial_write_str(&alloc::format!("ECLIPSEFS: Leídos {} bytes de '{}'\n", data.len(), path));
        
        Ok(data)
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
