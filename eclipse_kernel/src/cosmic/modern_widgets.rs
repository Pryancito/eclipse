//! Widgets modernos para COSMIC Desktop Environment
//! Proporciona componentes de interfaz de usuario modernos y reutilizables

use crate::drivers::framebuffer::{Color, FramebufferDriver};
use crate::math_utils::{atan2, sin, sqrt};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::ptr::NonNull;
use core::time::Duration;

// Importar el sistema de animaciones
use crate::cosmic::widget_animations::{
    AnimatableProperties, AnimationConfig, AnimationManager, AnimationState, AnimationType,
    EasingFunction,
};

/// Estado de un widget
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WidgetState {
    Normal,
    Hover,
    Pressed,
    Disabled,
    Focused,
}

/// Estilo de un widget
#[derive(Debug, Clone)]
pub struct WidgetStyle {
    pub background_color: Color,
    pub border_color: Color,
    pub text_color: Color,
    pub hover_color: Color,
    pub pressed_color: Color,
    pub disabled_color: Color,
    pub border_width: u32,
    pub corner_radius: f32,
    pub shadow_enabled: bool,
    pub shadow_color: Color,
    pub shadow_offset: (i32, i32),
    pub shadow_blur: f32,
}

impl Default for WidgetStyle {
    fn default() -> Self {
        Self {
            background_color: Color::from_hex(0x2d2d2d),
            border_color: Color::from_hex(0x404040),
            text_color: Color::from_hex(0xffffff),
            hover_color: Color::from_hex(0x3d3d3d),
            pressed_color: Color::from_hex(0x1d1d1d),
            disabled_color: Color::from_hex(0x1a1a1a),
            border_width: 1,
            corner_radius: 8.0,
            shadow_enabled: true,
            shadow_color: Color::from_hex(0x000000),
            shadow_offset: (0, 2),
            shadow_blur: 4.0,
        }
    }
}

/// Botón moderno
#[derive(Debug, Clone)]
pub struct ModernButton {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub text: String,
    pub style: WidgetStyle,
    pub state: WidgetState,
    pub enabled: bool,
    pub visible: bool,
    // Propiedades de animación
    pub animation_properties: AnimatableProperties,
    pub animation_id: Option<u32>,
    pub hover_animation_id: Option<u32>,
    pub click_animation_id: Option<u32>,
}

impl ModernButton {
    pub fn new(x: u32, y: u32, width: u32, height: u32, text: &str) -> Self {
        let mut animation_properties = AnimatableProperties::default();
        animation_properties.x = x as f32;
        animation_properties.y = y as f32;
        animation_properties.width = width as f32;
        animation_properties.height = height as f32;
        animation_properties.color = Color::from_hex(0x2d2d2d);
        animation_properties.border_radius = 8.0;

        Self {
            x,
            y,
            width,
            height,
            text: text.to_string(),
            style: WidgetStyle::default(),
            state: WidgetState::Normal,
            enabled: true,
            visible: true,
            animation_properties,
            animation_id: None,
            hover_animation_id: None,
            click_animation_id: None,
        }
    }

    pub fn with_style(mut self, style: WidgetStyle) -> Self {
        self.style = style;
        self
    }

