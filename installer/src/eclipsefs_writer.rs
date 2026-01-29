//! Escritor de EclipseFS usando la librería común

use eclipsefs_lib::{
    EclipseFS, NodeKind, EclipseFSWriter, constants
};

/// Wrapper para el escritor de EclipseFS que mantiene compatibilidad con el instalador
pub struct EclipseFSInstaller {
    filesystem: EclipseFS,
    image_path: String,
}

impl EclipseFSInstaller {
    /// Crear un nuevo instalador de EclipseFS
    pub fn new(image_path: String) -> Self {
        Self {
            filesystem: EclipseFS::new(),
            image_path,
        }
    }
    
    /// Crear estructura básica del sistema de archivos (idempotente)
    pub fn create_basic_structure(&mut self) -> Result<(), String> {
        println!("📁 Creando estructura básica del sistema de archivos...");
        
        // Crear directorios básicos (en orden para evitar problemas de dependencias)
        let dirs = vec![
            "bin", "sbin", "usr", "etc", "var", "tmp", "home", 
            "root", "proc", "sys", "dev", "mnt", "opt", "lib",
            "lib64", "usr/bin", "usr/sbin", "usr/lib", "usr/lib64"
        ];
        
        let mut created_count = 0;
        let mut skipped_count = 0;
        
        for dir_path in dirs {
            match self.create_directory(dir_path) {
                Ok(_) => {
                    // Verificar si realmente se creó o ya existía
                    let full_path = format!("/{}", dir_path);
                    if self.filesystem.lookup_path(&full_path).is_ok() {
                        created_count += 1;
                    } else {
                        skipped_count += 1;
                    }
                },
                Err(e) => {
                    // Si es DuplicateEntry, es normal, solo continuar
                    if e.contains("DuplicateEntry") {
                        skipped_count += 1;
                        continue;
                    }
                    return Err(format!("Error creando estructura básica en {}: {}", dir_path, e));
                }
            }
        }
        
        println!("📁 Estructura básica: {} directorios creados, {} ya existían", created_count, skipped_count);
        Ok(())
    }
    
    /// Crear un directorio
    pub fn create_directory(&mut self, path: &str) -> Result<(), String> {
        let normalized = path.trim_start_matches('/');
        let path_parts: Vec<&str> = normalized.split('/').filter(|p| !p.is_empty()).collect();
        
        if path_parts.is_empty() {
            return Ok(()); // Ya existe el directorio raíz
        }
        
        // Construir el path completo
        let full_path = format!("/{}", path_parts.join("/"));
        
        // Verificar si ya existe el directorio completo
        if self.filesystem.lookup_path(&full_path).is_ok() {
            return Ok(()); // Ya existe
        }
        
        // Crear directorios padre si no existen
        let mut current_inode = constants::ROOT_INODE;
        let mut current_path = String::new();
        
        for part in path_parts.iter() {
            current_path.push('/');
            current_path.push_str(part);
            
            // Verificar si este nivel ya existe
            if let Ok(inode) = self.filesystem.lookup_path(&current_path) {
                current_inode = inode;
                continue;
            }
            
            // Crear el directorio en este nivel
            match self.filesystem.create_directory(current_inode, part) {
                Ok(new_inode) => {
                    current_inode = new_inode;
                },
                Err(e) => {
                    // Si es DuplicateEntry, significa que se creó entre verificaciones
                    if e == eclipsefs_lib::EclipseFSError::DuplicateEntry {
                        // Intentar obtener el inode nuevamente
                        if let Ok(inode) = self.filesystem.lookup_path(&current_path) {
                            current_inode = inode;
                            continue;
                        }
                    }
                    return Err(format!("Error creando directorio {}: {:?}", current_path, e));
                }
            }
        }
        
        Ok(())
    }
    
    /// Crear un archivo
    pub fn create_file(&mut self, path: &str, content: Vec<u8>) -> Result<(), String> {
        let normalized = path.trim_start_matches('/');
        let path_parts: Vec<&str> = normalized.split('/').filter(|p| !p.is_empty()).collect();
        
        if path_parts.is_empty() {
            return Err("Path inválido".to_string());
        }
        
        // Asegurar que el directorio padre existe
        if path_parts.len() > 1 {
            let parent_path = path_parts[..path_parts.len()-1].join("/");
            self.create_directory(&parent_path)?;
        }
        
        // Obtener el directorio padre
        let parent_path = if path_parts.len() > 1 {
            format!("/{}", path_parts[..path_parts.len()-1].join("/"))
        } else {
            "/".to_string()
        };
        
        let parent_inode = self.find_inode_by_path(&parent_path)
            .ok_or_else(|| format!("No se pudo encontrar el directorio padre: {}", parent_path))?;
        
        let file_name = path_parts.last().unwrap();
        
        // Crear el archivo
        match self.filesystem.create_file(parent_inode, file_name) {
            Ok(file_inode) => {
                // Escribir el contenido
                match self.filesystem.write_file(file_inode, &content) {
                    Ok(_) => Ok(()),
                    Err(e) => Err(format!("Error escribiendo archivo {}: {:?}", path, e)),
                }
            },
            Err(e) => {
                if e == eclipsefs_lib::EclipseFSError::DuplicateEntry {
                    // El archivo ya existe - esto es una duplicación que debemos prevenir
                    eprintln!("⚠️  WARNING: Intento de crear archivo duplicado: {}", path);
                    eprintln!("    Este archivo ya existe en el sistema de archivos.");
                    eprintln!("    Ignorando esta operación para prevenir duplicados.");
                    return Ok(()); // Retornar Ok para continuar, pero no crear el duplicado
                }
                Err(format!("Error creando archivo {}: {:?}", path, e))
            },
        }
    }
    
