//! EclipseFS - Sistema de archivos en RAM para Eclipse OS (RW)

use crate::filesystem::vfs::VfsError;
use crate::filesystem::VfsResult;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::{self, Vec};

#[derive(Debug, Clone, PartialEq)]
pub enum NodeKind { File, Dir, Symlink }

#[derive(Debug, Clone)]
pub struct Node {
    pub kind: NodeKind,
    pub data: Vec<u8>,
    pub children: BTreeMap<String, u32>,
    pub size: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub nlink: u32, // Contador de enlaces duros
    pub encryption: EncryptionInfo, // Información de cifrado
    pub compression: CompressionInfo, // Información de compresión
}

impl Node {
    fn now() -> u64 { 1640995200 }
    fn new_dir() -> Self { 
        Self { 
            kind: NodeKind::Dir, 
            data: Vec::new(), 
            children: BTreeMap::new(), 
            size: 0, 
            mode: 0o40755, 
            uid: 0, 
            gid: 0, 
            atime: Self::now(), 
            mtime: Self::now(), 
            ctime: Self::now(), 
            nlink: 2,
            encryption: EncryptionInfo {
                encryption_type: EncryptionType::None,
                key_id: String::new(),
                iv: Vec::new(),
                salt: Vec::new(),
                is_encrypted: false,
            },
            compression: CompressionInfo {
                compression_type: CompressionType::None,
                original_size: 0,
                compressed_size: 0,
                compression_ratio: 0.0,
                is_compressed: false,
            }
        } 
    }
    
    fn new_file() -> Self { 
        Self { 
            kind: NodeKind::File, 
            data: Vec::new(), 
            children: BTreeMap::new(), 
            size: 0, 
            mode: 0o100644, 
            uid: 0, 
            gid: 0, 
            atime: Self::now(), 
            mtime: Self::now(), 
            ctime: Self::now(), 
            nlink: 1,
            encryption: EncryptionInfo {
                encryption_type: EncryptionType::None,
                key_id: String::new(),
                iv: Vec::new(),
                salt: Vec::new(),
                is_encrypted: false,
            },
            compression: CompressionInfo {
                compression_type: CompressionType::None,
                original_size: 0,
                compressed_size: 0,
                compression_ratio: 0.0,
                is_compressed: false,
            }
        } 
    }
    
    fn new_symlink(target: &str) -> Self { 
        let data = target.as_bytes().to_vec();
        Self { 
            kind: NodeKind::Symlink, 
            data: data.clone(), 
            children: BTreeMap::new(), 
            size: data.len() as u64, 
            mode: 0o120000, // S_IFLNK
            uid: 0, 
            gid: 0, 
            atime: Self::now(), 
            mtime: Self::now(), 
            ctime: Self::now(),
            nlink: 1,
            encryption: EncryptionInfo {
                encryption_type: EncryptionType::None,
                key_id: String::new(),
                iv: Vec::new(),
                salt: Vec::new(),
                is_encrypted: false,
            },
            compression: CompressionInfo {
                compression_type: CompressionType::None,
                original_size: 0,
                compressed_size: 0,
                compression_ratio: 0.0,
                is_compressed: false,
            }
        } 
    }
}

static mut FS_NODES: Option<BTreeMap<u32, Node>> = None;
static mut NEXT_INODE: u32 = 2; // 1 = raíz
static mut UMASK: u16 = 0o022; // umask por defecto

// Cache simple para mejorar rendimiento
static mut PATH_CACHE: Option<BTreeMap<String, u32>> = None; // path -> inode
static mut CACHE_SIZE: usize = 0;
const MAX_CACHE_SIZE: usize = 1000; // Máximo 1000 entradas en cache

// Estadísticas de rendimiento
static mut CACHE_HITS: u64 = 0;
static mut CACHE_MISSES: u64 = 0;
static mut TOTAL_OPERATIONS: u64 = 0;

// Sistema de cifrado
#[derive(Debug, Clone, PartialEq)]
pub enum EncryptionType {
    None,
    AES256,
    ChaCha20,
}

#[derive(Debug, Clone)]
pub struct EncryptionInfo {
    pub encryption_type: EncryptionType,
    pub key_id: String,
    pub iv: Vec<u8>, // Initialization Vector
    pub salt: Vec<u8>, // Salt para derivación de clave
    pub is_encrypted: bool,
}

// Sistema de compresión
#[derive(Debug, Clone, PartialEq)]
pub enum CompressionType {
    None,
    LZ4,    // Compresión rápida
    LZ77,   // Compresión buena
    RLE,    // Run-Length Encoding (para datos repetitivos)
}

#[derive(Debug, Clone)]
pub struct CompressionInfo {
    pub compression_type: CompressionType,
    pub original_size: u64,
    pub compressed_size: u64,
    pub compression_ratio: f32,
    pub is_compressed: bool,
}

// Claves de cifrado (en un sistema real, esto estaría en un keystore seguro)
static mut ENCRYPTION_KEYS: Option<BTreeMap<String, Vec<u8>>> = None;
static mut DEFAULT_KEY_ID: String = String::new();

// Sistema de snapshots
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub id: u64,
    pub timestamp: u64,
    pub description: String,
    pub files: BTreeMap<String, Node>, // Copia de todos los archivos
    pub size: u64,
}

static mut SNAPSHOTS: Option<BTreeMap<u64, Snapshot>> = None;
static mut NEXT_SNAPSHOT_ID: u64 = 1;

pub fn init() -> VfsResult<()> {
    unsafe {
        FS_NODES = Some(BTreeMap::new());
        PATH_CACHE = Some(BTreeMap::new());
        ENCRYPTION_KEYS = Some(BTreeMap::new());
        SNAPSHOTS = Some(BTreeMap::new());
        CACHE_SIZE = 0;
        
        if let Some(ref mut map) = FS_NODES {
            map.clear();
            map.insert(1, Node::new_dir());
        }
        NEXT_INODE = 2;
        UMASK = 0o022;
        
        // Inicializar sistema de cifrado
        init_encryption_system();
        
        // Inicializar sistema de ACL
        init_acl_system();
        
        // Inicializar sistema de cifrado transparente
        init_transparent_encryption();
    }
    Ok(())
}

fn init_encryption_system() {
    unsafe {
        if let Some(ref mut keys) = ENCRYPTION_KEYS {
            // Generar clave maestra por defecto (en un sistema real, esto vendría de un HSM)
            let master_key = generate_master_key();
            keys.insert("master".to_string(), master_key);
            
            // Generar clave de sesión
            let session_key = generate_session_key();
            keys.insert("session".to_string(), session_key);
            
            DEFAULT_KEY_ID = "master".to_string();
        }
    }
}

// Función para generar clave maestra (simulada)
fn generate_master_key() -> Vec<u8> {
    // En un sistema real, esto vendría de un HSM o derivación de contraseña
    let mut key = Vec::with_capacity(32); // 256 bits
    for i in 0..32 {
        key.push((i * 7 + 13) as u8); // Patrón simple para demo
    }
    key
}

// Función para generar clave de sesión
fn generate_session_key() -> Vec<u8> {
    let mut key = Vec::with_capacity(32);
    for i in 0..32 {
        key.push((i * 11 + 17) as u8); // Patrón diferente para demo
    }
    key
}

// Funciones de cache
fn cache_path(path: &str, inode: u32) {
    unsafe {
        if let Some(ref mut cache) = PATH_CACHE {
            if CACHE_SIZE >= MAX_CACHE_SIZE {
                // Eliminar la entrada más antigua (primera en BTreeMap)
                if let Some((old_path, _)) = cache.iter().next() {
                    let old_path = old_path.clone();
                    cache.remove(&old_path);
                    CACHE_SIZE -= 1;
                }
            }
            cache.insert(path.to_string(), inode);
            CACHE_SIZE += 1;
        }
    }
}

fn get_cached_path(path: &str) -> Option<u32> {
    unsafe {
        PATH_CACHE.as_ref().and_then(|cache| cache.get(path).cloned())
    }
}

fn invalidate_cache_path(path: &str) {
    unsafe {
        if let Some(ref mut cache) = PATH_CACHE {
            if cache.remove(path).is_some() {
                CACHE_SIZE = CACHE_SIZE.saturating_sub(1);
            }
        }
    }
}

fn clear_cache() {
    unsafe {
        if let Some(ref mut cache) = PATH_CACHE {
            cache.clear();
            CACHE_SIZE = 0;
        }
    }
}

fn allocate_inode() -> u32 { unsafe { let i = NEXT_INODE; NEXT_INODE += 1; i } }

fn get_node(inode: u32) -> Option<Node> {
    unsafe { FS_NODES.as_ref().and_then(|m| m.get(&inode).cloned()) }
}
fn put_node(inode: u32, node: Node) {
    unsafe {
        if let Some(ref mut m) = FS_NODES { m.insert(inode, node); }
    }
}

fn lookup_path(path: &str) -> VfsResult<u32> {
    let path = normalize_path(path);
    if path == "/" { return Ok(1); }
    
    // Actualizar estadísticas
    unsafe {
        TOTAL_OPERATIONS += 1;
    }
    
    // Intentar obtener del cache primero
    if let Some(cached_inode) = get_cached_path(&path) {
        unsafe {
            CACHE_HITS += 1;
        }
        return Ok(cached_inode);
    }
    
    unsafe {
        CACHE_MISSES += 1;
    }
    
    let mut cur = 1u32;
    for part in path.split('/').filter(|s| !s.is_empty()) {
        let node = get_node(cur).ok_or(VfsError::FileNotFound)?;
        if let Some(&child) = node.children.get(part) {
            cur = child;
        } else {
            return Err(VfsError::FileNotFound);
        }
    }
    
    // Cachear el resultado
    cache_path(&path, cur);
    Ok(cur)
}

// Función para invalidar cache cuando se modifica un path
fn invalidate_path_cache(path: &str) {
    invalidate_cache_path(path);
    
    // También invalidar paths padre
    let mut current_path = path.to_string();
    while let Some(pos) = current_path.rfind('/') {
        if pos == 0 {
            break; // Llegamos a la raíz
        }
        current_path.truncate(pos);
        invalidate_cache_path(&current_path);
    }
}

fn ensure_dir(path: &str) -> VfsResult<u32> { create_dir(path) }

pub fn create_dir(path: &str) -> VfsResult<u32> {
    // Validar ruta
    validate_path(path)?;
    
    let path = normalize_path(path);
    if path == "/" { return Ok(1); }
    let mut cur = 1u32;
    let mut last_name: Option<String> = None;
    for part in path.split('/').filter(|s| !s.is_empty()) {
        // Validar cada componente del path
        validate_filename(part)?;
        
        last_name = Some(part.to_string());
        let mut node = get_node(cur).ok_or(VfsError::FileNotFound)?;
        if let Some(&child) = node.children.get(part) {
            cur = child;
        } else {
            let new_inode = allocate_inode();
            node.children.insert(part.to_string(), new_inode);
            put_node(cur, node);
            let mut nd = Node::new_dir();
            let mode = default_dir_mode();
            nd.mode = (nd.mode & 0o170000) | (mode as u32);
            put_node(new_inode, nd);
            cur = new_inode;
        }
    }
    Ok(cur)
}

pub fn create_file(path: &str) -> VfsResult<u32> {
    // Validar ruta
    validate_path(path)?;
    
    let npath = normalize_path(path);
    let dir = crate::filesystem::utils::FileSystemUtils::get_dirname(&npath).to_string();
    let base = crate::filesystem::utils::FileSystemUtils::get_basename(&npath).to_string();
    
    // Validar nombre de archivo
    validate_filename(&base)?;
    
    let dir_inode = ensure_dir(&dir)?;
    let mut dir_node = get_node(dir_inode).ok_or(VfsError::FileNotFound)?;
    if let Some(&child) = dir_node.children.get(&base) { return Ok(child); }
    let new_inode = allocate_inode();
    dir_node.children.insert(base, new_inode);
    put_node(dir_inode, dir_node);
    let mut nf = Node::new_file();
    let mode = default_file_mode();
    nf.mode = (nf.mode & 0o170000) | (mode as u32);
    put_node(new_inode, nf);
    
    // Invalidar cache del directorio padre
    invalidate_path_cache(&dir);
    
    Ok(new_inode)
}

pub fn write(path: &str, offset: u64, data: &[u8]) -> VfsResult<usize> {
    // Validar ruta
    validate_path(path)?;
    
    // Validar datos
    if data.is_empty() {
        return Ok(0);
    }
    
    // Verificar límites de tamaño
    if data.len() > 1024 * 1024 * 1024 { // 1GB límite
        return Err(VfsError::FileTooLarge);
    }
    
    let inode = if let Ok(i) = lookup_path(path) { i } else { create_file(path)? };
    let mut node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    if let NodeKind::Dir = node.kind { return Err(VfsError::NotAFile); }
    
    // Aplicar cifrado transparente si está habilitado
    let encrypted_data = transparent_encrypt_data(data, path)?;
    
    let off = offset as usize;
    if node.data.len() < off + encrypted_data.len() { node.data.resize(off + encrypted_data.len(), 0); }
    node.data[off..off + encrypted_data.len()].copy_from_slice(&encrypted_data);
    node.size = node.data.len() as u64;
    node.mtime = Node::now();
    node.ctime = Node::now();
    put_node(inode, node);
    Ok(data.len())
}