    pub fn set_state(&mut self, state: WidgetState) {
        self.state = state;
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.state = WidgetState::Disabled;
        }
    }

    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    pub fn render(&self, fb: &mut FramebufferDriver) -> Result<(), String> {
        if !self.visible {
            return Ok(());
        }

        // Renderizar sombra si está habilitada
        if self.style.shadow_enabled && self.enabled {
            self.render_shadow(fb)?;
        }

        // Determinar color de fondo basado en el estado
        let bg_color = match (self.state, self.enabled) {
            (WidgetState::Disabled, _) => self.style.disabled_color,
            (WidgetState::Pressed, _) => self.style.pressed_color,
            (WidgetState::Hover, _) => self.style.hover_color,
            _ => self.style.background_color,
        };

        // Renderizar fondo con bordes redondeados
        self.render_rounded_rectangle(
            fb,
            self.x,
            self.y,
            self.width,
            self.height,
            self.style.corner_radius,
            bg_color,
        )?;

        // Renderizar borde
        if self.style.border_width > 0 {
            self.render_rounded_border(
                fb,
                self.x,
                self.y,
                self.width,
                self.height,
                self.style.corner_radius,
                self.style.border_width,
                self.style.border_color,
            )?;
        }

        // Renderizar texto centrado
        self.render_centered_text(fb)?;

        Ok(())
    }

    fn render_shadow(&self, fb: &mut FramebufferDriver) -> Result<(), String> {
        let shadow_x = (self.x as i32 + self.style.shadow_offset.0) as u32;
        let shadow_y = (self.y as i32 + self.style.shadow_offset.1) as u32;

        // Renderizar sombra con blur
        for dy in 0..(self.style.shadow_blur as u32 * 2) {
            for dx in 0..(self.style.shadow_blur as u32 * 2) {
                let px = shadow_x + dx;
                let py = shadow_y + dy;

                if px < fb.info.width && py < fb.info.height {
                    let dx_f32 = dx as f32 - self.style.shadow_blur;
                    let dy_f32 = dy as f32 - self.style.shadow_blur;
                    let distance = sqrt((dx_f32 * dx_f32 + dy_f32 * dy_f32) as f64) as f32;
                    let alpha = (1.0 - (distance / self.style.shadow_blur).min(1.0)) * 0.3;

                    if alpha > 0.0 {
                        let shadow_color = self.apply_alpha(self.style.shadow_color, alpha);
                        let _ = fb.put_pixel(px, py, shadow_color);
                    }
                }
            }
        }

        Ok(())
    }

    fn render_rounded_rectangle(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        radius: f32,
        color: Color,
    ) -> Result<(), String> {
        let radius = radius.min((width.min(height) as f32) / 2.0);

        for py in y..(y + height) {
            for px in x..(x + width) {
                if self.is_point_in_rounded_rect(px, py, x, y, width, height, radius) {
                    let _ = fb.put_pixel(px, py, color);
                }
            }
        }

        Ok(())
    }

    fn render_rounded_border(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        radius: f32,
        border_width: u32,
        color: Color,
    ) -> Result<(), String> {
        let radius = radius.min((width.min(height) as f32) / 2.0);

        for bw in 0..border_width {
            for py in y..(y + height) {
                for px in x..(x + width) {
                    if self.is_point_on_rounded_border(px, py, x, y, width, height, radius, bw) {
                        let _ = fb.put_pixel(px, py, color);
                    }
                }
            }
        }

        Ok(())
    }

    fn is_point_in_rounded_rect(
        &self,
        px: u32,
        py: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        radius: f32,
    ) -> bool {
        // Verificar si está dentro del rectángulo principal
        if px < x || px >= x + width || py < y || py >= y + height {
            return false;
        }

        // Verificar esquinas redondeadas
        let radius = radius as u32;

        // Esquina superior izquierda
        if px < x + radius && py < y + radius {
            let dx = (px - x) as f32;
            let dy = (py - y) as f32;
            let distance = sqrt((dx * dx + dy * dy) as f64) as f32;
            return distance <= radius as f32;
        }

        // Esquina superior derecha
        if px >= x + width - radius && py < y + radius {
            let dx = (px - (x + width - radius)) as f32;
            let dy = (py - y) as f32;
            let distance = sqrt((dx * dx + dy * dy) as f64) as f32;
            return distance <= radius as f32;
        }

        // Esquina inferior izquierda
        if px < x + radius && py >= y + height - radius {
            let dx = (px - x) as f32;
            let dy = (py - (y + height - radius)) as f32;
            let distance = sqrt((dx * dx + dy * dy) as f64) as f32;
            return distance <= radius as f32;
        }

        // Esquina inferior derecha
        if px >= x + width - radius && py >= y + height - radius {
            let dx = (px - (x + width - radius)) as f32;
            let dy = (py - (y + height - radius)) as f32;
            let distance = sqrt((dx * dx + dy * dy) as f64) as f32;
            return distance <= radius as f32;
        }

        true
    }

    fn is_point_on_rounded_border(
        &self,
        px: u32,
        py: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        radius: f32,
        border_width: u32,
    ) -> bool {
        // Verificar si está en el borde del rectángulo redondeado
        let radius = radius as u32;
        let outer_radius = radius;
        let inner_radius = if radius > border_width {
            radius - border_width
        } else {
            0
        };

        // Verificar si está dentro del rectángulo principal
        if px < x || px >= x + width || py < y || py >= y + height {
            return false;
        }

        // Verificar esquinas redondeadas
        let mut in_outer = false;
        let mut in_inner = false;

        // Esquina superior izquierda
        if px < x + outer_radius && py < y + outer_radius {
            let dx = (px - x) as f32;
            let dy = (py - y) as f32;
            let distance = sqrt((dx * dx + dy * dy) as f64) as f32;
            in_outer = distance <= (outer_radius as f32);
            if inner_radius > 0 {
                in_inner = distance <= (inner_radius as f32);
            }
        }
        // Esquina superior derecha
        else if px >= x + width - outer_radius && py < y + outer_radius {
            let dx = (px - (x + width - outer_radius)) as f32;
            let dy = (py - y) as f32;
            let distance = sqrt((dx * dx + dy * dy) as f64) as f32;
            in_outer = distance <= (outer_radius as f32);
            if inner_radius > 0 {
                in_inner = distance <= (inner_radius as f32);
            }
        }
        // Esquina inferior izquierda
        else if px < x + outer_radius && py >= y + height - outer_radius {
            let dx = (px - x) as f32;
            let dy = (py - (y + height - outer_radius)) as f32;
            let distance = sqrt((dx * dx + dy * dy) as f64) as f32;
            in_outer = distance <= (outer_radius as f32);
            if inner_radius > 0 {
                in_inner = distance <= (inner_radius as f32);
            }
        }
        // Esquina inferior derecha
        else if px >= x + width - outer_radius && py >= y + height - outer_radius {
            let dx = (px - (x + width - outer_radius)) as f32;
            let dy = (py - (y + height - outer_radius)) as f32;
            let distance = sqrt((dx * dx + dy * dy) as f64) as f32;
            in_outer = distance <= (outer_radius as f32);
            if inner_radius > 0 {
                in_inner = distance <= (inner_radius as f32);
            }
        }
        // Bordes rectos
        else {
            in_outer = true;
            if inner_radius > 0 {
                in_inner = (px >= x + inner_radius && px < x + width - inner_radius)
                    || (py >= y + inner_radius && py < y + height - inner_radius);
            }
        }

        in_outer && !in_inner
    }

    fn render_centered_text(&self, fb: &mut FramebufferDriver) -> Result<(), String> {
        let text_x = self.x + self.width / 2 - (self.text.len() as u32 * 8) / 2;
        let text_y = self.y + self.height / 2 - 8;

        let text_color = if self.enabled {
            self.style.text_color
        } else {
            Color::from_hex(0x666666)
        };

        let _ = fb.write_text_kernel_typing(text_x, text_y, &self.text, text_color);
        Ok(())
    }

    fn apply_alpha(&self, color: Color, alpha: f32) -> Color {
        let (r, g, b, a) = color.to_rgba();
        let new_alpha = (a as f32 * alpha) as u8;
        Color::from_rgba(r, g, b, new_alpha)
    }

    pub fn contains_point(&self, x: u32, y: u32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    pub fn handle_click(&mut self, x: u32, y: u32) -> bool {
        if !self.enabled || !self.visible {
            return false;
        }

        if self.contains_point(x, y) {
            self.state = WidgetState::Pressed;
            true
        } else {
            false
        }
    }

    pub fn handle_hover(&mut self, x: u32, y: u32) {
        if !self.enabled || !self.visible {
            return;
        }

        if self.contains_point(x, y) {
            if self.state != WidgetState::Pressed {
                self.state = WidgetState::Hover;
            }
        } else {
            self.state = WidgetState::Normal;
        }
    }

    /// Crear animación de hover
    pub fn create_hover_animation(&mut self, animation_manager: &mut AnimationManager) -> u32 {
        let start_color = self.style.background_color;
        let end_color = self.style.hover_color;

        let animation_id = animation_manager.create_color_animation(
            start_color,
            end_color,
            Duration::from_millis(200),
            EasingFunction::EaseOut,
        );

        self.hover_animation_id = Some(animation_id);
        animation_id
    }

    /// Crear animación de click
    pub fn create_click_animation(&mut self, animation_manager: &mut AnimationManager) -> u32 {
        let start_scale = 1.0;
        let end_scale = 0.95;

        let animation_id = animation_manager.create_scale_animation(
            start_scale,
            end_scale,
            Duration::from_millis(100),
            EasingFunction::EaseInOut,
        );

        self.click_animation_id = Some(animation_id);
        animation_id
    }

    /// Crear animación de fade in
    pub fn create_fade_in_animation(&mut self, animation_manager: &mut AnimationManager) -> u32 {
        let start_opacity = 0.0;
        let end_opacity = 1.0;

        let animation_id = animation_manager.create_opacity_animation(
            start_opacity,
            end_opacity,
            Duration::from_millis(300),
            EasingFunction::EaseOut,
        );

        self.animation_id = Some(animation_id);
        animation_id
    }

    /// Crear animación de fade out
    pub fn create_fade_out_animation(&mut self, animation_manager: &mut AnimationManager) -> u32 {
        let start_opacity = 1.0;
        let end_opacity = 0.0;

        let animation_id = animation_manager.create_opacity_animation(
            start_opacity,
            end_opacity,
            Duration::from_millis(200),
            EasingFunction::EaseIn,
        );

        self.animation_id = Some(animation_id);
        animation_id
    }

    /// Actualizar propiedades de animación
    pub fn update_animation_properties(&mut self, animation_manager: &AnimationManager) {
        if let Some(animation_id) = self.animation_id {
            if let Some(properties) = animation_manager.get_animation_properties(animation_id) {
                self.animation_properties = properties;
            }
        }

        if let Some(hover_id) = self.hover_animation_id {
            if let Some(properties) = animation_manager.get_animation_properties(hover_id) {
                self.animation_properties.color = properties.color;
            }
        }

        if let Some(click_id) = self.click_animation_id {
            if let Some(properties) = animation_manager.get_animation_properties(click_id) {
                self.animation_properties.scale_x = properties.scale_x;
                self.animation_properties.scale_y = properties.scale_y;
            }
        }
    }

    /// Renderizar con propiedades de animación
    pub fn render_animated(&self, fb: &mut FramebufferDriver) -> Result<(), String> {
        if !self.visible {
            return Ok(());
        }

        // Usar propiedades de animación si están disponibles
        let x = if self.animation_properties.x != 0.0 {
            self.animation_properties.x as u32
        } else {
            self.x
        };

        let y = if self.animation_properties.y != 0.0 {
            self.animation_properties.y as u32
        } else {
            self.y
        };

        let width = if self.animation_properties.width != 0.0 {
            self.animation_properties.width as u32
        } else {
            self.width
        };

        let height = if self.animation_properties.height != 0.0 {
            self.animation_properties.height as u32
        } else {
            self.height
        };

        // Aplicar escala si está animada
        let scaled_width = (width as f32 * self.animation_properties.scale_x) as u32;
        let scaled_height = (height as f32 * self.animation_properties.scale_y) as u32;

        // Centrar el widget escalado
        let offset_x = (width - scaled_width) / 2;
        let offset_y = (height - scaled_height) / 2;

        let final_x = x + offset_x;
        let final_y = y + offset_y;

        // Renderizar sombra si está habilitada
        if self.style.shadow_enabled && self.enabled {
            self.render_animated_shadow(fb, final_x, final_y, scaled_width, scaled_height)?;
        }

        // Determinar color de fondo basado en el estado y animación
        let bg_color = if self.animation_properties.color != Color::from_hex(0x2d2d2d) {
            self.animation_properties.color
        } else {
            match (self.state, self.enabled) {
                (WidgetState::Disabled, _) => self.style.disabled_color,
                (WidgetState::Pressed, _) => self.style.pressed_color,
                (WidgetState::Hover, _) => self.style.hover_color,
                _ => self.style.background_color,
            }
        };

        // Aplicar opacidad si está animada
        let final_color = if self.animation_properties.opacity < 1.0 {
            self.apply_alpha(bg_color, self.animation_properties.opacity)
        } else {
            bg_color
        };

        // Renderizar fondo con bordes redondeados
        self.render_rounded_rectangle(
            fb,
            final_x,
            final_y,
            scaled_width,
            scaled_height,
            self.animation_properties.border_radius,
            final_color,
        )?;

        // Renderizar borde
        if self.style.border_width > 0 {
            self.render_rounded_border(
                fb,
                final_x,
                final_y,
                scaled_width,
                scaled_height,
                self.animation_properties.border_radius,
                self.style.border_width,
                self.style.border_color,
            )?;
        }

        // Renderizar texto centrado
        self.render_centered_text_at(fb, final_x, final_y, scaled_width, scaled_height)?;

        Ok(())
    }

    /// Renderizar sombra animada
    fn render_animated_shadow(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let shadow_x = (x as i32 + self.style.shadow_offset.0) as u32;
        let shadow_y = (y as i32 + self.style.shadow_offset.1) as u32;

        let shadow_blur = if self.animation_properties.shadow_blur > 0.0 {
            self.animation_properties.shadow_blur
        } else {
            self.style.shadow_blur
        };

        // Renderizar sombra con blur
        for dy in 0..(shadow_blur as u32 * 2) {
            for dx in 0..(shadow_blur as u32 * 2) {
                let px = shadow_x + dx;
                let py = shadow_y + dy;

                if px < fb.info.width && py < fb.info.height {
                    let dx_f32 = dx as f32 - shadow_blur;
                    let dy_f32 = dy as f32 - shadow_blur;
                    let distance = sqrt((dx_f32 * dx_f32 + dy_f32 * dy_f32) as f64) as f32;
                    let alpha = (1.0 - (distance / shadow_blur).min(1.0)) * 0.3;

                    if alpha > 0.0 {
                        let shadow_color = self.apply_alpha(self.style.shadow_color, alpha);
                        let _ = fb.put_pixel(px, py, shadow_color);
                    }
                }
            }
        }

        Ok(())
    }

    /// Renderizar texto centrado en posición específica
    fn render_centered_text_at(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let text_x = x + width / 2 - (self.text.len() as u32 * 4) / 2; // Aproximación simple
        let text_y = y + height / 2 - 4; // Aproximación simple

        let text_color = if self.animation_properties.opacity < 1.0 {
            self.apply_alpha(self.style.text_color, self.animation_properties.opacity)
        } else {
            self.style.text_color
        };

        let _ = fb.write_text_kernel_typing(text_x, text_y, &self.text, text_color);
        Ok(())
    }
}

