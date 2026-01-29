//! Implementación de EclipseFS del kernel para el instalador
//! 
//! Esta implementación usa las mismas estructuras que el kernel para garantizar compatibilidad

use std::fs;
use std::path::Path;
use std::io::Write;
use std::collections::BTreeMap;

/// Información de cifrado (simplificada para el instalador)
#[derive(Debug, Clone)]
pub struct EncryptionInfo {
    pub encryption_type: EncryptionType,
    pub key_id: String,
    pub iv: Vec<u8>,
    pub salt: Vec<u8>,
    pub is_encrypted: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EncryptionType {
    None,
    AES256,
    ChaCha20,
}

/// Información de compresión (simplificada para el instalador)
#[derive(Debug, Clone)]
pub struct CompressionInfo {
    pub compression_type: CompressionType,
    pub original_size: u64,
    pub compressed_size: u64,
    pub is_compressed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CompressionType {
    None,
    LZ4,
    LZ77,
    RLE,
}

/// Tipo de nodo
#[derive(Debug, Clone, PartialEq)]
pub enum NodeKind { 
    File, 
    Dir, 
    Symlink 
}

/// Nodo de EclipseFS
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
    pub nlink: u32,
    pub encryption: EncryptionInfo,
    pub compression: CompressionInfo,
}

impl Node {
    fn now() -> u64 { 
        // Timestamp fijo para el instalador
        1640995200 
    }
    
    pub fn new_dir() -> Self { 
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
                is_compressed: false,
            },
        } 
    }
    
    pub fn new_file(data: Vec<u8>) -> Self {
        let size = data.len() as u64;
        Self {
            kind: NodeKind::File,
            data,
            children: BTreeMap::new(),
            size,
            mode: 0o644,
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
                original_size: size,
                compressed_size: size,
                is_compressed: false,
            },
        }
    }

    pub fn new_symlink(target: String) -> Self {
        let data = target.into_bytes();
        let size = data.len() as u64;
        Self {
            kind: NodeKind::Symlink,
            data,
            children: BTreeMap::new(),
            size,
            mode: 0o777,
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
                original_size: size,
                compressed_size: size,
                is_compressed: false,
            },
        }
    }
}

/// Gestor de EclipseFS para el instalador
pub struct EclipseFSInstaller {
    nodes: BTreeMap<u32, Node>,
    next_inode: u32,
    image_path: String,
}

impl EclipseFSInstaller {
    pub fn new(image_path: String) -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(1, Node::new_dir());
        