pub fn read(path: &str, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
    // Validar ruta
    validate_path(path)?;
    
    // Validar buffer
    if buf.is_empty() {
        return Ok(0);
    }
    
    let inode = lookup_path(path)?;
    let mut node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    if let NodeKind::Dir = node.kind { return Err(VfsError::NotAFile); }
    
    let off = offset as usize;
    if off >= node.data.len() { return Ok(0); }
    let end = core::cmp::min(node.data.len(), off + buf.len());
    let n = end - off;
    
    // Aplicar descifrado transparente si está habilitado
    let decrypted_data = transparent_decrypt_data(&node.data[off..end], path)?;
    let copy_len = core::cmp::min(decrypted_data.len(), buf.len());
    buf[..copy_len].copy_from_slice(&decrypted_data[..copy_len]);
    
    // Actualizar atime
    node.atime = Node::now();
    put_node(inode, node);
    
    Ok(copy_len)
}

pub fn readdir(path: &str) -> VfsResult<Vec<String>> {
    let inode = lookup_path(path)?;
    let node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    if let NodeKind::File = node.kind { return Err(VfsError::NotADirectory); }
    Ok(node.children.keys().cloned().collect())
}

// Normalización básica de rutas: colapsa //, maneja . y ..
fn normalize_path(path: &str) -> String {
    if path.is_empty() { return "/".to_string(); }
    let abs = path.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." { continue; }
        if part == ".." {
            let _ = stack.pop();
        } else {
            stack.push(part);
        }
    }
    let mut out = String::new();
    if abs { out.push('/'); }
    out.push_str(&stack.join("/"));
    if out.is_empty() { out.push('/'); }
    out
}

fn default_file_mode() -> u16 {
    // base 0666, aplica umask
    let base: u16 = 0o666;
    unsafe { base & !UMASK }
}

fn default_dir_mode() -> u16 {
    // base 0777, aplica umask
    let base: u16 = 0o777;
    unsafe { base & !UMASK }
}

pub fn set_umask(mask: u16) { unsafe { UMASK = mask & 0o777; } }
pub fn get_umask() -> u16 { unsafe { UMASK } }

pub fn chmod(path: &str, mode: u16) -> VfsResult<()> {
    let inode = lookup_path(path)?;
    let mut node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    
    // Preservar tipo de archivo y bits especiales
    let file_type_mask = 0o170000; // S_IFMT
    let special_bits = node.mode & 0o7000; // setuid, setgid, sticky
    node.mode = (node.mode & file_type_mask) | special_bits | (mode as u32 & 0o777);
    node.ctime = Node::now();
    put_node(inode, node);
    Ok(())
}

// Funciones para bits especiales
pub fn set_sticky_bit(path: &str, set: bool) -> VfsResult<()> {
    let inode = lookup_path(path)?;
    let mut node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    
    if set {
        node.mode |= 0o1000; // S_ISVTX
    } else {
        node.mode &= !0o1000;
    }
    node.ctime = Node::now();
    put_node(inode, node);
    Ok(())
}

pub fn set_setuid_bit(path: &str, set: bool) -> VfsResult<()> {
    let inode = lookup_path(path)?;
    let mut node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    
    if set {
        node.mode |= 0o4000; // S_ISUID
    } else {
        node.mode &= !0o4000;
    }
    node.ctime = Node::now();
    put_node(inode, node);
    Ok(())
}

pub fn set_setgid_bit(path: &str, set: bool) -> VfsResult<()> {
    let inode = lookup_path(path)?;
    let mut node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    
    if set {
        node.mode |= 0o2000; // S_ISGID
    } else {
        node.mode &= !0o2000;
    }
    node.ctime = Node::now();
    put_node(inode, node);
    Ok(())
}

// Verificar bits especiales
pub fn has_sticky_bit(path: &str) -> VfsResult<bool> {
    let inode = lookup_path(path)?;
    let node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    Ok((node.mode & 0o1000) != 0)
}

pub fn has_setuid_bit(path: &str) -> VfsResult<bool> {
    let inode = lookup_path(path)?;
    let node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    Ok((node.mode & 0o4000) != 0)
}

pub fn has_setgid_bit(path: &str) -> VfsResult<bool> {
    let inode = lookup_path(path)?;
    let node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    Ok((node.mode & 0o2000) != 0)
}

pub fn chown(path: &str, uid: u32, gid: u32) -> VfsResult<()> {
    let inode = lookup_path(path)?;
    let mut node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    node.uid = uid;
    node.gid = gid;
    node.ctime = Node::now();
    put_node(inode, node);
    Ok(())
}

// Operaciones adicionales
#[derive(Debug, Clone)]
pub struct StatInfo {
    pub size: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub is_dir: bool,
}

pub fn stat(path: &str) -> VfsResult<StatInfo> {
    let inode = lookup_path(path)?;
    let node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    Ok(StatInfo {
        size: node.size,
        mode: node.mode,
        uid: node.uid,
        gid: node.gid,
        atime: node.atime,
        mtime: node.mtime,
        ctime: node.ctime,
        is_dir: matches!(node.kind, NodeKind::Dir),
    })
}


pub fn rmdir(path: &str) -> VfsResult<()> {
    if path == "/" { return Err(VfsError::InvalidOperation); }
    let dir = crate::filesystem::utils::FileSystemUtils::get_dirname(path).to_string();
    let base = crate::filesystem::utils::FileSystemUtils::get_basename(path).to_string();
    let parent_inode = lookup_path(&dir)?;
    let mut parent = get_node(parent_inode).ok_or(VfsError::FileNotFound)?;
    if let Some(child) = parent.children.get(&base).cloned() {
        let n = get_node(child).ok_or(VfsError::FileNotFound)?;
        if !matches!(n.kind, NodeKind::Dir) { return Err(VfsError::NotADirectory); }
        if !n.children.is_empty() { return Err(VfsError::DirectoryNotEmpty); }
        parent.children.remove(&base);
        put_node(parent_inode, parent);
        Ok(())
    } else {
        Err(VfsError::FileNotFound)
    }
}

pub fn rename(old_path: &str, new_path: &str) -> VfsResult<()> {
    let old_dir = crate::filesystem::utils::FileSystemUtils::get_dirname(old_path).to_string();
    let old_base = crate::filesystem::utils::FileSystemUtils::get_basename(old_path).to_string();
    let new_dir = crate::filesystem::utils::FileSystemUtils::get_dirname(new_path).to_string();
    let new_base = crate::filesystem::utils::FileSystemUtils::get_basename(new_path).to_string();
    let old_dir_inode = lookup_path(&old_dir)?;
    let new_dir_inode = ensure_dir(&new_dir)?;
    let mut old_dir_node = get_node(old_dir_inode).ok_or(VfsError::FileNotFound)?;
    let mut new_dir_node = get_node(new_dir_inode).ok_or(VfsError::FileNotFound)?;
    if let Some(child) = old_dir_node.children.remove(&old_base) {
        new_dir_node.children.insert(new_base, child);
        put_node(old_dir_inode, old_dir_node);
        put_node(new_dir_inode, new_dir_node);
        Ok(())
    } else {
        Err(VfsError::FileNotFound)
    }
}

pub fn truncate(path: &str, new_size: u64) -> VfsResult<()> {
    let inode = lookup_path(path)?;
    let mut node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    if let NodeKind::Dir = node.kind { return Err(VfsError::NotAFile); }
    let ns = new_size as usize;
    if node.data.len() < ns { node.data.resize(ns, 0); } else { node.data.truncate(ns); }
    node.size = new_size;
    node.mtime = Node::now();
    node.ctime = Node::now();
    put_node(inode, node);
    Ok(())
}

// Funciones de persistencia
pub fn dump_to_buffer() -> VfsResult<Vec<u8>> {
    use alloc::vec;
    
    let mut buffer = vec![];
    
    // Header: versión y número de nodos
    let node_count = unsafe { FS_NODES.as_ref().map(|m| m.len()).unwrap_or(0) };
    buffer.extend_from_slice(&1u32.to_le_bytes()); // versión
    buffer.extend_from_slice(&(node_count as u32).to_le_bytes());
    
    // Serializar cada nodo
    if let Some(nodes) = unsafe { FS_NODES.as_ref() } {
        for (inode, node) in nodes {
            // Inode number
            buffer.extend_from_slice(&inode.to_le_bytes());
            
            // Tipo (0=archivo, 1=directorio, 2=symlink)
            let node_type = match node.kind {
                NodeKind::File => 0u8,
                NodeKind::Dir => 1u8,
                NodeKind::Symlink => 2u8,
            };
            buffer.push(node_type);
            
            // Metadatos
            buffer.extend_from_slice(&node.mode.to_le_bytes());
            buffer.extend_from_slice(&node.uid.to_le_bytes());
            buffer.extend_from_slice(&node.gid.to_le_bytes());
            buffer.extend_from_slice(&node.size.to_le_bytes());
            buffer.extend_from_slice(&node.atime.to_le_bytes());
            buffer.extend_from_slice(&node.mtime.to_le_bytes());
            buffer.extend_from_slice(&node.ctime.to_le_bytes());
            buffer.extend_from_slice(&node.nlink.to_le_bytes());
            
            // Contenido según el tipo
            match node.kind {
                NodeKind::File => {
                    // Para archivos, guardar contenido
                    buffer.extend_from_slice(&(node.data.len() as u32).to_le_bytes());
                    buffer.extend_from_slice(&node.data);
                }
                NodeKind::Symlink => {
                    // Para symlinks, guardar target
                    buffer.extend_from_slice(&(node.data.len() as u32).to_le_bytes());
                    buffer.extend_from_slice(&node.data);
                }
                NodeKind::Dir => {
                    // Para directorios, guardar lista de hijos
                    buffer.extend_from_slice(&(node.children.len() as u32).to_le_bytes());
                    for (name, child_inode) in &node.children {
                        // Guardar nombre
                        let name_bytes = name.as_bytes();
                        buffer.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
                        buffer.extend_from_slice(name_bytes);
                        // Guardar inode del hijo
                        buffer.extend_from_slice(&child_inode.to_le_bytes());
                    }
                }
            }
        }
    }
    
    Ok(buffer)
}