/// Barra de progreso moderna
#[derive(Debug, Clone)]
pub struct ModernProgressBar {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub progress: f32, // 0.0 a 1.0
    pub style: WidgetStyle,
    pub visible: bool,
    pub animated: bool,
    pub animation_speed: f32,
}

impl ModernProgressBar {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
            progress: 0.0,
            style: WidgetStyle::default(),
            visible: true,
            animated: true,
            animation_speed: 0.02,
        }
    }

    pub fn set_progress(&mut self, progress: f32) {
        self.progress = progress.clamp(0.0, 1.0);
    }

    pub fn render(&self, fb: &mut FramebufferDriver) -> Result<(), String> {
        if !self.visible {
            return Ok(());
        }

        // Renderizar fondo
        self.render_rounded_rectangle(
            fb,
            self.x,
            self.y,
            self.width,
            self.height,
            self.style.corner_radius,
            self.style.background_color,
        )?;

        // Renderizar barra de progreso
        if self.progress > 0.0 {
            let progress_width = (self.width as f32 * self.progress) as u32;
            if progress_width > 0 {
                self.render_rounded_rectangle(
                    fb,
                    self.x,
                    self.y,
                    progress_width,
                    self.height,
                    self.style.corner_radius,
                    self.style.hover_color,
                )?;
            }
        }

        // Renderizar borde
        if self.style.border_width > 0 {
            self.render_rounded_border(
                fb,
                self.x,
                self.y,
                self.width,
                self.height,
                self.style.corner_radius,
                self.style.border_width,
                self.style.border_color,
            )?;
        }

        Ok(())
    }

    fn render_rounded_rectangle(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        radius: f32,
        color: Color,
    ) -> Result<(), String> {
        let radius = radius.min((width.min(height) as f32) / 2.0);

        for py in y..(y + height) {
            for px in x..(x + width) {
                if self.is_point_in_rounded_rect(px, py, x, y, width, height, radius) {
                    let _ = fb.put_pixel(px, py, color);
                }
            }
        }

        Ok(())
    }

    fn render_rounded_border(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        radius: f32,
        border_width: u32,
        color: Color,
    ) -> Result<(), String> {
        let radius = radius.min((width.min(height) as f32) / 2.0);

        for bw in 0..border_width {
            for py in y..(y + height) {
                for px in x..(x + width) {
                    if self.is_point_on_rounded_border(px, py, x, y, width, height, radius, bw) {
                        let _ = fb.put_pixel(px, py, color);
                    }
                }
            }
        }

        Ok(())
    }

    fn is_point_in_rounded_rect(
        &self,
        px: u32,
        py: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        radius: f32,
    ) -> bool {
        // Implementación similar a ModernButton
        if px < x || px >= x + width || py < y || py >= y + height {
            return false;
        }

        let radius = radius as u32;

        // Verificar esquinas redondeadas
        if px < x + radius && py < y + radius {
            let dx = (px - x) as f32;
            let dy = (py - y) as f32;
            let distance = sqrt((dx * dx + dy * dy) as f64) as f32;
            return distance <= radius as f32;
        }

        if px >= x + width - radius && py < y + radius {
            let dx = (px - (x + width - radius)) as f32;
            let dy = (py - y) as f32;
            let distance = sqrt((dx * dx + dy * dy) as f64) as f32;
            return distance <= radius as f32;
        }

        if px < x + radius && py >= y + height - radius {
            let dx = (px - x) as f32;
            let dy = (py - (y + height - radius)) as f32;
            let distance = sqrt((dx * dx + dy * dy) as f64) as f32;
            return distance <= radius as f32;
        }

        if px >= x + width - radius && py >= y + height - radius {
            let dx = (px - (x + width - radius)) as f32;
            let dy = (py - (y + height - radius)) as f32;
            let distance = sqrt((dx * dx + dy * dy) as f64) as f32;
            return distance <= radius as f32;
        }

        true
    }

    fn is_point_on_rounded_border(
        &self,
        px: u32,
        py: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        radius: f32,
        border_width: u32,
    ) -> bool {
        // Implementación similar a ModernButton
        let radius = radius as u32;
        let outer_radius = radius;
        let inner_radius = if radius > border_width {
            radius - border_width
        } else {
            0
        };

        if px < x || px >= x + width || py < y || py >= y + height {
            return false;
        }

        let mut in_outer = false;
        let mut in_inner = false;

        if px < x + outer_radius && py < y + outer_radius {
            let dx = (px - x) as f32;
            let dy = (py - y) as f32;
            let distance = sqrt((dx * dx + dy * dy) as f64) as f32;
            in_outer = distance <= (outer_radius as f32);
            if inner_radius > 0 {
                in_inner = distance <= (inner_radius as f32);
            }
        } else if px >= x + width - outer_radius && py < y + outer_radius {
            let dx = (px - (x + width - outer_radius)) as f32;
            let dy = (py - y) as f32;
            let distance = sqrt((dx * dx + dy * dy) as f64) as f32;
            in_outer = distance <= (outer_radius as f32);
            if inner_radius > 0 {
                in_inner = distance <= (inner_radius as f32);
            }
        } else if px < x + outer_radius && py >= y + height - outer_radius {
            let dx = (px - x) as f32;
            let dy = (py - (y + height - outer_radius)) as f32;
            let distance = sqrt((dx * dx + dy * dy) as f64) as f32;
            in_outer = distance <= (outer_radius as f32);
            if inner_radius > 0 {
                in_inner = distance <= (inner_radius as f32);
            }
        } else if px >= x + width - outer_radius && py >= y + height - outer_radius {
            let dx = (px - (x + width - outer_radius)) as f32;
            let dy = (py - (y + height - outer_radius)) as f32;
            let distance = sqrt((dx * dx + dy * dy) as f64) as f32;
            in_outer = distance <= (outer_radius as f32);
            if inner_radius > 0 {
                in_inner = distance <= (inner_radius as f32);
            }
        } else {
            in_outer = true;
            if inner_radius > 0 {
                in_inner = (px >= x + inner_radius && px < x + width - inner_radius)
                    || (py >= y + inner_radius && py < y + height - inner_radius);
            }
        }

        in_outer && !in_inner
    }
}

