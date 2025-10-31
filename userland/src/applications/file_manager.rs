use alloc::vec::Vec;
use alloc::string::String;
use alloc::collections::BTreeMap;
//! Gestor de Archivos Avanzado para Eclipse OS
//! 
//! Implementa un gestor de archivos completo con:
//! - Navegación de directorios
//! - Operaciones de archivos (copiar, mover, eliminar)
//! - Vista previa de archivos
//! - Búsqueda de archivos
//! - Gestión de permisos
//! - Compresión y descompresión

use Result<(), &'static str>;
// use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Gestor de archivos principal
pub struct FileManager {
    /// Directorio actual
    current_path: PathBuf,
    /// Historial de navegación
    navigation_history: Vec<PathBuf>,
    /// Historial actual en el historial
    history_index: usize,
    /// Archivos y directorios en el directorio actual
    current_items: Vec<FileItem>,
    /// Configuración del gestor
    config: FileManagerConfig,
    /// Estado del gestor
    state: FileManagerState,
    /// Selección actual
    selection: Vec<usize>,
    /// Vista actual
    view_mode: ViewMode,
}

/// Elemento de archivo o directorio
#[derive(Debug, Clone)]
pub struct FileItem {
    pub name: String,
    pub path: PathBuf,
    pub item_type: FileType,
    pub size: u64,
    pub permissions: FilePermissions,
    pub owner: String,
    pub group: String,
    pub modified: String,
    pub accessed: String,
    pub created: String,
    pub attributes: FileAttributes,
}

/// Tipos de archivo
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum FileType {
    File,
    Directory,
    Symlink,
    BlockDevice,
    CharacterDevice,
    NamedPipe,
    Socket,
    Unknown,
}

/// Permisos de archivo
#[derive(Debug, Clone)]
pub struct FilePermissions {
    pub owner_read: bool,
    pub owner_write: bool,
    pub owner_execute: bool,
    pub group_read: bool,
    pub group_write: bool,
    pub group_execute: bool,
    pub other_read: bool,
    pub other_write: bool,
    pub other_execute: bool,
    pub setuid: bool,
    pub setgid: bool,
    pub sticky: bool,
}

/// Atributos de archivo
#[derive(Debug, Clone)]
pub struct FileAttributes {
    pub hidden: bool,
    pub system: bool,
    pub archive: bool,
    pub compressed: bool,
    pub encrypted: bool,
    pub readonly: bool,
}

/// Configuración del gestor de archivos
#[derive(Debug, Clone)]
pub struct FileManagerConfig {
    pub show_hidden_files: bool,
    pub show_system_files: bool,
    pub sort_by: SortBy,
    pub sort_order: SortOrder,
    pub view_mode: ViewMode,
    pub show_thumbnails: bool,
    pub auto_refresh: bool,
    pub confirm_deletions: bool,
    pub confirm_overwrites: bool,
    pub max_file_size_preview: u64,
    pub supported_formats: Vec<String>,
}

/// Criterios de ordenamiento
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SortBy {
    Name,
    Size,
    Modified,
    Type,
    Permissions,
}

/// Orden de clasificación
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SortOrder {
    Ascending,
    Descending,
}

/// Modos de vista
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ViewMode {
    List,
    Grid,
    Tree,
    Details,
}

/// Estados del gestor
#[derive(Debug, Clone, PartialEq)]
pub enum FileManagerState {
    Normal,
    Selecting,
    Copying,
    Moving,
    Deleting,
    Searching,
    Renaming,
    Creating,
}

impl FileManager {
    /// Crear nuevo gestor de archivos
    pub fn new(config: FileManagerConfig) -> Self {
        let mut manager = Self {
            current_path: PathBuf::from("/"),
            navigation_history: vec![PathBuf::from("/")],
            history_index: 0,
            current_items: Vec::new(),
            config,
            state: FileManagerState::Normal,
            selection: Vec::new(),
            view_mode: ViewMode::List,
        };
        
        // Cargar contenido inicial
        manager.refresh_directory();
        manager
    }

