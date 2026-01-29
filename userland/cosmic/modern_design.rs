//! Diseño Moderno de COSMIC - Inspirado en COSMIC Epoch
//!
//! Este módulo implementa un sistema de diseño moderno y atractivo
//! basado en los principios de diseño de COSMIC Epoch.

// USERLAND: use crate::drivers::framebuffer::{Color, FramebufferDriver};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Sistema de diseño moderno de COSMIC
#[derive(Debug, Clone)]
pub struct ModernDesign {
    /// Tema actual del diseño
    pub theme: CosmicTheme,
    /// Configuración de colores
    pub colors: ColorScheme,
    /// Configuración de tipografía
    pub typography: TypographyConfig,
    /// Configuración de espaciado
    pub spacing: SpacingConfig,
    /// Configuración de bordes
    pub borders: BorderConfig,
    /// Configuración de sombras
    pub shadows: ShadowConfig,
    /// Configuración de animaciones
    pub animations: AnimationConfig,
}

/// Tema de COSMIC
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CosmicTheme {
    Light,
    Dark,
    Auto,
    Cosmic,
}

/// Esquema de colores
#[derive(Debug, Clone)]
pub struct ColorScheme {
    /// Color de fondo principal
    pub background: Color,
    /// Color de fondo secundario
    pub background_secondary: Color,
    /// Color de superficie
    pub surface: Color,
    /// Color de superficie elevada
    pub surface_elevated: Color,
    /// Color de texto primario
    pub text_primary: Color,
    /// Color de texto secundario
    pub text_secondary: Color,
    /// Color de texto deshabilitado
    pub text_disabled: Color,
    /// Color de acento
    pub accent: Color,
    /// Color de acento secundario
    pub accent_secondary: Color,
    /// Color de éxito
    pub success: Color,
    /// Color de advertencia
    pub warning: Color,
    /// Color de error
    pub error: Color,
    /// Color de información
    pub info: Color,
    /// Color de borde
    pub border: Color,
    /// Color de borde sutil
    pub border_subtle: Color,
    /// Color de sombra
    pub shadow: Color,
}

/// Configuración de tipografía
#[derive(Debug, Clone)]
pub struct TypographyConfig {
    /// Tamaño de fuente base
    pub base_size: u32,
    /// Tamaño de fuente pequeño
    pub small_size: u32,
    /// Tamaño de fuente grande
    pub large_size: u32,
    /// Tamaño de fuente extra grande
    pub xl_size: u32,
    /// Altura de línea
    pub line_height: f32,
    /// Espaciado entre letras
    pub letter_spacing: f32,
}

/// Configuración de espaciado
#[derive(Debug, Clone)]
pub struct SpacingConfig {
    /// Espaciado extra pequeño
    pub xs: u32,
    /// Espaciado pequeño
    pub sm: u32,
    /// Espaciado medio
    pub md: u32,
    /// Espaciado grande
    pub lg: u32,
    /// Espaciado extra grande
    pub xl: u32,
    /// Espaciado extra extra grande
    pub xxl: u32,
}

/// Configuración de bordes
#[derive(Debug, Clone)]
pub struct BorderConfig {
    /// Radio de borde pequeño
    pub radius_sm: u32,
    /// Radio de borde medio
    pub radius_md: u32,
    /// Radio de borde grande
    pub radius_lg: u32,
    /// Radio de borde extra grande
    pub radius_xl: u32,
    /// Ancho de borde
    pub width: u32,
}

/// Configuración de sombras
#[derive(Debug, Clone)]
pub struct ShadowConfig {
    /// Sombra pequeña
    pub small: ShadowStyle,
    /// Sombra media
    pub medium: ShadowStyle,
    /// Sombra grande
    pub large: ShadowStyle,
    /// Sombra extra grande
    pub xl: ShadowStyle,
}

/// Estilo de sombra
#[derive(Debug, Clone)]
pub struct ShadowStyle {
    /// Desplazamiento X
    pub offset_x: i32,
    /// Desplazamiento Y
    pub offset_y: i32,
    /// Desenfoque
    pub blur: u32,
    /// Color de la sombra
    pub color: Color,
    /// Opacidad
    pub opacity: f32,
}

