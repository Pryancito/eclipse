//! Sistema de iconos y recursos gráficos para COSMIC Desktop Environment
//!
//! Implementa iconos vectoriales, cache de recursos, sprites y temas de iconos.

// USERLAND: use crate::drivers::framebuffer::{Color, FramebufferDriver};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

/// Tipo de icono
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IconType {
    // Iconos del sistema
    Start,
    Close,
    Minimize,
    Maximize,
    Settings,

    // Iconos de aplicaciones
    Terminal,
    FileManager,
    Browser,
    Calculator,
    TextEditor,

    // Iconos de estado
    Wifi,
    Battery,
    Volume,
    Clock,
    Notification,

    // Iconos temáticos espaciales
    Rocket,
    Planet,
    Star,
    Galaxy,
    Comet,
    Asteroid,
    Satellite,
    SpaceStation,
}

/// Tamaño de icono
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IconSize {
    Tiny,   // 8x8
    Small,  // 16x16
    Medium, // 24x24
    Large,  // 32x32
    XLarge, // 48x48
    Huge,   // 64x64
}

/// Estilo de icono
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IconStyle {
    Outline,
    Filled,
    Gradient,
    Glow,
    Neon,
}

/// Tema de iconos
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IconTheme {
    Cosmic,
    Space,
    Neon,
    Classic,
    Minimal,
}

/// Definición de icono vectorial
#[derive(Debug, Clone)]
pub struct IconDefinition {
    pub icon_type: IconType,
    pub size: IconSize,
    pub style: IconStyle,
    pub theme: IconTheme,
    pub paths: Vec<IconPath>,
    pub colors: Vec<Color>,
}

/// Ruta de dibujo para iconos vectoriales
#[derive(Debug, Clone)]
pub struct IconPath {
    pub commands: Vec<IconCommand>,
    pub color_index: usize,
    pub filled: bool,
}

/// Comando de dibujo vectorial
#[derive(Debug, Clone)]
pub enum IconCommand {
    MoveTo(f32, f32),
    LineTo(f32, f32),
    CurveTo(f32, f32, f32, f32, f32, f32),
    Circle(f32, f32, f32),
    Rectangle(f32, f32, f32, f32),
    Polygon(Vec<(f32, f32)>),
}

/// Sprite animado
#[derive(Debug, Clone)]
pub struct Sprite {
    pub id: String,
    pub frames: Vec<IconDefinition>,
    pub frame_duration: f32,
    pub loop_animation: bool,
    pub current_frame: usize,
    pub animation_time: f32,
}

/// Cache de iconos
#[derive(Debug, Clone)]
pub struct IconCache {
    pub cached_icons: BTreeMap<(IconType, IconSize, IconStyle, IconTheme), IconDefinition>,
    pub cached_sprites: BTreeMap<String, Sprite>,
    pub max_cache_size: usize,
}

/// Gestor de iconos y recursos gráficos
pub struct IconSystem {
    cache: IconCache,
    current_theme: IconTheme,
    current_style: IconStyle,
}

impl IconSystem {
    pub fn new() -> Self {
        let cache = IconCache {
            cached_icons: BTreeMap::new(),
            cached_sprites: BTreeMap::new(),
            max_cache_size: 1000,
        };

        let mut system = Self {
            cache,
            current_theme: IconTheme::Cosmic,
            current_style: IconStyle::Glow,
        };

        system.initialize_default_icons();
        system
    }

    /// Inicializar iconos por defecto
    fn initialize_default_icons(&mut self) {
        // Iconos del sistema
        self.create_start_icon();
        self.create_close_icon();
        self.create_minimize_icon();
        self.create_maximize_icon();
        self.create_settings_icon();

        // Iconos de aplicaciones
        self.create_terminal_icon();
        self.create_file_manager_icon();
        self.create_browser_icon();

        // Iconos de estado
        self.create_wifi_icon();
        self.create_battery_icon();
        self.create_clock_icon();

        // Iconos temáticos espaciales
        self.create_rocket_icon();
        self.create_planet_icon();
        self.create_star_icon();
    }

