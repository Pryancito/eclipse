//! Menú de inicio para COSMIC Desktop Environment
//! 
//! Implementa un menú de inicio moderno con acceso a aplicaciones,
//! búsqueda y configuración del sistema.

use crate::drivers::framebuffer::{FramebufferDriver, Color};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use alloc::format;

/// Aplicación en el menú de inicio
#[derive(Debug, Clone)]
pub struct StartMenuItem {
    pub name: String,
    pub description: String,
    pub icon: String, // Path al icono o emoji
    pub command: String,
    pub category: String,
}

/// Categorías de aplicaciones
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AppCategory {
    System,
    Development,
    Graphics,
    Multimedia,
    Office,
    Games,
    Utilities,
}

/// Menú de inicio de COSMIC
pub struct StartMenu {
    is_open: bool,
    items: Vec<StartMenuItem>,
    filtered_items: Vec<StartMenuItem>,
    search_query: String,
    selected_index: usize,
    categories: BTreeMap<AppCategory, Vec<usize>>, // Índices de items por categoría
}

impl StartMenu {
    pub fn new() -> Self {
        let mut menu = Self {
            is_open: false,
            items: Vec::new(),
            filtered_items: Vec::new(),
            search_query: String::new(),
            selected_index: 0,
            categories: BTreeMap::new(),
        };
        
        menu.initialize_default_apps();
        menu
    }

    /// Inicializar aplicaciones por defecto
    fn initialize_default_apps(&mut self) {
        let mut default_apps = Vec::new();
        
        default_apps.push(StartMenuItem {
            name: "Calculator".to_string(),
            description: "Calculadora científica".to_string(),
            icon: "🧮".to_string(),
            command: "eclipse-calculator".to_string(),
            category: "Utilities".to_string(),
        });
        
        default_apps.push(StartMenuItem {
            name: "Text Editor".to_string(),
            description: "Editor de texto simple".to_string(),
            icon: "📝".to_string(),
            command: "eclipse-text-editor".to_string(),
            category: "Development".to_string(),
        });
        
        default_apps.push(StartMenuItem {
            name: "File Manager".to_string(),
            description: "Explorador de archivos".to_string(),
            icon: "📁".to_string(),
            command: "eclipse-file-manager".to_string(),
            category: "System".to_string(),
        });
        
        default_apps.push(StartMenuItem {
            name: "Terminal".to_string(),
            description: "Terminal de línea de comandos".to_string(),
            icon: "💻".to_string(),
            command: "eclipse-terminal".to_string(),
            category: "System".to_string(),
        });
        
        default_apps.push(StartMenuItem {
            name: "Settings".to_string(),
            description: "Configuración del sistema".to_string(),
            icon: "⚙️".to_string(),
            command: "eclipse-settings".to_string(),
            category: "System".to_string(),
        });
        
        default_apps.push(StartMenuItem {
            name: "About".to_string(),
            description: "Acerca de Eclipse OS".to_string(),
            icon: "🚀".to_string(),
            command: "eclipse-about".to_string(),
            category: "System".to_string(),
        });

        for (index, app) in default_apps.into_iter().enumerate() {
            self.items.push(app);
            self.filtered_items.push(self.items[index].clone());
        }
    }

    /// Abrir menú de inicio
    pub fn open(&mut self) {
        self.is_open = true;
        self.search_query.clear();
        self.filter_items();
        self.selected_index = 0;
    }

    /// Cerrar menú de inicio
    pub fn close(&mut self) {
        self.is_open = false;
        self.search_query.clear();
        self.selected_index = 0;
    }

    /// Alternar estado del menú
    pub fn toggle(&mut self) {
        if self.is_open {
            self.close();
        } else {
            self.open();
        }
    }

    /// Verificar si el menú está abierto
    pub fn is_open(&self) -> bool {
        self.is_open
    }

    /// Filtrar elementos según búsqueda
    fn filter_items(&mut self) {
        if self.search_query.is_empty() {
            self.filtered_items = self.items.clone();
        } else {
            self.filtered_items.clear();
            let query = self.search_query.to_lowercase();
            
            for item in &self.items {
                if item.name.to_lowercase().contains(&query) ||
                   item.description.to_lowercase().contains(&query) ||
                   item.category.to_lowercase().contains(&query) {
                    self.filtered_items.push(item.clone());
                }
            }
        }
        
        // Ajustar índice seleccionado
        if self.selected_index >= self.filtered_items.len() {
            self.selected_index = 0;
        }
    }

    /// Actualizar consulta de búsqueda
    pub fn update_search(&mut self, query: String) {
        self.search_query = query;
        self.filter_items();
        self.selected_index = 0;
    }

    /// Mover selección
    pub fn move_selection(&mut self, direction: i32) {
        if !self.filtered_items.is_empty() {
            let new_index = (self.selected_index as i32 + direction).max(0) as usize;
            self.selected_index = new_index.min(self.filtered_items.len() - 1);
        }
    }

    /// Ejecutar aplicación seleccionada
    pub fn execute_selected(&self) -> Option<String> {
        if self.selected_index < self.filtered_items.len() {
            Some(self.filtered_items[self.selected_index].command.clone())
        } else {
            None
        }
    }