/// Gestor de widgets modernos
#[derive(Debug)]
pub struct ModernWidgetManager {
    pub buttons: Vec<ModernButton>,
    pub progress_bars: Vec<ModernProgressBar>,
    pub screen_width: u32,
    pub screen_height: u32,
    pub animation_manager: AnimationManager,
    pub current_time: u64,
}

impl ModernWidgetManager {
    pub fn new(screen_width: u32, screen_height: u32) -> Self {
        Self {
            buttons: Vec::new(),
            progress_bars: Vec::new(),
            screen_width,
            screen_height,
            animation_manager: AnimationManager::new(),
            current_time: 0,
        }
    }

    pub fn add_button(&mut self, button: ModernButton) {
        self.buttons.push(button);
    }

    pub fn add_progress_bar(&mut self, progress_bar: ModernProgressBar) {
        self.progress_bars.push(progress_bar);
    }

    pub fn render_all(&self, fb: &mut FramebufferDriver) -> Result<(), String> {
        // Renderizar barras de progreso primero (fondo)
        for progress_bar in &self.progress_bars {
            progress_bar.render(fb)?;
        }

        // Renderizar botones encima
        for button in &self.buttons {
            button.render(fb)?;
        }

        Ok(())
    }

    pub fn handle_click(&mut self, x: u32, y: u32) -> bool {
        // Procesar clics en orden inverso (último renderizado primero)
        for button in self.buttons.iter_mut().rev() {
            if button.handle_click(x, y) {
                return true;
            }
        }
        false
    }

