use crate::cosmic::advanced_compositor::{
    AdvancedCompositor, CompositorLayer, GradientDirection, LayerContent, VisualEffect, WidgetType,
};
// USERLAND: use crate::drivers::framebuffer::{Color, FramebufferDriver};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::{format, vec};

/// Sistema de applets/widgets desmontables
pub struct AppletSystem {
    pub applets: Vec<Applet>,
    pub next_applet_id: u32,
    pub config: AppletSystemConfig,
    pub compositor: Option<AdvancedCompositor>,
}

/// Configuración del sistema de applets
#[derive(Debug, Clone)]
pub struct AppletSystemConfig {
    pub enable_drag_drop: bool,
    pub enable_resize: bool,
    pub enable_auto_arrange: bool,
    pub default_applet_size: (f32, f32),
    pub max_applets: usize,
    pub enable_animations: bool,
}

impl Default for AppletSystemConfig {
    fn default() -> Self {
        Self {
            enable_drag_drop: true,
            enable_resize: true,
            enable_auto_arrange: true,
            default_applet_size: (200.0, 150.0),
            max_applets: 50,
            enable_animations: true,
        }
    }
}

/// Applet individual
#[derive(Debug, Clone)]
pub struct Applet {
    pub id: u32,
    pub name: String,
    pub applet_type: AppletType,
    pub position: (f32, f32),
    pub size: (f32, f32),
    pub visible: bool,
    pub resizable: bool,
    pub draggable: bool,
    pub config: AppletConfig,
    pub data: AppletData,
    pub layer_id: Option<u32>,
}

/// Tipo de applet
#[derive(Debug, Clone, PartialEq)]
pub enum AppletType {
    Clock,
    SystemMonitor,
    Weather,
    Calendar,
    MusicPlayer,
    NetworkMonitor,
    DiskUsage,
    ProcessList,
    Custom { name: String },
}

/// Configuración del applet
#[derive(Debug, Clone)]
pub struct AppletConfig {
    pub refresh_interval: f32, // en segundos
    pub show_border: bool,
    pub border_color: Color,
    pub background_color: Color,
    pub text_color: Color,
    pub font_size: f32,
    pub transparency: f32,
    pub enable_effects: bool,
}

impl Default for AppletConfig {
    fn default() -> Self {
        Self {
            refresh_interval: 1.0,
            show_border: true,
            border_color: Color::WHITE,
            background_color: Color::BLACK,
            text_color: Color::WHITE,
            font_size: 14.0,
            transparency: 0.8,
            enable_effects: true,
        }
    }
}

/// Datos del applet
#[derive(Debug, Clone)]
pub enum AppletData {
    Clock {
        format: String,
        timezone: String,
    },
    SystemMonitor {
        show_cpu: bool,
        show_memory: bool,
        show_disk: bool,
        show_network: bool,
    },
    Weather {
        location: String,
        units: String,
        show_forecast: bool,
    },
    Calendar {
        show_events: bool,
        show_holidays: bool,
    },
    MusicPlayer {
        player_name: String,
        show_controls: bool,
    },
    NetworkMonitor {
        interface: String,
        show_speed: bool,
    },
    DiskUsage {
        show_percentages: bool,
        show_free_space: bool,
    },
    ProcessList {
        max_processes: usize,
        sort_by: ProcessSortBy,
    },
    Custom {
        data: Vec<u8>,
        format: String,
    },
}

/// Criterio de ordenación de procesos
#[derive(Debug, Clone, PartialEq)]
pub enum ProcessSortBy {
    CpuUsage,
    MemoryUsage,
    Name,
    Pid,
}

impl AppletSystem {
    /// Crear nuevo sistema de applets
    pub fn new() -> Self {
        Self {
            applets: Vec::new(),
            next_applet_id: 1,
            config: AppletSystemConfig::default(),
            compositor: None,
        }
    }

    /// Crear con configuración personalizada
    pub fn with_config(config: AppletSystemConfig) -> Self {
        Self {
            applets: Vec::new(),
            next_applet_id: 1,
            config,
            compositor: None,
        }
    }