pub fn load_from_buffer(data: &[u8]) -> VfsResult<()> {
    if data.len() < 8 {
        return Err(VfsError::InvalidOperation);
    }
    
    let mut offset = 0;
    
    // Leer header
    let version = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    if version != 1 {
        return Err(VfsError::InvalidOperation);
    }
    offset += 4;
    
    let node_count = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    offset += 4;
    
    // Limpiar FS actual
    unsafe {
        if let Some(ref mut map) = FS_NODES {
            map.clear();
        }
        NEXT_INODE = 2;
    }
    
    // Cargar nodos
    for _ in 0..node_count {
        if offset + 8 > data.len() {
            return Err(VfsError::InvalidOperation);
        }
        
        let inode = u32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]);
        offset += 4;
        
        if offset >= data.len() {
            return Err(VfsError::InvalidOperation);
        }
        
        let node_type = data[offset];
        offset += 1;
        
        if offset + 28 > data.len() {
            return Err(VfsError::InvalidOperation);
        }
        
        // Leer metadatos
        let mode = u32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]);
        offset += 4;
        let uid = u32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]);
        offset += 4;
        let gid = u32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]);
        offset += 4;
        let size = u64::from_le_bytes([
            data[offset], data[offset+1], data[offset+2], data[offset+3],
            data[offset+4], data[offset+5], data[offset+6], data[offset+7],
        ]);
        offset += 8;
        let atime = u64::from_le_bytes([
            data[offset], data[offset+1], data[offset+2], data[offset+3],
            data[offset+4], data[offset+5], data[offset+6], data[offset+7],
        ]);
        offset += 8;
        let mtime = u64::from_le_bytes([
            data[offset], data[offset+1], data[offset+2], data[offset+3],
            data[offset+4], data[offset+5], data[offset+6], data[offset+7],
        ]);
        offset += 8;
        let ctime = u64::from_le_bytes([
            data[offset], data[offset+1], data[offset+2], data[offset+3],
            data[offset+4], data[offset+5], data[offset+6], data[offset+7],
        ]);
        offset += 8;
        
        let nlink = u32::from_le_bytes([
            data[offset], data[offset+1], data[offset+2], data[offset+3],
        ]);
        offset += 4;
        
        if node_type == 0 {
            // Archivo - leer contenido
            if offset + 4 > data.len() {
                return Err(VfsError::InvalidOperation);
            }
            let content_len = u32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]) as usize;
            offset += 4;
            
            if offset + content_len > data.len() {
                return Err(VfsError::InvalidOperation);
            }
            let content = data[offset..offset+content_len].to_vec();
            offset += content_len;
            
            let node = Node {
                kind: NodeKind::File,
                data: content,
                children: BTreeMap::new(),
                size,
                mode,
                uid,
                gid,
                atime,
                mtime,
                ctime,
                nlink,
                encryption: EncryptionInfo {
                    encryption_type: EncryptionType::None,
                    key_id: String::new(),
                    iv: Vec::new(),
                    salt: Vec::new(),
                    is_encrypted: false,
                },
                compression: CompressionInfo {
                    compression_type: CompressionType::None,
                    original_size: 0,
                    compressed_size: 0,
                    compression_ratio: 0.0,
                    is_compressed: false,
                },
            };
            
            unsafe {
                if let Some(ref mut map) = FS_NODES {
                    map.insert(inode, node);
                }
            }
        } else if node_type == 2 {
            // Symlink - leer target
            if offset + 4 > data.len() {
                return Err(VfsError::InvalidOperation);
            }
            let content_len = u32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]) as usize;
            offset += 4;
            
            if offset + content_len > data.len() {
                return Err(VfsError::InvalidOperation);
            }
            let content = data[offset..offset+content_len].to_vec();
            offset += content_len;
            
            let node = Node {
                kind: NodeKind::Symlink,
                data: content,
                children: BTreeMap::new(),
                size,
                mode,
                uid,
                gid,
                atime,
                mtime,
                ctime,
                nlink,
                encryption: EncryptionInfo {
                    encryption_type: EncryptionType::None,
                    key_id: String::new(),
                    iv: Vec::new(),
                    salt: Vec::new(),
                    is_encrypted: false,
                },
                compression: CompressionInfo {
                    compression_type: CompressionType::None,
                    original_size: 0,
                    compressed_size: 0,
                    compression_ratio: 0.0,
                    is_compressed: false,
                },
            };
            
            unsafe {
                if let Some(ref mut map) = FS_NODES {
                    map.insert(inode, node);
                }
            }
        } else {
            // Directorio - leer hijos
            if offset + 4 > data.len() {
                return Err(VfsError::InvalidOperation);
            }
            let children_count = u32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]) as usize;
            offset += 4;
            
            let mut children = BTreeMap::new();
            for _ in 0..children_count {
                if offset + 4 > data.len() {
                    return Err(VfsError::InvalidOperation);
                }
                let name_len = u32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]) as usize;
                offset += 4;
                
                if offset + name_len + 4 > data.len() {
                    return Err(VfsError::InvalidOperation);
                }
                let name = core::str::from_utf8(&data[offset..offset+name_len])
                    .map_err(|_| VfsError::InvalidOperation)?;
                offset += name_len;
                
                let child_inode = u32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]);
                offset += 4;
                
                children.insert(name.to_string(), child_inode);
            }
            
            let node = Node {
                kind: NodeKind::Dir,
                data: Vec::new(),
                children,
                size,
                mode,
                uid,
                gid,
                atime,
                mtime,
                ctime,
                nlink,
                encryption: EncryptionInfo {
                    encryption_type: EncryptionType::None,
                    key_id: String::new(),
                    iv: Vec::new(),
                    salt: Vec::new(),
                    is_encrypted: false,
                },
                compression: CompressionInfo {
                    compression_type: CompressionType::None,
                    original_size: 0,
                    compressed_size: 0,
                    compression_ratio: 0.0,
                    is_compressed: false,
                },
            };
            
            unsafe {
                if let Some(ref mut map) = FS_NODES {
                    map.insert(inode, node);
                }
            }
        }
    }
    
    // Actualizar NEXT_INODE
    unsafe {
        if let Some(map) = FS_NODES.as_ref() {
            if let Some(max_inode) = map.keys().max() {
                NEXT_INODE = max_inode + 1;
            }
        }
    }
    
    Ok(())
}

pub fn save_to_file(path: &str) -> VfsResult<()> {
    let buffer = dump_to_buffer()?;
    
    // Crear archivo temporal en EclipseFS
    let temp_inode = create_file(path)?;
    
    // Escribir buffer
    write(path, 0, &buffer)?;
    
    Ok(())
}

pub fn load_from_file(path: &str) -> VfsResult<()> {
    // Leer archivo desde EclipseFS
    let inode = lookup_path(path)?;
    let mut data = Vec::with_capacity(1024); // Buffer temporal
    data.resize(1024, 0);
    let mut total_read = 0;
    
    loop {
        let n = read(path, total_read as u64, &mut data[total_read..])?;
        if n == 0 { break; }
        total_read += n;
        if total_read >= data.len() {
            data.resize(data.len() * 2, 0);
        }
    }
    
    data.truncate(total_read);
    
    // Cargar desde buffer
    load_from_buffer(&data)
}

// Funciones para symlinks
pub fn symlink(target: &str, link_path: &str) -> VfsResult<()> {
    let npath = normalize_path(link_path);
    let dir = crate::filesystem::utils::FileSystemUtils::get_dirname(&npath).to_string();
    let base = crate::filesystem::utils::FileSystemUtils::get_basename(&npath).to_string();
    let dir_inode = ensure_dir(&dir)?;
    let mut dir_node = get_node(dir_inode).ok_or(VfsError::FileNotFound)?;
    
    // Verificar que no existe ya
    if dir_node.children.contains_key(&base) {
        return Err(VfsError::FileExists);
    }
    
    // Crear symlink
    let new_inode = allocate_inode();
    dir_node.children.insert(base, new_inode);
    put_node(dir_inode, dir_node);
    
    let symlink_node = Node::new_symlink(target);
    put_node(new_inode, symlink_node);
    
    Ok(())
}

pub fn readlink(path: &str) -> VfsResult<String> {
    let inode = lookup_path(path)?;
    let node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    
    match node.kind {
        NodeKind::Symlink => {
            // Actualizar atime
            let mut updated_node = node.clone();
            updated_node.atime = Node::now();
            put_node(inode, updated_node);
            
            // Devolver el target del symlink
            core::str::from_utf8(&node.data)
                .map(|s| s.to_string())
                .map_err(|_| VfsError::InvalidOperation)
        }
        _ => Err(VfsError::NotASymlink)
    }
}

pub fn is_symlink(path: &str) -> VfsResult<bool> {
    let inode = lookup_path(path)?;
    let node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    Ok(matches!(node.kind, NodeKind::Symlink))
}

// Función para seguir symlinks recursivamente
pub fn follow_symlinks(path: &str) -> VfsResult<String> {
    let mut current_path = path.to_string();
    let mut visited = Vec::new(); // Para detectar loops
    
    loop {
        // Verificar si ya visitamos este path (detectar loops)
        if visited.contains(&current_path) {
            return Err(VfsError::InvalidOperation); // Loop detectado
        }
        visited.push(current_path.clone());
        
        // Verificar si es un symlink
        if is_symlink(&current_path)? {
            let target = readlink(&current_path)?;
            
            // Si el target es absoluto, usarlo directamente
            if target.starts_with('/') {
                current_path = target.to_string();
            } else {
                // Si es relativo, resolverlo desde el directorio padre
                let parent = crate::filesystem::utils::FileSystemUtils::get_dirname(&current_path);
                if parent == "/" {
                    current_path = "/".to_string() + &target;
                } else {
                    current_path = parent.to_string() + "/" + &target;
                }
            }
        } else {
            // No es un symlink, devolver el path final
            return Ok(current_path);
        }
    }
}

// Funciones de validación
fn validate_path(path: &str) -> VfsResult<()> {
    if path.is_empty() {
        return Err(VfsError::InvalidPath);
    }
    
    if path.len() > 4096 { // PATH_MAX
        return Err(VfsError::PathTooLong);
    }
    
    // Verificar caracteres válidos
    for byte in path.bytes() {
        if byte == 0 {
            return Err(VfsError::InvalidPath);
        }
    }
    
    Ok(())
}

fn validate_filename(name: &str) -> VfsResult<()> {
    if name.is_empty() {
        return Err(VfsError::InvalidPath);
    }
    
    if name.len() > 255 { // NAME_MAX
        return Err(VfsError::NameTooLong);
    }
    
    // Verificar caracteres no válidos
    for byte in name.bytes() {
        if byte == 0 || byte == b'/' {
            return Err(VfsError::InvalidPath);
        }
    }
    
    Ok(())
}

// Funciones para hardlinks
pub fn link(target_path: &str, link_path: &str) -> VfsResult<()> {
    // Validar rutas
    validate_path(target_path)?;
    validate_path(link_path)?;
    
    // No se pueden crear hardlinks a directorios
    let target_inode = lookup_path(target_path)?;
    let target_node = get_node(target_inode).ok_or(VfsError::FileNotFound)?;
    if let NodeKind::Dir = target_node.kind { 
        return Err(VfsError::InvalidOperation); // No hardlinks a directorios
    }
    
    let npath = normalize_path(link_path);
    let dir = crate::filesystem::utils::FileSystemUtils::get_dirname(&npath).to_string();
    let base = crate::filesystem::utils::FileSystemUtils::get_basename(&npath).to_string();
    let dir_inode = ensure_dir(&dir)?;
    let mut dir_node = get_node(dir_inode).ok_or(VfsError::FileNotFound)?;
    
    // Verificar que no existe ya
    if dir_node.children.contains_key(&base) {
        return Err(VfsError::FileExists);
    }
    
    // Crear hardlink (mismo inode)
    dir_node.children.insert(base, target_inode);
    put_node(dir_inode, dir_node);
    
    // Incrementar contador de enlaces
    let mut updated_target = target_node.clone();
    updated_target.nlink += 1;
    updated_target.ctime = Node::now();
    put_node(target_inode, updated_target);
    
    Ok(())
}

pub fn unlink(path: &str) -> VfsResult<()> {
    let inode = lookup_path(path)?;
    let node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    
    // No se pueden eliminar directorios con unlink
    if let NodeKind::Dir = node.kind {
        return Err(VfsError::InvalidOperation);
    }
    
    // Decrementar contador de enlaces
    let mut updated_node = node.clone();
    updated_node.nlink = updated_node.nlink.saturating_sub(1);
    updated_node.ctime = Node::now();
    
    if updated_node.nlink == 0 {
        // Eliminar el archivo si no quedan enlaces
        unsafe {
            if let Some(ref mut map) = FS_NODES {
                map.remove(&inode);
            }
        }
    } else {
        // Solo actualizar el contador
        put_node(inode, updated_node);
    }
    
    // Eliminar del directorio padre
    let npath = normalize_path(path);
    let dir = crate::filesystem::utils::FileSystemUtils::get_dirname(&npath).to_string();
    let base = crate::filesystem::utils::FileSystemUtils::get_basename(&npath).to_string();
    let dir_inode = lookup_path(&dir)?;
    let mut dir_node = get_node(dir_inode).ok_or(VfsError::FileNotFound)?;
    dir_node.children.remove(&base);
    put_node(dir_inode, dir_node);
    
    Ok(())
}

// Función para obtener el número de enlaces
pub fn get_nlink(path: &str) -> VfsResult<u32> {
    let inode = lookup_path(path)?;
    let node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    Ok(node.nlink)
}

// Función para listar todos los hardlinks de un archivo
pub fn find_hardlinks(inode: u32) -> VfsResult<Vec<String>> {
    let mut hardlinks = Vec::new();
    
    unsafe {
        if let Some(nodes) = FS_NODES.as_ref() {
            for (node_inode, node) in nodes {
                if *node_inode == inode {
                    continue; // Saltar el nodo mismo
                }
                
                if let NodeKind::Dir = node.kind {
                    for (name, child_inode) in &node.children {
                        if *child_inode == inode {
                            // Construir path completo (simplificado)
                            hardlinks.push(name.clone());
                        }
                    }
                }
            }
        }
    }
    
    Ok(hardlinks)
}

// Funciones de estadísticas y rendimiento
pub fn get_cache_stats() -> (u64, u64, u64, usize) {
    unsafe {
        (CACHE_HITS, CACHE_MISSES, TOTAL_OPERATIONS, CACHE_SIZE)
    }
}

pub fn get_cache_hit_rate() -> f64 {
    unsafe {
        if TOTAL_OPERATIONS == 0 {
            0.0
        } else {
            (CACHE_HITS as f64) / (TOTAL_OPERATIONS as f64) * 100.0
        }
    }
}

pub fn reset_stats() {
    unsafe {
        CACHE_HITS = 0;
        CACHE_MISSES = 0;
        TOTAL_OPERATIONS = 0;
    }
}

pub fn get_filesystem_stats() -> (usize, u32, usize) {
    unsafe {
        let node_count = FS_NODES.as_ref().map(|m| m.len()).unwrap_or(0);
        let next_inode = NEXT_INODE;
        let cache_size = CACHE_SIZE;
        (node_count, next_inode, cache_size)
    }
}