    /// Refrescar directorio actual
    pub fn refresh_directory(&mut self) {
        self.current_items.clear();
        self.load_directory_contents();
        self.sort_items();
    }

    /// Cargar contenido del directorio
    fn load_directory_contents(&mut self) {
        // Simular contenido del directorio
        let items = vec![
            FileItem {
                name: "bin".to_string(),
                path: self.current_path.join("bin"),
                item_type: FileType::Directory,
                size: 4096,
                permissions: FilePermissions {
                    owner_read: true,
                    owner_write: true,
                    owner_execute: true,
                    group_read: true,
                    group_write: false,
                    group_execute: true,
                    other_read: true,
                    other_write: false,
                    other_execute: true,
                    setuid: false,
                    setgid: false,
                    sticky: false,
                },
                owner: "root".to_string(),
                group: "root".to_string(),
                modified: "2024-12-15 10:30:45".to_string(),
                accessed: "2024-12-15 10:30:45".to_string(),
                created: "2024-12-15 10:30:45".to_string(),
                attributes: FileAttributes {
                    hidden: false,
                    system: true,
                    archive: false,
                    compressed: false,
                    encrypted: false,
                    readonly: false,
                },
            },
            FileItem {
                name: "dev".to_string(),
                path: self.current_path.join("dev"),
                item_type: FileType::Directory,
                size: 4096,
                permissions: FilePermissions {
                    owner_read: true,
                    owner_write: true,
                    owner_execute: true,
                    group_read: true,
                    group_write: false,
                    group_execute: true,
                    other_read: true,
                    other_write: false,
                    other_execute: true,
                    setuid: false,
                    setgid: false,
                    sticky: false,
                },
                owner: "root".to_string(),
                group: "root".to_string(),
                modified: "2024-12-15 10:30:45".to_string(),
                accessed: "2024-12-15 10:30:45".to_string(),
                created: "2024-12-15 10:30:45".to_string(),
                attributes: FileAttributes {
                    hidden: false,
                    system: true,
                    archive: false,
                    compressed: false,
                    encrypted: false,
                    readonly: false,
                },
            },
            FileItem {
                name: "etc".to_string(),
                path: self.current_path.join("etc"),
                item_type: FileType::Directory,
                size: 4096,
                permissions: FilePermissions {
                    owner_read: true,
                    owner_write: true,
                    owner_execute: true,
                    group_read: true,
                    group_write: false,
                    group_execute: true,
                    other_read: true,
                    other_write: false,
                    other_execute: true,
                    setuid: false,
                    setgid: false,
                    sticky: false,
                },
                owner: "root".to_string(),
                group: "root".to_string(),
                modified: "2024-12-15 10:30:45".to_string(),
                accessed: "2024-12-15 10:30:45".to_string(),
                created: "2024-12-15 10:30:45".to_string(),
                attributes: FileAttributes {
                    hidden: false,
                    system: true,
                    archive: false,
                    compressed: false,
                    encrypted: false,
                    readonly: false,
                },
            },
            FileItem {
                name: "home".to_string(),
                path: self.current_path.join("home"),
                item_type: FileType::Directory,
                size: 4096,
                permissions: FilePermissions {
                    owner_read: true,
                    owner_write: true,
                    owner_execute: true,
                    group_read: true,
                    group_write: false,
                    group_execute: true,
                    other_read: true,
                    other_write: false,
                    other_execute: true,
                    setuid: false,
                    setgid: false,
                    sticky: false,
                },
                owner: "root".to_string(),
                group: "root".to_string(),
                modified: "2024-12-15 10:30:45".to_string(),
                accessed: "2024-12-15 10:30:45".to_string(),
                created: "2024-12-15 10:30:45".to_string(),
                attributes: FileAttributes {
                    hidden: false,
                    system: false,
                    archive: false,
                    compressed: false,
                    encrypted: false,
                    readonly: false,
                },
            },
            FileItem {
                name: "lib".to_string(),
                path: self.current_path.join("lib"),
                item_type: FileType::Directory,
                size: 4096,
                permissions: FilePermissions {
                    owner_read: true,
                    owner_write: true,
                    owner_execute: true,
                    group_read: true,
                    group_write: false,
                    group_execute: true,
                    other_read: true,
                    other_write: false,
                    other_execute: true,
                    setuid: false,
                    setgid: false,
                    sticky: false,
                },
                owner: "root".to_string(),
                group: "root".to_string(),
                modified: "2024-12-15 10:30:45".to_string(),
                accessed: "2024-12-15 10:30:45".to_string(),
                created: "2024-12-15 10:30:45".to_string(),
                attributes: FileAttributes {
                    hidden: false,
                    system: true,
                    archive: false,
                    compressed: false,
                    encrypted: false,
                    readonly: false,
                },
            },
            FileItem {
                name: "proc".to_string(),
                path: self.current_path.join("proc"),
                item_type: FileType::Directory,
                size: 4096,
                permissions: FilePermissions {
                    owner_read: true,
                    owner_write: true,
                    owner_execute: true,
                    group_read: true,
                    group_write: false,
                    group_execute: true,
                    other_read: true,
                    other_write: false,
                    other_execute: true,
                    setuid: false,
                    setgid: false,
                    sticky: false,
                },
                owner: "root".to_string(),
                group: "root".to_string(),
                modified: "2024-12-15 10:30:45".to_string(),
                accessed: "2024-12-15 10:30:45".to_string(),
                created: "2024-12-15 10:30:45".to_string(),
                attributes: FileAttributes {
                    hidden: false,
                    system: true,
                    archive: false,
                    compressed: false,
                    encrypted: false,
                    readonly: false,
                },
            },
            FileItem {
                name: "sys".to_string(),
                path: self.current_path.join("sys"),
                item_type: FileType::Directory,
                size: 4096,
                permissions: FilePermissions {
                    owner_read: true,
                    owner_write: true,
                    owner_execute: true,
                    group_read: true,
                    group_write: false,
                    group_execute: true,
                    other_read: true,
                    other_write: false,
                    other_execute: true,
                    setuid: false,
                    setgid: false,
                    sticky: false,
                },
                owner: "root".to_string(),
                group: "root".to_string(),
                modified: "2024-12-15 10:30:45".to_string(),
                accessed: "2024-12-15 10:30:45".to_string(),
                created: "2024-12-15 10:30:45".to_string(),
                attributes: FileAttributes {
                    hidden: false,
                    system: true,
                    archive: false,
                    compressed: false,
                    encrypted: false,
                    readonly: false,
                },
            },
            FileItem {
                name: "tmp".to_string(),
                path: self.current_path.join("tmp"),
                item_type: FileType::Directory,
                size: 4096,
                permissions: FilePermissions {
                    owner_read: true,
                    owner_write: true,
                    owner_execute: true,
                    group_read: true,
                    group_write: true,
                    group_execute: true,
                    other_read: true,
                    other_write: true,
                    other_execute: true,
                    setuid: false,
                    setgid: false,
                    sticky: true,
                },
                owner: "root".to_string(),
                group: "root".to_string(),
                modified: "2024-12-15 10:30:45".to_string(),
                accessed: "2024-12-15 10:30:45".to_string(),
                created: "2024-12-15 10:30:45".to_string(),
                attributes: FileAttributes {
                    hidden: false,
                    system: false,
                    archive: false,
                    compressed: false,
                    encrypted: false,
                    readonly: false,
                },
            },
            FileItem {
                name: "usr".to_string(),
                path: self.current_path.join("usr"),
                item_type: FileType::Directory,
                size: 4096,
                permissions: FilePermissions {
                    owner_read: true,
                    owner_write: true,
                    owner_execute: true,
                    group_read: true,
                    group_write: false,
                    group_execute: true,
                    other_read: true,
                    other_write: false,
                    other_execute: true,
                    setuid: false,
                    setgid: false,
                    sticky: false,
                },
                owner: "root".to_string(),
                group: "root".to_string(),
                modified: "2024-12-15 10:30:45".to_string(),
                accessed: "2024-12-15 10:30:45".to_string(),
                created: "2024-12-15 10:30:45".to_string(),
                attributes: FileAttributes {
                    hidden: false,
                    system: false,
                    archive: false,
                    compressed: false,
                    encrypted: false,
                    readonly: false,
                },
            },
            FileItem {
                name: "var".to_string(),
                path: self.current_path.join("var"),
                item_type: FileType::Directory,
                size: 4096,
                permissions: FilePermissions {
                    owner_read: true,
                    owner_write: true,
                    owner_execute: true,
                    group_read: true,
                    group_write: false,
                    group_execute: true,
                    other_read: true,
                    other_write: false,
                    other_execute: true,
                    setuid: false,
                    setgid: false,
                    sticky: false,
                },
                owner: "root".to_string(),
                group: "root".to_string(),
                modified: "2024-12-15 10:30:45".to_string(),
                accessed: "2024-12-15 10:30:45".to_string(),
                created: "2024-12-15 10:30:45".to_string(),
                attributes: FileAttributes {
                    hidden: false,
                    system: false,
                    archive: false,
                    compressed: false,
                    encrypted: false,
                    readonly: false,
                },
            },
        ];

        // Filtrar elementos según configuración
        for item in items {
            if self.should_show_item(&item) {
                self.current_items.push(item);
            }
        }
    }