    /// Inicializar el sistema de applets
    pub fn initialize(&mut self, compositor: AdvancedCompositor) -> Result<(), String> {
        self.compositor = Some(compositor);

        // Crear applets por defecto
        self.create_default_applets()?;

        Ok(())
    }

    /// Crear applets por defecto
    fn create_default_applets(&mut self) -> Result<(), String> {
        // Applet de reloj
        self.create_applet(AppletType::Clock, "Reloj", (50.0, 50.0))?;

        // Applet de monitor del sistema
        self.create_applet(
            AppletType::SystemMonitor,
            "Monitor del Sistema",
            (300.0, 50.0),
        )?;

        // Applet de calendario
        self.create_applet(AppletType::Calendar, "Calendario", (550.0, 50.0))?;

        Ok(())
    }

    /// Crear nuevo applet
    pub fn create_applet(
        &mut self,
        applet_type: AppletType,
        name: &str,
        position: (f32, f32),
    ) -> Result<u32, String> {
        if self.applets.len() >= self.config.max_applets {
            return Err("Máximo número de applets alcanzado".to_string());
        }

        let applet = Applet {
            id: self.next_applet_id,
            name: name.to_string(),
            applet_type: applet_type.clone(),
            position,
            size: self.config.default_applet_size,
            visible: true,
            resizable: self.config.enable_resize,
            draggable: self.config.enable_drag_drop,
            config: AppletConfig::default(),
            data: self.create_applet_data(&applet_type),
            layer_id: None,
        };

        // Crear capa en el compositor
        if let Some(ref mut compositor) = self.compositor {
            let layer = Self::create_applet_layer_static(&applet)?;
            let layer_id = compositor.add_layer(layer)?;

            // Actualizar el applet con el ID de la capa
            let mut applet = applet;
            applet.layer_id = Some(layer_id);
            self.applets.push(applet);
        } else {
            self.applets.push(applet);
        }

        self.next_applet_id += 1;
        Ok(self.next_applet_id - 1)
    }

    /// Crear datos del applet según su tipo
    fn create_applet_data(&self, applet_type: &AppletType) -> AppletData {
        match applet_type {
            AppletType::Clock => AppletData::Clock {
                format: "%H:%M:%S".to_string(),
                timezone: "UTC".to_string(),
            },
            AppletType::SystemMonitor => AppletData::SystemMonitor {
                show_cpu: true,
                show_memory: true,
                show_disk: true,
                show_network: true,
            },
            AppletType::Weather => AppletData::Weather {
                location: "Madrid".to_string(),
                units: "metric".to_string(),
                show_forecast: true,
            },
            AppletType::Calendar => AppletData::Calendar {
                show_events: true,
                show_holidays: true,
            },
            AppletType::MusicPlayer => AppletData::MusicPlayer {
                player_name: "COSMIC Player".to_string(),
                show_controls: true,
            },
            AppletType::NetworkMonitor => AppletData::NetworkMonitor {
                interface: "eth0".to_string(),
                show_speed: true,
            },
            AppletType::DiskUsage => AppletData::DiskUsage {
                show_percentages: true,
                show_free_space: true,
            },
            AppletType::ProcessList => AppletData::ProcessList {
                max_processes: 10,
                sort_by: ProcessSortBy::CpuUsage,
            },
            AppletType::Custom { name } => AppletData::Custom {
                data: Vec::new(),
                format: "text".to_string(),
            },
        }
    }