/// Configuración de animaciones
#[derive(Debug, Clone)]
pub struct AnimationConfig {
    /// Duración de animación rápida
    pub fast_duration: f32,
    /// Duración de animación normal
    pub normal_duration: f32,
    /// Duración de animación lenta
    pub slow_duration: f32,
    /// Curva de animación
    pub easing: EasingCurve,
}

/// Curva de animación
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EasingCurve {
    Linear,
    EaseIn,
    EaseOut,
    EaseInOut,
    Bounce,
    Elastic,
}

impl ModernDesign {
    /// Crear nuevo sistema de diseño
    pub fn new() -> Self {
        Self {
            theme: CosmicTheme::Dark,
            colors: ColorScheme::cosmic_dark(),
            typography: TypographyConfig::default(),
            spacing: SpacingConfig::default(),
            borders: BorderConfig::default(),
            shadows: ShadowConfig::default(),
            animations: AnimationConfig::default(),
        }
    }

    /// Crear con tema específico
    pub fn with_theme(theme: CosmicTheme) -> Self {
        let mut design = Self::new();
        design.theme = theme;
        design.colors = match theme {
            CosmicTheme::Light => ColorScheme::cosmic_light(),
            CosmicTheme::Dark => ColorScheme::cosmic_dark(),
            CosmicTheme::Auto => ColorScheme::cosmic_dark(),
            CosmicTheme::Cosmic => ColorScheme::cosmic_cosmic(),
        };
        design
    }

    /// Aplicar tema
    pub fn apply_theme(&mut self, theme: CosmicTheme) {
        self.theme = theme;
        self.colors = match theme {
            CosmicTheme::Light => ColorScheme::cosmic_light(),
            CosmicTheme::Dark => ColorScheme::cosmic_dark(),
            CosmicTheme::Auto => ColorScheme::cosmic_dark(),
            CosmicTheme::Cosmic => ColorScheme::cosmic_cosmic(),
        };
    }

    /// Renderizar panel moderno
    pub fn render_modern_panel(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        // Fondo del panel con gradiente sutil
        self.render_panel_background(fb, x, y, width, height)?;

        // Borde del panel
        self.render_panel_border(fb, x, y, width, height)?;

        // Sombra del panel
        self.render_panel_shadow(fb, x, y, width, height)?;

        Ok(())
    }

    /// Renderizar fondo del panel
    fn render_panel_background(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        // Gradiente sutil de fondo
        for py in y..(y + height) {
            for px in x..(x + width) {
                if px < fb.info.width && py < fb.info.height {
                    // Crear gradiente vertical sutil
                    let progress = (py - y) as f32 / height as f32;
                    let color = self.interpolate_color(
                        &self.colors.background,
                        &self.colors.background_secondary,
                        progress,
                    );
                    fb.put_pixel(px, py, color);
                }
            }
        }
        Ok(())
    }

    /// Renderizar borde del panel
    fn render_panel_border(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        // Borde superior
        for px in x..(x + width) {
            if px < fb.info.width && y < fb.info.height {
                fb.put_pixel(px, y, self.colors.border_subtle);
            }
        }

        // Borde inferior
        for px in x..(x + width) {
            if px < fb.info.width && (y + height - 1) < fb.info.height {
                fb.put_pixel(px, y + height - 1, self.colors.border);
            }
        }

        // Bordes laterales
        for py in y..(y + height) {
            if x < fb.info.width && py < fb.info.height {
                fb.put_pixel(x, py, self.colors.border_subtle);
            }
            if (x + width - 1) < fb.info.width && py < fb.info.height {
                fb.put_pixel(x + width - 1, py, self.colors.border_subtle);
            }
        }

        Ok(())
    }

    /// Renderizar sombra del panel
    fn render_panel_shadow(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let shadow = &self.shadows.medium;

        // Sombra inferior
        for px in x..(x + width) {
            for py in (y + height)..(y + height + shadow.blur) {
                if px < fb.info.width && py < fb.info.height {
                    let intensity = 1.0 - ((py - (y + height)) as f32 / shadow.blur as f32);
                    if intensity > 0.0 {
                        fb.put_pixel(px, py, self.colors.shadow);
                    }
                }
            }
        }

        // Sombra lateral derecha
        for px in (x + width)..(x + width + shadow.blur) {
            for py in y..(y + height) {
                if px < fb.info.width && py < fb.info.height {
                    let intensity = 1.0 - ((px - (x + width)) as f32 / shadow.blur as f32);
                    if intensity > 0.0 {
                        fb.put_pixel(px, py, self.colors.shadow);
                    }
                }
            }
        }

        Ok(())
    }

