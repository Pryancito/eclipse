//! Gestor de archivos gráfico para Eclipse OS
//! 
//! Proporciona una interfaz gráfica moderna para la gestión de archivos
//! con soporte para operaciones de arrastrar y soltar, vista previa y más.

use alloc::vec::Vec;
use alloc::vec;
use alloc::string::{String, ToString};
use alloc::collections::BTreeMap;
use alloc::format;

/// Tipo de archivo
#[derive(Debug, Clone, PartialEq)]
pub enum FileType {
    Directory,
    File,
    Image,
    Text,
    Archive,
    Executable,
    Unknown,
}

/// Información de archivo
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub name: String,
    pub path: String,
    pub file_type: FileType,
    pub size: u64,
    pub modified: String,
    pub permissions: String,
    pub is_hidden: bool,
    pub is_readonly: bool,
}

/// Modo de vista
#[derive(Debug, Clone, PartialEq)]
pub enum ViewMode {
    List,
    Grid,
    Details,
    Tree,
}

/// Gestor de archivos gráfico
pub struct FileManagerGui {
    pub current_path: String,
    pub files: Vec<FileInfo>,
    pub selected_files: Vec<String>,
    pub view_mode: ViewMode,
    pub sort_by: String,
    pub sort_ascending: bool,
    pub show_hidden: bool,
    pub window_width: u32,
    pub window_height: u32,
    pub sidebar_width: u32,
    pub status_bar_height: u32,
}

impl FileManagerGui {
    pub fn new() -> Self {
        Self {
            current_path: "/".to_string(),
            files: Vec::new(),
            selected_files: Vec::new(),
            view_mode: ViewMode::List,
            sort_by: "name".to_string(),
            sort_ascending: true,
            show_hidden: false,
            window_width: 1024,
            window_height: 768,
            sidebar_width: 200,
            status_bar_height: 30,
        }
    }
    