    /// Crear capa del applet
    fn create_applet_layer_static(applet: &Applet) -> Result<CompositorLayer, String> {
        let effects = if applet.config.enable_effects {
            vec![
                VisualEffect::Border {
                    width: 2.0,
                    color: applet.config.border_color,
                    style: crate::cosmic::advanced_compositor::BorderStyle::Solid,
                },
                VisualEffect::Transparency {
                    alpha: applet.config.transparency,
                },
            ]
        } else {
            vec![]
        };

        let content = match &applet.applet_type {
            AppletType::Clock => LayerContent::Widget {
                widget_type: WidgetType::Clock,
            },
            AppletType::SystemMonitor => LayerContent::Widget {
                widget_type: WidgetType::SystemMonitor,
            },
            _ => LayerContent::SolidColor {
                color: applet.config.background_color,
            },
        };

        Ok(CompositorLayer {
            id: 0, // Se asignará en el compositor
            x: applet.position.0,
            y: applet.position.1,
            width: applet.size.0,
            height: applet.size.1,
            z_index: 100, // Los applets están por encima del escritorio
            visible: applet.visible,
            opacity: applet.config.transparency,
            effects,
            content,
            animation_state: crate::cosmic::advanced_compositor::AnimationState {
                is_animating: false,
                start_time: 0.0,
                duration: 0.0,
                start_values: crate::cosmic::advanced_compositor::AnimationValues {
                    x: applet.position.0,
                    y: applet.position.1,
                    width: applet.size.0,
                    height: applet.size.1,
                    opacity: applet.config.transparency,
                    rotation: 0.0,
                    scale_x: 1.0,
                    scale_y: 1.0,
                },
                end_values: crate::cosmic::advanced_compositor::AnimationValues {
                    x: applet.position.0,
                    y: applet.position.1,
                    width: applet.size.0,
                    height: applet.size.1,
                    opacity: applet.config.transparency,
                    rotation: 0.0,
                    scale_x: 1.0,
                    scale_y: 1.0,
                },
                easing: crate::cosmic::advanced_compositor::EasingType::Linear,
            },
        })
    }

    /// Actualizar applet
    pub fn update_applet(&mut self, applet_id: u32, updates: AppletUpdate) -> Result<(), String> {
        if let Some(applet) = self.applets.iter_mut().find(|a| a.id == applet_id) {
            match updates {
                AppletUpdate::Position { x, y } => {
                    applet.position = (x, y);
                    if let Some(layer_id) = applet.layer_id {
                        if let Some(ref mut compositor) = self.compositor {
                            compositor.update_layer(
                                layer_id,
                                crate::cosmic::advanced_compositor::LayerUpdate::Position { x, y },
                            )?;
                        }
                    }
                }
                AppletUpdate::Size { width, height } => {
                    if applet.resizable {
                        applet.size = (width, height);
                        if let Some(layer_id) = applet.layer_id {
                            if let Some(ref mut compositor) = self.compositor {
                                compositor.update_layer(
                                    layer_id,
                                    crate::cosmic::advanced_compositor::LayerUpdate::Size {
                                        width,
                                        height,
                                    },
                                )?;
                            }
                        }
                    }
                }
                AppletUpdate::Visibility { visible } => {
                    applet.visible = visible;
                    if let Some(layer_id) = applet.layer_id {
                        if let Some(ref mut compositor) = self.compositor {
                            compositor.update_layer(
                                layer_id,
                                crate::cosmic::advanced_compositor::LayerUpdate::Visibility {
                                    visible,
                                },
                            )?;
                        }
                    }
                }
                AppletUpdate::Config { config } => {
                    applet.config = config;
                }
                AppletUpdate::Data { data } => {
                    applet.data = data;
                }
            }
            Ok(())
        } else {
            Err("Applet no encontrado".to_string())
        }
    }

    /// Mover applet
    pub fn move_applet(&mut self, applet_id: u32, new_position: (f32, f32)) -> Result<(), String> {
        self.update_applet(
            applet_id,
            AppletUpdate::Position {
                x: new_position.0,
                y: new_position.1,
            },
        )
    }

    /// Redimensionar applet
    pub fn resize_applet(&mut self, applet_id: u32, new_size: (f32, f32)) -> Result<(), String> {
        self.update_applet(
            applet_id,
            AppletUpdate::Size {
                width: new_size.0,
                height: new_size.1,
            },
        )
    }