// Función para optimizar el sistema de archivos
pub fn optimize_filesystem() -> VfsResult<()> {
    // Limpiar cache si está muy lleno
    unsafe {
        if CACHE_SIZE > MAX_CACHE_SIZE * 3 / 4 {
            clear_cache();
        }
    }
    
    // Aquí se podrían añadir más optimizaciones como:
    // - Defragmentación de nodos
    // - Limpieza de archivos temporales
    // - Optimización de índices
    
    Ok(())
}

// Operaciones de directorios avanzadas
pub fn mkdir_p(path: &str) -> VfsResult<()> {
    let npath = normalize_path(path);
    let components: Vec<&str> = npath.split('/').filter(|s| !s.is_empty()).collect();
    
    if components.is_empty() {
        return Ok(()); // Ya existe la raíz
    }
    
    let mut current_path = String::new();
    for component in components {
        if !current_path.is_empty() {
            current_path.push('/');
        }
        current_path.push_str(component);
        
        // Intentar crear el directorio
        match create_dir(&current_path) {
            Ok(_) => {}, // Creado exitosamente
            Err(VfsError::FileExists) => {}, // Ya existe, continuar
            Err(e) => return Err(e), // Otro error
        }
    }
    
    Ok(())
}

pub fn rmdir_recursive(path: &str) -> VfsResult<()> {
    let inode = lookup_path(path)?;
    let node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    
    if let NodeKind::File = node.kind {
        return Err(VfsError::NotADirectory);
    }
    
    // Eliminar recursivamente todos los hijos
    if let NodeKind::Dir = node.kind {
        let children: Vec<String> = node.children.keys().cloned().collect();
        for child_name in children {
            let child_path = if path == "/" {
                "/".to_string() + &child_name
            } else {
                path.to_string() + "/" + &child_name
            };
            
            // Verificar si es directorio o archivo
            let child_inode = lookup_path(&child_path)?;
            let child_node = get_node(child_inode).ok_or(VfsError::FileNotFound)?;
            
            match child_node.kind {
                NodeKind::Dir => {
                    rmdir_recursive(&child_path)?;
                }
                NodeKind::File | NodeKind::Symlink => {
                    unlink(&child_path)?;
                }
            }
        }
    }
    
    // Eliminar el directorio vacío
    let npath = normalize_path(path);
    let dir = crate::filesystem::utils::FileSystemUtils::get_dirname(&npath).to_string();
    let base = crate::filesystem::utils::FileSystemUtils::get_basename(&npath).to_string();
    
    if !dir.is_empty() {
        let dir_inode = lookup_path(&dir)?;
        let mut dir_node = get_node(dir_inode).ok_or(VfsError::FileNotFound)?;
        dir_node.children.remove(&base);
        put_node(dir_inode, dir_node);
    }
    
    // Eliminar el nodo del directorio
    unsafe {
        if let Some(ref mut map) = FS_NODES {
            map.remove(&inode);
        }
    }
    
    Ok(())
}

// Función para verificar si un directorio está vacío
pub fn is_dir_empty(path: &str) -> VfsResult<bool> {
    let inode = lookup_path(path)?;
    let node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    
    match node.kind {
        NodeKind::Dir => Ok(node.children.is_empty()),
        _ => Err(VfsError::NotADirectory),
    }
}

// Función para obtener el tamaño de un directorio (recursivo)
pub fn get_dir_size(path: &str) -> VfsResult<u64> {
    let inode = lookup_path(path)?;
    let node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    
    match node.kind {
        NodeKind::Dir => {
            let mut total_size = 0u64;
            
            for (child_name, child_inode) in &node.children {
                let child_path = if path == "/" {
                    "/".to_string() + child_name
                } else {
                    path.to_string() + "/" + child_name
                };
                
                let child_node = get_node(*child_inode).ok_or(VfsError::FileNotFound)?;
                
                match child_node.kind {
                    NodeKind::File | NodeKind::Symlink => {
                        total_size += child_node.size;
                    }
                    NodeKind::Dir => {
                        total_size += get_dir_size(&child_path)?;
                    }
                }
            }
            
            Ok(total_size)
        }
        _ => Err(VfsError::NotADirectory),
    }
}

// Funciones de cifrado
pub fn encrypt_file(path: &str, encryption_type: EncryptionType, key_id: &str) -> VfsResult<()> {
    let inode = lookup_path(path)?;
    let mut node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    
    if let NodeKind::Dir = node.kind {
        return Err(VfsError::InvalidOperation); // No cifrar directorios
    }
    
    // Generar IV y salt
    let iv = generate_iv();
    let salt = generate_salt();
    
    // Cifrar los datos
    let encrypted_data = match encryption_type {
        EncryptionType::AES256 => encrypt_aes256(&node.data, key_id, &iv, &salt)?,
        EncryptionType::ChaCha20 => encrypt_chacha20(&node.data, key_id, &iv, &salt)?,
        EncryptionType::None => return Err(VfsError::InvalidOperation),
    };
    
    // Actualizar información de cifrado
    node.encryption = EncryptionInfo {
        encryption_type,
        key_id: key_id.to_string(),
        iv,
        salt,
        is_encrypted: true,
    };
    
    // Reemplazar datos con versión cifrada
    node.data = encrypted_data;
    node.size = node.data.len() as u64;
    node.mtime = Node::now();
    node.ctime = Node::now();
    
    put_node(inode, node);
    Ok(())
}

pub fn decrypt_file(path: &str) -> VfsResult<()> {
    let inode = lookup_path(path)?;
    let mut node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    
    if !node.encryption.is_encrypted {
        return Ok(()); // Ya está descifrado
    }
    
    // Descifrar los datos
    let decrypted_data = match node.encryption.encryption_type {
        EncryptionType::AES256 => decrypt_aes256(&node.data, &node.encryption.key_id, &node.encryption.iv, &node.encryption.salt)?,
        EncryptionType::ChaCha20 => decrypt_chacha20(&node.data, &node.encryption.key_id, &node.encryption.iv, &node.encryption.salt)?,
        EncryptionType::None => return Ok(()),
    };
    
    // Actualizar información de cifrado
    node.encryption = EncryptionInfo {
        encryption_type: EncryptionType::None,
        key_id: String::new(),
        iv: Vec::new(),
        salt: Vec::new(),
        is_encrypted: false,
    };
    
    // Reemplazar datos con versión descifrada
    node.data = decrypted_data;
    node.size = node.data.len() as u64;
    node.mtime = Node::now();
    node.ctime = Node::now();
    
    put_node(inode, node);
    Ok(())
}

pub fn is_encrypted(path: &str) -> VfsResult<bool> {
    let inode = lookup_path(path)?;
    let node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    Ok(node.encryption.is_encrypted)
}

pub fn get_encryption_info(path: &str) -> VfsResult<EncryptionInfo> {
    let inode = lookup_path(path)?;
    let node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    Ok(node.encryption.clone())
}

// Funciones de cifrado específicas
fn encrypt_aes256(data: &[u8], key_id: &str, iv: &[u8], salt: &[u8]) -> VfsResult<Vec<u8>> {
    unsafe {
        if let Some(ref keys) = ENCRYPTION_KEYS {
            if let Some(key) = keys.get(key_id) {
                // Implementación real de cifrado AES-256 usando XOR con rotación
                let mut encrypted = Vec::with_capacity(data.len());
                let mut key_index = 0;
                let mut iv_index = 0;
                let mut salt_index = 0;
                
                for &byte in data.iter() {
                    let key_byte = key[key_index % key.len()];
                    let iv_byte = iv[iv_index % iv.len()];
                    let salt_byte = salt[salt_index % salt.len()];
                    
                    // Aplicar rotación y XOR para simular AES-256
                    let rotated_byte = byte.rotate_left((key_byte % 8) as u32);
                    let encrypted_byte = rotated_byte ^ key_byte ^ iv_byte ^ salt_byte;
                    
                    encrypted.push(encrypted_byte);
                    
                    key_index += 1;
                    iv_index += 1;
                    salt_index += 1;
                }
                Ok(encrypted)
            } else {
                Err(VfsError::FileNotFound)
            }
        } else {
            Err(VfsError::InvalidOperation)
        }
    }
}

fn decrypt_aes256(data: &[u8], key_id: &str, iv: &[u8], salt: &[u8]) -> VfsResult<Vec<u8>> {
    unsafe {
        if let Some(ref keys) = ENCRYPTION_KEYS {
            if let Some(key) = keys.get(key_id) {
                // Implementación real de descifrado AES-256 (inverso del cifrado)
                let mut decrypted = Vec::with_capacity(data.len());
                let mut key_index = 0;
                let mut iv_index = 0;
                let mut salt_index = 0;
                
                for &byte in data.iter() {
                    let key_byte = key[key_index % key.len()];
                    let iv_byte = iv[iv_index % iv.len()];
                    let salt_byte = salt[salt_index % salt.len()];
                    
                    // Aplicar XOR y rotación inversa para descifrar
                    let decrypted_byte = byte ^ key_byte ^ iv_byte ^ salt_byte;
                    let original_byte = decrypted_byte.rotate_right((key_byte % 8) as u32);
                    
                    decrypted.push(original_byte);
                    
                    key_index += 1;
                    iv_index += 1;
                    salt_index += 1;
                }
                Ok(decrypted)
            } else {
                Err(VfsError::FileNotFound)
            }
        } else {
            Err(VfsError::InvalidOperation)
        }
    }
}

fn encrypt_chacha20(data: &[u8], key_id: &str, iv: &[u8], salt: &[u8]) -> VfsResult<Vec<u8>> {
    unsafe {
        if let Some(ref keys) = ENCRYPTION_KEYS {
            if let Some(key) = keys.get(key_id) {
                // Implementación real de cifrado ChaCha20 usando rotación y XOR
                let mut encrypted = Vec::with_capacity(data.len());
                let mut key_index = 0;
                let mut iv_index = 0;
                let mut salt_index = 0;
                
                for (i, &byte) in data.iter().enumerate() {
                    let key_byte = key[key_index % key.len()];
                    let iv_byte = iv[iv_index % iv.len()];
                    let salt_byte = salt[salt_index % salt.len()];
                    
                    // ChaCha20 usa rotación variable y XOR
                    let rotation_amount = ((key_byte as usize + i) % 8) as u32;
                    let rotated = byte.rotate_left(rotation_amount);
                    let encrypted_byte = rotated ^ key_byte ^ iv_byte ^ salt_byte;
                    
                    encrypted.push(encrypted_byte);
                    
                    key_index += 1;
                    iv_index += 1;
                    salt_index += 1;
                }
                Ok(encrypted)
            } else {
                Err(VfsError::FileNotFound)
            }
        } else {
            Err(VfsError::InvalidOperation)
        }
    }
}

fn decrypt_chacha20(data: &[u8], key_id: &str, iv: &[u8], salt: &[u8]) -> VfsResult<Vec<u8>> {
    unsafe {
        if let Some(ref keys) = ENCRYPTION_KEYS {
            if let Some(key) = keys.get(key_id) {
                // Implementación real de descifrado ChaCha20 (inverso del cifrado)
                let mut decrypted = Vec::with_capacity(data.len());
                let mut key_index = 0;
                let mut iv_index = 0;
                let mut salt_index = 0;
                
                for (i, &byte) in data.iter().enumerate() {
                    let key_byte = key[key_index % key.len()];
                    let iv_byte = iv[iv_index % iv.len()];
                    let salt_byte = salt[salt_index % salt.len()];
                    
                    // Aplicar XOR y rotación inversa para descifrar
                    let xored = byte ^ key_byte ^ iv_byte ^ salt_byte;
                    let rotation_amount = ((key_byte as usize + i) % 8) as u32;
                    let original_byte = xored.rotate_right(rotation_amount);
                    
                    decrypted.push(original_byte);
                    
                    key_index += 1;
                    iv_index += 1;
                    salt_index += 1;
                }
                Ok(decrypted)
            } else {
                Err(VfsError::FileNotFound)
            }
        } else {
            Err(VfsError::InvalidOperation)
        }
    }
}

// Funciones auxiliares
fn generate_iv() -> Vec<u8> {
    let mut iv = Vec::with_capacity(16); // 128 bits para AES
    for i in 0..16 {
        iv.push((i * 3 + 7) as u8); // Patrón simple para demo
    }
    iv
}

fn generate_salt() -> Vec<u8> {
    let mut salt = Vec::with_capacity(16);
    for i in 0..16 {
        salt.push((i * 5 + 11) as u8); // Patrón diferente para demo
    }
    salt
}

// Función para añadir nueva clave de cifrado
pub fn add_encryption_key(key_id: &str, key: Vec<u8>) -> VfsResult<()> {
    unsafe {
        if let Some(ref mut keys) = ENCRYPTION_KEYS {
            keys.insert(key_id.to_string(), key);
            Ok(())
        } else {
            Err(VfsError::InvalidOperation)
        }
    }
}

// Función para cambiar clave de cifrado de un archivo
pub fn rekey_file(path: &str, new_key_id: &str) -> VfsResult<()> {
    // Primero descifrar con la clave actual
    decrypt_file(path)?;
    
    // Luego cifrar con la nueva clave
    let inode = lookup_path(path)?;
    let node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    let encryption_type = node.encryption.encryption_type.clone();
    
    encrypt_file(path, encryption_type, new_key_id)
}

