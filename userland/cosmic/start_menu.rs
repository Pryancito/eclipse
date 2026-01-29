//! Menú de inicio para COSMIC Desktop Environment
//!
//! Implementa un menú de inicio moderno con acceso a aplicaciones,
//! búsqueda y configuración del sistema.

// USERLAND: use crate::drivers::framebuffer::{Color, FramebufferDriver};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

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

/// Estado de animación del menú
#[derive(Debug, Clone)]
pub struct StartMenuAnimation {
    pub slide_in_progress: f32,
    pub fade_in_progress: f32,
    pub item_hover_progress: Vec<f32>,
    pub search_focus_progress: f32,
}

/// Configuración del menú
#[derive(Debug, Clone)]
pub struct StartMenuConfig {
    pub width: u32,
    pub height: u32,
    pub item_height: u32,
    pub animation_speed: f32,
    pub max_items_visible: usize,
}

/// Menú de inicio de COSMIC
pub struct StartMenu {
    is_open: bool,
    items: Vec<StartMenuItem>,
    filtered_items: Vec<StartMenuItem>,
    search_query: String,
    selected_index: usize,
    categories: BTreeMap<AppCategory, Vec<usize>>, // Índices de items por categoría
    animation: StartMenuAnimation,
    config: StartMenuConfig,
    current_category: Option<AppCategory>,
    show_categories: bool,
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
            animation: StartMenuAnimation {
                slide_in_progress: 0.0,
                fade_in_progress: 0.0,
                item_hover_progress: Vec::new(),
                search_focus_progress: 0.0,
            },
            config: StartMenuConfig {
                width: 400,
                height: 500,
                item_height: 50,
                animation_speed: 8.0,
                max_items_visible: 8,
            },
            current_category: None,
            show_categories: true,
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
                if item.name.to_lowercase().contains(&query)
                    || item.description.to_lowercase().contains(&query)
                    || item.category.to_lowercase().contains(&query)
                {
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

    /// Actualizar animaciones del menú
    pub fn update_animations(&mut self, delta_time: f32) {
        // Animación de deslizamiento al abrir
        if self.is_open {
            self.animation.slide_in_progress += delta_time * self.config.animation_speed;
            if self.animation.slide_in_progress > 1.0 {
                self.animation.slide_in_progress = 1.0;
            }

            self.animation.fade_in_progress += delta_time * self.config.animation_speed * 1.5;
            if self.animation.fade_in_progress > 1.0 {
                self.animation.fade_in_progress = 1.0;
            }
        } else {
            self.animation.slide_in_progress -= delta_time * self.config.animation_speed * 2.0;
            if self.animation.slide_in_progress < 0.0 {
                self.animation.slide_in_progress = 0.0;
            }

            self.animation.fade_in_progress -= delta_time * self.config.animation_speed * 3.0;
            if self.animation.fade_in_progress < 0.0 {
                self.animation.fade_in_progress = 0.0;
            }
        }

        // Actualizar animaciones de hover de items
        self.animation
            .item_hover_progress
            .resize(self.filtered_items.len(), 0.0);
        for i in 0..self.filtered_items.len() {
            if i == self.selected_index {
                self.animation.item_hover_progress[i] += delta_time * self.config.animation_speed;
                if self.animation.item_hover_progress[i] > 1.0 {
                    self.animation.item_hover_progress[i] = 1.0;
                }
            } else {
                self.animation.item_hover_progress[i] -=
                    delta_time * self.config.animation_speed * 2.0;
                if self.animation.item_hover_progress[i] < 0.0 {
                    self.animation.item_hover_progress[i] = 0.0;
                }
            }
        }
    }

    /// Verificar si el menú está completamente abierto
    pub fn is_fully_open(&self) -> bool {
        self.is_open && self.animation.slide_in_progress >= 1.0
    }

    /// Obtener progreso de animación de deslizamiento
    pub fn get_slide_progress(&self) -> f32 {
        self.animation.slide_in_progress
    }

    /// Obtener progreso de animación de fade
    pub fn get_fade_progress(&self) -> f32 {
        self.animation.fade_in_progress
    }

    /// Cambiar categoría actual
    pub fn set_category(&mut self, category: Option<AppCategory>) {
        self.current_category = category;
        self.filter_items();
        self.selected_index = 0;
    }

    /// Alternar visualización de categorías
    pub fn toggle_categories(&mut self) {
        self.show_categories = !self.show_categories;
    }

    /// Obtener nombre de categoría
    pub fn get_category_name(category: AppCategory) -> &'static str {
        match category {
            AppCategory::System => "Sistema",
            AppCategory::Development => "Desarrollo",
            AppCategory::Graphics => "Gráficos",
            AppCategory::Multimedia => "Multimedia",
            AppCategory::Office => "Oficina",
            AppCategory::Games => "Juegos",
            AppCategory::Utilities => "Utilidades",
        }
    }