    /// Determinar si mostrar un elemento
    fn should_show_item(&self, item: &FileItem) -> bool {
        if item.attributes.hidden && !self.config.show_hidden_files {
            return false;
        }
        if item.attributes.system && !self.config.show_system_files {
            return false;
        }
        true
    }

    /// Ordenar elementos
    fn sort_items(&mut self) {
        self.current_items.sort_by(|a, b| {
            let comparison = match self.config.sort_by {
                SortBy::Name => a.name.cmp(&b.name),
                SortBy::Size => a.size.cmp(&b.size),
                SortBy::Modified => a.modified.cmp(&b.modified),
                SortBy::Type => a.item_type.cmp(&b.item_type),
                SortBy::Permissions => {
                    let a_perm = format!("{:?}", a.permissions);
                    let b_perm = format!("{:?}", b.permissions);
                    a_perm.cmp(&b_perm)
                },
            };

            match self.config.sort_order {
                SortOrder::Ascending => comparison,
                SortOrder::Descending => comparison.reverse(),
            }
        });
    }

    /// Navegar a directorio
    pub fn navigate_to(&mut self, path: &str) -> Result<(), &'static str> {
        let new_path = if path.starts_with('/') {
            PathBuf::from(path)
        } else {
            self.current_path.join(path)
        };

