// USERLAND: use crate::drivers::framebuffer::{Color, FramebufferDriver};
use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Sistema de logs visuales para COSMIC
pub struct VisualLogSystem {
    /// Logs almacenados
    logs: VecDeque<VisualLog>,
    /// Configuración del sistema
    config: VisualLogConfig,
    /// Estadísticas del sistema
    stats: VisualLogStats,
    /// Posición de scroll
    scroll_offset: usize,
    /// Máximo número de logs visibles
    max_visible_logs: usize,
}

/// Configuración del sistema de logs visuales
#[derive(Debug, Clone)]
pub struct VisualLogConfig {
    /// Habilitar logs visuales
    pub enabled: bool,
    /// Posición X del centro de notificaciones
    pub center_x: u32,
    /// Posición Y del centro de notificaciones
    pub center_y: u32,
    /// Ancho del área de logs
    pub width: u32,
    /// Alto del área de logs
    pub height: u32,
    /// Duración de cada log en segundos
    pub log_duration: f32,
    /// Máximo número de logs en memoria
    pub max_logs: usize,
    /// Mostrar timestamp
    pub show_timestamp: bool,
    /// Mostrar iconos
    pub show_icons: bool,
}

impl Default for VisualLogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            center_x: 960, // Centro de pantalla 1920x1080
            center_y: 200, // Parte superior
            width: 800,
            height: 600,
            log_duration: 5.0,
            max_logs: 100,
            show_timestamp: true,
            show_icons: true,
        }
    }
}

/// Estadísticas del sistema de logs visuales
#[derive(Debug, Clone)]
pub struct VisualLogStats {
    /// Total de logs procesados
    pub total_logs: usize,
    /// Logs activos actualmente
    pub active_logs: usize,
    /// Logs por tipo
    pub logs_by_type: [usize; 4], // Info, Warning, Error, Success
    /// FPS de renderizado
    pub render_fps: f32,
    /// Memoria utilizada
    pub memory_usage: usize,
}

/// Log visual individual
#[derive(Debug, Clone)]
pub struct VisualLog {
    /// ID único del log
    pub id: String,
    /// Tipo de log
    pub log_type: LogType,
    /// Mensaje del log
    pub message: String,
    /// Timestamp de creación
    pub timestamp: f32,
    /// Duración restante
    pub remaining_time: f32,
    /// Estado del log
    pub state: LogState,
    /// Posición Y actual
    pub current_y: f32,
    /// Opacidad actual
    pub opacity: f32,
    /// Velocidad de animación
    pub animation_speed: f32,
}

/// Tipo de log
#[derive(Debug, Clone, PartialEq)]
pub enum LogType {
    /// Información general
    Info,
    /// Advertencia
    Warning,
    /// Error
    Error,
    /// Éxito
    Success,
}

/// Estado del log
#[derive(Debug, Clone, PartialEq)]
pub enum LogState {
    /// Apareciendo
    Appearing,
    /// Visible
    Visible,
    /// Desapareciendo
    Disappearing,
    /// Eliminado
    Removed,
}

impl VisualLogSystem {
    /// Crear nuevo sistema de logs visuales
    pub fn new() -> Self {
        Self {
            logs: VecDeque::new(),
            config: VisualLogConfig::default(),
            stats: VisualLogStats {
                total_logs: 0,
                active_logs: 0,
                logs_by_type: [0, 0, 0, 0],
                render_fps: 0.0,
                memory_usage: 0,
            },
            scroll_offset: 0,
            max_visible_logs: 10,
        }
    }

    /// Crear sistema con configuración personalizada
    pub fn with_config(config: VisualLogConfig) -> Self {
        Self {
            logs: VecDeque::new(),
            config,
            stats: VisualLogStats {
                total_logs: 0,
                active_logs: 0,
                logs_by_type: [0, 0, 0, 0],
                render_fps: 0.0,
                memory_usage: 0,
            },
            scroll_offset: 0,
            max_visible_logs: 10,
        }
    }

    /// Agregar un nuevo log
    pub fn add_log(&mut self, log_type: LogType, message: &str) {
        if !self.config.enabled {
            return;
        }

        let log_id = alloc::format!("log_{}", self.stats.total_logs);
        let timestamp = self.get_current_time();

        let visual_log = VisualLog {
            id: log_id,
            log_type,
            message: String::from(message),
            timestamp,
            remaining_time: self.config.log_duration,
            state: LogState::Appearing,
            current_y: self.config.center_y as f32,
            opacity: 0.0,
            animation_speed: 1.0,
        };

        // Agregar al inicio de la cola
        self.logs.push_front(visual_log);

        // Limitar número de logs
        while self.logs.len() > self.config.max_logs {
            self.logs.pop_back();
        }

        // Actualizar estadísticas
        self.stats.total_logs += 1;
        self.update_stats();
    }