    /// Obtener icono
    pub fn get_icon(&self, icon_type: IconType, size: IconSize) -> Option<&IconDefinition> {
        self.cache
            .cached_icons
            .get(&(icon_type, size, self.current_style, self.current_theme))
    }

    /// Renderizar icono
    pub fn render_icon(
        &self,
        fb: &mut FramebufferDriver,
        icon_type: IconType,
        size: IconSize,
        x: u32,
        y: u32,
    ) -> Result<(), String> {
        if let Some(icon) = self.get_icon(icon_type, size) {
            self.render_icon_definition(fb, icon, x, y)
        } else {
            // Renderizar icono por defecto si no se encuentra
            self.render_default_icon(fb, x, y, size)
        }
    }

    /// Renderizar definición de icono
    fn render_icon_definition(
        &self,
        fb: &mut FramebufferDriver,
        icon: &IconDefinition,
        x: u32,
        y: u32,
    ) -> Result<(), String> {
        let size_pixels = self.get_size_pixels(icon.size);

        for path in &icon.paths {
            let color = if path.color_index < icon.colors.len() {
                icon.colors[path.color_index]
            } else {
                Color::WHITE
            };

            for command in &path.commands {
                match command {
                    IconCommand::MoveTo(_, _) => {
                        // Solo mover el cursor, no dibujar
                    }
                    IconCommand::LineTo(end_x, end_y) => {
                        let start_x = x as f32;
                        let start_y = y as f32;
                        self.draw_line(fb, start_x, start_y, *end_x, *end_y, color);
                    }
                    IconCommand::Circle(center_x, center_y, radius) => {
                        self.draw_circle(
                            fb,
                            x as f32 + center_x,
                            y as f32 + center_y,
                            *radius,
                            color,
                        );
                    }
                    IconCommand::Rectangle(rect_x, rect_y, width, height) => {
                        fb.draw_rect(
                            (x as f32 + rect_x) as u32,
                            (y as f32 + rect_y) as u32,
                            *width as u32,
                            *height as u32,
                            color,
                        );
                    }
                    IconCommand::Polygon(points) => {
                        self.draw_polygon(fb, points, x, y, color);
                    }
                    _ => {
                        // Comandos avanzados no implementados aún
                    }
                }
            }
        }
        Ok(())
    }

    /// Renderizar icono por defecto
    fn render_default_icon(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        size: IconSize,
    ) -> Result<(), String> {
        let size_pixels = self.get_size_pixels(size);
        let color = Color::from_hex(0x666666);

        // Dibujar un cuadrado simple como icono por defecto
        fb.draw_rect(x, y, size_pixels, size_pixels, color);

        // Dibujar una X en el centro
        let center = size_pixels / 2;
        let offset = center / 3;

        // Líneas de la X
        self.draw_line(
            fb,
            (x + center - offset) as f32,
            (y + center - offset) as f32,
            (x + center + offset) as f32,
            (y + center + offset) as f32,
            Color::WHITE,
        );
        self.draw_line(
            fb,
            (x + center + offset) as f32,
            (y + center - offset) as f32,
            (x + center - offset) as f32,
            (y + center + offset) as f32,
            Color::WHITE,
        );

        Ok(())
    }

    /// Obtener tamaño en píxeles
    fn get_size_pixels(&self, size: IconSize) -> u32 {
        match size {
            IconSize::Tiny => 8,
            IconSize::Small => 16,
            IconSize::Medium => 24,
            IconSize::Large => 32,
            IconSize::XLarge => 48,
            IconSize::Huge => 64,
        }
    }

    /// Dibujar línea
    fn draw_line(
        &self,
        fb: &mut FramebufferDriver,
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        color: Color,
    ) {
        let dx = (x2 - x1).abs();
        let dy = (y2 - y1).abs();
        let steps = (dx.max(dy) + 0.5) as i32;

        if steps > 0 {
            let x_inc = (x2 - x1) / steps as f32;
            let y_inc = (y2 - y1) / steps as f32;

            for i in 0..=steps {
                let x = (x1 + i as f32 * x_inc) as u32;
                let y = (y1 + i as f32 * y_inc) as u32;
                if x < fb.info.width && y < fb.info.height {
                    fb.draw_rect(x, y, 1, 1, color);
                }
            }
        }
    }