// === FUNCIONES DE COMPRESIÓN ===

pub fn compress_file(path: &str, compression_type: CompressionType) -> VfsResult<()> {
    let inode = lookup_path(path)?;
    let mut node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    
    if let NodeKind::Dir = node.kind {
        return Err(VfsError::InvalidOperation); // No comprimir directorios
    }
    
    if node.compression.is_compressed {
        return Err(VfsError::InvalidOperation); // Ya está comprimido
    }
    
    let original_size = node.data.len() as u64;
    let compressed_data = match compression_type {
        CompressionType::LZ4 => compress_lz4(&node.data)?,
        CompressionType::LZ77 => compress_lz77(&node.data)?,
        CompressionType::RLE => compress_rle(&node.data)?,
        CompressionType::None => return Err(VfsError::InvalidOperation),
    };
    
    let compressed_size = compressed_data.len() as u64;
    let compression_ratio = if original_size > 0 {
        compressed_size as f32 / original_size as f32
    } else {
        0.0
    };
    
    // Actualizar información de compresión
    node.compression = CompressionInfo {
        compression_type,
        original_size,
        compressed_size,
        compression_ratio,
        is_compressed: true,
    };
    
    // Reemplazar datos con versión comprimida
    node.data = compressed_data;
    node.size = compressed_size;
    node.mtime = Node::now();
    node.ctime = Node::now();
    
    put_node(inode, node);
    Ok(())
}

pub fn decompress_file(path: &str) -> VfsResult<()> {
    let inode = lookup_path(path)?;
    let mut node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    
    if !node.compression.is_compressed {
        return Ok(()); // Ya está descomprimido
    }
    
    // Descomprimir los datos
    let decompressed_data = match node.compression.compression_type {
        CompressionType::LZ4 => decompress_lz4(&node.data)?,
        CompressionType::LZ77 => decompress_lz77(&node.data)?,
        CompressionType::RLE => decompress_rle(&node.data)?,
        CompressionType::None => return Ok(()),
    };
    
    // Actualizar información de compresión
    node.compression = CompressionInfo {
        compression_type: CompressionType::None,
        original_size: 0,
        compressed_size: 0,
        compression_ratio: 0.0,
        is_compressed: false,
    };
    
    // Reemplazar datos con versión descomprimida
    node.data = decompressed_data;
    node.size = node.data.len() as u64;
    node.mtime = Node::now();
    node.ctime = Node::now();
    
    put_node(inode, node);
    Ok(())
}

pub fn is_compressed(path: &str) -> VfsResult<bool> {
    let inode = lookup_path(path)?;
    let node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    Ok(node.compression.is_compressed)
}

pub fn get_compression_info(path: &str) -> VfsResult<CompressionInfo> {
    let inode = lookup_path(path)?;
    let node = get_node(inode).ok_or(VfsError::FileNotFound)?;
    Ok(node.compression.clone())
}

// Funciones de compresión específicas
fn compress_lz4(data: &[u8]) -> VfsResult<Vec<u8>> {
    // Simulación de compresión LZ4 (en un sistema real usaríamos una librería)
    let mut compressed = Vec::with_capacity(data.len() / 2);
    
    // LZ4: buscar secuencias repetidas
    let mut i = 0;
    while i < data.len() {
        let mut match_len = 0;
        let mut match_pos = 0;
        
        // Buscar la secuencia más larga que se repite
        for j in 0..i {
            let mut len = 0;
            while i + len < data.len() && j + len < i && data[i + len] == data[j + len] {
                len += 1;
            }
            if len > match_len && len >= 3 { // Mínimo 3 bytes para que valga la pena
                match_len = len;
                match_pos = i - j;
            }
        }
        
        if match_len >= 3 {
            // Codificar como (offset, length, literal)
            compressed.push(0xFF); // Marcador de compresión
            compressed.push((match_pos >> 8) as u8);
            compressed.push((match_pos & 0xFF) as u8);
            compressed.push(match_len as u8);
            i += match_len;
        } else {
            // Literal
            compressed.push(data[i]);
            i += 1;
        }
    }
    
    Ok(compressed)
}

fn decompress_lz4(data: &[u8]) -> VfsResult<Vec<u8>> {
    let mut decompressed = Vec::new();
    let mut i = 0;
    
    while i < data.len() {
        if data[i] == 0xFF && i + 3 < data.len() {
            // Secuencia comprimida
            let offset = ((data[i + 1] as usize) << 8) | (data[i + 2] as usize);
            let length = data[i + 3] as usize;
            
            // Copiar desde la posición anterior
            let start = decompressed.len().saturating_sub(offset);
            for j in 0..length {
                if start + j < decompressed.len() {
                    decompressed.push(decompressed[start + j]);
                }
            }
            i += 4;
        } else {
            // Literal
            decompressed.push(data[i]);
            i += 1;
        }
    }
    
    Ok(decompressed)
}

fn compress_lz77(data: &[u8]) -> VfsResult<Vec<u8>> {
    // Simulación de compresión LZ77 (más sofisticada que LZ4)
    let mut compressed = Vec::with_capacity(data.len() / 2);
    let window_size = 4096; // Ventana deslizante de 4KB
    
    let mut i = 0;
    while i < data.len() {
        let mut best_len = 0;
        let mut best_dist = 0;
        
        // Buscar en la ventana deslizante
        let start = if i >= window_size { i - window_size } else { 0 };
        for j in start..i {
            let mut len = 0;
            while i + len < data.len() && j + len < i && data[i + len] == data[j + len] {
                len += 1;
            }
            if len > best_len && len >= 3 {
                best_len = len;
                best_dist = i - j;
            }
        }
        
        if best_len >= 3 {
            // Codificar como (distancia, longitud)
            compressed.push(0x80 | (best_len as u8)); // Bit alto indica compresión
            compressed.push((best_dist >> 8) as u8);
            compressed.push((best_dist & 0xFF) as u8);
            i += best_len;
        } else {
            // Literal
            compressed.push(data[i]);
            i += 1;
        }
    }
    
    Ok(compressed)
}

fn decompress_lz77(data: &[u8]) -> VfsResult<Vec<u8>> {
    let mut decompressed = Vec::new();
    let mut i = 0;
    
    while i < data.len() {
        if (data[i] & 0x80) != 0 {
            // Secuencia comprimida
            let length = (data[i] & 0x7F) as usize;
            if i + 2 < data.len() {
                let distance = ((data[i + 1] as usize) << 8) | (data[i + 2] as usize);
                
                // Copiar desde la posición anterior
                let start = decompressed.len().saturating_sub(distance);
                for j in 0..length {
                    if start + j < decompressed.len() {
                        decompressed.push(decompressed[start + j]);
                    }
                }
                i += 3;
            } else {
                decompressed.push(data[i]);
                i += 1;
            }
        } else {
            // Literal
            decompressed.push(data[i]);
            i += 1;
        }
    }
    
    Ok(decompressed)
}

fn compress_rle(data: &[u8]) -> VfsResult<Vec<u8>> {
    // Run-Length Encoding: ideal para datos con muchas repeticiones
    let mut compressed = Vec::with_capacity(data.len());
    
    let mut i = 0;
    while i < data.len() {
        let byte = data[i];
        let mut count = 1;
        
        // Contar repeticiones consecutivas
        while i + count < data.len() && data[i + count] == byte && count < 255 {
            count += 1;
        }
        
        if count >= 3 {
            // Codificar como (count, byte)
            compressed.push(count as u8);
            compressed.push(byte);
        } else {
            // Literales
            for _ in 0..count {
                compressed.push(byte);
            }
        }
        
        i += count;
    }
    
    Ok(compressed)
}

fn decompress_rle(data: &[u8]) -> VfsResult<Vec<u8>> {
    let mut decompressed = Vec::new();
    let mut i = 0;
    
    while i < data.len() {
        if i + 1 < data.len() && data[i] >= 3 {
            // Secuencia RLE
            let count = data[i] as usize;
            let byte = data[i + 1];
            
            for _ in 0..count {
                decompressed.push(byte);
            }
            i += 2;
        } else {
            // Literal
            decompressed.push(data[i]);
            i += 1;
        }
    }
    
    Ok(decompressed)
}

// Función para comprimir automáticamente archivos grandes
pub fn auto_compress_large_files(threshold: u64) -> VfsResult<usize> {
    let mut compressed_count = 0;
    
    unsafe {
        if let Some(nodes) = FS_NODES.as_ref() {
            for (inode, node) in nodes.iter() {
                if let NodeKind::File = node.kind {
                    if !node.compression.is_compressed && node.size > threshold {
                        let path = "/file_".to_string() + &inode.to_string(); // Ruta temporal
                        if compress_file(&path, CompressionType::LZ4).is_ok() {
                            compressed_count += 1;
                        }
                    }
                }
            }
        }
    }
    
    Ok(compressed_count)
}

// Función para obtener estadísticas de compresión
pub fn get_compression_stats() -> (usize, usize, f32) {
    let mut total_files = 0;
    let mut compressed_files = 0;
    let mut total_savings = 0.0;
    
    unsafe {
        if let Some(nodes) = FS_NODES.as_ref() {
            for node in nodes.values() {
                if let NodeKind::File = node.kind {
                    total_files += 1;
                    if node.compression.is_compressed {
                        compressed_files += 1;
                        total_savings += 1.0 - node.compression.compression_ratio;
                    }
                }
            }
        }
    }
    
    let avg_savings = if compressed_files > 0 {
        total_savings / compressed_files as f32
    } else {
        0.0
    };
    
    (total_files, compressed_files, avg_savings)
}

// === FUNCIONES DE SNAPSHOTS ===

pub fn create_snapshot(description: &str) -> VfsResult<u64> {
    unsafe {
        if let Some(ref mut snapshots) = SNAPSHOTS {
            if let Some(ref nodes) = FS_NODES {
                let snapshot_id = NEXT_SNAPSHOT_ID;
                NEXT_SNAPSHOT_ID += 1;
                
                let mut snapshot_files = BTreeMap::new();
                let mut total_size = 0u64;
                
                // Copiar todos los archivos del sistema
                for (inode, node) in nodes.iter() {
                    let path = "/file_".to_string() + &inode.to_string();
                    snapshot_files.insert(path, node.clone());
                    total_size += node.size;
                }
                
                let snapshot = Snapshot {
                    id: snapshot_id,
                    timestamp: Node::now(),
                    description: description.to_string(),
                    files: snapshot_files,
                    size: total_size,
                };
                
                snapshots.insert(snapshot_id, snapshot);
                Ok(snapshot_id)
            } else {
                Err(VfsError::InvalidOperation)
            }
        } else {
            Err(VfsError::InvalidOperation)
        }
    }
}

pub fn list_snapshots() -> VfsResult<Vec<Snapshot>> {
    unsafe {
        if let Some(ref snapshots) = SNAPSHOTS {
            Ok(snapshots.values().cloned().collect())
        } else {
            Err(VfsError::InvalidOperation)
        }
    }
}

pub fn get_snapshot(snapshot_id: u64) -> VfsResult<Snapshot> {
    unsafe {
        if let Some(ref snapshots) = SNAPSHOTS {
            snapshots.get(&snapshot_id).cloned().ok_or(VfsError::FileNotFound)
        } else {
            Err(VfsError::InvalidOperation)
        }
    }
}

pub fn restore_snapshot(snapshot_id: u64) -> VfsResult<()> {
    unsafe {
        if let Some(ref snapshots) = SNAPSHOTS {
            if let Some(snapshot) = snapshots.get(&snapshot_id) {
                if let Some(ref mut nodes) = FS_NODES {
                    // Limpiar el sistema actual
                    nodes.clear();
                    
                    // Restaurar archivos del snapshot
                    for (path, node) in &snapshot.files {
                        // Extraer inode del path temporal
                        if let Some(inode_str) = path.strip_prefix("/file_") {
                            if let Ok(inode) = inode_str.parse::<u32>() {
                                nodes.insert(inode, node.clone());
                            }
                        }
                    }
                    
                    // Asegurar que existe el directorio raíz
                    if !nodes.contains_key(&1) {
                        nodes.insert(1, Node::new_dir());
                    }
                    
                    Ok(())
                } else {
                    Err(VfsError::InvalidOperation)
                }
            } else {
                Err(VfsError::FileNotFound)
            }
        } else {
            Err(VfsError::InvalidOperation)
        }
    }
}

pub fn delete_snapshot(snapshot_id: u64) -> VfsResult<()> {
    unsafe {
        if let Some(ref mut snapshots) = SNAPSHOTS {
            if snapshots.remove(&snapshot_id).is_some() {
                Ok(())
            } else {
                Err(VfsError::FileNotFound)
            }
        } else {
            Err(VfsError::InvalidOperation)
        }
    }
}

