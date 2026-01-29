//! Barra de tareas moderna inspirada en COSMIC Epoch

// USERLAND: use crate::drivers::framebuffer::{Color, FramebufferDriver};
use crate::math_utils::{sin, sqrt};
use alloc::format;
use alloc::string::ToString;
use heapless::{String, Vec};

/// Configuración de la barra de tareas
#[derive(Debug, Clone)]
pub struct TaskbarConfig {
    pub height: u32,
    pub background_color: [u8; 4],
    pub border_radius: f32,
    pub blur_enabled: bool,
    pub transparency: f32,
    pub show_clock: bool,
    pub show_system_tray: bool,
    pub show_workspaces: bool,
    pub animation_speed: f32,
}

impl Default for TaskbarConfig {
    fn default() -> Self {
        Self {
            height: 48,
            background_color: [30, 30, 30, 200],
            border_radius: 12.0,
            blur_enabled: true,
            transparency: 0.8,
            show_clock: true,
            show_system_tray: true,
            show_workspaces: true,
            animation_speed: 0.1,
        }
    }
}

/// Elemento de la barra de tareas
#[derive(Debug, Clone)]
pub struct TaskbarItem {
    pub id: String<32>,
    pub title: String<64>,
    pub icon: String<16>,
    pub is_active: bool,
    pub is_minimized: bool,
    pub position: (u32, u32),
    pub size: (u32, u32),
    pub hover_effect: bool,
    pub click_animation: f32,
}

impl TaskbarItem {
    pub fn new(id: &str, title: &str, icon: &str) -> Self {
        Self {
            id: str_to_heapless_32(id),
            title: str_to_heapless_64(title),
            icon: str_to_heapless_16(icon),
            is_active: false,
            is_minimized: false,
            position: (0, 0),
            size: (40, 40),
            hover_effect: false,
            click_animation: 0.0,
        }
    }
}

/// Espacio de trabajo
#[derive(Debug, Clone)]
pub struct Workspace {
    pub id: u32,
    pub name: String<32>,
    pub is_active: bool,
    pub window_count: u32,
    pub position: (u32, u32),
}

impl Workspace {
    pub fn new(id: u32, name: &str) -> Self {
        Self {
            id,
            name: str_to_heapless_32(name),
            is_active: false,
            window_count: 0,
            position: (0, 0),
        }
    }
}

/// Barra de tareas principal
pub struct Taskbar {
    pub config: TaskbarConfig,
    pub items: Vec<TaskbarItem, 16>,
    pub workspaces: Vec<Workspace, 8>,
    pub clock_text: String<32>,
    pub system_tray_items: Vec<TaskbarItem, 8>,
    pub animation_time: f32,
    pub hover_item: Option<usize>,
    pub click_animation: f32,
    pub background_animation: f32,
    pub blur_effect: f32,
    pub start_button_pressed: bool,
}

impl Taskbar {
    pub fn new() -> Self {
        Self {
            config: TaskbarConfig::default(),
            items: Vec::new(),
            workspaces: Vec::new(),
            clock_text: String::new(),
            system_tray_items: Vec::new(),
            animation_time: 0.0,
            hover_item: None,
            click_animation: 0.0,
            background_animation: 0.0,
            blur_effect: 0.0,
            start_button_pressed: false,
        }
    }

    pub fn with_config(config: TaskbarConfig) -> Self {
        Self {
            config,
            items: Vec::new(),
            workspaces: Vec::new(),
            clock_text: String::new(),
            system_tray_items: Vec::new(),
            animation_time: 0.0,
            hover_item: None,
            click_animation: 0.0,
            background_animation: 0.0,
            blur_effect: 0.0,
            start_button_pressed: false,
        }
    }

