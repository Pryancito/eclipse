// USERLAND: use crate::drivers::framebuffer::{Color, FramebufferDriver};
use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Sistema de widgets flotantes interactivos para COSMIC
pub struct FloatingWidgetSystem {
    /// Widgets activos
    widgets: VecDeque<FloatingWidget>,
    /// Configuración del sistema
    config: FloatingWidgetConfig,
    /// Estadísticas del sistema
    stats: FloatingWidgetStats,
    /// Widget interactivo actual
    active_widget: Option<String>,
    /// Posición del mouse
    mouse_position: (f32, f32),
}

/// Configuración del sistema de widgets flotantes
#[derive(Debug, Clone)]
pub struct FloatingWidgetConfig {
    /// Habilitar widgets flotantes
    pub enabled: bool,
    /// Máximo número de widgets simultáneos
    pub max_widgets: usize,
    /// Velocidad de animación
    pub animation_speed: f32,
    /// Tamaño mínimo de widget
    pub min_size: (u32, u32),
    /// Tamaño máximo de widget
    pub max_size: (u32, u32),
    /// Habilitar interacciones
    pub enable_interactions: bool,
    /// Habilitar efectos de hover
    pub enable_hover_effects: bool,
}

impl Default for FloatingWidgetConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_widgets: 8,
            animation_speed: 1.0,
            min_size: (100, 80),
            max_size: (300, 200),
            enable_interactions: true,
            enable_hover_effects: true,
        }
    }
}

/// Estadísticas del sistema de widgets flotantes
#[derive(Debug, Clone)]
pub struct FloatingWidgetStats {
    /// Total de widgets creados
    pub total_widgets: usize,
    /// Widgets activos actualmente
    pub active_widgets: usize,
    /// Widgets por tipo
    pub widgets_by_type: [usize; 5], // Clock, Weather, System, Media, Custom
    /// Interacciones registradas
    pub interactions_count: usize,
    /// FPS de renderizado
    pub render_fps: f32,
}

/// Widget flotante individual
#[derive(Debug, Clone)]
pub struct FloatingWidget {
    /// ID único del widget
    pub id: String,
    /// Tipo de widget
    pub widget_type: WidgetType,
    /// Posición actual
    pub position: (f32, f32),
    /// Tamaño actual
    pub size: (u32, u32),
    /// Estado del widget
    pub state: WidgetState,
    /// Contenido del widget
    pub content: WidgetContent,
    /// Animación del widget
    pub animation: WidgetAnimation,
    /// Configuración específica
    pub settings: WidgetSettings,
}

/// Tipo de widget
#[derive(Debug, Clone, PartialEq)]
pub enum WidgetType {
    /// Reloj digital
    Clock,
    /// Información del clima
    Weather,
    /// Monitor del sistema
    SystemMonitor,
    /// Control de medios
    MediaControl,
    /// Widget personalizado
    Custom,
}

/// Estado del widget
#[derive(Debug, Clone, PartialEq)]
pub enum WidgetState {
    /// Apareciendo
    Appearing,
    /// Visible
    Visible,
    /// Interactuando
    Interacting,
    /// Desapareciendo
    Disappearing,
    /// Oculto
    Hidden,
}

/// Contenido del widget
#[derive(Debug, Clone)]
pub enum WidgetContent {
    /// Contenido de reloj
    Clock { time: String, date: String },
    /// Contenido del clima
    Weather {
        temperature: f32,
        condition: String,
        humidity: f32,
    },
    /// Contenido del monitor del sistema
    SystemMonitor { cpu: f32, memory: f32, disk: f32 },
    /// Contenido de control de medios
    MediaControl {
        title: String,
        artist: String,
        is_playing: bool,
    },
    /// Contenido personalizado
    Custom { text: String, data: Vec<f32> },
}

