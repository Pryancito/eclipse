//! Barra de tareas para COSMIC Desktop Environment
//!
//! Implementa una barra de tareas moderna en la parte inferior de la pantalla
//! con botón de inicio, aplicaciones abiertas y área de notificaciones.

use crate::drivers::framebuffer::{FramebufferDriver, Color};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::format;

/// Información de una ventana abierta en la barra de tareas
#[derive(Debug, Clone)]
pub struct TaskbarWindow {
    pub id: u32,
    pub title: String,
    pub icon: String,
    pub is_minimized: bool,
    pub is_active: bool,
}

/// Área de notificaciones
#[derive(Debug)]
pub struct NotificationArea {
    pub system_time: String,
    pub battery_level: u8,
    pub network_status: String,
}

/// Barra de tareas de COSMIC
#[derive(Debug)]
pub struct Taskbar {
    pub height: u32,
    pub start_button_size: u32,
    pub window_buttons: Vec<TaskbarWindow>,
    pub notification_area: NotificationArea,
    pub is_start_button_pressed: bool,
}

impl Taskbar {
    pub fn new() -> Self {
        Self {
            height: 40,
            start_button_size: 36,
            window_buttons: Vec::new(),
            notification_area: NotificationArea {
                system_time: "12:34".to_string(),
                battery_level: 85,
                network_status: "WiFi".to_string(),
            },
            is_start_button_pressed: false,
        }
    }

    /// Agregar una ventana a la barra de tareas
    pub fn add_window(&mut self, id: u32, title: String, icon: String) {
        // Verificar si la ventana ya existe
        if !self.window_buttons.iter().any(|w| w.id == id) {
            self.window_buttons.push(TaskbarWindow {
                id,
                title,
                icon,
                is_minimized: false,
                is_active: false,
            });
        }
    }

    /// Remover una ventana de la barra de tareas
    pub fn remove_window(&mut self, id: u32) {
        self.window_buttons.retain(|w| w.id != id);
    }

    /// Marcar una ventana como activa
    pub fn set_active_window(&mut self, id: u32) {
        for window in &mut self.window_buttons {
            window.is_active = window.id == id;
        }
    }

    /// Toggle de minimizar/restaurar ventana
    pub fn toggle_window_minimize(&mut self, id: u32) {
        if let Some(window) = self.window_buttons.iter_mut().find(|w| w.id == id) {
            window.is_minimized = !window.is_minimized;
        }
    }

    /// Obtener el índice de una ventana por su ID
    pub fn get_window_index(&self, id: u32) -> Option<usize> {
        self.window_buttons.iter().position(|w| w.id == id)
    }

    /// Actualizar la hora del sistema
    pub fn update_time(&mut self, time: String) {
        self.notification_area.system_time = time;
    }

    /// Actualizar el nivel de batería
    pub fn update_battery(&mut self, level: u8) {
        self.notification_area.battery_level = level;
    }

    /// Actualizar el estado de red
    pub fn update_network(&mut self, status: String) {
        self.notification_area.network_status = status;
    }

    /// Verificar si se hizo clic en el botón de inicio
    pub fn is_start_button_clicked(&self, x: u32, y: u32, screen_height: u32) -> bool {
        let taskbar_y = screen_height - self.height;
        x < self.start_button_size + 4 && y >= taskbar_y && y < screen_height
    }

    /// Verificar si se hizo clic en una ventana de la barra de tareas
    pub fn get_clicked_window(&self, x: u32, y: u32, screen_height: u32) -> Option<u32> {
        let taskbar_y = screen_height - self.height;
        if y < taskbar_y || y >= screen_height {
            return None;
        }

        let start_x = self.start_button_size + 8;
        let button_width = 120;
        let button_height = self.height - 4;

        for (i, window) in self.window_buttons.iter().enumerate() {
            let button_x = start_x + (i as u32 * (button_width + 4));
            if x >= button_x && x < button_x + button_width {
                return Some(window.id);
            }
        }
        None
    }
}