    /// Actualiza la barra de tareas
    pub fn update(&mut self, delta_time: f32) {
        self.animation_time += delta_time;
        self.background_animation += delta_time * self.config.animation_speed;

        // Actualizar animaciones de hover
        if let Some(hover_idx) = self.hover_item {
            if hover_idx < self.items.len() {
                self.items[hover_idx].hover_effect = true;
            }
        }

        // Actualizar animaciones de click
        if self.click_animation > 0.0 {
            self.click_animation -= delta_time * 2.0;
            if self.click_animation < 0.0 {
                self.click_animation = 0.0;
            }
        }

        // Actualizar efectos de blur
        if self.config.blur_enabled {
            self.blur_effect = (sin(self.background_animation) * 0.5 + 0.5) * 0.3;
        }
    }

    /// Renderiza la barra de tareas
    pub fn render(
        &self,
        fb: &mut FramebufferDriver,
        screen_width: u32,
        screen_height: u32,
    ) -> Result<(), &'static str> {
        let taskbar_y = screen_height - self.config.height;

        // Renderizar fondo con efectos
        self.render_background(fb, screen_width, taskbar_y);

        // Renderizar elementos de la barra de tareas
        self.render_items(fb, screen_width, taskbar_y);

        // Renderizar espacios de trabajo
        if self.config.show_workspaces {
            self.render_workspaces(fb, screen_width, taskbar_y);
        }

        // Renderizar reloj
        if self.config.show_clock {
            self.render_clock(fb, screen_width, taskbar_y);
        }

        // Renderizar bandeja del sistema
        if self.config.show_system_tray {
            self.render_system_tray(fb, screen_width, taskbar_y);
        }