        // Verificar si el directorio existe (simulado)
        if self.directory_exists(&new_path) {
            // Agregar al historial
            if self.history_index < self.navigation_history.len() - 1 {
                self.navigation_history.truncate(self.history_index + 1);
            }
            self.navigation_history.push(new_path.clone());
            self.history_index = self.navigation_history.len() - 1;
            
            self.current_path = new_path;
            self.refresh_directory();
            self.selection.clear();
        } else {
            return Err(anyhow::anyhow!("Directorio no encontrado: {}", path));
        }
        
        Ok(())
    }

    /// Verificar si un directorio existe (simulado)
    fn directory_exists(&self, path: &PathBuf) -> bool {
        // Simular verificación de existencia
        let path_str = path.to_string_lossy();
        matches!(path_str.as_ref(), "/" | "/bin" | "/dev" | "/etc" | "/home" | "/lib" | "/proc" | "/sys" | "/tmp" | "/usr" | "/var")
    }

    /// Navegar hacia atrás
    pub fn go_back(&mut self) -> Result<(), &'static str> {
        if self.history_index > 0 {
            self.history_index -= 1;
            self.current_path = self.navigation_history[self.history_index].clone();
            self.refresh_directory();
            self.selection.clear();
        }
        Ok(())
    }

    /// Navegar hacia adelante
    pub fn go_forward(&mut self) -> Result<(), &'static str> {
        if self.history_index < self.navigation_history.len() - 1 {
            self.history_index += 1;
            self.current_path = self.navigation_history[self.history_index].clone();
            self.refresh_directory();
            self.selection.clear();
        }
        Ok(())
    }

    /// Ir al directorio padre
    pub fn go_up(&mut self) -> Result<(), &'static str> {
        if let Some(parent) = self.current_path.parent() {
            self.navigate_to(parent.to_string_lossy().as_ref())?;
        }
        Ok(())
    }

    /// Ir al directorio home
    pub fn go_home(&mut self) -> Result<(), &'static str> {
        self.navigate_to("/home/user")?;
        Ok(())
    }

    /// Seleccionar elemento
    pub fn select_item(&mut self, index: usize) {
        if index < self.current_items.len() {
            if let Some(pos) = self.selection.iter().position(|&i| i == index) {
                self.selection.remove(pos);
            } else {
                self.selection.push(index);
            }
        }
    }

    /// Seleccionar todo
    pub fn select_all(&mut self) {
        self.selection = (0..self.current_items.len()).collect();
    }

    /// Deseleccionar todo
    pub fn deselect_all(&mut self) {
        self.selection.clear();
    }

    /// Copiar elementos seleccionados
    pub fn copy_selected(&mut self) -> Result<String, &'static str> {
        if self.selection.is_empty() {
            return Ok("No hay elementos seleccionados".to_string());
        }

        self.state = FileManagerState::Copying;
        let count = self.selection.len();
        Ok(format!("{} elementos copiados", count))
    }

    /// Cortar elementos seleccionados
    pub fn cut_selected(&mut self) -> Result<String, &'static str> {
        if self.selection.is_empty() {
            return Ok("No hay elementos seleccionados".to_string());
        }

        self.state = FileManagerState::Moving;
        let count = self.selection.len();
        Ok(format!("{} elementos cortados", count))
    }

    /// Pegar elementos
    pub fn paste_items(&mut self) -> Result<String, &'static str> {
        match self.state {
            FileManagerState::Copying => {
                let count = self.selection.len();
                self.state = FileManagerState::Normal;
                Ok(format!("{} elementos pegados (copia)", count))
            },
            FileManagerState::Moving => {
                let count = self.selection.len();
                self.state = FileManagerState::Normal;
                Ok(format!("{} elementos pegados (movimiento)", count))
            },
            _ => Ok("No hay elementos en el portapapeles".to_string()),
        }
    }

    /// Eliminar elementos seleccionados
    pub fn delete_selected(&mut self) -> Result<String, &'static str> {
        if self.selection.is_empty() {
            return Ok("No hay elementos seleccionados".to_string());
        }

        if self.config.confirm_deletions {
            // Simular confirmación
            println!("¿Eliminar {} elementos? (y/n)", self.selection.len());
        }

        self.state = FileManagerState::Deleting;
        let count = self.selection.len();
        self.selection.clear();
        Ok(format!("{} elementos eliminados", count))
    }

    /// Renombrar elemento
    pub fn rename_item(&mut self, index: usize, new_name: &str) -> Result<String, &'static str> {
        if index >= self.current_items.len() {
            return Err("Índice de elemento inválido");
        }

        let old_name = self.current_items[index].name.clone();
        self.current_items[index].name = new_name.to_string();
        Ok(format!("'{}' renombrado a '{}'", old_name, new_name))
    }

    /// Crear nuevo directorio
    pub fn create_directory(&mut self, name: &str) -> Result<String, &'static str> {
        let new_dir = FileItem {
            name: name.to_string(),
            path: self.current_path.join(name),
            item_type: FileType::Directory,
            size: 4096,
            permissions: FilePermissions {
                owner_read: true,
                owner_write: true,
                owner_execute: true,
                group_read: true,
                group_write: false,
                group_execute: true,
                other_read: true,
                other_write: false,
                other_execute: true,
                setuid: false,
                setgid: false,
                sticky: false,
            },
            owner: "user".to_string(),
            group: "user".to_string(),
            modified: "2024-12-15 10:30:45".to_string(),
            accessed: "2024-12-15 10:30:45".to_string(),
            created: "2024-12-15 10:30:45".to_string(),
            attributes: FileAttributes {
                hidden: false,
                system: false,
                archive: false,
                compressed: false,
                encrypted: false,
                readonly: false,
            },
        };

        self.current_items.push(new_dir);
        self.sort_items();
        Ok(format!("Directorio '{}' creado", name))
    }

    /// Crear nuevo archivo
    pub fn create_file(&mut self, name: &str) -> Result<String, &'static str> {
        let new_file = FileItem {
            name: name.to_string(),
            path: self.current_path.join(name),
            item_type: FileType::File,
            size: 0,
            permissions: FilePermissions {
                owner_read: true,
                owner_write: true,
                owner_execute: false,
                group_read: true,
                group_write: false,
                group_execute: false,
                other_read: true,
                other_write: false,
                other_execute: false,
                setuid: false,
                setgid: false,
                sticky: false,
            },
            owner: "user".to_string(),
            group: "user".to_string(),
            modified: "2024-12-15 10:30:45".to_string(),
            accessed: "2024-12-15 10:30:45".to_string(),
            created: "2024-12-15 10:30:45".to_string(),
            attributes: FileAttributes {
                hidden: false,
                system: false,
                archive: false,
                compressed: false,
                encrypted: false,
                readonly: false,
            },
        };

        self.current_items.push(new_file);
        self.sort_items();
        Ok(format!("Archivo '{}' creado", name))
    }

    /// Buscar archivos
    pub fn search_files(&mut self, pattern: &str) -> Result<Vec<[^>]*>, &'static str> {
        self.state = FileManagerState::Searching;
        let mut results = Vec::new();

        // Simular búsqueda
        for item in &self.current_items {
            if item.name.contains(pattern) {
                results.push(item.clone());
            }
        }

        Ok(results)
    }

    /// Obtener vista del directorio actual
    pub fn get_directory_view(&self) -> String {
        let mut output = String::new();
        
        match self.view_mode {
            ViewMode::List => {
                output.push_str(&format!("📁 {}\n", self.current_path.display()));
                output.push_str("────────────────────────────────────────────────────────\n");
                for (i, item) in self.current_items.iter().enumerate() {
                    let icon = match item.item_type {
                        FileType::Directory => "📁",
                        FileType::File => "📄",
                        FileType::Symlink => "🔗",
                        _ => "❓",
                    };
                    
                    let selected = if self.selection.contains(&i) { "✓ " } else { "  " };
                    let size_str = if item.item_type == FileType::Directory {
                        "<DIR>".to_string()
                    } else {
                        format!("{}B", item.size)
                    };
                    
                    output.push_str(&format!("{} {} {} {} {}\n", 
                        selected, icon, item.name, size_str, item.modified));
                }
            },
            ViewMode::Details => {
                output.push_str(&format!("📁 {}\n", self.current_path.display()));
                output.push_str("────────────────────────────────────────────────────────────────────────────────────────\n");
                output.push_str("Nombre                    Tamaño    Permisos    Propietario  Grupo    Modificado\n");
                output.push_str("────────────────────────────────────────────────────────────────────────────────────────\n");
                
                for (i, item) in self.current_items.iter().enumerate() {
                    let selected = if self.selection.contains(&i) { "✓ " } else { "  " };
                    let size_str = if item.item_type == FileType::Directory {
                        "<DIR>".to_string()
                    } else {
                        format!("{}B", item.size)
                    };
                    
                    let perms = format!("{}{}{}{}{}{}{}{}{}",
                        if item.permissions.owner_read { "r" } else { "-" },
                        if item.permissions.owner_write { "w" } else { "-" },
                        if item.permissions.owner_execute { "x" } else { "-" },
                        if item.permissions.group_read { "r" } else { "-" },
                        if item.permissions.group_write { "w" } else { "-" },
                        if item.permissions.group_execute { "x" } else { "-" },
                        if item.permissions.other_read { "r" } else { "-" },
                        if item.permissions.other_write { "w" } else { "-" },
                        if item.permissions.other_execute { "x" } else { "-" }
                    );
                    
                    output.push_str(&format!("{}{:<20} {:<8} {:<9} {:<10} {:<8} {}\n",
                        selected, item.name, size_str, perms, item.owner, item.group, item.modified));
                }
            },
            _ => {
                output.push_str("Vista no implementada");
            }
        }
        
        output
    }

    /// Obtener información de estado
    pub fn get_status_info(&self) -> String {
        format!(
            "Directorio: {} | Elementos: {} | Seleccionados: {} | Vista: {:?}",
            self.current_path.display(),
            self.current_items.len(),
            self.selection.len(),
            self.view_mode
        )
    }

    /// Cambiar modo de vista
    pub fn set_view_mode(&mut self, mode: ViewMode) {
        self.view_mode = mode;
    }

    /// Cambiar criterio de ordenamiento
    pub fn set_sort_by(&mut self, sort_by: SortBy) {
        self.config.sort_by = sort_by;
        self.sort_items();
    }

    /// Cambiar orden de clasificación
    pub fn set_sort_order(&mut self, order: SortOrder) {
        self.config.sort_order = order;
        self.sort_items();
    }

    /// Obtener elementos actuales
    pub fn get_current_items(&self) -> &[FileItem] {
        &self.current_items
    }

    /// Obtener elementos seleccionados
    pub fn get_selected_items(&self) -> Vec<&FileItem> {
        self.selection.iter()
            .filter_map(|&i| self.current_items.get(i))
            .collect()
    }

    /// Obtener directorio actual
    pub fn get_current_path(&self) -> &PathBuf {
        &self.current_path
    }

    /// Obtener estado actual
    pub fn get_state(&self) -> &FileManagerState {
        &self.state
    }
}

/// Gestor de gestores de archivos
pub struct FileManagerManager {
    managers: BTreeMap<u32, FileManager>,
    next_manager_id: u32,
}

impl FileManagerManager {
    pub fn new() -> Self {
        Self {
            managers: BTreeMap::new(),
            next_manager_id: 1,
        }
    }

    /// Crear nuevo gestor de archivos
    pub fn create_manager(&mut self, config: FileManagerConfig) -> u32 {
        let manager_id = self.next_manager_id;
        self.next_manager_id += 1;

        let manager = FileManager::new(config);
        self.managers.insert(manager_id, manager);
        manager_id
    }

    /// Obtener gestor de archivos
    pub fn get_manager(&mut self, manager_id: u32) -> Option<&mut FileManager> {
        self.managers.get_mut(&manager_id)
    }

    /// Cerrar gestor de archivos
    pub fn close_manager(&mut self, manager_id: u32) -> bool {
        self.managers.remove(&manager_id).is_some()
    }

    /// Listar gestores de archivos
    pub fn list_managers(&self) -> Vec<u32> {
        self.managers.keys().cloned().collect()
    }
}