    /// Ocultar/mostrar applet
    pub fn toggle_applet_visibility(&mut self, applet_id: u32) -> Result<(), String> {
        if let Some(applet) = self.applets.iter().find(|a| a.id == applet_id) {
            self.update_applet(
                applet_id,
                AppletUpdate::Visibility {
                    visible: !applet.visible,
                },
            )
        } else {
            Err("Applet no encontrado".to_string())
        }
    }

    /// Eliminar applet
    pub fn remove_applet(&mut self, applet_id: u32) -> Result<(), String> {
        if let Some(pos) = self.applets.iter().position(|a| a.id == applet_id) {
            let applet = self.applets.remove(pos);

            // Eliminar capa del compositor
            if let Some(layer_id) = applet.layer_id {
                if let Some(ref mut compositor) = self.compositor {
                    compositor.remove_layer(layer_id)?;
                }
            }

            Ok(())
        } else {
            Err("Applet no encontrado".to_string())
        }
    }

    /// Obtener applet por ID
    pub fn get_applet(&self, applet_id: u32) -> Option<&Applet> {
        self.applets.iter().find(|a| a.id == applet_id)
    }

    /// Obtener todos los applets
    pub fn get_all_applets(&self) -> &[Applet] {
        &self.applets
    }

    /// Obtener applets visibles
    pub fn get_visible_applets(&self) -> Vec<&Applet> {
        self.applets.iter().filter(|a| a.visible).collect()
    }

    /// Organizar applets automáticamente
    pub fn auto_arrange_applets(&mut self) -> Result<(), String> {
        if !self.config.enable_auto_arrange {
            return Ok(());
        }

        let visible_applet_ids: Vec<u32> = self
            .applets
            .iter()
            .filter(|a| a.visible)
            .map(|a| a.id)
            .collect();

        let applet_width = self.config.default_applet_size.0;
        let applet_height = self.config.default_applet_size.1;
        let margin = 20.0;
        let start_x = 50.0;
        let start_y = 50.0;
        let max_per_row = 4;

        for (index, applet_id) in visible_applet_ids.iter().enumerate() {
            let row = index / max_per_row;
            let col = index % max_per_row;

            let x = start_x + (col as f32 * (applet_width + margin));
            let y = start_y + (row as f32 * (applet_height + margin));

            self.move_applet(*applet_id, (x, y))?;
        }

        Ok(())
    }

    /// Renderizar todos los applets
    pub fn render_applets(&mut self, fb: &mut FramebufferDriver) -> Result<(), String> {
        for applet in &self.applets {
            if applet.visible {
                self.render_applet(fb, applet)?;
            }
        }
        Ok(())
    }

    /// Renderizar applet individual
    fn render_applet(&self, fb: &mut FramebufferDriver, applet: &Applet) -> Result<(), String> {
        let x = applet.position.0 as u32;
        let y = applet.position.1 as u32;
        let width = applet.size.0 as u32;
        let height = applet.size.1 as u32;

        // Dibujar fondo del applet
        for current_y in y..(y + height) {
            for current_x in x..(x + width) {
                fb.put_pixel(current_x, current_y, applet.config.background_color);
            }
        }

        // Dibujar borde si está habilitado
        if applet.config.show_border {
            self.draw_applet_border(fb, x, y, width, height, applet.config.border_color)?;
        }

        // Renderizar contenido específico del applet
        self.render_applet_content(fb, applet)?;

        Ok(())
    }

    /// Dibujar borde del applet
    fn draw_applet_border(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        color: Color,
    ) -> Result<(), String> {
        // Borde superior
        for current_x in x..(x + width) {
            fb.put_pixel(current_x, y, color);
        }
        // Borde inferior
        for current_x in x..(x + width) {
            fb.put_pixel(current_x, y + height - 1, color);
        }
        // Borde izquierdo
        for current_y in y..(y + height) {
            fb.put_pixel(x, current_y, color);
        }
        // Borde derecho
        for current_y in y..(y + height) {
            fb.put_pixel(x + width - 1, current_y, color);
        }
        Ok(())
    }