    pub fn handle_hover(&mut self, x: u32, y: u32) {
        for button in &mut self.buttons {
            button.handle_hover(x, y);
        }
    }

    pub fn update_progress(&mut self, index: usize, progress: f32) -> Result<(), String> {
        if index < self.progress_bars.len() {
            self.progress_bars[index].set_progress(progress);
            Ok(())
        } else {
            Err("Índice de barra de progreso inválido".to_string())
        }
    }

    /// Actualizar el tiempo actual para las animaciones
    pub fn update_time(&mut self, current_time: u64) {
        self.current_time = current_time;
        self.animation_manager.update_time(current_time);
    }

    /// Renderizar todos los widgets con animaciones
    pub fn render_all_animated(&mut self, fb: &mut FramebufferDriver) -> Result<(), String> {
        // Actualizar animaciones
        self.animation_manager.update_animations();

        // Actualizar propiedades de animación de los botones
        for button in &mut self.buttons {
            button.update_animation_properties(&self.animation_manager);
        }

        // Renderizar barras de progreso primero (fondo)
        for progress_bar in &self.progress_bars {
            progress_bar.render(fb)?;
        }

        // Renderizar botones con animaciones
        for button in &self.buttons {
            button.render_animated(fb)?;
        }

        Ok(())
    }