/// Animación del widget
#[derive(Debug, Clone)]
pub struct WidgetAnimation {
    /// Tiempo de animación
    pub time: f32,
    /// Velocidad de animación
    pub speed: f32,
    /// Escala actual
    pub scale: f32,
    /// Rotación actual
    pub rotation: f32,
    /// Opacidad actual
    pub opacity: f32,
    /// Efecto de hover
    pub hover_effect: f32,
    /// Efecto de pulso
    pub pulse_effect: f32,
}

/// Configuración específica del widget
#[derive(Debug, Clone)]
pub struct WidgetSettings {
    /// Color de fondo
    pub background_color: Color,
    /// Color del borde
    pub border_color: Color,
    /// Color del texto
    pub text_color: Color,
    /// Habilitar sombra
    pub enable_shadow: bool,
    /// Habilitar efectos
    pub enable_effects: bool,
    /// Transparencia
    pub transparency: f32,
}

impl FloatingWidgetSystem {
    /// Crear nuevo sistema de widgets flotantes
    pub fn new() -> Self {
        Self {
            widgets: VecDeque::new(),
            config: FloatingWidgetConfig::default(),
            stats: FloatingWidgetStats {
                total_widgets: 0,
                active_widgets: 0,
                widgets_by_type: [0, 0, 0, 0, 0],
                interactions_count: 0,
                render_fps: 0.0,
            },
            active_widget: None,
            mouse_position: (0.0, 0.0),
        }
    }

    /// Crear sistema con configuración personalizada
    pub fn with_config(config: FloatingWidgetConfig) -> Self {
        Self {
            widgets: VecDeque::new(),
            config,
            stats: FloatingWidgetStats {
                total_widgets: 0,
                active_widgets: 0,
                widgets_by_type: [0, 0, 0, 0, 0],
                interactions_count: 0,
                render_fps: 0.0,
            },
            active_widget: None,
            mouse_position: (0.0, 0.0),
        }
    }

    /// Agregar un nuevo widget
    pub fn add_widget(
        &mut self,
        widget_type: WidgetType,
        position: (f32, f32),
        size: (u32, u32),
    ) -> String {
        if !self.config.enabled {
            return String::from("");
        }

        let widget_id = alloc::format!("widget_{}", self.stats.total_widgets);
        let content = self.create_widget_content(&widget_type);

        let widget_type_clone = widget_type.clone();
        let widget = FloatingWidget {
            id: widget_id.clone(),
            widget_type,
            position,
            size,
            state: WidgetState::Appearing,
            content,
            animation: WidgetAnimation {
                time: 0.0,
                speed: self.config.animation_speed,
                scale: 0.0,
                rotation: 0.0,
                opacity: 0.0,
                hover_effect: 0.0,
                pulse_effect: 0.0,
            },
            settings: self.create_widget_settings(&widget_type_clone),
        };

        self.widgets.push_front(widget);

        // Limitar número de widgets
        while self.widgets.len() > self.config.max_widgets {
            self.widgets.pop_back();
        }

        self.stats.total_widgets += 1;
        self.update_stats();

        widget_id
    }

    /// Crear contenido del widget según su tipo
    fn create_widget_content(&self, widget_type: &WidgetType) -> WidgetContent {
        match widget_type {
            WidgetType::Clock => WidgetContent::Clock {
                time: String::from("12:34:56"),
                date: String::from("2024-01-01"),
            },
            WidgetType::Weather => WidgetContent::Weather {
                temperature: 22.5,
                condition: String::from("Soleado"),
                humidity: 65.0,
            },
            WidgetType::SystemMonitor => WidgetContent::SystemMonitor {
                cpu: 45.0,
                memory: 67.0,
                disk: 23.0,
            },
            WidgetType::MediaControl => WidgetContent::MediaControl {
                title: String::from("Eclipse OS Theme"),
                artist: String::from("COSMIC Audio"),
                is_playing: true,
            },
            WidgetType::Custom => WidgetContent::Custom {
                text: String::from("Widget Personalizado"),
                data: Vec::from([1.0, 2.0, 3.0, 4.0, 5.0]),
            },
        }
    }

