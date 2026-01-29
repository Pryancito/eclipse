//! Sistema de ventanas avanzado para COSMIC Desktop Environment
//!
//! Implementa ventanas con bordes, controles, efectos visuales y gestión completa.

// USERLAND: use crate::drivers::framebuffer::{Color, FramebufferDriver};
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Estado de una ventana
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowState {
    Normal,
    Minimized,
    Maximized,
    Hidden,
}

/// Tipo de ventana
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowType {
    Application,
    Dialog,
    Tooltip,
    Splash,
}

/// Decoraciones de ventana
#[derive(Debug, Clone)]
pub struct WindowDecorations {
    pub title_bar: bool,
    pub close_button: bool,
    pub minimize_button: bool,
    pub maximize_button: bool,
    pub resize_handles: bool,
    pub border: bool,
}

/// Efectos visuales de ventana
#[derive(Debug, Clone)]
pub struct WindowEffects {
    pub shadow: bool,
    pub glow: bool,
    pub transparency: f32, // 0.0 = transparente, 1.0 = opaco
    pub blur: bool,
    pub animation_speed: f32,
}

/// Animación de ventana
#[derive(Debug, Clone)]
pub struct WindowAnimation {
    pub fade_in: f32,
    pub fade_out: f32,
    pub slide_in: f32,
    pub slide_out: f32,
    pub scale_in: f32,
    pub scale_out: f32,
    pub rotation: f32,
}

/// Configuración de ventana
#[derive(Debug, Clone)]
pub struct WindowConfig {
    pub width: u32,
    pub height: u32,
    pub min_width: u32,
    pub min_height: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub resizable: bool,
    pub movable: bool,
    pub decorations: WindowDecorations,
    pub effects: WindowEffects,
}

/// Ventana de aplicación
#[derive(Debug, Clone)]
pub struct Window {
    pub id: u32,
    pub title: String,
    pub window_type: WindowType,
    pub state: WindowState,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub config: WindowConfig,
    pub animation: WindowAnimation,
    pub is_focused: bool,
    pub is_dragging: bool,
    pub is_resizing: bool,
    pub drag_start_x: i32,
    pub drag_start_y: i32,
    pub content: Vec<String>, // Contenido simulado de la ventana
    pub z_order: u32,         // Orden de apilamiento
}

/// Gestor de ventanas
pub struct WindowManager {
    windows: BTreeMap<u32, Window>,
    next_window_id: u32,
    focused_window_id: Option<u32>,
    window_counter: u32,
}

impl WindowManager {
    pub fn new() -> Self {
        Self {
            windows: BTreeMap::new(),
            next_window_id: 1,
            focused_window_id: None,
            window_counter: 0,
        }
    }

    /// Crear nueva ventana
    pub fn create_window(
        &mut self,
        title: String,
        width: u32,
        height: u32,
        window_type: WindowType,
    ) -> u32 {
        let id = self.next_window_id;
        self.next_window_id += 1;

        let decorations = WindowDecorations {
            title_bar: true,
            close_button: true,
            minimize_button: true,
            maximize_button: true,
            resize_handles: true,
            border: true,
        };

        let effects = WindowEffects {
            shadow: true,
            glow: false,
            transparency: 1.0,
            blur: false,
            animation_speed: 8.0,
        };

        let config = WindowConfig {
            width,
            height,
            min_width: 200,
            min_height: 150,
            max_width: 1920,
            max_height: 1080,
            resizable: true,
            movable: true,
            decorations,
            effects,
        };

        let window = Window {
            id,
            title,
            window_type,
            state: WindowState::Normal,
            x: 100 + (self.window_counter * 50) as i32,
            y: 100 + (self.window_counter * 50) as i32,
            width,
            height,
            config,
            animation: WindowAnimation {
                fade_in: 0.0,
                fade_out: 1.0,
                slide_in: 0.0,
                slide_out: 0.0,
                scale_in: 0.0,
                scale_out: 1.0,
                rotation: 0.0,
            },
            is_focused: false,
            is_dragging: false,
            is_resizing: false,
            drag_start_x: 0,
            drag_start_y: 0,
            content: Vec::new(),
            z_order: self.window_counter,
        };

        self.windows.insert(id, window);
        self.window_counter += 1;
        id
    }

    /// Obtener ventana por ID
    pub fn get_window(&self, id: u32) -> Option<&Window> {
        self.windows.get(&id)
    }

    /// Obtener ventana mutable por ID
    pub fn get_window_mut(&mut self, id: u32) -> Option<&mut Window> {
        self.windows.get_mut(&id)
    }

    /// Cerrar ventana
    pub fn close_window(&mut self, id: u32) -> bool {
        self.windows.remove(&id).is_some()
    }