    /// Renderizar botón moderno
    pub fn render_modern_button(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        text: &str,
        pressed: bool,
    ) -> Result<(), String> {
        // Fondo del botón
        let bg_color = if pressed {
            self.colors.surface_elevated
        } else {
            self.colors.surface
        };
        self.render_rounded_rectangle(fb, x, y, width, height, bg_color, self.borders.radius_md)?;

        // Borde del botón
        self.render_rounded_border(
            fb,
            x,
            y,
            width,
            height,
            self.colors.border,
            self.borders.radius_md,
        )?;

        // Sombra del botón
        if !pressed {
            self.render_button_shadow(fb, x, y, width, height)?;
        }

        // Texto del botón
        self.render_button_text(fb, x, y, width, height, text)?;

        Ok(())
    }

    /// Renderizar rectángulo redondeado
    pub fn render_rounded_rectangle(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        color: Color,
        radius: u32,
    ) -> Result<(), String> {
        for py in y..(y + height) {
            for px in x..(x + width) {
                if px < fb.info.width && py < fb.info.height {
                    if self.is_point_in_rounded_rect(px, py, x, y, width, height, radius) {
                        fb.put_pixel(px, py, color);
                    }
                }
            }
        }
        Ok(())
    }

    /// Verificar si un punto está dentro de un rectángulo redondeado
    fn is_point_in_rounded_rect(
        &self,
        px: u32,
        py: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        radius: u32,
    ) -> bool {
        // Verificar si está en el área central
        if px >= x + radius && px < x + width - radius && py >= y && py < y + height {
            return true;
        }
        if px >= x && px < x + width && py >= y + radius && py < y + height - radius {
            return true;
        }

        // Verificar esquinas redondeadas
        let corners = [
            (x + radius, y + radius, radius), // Esquina superior izquierda
            (x + width - radius, y + radius, radius), // Esquina superior derecha
            (x + radius, y + height - radius, radius), // Esquina inferior izquierda
            (x + width - radius, y + height - radius, radius), // Esquina inferior derecha
        ];

        for (cx, cy, r) in corners {
            let dx = px as i32 - cx as i32;
            let dy = py as i32 - cy as i32;
            if dx * dx + dy * dy <= (r as i32).pow(2) {
                return true;
            }
        }

        false
    }

    /// Renderizar borde redondeado
    pub fn render_rounded_border(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        color: Color,
        radius: u32,
    ) -> Result<(), String> {
        // Borde superior
        for px in x..(x + width) {
            if px < fb.info.width && y < fb.info.height {
                if self.is_point_on_rounded_border(px, y, x, y, width, height, radius) {
                    fb.put_pixel(px, y, color);
                }
            }
        }

        // Borde inferior
        for px in x..(x + width) {
            if px < fb.info.width && (y + height - 1) < fb.info.height {
                if self.is_point_on_rounded_border(px, y + height - 1, x, y, width, height, radius)
                {
                    fb.put_pixel(px, y + height - 1, color);
                }
            }
        }

        // Bordes laterales
        for py in y..(y + height) {
            if x < fb.info.width && py < fb.info.height {
                if self.is_point_on_rounded_border(x, py, x, y, width, height, radius) {
                    fb.put_pixel(x, py, color);
                }
            }
            if (x + width - 1) < fb.info.width && py < fb.info.height {
                if self.is_point_on_rounded_border(x + width - 1, py, x, y, width, height, radius) {
                    fb.put_pixel(x + width - 1, py, color);
                }
            }
        }

        Ok(())
    }

    /// Verificar si un punto está en el borde redondeado
    fn is_point_on_rounded_border(
        &self,
        px: u32,
        py: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        radius: u32,
    ) -> bool {
        // Verificar si está en el borde superior o inferior
        if py == y || py == y + height - 1 {
            return px >= x && px < x + width;
        }

        // Verificar si está en el borde izquierdo o derecho
        if px == x || px == x + width - 1 {
            return py >= y && py < y + height;
        }

        false
    }