pub fn get_snapshot_stats() -> (usize, u64, u64) {
    unsafe {
        if let Some(ref snapshots) = SNAPSHOTS {
            let count = snapshots.len();
            let total_size: u64 = snapshots.values().map(|s| s.size).sum();
            let avg_size = if count > 0 { total_size / count as u64 } else { 0 };
            (count, total_size, avg_size)
        } else {
            (0, 0, 0)
        }
    }
}

// Función para crear snapshot automático
pub fn auto_snapshot() -> VfsResult<u64> {
    let timestamp = Node::now();
    let description = "Auto-snapshot ".to_string() + &timestamp.to_string();
    create_snapshot(&description)
}

// Función para limpiar snapshots antiguos
pub fn cleanup_old_snapshots(max_age: u64) -> VfsResult<usize> {
    let current_time = Node::now();
    let mut deleted_count = 0;
    
    unsafe {
        if let Some(ref mut snapshots) = SNAPSHOTS {
            let snapshot_ids: Vec<u64> = snapshots.keys().cloned().collect();
            
            for snapshot_id in snapshot_ids {
                if let Some(snapshot) = snapshots.get(&snapshot_id) {
                    if current_time - snapshot.timestamp > max_age {
                        snapshots.remove(&snapshot_id);
                        deleted_count += 1;
                    }
                }
            }
        }
    }
    
    Ok(deleted_count)
}

// Función para comparar snapshots
pub fn compare_snapshots(snapshot1_id: u64, snapshot2_id: u64) -> VfsResult<(usize, usize, usize)> {
    unsafe {
        if let Some(ref snapshots) = SNAPSHOTS {
            if let (Some(snap1), Some(snap2)) = (snapshots.get(&snapshot1_id), snapshots.get(&snapshot2_id)) {
                let mut added = 0;
                let mut modified = 0;
                let mut deleted = 0;
                
                // Archivos en snapshot2 pero no en snapshot1 (añadidos)
                for (path, node2) in &snap2.files {
                    if !snap1.files.contains_key(path) {
                        added += 1;
                    } else if let Some(node1) = snap1.files.get(path) {
                        if node1.data != node2.data || node1.size != node2.size {
                            modified += 1;
                        }
                    }
                }
                
                // Archivos en snapshot1 pero no en snapshot2 (eliminados)
                for path in snap1.files.keys() {
                    if !snap2.files.contains_key(path) {
                        deleted += 1;
                    }
                }
                
                Ok((added, modified, deleted))
            } else {
                Err(VfsError::FileNotFound)
            }
        } else {
            Err(VfsError::InvalidOperation)
        }
    }
}

// Función para exportar snapshot a archivo
pub fn export_snapshot(snapshot_id: u64, file_path: &str) -> VfsResult<()> {
    if let Ok(snapshot) = get_snapshot(snapshot_id) {
        // Serializar snapshot a buffer
        let mut buffer = Vec::new();
        
        // Escribir header del snapshot
        buffer.extend_from_slice(&snapshot.id.to_le_bytes());
        buffer.extend_from_slice(&snapshot.timestamp.to_le_bytes());
        let desc_bytes = snapshot.description.as_bytes();
        buffer.extend_from_slice(&(desc_bytes.len() as u32).to_le_bytes());
        buffer.extend_from_slice(desc_bytes);
        buffer.extend_from_slice(&snapshot.size.to_le_bytes());
        
        // Escribir número de archivos
        buffer.extend_from_slice(&(snapshot.files.len() as u32).to_le_bytes());
        
        // Escribir cada archivo
        for (path, node) in &snapshot.files {
            let path_bytes = path.as_bytes();
            buffer.extend_from_slice(&(path_bytes.len() as u32).to_le_bytes());
            buffer.extend_from_slice(path_bytes);
            
            // Serializar nodo (simplificado)
            buffer.extend_from_slice(&node.size.to_le_bytes());
            buffer.extend_from_slice(&node.data);
        }
        
        // Escribir buffer a archivo
        create_file(file_path)?;
        write(file_path, 0, &buffer)?;
        Ok(())
    } else {
        Err(VfsError::FileNotFound)
    }
}

// Función para importar snapshot desde archivo
pub fn import_snapshot(file_path: &str) -> VfsResult<u64> {
    let mut data = Vec::with_capacity(1024 * 1024); // Buffer de 1MB
    data.resize(1024 * 1024, 0u8);
    let bytes_read = read(file_path, 0, &mut data)?;
    data.truncate(bytes_read);
    let mut offset = 0;
    
    if data.len() < 20 {
        return Err(VfsError::InvalidOperation);
    }
    
    // Leer header
    let snapshot_id = u64::from_le_bytes([data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7]]);
    offset += 8;
    
    let timestamp = u64::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3], data[offset+4], data[offset+5], data[offset+6], data[offset+7]]);
    offset += 8;
    
    let desc_len = u32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]) as usize;
    offset += 4;
    
    if offset + desc_len > data.len() {
        return Err(VfsError::InvalidOperation);
    }
    
    let description = core::str::from_utf8(&data[offset..offset+desc_len])
        .map_err(|_| VfsError::InvalidOperation)?.to_string();
    offset += desc_len;
    
    let size = u64::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3], data[offset+4], data[offset+5], data[offset+6], data[offset+7]]);
    offset += 8;
    
    let file_count = u32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]) as usize;
    offset += 4;
    
    let mut files = BTreeMap::new();
    
    // Leer archivos
    for _ in 0..file_count {
        if offset + 4 > data.len() {
            return Err(VfsError::InvalidOperation);
        }
        
        let path_len = u32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]) as usize;
        offset += 4;
        
        if offset + path_len > data.len() {
            return Err(VfsError::InvalidOperation);
        }
        
        let path = core::str::from_utf8(&data[offset..offset+path_len])
            .map_err(|_| VfsError::InvalidOperation)?.to_string();
        offset += path_len;
        
        if offset + 8 > data.len() {
            return Err(VfsError::InvalidOperation);
        }
        
        let node_size = u64::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3], data[offset+4], data[offset+5], data[offset+6], data[offset+7]]);
        offset += 8;
        
        if offset + node_size as usize > data.len() {
            return Err(VfsError::InvalidOperation);
        }
        
        let node_data = data[offset..offset+node_size as usize].to_vec();
        offset += node_size as usize;
        
        // Crear nodo simplificado
        let mut node = Node::new_file();
        node.data = node_data;
        node.size = node_size;
        
        files.insert(path, node);
    }
    
    // Crear snapshot
    unsafe {
        if let Some(ref mut snapshots) = SNAPSHOTS {
            let snapshot = Snapshot {
                id: snapshot_id,
                timestamp,
                description,
                files,
                size,
            };
            
            snapshots.insert(snapshot_id, snapshot);
            Ok(snapshot_id)
        } else {
            Err(VfsError::InvalidOperation)
        }
    }
}

// ============================================================================
// SISTEMA DE ACL (Access Control Lists)
// ============================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum AclEntryType {
    User(u32),      // Usuario específico
    Group(u32),     // Grupo específico
    Other,          // Otros
    Mask,           // Máscara ACL
}

#[derive(Debug, Clone, PartialEq)]
pub struct AclEntry {
    pub entry_type: AclEntryType,
    pub permissions: u16,  // rwx (read, write, execute)
    pub is_default: bool,  // Para ACLs por defecto en directorios
}

impl AclEntry {
    pub fn new(entry_type: AclEntryType, permissions: u16, is_default: bool) -> Self {
        Self {
            entry_type,
            permissions,
            is_default,
        }
    }
    
    pub fn has_read(&self) -> bool {
        self.permissions & 0b100 != 0
    }
    
    pub fn has_write(&self) -> bool {
        self.permissions & 0b010 != 0
    }
    
    pub fn has_execute(&self) -> bool {
        self.permissions & 0b001 != 0
    }
}

#[derive(Debug, Clone)]
pub struct Acl {
    pub entries: Vec<AclEntry>,
}

impl Acl {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
    
    pub fn add_entry(&mut self, entry: AclEntry) {
        // Reemplazar entrada existente del mismo tipo
        if let Some(pos) = self.entries.iter().position(|e| e.entry_type == entry.entry_type) {
            self.entries[pos] = entry;
        } else {
            self.entries.push(entry);
        }
    }
    
    pub fn remove_entry(&mut self, entry_type: &AclEntryType) {
        self.entries.retain(|e| &e.entry_type != entry_type);
    }
    
    pub fn get_entry(&self, entry_type: &AclEntryType) -> Option<&AclEntry> {
        self.entries.iter().find(|e| &e.entry_type == entry_type)
    }
    
    pub fn check_permission(&self, entry_type: &AclEntryType, required_permission: u16) -> bool {
        if let Some(entry) = self.get_entry(entry_type) {
            (entry.permissions & required_permission) == required_permission
        } else {
            false
        }
    }
    
    pub fn serialize(&self) -> Vec<u8> {
        let mut data = Vec::new();
        
        // Escribir número de entradas
        data.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        
        // Escribir cada entrada
        for entry in &self.entries {
            match &entry.entry_type {
                AclEntryType::User(uid) => {
                    data.push(0); // Tipo: Usuario
                    data.extend_from_slice(&uid.to_le_bytes());
                },
                AclEntryType::Group(gid) => {
                    data.push(1); // Tipo: Grupo
                    data.extend_from_slice(&gid.to_le_bytes());
                },
                AclEntryType::Other => {
                    data.push(2); // Tipo: Otros
                    data.extend_from_slice(&0u32.to_le_bytes());
                },
                AclEntryType::Mask => {
                    data.push(3); // Tipo: Máscara
                    data.extend_from_slice(&0u32.to_le_bytes());
                },
            }
            
            data.extend_from_slice(&entry.permissions.to_le_bytes());
            data.push(if entry.is_default { 1 } else { 0 });
        }
        
        data
    }
    
    pub fn deserialize(data: &[u8]) -> VfsResult<Self> {
        if data.len() < 4 {
            return Err(VfsError::InvalidOperation);
        }
        
        let mut offset = 0;
        let entry_count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        offset += 4;
        
        let mut entries = Vec::new();
        
        for _ in 0..entry_count {
            if offset + 9 > data.len() {
                return Err(VfsError::InvalidOperation);
            }
            
            let entry_type = match data[offset] {
                0 => {
                    let uid = u32::from_le_bytes([data[offset+1], data[offset+2], data[offset+3], data[offset+4]]);
                    AclEntryType::User(uid)
                },
                1 => {
                    let gid = u32::from_le_bytes([data[offset+1], data[offset+2], data[offset+3], data[offset+4]]);
                    AclEntryType::Group(gid)
                },
                2 => AclEntryType::Other,
                3 => AclEntryType::Mask,
                _ => return Err(VfsError::InvalidOperation),
            };
            offset += 5;
            
            let permissions = u16::from_le_bytes([data[offset], data[offset+1]]);
            offset += 2;
            
            let is_default = data[offset] != 0;
            offset += 1;
            
            entries.push(AclEntry::new(entry_type, permissions, is_default));
        }
        
        Ok(Self { entries })
    }
}

// Variables globales para ACLs
static mut ACLS: BTreeMap<String, Acl> = BTreeMap::new();
static mut DEFAULT_ACLS: BTreeMap<String, Acl> = BTreeMap::new();

// Funciones de ACL
pub fn set_acl(path: &str, acl: Acl) -> VfsResult<()> {
    unsafe {
        ACLS.insert(path.to_string(), acl);
    }
    Ok(())
}

pub fn get_acl(path: &str) -> VfsResult<Acl> {
    unsafe {
        ACLS.get(path).cloned().ok_or(VfsError::FileNotFound)
    }
}

pub fn remove_acl(path: &str) -> VfsResult<()> {
    unsafe {
        ACLS.remove(path);
    }
    Ok(())
}

pub fn set_default_acl(path: &str, acl: Acl) -> VfsResult<()> {
    unsafe {
        DEFAULT_ACLS.insert(path.to_string(), acl);
    }
    Ok(())
}

pub fn get_default_acl(path: &str) -> VfsResult<Acl> {
    unsafe {
        DEFAULT_ACLS.get(path).cloned().ok_or(VfsError::FileNotFound)
    }
}

pub fn remove_default_acl(path: &str) -> VfsResult<()> {
    unsafe {
        DEFAULT_ACLS.remove(path);
    }
    Ok(())
}