        Self {
            nodes,
            next_inode: 2,
            image_path,
        }
    }
    
    /// Crear directorio
    pub fn create_dir(&mut self, path: &str) -> Result<(), String> {
        // Crear directorios padre si no existen
        self.ensure_parent_dirs(path)?;
        
        let inode = self.next_inode;
        self.next_inode += 1;
        
        let mut node = Node::new_dir();
        node.mode = 0o755;
        
        self.nodes.insert(inode, node);
        
        // Encontrar el directorio padre correcto
        let path_parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let dir_name = path_parts[path_parts.len() - 1];
        
        let mut parent_inode = 1; // Directorio raíz por defecto
        
        if path_parts.len() == 1 {
            // Directorio en la raíz (ej: /usr)
            parent_inode = 1;
        } else if path_parts.len() == 2 {
            // Directorio de segundo nivel (ej: /usr/bin)
            if let Some(parent_inode_found) = self.nodes.get(&1).and_then(|root| root.children.get(path_parts[0])) {
                parent_inode = *parent_inode_found;
            }
        } else if path_parts.len() == 3 {
            // Directorio de tercer nivel (ej: /usr/bin/eclipse)
            if let Some(parent_inode_found) = self.nodes.get(&1)
                .and_then(|root| root.children.get(path_parts[0]))
                .and_then(|first_level| self.nodes.get(first_level))
                .and_then(|first_level_node| first_level_node.children.get(path_parts[1])) {
                parent_inode = *parent_inode_found;
            }
        }
        
        // Agregar al directorio padre correcto
        if let Some(parent) = self.nodes.get_mut(&parent_inode) {
            parent.children.insert(dir_name.to_string(), inode);
            println!("         ✓ Directorio {} creado con inode {} en padre {}", dir_name, inode, parent_inode);
        }
        
        Ok(())
    }
    
    /// Asegurar que los directorios padre existan
    fn ensure_parent_dirs(&mut self, path: &str) -> Result<(), String> {
        let path_parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        
        // Crear directorios padre si no existen
        for i in 1..path_parts.len() {
            let parent_path = "/".to_string() + &path_parts[0..i].join("/");
            if !self.dir_exists(&parent_path) {
                self.create_dir_direct(&parent_path)?;
            }
        }
        
        Ok(())
    }
    
    /// Crear directorio directamente sin verificar padres
    fn create_dir_direct(&mut self, path: &str) -> Result<(), String> {
        let inode = self.next_inode;
        self.next_inode += 1;
        
        let mut node = Node::new_dir();
        node.mode = 0o755;
        
        self.nodes.insert(inode, node);
        
        let path_parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let dir_name = path_parts[path_parts.len() - 1];
        
        // Encontrar el directorio padre correcto de forma recursiva
        let parent_inode = self.find_parent_inode(&path_parts)?;
        
        // Agregar al directorio padre correcto
        if let Some(parent) = self.nodes.get_mut(&parent_inode) {
            parent.children.insert(dir_name.to_string(), inode);
            println!("         ✓ Directorio {} creado con inode {} en padre {}", dir_name, inode, parent_inode);
        }
        
        Ok(())
    }
    
    /// Encontrar el inode del directorio padre de forma recursiva
    fn find_parent_inode(&self, path_parts: &[&str]) -> Result<u32, String> {
        if path_parts.is_empty() {
            return Err("Path vacío".to_string());
        }
        
        if path_parts.len() == 1 {
            // Directorio en la raíz (ej: /usr)
            return Ok(1);
        }
        
        // Navegar recursivamente por la jerarquía de directorios
        let mut current_inode = 1; // Empezar desde el directorio raíz
        
        for i in 0..path_parts.len() - 1 {
            let part = path_parts[i];
            if let Some(node) = self.nodes.get(&current_inode) {
                if let Some(&child_inode) = node.children.get(part) {
                    current_inode = child_inode;
                } else {
                    return Err(format!("Directorio padre '{}' no encontrado en el path", part));
                }
            } else {
                return Err(format!("Inode {} no encontrado", current_inode));
            }
        }
        
        Ok(current_inode)
    }
    
    /// Verificar si un directorio existe
    fn dir_exists(&self, path: &str) -> bool {
        let path_parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        
        if path_parts.is_empty() {
            return true; // Directorio raíz siempre existe
        }
        
        let mut current_inode = 1; // Empezar desde la raíz
        
        for part in &path_parts {
            if let Some(node) = self.nodes.get(&current_inode) {
                if let Some(child_inode) = node.children.get(*part) {
                    current_inode = *child_inode;
                } else {
                    return false;
                }
            } else {
                return false;
            }
        }
        
        true
    }
    
    /// Crear archivo
    pub fn create_file(&mut self, path: &str, content: &[u8]) -> Result<(), String> {
        println!("         🔧 Creando archivo: {} ({} bytes)", path, content.len());
        
        let inode = self.next_inode;
        self.next_inode += 1;
        
        let node = Node::new_file(content.to_vec());
        self.nodes.insert(inode, node);
        
        // Encontrar el directorio padre correcto usando la función recursiva
        let path_parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let file_name = path_parts[path_parts.len() - 1];
        
        let parent_inode = self.find_parent_inode(&path_parts)?;
        
        // Agregar al directorio padre correcto
        if let Some(parent) = self.nodes.get_mut(&parent_inode) {
            parent.children.insert(file_name.to_string(), inode);
            println!("         📁 Agregado a inode {}: {} -> inode {}", parent_inode, file_name, inode);
        } else {
            return Err(format!("No se pudo encontrar el directorio padre para: {}", path));
        }
        
        println!("         ✅ Archivo creado exitosamente");
        Ok(())
    }
    
    /// Escribir imagen EclipseFS
    pub fn write_image(&self, _size_mb: u64) -> Result<(), String> {
        println!("     💾 Escribiendo imagen EclipseFS real...");
        println!("     📊 Total de nodos a escribir: {}", self.nodes.len());
        
        // Mostrar información de los nodos
        for (inode, node) in &self.nodes {
            match node.kind {
                NodeKind::File => {
                    println!("       📄 Inode {}: archivo ({} bytes)", inode, node.data.len());
                }
                NodeKind::Dir => {
                    println!("       📁 Inode {}: directorio ({} hijos)", inode, node.children.len());
                }
                NodeKind::Symlink => {
                    println!("       🔗 Inode {}: enlace simbólico", inode);
                }
            }
        }
        
        // Crear archivo de imagen
        let mut file = fs::File::create(&self.image_path)
            .map_err(|e| format!("Error creando imagen: {}", e))?;
        
        // No establecer tamaño - trabajar directamente con la partición completa
        // El archivo ya tiene el tamaño de la partición (240GB en este caso)
        
        // Escribir header de EclipseFS
        self.write_header(&mut file)?;
        
        // Escribir nodos
        self.write_nodes(&mut file)?;
        
        println!("     ✅ Imagen EclipseFS real escrita");
        Ok(())
    }
    
    /// Escribir header de EclipseFS
    fn write_header(&self, file: &mut fs::File) -> Result<(), String> {
        // Magic number
        file.write_all(b"ECLIPSEFS")
            .map_err(|e| format!("Error escribiendo magic: {}", e))?;
        
        // Versión v2.0 (major=2, minor=0) -> 0x00020000
        let version = (2u32 << 16) | 0u32; // (major << 16) | minor
        file.write_all(&version.to_le_bytes())
            .map_err(|e| format!("Error escribiendo versión: {}", e))?;
        
        // Inode table offset (32 bytes después del header)
        file.write_all(&32u64.to_le_bytes())
            .map_err(|e| format!("Error escribiendo inode table offset: {}", e))?;
        
        // Inode table size (8 bytes por inodo)
        file.write_all(&((self.nodes.len() * 8) as u64).to_le_bytes())
            .map_err(|e| format!("Error escribiendo inode table size: {}", e))?;
        
        // Total inodes
        file.write_all(&(self.nodes.len() as u32).to_le_bytes())
            .map_err(|e| format!("Error escribiendo total inodes: {}", e))?;
        
        // Padding para alinear a 4KB
        let padding = vec![0u8; 4096 - 25];
        file.write_all(&padding)
            .map_err(|e| format!("Error escribiendo padding: {}", e))?;
        
        Ok(())
    }
    
    /// Escribir nodos
    fn write_nodes(&self, file: &mut fs::File) -> Result<(), String> {
        // Escribir tabla de inodos
        self.write_inode_table(file)?;
        
        // Escribir registros de nodos
        for (inode, node) in &self.nodes {
            self.write_node_v2(file, *inode, node)?;
        }
        Ok(())
    }
    
    /// Escribir tabla de inodos v0.2.0
    fn write_inode_table(&self, file: &mut fs::File) -> Result<(), String> {
        // Calcular offset inicial de registros (después de header + tabla)
        let mut current_offset = 32 + (self.nodes.len() * 8) as u64;
        
        // Escribir entradas de tabla de inodos
        for (inode, _) in &self.nodes {
            file.write_all(&current_offset.to_le_bytes())
                .map_err(|e| format!("Error escribiendo offset de inode {}: {}", inode, e))?;
            
            // Calcular tamaño del registro (estimación)
            let estimated_size = self.estimate_node_size(inode);
            current_offset += estimated_size;
        }
        
        Ok(())
    }
    
    /// Estimar tamaño de un registro de nodo
    fn estimate_node_size(&self, inode: &u32) -> u64 {
        let node = &self.nodes[inode];
        let mut size = 8; // Header (inode + size)
        
        // TLV entries
        size += 5; // NODE_TYPE (1 + 4)
        size += 8; // MODE (4 + 4)
        size += 8; // UID (4 + 4)
        size += 8; // GID (4 + 4)
        size += 12; // SIZE (8 + 4)
        size += 12; // ATIME (8 + 4)
        size += 12; // MTIME (8 + 4)
        size += 12; // CTIME (8 + 4)
        size += 8; // NLINK (4 + 4)
        
        match node.kind {
            NodeKind::File | NodeKind::Symlink => {
                size += 4; // CONTENT tag
                size += node.data.len() as u64;
            },
            NodeKind::Dir => {
                size += 4; // DIRECTORY_ENTRIES tag
                for (name, _) in &node.children {
                    size += 4; // name_len + child_inode
                    size += name.len() as u64;
                }
            }
        }
        
        size
    }
    
    /// Escribir nodo v0.2.0 (formato TLV)
    fn write_node_v2(&self, file: &mut fs::File, inode: u32, node: &Node) -> Result<(), String> {
        // Calcular tamaño total del registro
        let mut total_size = 8; // Header (inode + size)
        
        // TLV entries
        total_size += 5; // NODE_TYPE (1 + 4)
        total_size += 8; // MODE (4 + 4)
        total_size += 8; // UID (4 + 4)
        total_size += 8; // GID (4 + 4)
        total_size += 12; // SIZE (8 + 4)
        total_size += 12; // ATIME (8 + 4)
        total_size += 12; // MTIME (8 + 4)
        total_size += 12; // CTIME (8 + 4)
        total_size += 8; // NLINK (4 + 4)
        
        match node.kind {
            NodeKind::File | NodeKind::Symlink => {
                total_size += 4; // CONTENT tag
                total_size += node.data.len() as u64;
            },
            NodeKind::Dir => {
                total_size += 4; // DIRECTORY_ENTRIES tag
                for (name, _) in &node.children {
                    total_size += 4; // name_len + child_inode
                    total_size += name.len() as u64;
                }
            }
        }
        
        // Escribir header del registro
        file.write_all(&inode.to_le_bytes())
            .map_err(|e| format!("Error escribiendo inode: {}", e))?;
        file.write_all(&(total_size as u32).to_le_bytes())
            .map_err(|e| format!("Error escribiendo size: {}", e))?;
        
        // Escribir TLV entries
        self.write_tlv_entry(file, 0x0001, 1, &[match node.kind { NodeKind::File => 0, NodeKind::Dir => 1, NodeKind::Symlink => 2 }])?; // NODE_TYPE
        self.write_tlv_entry(file, 0x0002, 4, &node.mode.to_le_bytes())?; // MODE
        self.write_tlv_entry(file, 0x0003, 4, &node.uid.to_le_bytes())?; // UID
        self.write_tlv_entry(file, 0x0004, 4, &node.gid.to_le_bytes())?; // GID
        self.write_tlv_entry(file, 0x0005, 8, &node.size.to_le_bytes())?; // SIZE
        self.write_tlv_entry(file, 0x0006, 8, &node.atime.to_le_bytes())?; // ATIME
        self.write_tlv_entry(file, 0x0007, 8, &node.mtime.to_le_bytes())?; // MTIME
        self.write_tlv_entry(file, 0x0008, 8, &node.ctime.to_le_bytes())?; // CTIME
        self.write_tlv_entry(file, 0x0009, 4, &node.nlink.to_le_bytes())?; // NLINK
        
        match node.kind {
            NodeKind::File | NodeKind::Symlink => {
                self.write_tlv_entry(file, 0x000A, node.data.len() as u16, &node.data)?; // CONTENT
            },
            NodeKind::Dir => {
                // Escribir entradas de directorio
                let mut dir_data = Vec::new();
                for (name, child_inode) in &node.children {
                    dir_data.extend_from_slice(&(name.len() as u16).to_le_bytes());
                    dir_data.extend_from_slice(&child_inode.to_le_bytes());
                    dir_data.extend_from_slice(name.as_bytes());
                }
                self.write_tlv_entry(file, 0x000B, dir_data.len() as u16, &dir_data)?; // DIRECTORY_ENTRIES
            }
        }
        
        Ok(())
    }
    
    /// Escribir entrada TLV (Type-Length-Value)
    fn write_tlv_entry(&self, file: &mut fs::File, tag: u16, length: u16, data: &[u8]) -> Result<(), String> {
        file.write_all(&tag.to_le_bytes())
            .map_err(|e| format!("Error escribiendo tag: {}", e))?;
        file.write_all(&length.to_le_bytes())
            .map_err(|e| format!("Error escribiendo length: {}", e))?;
        file.write_all(data)
            .map_err(|e| format!("Error escribiendo data: {}", e))?;
        Ok(())
    }
    
    /// Escribir nodo individual (formato fijo de 64 bytes que espera el kernel) - DEPRECATED
    fn write_node(&self, file: &mut fs::File, inode: u32, node: &Node) -> Result<(), String> {
        // Buffer fijo de 64 bytes, con layout mínimo que parsea el kernel
        let mut buffer = [0u8; 64];

        // Campos base en offsets acordados
        // inode (0..4)
        buffer[0..4].copy_from_slice(&inode.to_le_bytes());

        // kind (4)
        buffer[4] = match node.kind { NodeKind::File => 0, NodeKind::Dir => 1, NodeKind::Symlink => 2 };

        // mode (8..12)
        buffer[8..12].copy_from_slice(&node.mode.to_le_bytes());

        // size (20..28)
        buffer[20..28].copy_from_slice(&node.size.to_le_bytes());

        // children_count (28..32)
        let children_count = node.children.len() as u32;
        buffer[28..32].copy_from_slice(&children_count.to_le_bytes());

        // Entradas de directorio desde el byte 32 hasta 63: [len(1), nombre, inode(4)]
        let mut off = 32usize;
        if node.kind == NodeKind::Dir {
            for (name, child_inode) in &node.children {
                let name_bytes = name.as_bytes();
                let need = 1 + name_bytes.len() + 4;
                if off + need > 64 { break; }
                if name_bytes.len() > 31 { continue; }
                buffer[off] = name_bytes.len() as u8; off += 1;
                buffer[off..off + name_bytes.len()].copy_from_slice(name_bytes); off += name_bytes.len();
                buffer[off..off + 4].copy_from_slice(&child_inode.to_le_bytes()); off += 4;
            }
        }

        // Escribir los 64 bytes del nodo
        file.write_all(&buffer).map_err(|e| format!("Error escribiendo nodo: {}", e))?;
        
        // Escribir datos del archivo si es un archivo (después del nodo)
        if node.kind == NodeKind::File && !node.data.is_empty() {
            file.write_all(&node.data)
                .map_err(|e| format!("Error escribiendo datos del archivo: {}", e))?;
        }
        
        Ok(())
    }
    
    /// Crear estructura básica del sistema
    pub fn create_basic_structure(&mut self) -> Result<(), String> {
        println!("       📁 Creando estructura básica de EclipseFS...");
        
        // Crear directorios esenciales
        let directories = vec![
            "/usr",
            "/usr/bin",
            "/usr/sbin",
            "/usr/lib",
            "/sbin",
            "/bin",
            "/etc",
            "/var",
            "/var/log",
            "/tmp",
            "/proc",
            "/sys",
            "/dev",
            "/mnt",
            "/run",
            "/boot",
        ];
        
        for dir in directories {
            self.create_dir(dir)?;
            println!("         ✓ Directorio {} creado", dir);
        }
        
        // Crear archivos de sistema
        self.create_system_files()?;
        
        println!("       ✅ Estructura básica creada");
        Ok(())
    }
    
    /// Crear archivos de sistema
    fn create_system_files(&mut self) -> Result<(), String> {
        println!("       📄 Creando archivos de sistema...");
        
        // /proc/version
        self.create_file("/proc/version", b"Eclipse OS Kernel v0.1.0\n")?;
        println!("         ✓ /proc/version creado");
        
        // /proc/cpuinfo
        self.create_file("/proc/cpuinfo", b"processor\t: 0\nvendor_id\t: Eclipse\ncpu family\t: 6\nmodel\t\t: 0\nmodel name\t: Eclipse CPU\n")?;
        println!("         ✓ /proc/cpuinfo creado");
        
        // /etc/hostname
        self.create_file("/etc/hostname", b"eclipse-os\n")?;
        println!("         ✓ /etc/hostname creado");
        
        // /etc/hosts
        self.create_file("/etc/hosts", b"127.0.0.1\tlocalhost\n::1\t\tlocalhost\n127.0.1.1\teclipse-os\n")?;
        println!("         ✓ /etc/hosts creado");
        
        // /etc/fstab
        self.create_file("/etc/fstab", b"# /etc/fstab: static file system information\n# <file system> <mount point>   <type>  <options>       <dump>  <pass>\nproc            /proc           proc    defaults        0       0\nsysfs           /sys            sysfs   defaults        0       0\ndevtmpfs        /dev            devtmpfs defaults       0       0\ntmpfs           /tmp            tmpfs   defaults        0       0\n")?;
        println!("         ✓ /etc/fstab creado");
        
        Ok(())
    }
    
    /// Instalar binario en EclipseFS
    pub fn install_binary(&mut self, path: &str, binary_path: &str) -> Result<(), String> {
        println!("       📦 Instalando binario: {}", path);
        println!("         🔍 Ruta fuente: {}", binary_path);
        
        // Verificar si el archivo fuente existe
        if !Path::new(binary_path).exists() {
            return Err(format!("Archivo fuente no existe: {}", binary_path));
        }
        
        // Verificar el tamaño del archivo
        let metadata = fs::metadata(binary_path)
            .map_err(|e| format!("Error obteniendo metadata de {}: {}", binary_path, e))?;
        println!("         📏 Tamaño del archivo: {} bytes", metadata.len());
        
        let content = fs::read(binary_path)
            .map_err(|e| format!("Error leyendo binario {}: {}", binary_path, e))?;
        
        println!("         📖 Contenido leído: {} bytes", content.len());
        
        self.create_file(path, &content)?;
        
        println!("         ✓ Binario {} instalado ({} bytes)", path, content.len());
        Ok(())
    }
    
    /// Crear enlace simbólico
    pub fn create_symlink(&mut self, path: &str, target: &str) -> Result<(), String> {
        // Crear directorios padre si no existen
        self.ensure_parent_dirs(path)?;
        
        let inode = self.next_inode;
        self.next_inode += 1;
        
        let node = Node::new_symlink(target.to_string());
        
        self.nodes.insert(inode, node);
        
        // Encontrar el directorio padre correcto
        let path_parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let link_name = path_parts[path_parts.len() - 1];
        
        let parent_inode = self.find_parent_inode(&path_parts)?;
        
        // Agregar al directorio padre correcto
        if let Some(parent) = self.nodes.get_mut(&parent_inode) {
            parent.children.insert(link_name.to_string(), inode);
            println!("         ✓ Enlace simbólico {} -> {} creado con inode {} en padre {}", link_name, target, inode, parent_inode);
        }
        
        Ok(())
    }
    
    /// Listar contenido de un directorio
    pub fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        let path_parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        
        // Si el path es "/" o está vacío, listar el directorio raíz
        if path == "/" || path_parts.is_empty() || (path_parts.len() == 1 && path_parts[0].is_empty()) {
            if let Some(node) = self.nodes.get(&1) {
                return Ok(node.children.keys().cloned().collect());
            } else {
                return Err("Directorio raíz no encontrado".to_string());
            }
        }
        
        let mut current_inode = 1;
        for part in path_parts {
            if part.is_empty() {
                continue; // Saltar partes vacías
            }
            if let Some(node) = self.nodes.get(&current_inode) {
                if let Some(&child_inode) = node.children.get(part) {
                    current_inode = child_inode;
                } else {
                    return Err(format!("Directorio '{}' no encontrado", part));
                }
            } else {
                return Err(format!("Inode {} no encontrado", current_inode));
            }
        }
        
        if let Some(node) = self.nodes.get(&current_inode) {
            Ok(node.children.keys().cloned().collect())
        } else {
            Err("Inode no encontrado".to_string())
        }
    }
}