    /// Renderizar sombra del botón
    pub fn render_button_shadow(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let shadow = &self.shadows.small;

        // Sombra inferior
        for px in x..(x + width) {
            for py in (y + height)..(y + height + shadow.blur) {
                if px < fb.info.width && py < fb.info.height {
                    let intensity = 1.0 - ((py - (y + height)) as f32 / shadow.blur as f32);
                    if intensity > 0.0 {
                        fb.put_pixel(px, py, self.colors.shadow);
                    }
                }
            }
        }

        Ok(())
    }

    /// Renderizar texto del botón
    fn render_button_text(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        text: &str,
    ) -> Result<(), String> {
        let text_x = x + (width - text.len() as u32 * 8) / 2;
        let text_y = y + (height - 16) / 2;

        fb.write_text_kernel(text, self.colors.text_primary);
        Ok(())
    }

    /// Interpolar entre dos colores
    pub fn interpolate_color(&self, start: &Color, end: &Color, progress: f32) -> Color {
        let progress = progress.max(0.0).min(1.0);

        // Interpolación simple basada en el progreso
        if progress < 0.5 {
            *start
        } else {
            *end
        }
    }

    /// Renderizar ventana moderna
    pub fn render_modern_window(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        title: &str,
    ) -> Result<(), String> {
        // Fondo de la ventana
        self.render_rounded_rectangle(
            fb,
            x,
            y,
            width,
            height,
            self.colors.surface,
            self.borders.radius_lg,
        )?;

        // Borde de la ventana
        self.render_rounded_border(
            fb,
            x,
            y,
            width,
            height,
            self.colors.border,
            self.borders.radius_lg,
        )?;

        // Barra de título
        self.render_title_bar(fb, x, y, width, title)?;

        // Sombra de la ventana
        self.render_window_shadow(fb, x, y, width, height)?;

        Ok(())
    }

    /// Renderizar barra de título
    fn render_title_bar(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        title: &str,
    ) -> Result<(), String> {
        let title_height = 32;

        // Fondo de la barra de título
        self.render_rounded_rectangle(
            fb,
            x,
            y,
            width,
            title_height,
            self.colors.surface_elevated,
            self.borders.radius_lg,
        )?;

        // Texto del título
        let title_x = x + 16;
        let title_y = y + 8;
        fb.write_text_kernel(title, self.colors.text_primary);

        // Botones de la barra de título
        self.render_title_buttons(fb, x + width - 80, y, title_height)?;

        Ok(())
    }

    /// Renderizar botones de la barra de título
    fn render_title_buttons(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        height: u32,
    ) -> Result<(), String> {
        let button_size = 20;
        let button_spacing = 4;

        // Botón de minimizar
        self.render_modern_button(
            fb,
            x,
            y + (height - button_size) / 2,
            button_size,
            button_size,
            "-",
            false,
        )?;

        // Botón de maximizar
        self.render_modern_button(
            fb,
            x + button_size + button_spacing,
            y + (height - button_size) / 2,
            button_size,
            button_size,
            "□",
            false,
        )?;

        // Botón de cerrar
        self.render_modern_button(
            fb,
            x + 2 * (button_size + button_spacing),
            y + (height - button_size) / 2,
            button_size,
            button_size,
            "×",
            false,
        )?;

        Ok(())
    }

    /// Renderizar sombra de la ventana
    fn render_window_shadow(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let shadow = &self.shadows.large;

        // Sombra inferior
        for px in x..(x + width) {
            for py in (y + height)..(y + height + shadow.blur) {
                if px < fb.info.width && py < fb.info.height {
                    let intensity = 1.0 - ((py - (y + height)) as f32 / shadow.blur as f32);
                    if intensity > 0.0 {
                        fb.put_pixel(px, py, self.colors.shadow);
                    }
                }
            }
        }

        // Sombra lateral derecha
        for px in (x + width)..(x + width + shadow.blur) {
            for py in y..(y + height) {
                if px < fb.info.width && py < fb.info.height {
                    let intensity = 1.0 - ((px - (x + width)) as f32 / shadow.blur as f32);
                    if intensity > 0.0 {
                        fb.put_pixel(px, py, self.colors.shadow);
                    }
                }
            }
        }

        Ok(())
    }