    /// Crear configuración del widget según su tipo
    fn create_widget_settings(&self, widget_type: &WidgetType) -> WidgetSettings {
        match widget_type {
            WidgetType::Clock => WidgetSettings {
                background_color: Color {
                    r: 20,
                    g: 40,
                    b: 80,
                    a: 200,
                },
                border_color: Color {
                    r: 0,
                    g: 150,
                    b: 255,
                    a: 255,
                },
                text_color: Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: 255,
                },
                enable_shadow: true,
                enable_effects: true,
                transparency: 0.8,
            },
            WidgetType::Weather => WidgetSettings {
                background_color: Color {
                    r: 40,
                    g: 80,
                    b: 160,
                    a: 200,
                },
                border_color: Color {
                    r: 100,
                    g: 200,
                    b: 255,
                    a: 255,
                },
                text_color: Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: 255,
                },
                enable_shadow: true,
                enable_effects: true,
                transparency: 0.8,
            },
            WidgetType::SystemMonitor => WidgetSettings {
                background_color: Color {
                    r: 80,
                    g: 40,
                    b: 20,
                    a: 200,
                },
                border_color: Color {
                    r: 255,
                    g: 150,
                    b: 0,
                    a: 255,
                },
                text_color: Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: 255,
                },
                enable_shadow: true,
                enable_effects: true,
                transparency: 0.8,
            },
            WidgetType::MediaControl => WidgetSettings {
                background_color: Color {
                    r: 40,
                    g: 20,
                    b: 80,
                    a: 200,
                },
                border_color: Color {
                    r: 200,
                    g: 100,
                    b: 255,
                    a: 255,
                },
                text_color: Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: 255,
                },
                enable_shadow: true,
                enable_effects: true,
                transparency: 0.8,
            },
            WidgetType::Custom => WidgetSettings {
                background_color: Color {
                    r: 60,
                    g: 60,
                    b: 60,
                    a: 200,
                },
                border_color: Color {
                    r: 150,
                    g: 150,
                    b: 150,
                    a: 255,
                },
                text_color: Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: 255,
                },
                enable_shadow: true,
                enable_effects: true,
                transparency: 0.8,
            },
        }
    }

    /// Actualizar todos los widgets
    pub fn update(&mut self, delta_time: f32) {
        if !self.config.enabled {
            return;
        }

        for widget in &mut self.widgets {
            widget.animation.time += delta_time * widget.animation.speed;

            match widget.state {
                WidgetState::Appearing => {
                    widget.animation.opacity += delta_time * 2.0;
                    widget.animation.scale += delta_time * 2.0;
                    if widget.animation.opacity >= 1.0 {
                        widget.animation.opacity = 1.0;
                        widget.animation.scale = 1.0;
                        widget.state = WidgetState::Visible;
                    }
                }
                WidgetState::Visible => {
                    // Efectos de animación continua (aproximación de sin para no_std)
                    widget.animation.pulse_effect =
                        ((widget.animation.time * 2.0) * 0.5 + 1.0) * 0.1 + 1.0;
                    widget.animation.rotation += delta_time * 0.5;
                }
                WidgetState::Interacting => {
                    widget.animation.hover_effect =
                        ((widget.animation.time * 4.0) * 0.5 + 1.0) * 0.2 + 1.0;
                }
                WidgetState::Disappearing => {
                    widget.animation.opacity -= delta_time * 2.0;
                    if widget.animation.opacity <= 0.0 {
                        widget.animation.opacity = 0.0;
                        widget.state = WidgetState::Hidden;
                    }
                }
                WidgetState::Hidden => {
                    // Widget oculto
                }
            }
        }

        // Eliminar widgets ocultos
        self.widgets
            .retain(|widget| widget.state != WidgetState::Hidden);
        self.update_stats();
    }

    /// Renderizar todos los widgets
    pub fn render(&mut self, fb: &mut FramebufferDriver) -> Result<(), String> {
        if !self.config.enabled {
            return Ok(());
        }

        // Crear una copia de los widgets para evitar problemas de borrowing
        let widgets_copy: Vec<_> = self.widgets.iter().collect();

        for widget in widgets_copy {
            if widget.state != WidgetState::Hidden {
                Self::render_single_widget(fb, widget)?;
            }
        }

        Ok(())
    }

    /// Renderizar un widget individual
    fn render_single_widget(
        fb: &mut FramebufferDriver,
        widget: &FloatingWidget,
    ) -> Result<(), String> {
        let width = widget.size.0;
        let height = widget.size.1;
        let x = widget.position.0 as u32;
        let y = widget.position.1 as u32;

        // Calcular colores con efectos
        let bg_color = Color {
            r: (widget.settings.background_color.r as f32
                * widget.animation.opacity
                * widget.animation.scale) as u8,
            g: (widget.settings.background_color.g as f32
                * widget.animation.opacity
                * widget.animation.scale) as u8,
            b: (widget.settings.background_color.b as f32
                * widget.animation.opacity
                * widget.animation.scale) as u8,
            a: (widget.settings.background_color.a as f32 * widget.animation.opacity) as u8,
        };

        let border_color = Color {
            r: (widget.settings.border_color.r as f32
                * widget.animation.opacity
                * widget.animation.hover_effect) as u8,
            g: (widget.settings.border_color.g as f32
                * widget.animation.opacity
                * widget.animation.hover_effect) as u8,
            b: (widget.settings.border_color.b as f32
                * widget.animation.opacity
                * widget.animation.hover_effect) as u8,
            a: (widget.settings.border_color.a as f32 * widget.animation.opacity) as u8,
        };

        // Renderizar sombra si está habilitada
        if widget.settings.enable_shadow {
            let shadow_color = Color {
                r: 0,
                g: 0,
                b: 0,
                a: 50,
            };
            fb.draw_rect(x + 3, y + 3, width, height, shadow_color);
        }

        // Renderizar fondo del widget
        fb.draw_rect(x, y, width, height, bg_color);

        // Renderizar borde
        fb.draw_rect(x, y, width, 2, border_color);
        fb.draw_rect(x, y, 2, height, border_color);
        fb.draw_rect(x + width - 2, y, 2, height, border_color);
        fb.draw_rect(x, y + height - 2, width, 2, border_color);

        // Renderizar contenido según el tipo
        Self::render_widget_content(fb, widget)?;

        Ok(())
    }

    /// Renderizar contenido del widget
    fn render_widget_content(
        fb: &mut FramebufferDriver,
        widget: &FloatingWidget,
    ) -> Result<(), String> {
        let text_color = Color {
            r: (widget.settings.text_color.r as f32 * widget.animation.opacity) as u8,
            g: (widget.settings.text_color.g as f32 * widget.animation.opacity) as u8,
            b: (widget.settings.text_color.b as f32 * widget.animation.opacity) as u8,
            a: (widget.settings.text_color.a as f32 * widget.animation.opacity) as u8,
        };

        let x = widget.position.0 as u32 + 10;
        let y = widget.position.1 as u32 + 20;

        match &widget.content {
            WidgetContent::Clock { time, date } => {
                fb.write_text_kernel_typing(x, y, time, text_color);
                fb.write_text_kernel_typing(x, y + 20, date, text_color);
            }
            WidgetContent::Weather {
                temperature,
                condition,
                humidity,
            } => {
                let temp_text = alloc::format!("{:.1}°C", temperature);
                let humidity_text = alloc::format!("Humedad: {:.0}%", humidity);
                fb.write_text_kernel_typing(x, y, &temp_text, text_color);
                fb.write_text_kernel_typing(x, y + 20, condition, text_color);
                fb.write_text_kernel_typing(x, y + 40, &humidity_text, text_color);
            }
            WidgetContent::SystemMonitor { cpu, memory, disk } => {
                let cpu_text = alloc::format!("CPU: {:.1}%", cpu);
                let mem_text = alloc::format!("RAM: {:.1}%", memory);
                let disk_text = alloc::format!("Disco: {:.1}%", disk);
                fb.write_text_kernel_typing(x, y, &cpu_text, text_color);
                fb.write_text_kernel_typing(x, y + 20, &mem_text, text_color);
                fb.write_text_kernel_typing(x, y + 40, &disk_text, text_color);
            }
            WidgetContent::MediaControl {
                title,
                artist,
                is_playing,
            } => {
                let status = if *is_playing { "▶️" } else { "⏸️" };
                fb.write_text_kernel_typing(x, y, status, text_color);
                fb.write_text_kernel_typing(x, y + 20, title, text_color);
                fb.write_text_kernel_typing(x, y + 40, artist, text_color);
            }
            WidgetContent::Custom { text, data } => {
                fb.write_text_kernel_typing(x, y, text, text_color);
                // Renderizar datos como barras simples
                for (i, value) in data.iter().enumerate() {
                    let bar_width = (value * 50.0) as u32;
                    let bar_color = Color {
                        r: (value * 255.0) as u8,
                        g: ((1.0 - value) * 255.0) as u8,
                        b: 128,
                        a: 255,
                    };
                    fb.draw_rect(x + (i as u32 * 15), y + 30, bar_width, 10, bar_color);
                }
            }
        }

        Ok(())
    }

    /// Actualizar estadísticas
    fn update_stats(&mut self) {
        self.stats.active_widgets = self
            .widgets
            .iter()
            .filter(|w| w.state != WidgetState::Hidden)
            .count();

        // Contar widgets por tipo
        self.stats.widgets_by_type = [0, 0, 0, 0, 0];
        for widget in &self.widgets {
            match widget.widget_type {
                WidgetType::Clock => self.stats.widgets_by_type[0] += 1,
                WidgetType::Weather => self.stats.widgets_by_type[1] += 1,
                WidgetType::SystemMonitor => self.stats.widgets_by_type[2] += 1,
                WidgetType::MediaControl => self.stats.widgets_by_type[3] += 1,
                WidgetType::Custom => self.stats.widgets_by_type[4] += 1,
            }
        }
    }

    /// Obtener estadísticas
    pub fn get_stats(&self) -> &FloatingWidgetStats {
        &self.stats
    }

    /// Configurar el sistema
    pub fn configure(&mut self, config: FloatingWidgetConfig) {
        self.config = config;
    }

    /// Obtener configuración
    pub fn get_config(&self) -> &FloatingWidgetConfig {
        &self.config
    }

    /// Limpiar todos los widgets
    pub fn clear_widgets(&mut self) {
        self.widgets.clear();
        self.update_stats();
    }

    /// Habilitar/deshabilitar widgets flotantes
    pub fn set_enabled(&mut self, enabled: bool) {
        self.config.enabled = enabled;
    }

    /// Crear widgets de ejemplo
    pub fn create_sample_widgets(&mut self) -> Vec<String> {
        let mut widget_ids = Vec::new();

        // Widget de reloj
        let clock_id = self.add_widget(WidgetType::Clock, (100.0, 100.0), (150, 80));
        widget_ids.push(clock_id);

        // Widget del clima
        let weather_id = self.add_widget(WidgetType::Weather, (300.0, 100.0), (150, 100));
        widget_ids.push(weather_id);

        // Widget del monitor del sistema
        let system_id = self.add_widget(WidgetType::SystemMonitor, (500.0, 100.0), (150, 100));
        widget_ids.push(system_id);

        // Widget de control de medios
        let media_id = self.add_widget(WidgetType::MediaControl, (700.0, 100.0), (150, 100));
        widget_ids.push(media_id);

        widget_ids
    }
}