        Ok(())
    }

    /// Renderiza el fondo de la barra de tareas
    fn render_background(
        &self,
        fb: &mut FramebufferDriver,
        screen_width: u32,
        taskbar_y: u32,
    ) -> Result<(), &'static str> {
        let height = self.config.height;
        let screen_height = fb.get_info().height;

        // Renderizar fondo sólido moderno con gradiente
        for py in taskbar_y..taskbar_y + height {
            for px in 0..screen_width {
                // Verificar límites para evitar crash
                if px < screen_width && py < screen_height {
                    // Crear gradiente vertical
                    let gradient_factor = (py - taskbar_y) as f32 / height as f32;
                    let r = (20.0 + gradient_factor * 10.0) as u8;
                    let g = (25.0 + gradient_factor * 15.0) as u8;
                    let b = (35.0 + gradient_factor * 20.0) as u8;
                    let a = 200;

                    let color = Color::from_rgba(r, g, b, a);
                    let _ = fb.put_pixel(px, py, color);
                }
            }
        }

        // Renderizar borde superior sutil
        for px in 0..screen_width {
            let color = Color::from_rgba(255, 255, 255, 50);
            let _ = fb.put_pixel(px, taskbar_y, color);
        }

        // Renderizar línea de resplandor inferior
        for px in 0..screen_width {
            let color = Color::from_rgba(100, 150, 255, 30);
            let _ = fb.put_pixel(px, taskbar_y + height - 1, color);
        }

        Ok(())
    }

    /// Renderiza los elementos de la barra de tareas
    fn render_items(
        &self,
        fb: &mut FramebufferDriver,
        screen_width: u32,
        taskbar_y: u32,
    ) -> Result<(), &'static str> {
        let item_size = 40;
        let spacing = 8;
        let start_x = 16;
        let center_y = taskbar_y + (self.config.height - item_size) / 2;
        let screen_height = fb.get_info().height;

        for (i, item) in self.items.iter().enumerate() {
            let x = start_x + (item_size + spacing) as u32 * i as u32;
            let y = center_y;

            // Efecto de hover
            let scale = if item.hover_effect { 1.1 } else { 1.0 };
            let scaled_size = (item_size as f32 * scale) as u32;
            let offset = (scaled_size - item_size) / 2;

            // Color del elemento
            let item_color = if item.is_active {
                [70, 130, 180, 255] // Azul activo
            } else if item.is_minimized {
                [100, 100, 100, 200] // Gris minimizado
            } else {
                [200, 200, 200, 255] // Blanco normal
            };

            // Renderizar sombra del elemento
            for py in (y - offset + 2)..(y - offset + scaled_size + 2) {
                for px in (x - offset + 2)..(x - offset + scaled_size + 2) {
                    if px < screen_width && py < screen_height {
                        let shadow_color = Color::from_rgba(0, 0, 0, 50);
                        let _ = fb.put_pixel(px, py, shadow_color);
                    }
                }
            }

            // Renderizar elemento (simplificado)
            for py in (y - offset)..(y - offset + scaled_size) {
                for px in (x - offset)..(x - offset + scaled_size) {
                    if px < screen_width && py < screen_height {
                        let color = Color::from_rgba(
                            item_color[0],
                            item_color[1],
                            item_color[2],
                            item_color[3],
                        );
                        let _ = fb.put_pixel(px, py, color);
                    }
                }
            }

            // Renderizar icono (simulado con texto)
            let icon_text = format!("{}", item.icon.as_str());
            let icon_x = x + (item_size - 16) / 2;
            let icon_y = y + (item_size - 16) / 2;

            fb.write_text_kernel_typing(icon_x, icon_y, &icon_text, Color::from_hex(0xffffff));
        }

        Ok(())
    }

    /// Renderiza los espacios de trabajo
    fn render_workspaces(
        &self,
        fb: &mut FramebufferDriver,
        screen_width: u32,
        taskbar_y: u32,
    ) -> Result<(), &'static str> {
        let workspace_size = 32;
        let spacing = 4;
        let start_x =
            screen_width - (self.workspaces.len() as u32 * (workspace_size + spacing) + 16);
        let center_y = taskbar_y + (self.config.height - workspace_size) / 2;

        for (i, workspace) in self.workspaces.iter().enumerate() {
            let x = start_x + (workspace_size + spacing) as u32 * i as u32;
            let y = center_y;

            // Color del espacio de trabajo
            let workspace_color = if workspace.is_active {
                [70, 130, 180, 255] // Azul activo
            } else {
                [60, 60, 60, 200] // Gris inactivo
            };

            // Renderizar espacio de trabajo
            self.render_rounded_rectangle(
                fb,
                x,
                y,
                workspace_size,
                workspace_size,
                6.0,
                workspace_color,
            );

            // Renderizar número del espacio de trabajo
            let workspace_text = format!("{}", workspace.id);
            let text_x = x + (workspace_size - 8) / 2;
            let text_y = y + (workspace_size - 8) / 2;

            fb.write_text_kernel_typing(text_x, text_y, &workspace_text, Color::from_hex(0xffffff));
        }

        Ok(())
    }

    /// Renderiza el reloj
    fn render_clock(
        &self,
        fb: &mut FramebufferDriver,
        screen_width: u32,
        taskbar_y: u32,
    ) -> Result<(), &'static str> {
        let clock_width = 120;
        let clock_height = 32;
        let clock_x = screen_width - clock_width - 16;
        let clock_y = taskbar_y + (self.config.height - clock_height) / 2;
        let screen_height = fb.get_info().height;

        // Fondo del reloj
        let clock_bg = [40, 40, 40, 200];
        // Renderizar sombra del reloj
        for py in (clock_y + 2)..(clock_y + clock_height + 2) {
            for px in (clock_x + 2)..(clock_x + clock_width + 2) {
                if px < screen_width && py < screen_height {
                    let shadow_color = Color::from_rgba(0, 0, 0, 30);
                    let _ = fb.put_pixel(px, py, shadow_color);
                }
            }
        }

        // Renderizar fondo del reloj (simplificado)
        for py in clock_y..clock_y + clock_height {
            for px in clock_x..clock_x + clock_width {
                if px < screen_width && py < screen_height {
                    let color =
                        Color::from_rgba(clock_bg[0], clock_bg[1], clock_bg[2], clock_bg[3]);
                    let _ = fb.put_pixel(px, py, color);
                }
            }
        }

        // Renderizar borde del reloj
        for py in clock_y..clock_y + clock_height {
            if clock_x < screen_width && py < screen_height {
                let border_color = Color::from_rgba(255, 255, 255, 40);
                let _ = fb.put_pixel(clock_x, py, border_color);
            }
            if (clock_x + clock_width - 1) < screen_width && py < screen_height {
                let border_color = Color::from_rgba(255, 255, 255, 40);
                let _ = fb.put_pixel(clock_x + clock_width - 1, py, border_color);
            }
        }
        for px in clock_x..clock_x + clock_width {
            if px < screen_width && clock_y < screen_height {
                let border_color = Color::from_rgba(255, 255, 255, 40);
                let _ = fb.put_pixel(px, clock_y, border_color);
            }
            if px < screen_width && (clock_y + clock_height - 1) < screen_height {
                let border_color = Color::from_rgba(255, 255, 255, 40);
                let _ = fb.put_pixel(px, clock_y + clock_height - 1, border_color);
            }
        }

        // Texto del reloj
        let clock_text = if self.clock_text.is_empty() {
            "12:34:56".to_string()
        } else {
            self.clock_text.as_str().to_string()
        };

        fb.write_text_kernel_typing(
            clock_x + 8,
            clock_y + 8,
            &clock_text,
            Color::from_hex(0xffffff),
        );

        Ok(())
    }

    /// Renderiza la bandeja del sistema
    fn render_system_tray(
        &self,
        fb: &mut FramebufferDriver,
        screen_width: u32,
        taskbar_y: u32,
    ) -> Result<(), &'static str> {
        let tray_size = 32;
        let spacing = 4;
        let start_x = 16;
        let center_y = taskbar_y + (self.config.height - tray_size) / 2;

        for (i, item) in self.system_tray_items.iter().enumerate() {
            let x = start_x + (tray_size + spacing) as u32 * i as u32;
            let y = center_y;

            // Color del elemento de la bandeja
            let tray_color = [50, 50, 50, 200];

            // Renderizar elemento de la bandeja
            self.render_rounded_rectangle(fb, x, y, tray_size, tray_size, 6.0, tray_color);

            // Renderizar icono
            let icon_text = format!("{}", item.icon.as_str());
            let icon_x = x + (tray_size - 16) / 2;
            let icon_y = y + (tray_size - 16) / 2;

            fb.write_text_kernel_typing(icon_x, icon_y, &icon_text, Color::from_hex(0xffffff));
        }

        Ok(())
    }

    /// Renderiza un rectángulo redondeado
    fn render_rounded_rectangle(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        radius: f32,
        color: [u8; 4],
    ) -> Result<(), &'static str> {
        // Implementación simplificada de rectángulo redondeado
        for py in y..y + height {
            for px in x..x + width {
                let dx = if px < x + radius as u32 {
                    (x + radius as u32 - px) as f32
                } else if px >= x + width - radius as u32 {
                    (px - (x + width - radius as u32)) as f32
                } else {
                    0.0
                };

                let dy = if py < y + radius as u32 {
                    (y + radius as u32 - py) as f32
                } else if py >= y + height - radius as u32 {
                    (py - (y + height - radius as u32)) as f32
                } else {
                    0.0
                };

                let distance = sqrt(((dx * dx + dy * dy) as f32).into());

                if distance <= (radius as f32).into() {
                    let color_obj = Color::from_rgba(color[0], color[1], color[2], color[3]);
                    fb.put_pixel(px, py, color_obj);
                }
            }
        }
        Ok(())
    }

    /// Renderiza el borde de un rectángulo redondeado
    fn render_rounded_border(
        &self,
        fb: &mut FramebufferDriver,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        radius: f32,
        color: [u8; 4],
    ) -> Result<(), &'static str> {
        // Implementación simplificada de borde redondeado
        let border_width = 1;
        for py in y..y + height {
            for px in x..x + width {
                let dx = if px < x + radius as u32 {
                    (x + radius as u32 - px) as f32
                } else if px >= x + width - radius as u32 {
                    (px - (x + width - radius as u32)) as f32
                } else {
                    0.0
                };

                let dy = if py < y + radius as u32 {
                    (y + radius as u32 - py) as f32
                } else if py >= y + height - radius as u32 {
                    (py - (y + height - radius as u32)) as f32
                } else {
                    0.0
                };

                let distance = sqrt(((dx * dx + dy * dy) as f32).into());

                if distance <= (radius as f32).into()
                    && distance >= ((radius - border_width as f32) as f32).into()
                {
                    let color_obj = Color::from_rgba(color[0], color[1], color[2], color[3]);
                    fb.put_pixel(px, py, color_obj);
                }
            }
        }
        Ok(())
    }

    /// Agrega un elemento a la barra de tareas
    pub fn add_item(&mut self, item: TaskbarItem) -> Result<(), &'static str> {
        if self.items.len() < self.items.capacity() {
            self.items
                .push(item)
                .map_err(|_| "No se pudo agregar elemento");
            Ok(())
        } else {
            Err("Barra de tareas llena")
        }
    }

    /// Agrega un espacio de trabajo
    pub fn add_workspace(&mut self, workspace: Workspace) -> Result<(), &'static str> {
        if self.workspaces.len() < self.workspaces.capacity() {
            self.workspaces
                .push(workspace)
                .map_err(|_| "No se pudo agregar espacio de trabajo");
            Ok(())
        } else {
            Err("Máximo de espacios de trabajo alcanzado")
        }
    }

    /// Actualiza el reloj
    pub fn update_clock(&mut self, time_text: &str) {
        self.clock_text = str_to_heapless_32(time_text);
    }

    /// Maneja el hover sobre un elemento
    pub fn handle_hover(&mut self, x: u32, y: u32) {
        // Lógica simplificada de hover
        self.hover_item = None;
        for (i, item) in self.items.iter().enumerate() {
            if x >= item.position.0
                && x < item.position.0 + item.size.0
                && y >= item.position.1
                && y < item.position.1 + item.size.1
            {
                self.hover_item = Some(i);
                break;
            }
        }
    }

    /// Maneja el click en un elemento
    pub fn handle_click(&mut self, x: u32, y: u32) -> Option<usize> {
        for (i, item) in self.items.iter().enumerate() {
            if x >= item.position.0
                && x < item.position.0 + item.size.0
                && y >= item.position.1
                && y < item.position.1 + item.size.1
            {
                self.click_animation = 1.0;
                return Some(i);
            }
        }
        None
    }

    /// Obtiene estadísticas de la barra de tareas
    pub fn get_stats(&self) -> TaskbarStats {
        TaskbarStats {
            total_items: self.items.len(),
            active_items: self.items.iter().filter(|item| item.is_active).count(),
            minimized_items: self.items.iter().filter(|item| item.is_minimized).count(),
            total_workspaces: self.workspaces.len(),
            active_workspace: self
                .workspaces
                .iter()
                .position(|w| w.is_active)
                .unwrap_or(0),
            system_tray_items: self.system_tray_items.len(),
        }
    }

    // === MÉTODOS DE COMPATIBILIDAD CON API ANTIGUA ===

    /// Agregar ventana (compatibilidad con API antigua)
    pub fn add_window(&mut self, id: u32, title: String<64>, icon: String<64>) {
        let item = TaskbarItem::new(&id.to_string(), &title.as_str(), &icon.as_str());
        let _ = self.add_item(item);
    }

    /// Remover ventana (compatibilidad con API antigua)
    pub fn remove_window(&mut self, id: u32) {
        self.items
            .retain(|item| item.id.parse::<u32>().unwrap_or(0) != id);
    }

    /// Establecer ventana activa (compatibilidad con API antigua)
    pub fn set_active_window(&mut self, id: u32) {
        for item in &mut self.items {
            item.is_active = item.id.parse::<u32>().unwrap_or(0) == id;
        }
    }

    /// Toggle minimizar ventana (compatibilidad con API antigua)
    pub fn toggle_window_minimize(&mut self, id: u32) {
        for item in &mut self.items {
            if item.id.parse::<u32>().unwrap_or(0) == id {
                item.is_minimized = !item.is_minimized;
                break;
            }
        }
    }

    /// Actualizar tiempo (compatibilidad con API antigua)
    pub fn update_time(&mut self, time: String<32>) {
        self.update_clock(&time);
    }

    /// Actualizar batería (compatibilidad con API antigua)
    pub fn update_battery(&mut self, _level: u8) {
        // Implementación simplificada
    }

    /// Actualizar red (compatibilidad con API antigua)
    pub fn update_network(&mut self, _status: String<32>) {
        // Implementación simplificada
    }

    /// Obtener ventanas (compatibilidad con API antigua)
    pub fn get_windows(&self) -> &Vec<TaskbarItem, 16> {
        &self.items
    }

    /// Press start button
    pub fn press_start_button(&mut self) {
        self.start_button_pressed = true;
    }

    /// Release start button
    pub fn release_start_button(&mut self) {
        self.start_button_pressed = false;
    }

    /// Toggle start button
    pub fn toggle_start_button(&mut self) {
        self.start_button_pressed = !self.start_button_pressed;
    }

    /// Check if start button is pressed
    pub fn is_start_button_pressed(&self) -> bool {
        self.start_button_pressed
    }
}