    /// Obtener estadísticas del diseño
    pub fn get_design_stats(&self) -> String {
        format!(
            "Tema: {:?}, Colores: {} esquemas, Tipografía: {}px base",
            self.theme,
            "1", // Simplificado
            self.typography.base_size
        )
    }
}

impl ColorScheme {
    /// Esquema de colores oscuro de COSMIC
    pub fn cosmic_dark() -> Self {
        Self {
            background: Color::DARK_BLUE,
            background_secondary: Color::BLACK,
            surface: Color::DARK_GRAY,
            surface_elevated: Color::GRAY,
            text_primary: Color::WHITE,
            text_secondary: Color::LIGHT_GRAY,
            text_disabled: Color::GRAY,
            accent: Color::CYAN,
            accent_secondary: Color::MAGENTA,
            success: Color::GREEN,
            warning: Color::YELLOW,
            error: Color::RED,
            info: Color::BLUE,
            border: Color::GRAY,
            border_subtle: Color::DARK_GRAY,
            shadow: Color::BLACK,
        }
    }

    /// Esquema de colores claro de COSMIC
    pub fn cosmic_light() -> Self {
        Self {
            background: Color::WHITE,
            background_secondary: Color::LIGHT_GRAY,
            surface: Color::LIGHT_GRAY,
            surface_elevated: Color::WHITE,
            text_primary: Color::BLACK,
            text_secondary: Color::DARK_GRAY,
            text_disabled: Color::GRAY,
            accent: Color::BLUE,
            accent_secondary: Color::MAGENTA,
            success: Color::GREEN,
            warning: Color::YELLOW,
            error: Color::RED,
            info: Color::BLUE,
            border: Color::GRAY,
            border_subtle: Color::LIGHT_GRAY,
            shadow: Color::DARK_GRAY,
        }
    }

    /// Esquema de colores COSMIC personalizado
    pub fn cosmic_cosmic() -> Self {
        Self {
            background: Color::DARK_BLUE,
            background_secondary: Color::BLACK,
            surface: Color::DARK_GRAY,
            surface_elevated: Color::GRAY,
            text_primary: Color::CYAN,
            text_secondary: Color::WHITE,
            text_disabled: Color::GRAY,
            accent: Color::MAGENTA,
            accent_secondary: Color::YELLOW,
            success: Color::GREEN,
            warning: Color::YELLOW,
            error: Color::RED,
            info: Color::CYAN,
            border: Color::CYAN,
            border_subtle: Color::DARK_GRAY,
            shadow: Color::BLACK,
        }
    }
}

impl Default for TypographyConfig {
    fn default() -> Self {
        Self {
            base_size: 14,
            small_size: 12,
            large_size: 18,
            xl_size: 24,
            line_height: 1.5,
            letter_spacing: 0.0,
        }
    }
}

impl Default for SpacingConfig {
    fn default() -> Self {
        Self {
            xs: 4,
            sm: 8,
            md: 16,
            lg: 24,
            xl: 32,
            xxl: 48,
        }
    }
}

impl Default for BorderConfig {
    fn default() -> Self {
        Self {
            radius_sm: 4,
            radius_md: 8,
            radius_lg: 12,
            radius_xl: 16,
            width: 1,
        }
    }
}

impl Default for ShadowConfig {
    fn default() -> Self {
        Self {
            small: ShadowStyle {
                offset_x: 0,
                offset_y: 2,
                blur: 4,
                color: Color::BLACK,
                opacity: 0.1,
            },
            medium: ShadowStyle {
                offset_x: 0,
                offset_y: 4,
                blur: 8,
                color: Color::BLACK,
                opacity: 0.15,
            },
            large: ShadowStyle {
                offset_x: 0,
                offset_y: 8,
                blur: 16,
                color: Color::BLACK,
                opacity: 0.2,
            },
            xl: ShadowStyle {
                offset_x: 0,
                offset_y: 16,
                blur: 32,
                color: Color::BLACK,
                opacity: 0.25,
            },
        }
    }
}

impl Default for AnimationConfig {
    fn default() -> Self {
        Self {
            fast_duration: 0.15,
            normal_duration: 0.3,
            slow_duration: 0.5,
            easing: EasingCurve::EaseOut,
        }
    }
}