    /// Ejecutar el gestor de archivos
    pub fn run(&mut self) -> Result<(), &'static str> {
        self.show_welcome();
        self.load_directory("/")?;
        self.render_interface();
        Ok(())
    }
    
    fn show_welcome(&self) {
        self.print_info("╔══════════════════════════════════════════════════════════════╗");
        self.print_info("║                                                              ║");
        self.print_info("║                ECLIPSE FILE MANAGER                          ║");
        self.print_info("║                                                              ║");
        self.print_info("║  Gestor de archivos gráfico con interfaz moderna           ║");
        self.print_info("║  Soporte para operaciones avanzadas de archivos            ║");
        self.print_info("║                                                              ║");
        self.print_info("╚══════════════════════════════════════════════════════════════╝");
        self.print_info("");
    }
    
    fn load_directory(&mut self, path: &str) -> Result<(), &'static str> {
        self.current_path = path.to_string();
        self.files.clear();
        
        // Simular carga de archivos del directorio
        self.files = self.get_directory_contents(path);
        
        // Ordenar archivos
        self.sort_files();
        
        Ok(())
    }
    
    fn get_directory_contents(&self, path: &str) -> Vec<FileInfo> {
        // Simular contenido de directorio
        match path {
            "/" => {
                vec![
                    FileInfo {
                        name: "welcome.txt".to_string(),
                        path: "/welcome.txt".to_string(),
                        file_type: FileType::Text,
                        size: 123,
                        modified: "2024-01-01 00:00:00".to_string(),
                        permissions: "-rw-r--r--".to_string(),
                        is_hidden: false,
                        is_readonly: false,
                    },
                    FileInfo {
                        name: "config.ini".to_string(),
                        path: "/config.ini".to_string(),
                        file_type: FileType::Text,
                        size: 456,
                        modified: "2024-01-01 00:00:00".to_string(),
                        permissions: "-rw-r--r--".to_string(),
                        is_hidden: false,
                        is_readonly: false,
                    },
                    FileInfo {
                        name: "system.log".to_string(),
                        path: "/system.log".to_string(),
                        file_type: FileType::Text,
                        size: 789,
                        modified: "2024-01-01 00:00:00".to_string(),
                        permissions: "-rw-r--r--".to_string(),
                        is_hidden: false,
                        is_readonly: false,
                    },
                    FileInfo {
                        name: "system".to_string(),
                        path: "/system".to_string(),
                        file_type: FileType::Directory,
                        size: 4096,
                        modified: "2024-01-01 00:00:00".to_string(),
                        permissions: "drwxr-xr-x".to_string(),
                        is_hidden: false,
                        is_readonly: false,
                    },
                    FileInfo {
                        name: "users".to_string(),
                        path: "/users".to_string(),
                        file_type: FileType::Directory,
                        size: 4096,
                        modified: "2024-01-01 00:00:00".to_string(),
                        permissions: "drwxr-xr-x".to_string(),
                        is_hidden: false,
                        is_readonly: false,
                    },
                    FileInfo {
                        name: ".hidden_file".to_string(),
                        path: "/.hidden_file".to_string(),
                        file_type: FileType::File,
                        size: 64,
                        modified: "2024-01-01 00:00:00".to_string(),
                        permissions: "-rw-r--r--".to_string(),
                        is_hidden: true,
                        is_readonly: false,
                    },
                ]
            },
            "/system" => {
                vec![
                    FileInfo {
                        name: "kernel".to_string(),
                        path: "/system/kernel".to_string(),
                        file_type: FileType::Executable,
                        size: 2048,
                        modified: "2024-01-01 00:00:00".to_string(),
                        permissions: "-rwxr-xr-x".to_string(),
                        is_hidden: false,
                        is_readonly: false,
                    },
                    FileInfo {
                        name: "drivers".to_string(),
                        path: "/system/drivers".to_string(),
                        file_type: FileType::Directory,
                        size: 4096,
                        modified: "2024-01-01 00:00:00".to_string(),
                        permissions: "drwxr-xr-x".to_string(),
                        is_hidden: false,
                        is_readonly: false,
                    },
                ]
            },
            "/users" => {
                vec![
                    FileInfo {
                        name: "eclipse".to_string(),
                        path: "/users/eclipse".to_string(),
                        file_type: FileType::Directory,
                        size: 4096,
                        modified: "2024-01-01 00:00:00".to_string(),
                        permissions: "drwxr-xr-x".to_string(),
                        is_hidden: false,
                        is_readonly: false,
                    },
                ]
            },
            _ => Vec::new(),
        }
    }
    
    fn render_interface(&self) {
        self.render_title_bar();
        self.render_menu_bar();
        self.render_toolbar();
        self.render_sidebar();
        self.render_main_area();
        self.render_status_bar();
    }
    
    fn render_title_bar(&self) {
        self.print_info("┌─ Eclipse File Manager ──────────────────────────────────────────────┐");
    }
    
    fn render_menu_bar(&self) {
        self.print_info("│ Archivo  Editar  Ver  Herramientas  Ayuda                          │");
        self.print_info("├─────────────────────────────────────────────────────────────────────┤");
    }
    
    fn render_toolbar(&self) {
        self.print_info("│ [←] [→] [↑] [🔄] [📁] [📄] [✂️] [📋] [🗑️] [🔍] [⚙️]                │");
        self.print_info("├─────────────────────────────────────────────────────────────────────┤");
    }
    
    fn render_sidebar(&self) {
        self.print_info("│ LUGARES                    │ ARCHIVOS Y CARPETAS                    │");
        self.print_info("│                            │                                        │");
        self.print_info("│ 📁 Inicio                   │                                        │");
        self.print_info("│ 📁 Documentos               │                                        │");
        self.print_info("│ 📁 Descargas                │                                        │");
        self.print_info("│ 📁 Imágenes                 │                                        │");
        self.print_info("│ 📁 Música                   │                                        │");
        self.print_info("│ 📁 Videos                   │                                        │");
        self.print_info("│ 📁 Escritorio               │                                        │");
        self.print_info("│                            │                                        │");
        self.print_info("│ DISPOSITIVOS                │                                        │");
        self.print_info("│                            │                                        │");
        self.print_info("│ 💾 Disco Local (C:)         │                                        │");
        self.print_info("│ 💿 CD/DVD                   │                                        │");
        self.print_info("│                            │                                        │");
        self.print_info("│ RED                         │                                        │");
        self.print_info("│                            │                                        │");
        self.print_info("│ 🌐 Red Local                │                                        │");
        self.print_info("│                            │                                        │");
        self.print_info("├────────────────────────────┼────────────────────────────────────────┤");
    }
    
    fn render_main_area(&self) {
        self.print_info("│ RUTA: /                                                                 │");
        self.print_info("├─────────────────────────────────────────────────────────────────────┤");
        
        // Mostrar archivos según el modo de vista
        match self.view_mode {
            ViewMode::List => self.render_list_view(),
            ViewMode::Grid => self.render_grid_view(),
            ViewMode::Details => self.render_details_view(),
            ViewMode::Tree => self.render_tree_view(),
        }
    }
    
    fn render_list_view(&self) {
        self.print_info("│ Nombre                Tamaño    Modificado        Permisos          │");
        self.print_info("├─────────────────────────────────────────────────────────────────────┤");
        
        for file in &self.files {
            if !self.show_hidden && file.is_hidden {
                continue;
            }
            
            let icon = self.get_file_icon(&file.file_type);
            let size_str = self.format_size(file.size);
            let name = if file.name.len() > 20 {
                format!("{}...", &file.name[..17])
            } else {
                format!("{:<20}", file.name)
            };
            
            self.print_info(&format!("│ {} {:<20} {:<8} {:<16} {:<10} │",
                icon, name, size_str, file.modified, file.permissions));
        }
    }
    
    fn render_grid_view(&self) {
        self.print_info("│                                                                     │");
        
        let items_per_row = 6;
        let mut i = 0;
        
        for file in &self.files {
            if !self.show_hidden && file.is_hidden {
                continue;
            }
            
            if i % items_per_row == 0 {
                if i > 0 {
                    self.print_info("│                                                                     │");
                }
                self.print_info("│ ");
            }
            
            let icon = self.get_file_icon(&file.file_type);
            let name = if file.name.len() > 8 {
                format!("{}...", &file.name[..5])
            } else {
                file.name.clone()
            };
            
            self.print_info(&format!("{} {:<8} ", icon, name));
            
            i += 1;
        }
        
        if i % items_per_row != 0 {
            self.print_info("│");
        }
    }
    
    fn render_details_view(&self) {
        self.print_info("│ Nombre                Tamaño    Tipo      Modificado        Permisos │");
        self.print_info("├─────────────────────────────────────────────────────────────────────┤");
        
        for file in &self.files {
            if !self.show_hidden && file.is_hidden {
                continue;
            }
            
            let icon = self.get_file_icon(&file.file_type);
            let size_str = self.format_size(file.size);
            let type_str = self.get_file_type_string(&file.file_type);
            let name = if file.name.len() > 20 {
                format!("{}...", &file.name[..17])
            } else {
                format!("{:<20}", file.name)
            };
            
            self.print_info(&format!("│ {} {:<20} {:<8} {:<8} {:<16} {:<10} │",
                icon, name, size_str, type_str, file.modified, file.permissions));
        }
    }
    
    fn render_tree_view(&self) {
        self.print_info("│ 📁 /                                                                  │");
        
        for file in &self.files {
            if !self.show_hidden && file.is_hidden {
                continue;
            }
            
            let icon = self.get_file_icon(&file.file_type);
            let indent = "  ";
            
            self.print_info(&format!("│ {}├─ {} {}                                                      │",
                indent, icon, file.name));
        }
    }
    
    fn render_status_bar(&self) {
        let total_files = self.files.len();
        let selected_count = self.selected_files.len();
        let total_size = self.files.iter().map(|f| f.size).sum::<u64>();
        
        self.print_info("├─────────────────────────────────────────────────────────────────────┤");
        self.print_info(&format!("│ {} archivos, {} seleccionados, {} total                    │",
            total_files, selected_count, self.format_size(total_size)));
        self.print_info("└─────────────────────────────────────────────────────────────────────┘");
    }
    
    fn get_file_icon(&self, file_type: &FileType) -> &'static str {
        match file_type {
            FileType::Directory => "📁",
            FileType::File => "📄",
            FileType::Image => "🖼️",
            FileType::Text => "📝",
            FileType::Archive => "📦",
            FileType::Executable => "⚙️",
            FileType::Unknown => "❓",
        }
    }
    
    fn get_file_type_string(&self, file_type: &FileType) -> &'static str {
        match file_type {
            FileType::Directory => "Carpeta",
            FileType::File => "Archivo",
            FileType::Image => "Imagen",
            FileType::Text => "Texto",
            FileType::Archive => "Archivo",
            FileType::Executable => "Ejecutable",
            FileType::Unknown => "Desconocido",
        }
    }
    
    fn format_size(&self, size: u64) -> String {
        if size < 1024 {
            format!("{} B", size)
        } else if size < 1024 * 1024 {
            format!("{} KB", size / 1024)
        } else if size < 1024 * 1024 * 1024 {
            format!("{} MB", size / (1024 * 1024))
        } else {
            format!("{} GB", size / (1024 * 1024 * 1024))
        }
    }
    
    fn sort_files(&mut self) {
        let sort_by = self.sort_by.clone();
        let sort_ascending = self.sort_ascending;
        self.files.sort_by(|a, b| {
            let result = match sort_by.as_str() {
                "name" => a.name.cmp(&b.name),
                "size" => a.size.cmp(&b.size),
                "modified" => a.modified.cmp(&b.modified),
                "type" => {
                    let a_type = Self::get_file_type_string_static(&Self::file_type_to_string(&a.file_type));
                    let b_type = Self::get_file_type_string_static(&Self::file_type_to_string(&b.file_type));
                    a_type.cmp(&b_type)
                },
                _ => a.name.cmp(&b.name),
            };
            
            if sort_ascending {
                result
            } else {
                result.reverse()
            }
        });
    }
    
    fn file_type_to_string(file_type: &FileType) -> String {
        match file_type {
            FileType::Directory => "Directorio".to_string(),
            FileType::File => "Archivo".to_string(),
            FileType::Image => "Imagen".to_string(),
            FileType::Text => "Texto".to_string(),
            FileType::Archive => "Archivo".to_string(),
            FileType::Executable => "Ejecutable".to_string(),
            FileType::Unknown => "Desconocido".to_string(),
        }
    }
    
    fn get_file_type_string_static(file_type: &str) -> String {
        match file_type {
            "txt" | "md" | "log" => "Documento".to_string(),
            "jpg" | "png" | "gif" | "bmp" => "Imagen".to_string(),
            "mp4" | "avi" | "mkv" => "Video".to_string(),
            "mp3" | "wav" | "flac" => "Audio".to_string(),
            "zip" | "tar" | "gz" => "Archivo".to_string(),
            "exe" | "bin" => "Ejecutable".to_string(),
            _ => "Desconocido".to_string(),
        }
    }
    
    /// Cambiar directorio
    pub fn change_directory(&mut self, path: &str) -> Result<(), &'static str> {
        self.load_directory(path)
    }
    
    /// Seleccionar archivo
    pub fn select_file(&mut self, filename: &str) {
        if !self.selected_files.contains(&filename.to_string()) {
            self.selected_files.push(filename.to_string());
        }
    }
    
    /// Deseleccionar archivo
    pub fn deselect_file(&mut self, filename: &str) {
        self.selected_files.retain(|f| f != filename);
    }
    
    /// Cambiar modo de vista
    pub fn set_view_mode(&mut self, mode: ViewMode) {
        self.view_mode = mode;
    }
    
    /// Ordenar archivos
    pub fn sort_by(&mut self, field: &str, ascending: bool) {
        self.sort_by = field.to_string();
        self.sort_ascending = ascending;
        self.sort_files();
    }
    
    /// Alternar archivos ocultos
    pub fn toggle_hidden_files(&mut self) {
        self.show_hidden = !self.show_hidden;
    }
    
    fn print_info(&self, text: &str) {
        // En una implementación real, esto renderizaría en la interfaz gráfica
        // Por ahora solo simulamos
    }
}

/// Función principal para ejecutar el gestor de archivos
pub fn run() -> Result<(), &'static str> {
    let mut file_manager = FileManagerGui::new();
    file_manager.run()
}