    /// Actualizar todos los logs
    pub fn update(&mut self, delta_time: f32) {
        if !self.config.enabled {
            return;
        }

        for log in &mut self.logs {
            match log.state {
                LogState::Appearing => {
                    log.opacity += delta_time * 2.0; // Aparecer en 0.5 segundos
                    if log.opacity >= 1.0 {
                        log.opacity = 1.0;
                        log.state = LogState::Visible;
                    }
                }
                LogState::Visible => {
                    log.remaining_time -= delta_time;
                    if log.remaining_time <= 0.0 {
                        log.state = LogState::Disappearing;
                    }
                }
                LogState::Disappearing => {
                    log.opacity -= delta_time * 2.0; // Desaparecer en 0.5 segundos
                    if log.opacity <= 0.0 {
                        log.opacity = 0.0;
                        log.state = LogState::Removed;
                    }
                }
                LogState::Removed => {
                    // Log marcado para eliminación
                }
            }
        }

        // Eliminar logs marcados para eliminación
        self.logs.retain(|log| log.state != LogState::Removed);

        self.update_stats();
    }

    /// Renderizar todos los logs
    pub fn render(&mut self, fb: &mut FramebufferDriver) -> Result<(), String> {
        if !self.config.enabled {
            return Ok(());
        }

        // Renderizar fondo del área de logs
        self.render_log_background(fb);

        // Renderizar logs visibles
        let visible_logs = self.get_visible_logs();
        for (index, log) in visible_logs.iter().enumerate() {
            self.render_single_log(fb, log, index)?;
        }

        Ok(())
    }

    /// Renderizar fondo del área de logs
    fn render_log_background(&self, fb: &mut FramebufferDriver) {
        let bg_color = Color {
            r: 20,
            g: 20,
            b: 40,
            a: 200, // Semi-transparente
        };

        let border_color = Color {
            r: 0,
            g: 170,
            b: 255,
            a: 255,
        };

        // Fondo principal
        fb.draw_rect(
            self.config.center_x - self.config.width / 2,
            self.config.center_y - self.config.height / 2,
            self.config.width,
            self.config.height,
            bg_color,
        );

        // Borde
        fb.draw_rect(
            self.config.center_x - self.config.width / 2,
            self.config.center_y - self.config.height / 2,
            self.config.width,
            2,
            border_color,
        );

        // Título
        let title_color = Color {
            r: 255,
            g: 255,
            b: 255,
            a: 255,
        };

        fb.write_text_kernel_typing(
            self.config.center_x - 100,
            self.config.center_y - self.config.height / 2 + 10,
            "COSMIC Logs",
            title_color,
        );
    }