    /// Enfocar ventana
    pub fn focus_window(&mut self, id: u32) -> bool {
        if let Some(window) = self.windows.get(&id) {
            // Desenfocar ventana anterior
            if let Some(prev_id) = self.focused_window_id {
                if let Some(prev_window) = self.windows.get_mut(&prev_id) {
                    prev_window.is_focused = false;
                }
            }

            // Enfocar nueva ventana
            if let Some(window) = self.windows.get_mut(&id) {
                window.is_focused = true;
                self.focused_window_id = Some(id);
                true
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Minimizar ventana
    pub fn minimize_window(&mut self, id: u32) -> bool {
        if let Some(window) = self.windows.get_mut(&id) {
            window.state = WindowState::Minimized;
            window.animation.fade_out = 0.0;
            true
        } else {
            false
        }
    }

    /// Maximizar ventana
    pub fn maximize_window(&mut self, id: u32) -> bool {
        if let Some(window) = self.windows.get_mut(&id) {
            if window.state == WindowState::Maximized {
                window.state = WindowState::Normal;
                window.width = window.config.width;
                window.height = window.config.height;
            } else {
                window.state = WindowState::Maximized;
                window.width = 1920; // Tamaño de pantalla completo
                window.height = 1040; // Altura menos barra de tareas
                window.x = 0;
                window.y = 0;
            }
            true
        } else {
            false
        }
    }

    /// Mover ventana
    pub fn move_window(&mut self, id: u32, x: i32, y: i32) -> bool {
        if let Some(window) = self.windows.get_mut(&id) {
            window.x = x.max(0);
            window.y = y.max(0);
            true
        } else {
            false
        }
    }

    /// Redimensionar ventana
    pub fn resize_window(&mut self, id: u32, width: u32, height: u32) -> bool {
        if let Some(window) = self.windows.get_mut(&id) {
            let new_width = width
                .max(window.config.min_width)
                .min(window.config.max_width);
            let new_height = height
                .max(window.config.min_height)
                .min(window.config.max_height);

            window.width = new_width;
            window.height = new_height;
            true
        } else {
            false
        }
    }

    /// Actualizar animaciones de ventanas
    pub fn update_animations(&mut self, delta_time: f32) {
        for (_, window) in self.windows.iter_mut() {
            // Animación de fade in
            if window.animation.fade_in < 1.0 {
                window.animation.fade_in += delta_time * window.config.effects.animation_speed;
                if window.animation.fade_in > 1.0 {
                    window.animation.fade_in = 1.0;
                }
            }

            // Animación de fade out
            if window.animation.fade_out < 1.0 {
                window.animation.fade_out += delta_time * window.config.effects.animation_speed;
                if window.animation.fade_out > 1.0 {
                    window.animation.fade_out = 1.0;
                }
            }

            // Animación de slide in
            if window.animation.slide_in < 1.0 {
                window.animation.slide_in +=
                    delta_time * window.config.effects.animation_speed * 0.5;
                if window.animation.slide_in > 1.0 {
                    window.animation.slide_in = 1.0;
                }
            }

            // Animación de scale in
            if window.animation.scale_in < 1.0 {
                window.animation.scale_in +=
                    delta_time * window.config.effects.animation_speed * 0.7;
                if window.animation.scale_in > 1.0 {
                    window.animation.scale_in = 1.0;
                }
            }
        }
    }

    /// Obtener todas las ventanas ordenadas por z-order
    pub fn get_windows_sorted(&self) -> Vec<&Window> {
        let mut windows: Vec<&Window> = self.windows.values().collect();
        windows.sort_by_key(|w| w.z_order);
        windows
    }

    /// Obtener ventana enfocada
    pub fn get_focused_window(&self) -> Option<&Window> {
        if let Some(id) = self.focused_window_id {
            self.windows.get(&id)
        } else {
            None
        }
    }

    /// Obtener estadísticas del gestor de ventanas
    pub fn get_stats(&self) -> (usize, usize, usize) {
        let total = self.windows.len();
        let visible = self
            .windows
            .values()
            .filter(|w| w.state != WindowState::Hidden)
            .count();
        let focused = if self.focused_window_id.is_some() {
            1
        } else {
            0
        };
        (total, visible, focused)
    }

    /// Obtener número total de ventanas
    pub fn get_window_count(&self) -> usize {
        self.windows.len()
    }
}

/// Renderizar sistema de ventanas
pub fn render_window_system(
    fb: &mut FramebufferDriver,
    window_manager: &WindowManager,
) -> Result<(), String> {
    let windows = window_manager.get_windows_sorted();

    for window in windows {
        if window.state != WindowState::Hidden {
            render_window(fb, window)?;
        }
    }

    Ok(())
}

/// Renderizar una ventana individual
fn render_window(fb: &mut FramebufferDriver, window: &Window) -> Result<(), String> {
    if window.animation.fade_out <= 0.0 {
        return Ok(());
    }

    let screen_width = fb.info.width;
    let screen_height = fb.info.height;

    // Verificar que la ventana esté en pantalla
    if window.x >= screen_width as i32 || window.y >= screen_height as i32 {
        return Ok(());
    }

    // Aplicar animaciones
    let fade_alpha = window.animation.fade_in * window.animation.fade_out;
    let slide_offset = (1.0 - window.animation.slide_in) * 50.0;
    let scale_factor = 0.5 + (window.animation.scale_in * 0.5);

    // Calcular posición y tamaño con animaciones
    let anim_x = (window.x as f32 + slide_offset) as i32;
    let anim_y = (window.y as f32 + slide_offset * 0.5) as i32;
    let anim_width = (window.width as f32 * scale_factor) as u32;
    let anim_height = (window.height as f32 * scale_factor) as u32;

    // Colores del tema espacial
    let shadow_color = Color::from_hex(0x000000);
    let border_color = Color::from_hex(0x0066aa);
    let title_bar_color = if window.is_focused {
        Color::from_hex(0x004488)
    } else {
        Color::from_hex(0x002244)
    };
    let background_color = Color::from_hex(0x001122);
    let text_color = Color::from_hex(0xffffff);
    let button_color = Color::from_hex(0x003366);
    let hover_color = Color::from_hex(0x004488);

    // Renderizar sombra si está habilitada
    if window.config.effects.shadow && fade_alpha > 0.5 {
        let shadow_alpha = (fade_alpha - 0.5) * 2.0;
        let shadow_color_fade = Color {
            r: (shadow_color.r as f32 * shadow_alpha * 0.3) as u8,
            g: (shadow_color.g as f32 * shadow_alpha * 0.3) as u8,
            b: (shadow_color.b as f32 * shadow_alpha * 0.3) as u8,
            a: (shadow_color.a as f32 * shadow_alpha * 0.3) as u8,
        };

        // Sombra desplazada
        fb.draw_rect(
            (anim_x + 4) as u32,
            (anim_y + 4) as u32,
            anim_width,
            anim_height,
            shadow_color_fade,
        );
    }

    // Aplicar transparencia a los colores
    let title_bar_fade = Color {
        r: (title_bar_color.r as f32 * fade_alpha) as u8,
        g: (title_bar_color.g as f32 * fade_alpha) as u8,
        b: (title_bar_color.b as f32 * fade_alpha) as u8,
        a: 255,
    };

    let background_fade = Color {
        r: (background_color.r as f32 * fade_alpha) as u8,
        g: (background_color.g as f32 * fade_alpha) as u8,
        b: (background_color.b as f32 * fade_alpha) as u8,
        a: 255,
    };

    let border_fade = Color {
        r: (border_color.r as f32 * fade_alpha) as u8,
        g: (border_color.g as f32 * fade_alpha) as u8,
        b: (border_color.b as f32 * fade_alpha) as u8,
        a: 255,
    };

    // Renderizar ventana
    if window.config.decorations.border {
        // Borde de la ventana
        fb.draw_rect(
            anim_x as u32,
            anim_y as u32,
            anim_width,
            anim_height,
            border_fade,
        );
    }

    // Fondo de la ventana
    fb.draw_rect(
        (anim_x + 1) as u32,
        (anim_y + 1) as u32,
        anim_width - 2,
        anim_height - 2,
        background_fade,
    );

    // Barra de título
    if window.config.decorations.title_bar {
        let title_bar_height = 30;
        fb.draw_rect(
            (anim_x + 1) as u32,
            (anim_y + 1) as u32,
            anim_width - 2,
            title_bar_height,
            title_bar_fade,
        );

        // Título de la ventana
        let title_text = if window.title.len() > 20 {
            alloc::format!("{}...", &window.title[..17])
        } else {
            window.title.clone()
        };

        let title_color = Color {
            r: (text_color.r as f32 * fade_alpha) as u8,
            g: (text_color.g as f32 * fade_alpha) as u8,
            b: (text_color.b as f32 * fade_alpha) as u8,
            a: 255,
        };

        fb.write_text_kernel_typing(
            (anim_x + 8) as u32,
            (anim_y + 8) as u32,
            &title_text,
            title_color,
        );

        // Botones de control
        if window.config.decorations.close_button
            || window.config.decorations.minimize_button
            || window.config.decorations.maximize_button
        {
            let button_size = 20;
            let button_y = anim_y + 5;
            let mut button_x = (anim_x + anim_width as i32) - 25;

            // Botón cerrar
            if window.config.decorations.close_button {
                let close_color = Color {
                    r: (Color::from_hex(0xff4444).r as f32 * fade_alpha) as u8,
                    g: (Color::from_hex(0xff4444).g as f32 * fade_alpha) as u8,
                    b: (Color::from_hex(0xff4444).b as f32 * fade_alpha) as u8,
                    a: 255,
                };
                fb.draw_rect(
                    button_x as u32,
                    button_y as u32,
                    button_size,
                    button_size,
                    close_color,
                );
                fb.write_text_kernel_typing(
                    (button_x + 6) as u32,
                    (button_y + 3) as u32,
                    "×",
                    title_color,
                );
                button_x -= 25;
            }

            // Botón maximizar
            if window.config.decorations.maximize_button {
                let max_color = Color {
                    r: (button_color.r as f32 * fade_alpha) as u8,
                    g: (button_color.g as f32 * fade_alpha) as u8,
                    b: (button_color.b as f32 * fade_alpha) as u8,
                    a: 255,
                };
                fb.draw_rect(
                    button_x as u32,
                    button_y as u32,
                    button_size,
                    button_size,
                    max_color,
                );
                fb.write_text_kernel_typing(
                    (button_x + 6) as u32,
                    (button_y + 3) as u32,
                    "□",
                    title_color,
                );
                button_x -= 25;
            }

            // Botón minimizar
            if window.config.decorations.minimize_button {
                let min_color = Color {
                    r: (button_color.r as f32 * fade_alpha) as u8,
                    g: (button_color.g as f32 * fade_alpha) as u8,
                    b: (button_color.b as f32 * fade_alpha) as u8,
                    a: 255,
                };
                fb.draw_rect(
                    button_x as u32,
                    button_y as u32,
                    button_size,
                    button_size,
                    min_color,
                );
                fb.write_text_kernel_typing(
                    (button_x + 6) as u32,
                    (button_y + 3) as u32,
                    "−",
                    title_color,
                );
            }
        }
    }

    // Contenido de la ventana (simulado)
    let content_y = anim_y + 35;
    let content_color = Color {
        r: (text_color.r as f32 * fade_alpha * 0.8) as u8,
        g: (text_color.g as f32 * fade_alpha * 0.8) as u8,
        b: (text_color.b as f32 * fade_alpha * 0.8) as u8,
        a: 255,
    };

    // Renderizar contenido de ejemplo
    match window.window_type {
        WindowType::Application => {
            fb.write_text_kernel_typing(
                (anim_x + 10) as u32,
                content_y as u32,
                "Aplicación",
                content_color,
            );
            fb.write_text_kernel_typing(
                (anim_x + 10) as u32,
                (content_y + 20) as u32,
                "Contenido de la ventana",
                content_color,
            );
            fb.write_text_kernel_typing(
                (anim_x + 10) as u32,
                (content_y + 40) as u32,
                "ID: ",
                content_color,
            );
            fb.write_text_kernel_typing(
                (anim_x + 30) as u32,
                (content_y + 40) as u32,
                &window.id.to_string(),
                content_color,
            );
        }
        WindowType::Dialog => {
            fb.write_text_kernel_typing(
                (anim_x + 10) as u32,
                content_y as u32,
                "Diálogo",
                content_color,
            );
            fb.write_text_kernel_typing(
                (anim_x + 10) as u32,
                (content_y + 20) as u32,
                "Mensaje del sistema",
                content_color,
            );
        }
        WindowType::Tooltip => {
            fb.write_text_kernel_typing(
                (anim_x + 10) as u32,
                content_y as u32,
                "Tooltip",
                content_color,
            );
        }
        WindowType::Splash => {
            fb.write_text_kernel_typing(
                (anim_x + 10) as u32,
                content_y as u32,
                "Splash Screen",
                content_color,
            );
            fb.write_text_kernel_typing(
                (anim_x + 10) as u32,
                (content_y + 20) as u32,
                "Cargando...",
                content_color,
            );
        }
    }

    // Indicador de estado
    let status_color = match window.state {
        WindowState::Normal => Color::from_hex(0x00ff00),
        WindowState::Minimized => Color::from_hex(0xffaa00),
        WindowState::Maximized => Color::from_hex(0x0088ff),
        WindowState::Hidden => Color::from_hex(0x666666),
    };

    let status_text = match window.state {
        WindowState::Normal => "Normal",
        WindowState::Minimized => "Minimizada",
        WindowState::Maximized => "Maximizada",
        WindowState::Hidden => "Oculta",
    };

    fb.write_text_kernel_typing(
        (anim_x + 10) as u32,
        (anim_y + anim_height as i32 - 20) as u32,
        status_text,
        status_color,
    );

    Ok(())
}