    /// Dibujar círculo
    fn draw_circle(
        &self,
        fb: &mut FramebufferDriver,
        center_x: f32,
        center_y: f32,
        radius: f32,
        color: Color,
    ) {
        let r = radius as i32;
        let cx = center_x as i32;
        let cy = center_y as i32;

        for y in -r..=r {
            for x in -r..=r {
                if x * x + y * y <= r * r {
                    let px = (cx + x) as u32;
                    let py = (cy + y) as u32;
                    if px < fb.info.width && py < fb.info.height {
                        fb.draw_rect(px, py, 1, 1, color);
                    }
                }
            }
        }
    }

    /// Dibujar polígono
    fn draw_polygon(
        &self,
        fb: &mut FramebufferDriver,
        points: &[(f32, f32)],
        offset_x: u32,
        offset_y: u32,
        color: Color,
    ) {
        if points.len() < 3 {
            return;
        }

        // Dibujar líneas entre puntos consecutivos
        for i in 0..points.len() {
            let start = points[i];
            let end = points[(i + 1) % points.len()];

            self.draw_line(
                fb,
                offset_x as f32 + start.0,
                offset_y as f32 + start.1,
                offset_x as f32 + end.0,
                offset_y as f32 + end.1,
                color,
            );
        }
    }

    /// Crear icono de inicio
    fn create_start_icon(&mut self) {
        for size in [IconSize::Small, IconSize::Medium, IconSize::Large] {
            let icon = IconDefinition {
                icon_type: IconType::Start,
                size,
                style: self.current_style,
                theme: self.current_theme,
                paths: Vec::from([IconPath {
                    commands: Vec::from([IconCommand::Circle(0.5, 0.5, 0.4)]),
                    color_index: 0,
                    filled: true,
                }]),
                colors: Vec::from([Color::from_hex(0x00aaff)]),
            };
            self.cache.cached_icons.insert(
                (
                    IconType::Start,
                    size,
                    self.current_style,
                    self.current_theme,
                ),
                icon,
            );
        }
    }

    /// Crear icono de cerrar
    fn create_close_icon(&mut self) {
        for size in [IconSize::Small, IconSize::Medium, IconSize::Large] {
            let icon = IconDefinition {
                icon_type: IconType::Close,
                size,
                style: self.current_style,
                theme: self.current_theme,
                paths: Vec::from([IconPath {
                    commands: Vec::from([
                        IconCommand::MoveTo(0.2, 0.2),
                        IconCommand::LineTo(0.8, 0.8),
                        IconCommand::MoveTo(0.8, 0.2),
                        IconCommand::LineTo(0.2, 0.8),
                    ]),
                    color_index: 0,
                    filled: false,
                }]),
                colors: Vec::from([Color::from_hex(0xff4444)]),
            };
            self.cache.cached_icons.insert(
                (
                    IconType::Close,
                    size,
                    self.current_style,
                    self.current_theme,
                ),
                icon,
            );
        }
    }

    /// Crear icono de minimizar
    fn create_minimize_icon(&mut self) {
        for size in [IconSize::Small, IconSize::Medium, IconSize::Large] {
            let icon = IconDefinition {
                icon_type: IconType::Minimize,
                size,
                style: self.current_style,
                theme: self.current_theme,
                paths: Vec::from([IconPath {
                    commands: Vec::from([
                        IconCommand::MoveTo(0.2, 0.5),
                        IconCommand::LineTo(0.8, 0.5),
                    ]),
                    color_index: 0,
                    filled: false,
                }]),
                colors: Vec::from([Color::from_hex(0xaaaaaa)]),
            };
            self.cache.cached_icons.insert(
                (
                    IconType::Minimize,
                    size,
                    self.current_style,
                    self.current_theme,
                ),
                icon,
            );
        }
    }