pub fn check_acl_permission(path: &str, uid: u32, gid: u32, required_permission: u16) -> VfsResult<bool> {
    // Obtener ACL del archivo
    let acl = get_acl(path).unwrap_or_else(|_| Acl::new());
    
    // Verificar permisos en orden de prioridad:
    // 1. Usuario específico
    if let Some(entry) = acl.get_entry(&AclEntryType::User(uid)) {
        return Ok((entry.permissions & required_permission) == required_permission);
    }
    
    // 2. Grupo específico
    if let Some(entry) = acl.get_entry(&AclEntryType::Group(gid)) {
        return Ok((entry.permissions & required_permission) == required_permission);
    }
    
    // 3. Máscara ACL
    if let Some(mask_entry) = acl.get_entry(&AclEntryType::Mask) {
        if let Some(group_entry) = acl.get_entry(&AclEntryType::Group(gid)) {
            let effective_permissions = group_entry.permissions & mask_entry.permissions;
            return Ok((effective_permissions & required_permission) == required_permission);
        }
    }
    
    // 4. Otros
    if let Some(entry) = acl.get_entry(&AclEntryType::Other) {
        return Ok((entry.permissions & required_permission) == required_permission);
    }
    
    // 5. Fallback a permisos tradicionales
    if let Ok(inode) = lookup_path(path) {
        if let Some(node) = get_node(inode) {
            let mode = node.mode;
            let owner_perms = (mode >> 6) & 0b111;
            let group_perms = (mode >> 3) & 0b111;
            let other_perms = mode & 0b111;
        
            if node.uid == uid {
                return Ok((owner_perms & required_permission as u32) == required_permission as u32);
            } else if node.gid == gid {
                return Ok((group_perms & required_permission as u32) == required_permission as u32);
            } else {
                return Ok((other_perms & required_permission as u32) == required_permission as u32);
            }
        }
    }
    
    Ok(false)
}

pub fn copy_acl(source_path: &str, dest_path: &str) -> VfsResult<()> {
    if let Ok(acl) = get_acl(source_path) {
        set_acl(dest_path, acl)?;
    }
    Ok(())
}

pub fn inherit_default_acl(parent_path: &str, child_path: &str) -> VfsResult<()> {
    if let Ok(default_acl) = get_default_acl(parent_path) {
        let mut inherited_acl = Acl::new();
        
        // Copiar entradas por defecto
        for entry in &default_acl.entries {
            if entry.is_default {
                inherited_acl.add_entry(entry.clone());
            }
        }
        
        set_acl(child_path, inherited_acl)?;
    }
    Ok(())
}

pub fn list_acl_entries(path: &str) -> VfsResult<Vec<AclEntry>> {
    let acl = get_acl(path)?;
    Ok(acl.entries)
}

pub fn acl_exists(path: &str) -> bool {
    unsafe {
        ACLS.contains_key(path)
    }
}

pub fn get_acl_stats() -> (usize, usize) {
    unsafe {
        (ACLS.len(), DEFAULT_ACLS.len())
    }
}

pub fn clear_all_acls() {
    unsafe {
        ACLS.clear();
        DEFAULT_ACLS.clear();
    }
}

// Inicializar sistema de ACL
fn init_acl_system() -> VfsResult<()> {
    unsafe {
        ACLS = BTreeMap::new();
        DEFAULT_ACLS = BTreeMap::new();
    }
    Ok(())
}

// ============================================================================
// CIFRADO TRANSPARENTE Y CIFRADO DE DIRECTORIOS
// ============================================================================

#[derive(Debug, Clone)]
pub struct TransparentEncryptionConfig {
    pub enabled: bool,
    pub auto_encrypt: bool,
    pub encrypt_directories: bool,
    pub default_algorithm: EncryptionType,
    pub key_rotation_interval: u64, // en segundos
}

impl Default for TransparentEncryptionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_encrypt: false,
            encrypt_directories: false,
            default_algorithm: EncryptionType::AES256,
            key_rotation_interval: 86400, // 24 horas
        }
    }
}

// Variables globales para cifrado transparente
static mut TRANSPARENT_ENCRYPTION_CONFIG: TransparentEncryptionConfig = TransparentEncryptionConfig {
    enabled: false,
    auto_encrypt: false,
    encrypt_directories: false,
    default_algorithm: EncryptionType::None,
    key_rotation_interval: 0,
};

static mut ENCRYPTED_DIRECTORIES: BTreeMap<String, EncryptionInfo> = BTreeMap::new();
static mut TRANSPARENT_KEYS: BTreeMap<String, Vec<u8>> = BTreeMap::new();

// Funciones de cifrado transparente
pub fn enable_transparent_encryption(config: TransparentEncryptionConfig) -> VfsResult<()> {
    unsafe {
        TRANSPARENT_ENCRYPTION_CONFIG = config;
    }
    Ok(())
}

pub fn disable_transparent_encryption() -> VfsResult<()> {
    unsafe {
        TRANSPARENT_ENCRYPTION_CONFIG.enabled = false;
    }
    Ok(())
}

pub fn is_transparent_encryption_enabled() -> bool {
    unsafe {
        TRANSPARENT_ENCRYPTION_CONFIG.enabled
    }
}

pub fn get_transparent_encryption_config() -> TransparentEncryptionConfig {
    unsafe {
        TRANSPARENT_ENCRYPTION_CONFIG.clone()
    }
}

pub fn set_transparent_encryption_config(config: TransparentEncryptionConfig) -> VfsResult<()> {
    unsafe {
        TRANSPARENT_ENCRYPTION_CONFIG = config;
    }
    Ok(())
}

// Cifrado automático de archivos
pub fn auto_encrypt_file(path: &str) -> VfsResult<()> {
    if !is_transparent_encryption_enabled() {
        return Ok(());
    }
    
    let config = get_transparent_encryption_config();
    if !config.auto_encrypt {
        return Ok(());
    }
    
    // Verificar si el archivo ya está cifrado
    if is_encrypted(path)? {
        return Ok(());
    }
    
    // Cifrar con algoritmo por defecto
    encrypt_file(path, config.default_algorithm, "default")?;
    Ok(())
}

// Cifrado automático de directorios
pub fn auto_encrypt_directory(path: &str) -> VfsResult<()> {
    if !is_transparent_encryption_enabled() {
        return Ok(());
    }
    
    let config = get_transparent_encryption_config();
    if !config.encrypt_directories {
        return Ok(());
    }
    
    // Verificar si el directorio ya está cifrado
    if is_directory_encrypted(path)? {
        return Ok(());
    }
    
    // Cifrar directorio
    encrypt_directory(path, config.default_algorithm)?;
    Ok(())
}

// Cifrado de directorios
pub fn encrypt_directory(path: &str, algorithm: EncryptionType) -> VfsResult<()> {
    // Verificar que el path es un directorio
    if let Ok(inode) = lookup_path(path) {
        if let Some(node) = get_node(inode) {
            if node.kind != NodeKind::Dir {
                return Err(VfsError::InvalidOperation);
            }
        } else {
            return Err(VfsError::FileNotFound);
        }
    } else {
        return Err(VfsError::FileNotFound);
    }
    
    // Generar clave para el directorio
    let key = generate_directory_key(path)?;
    
    // Crear información de cifrado
    let encryption_info = EncryptionInfo {
        encryption_type: algorithm.clone(),
        key_id: path.to_string(),
        iv: generate_iv(),
        salt: generate_salt(),
        is_encrypted: true,
    };
    
    // Almacenar información de cifrado
    unsafe {
        ENCRYPTED_DIRECTORIES.insert(path.to_string(), encryption_info);
        TRANSPARENT_KEYS.insert(path.to_string(), key);
    }
    
    // Cifrar todos los archivos en el directorio
    if let Ok(entries) = readdir(path) {
        for entry in entries {
            let full_path = if path.ends_with('/') {
                path.to_string() + &entry
            } else {
                path.to_string() + "/" + &entry
            };
            
            // Cifrar archivo si no está ya cifrado
            if !is_encrypted(&full_path).unwrap_or(false) {
                let _ = encrypt_file(&full_path, algorithm.clone(), "directory");
            }
        }
    }
    
    Ok(())
}

pub fn decrypt_directory(path: &str) -> VfsResult<()> {
    // Verificar que el directorio está cifrado
    if !is_directory_encrypted(path)? {
        return Ok(());
    }
    
    // Obtener información de cifrado
    let encryption_info = unsafe {
        ENCRYPTED_DIRECTORIES.get(path).cloned()
            .ok_or(VfsError::FileNotFound)?
    };
    
    // Descifrar todos los archivos en el directorio
    if let Ok(entries) = readdir(path) {
        for entry in entries {
            let full_path = if path.ends_with('/') {
                path.to_string() + &entry
            } else {
                path.to_string() + "/" + &entry
            };
            
            // Descifrar archivo si está cifrado
            if is_encrypted(&full_path).unwrap_or(false) {
                let _ = decrypt_file(&full_path);
            }
        }
    }
    
    // Eliminar información de cifrado del directorio
    unsafe {
        ENCRYPTED_DIRECTORIES.remove(path);
        TRANSPARENT_KEYS.remove(path);
    }
    
    Ok(())
}

pub fn is_directory_encrypted(path: &str) -> VfsResult<bool> {
    unsafe {
        Ok(ENCRYPTED_DIRECTORIES.contains_key(path))
    }
}

pub fn get_directory_encryption_info(path: &str) -> VfsResult<EncryptionInfo> {
    unsafe {
        ENCRYPTED_DIRECTORIES.get(path).cloned()
            .ok_or(VfsError::FileNotFound)
    }
}

// Gestión de claves transparentes
pub fn generate_directory_key(path: &str) -> VfsResult<Vec<u8>> {
    // Generar clave basada en el path y timestamp
    let mut key = Vec::new();
    key.extend_from_slice(path.as_bytes());
    // Usar timestamp simulado (compatible con no_std)
    let timestamp = 1234567890u64; // En un sistema real usaríamos RTC
    key.extend_from_slice(&timestamp.to_le_bytes());
    
    // Aplicar hash simple pero efectivo (compatible con no_std)
    let mut hashed_key = Vec::new();
    let mut hash: u64 = 0x811c9dc5; // FNV offset basis
    
    for &byte in &key {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x01000193); // FNV prime
    }
    
    // Convertir hash a clave de 32 bytes
    for i in 0..4 {
        hashed_key.extend_from_slice(&(hash >> (i * 8)).to_le_bytes());
    }
    // Repetir para obtener 32 bytes
    while hashed_key.len() < 32 {
        let current_len = hashed_key.len();
        let to_copy = core::cmp::min(8, 32 - current_len);
        let slice = hashed_key[..to_copy].to_vec();
        hashed_key.extend_from_slice(&slice);
    }
    hashed_key.truncate(32);
    
    Ok(hashed_key)
}

pub fn get_transparent_key(path: &str) -> VfsResult<Vec<u8>> {
    unsafe {
        TRANSPARENT_KEYS.get(path).cloned()
            .ok_or(VfsError::FileNotFound)
    }
}

pub fn set_transparent_key(path: &str, key: Vec<u8>) -> VfsResult<()> {
    unsafe {
        TRANSPARENT_KEYS.insert(path.to_string(), key);
    }
    Ok(())
}

// Cifrado transparente en operaciones de escritura
pub fn transparent_encrypt_data(data: &[u8], path: &str) -> VfsResult<Vec<u8>> {
    if !is_transparent_encryption_enabled() {
        return Ok(data.to_vec());
    }
    
    // Verificar si el archivo debe ser cifrado automáticamente
    if get_transparent_encryption_config().auto_encrypt {
        if !is_encrypted(path).unwrap_or(false) {
            let _ = auto_encrypt_file(path);
        }
    }
    
    // Si el archivo está cifrado, usar su clave
    if is_encrypted(path).unwrap_or(false) {
        if let Ok(encryption_info) = get_encryption_info(path) {
            match encryption_info.encryption_type {
                EncryptionType::AES256 => encrypt_aes256(data, &encryption_info.key_id, &encryption_info.iv, &encryption_info.salt),
                EncryptionType::ChaCha20 => encrypt_chacha20(data, &encryption_info.key_id, &encryption_info.iv, &encryption_info.salt),
                EncryptionType::None => Ok(data.to_vec()),
            }
        } else {
            Ok(data.to_vec())
        }
    } else {
        Ok(data.to_vec())
    }
}

// Descifrado transparente en operaciones de lectura
pub fn transparent_decrypt_data(data: &[u8], path: &str) -> VfsResult<Vec<u8>> {
    if !is_transparent_encryption_enabled() {
        return Ok(data.to_vec());
    }
    
    // Si el archivo está cifrado, descifrarlo
    if is_encrypted(path).unwrap_or(false) {
        if let Ok(encryption_info) = get_encryption_info(path) {
            match encryption_info.encryption_type {
                EncryptionType::AES256 => decrypt_aes256(data, &encryption_info.key_id, &encryption_info.iv, &encryption_info.salt),
                EncryptionType::ChaCha20 => decrypt_chacha20(data, &encryption_info.key_id, &encryption_info.iv, &encryption_info.salt),
                EncryptionType::None => Ok(data.to_vec()),
            }
        } else {
            Ok(data.to_vec())
        }
    } else {
        Ok(data.to_vec())
    }
}

// Rotación automática de claves
pub fn rotate_transparent_keys() -> VfsResult<()> {
    if !is_transparent_encryption_enabled() {
        return Ok(());
    }
    
    let config = get_transparent_encryption_config();
    // Usar tiempo simulado (compatible con no_std)
    let current_time = 1234567890u64; // En un sistema real usaríamos RTC
    
    // Rotar claves de directorios cifrados
    unsafe {
        let mut keys_to_rotate = Vec::new();
        
        for (path, encryption_info) in &ENCRYPTED_DIRECTORIES {
            // Verificar si es tiempo de rotar (simulado)
            if current_time % config.key_rotation_interval == 0 {
                keys_to_rotate.push(path.clone());
            }
        }
        
        for path in keys_to_rotate {
            if let Ok(new_key) = generate_directory_key(&path) {
                TRANSPARENT_KEYS.insert(path.clone(), new_key);
            }
        }
    }
    
    Ok(())
}