    /// Obtener icono de categoría
    pub fn get_category_icon(category: AppCategory) -> &'static str {
        match category {
            AppCategory::System => "⚙️",
            AppCategory::Development => "💻",
            AppCategory::Graphics => "🎨",
            AppCategory::Multimedia => "🎵",
            AppCategory::Office => "📄",
            AppCategory::Games => "🎮",
            AppCategory::Utilities => "🔧",
        }
    }

    /// Obtener configuración del menú
    pub fn get_config(&self) -> &StartMenuConfig {
        &self.config
    }

    /// Actualizar configuración del menú
    pub fn update_config(&mut self, config: StartMenuConfig) {
        self.config = config;
    }

    /// Obtener estadísticas del menú
    pub fn get_stats(&self) -> (usize, usize, usize) {
        (
            self.items.len(),
            self.filtered_items.len(),
            self.categories.len(),
        )
    }
}

/// Renderizar el menú de inicio mejorado con animaciones
pub fn render_start_menu(fb: &mut FramebufferDriver, menu: &StartMenu) -> Result<(), String> {
    if !menu.is_open || menu.animation.slide_in_progress <= 0.0 {
        return Ok(());
    }

    let screen_width = fb.info.width;
    let screen_height = fb.info.height;

    // Posición del menú con animación de deslizamiento
    let menu_width = menu.config.width;
    let menu_height = menu.config.height;
    let base_menu_x = 50;
    let base_menu_y = 50;

    // Aplicar animación de deslizamiento
    let slide_offset = (1.0 - menu.animation.slide_in_progress) * 100.0;
    let menu_x = (base_menu_x as f32 + slide_offset) as u32;
    let menu_y = (base_menu_y as f32 + slide_offset * 0.5) as u32;

    // Colores del tema espacial mejorados
    let background_color = Color::from_hex(0x001122);
    let panel_color = Color::from_hex(0x002244);
    let border_color = Color::from_hex(0x0066aa);
    let text_color = Color::from_hex(0xffffff);
    let selected_color = Color::from_hex(0x004488);
    let search_color = Color::from_hex(0x003366);
    let glow_color = Color::from_hex(0x00aaff);

    // Aplicar efecto de fade
    let fade_alpha = menu.animation.fade_in_progress;
    let faded_panel = Color {
        r: (panel_color.r as f32 * fade_alpha) as u8,
        g: (panel_color.g as f32 * fade_alpha) as u8,
        b: (panel_color.b as f32 * fade_alpha) as u8,
        a: 255,
    };

    // Fondo semitransparente con animación
    let overlay_color = Color {
        r: (Color::from_hex(0x000011).r as f32 * fade_alpha * 0.8) as u8,
        g: (Color::from_hex(0x000011).g as f32 * fade_alpha * 0.8) as u8,
        b: (Color::from_hex(0x000011).b as f32 * fade_alpha * 0.8) as u8,
        a: 255,
    };
    fb.draw_rect(0, 0, screen_width, screen_height, overlay_color);

    // Efecto de glow para el panel
    if menu.animation.slide_in_progress > 0.5 {
        let glow_intensity = (menu.animation.slide_in_progress - 0.5) * 2.0;
        let glow_color_fade = Color {
            r: (glow_color.r as f32 * glow_intensity * 0.3) as u8,
            g: (glow_color.g as f32 * glow_intensity * 0.3) as u8,
            b: (glow_color.b as f32 * glow_intensity * 0.3) as u8,
            a: (glow_color.a as f32 * glow_intensity * 0.2) as u8,
        };
        fb.draw_rect(
            menu_x - 5,
            menu_y - 5,
            menu_width + 10,
            menu_height + 10,
            glow_color_fade,
        );
    }

    // Panel principal del menú
    fb.draw_rect(menu_x, menu_y, menu_width, menu_height, faded_panel);
    fb.draw_rect(menu_x, menu_y, menu_width, menu_height, border_color);

    // Título con efecto de brillo
    let title_color = Color {
        r: (text_color.r as f32 * (0.8 + fade_alpha * 0.2)) as u8,
        g: (text_color.g as f32 * (0.8 + fade_alpha * 0.2)) as u8,
        b: (text_color.b as f32 * (0.8 + fade_alpha * 0.2)) as u8,
        a: 255,
    };
    fb.write_text_kernel_typing(menu_x + 20, menu_y + 20, "🚀 Eclipse OS", title_color);
    fb.write_text_kernel_typing(
        menu_x + 20,
        menu_y + 40,
        "Applications",
        Color::from_hex(0xaaaaaa),
    );

    // Barra de búsqueda mejorada
    let search_y = menu_y + 70;
    let search_bg_color = Color {
        r: (search_color.r as f32 * fade_alpha) as u8,
        g: (search_color.g as f32 * fade_alpha) as u8,
        b: (search_color.b as f32 * fade_alpha) as u8,
        a: 255,
    };
    fb.draw_rect(menu_x + 20, search_y, menu_width - 40, 30, search_bg_color);
    fb.draw_rect(menu_x + 20, search_y, menu_width - 40, 30, border_color);

    let search_text = if menu.search_query.is_empty() {
        "🔍 Search applications..."
    } else {
        &menu.search_query
    };
    fb.write_text_kernel_typing(menu_x + 30, search_y + 8, search_text, text_color);

    // Lista de aplicaciones con animaciones
    let list_y = search_y + 50;
    let list_height = menu_height - 120;
    let item_height = menu.config.item_height;
    let max_visible = (list_height / item_height).min(menu.config.max_items_visible as u32);

    // Fondo de la lista
    fb.draw_rect(
        menu_x + 20,
        list_y,
        menu_width - 40,
        list_height,
        background_color,
    );

    // Renderizar elementos visibles con animaciones
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

        // Obtener progreso de animación para este item
        let hover_progress = if item_index < menu.animation.item_hover_progress.len() {
            menu.animation.item_hover_progress[item_index]
        } else {
            0.0
        };

        // Calcular colores con animación
        let mut item_bg_color = background_color;
        let mut item_text_color = text_color;

        if item_index == menu.selected_index {
            // Aplicar efecto de selección con animación
            item_bg_color = Color {
                r: (selected_color.r as f32 * (0.7 + hover_progress * 0.3)) as u8,
                g: (selected_color.g as f32 * (0.7 + hover_progress * 0.3)) as u8,
                b: (selected_color.b as f32 * (0.7 + hover_progress * 0.3)) as u8,
                a: 255,
            };

            // Efecto de glow para item seleccionado
            if hover_progress > 0.0 {
                let glow_color_fade = Color {
                    r: (glow_color.r as f32 * hover_progress * 0.4) as u8,
                    g: (glow_color.g as f32 * hover_progress * 0.4) as u8,
                    b: (glow_color.b as f32 * hover_progress * 0.4) as u8,
                    a: (glow_color.a as f32 * hover_progress * 0.3) as u8,
                };
                fb.draw_rect(
                    menu_x + 23,
                    item_y + 3,
                    menu_width - 46,
                    item_height - 6,
                    glow_color_fade,
                );
            }

            item_text_color = Color {
                r: (text_color.r as f32 * (0.9 + hover_progress * 0.1)) as u8,
                g: (text_color.g as f32 * (0.9 + hover_progress * 0.1)) as u8,
                b: (text_color.b as f32 * (0.9 + hover_progress * 0.1)) as u8,
                a: 255,
            };
        }

        // Fondo del item
        fb.draw_rect(
            menu_x + 25,
            item_y + 5,
            menu_width - 50,
            item_height - 10,
            item_bg_color,
        );

        // Icono con efecto de brillo si está seleccionado
        let icon_color = if item_index == menu.selected_index {
            Color {
                r: (text_color.r as f32 * 1.1) as u8,
                g: (text_color.g as f32 * 1.1) as u8,
                b: (text_color.b as f32 * 1.1) as u8,
                a: 255,
            }
        } else {
            text_color
        };
        fb.write_text_kernel_typing(menu_x + 30, item_y + 10, &item.icon, icon_color);

        // Nombre de la aplicación
        fb.write_text_kernel_typing(menu_x + 70, item_y + 10, &item.name, item_text_color);

        // Descripción
        let desc_color = Color {
            r: (Color::from_hex(0xaaaaaa).r as f32 * fade_alpha) as u8,
            g: (Color::from_hex(0xaaaaaa).g as f32 * fade_alpha) as u8,
            b: (Color::from_hex(0xaaaaaa).b as f32 * fade_alpha) as u8,
            a: 255,
        };
        fb.write_text_kernel_typing(menu_x + 70, item_y + 25, &item.description, desc_color);
    }

    // Información de estado mejorada
    let status_text = format!("{} applications", menu.filtered_items.len());
    let status_color = Color {
        r: (Color::from_hex(0x666666).r as f32 * fade_alpha) as u8,
        g: (Color::from_hex(0x666666).g as f32 * fade_alpha) as u8,
        b: (Color::from_hex(0x666666).b as f32 * fade_alpha) as u8,
        a: 255,
    };
    fb.write_text_kernel_typing(
        menu_x + 20,
        menu_y + menu_height - 30,
        &status_text,
        status_color,
    );

    // Instrucciones mejoradas
    fb.write_text_kernel_typing(
        menu_x + 20,
        menu_y + menu_height - 15,
        "Enter: Open | ESC: Close | ↑↓: Navigate",
        status_color,
    );

    // Indicador de categoría actual si está filtrado
    if let Some(category) = menu.current_category {
        let category_text = format!("Categoría: {}", StartMenu::get_category_name(category));
        fb.write_text_kernel_typing(
            menu_x + 20,
            menu_y + menu_height - 45,
            &category_text,
            status_color,
        );
    }

    Ok(())
}

/// Procesar eventos de entrada del menú de inicio
pub fn handle_start_menu_input(menu: &mut StartMenu, key_code: u32) -> Option<String> {
    if !menu.is_open {
        return None;
    }

    match key_code {
        0x01 => {
            // ESC
            menu.close();
            None
        }
        0x48 => {
            // Up
            menu.move_selection(-1);
            None
        }
        0x50 => {
            // Down
            menu.move_selection(1);
            None
        }
        0x1C => {
            // Enter
            let command = menu.execute_selected();
            menu.close();
            command
        }
        0x0E => {
            // Backspace
            if !menu.search_query.is_empty() {
                menu.search_query.pop();
                menu.filter_items();
            }
            None
        }
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