/// Estadísticas de la barra de tareas
#[derive(Debug, Clone)]
pub struct TaskbarStats {
    pub total_items: usize,
    pub active_items: usize,
    pub minimized_items: usize,
    pub total_workspaces: usize,
    pub active_workspace: usize,
    pub system_tray_items: usize,
}

/// Acciones que puede realizar la barra de tareas
#[derive(Debug, Clone)]
pub enum TaskbarAction {
    ToggleStartMenu,
    SwitchToWindow(u32),
    MinimizeWindow(u32),
    CloseWindow(u32),
}

/// Ventana de la barra de tareas (compatibilidad con la API antigua)
pub type TaskbarWindow = TaskbarItem;

/// Manejar clic en la barra de tareas
pub fn handle_taskbar_click(
    taskbar: &mut Taskbar,
    x: u32,
    y: u32,
    screen_height: u32,
) -> Option<TaskbarAction> {
    // Verificar clic en elementos de la barra de tareas
    if let Some(item_index) = taskbar.handle_click(x, y) {
        if let Some(item) = taskbar.items.get(item_index) {
            return Some(TaskbarAction::SwitchToWindow(item.id.parse().unwrap_or(0)));
        }
    }

    // Verificar clic en espacios de trabajo
    let taskbar_y = screen_height - taskbar.config.height;
    let workspace_size = 32;
    let spacing = 4;
    let start_x =
        screen_height - (taskbar.workspaces.len() as u32 * (workspace_size + spacing) + 16);
    let center_y = taskbar_y + (taskbar.config.height - workspace_size) / 2;

    for (i, workspace) in taskbar.workspaces.iter().enumerate() {
        let workspace_x = start_x + (workspace_size + spacing) as u32 * i as u32;
        let workspace_y = center_y;

        if x >= workspace_x
            && x < workspace_x + workspace_size
            && y >= workspace_y
            && y < workspace_y + workspace_size
        {
            return Some(TaskbarAction::SwitchToWindow(workspace.id));
        }
    }

    None
}

// Funciones helper para conversión de strings
fn str_to_heapless_16(s: &str) -> String<16> {
    String::try_from(s).unwrap_or_else(|_| String::new())
}

fn str_to_heapless_32(s: &str) -> String<32> {
    String::try_from(s).unwrap_or_else(|_| String::new())
}

fn str_to_heapless_64(s: &str) -> String<64> {
    String::try_from(s).unwrap_or_else(|_| String::new())
}