    /// Crear icono de maximizar
    fn create_maximize_icon(&mut self) {
        for size in [IconSize::Small, IconSize::Medium, IconSize::Large] {
            let icon = IconDefinition {
                icon_type: IconType::Maximize,
                size,
                style: self.current_style,
                theme: self.current_theme,
                paths: Vec::from([IconPath {
                    commands: Vec::from([IconCommand::Rectangle(0.2, 0.2, 0.6, 0.6)]),
                    color_index: 0,
                    filled: false,
                }]),
                colors: Vec::from([Color::from_hex(0xaaaaaa)]),
            };
            self.cache.cached_icons.insert(
                (
                    IconType::Maximize,
                    size,
                    self.current_style,
                    self.current_theme,
                ),
                icon,
            );
        }
    }

    /// Crear icono de configuración
    fn create_settings_icon(&mut self) {
        for size in [IconSize::Small, IconSize::Medium, IconSize::Large] {
            let icon = IconDefinition {
                icon_type: IconType::Settings,
                size,
                style: self.current_style,
                theme: self.current_theme,
                paths: Vec::from([IconPath {
                    commands: Vec::from([
                        IconCommand::Circle(0.5, 0.5, 0.3),
                        IconCommand::Circle(0.5, 0.5, 0.15),
                        IconCommand::Rectangle(0.5, 0.1, 0.05, 0.2),
                        IconCommand::Rectangle(0.5, 0.7, 0.05, 0.2),
                        IconCommand::Rectangle(0.1, 0.5, 0.2, 0.05),
                        IconCommand::Rectangle(0.7, 0.5, 0.2, 0.05),
                    ]),
                    color_index: 0,
                    filled: false,
                }]),
                colors: Vec::from([Color::from_hex(0x888888)]),
            };
            self.cache.cached_icons.insert(
                (
                    IconType::Settings,
                    size,
                    self.current_style,
                    self.current_theme,
                ),
                icon,
            );
        }
    }

    /// Crear icono de terminal
    fn create_terminal_icon(&mut self) {
        for size in [IconSize::Small, IconSize::Medium, IconSize::Large] {
            let icon = IconDefinition {
                icon_type: IconType::Terminal,
                size,
                style: self.current_style,
                theme: self.current_theme,
                paths: Vec::from([IconPath {
                    commands: Vec::from([
                        IconCommand::Rectangle(0.1, 0.2, 0.8, 0.6),
                        IconCommand::MoveTo(0.3, 0.5),
                        IconCommand::LineTo(0.7, 0.5),
                        IconCommand::MoveTo(0.6, 0.4),
                        IconCommand::LineTo(0.7, 0.5),
                        IconCommand::LineTo(0.6, 0.6),
                    ]),
                    color_index: 0,
                    filled: false,
                }]),
                colors: Vec::from([Color::from_hex(0x00ff00)]),
            };
            self.cache.cached_icons.insert(
                (
                    IconType::Terminal,
                    size,
                    self.current_style,
                    self.current_theme,
                ),
                icon,
            );
        }
    }

    /// Crear icono de explorador de archivos
    fn create_file_manager_icon(&mut self) {
        for size in [IconSize::Small, IconSize::Medium, IconSize::Large] {
            let icon = IconDefinition {
                icon_type: IconType::FileManager,
                size,
                style: self.current_style,
                theme: self.current_theme,
                paths: Vec::from([IconPath {
                    commands: Vec::from([
                        IconCommand::Rectangle(0.2, 0.2, 0.6, 0.6),
                        IconCommand::Rectangle(0.3, 0.3, 0.4, 0.2),
                        IconCommand::Rectangle(0.3, 0.55, 0.4, 0.2),
                    ]),
                    color_index: 0,
                    filled: false,
                }]),
                colors: Vec::from([Color::from_hex(0x0066cc)]),
            };
            self.cache.cached_icons.insert(
                (
                    IconType::FileManager,
                    size,
                    self.current_style,
                    self.current_theme,
                ),
                icon,
            );
        }
    }