    /// Renderizar un log individual
    fn render_single_log(
        &self,
        fb: &mut FramebufferDriver,
        log: &VisualLog,
        index: usize,
    ) -> Result<(), String> {
        let log_height = 40;
        let log_width = self.config.width - 40;
        let log_x = self.config.center_x - log_width / 2;
        let log_y =
            (self.config.center_y - self.config.height / 2 + 50 + index as u32 * log_height) as f32;

        // Colores según el tipo de log
        let (bg_color, border_color, text_color, icon) = match log.log_type {
            LogType::Info => (
                Color {
                    r: 30,
                    g: 60,
                    b: 120,
                    a: (log.opacity * 255.0) as u8,
                },
                Color {
                    r: 0,
                    g: 150,
                    b: 255,
                    a: (log.opacity * 255.0) as u8,
                },
                Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: (log.opacity * 255.0) as u8,
                },
                "ℹ",
            ),
            LogType::Warning => (
                Color {
                    r: 120,
                    g: 80,
                    b: 30,
                    a: (log.opacity * 255.0) as u8,
                },
                Color {
                    r: 255,
                    g: 200,
                    b: 0,
                    a: (log.opacity * 255.0) as u8,
                },
                Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: (log.opacity * 255.0) as u8,
                },
                "⚠",
            ),
            LogType::Error => (
                Color {
                    r: 120,
                    g: 30,
                    b: 30,
                    a: (log.opacity * 255.0) as u8,
                },
                Color {
                    r: 255,
                    g: 50,
                    b: 50,
                    a: (log.opacity * 255.0) as u8,
                },
                Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: (log.opacity * 255.0) as u8,
                },
                "✕",
            ),
            LogType::Success => (
                Color {
                    r: 30,
                    g: 120,
                    b: 30,
                    a: (log.opacity * 255.0) as u8,
                },
                Color {
                    r: 0,
                    g: 255,
                    b: 100,
                    a: (log.opacity * 255.0) as u8,
                },
                Color {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: (log.opacity * 255.0) as u8,
                },
                "✓",
            ),
        };

        // Fondo del log
        fb.draw_rect(log_x as u32, log_y as u32, log_width, log_height, bg_color);

        // Borde del log
        fb.draw_rect(log_x as u32, log_y as u32, log_width, 2, border_color);

        // Icono (si está habilitado)
        if self.config.show_icons {
            fb.write_text_kernel_typing(log_x as u32 + 10, log_y as u32 + 15, icon, text_color);
        }

        // Mensaje del log
        let message_x = if self.config.show_icons {
            log_x as u32 + 40
        } else {
            log_x as u32 + 10
        };
        fb.write_text_kernel_typing(message_x, log_y as u32 + 15, &log.message, text_color);

        // Timestamp (si está habilitado)
        if self.config.show_timestamp {
            let timestamp_text = alloc::format!("{:.1}s", log.timestamp);
            fb.write_text_kernel_typing(
                log_x as u32 + log_width - 100,
                log_y as u32 + 15,
                &timestamp_text,
                text_color,
            );
        }

        // Barra de progreso del tiempo restante
        let progress_width =
            (log_width as f32 * (log.remaining_time / self.config.log_duration)) as u32;
        if progress_width > 0 {
            let progress_color = Color {
                r: border_color.r,
                g: border_color.g,
                b: border_color.b,
                a: (log.opacity * 128.0) as u8,
            };
            fb.draw_rect(
                log_x as u32,
                log_y as u32 + log_height - 4,
                progress_width,
                4,
                progress_color,
            );
        }

        Ok(())
    }

    /// Obtener logs visibles
    fn get_visible_logs(&self) -> Vec<&VisualLog> {
        let mut visible_logs = Vec::new();
        let mut count = 0;

        for log in &self.logs {
            if count >= self.max_visible_logs {
                break;
            }
            if log.state != LogState::Removed {
                visible_logs.push(log);
                count += 1;
            }
        }

        visible_logs
    }

    /// Actualizar estadísticas
    fn update_stats(&mut self) {
        self.stats.active_logs = self
            .logs
            .iter()
            .filter(|log| log.state != LogState::Removed)
            .count();
        self.stats.memory_usage = self.logs.len() * core::mem::size_of::<VisualLog>();

        // Contar logs por tipo
        self.stats.logs_by_type = [0, 0, 0, 0];
        for log in &self.logs {
            match log.log_type {
                LogType::Info => self.stats.logs_by_type[0] += 1,
                LogType::Warning => self.stats.logs_by_type[1] += 1,
                LogType::Error => self.stats.logs_by_type[2] += 1,
                LogType::Success => self.stats.logs_by_type[3] += 1,
            }
        }
    }

    /// Obtener tiempo actual simulado
    fn get_current_time(&self) -> f32 {
        // Simular tiempo basado en el número de logs
        self.stats.total_logs as f32 * 0.1
    }

    /// Obtener estadísticas
    pub fn get_stats(&self) -> &VisualLogStats {
        &self.stats
    }

    /// Configurar el sistema
    pub fn configure(&mut self, config: VisualLogConfig) {
        self.config = config;
    }

    /// Obtener configuración
    pub fn get_config(&self) -> &VisualLogConfig {
        &self.config
    }

    /// Limpiar todos los logs
    pub fn clear_logs(&mut self) {
        self.logs.clear();
        self.update_stats();
    }

    /// Habilitar/deshabilitar logs visuales
    pub fn set_enabled(&mut self, enabled: bool) {
        self.config.enabled = enabled;
    }

    /// Agregar log de información
    pub fn info(&mut self, message: &str) {
        self.add_log(LogType::Info, message);
    }

    /// Agregar log de advertencia
    pub fn warning(&mut self, message: &str) {
        self.add_log(LogType::Warning, message);
    }

    /// Agregar log de error
    pub fn error(&mut self, message: &str) {
        self.add_log(LogType::Error, message);
    }

    /// Agregar log de éxito
    pub fn success(&mut self, message: &str) {
        self.add_log(LogType::Success, message);
    }
}