    /// Crear animación de hover para un botón
    pub fn create_button_hover_animation(&mut self, button_index: usize) -> Result<u32, String> {
        if button_index < self.buttons.len() {
            let animation_id =
                self.buttons[button_index].create_hover_animation(&mut self.animation_manager);
            Ok(animation_id)
        } else {
            Err("Índice de botón inválido".to_string())
        }
    }

    /// Crear animación de click para un botón
    pub fn create_button_click_animation(&mut self, button_index: usize) -> Result<u32, String> {
        if button_index < self.buttons.len() {
            let animation_id =
                self.buttons[button_index].create_click_animation(&mut self.animation_manager);
            Ok(animation_id)
        } else {
            Err("Índice de botón inválido".to_string())
        }
    }

    /// Crear animación de fade in para un botón
    pub fn create_button_fade_in_animation(&mut self, button_index: usize) -> Result<u32, String> {
        if button_index < self.buttons.len() {
            let animation_id =
                self.buttons[button_index].create_fade_in_animation(&mut self.animation_manager);
            Ok(animation_id)
        } else {
            Err("Índice de botón inválido".to_string())
        }
    }

    /// Crear animación de fade out para un botón
    pub fn create_button_fade_out_animation(&mut self, button_index: usize) -> Result<u32, String> {
        if button_index < self.buttons.len() {
            let animation_id =
                self.buttons[button_index].create_fade_out_animation(&mut self.animation_manager);
            Ok(animation_id)
        } else {
            Err("Índice de botón inválido".to_string())
        }
    }