    /// Renderizar contenido del applet
    fn render_applet_content(
        &self,
        fb: &mut FramebufferDriver,
        applet: &Applet,
    ) -> Result<(), String> {
        match &applet.applet_type {
            AppletType::Clock => {
                self.render_clock_applet(fb, applet)?;
            }
            AppletType::SystemMonitor => {
                self.render_system_monitor_applet(fb, applet)?;
            }
            AppletType::Calendar => {
                self.render_calendar_applet(fb, applet)?;
            }
            AppletType::Weather => {
                self.render_weather_applet(fb, applet)?;
            }
            _ => {
                // Applet genérico
                self.render_generic_applet(fb, applet)?;
            }
        }
        Ok(())
    }

    /// Renderizar applet de reloj
    fn render_clock_applet(
        &self,
        fb: &mut FramebufferDriver,
        applet: &Applet,
    ) -> Result<(), String> {
        let x = applet.position.0 as u32 + 10;
        let y = applet.position.1 as u32 + 20;

        // Simular tiempo actual
        let time_text = "12:34:56";
        fb.write_text_kernel(time_text, applet.config.text_color);

        Ok(())
    }

    /// Renderizar applet de monitor del sistema
    fn render_system_monitor_applet(
        &self,
        fb: &mut FramebufferDriver,
        applet: &Applet,
    ) -> Result<(), String> {
        let x = applet.position.0 as u32 + 10;
        let mut y = applet.position.1 as u32 + 20;

        // Información del sistema (simulada)
        let info_lines = vec![
            "CPU: 45%",
            "RAM: 2.1GB/8GB",
            "Disk: 120GB/500GB",
            "Network: 1.2MB/s",
        ];

        for line in info_lines {
            fb.write_text_kernel(line, applet.config.text_color);
            y += 20;
        }

        Ok(())
    }

    /// Renderizar applet de calendario
    fn render_calendar_applet(
        &self,
        fb: &mut FramebufferDriver,
        applet: &Applet,
    ) -> Result<(), String> {
        let x = applet.position.0 as u32 + 10;
        let y = applet.position.1 as u32 + 20;

        // Calendario simple (simulado)
        let calendar_text = "Lun 15\nMar 16\nMié 17";
        fb.write_text_kernel(calendar_text, applet.config.text_color);

        Ok(())
    }

    /// Renderizar applet del clima
    fn render_weather_applet(
        &self,
        fb: &mut FramebufferDriver,
        applet: &Applet,
    ) -> Result<(), String> {
        let x = applet.position.0 as u32 + 10;
        let y = applet.position.1 as u32 + 20;

        // Información del clima (simulada)
        let weather_text = "Madrid\n22°C\nSoleado";
        fb.write_text_kernel(weather_text, applet.config.text_color);

        Ok(())
    }

    /// Renderizar applet genérico
    fn render_generic_applet(
        &self,
        fb: &mut FramebufferDriver,
        applet: &Applet,
    ) -> Result<(), String> {
        let x = applet.position.0 as u32 + 10;
        let y = applet.position.1 as u32 + 20;

        let text = format!("Applet: {}", applet.name);
        fb.write_text_kernel(&text, applet.config.text_color);

        Ok(())
    }

    /// Obtener información del sistema de applets
    pub fn get_info(&self) -> String {
        format!(
            "Applet System: {} applets | Visibles: {} | Config: Drag={}, Resize={}, AutoArrange={}",
            self.applets.len(),
            self.get_visible_applets().len(),
            self.config.enable_drag_drop,
            self.config.enable_resize,
            self.config.enable_auto_arrange
        )
    }
}

/// Actualización de applet
#[derive(Debug, Clone)]
pub enum AppletUpdate {
    Position { x: f32, y: f32 },
    Size { width: f32, height: f32 },
    Visibility { visible: bool },
    Config { config: AppletConfig },
    Data { data: AppletData },
}