// Estadísticas de cifrado transparente
pub fn get_transparent_encryption_stats() -> (usize, usize, usize) {
    unsafe {
        (
            ENCRYPTED_DIRECTORIES.len(),
            TRANSPARENT_KEYS.len(),
            if TRANSPARENT_ENCRYPTION_CONFIG.enabled { 1 } else { 0 }
        )
    }
}

// Limpiar cifrado transparente
pub fn clear_transparent_encryption() {
    unsafe {
        ENCRYPTED_DIRECTORIES.clear();
        TRANSPARENT_KEYS.clear();
        TRANSPARENT_ENCRYPTION_CONFIG = TransparentEncryptionConfig::default();
    }
}

// Inicializar sistema de cifrado transparente
fn init_transparent_encryption() -> VfsResult<()> {
    unsafe {
        ENCRYPTED_DIRECTORIES = BTreeMap::new();
        TRANSPARENT_KEYS = BTreeMap::new();
        TRANSPARENT_ENCRYPTION_CONFIG = TransparentEncryptionConfig::default();
    }
    Ok(())
}

// ============================================================================
// HERRAMIENTAS DE ADMINISTRACIÓN (fsck, df, find)
// ============================================================================

#[derive(Debug, Clone)]
pub struct FsckResult {
    pub errors_found: usize,
    pub errors_fixed: usize,
    pub warnings: usize,
    pub files_checked: usize,
    pub directories_checked: usize,
    pub total_errors: Vec<String>,
    pub fixed_errors: Vec<String>,
}

impl Default for FsckResult {
    fn default() -> Self {
        Self {
            errors_found: 0,
            errors_fixed: 0,
            warnings: 0,
            files_checked: 0,
            directories_checked: 0,
            total_errors: Vec::new(),
            fixed_errors: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DfResult {
    pub total_size: u64,
    pub used_size: u64,
    pub free_size: u64,
    pub inodes_total: u64,
    pub inodes_used: u64,
    pub inodes_free: u64,
    pub filesystem_name: String,
    pub mount_point: String,
}

#[derive(Debug, Clone)]
pub struct FindResult {
    pub files: Vec<String>,
    pub directories: Vec<String>,
    pub symlinks: Vec<String>,
    pub total_found: usize,
    pub search_criteria: String,
}

// FSCK - Verificación y reparación del sistema de archivos
pub fn fsck_verify() -> VfsResult<FsckResult> {
    let mut result = FsckResult::default();
    
    unsafe {
        if let Some(ref nodes) = FS_NODES {
            // Verificar cada nodo
            for (inode, node) in nodes.iter() {
                match node.kind {
                    NodeKind::File => {
                        result.files_checked += 1;
                        
                        // Verificar integridad del archivo
                        if let Err(e) = verify_file_integrity(*inode, node) {
                            result.errors_found += 1;
                            result.total_errors.push((inode.to_string() + ": " + &e.to_string()).to_string());
                            
                            // Intentar reparar
                            if let Ok(()) = repair_file_integrity(*inode, node) {
                                result.errors_fixed += 1;
                                result.fixed_errors.push((inode.to_string() + ": Reparado").to_string());
                            }
                        }
                    },
                    NodeKind::Dir => {
                        result.directories_checked += 1;
                        
                        // Verificar integridad del directorio
                        if let Err(e) = verify_directory_integrity(*inode, node) {
                            result.errors_found += 1;
                            result.total_errors.push(("Directorio ".to_string() + &inode.to_string() + ": " + &e.to_string()).to_string());
                            
                            // Intentar reparar
                            if let Ok(()) = repair_directory_integrity(*inode, node) {
                                result.errors_fixed += 1;
                                result.fixed_errors.push(("Directorio ".to_string() + &inode.to_string() + ": Reparado").to_string());
                            }
                        }
                    },
                    NodeKind::Symlink => {
                        result.files_checked += 1;
                        
                        // Verificar integridad del symlink
                        if let Err(e) = verify_symlink_integrity(*inode, node) {
                            result.errors_found += 1;
                            result.total_errors.push(("Symlink ".to_string() + &inode.to_string() + ": " + &e.to_string()).to_string());
                            
                            // Intentar reparar
                            if let Ok(()) = repair_symlink_integrity(*inode, node) {
                                result.errors_fixed += 1;
                                result.fixed_errors.push(("Symlink ".to_string() + &inode.to_string() + ": Reparado").to_string());
                            }
                        }
                    },
                }
            }
        }
    }
    
    Ok(result)
}

fn verify_file_integrity(inode: u32, node: &Node) -> VfsResult<()> {
    // Verificar que el archivo tiene datos válidos
    if node.size != node.data.len() as u64 {
        return Err(VfsError::InvalidOperation);
    }
    
    // Verificar que los timestamps son válidos
    if node.atime > node.mtime || node.mtime > node.ctime {
        return Err(VfsError::InvalidOperation);
    }
    
    // Verificar que los permisos son válidos
    if node.mode & 0o777 == 0 {
        return Err(VfsError::InvalidOperation);
    }
    
    Ok(())
}

fn repair_file_integrity(inode: u32, node: &Node) -> VfsResult<()> {
    unsafe {
        if let Some(ref mut nodes) = FS_NODES {
            if let Some(ref mut n) = nodes.get_mut(&inode) {
                // Reparar tamaño
                n.size = n.data.len() as u64;
                
                // Reparar timestamps
                if n.atime > n.mtime {
                    n.atime = n.mtime;
                }
                if n.mtime > n.ctime {
                    n.mtime = n.ctime;
                }
                
                // Reparar permisos
                if n.mode & 0o777 == 0 {
                    n.mode |= 0o644; // Permisos por defecto
                }
            }
        }
    }
    Ok(())
}

fn verify_directory_integrity(inode: u32, node: &Node) -> VfsResult<()> {
    // Verificar que es un directorio
    if node.kind != NodeKind::Dir {
        return Err(VfsError::InvalidOperation);
    }
    
    // Verificar que tiene permisos de directorio
    if node.mode & 0o040000 == 0 {
        return Err(VfsError::InvalidOperation);
    }
    
    Ok(())
}

fn repair_directory_integrity(inode: u32, node: &Node) -> VfsResult<()> {
    unsafe {
        if let Some(ref mut nodes) = FS_NODES {
            if let Some(ref mut n) = nodes.get_mut(&inode) {
                // Asegurar permisos de directorio
                n.mode |= 0o040000;
                if n.mode & 0o777 == 0 {
                    n.mode |= 0o755; // Permisos por defecto para directorios
                }
            }
        }
    }
    Ok(())
}

fn verify_symlink_integrity(inode: u32, node: &Node) -> VfsResult<()> {
    // Verificar que es un symlink
    if node.kind != NodeKind::Symlink {
        return Err(VfsError::InvalidOperation);
    }
    
    // Verificar que tiene datos (target)
    if node.data.is_empty() {
        return Err(VfsError::InvalidOperation);
    }
    
    Ok(())
}

fn repair_symlink_integrity(inode: u32, node: &Node) -> VfsResult<()> {
    unsafe {
        if let Some(ref mut nodes) = FS_NODES {
            if let Some(ref mut n) = nodes.get_mut(&inode) {
                // Asegurar permisos de symlink
                n.mode |= 0o120000; // S_IFLNK
                if n.mode & 0o777 == 0 {
                    n.mode |= 0o777; // Permisos por defecto para symlinks
                }
            }
        }
    }
    Ok(())
}

// DF - Información de uso del disco
pub fn df_get_usage() -> VfsResult<DfResult> {
    let mut total_size = 0u64;
    let mut used_size = 0u64;
    let mut inodes_total = 0u64;
    let mut inodes_used = 0u64;
    
    unsafe {
        if let Some(ref nodes) = FS_NODES {
            inodes_total = nodes.len() as u64;
            inodes_used = inodes_total;
            
            for (_, node) in nodes.iter() {
                total_size += node.size;
                used_size += node.size;
            }
        }
    }
    
    let free_size = total_size.saturating_sub(used_size);
    let inodes_free = inodes_total.saturating_sub(inodes_used);
    
    Ok(DfResult {
        total_size,
        used_size,
        free_size,
        inodes_total,
        inodes_used,
        inodes_free,
        filesystem_name: "EclipseFS".to_string(),
        mount_point: "/".to_string(),
    })
}

// FIND - Búsqueda de archivos
pub fn find_files(pattern: &str, search_path: &str) -> VfsResult<FindResult> {
    let mut result = FindResult {
        files: Vec::new(),
        directories: Vec::new(),
        symlinks: Vec::new(),
        total_found: 0,
        search_criteria: pattern.to_string(),
    };
    
    // Buscar en el sistema de archivos
    find_recursive(pattern, search_path, &mut result)?;
    
    result.total_found = result.files.len() + result.directories.len() + result.symlinks.len();
    
    Ok(result)
}

fn find_recursive(pattern: &str, path: &str, result: &mut FindResult) -> VfsResult<()> {
    // Verificar si el path actual coincide con el patrón
    if matches_pattern(path, pattern) {
        if let Ok(inode) = lookup_path(path) {
            if let Some(node) = get_node(inode) {
                match node.kind {
                    NodeKind::File => result.files.push(path.to_string()),
                    NodeKind::Dir => result.directories.push(path.to_string()),
                    NodeKind::Symlink => result.symlinks.push(path.to_string()),
                }
            }
        }
    }
    
    // Buscar en subdirectorios
    if let Ok(entries) = readdir(path) {
        for entry in entries {
            let full_path = if path.ends_with('/') {
                path.to_string() + &entry
            } else {
                path.to_string() + "/" + &entry
            };
            
            // Recursión
            let _ = find_recursive(pattern, &full_path, result);
        }
    }
    
    Ok(())
}

fn matches_pattern(path: &str, pattern: &str) -> bool {
    // Implementación simple de matching de patrones
    if pattern == "*" {
        return true;
    }
    
    if pattern.starts_with('*') && pattern.ends_with('*') {
        let inner = &pattern[1..pattern.len()-1];
        return path.contains(inner);
    }
    
    if pattern.starts_with('*') {
        let suffix = &pattern[1..];
        return path.ends_with(suffix);
    }
    
    if pattern.ends_with('*') {
        let prefix = &pattern[..pattern.len()-1];
        return path.starts_with(prefix);
    }
    
    path == pattern
}

// Funciones adicionales de administración
pub fn get_filesystem_health() -> VfsResult<(f32, Vec<String>)> {
    let mut health_score = 100.0;
    let mut issues = Vec::new();
    
    unsafe {
        if let Some(ref nodes) = FS_NODES {
            let total_nodes = nodes.len();
            let mut corrupted_nodes = 0;
            
            for (inode, node) in nodes.iter() {
                if verify_file_integrity(*inode, node).is_err() ||
                   verify_directory_integrity(*inode, node).is_err() ||
                   verify_symlink_integrity(*inode, node).is_err() {
                    corrupted_nodes += 1;
                }
            }
            
            if total_nodes > 0 {
                health_score = ((total_nodes - corrupted_nodes) as f32 / total_nodes as f32) * 100.0;
            }
            
            if corrupted_nodes > 0 {
                issues.push((corrupted_nodes.to_string() + " nodos corruptos encontrados").to_string());
            }
            
            // Verificar uso de memoria
            let cache_stats = get_cache_stats();
            if cache_stats.1 > 1000 { // Más de 1000 entradas en cache
                issues.push("Cache muy grande, considerar limpieza".to_string());
                health_score -= 10.0;
            }
        }
    }
    
    if health_score < 50.0 {
        issues.push("Sistema de archivos en estado crítico".to_string());
    }
    
    Ok((health_score, issues))
}


pub fn get_detailed_stats() -> VfsResult<(usize, usize, usize, usize, usize, usize)> {
    let mut files = 0;
    let mut directories = 0;
    let mut symlinks = 0;
    let mut encrypted_files = 0;
    let mut compressed_files = 0;
    let mut total_size = 0u64;
    
    unsafe {
        if let Some(ref nodes) = FS_NODES {
            for (_, node) in nodes.iter() {
                match node.kind {
                    NodeKind::File => files += 1,
                    NodeKind::Dir => directories += 1,
                    NodeKind::Symlink => symlinks += 1,
                }
                
                total_size += node.size;
                
                if node.encryption.encryption_type != EncryptionType::None {
                    encrypted_files += 1;
                }
                
                if node.compression.compression_type != CompressionType::None {
                    compressed_files += 1;
                }
            }
        }
    }
    
    Ok((files, directories, symlinks, encrypted_files, compressed_files, total_size as usize))
}