/// Renderizar la barra de tareas
pub fn render_taskbar(fb: &mut FramebufferDriver, taskbar: &Taskbar) -> Result<(), String> {
    let info = fb.get_info();
    let screen_width = info.width;
    let screen_height = info.height;
    let taskbar_y = screen_height - taskbar.height;

    // Colores del tema
    let background_color = Color::from_hex(0x1a1a2e);
    let border_color = Color::from_hex(0x0066aa);
    let button_color = Color::from_hex(0x0f0f1a);
    let active_button_color = Color::from_hex(0x004488);
    let text_color = Color::from_hex(0xffffff);
    let pressed_color = Color::from_hex(0x002244);

    // Fondo de la barra de tareas
    fb.draw_rect(0, taskbar_y, screen_width, taskbar.height, background_color);
    
    // Borde superior
    fb.draw_rect(0, taskbar_y, screen_width, 2, border_color);

    // Botón de inicio
    let start_button_color = if taskbar.is_start_button_pressed {
        pressed_color
    } else {
        button_color
    };
    
    fb.draw_rect(2, taskbar_y + 2, taskbar.start_button_size, taskbar.height - 4, start_button_color);
    fb.draw_rect(2, taskbar_y + 2, taskbar.start_button_size, taskbar.height - 4, border_color);
    
    // Icono del botón de inicio
    fb.draw_text_simple(8, taskbar_y + 12, "🚀", text_color);
    fb.draw_text_simple(8, taskbar_y + 24, "Start", Color::from_hex(0xaaaaaa));

    // Botones de ventanas abiertas
    let start_x = taskbar.start_button_size + 8;
    let button_width = 120;
    let button_height = taskbar.height - 4;

    for (i, window) in taskbar.window_buttons.iter().enumerate() {
        let button_x = start_x + (i as u32 * (button_width + 4));
        let button_color = if window.is_active {
            active_button_color
        } else if window.is_minimized {
            Color::from_hex(0x0a0a1a)
        } else {
            button_color
        };

        // Fondo del botón
        fb.draw_rect(button_x, taskbar_y + 2, button_width, button_height, button_color);
        fb.draw_rect(button_x, taskbar_y + 2, button_width, button_height, border_color);

        // Icono de la ventana
        fb.draw_text_simple(button_x + 4, taskbar_y + 8, &window.icon, text_color);

        // Título de la ventana (truncado si es muy largo)
        let title = if window.title.len() > 12 {
            format!("{}...", &window.title[..9])
        } else {
            window.title.clone()
        };
        
        let title_color = if window.is_active {
            text_color
        } else {
            Color::from_hex(0xaaaaaa)
        };
        
        fb.draw_text_simple(button_x + 24, taskbar_y + 12, &title, title_color);
        
        // Indicador de estado
        if window.is_minimized {
            fb.draw_text_simple(button_x + button_width - 16, taskbar_y + 8, "−", Color::from_hex(0x888888));
        } else {
            fb.draw_text_simple(button_x + button_width - 16, taskbar_y + 8, "□", Color::from_hex(0x888888));
        }
    }

    // Área de notificaciones
    let notification_x = screen_width - 200;
    
    // Fondo del área de notificaciones
    fb.draw_rect(notification_x, taskbar_y + 2, 196, taskbar.height - 4, button_color);
    fb.draw_rect(notification_x, taskbar_y + 2, 196, taskbar.height - 4, border_color);

    // Estado de red
    let network_color = match taskbar.notification_area.network_status.as_str() {
        "WiFi" => Color::from_hex(0x00ff00),
        "Ethernet" => Color::from_hex(0x0088ff),
        "Disconnected" => Color::from_hex(0xff4444),
        _ => Color::from_hex(0xaaaaaa),
    };
    
    fb.draw_text_simple(notification_x + 4, taskbar_y + 8, "📶", network_color);
    fb.draw_text_simple(notification_x + 20, taskbar_y + 12, &taskbar.notification_area.network_status, Color::from_hex(0xaaaaaa));

    // Nivel de batería
    let battery_color = if taskbar.notification_area.battery_level > 50 {
        Color::from_hex(0x00ff00)
    } else if taskbar.notification_area.battery_level > 20 {
        Color::from_hex(0xffff00)
    } else {
        Color::from_hex(0xff4444)
    };
    
    let battery_icon = if taskbar.notification_area.battery_level > 75 {
        "🔋"
    } else if taskbar.notification_area.battery_level > 25 {
        "🔋"
    } else {
        "🔋"
    };
    
    fb.draw_text_simple(notification_x + 80, taskbar_y + 8, battery_icon, battery_color);
    fb.draw_text_simple(notification_x + 96, taskbar_y + 12, &format!("{}%", taskbar.notification_area.battery_level), Color::from_hex(0xaaaaaa));

    // Hora del sistema
    fb.draw_text_simple(notification_x + 140, taskbar_y + 12, &taskbar.notification_area.system_time, text_color);

    Ok(())
}

/// Manejar clics en la barra de tareas
pub fn handle_taskbar_click(taskbar: &mut Taskbar, x: u32, y: u32, screen_height: u32) -> Option<TaskbarAction> {
    // Verificar clic en botón de inicio
    if taskbar.is_start_button_clicked(x, y, screen_height) {
        taskbar.is_start_button_pressed = !taskbar.is_start_button_pressed;
        return Some(TaskbarAction::ToggleStartMenu);
    }

    // Verificar clic en ventana
    if let Some(window_id) = taskbar.get_clicked_window(x, y, screen_height) {
        taskbar.set_active_window(window_id);
        return Some(TaskbarAction::SwitchToWindow(window_id));
    }

    None
}

/// Acciones que puede realizar la barra de tareas
#[derive(Debug, Clone)]
pub enum TaskbarAction {
    ToggleStartMenu,
    SwitchToWindow(u32),
    MinimizeWindow(u32),
    CloseWindow(u32),
}
