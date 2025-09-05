//! Sistema de interfaz de usuario para Eclipse OS
//! 
//! Este módulo proporciona:
//! - Sistema de ventanas y gestión de ventanas
//! - Manejo de eventos de entrada (teclado, mouse)
//! - Sistema de renderizado gráfico
//! - Interfaz de línea de comandos
//! - Sistema de compositor

pub mod window;
pub mod event;
pub mod graphics;
pub mod terminal;
pub mod compositor;
pub mod widget;

// Re-exportar tipos principales
pub use graphics::Color;

// Constantes del sistema de UI
pub const MAX_WINDOWS: usize = 256;
pub const MAX_LAYERS: usize = 32;
pub const DEFAULT_WINDOW_WIDTH: u32 = 800;
pub const DEFAULT_WINDOW_HEIGHT: u32 = 600;
pub const TITLE_BAR_HEIGHT: u32 = 24;
pub const BORDER_WIDTH: u32 = 2;

// Colores del sistema
pub const COLOR_BACKGROUND: Color = Color { r: 45, g: 45, b: 45, a: 255 };
pub const COLOR_WINDOW_BG: Color = Color { r: 240, g: 240, b: 240, a: 255 };
pub const COLOR_TITLE_BAR: Color = Color { r: 70, g: 130, b: 180, a: 255 };
pub const COLOR_BORDER: Color = Color { r: 100, g: 100, b: 100, a: 255 };
pub const COLOR_TEXT: Color = Color { r: 0, g: 0, b: 0, a: 255 };
pub const COLOR_BUTTON: Color = Color { r: 200, g: 200, b: 200, a: 255 };
pub const COLOR_BUTTON_HOVER: Color = Color { r: 220, g: 220, b: 220, a: 255 };

/// Inicializar el sistema de interfaz de usuario
pub fn init_ui_system() -> Result<(), &'static str> {
    // Inicializar componentes del sistema UI
    window::init_window_manager()?;
    event::init_event_manager()?;
    graphics::init_graphics_system()?;
    terminal::init_terminal_system()?;
    compositor::init_compositor()?;
    widget::init_widget_manager()?;
    
    Ok(())
}

/// Obtener información del sistema de UI
pub fn get_ui_system_info() -> &'static str {
    "Sistema de Interfaz de Usuario Eclipse OS v1.0 - Window Manager + Graphics + Terminal"
}