    /// Iniciar una animación
    pub fn start_animation(&mut self, animation_id: u32) -> bool {
        self.animation_manager.start_animation(animation_id)
    }

    /// Pausar una animación
    pub fn pause_animation(&mut self, animation_id: u32) -> bool {
        self.animation_manager.pause_animation(animation_id)
    }

    /// Cancelar una animación
    pub fn cancel_animation(&mut self, animation_id: u32) -> bool {
        self.animation_manager.cancel_animation(animation_id)
    }

    /// Obtener el número de animaciones activas
    pub fn active_animations_count(&self) -> usize {
        self.animation_manager.active_animations_count()
    }

    /// Limpiar todas las animaciones
    pub fn clear_animations(&mut self) {
        self.animation_manager.clear_animations();
    }

    /// Manejar hover con animaciones
    pub fn handle_hover_animated(&mut self, x: u32, y: u32) {
        // Primero actualizar todos los estados de hover y crear animaciones
        for (index, button) in self.buttons.iter_mut().enumerate() {
            let was_hovered = button.state == WidgetState::Hover;
            button.handle_hover(x, y);
            let is_hovered = button.state == WidgetState::Hover;

            // Crear animación de hover si el estado cambió
            if !was_hovered && is_hovered {
                // Crear animación de hover
                let animation_id = self.animation_manager.create_color_animation(
                    button.style.background_color,
                    button.style.hover_color,
                    Duration::from_millis(200),
                    EasingFunction::EaseOut,
                );

                // Asignar el ID de animación al botón
                button.hover_animation_id = Some(animation_id);

                // Iniciar la animación
                let _ = self.animation_manager.start_animation(animation_id);
            }
        }
    }

    /// Manejar click con animaciones
    pub fn handle_click_animated(&mut self, x: u32, y: u32) -> bool {
        let mut any_clicked = false;

        // Procesar cada botón y crear animaciones si es necesario
        for button in &mut self.buttons {
            if button.handle_click(x, y) {
                any_clicked = true;

                // Crear animación de click
                let animation_id = self.animation_manager.create_scale_animation(
                    1.0,
                    0.95,
                    Duration::from_millis(100),
                    EasingFunction::EaseInOut,
                );

                // Asignar el ID de animación al botón
                button.click_animation_id = Some(animation_id);

                // Iniciar la animación
                let _ = self.animation_manager.start_animation(animation_id);
            }
        }

        any_clicked
    }
}