    /// Obtener elemento seleccionado
    pub fn get_selected_item(&self) -> Option<&StartMenuItem> {
        if self.selected_index < self.filtered_items.len() {
            Some(&self.filtered_items[self.selected_index])
        } else {
            None
        }
    }
}

/// Renderizar el menú de inicio
pub fn render_start_menu(fb: &mut FramebufferDriver, menu: &StartMenu) -> Result<(), String> {
    if !menu.is_open {
        return Ok(());
    }

    let screen_width = fb.info.width;
    let screen_height = fb.info.height;
    
    // Dimensiones del menú
    let menu_width = 400;
    let menu_height = 500;
    let menu_x = 50;
    let menu_y = 50;
    
    // Colores del tema espacial
    let background_color = Color::from_hex(0x001122);
    let panel_color = Color::from_hex(0x002244);
    let border_color = Color::from_hex(0x0066aa);
    let text_color = Color::from_hex(0xffffff);
    let selected_color = Color::from_hex(0x004488);
    let search_color = Color::from_hex(0x003366);
    
    // Fondo semitransparente (simulado)
    fb.draw_rect(0, 0, screen_width, screen_height, Color::from_hex(0x000011));
    
    // Panel principal del menú
    fb.draw_rect(menu_x, menu_y, menu_width, menu_height, panel_color);
    fb.draw_rect(menu_x, menu_y, menu_width, menu_height, border_color);
    
    // Título
    fb.draw_text_simple(menu_x + 20, menu_y + 20, "Eclipse OS", text_color);
    fb.draw_text_simple(menu_x + 20, menu_y + 40, "Applications", Color::from_hex(0xaaaaaa));
    
    // Barra de búsqueda
    let search_y = menu_y + 70;
    fb.draw_rect(menu_x + 20, search_y, menu_width - 40, 30, search_color);
    fb.draw_rect(menu_x + 20, search_y, menu_width - 40, 30, border_color);
    
    let search_text = if menu.search_query.is_empty() {
        "Search applications..."
    } else {
        &menu.search_query
    };
    fb.draw_text_simple(menu_x + 30, search_y + 8, search_text, text_color);
    
    // Lista de aplicaciones
    let list_y = search_y + 50;
    let list_height = menu_height - 120;
    let item_height = 40;
    let max_visible = list_height / item_height;
    
    // Fondo de la lista
    fb.draw_rect(menu_x + 20, list_y, menu_width - 40, list_height, background_color);
    
    // Renderizar elementos visibles
    let start_index = if menu.selected_index >= max_visible as usize {
        menu.selected_index - max_visible as usize + 1
    } else {
        0
    };
    
    for i in 0..max_visible {
        let item_index = start_index + i as usize;
        if item_index >= menu.filtered_items.len() {
            break;
        }
        
        let item = &menu.filtered_items[item_index];
        let item_y = list_y + i * item_height;
        
        // Resaltar elemento seleccionado
        if item_index == menu.selected_index {
            fb.draw_rect(menu_x + 25, item_y + 5, menu_width - 50, item_height - 10, selected_color);
        }
        
        // Icono
        fb.draw_text_simple(menu_x + 30, item_y + 10, &item.icon, text_color);
        
        // Nombre de la aplicación
        fb.draw_text_simple(menu_x + 70, item_y + 10, &item.name, text_color);
        
        // Descripción
        fb.draw_text_simple(menu_x + 70, item_y + 25, &item.description, Color::from_hex(0xaaaaaa));
    }
    
    // Información de estado
    let status_text = format!("{} applications", menu.filtered_items.len());
    fb.draw_text_simple(menu_x + 20, menu_y + menu_height - 30, &status_text, Color::from_hex(0x666666));
    
    // Instrucciones
    fb.draw_text_simple(menu_x + 20, menu_y + menu_height - 15, "Enter: Open | ESC: Close | ↑↓: Navigate", Color::from_hex(0x666666));
    
    Ok(())
}

/// Procesar eventos de entrada del menú de inicio
pub fn handle_start_menu_input(menu: &mut StartMenu, key_code: u32) -> Option<String> {
    if !menu.is_open {
        return None;
    }
    
    match key_code {
        0x01 => { // ESC
            menu.close();
            None
        },
        0x48 => { // Up
            menu.move_selection(-1);
            None
        },
        0x50 => { // Down
            menu.move_selection(1);
            None
        },
        0x1C => { // Enter
            let command = menu.execute_selected();
            menu.close();
            command
        },
        0x0E => { // Backspace
            if !menu.search_query.is_empty() {
                menu.search_query.pop();
                menu.filter_items();
            }
            None
        },
        _ => {
            // Procesar caracteres para búsqueda
            if let Some(ch) = keycode_to_char(key_code) {
                menu.search_query.push(ch);
                menu.filter_items();
            }
            None
        }
    }
}

/// Convertir código de tecla a carácter (simplificado)
fn keycode_to_char(key_code: u32) -> Option<char> {
    match key_code {
        // Letras a-z
        0x1E..=0x26 => Some((b'a' + (key_code - 0x1E) as u8) as char),
        0x2C..=0x32 => Some((b'z' - (0x32 - key_code) as u8) as char),
        // Números
        0x02..=0x0B => Some((b'0' + ((key_code - 0x02 + 1) % 10) as u8) as char),
        0x0C => Some('0'),
        // Espacio
        0x39 => Some(' '),
        _ => None,
    }
}