    /// Crear un enlace simbólico
    pub fn create_symlink(&mut self, target: &str, link_path: &str) -> Result<(), String> {
        let path_parts: Vec<&str> = link_path.split('/').filter(|p| !p.is_empty()).collect();
        
        if path_parts.is_empty() {
            return Err("Path inválido".to_string());
        }
        
        let mut current_inode = constants::ROOT_INODE;
        
        // Navegar hasta el directorio padre
        if path_parts.len() > 1 {
            let parent_path = path_parts[..path_parts.len()-1].join("/");
            current_inode = self.find_inode_by_path(&format!("/{}", parent_path))
                .unwrap_or(constants::ROOT_INODE);
        }
        
        let link_name = path_parts.last().unwrap();
        
        // Crear el enlace simbólico
        match self.filesystem.create_symlink(current_inode, link_name, target) {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("Error creando enlace simbólico {} -> {}: {:?}", link_path, target, e)),
        }
    }
    
    /// Instalar un binario
    pub fn install_binary(&mut self, target_path: &str, source_path: &str) -> Result<(), String> {
        let content = std::fs::read(source_path)
            .map_err(|e| format!("Error leyendo archivo {}: {}", source_path, e))?;
        
        self.create_file(target_path, content)
    }
    
    /// Encontrar inode por path
    fn find_inode_by_path(&self, path: &str) -> Option<u32> {
        self.filesystem.lookup_path(path).ok()
    }
    
    /// Escribir la imagen del sistema de archivos
    pub fn write_image(&mut self) -> Result<(), String> {
        println!("🔧 Escribiendo imagen EclipseFS a: {}", self.image_path);
        
        // Contar el número total de nodos que vamos a escribir
        let stats = self.filesystem.get_stats();
        let total_inodes = stats.0; // total_nodes
        
        println!("📊 Escribiendo {} nodos al sistema de archivos", total_inodes);
        
        // Crear el escritor EclipseFS
        let mut writer = EclipseFSWriter::from_path(&self.image_path)
            .map_err(|e| format!("Error creando EclipseFSWriter: {:?}", e))?;
        
        // Agregar todos los nodos del sistema de archivos al writer
        self.add_all_nodes_to_writer(&mut writer)?;
        
        // Escribir la imagen completa
        writer.write_image()
            .map_err(|e| format!("Error escribiendo imagen: {:?}", e))?;
        
        println!("✅ Imagen EclipseFS escrita exitosamente con {} nodos", total_inodes);
        Ok(())
    }
    
    /// Agregar todos los nodos del sistema de archivos al writer
    fn add_all_nodes_to_writer(&self, writer: &mut EclipseFSWriter) -> Result<(), String> {
        // Obtener el nodo raíz
        let root_node = self.filesystem.get_node(constants::ROOT_INODE)
            .ok_or("Nodo raíz no encontrado")?;
        
        // Agregar el nodo raíz
        writer.add_node(constants::ROOT_INODE, root_node.clone())
            .map_err(|e| format!("Error agregando nodo raíz: {:?}", e))?;
        
        // Recursivamente agregar todos los nodos hijos
        self.add_node_recursive(constants::ROOT_INODE, writer)?;
        
        Ok(())
    }
    
    /// Recursivamente agregar nodos hijos al writer
    fn add_node_recursive(&self, parent_inode: u32, writer: &mut EclipseFSWriter) -> Result<(), String> {
        let parent_node = self.filesystem.get_node(parent_inode)
            .ok_or_else(|| format!("Nodo padre {} no encontrado", parent_inode))?;
        
        // Agregar todos los hijos del nodo padre
        for (name, child_inode) in parent_node.get_children().iter() {
            if let Some(child_node) = self.filesystem.get_node(*child_inode) {
                // Agregar el nodo hijo al writer
                writer.add_node(*child_inode, child_node.clone())
                    .map_err(|e| format!("Error agregando nodo {}: {:?}", name, e))?;
                
                // Si es un directorio, agregar recursivamente sus hijos
                if child_node.kind == NodeKind::Directory {
                    self.add_node_recursive(*child_inode, writer)?;
                }
            }
        }
        
        Ok(())
    }
    
    /// Obtener estadísticas del sistema de archivos
    pub fn get_stats(&self) -> (u32, u32, u32) {
        self.filesystem.get_stats()
    }
}