    /// Crear icono de navegador
    fn create_browser_icon(&mut self) {
        for size in [IconSize::Small, IconSize::Medium, IconSize::Large] {
            let icon = IconDefinition {
                icon_type: IconType::Browser,
                size,
                style: self.current_style,
                theme: self.current_theme,
                paths: Vec::from([IconPath {
                    commands: Vec::from([
                        IconCommand::Rectangle(0.1, 0.1, 0.8, 0.8),
                        IconCommand::Rectangle(0.1, 0.1, 0.8, 0.2),
                        IconCommand::Circle(0.2, 0.2, 0.05),
                        IconCommand::Circle(0.3, 0.2, 0.05),
                        IconCommand::Circle(0.4, 0.2, 0.05),
                    ]),
                    color_index: 0,
                    filled: false,
                }]),
                colors: Vec::from([Color::from_hex(0x0088ff)]),
            };
            self.cache.cached_icons.insert(
                (
                    IconType::Browser,
                    size,
                    self.current_style,
                    self.current_theme,
                ),
                icon,
            );
        }
    }

    /// Crear icono de WiFi
    fn create_wifi_icon(&mut self) {
        for size in [IconSize::Small, IconSize::Medium, IconSize::Large] {
            let icon = IconDefinition {
                icon_type: IconType::Wifi,
                size,
                style: self.current_style,
                theme: self.current_theme,
                paths: Vec::from([IconPath {
                    commands: Vec::from([
                        IconCommand::MoveTo(0.5, 0.8),
                        IconCommand::LineTo(0.3, 0.6),
                        IconCommand::MoveTo(0.5, 0.8),
                        IconCommand::LineTo(0.7, 0.6),
                        IconCommand::MoveTo(0.4, 0.6),
                        IconCommand::LineTo(0.6, 0.6),
                        IconCommand::Circle(0.5, 0.5, 0.2),
                    ]),
                    color_index: 0,
                    filled: false,
                }]),
                colors: Vec::from([Color::from_hex(0x00ff00)]),
            };
            self.cache.cached_icons.insert(
                (IconType::Wifi, size, self.current_style, self.current_theme),
                icon,
            );
        }
    }

    /// Crear icono de batería
    fn create_battery_icon(&mut self) {
        for size in [IconSize::Small, IconSize::Medium, IconSize::Large] {
            let icon = IconDefinition {
                icon_type: IconType::Battery,
                size,
                style: self.current_style,
                theme: self.current_theme,
                paths: Vec::from([IconPath {
                    commands: Vec::from([
                        IconCommand::Rectangle(0.2, 0.3, 0.5, 0.4),
                        IconCommand::Rectangle(0.7, 0.4, 0.1, 0.2),
                        IconCommand::Rectangle(0.25, 0.35, 0.4, 0.3),
                    ]),
                    color_index: 0,
                    filled: false,
                }]),
                colors: Vec::from([Color::from_hex(0x00ff00)]),
            };
            self.cache.cached_icons.insert(
                (
                    IconType::Battery,
                    size,
                    self.current_style,
                    self.current_theme,
                ),
                icon,
            );
        }
    }

    /// Crear icono de reloj
    fn create_clock_icon(&mut self) {
        for size in [IconSize::Small, IconSize::Medium, IconSize::Large] {
            let icon = IconDefinition {
                icon_type: IconType::Clock,
                size,
                style: self.current_style,
                theme: self.current_theme,
                paths: Vec::from([IconPath {
                    commands: Vec::from([
                        IconCommand::Circle(0.5, 0.5, 0.4),
                        IconCommand::MoveTo(0.5, 0.5),
                        IconCommand::LineTo(0.5, 0.3),
                        IconCommand::MoveTo(0.5, 0.5),
                        IconCommand::LineTo(0.7, 0.5),
                        IconCommand::Circle(0.5, 0.5, 0.02),
                    ]),
                    color_index: 0,
                    filled: false,
                }]),
                colors: Vec::from([Color::from_hex(0xffffff)]),
            };
            self.cache.cached_icons.insert(
                (
                    IconType::Clock,
                    size,
                    self.current_style,
                    self.current_theme,
                ),
                icon,
            );
        }
    }

    /// Crear icono de cohete
    fn create_rocket_icon(&mut self) {
        for size in [IconSize::Small, IconSize::Medium, IconSize::Large] {
            let icon = IconDefinition {
                icon_type: IconType::Rocket,
                size,
                style: self.current_style,
                theme: self.current_theme,
                paths: Vec::from([IconPath {
                    commands: Vec::from([
                        IconCommand::Polygon(Vec::from([(0.5, 0.1), (0.3, 0.7), (0.7, 0.7)])),
                        IconCommand::Rectangle(0.4, 0.7, 0.2, 0.2),
                        IconCommand::MoveTo(0.3, 0.9),
                        IconCommand::LineTo(0.7, 0.9),
                    ]),
                    color_index: 0,
                    filled: false,
                }]),
                colors: Vec::from([Color::from_hex(0xff6600)]),
            };
            self.cache.cached_icons.insert(
                (
                    IconType::Rocket,
                    size,
                    self.current_style,
                    self.current_theme,
                ),
                icon,
            );
        }
    }

    /// Crear icono de planeta
    fn create_planet_icon(&mut self) {
        for size in [IconSize::Small, IconSize::Medium, IconSize::Large] {
            let icon = IconDefinition {
                icon_type: IconType::Planet,
                size,
                style: self.current_style,
                theme: self.current_theme,
                paths: Vec::from([IconPath {
                    commands: Vec::from([
                        IconCommand::Circle(0.5, 0.5, 0.4),
                        IconCommand::MoveTo(0.2, 0.5),
                        IconCommand::LineTo(0.8, 0.5),
                        IconCommand::MoveTo(0.5, 0.2),
                        IconCommand::LineTo(0.5, 0.8),
                    ]),
                    color_index: 0,
                    filled: false,
                }]),
                colors: Vec::from([Color::from_hex(0x0066cc)]),
            };
            self.cache.cached_icons.insert(
                (
                    IconType::Planet,
                    size,
                    self.current_style,
                    self.current_theme,
                ),
                icon,
            );
        }
    }

    /// Crear icono de estrella
    fn create_star_icon(&mut self) {
        for size in [IconSize::Small, IconSize::Medium, IconSize::Large] {
            let icon = IconDefinition {
                icon_type: IconType::Star,
                size,
                style: self.current_style,
                theme: self.current_theme,
                paths: Vec::from([IconPath {
                    commands: Vec::from([IconCommand::Polygon(Vec::from([
                        (0.5, 0.1),
                        (0.6, 0.4),
                        (0.9, 0.4),
                        (0.7, 0.6),
                        (0.8, 0.9),
                        (0.5, 0.7),
                        (0.2, 0.9),
                        (0.3, 0.6),
                        (0.1, 0.4),
                        (0.4, 0.4),
                    ]))]),
                    color_index: 0,
                    filled: false,
                }]),
                colors: Vec::from([Color::from_hex(0xffff00)]),
            };
            self.cache.cached_icons.insert(
                (IconType::Star, size, self.current_style, self.current_theme),
                icon,
            );
        }
    }

    /// Cambiar tema de iconos
    pub fn set_theme(&mut self, theme: IconTheme) {
        self.current_theme = theme;
        // Limpiar cache para regenerar iconos con nuevo tema
        self.cache.cached_icons.clear();
        self.initialize_default_icons();
    }

    /// Cambiar estilo de iconos
    pub fn set_style(&mut self, style: IconStyle) {
        self.current_style = style;
        // Limpiar cache para regenerar iconos con nuevo estilo
        self.cache.cached_icons.clear();
        self.initialize_default_icons();
    }

    /// Obtener estadísticas del sistema de iconos
    pub fn get_stats(&self) -> (usize, usize) {
        (
            self.cache.cached_icons.len(),
            self.cache.cached_sprites.len(),
        )
    }
}
