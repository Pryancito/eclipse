//! Gestor de gráficos para Eclipse OS
//! 
//! Coordina drivers de gráficos, sistema de ventanas,
//! widgets y aceleración por hardware.

use super::window_system::{WindowCompositor, WindowId, Position, Size};
use super::widgets::{WidgetManager, WidgetId, WidgetType};
use super::multi_gpu_manager::{MultiGpuManager, UnifiedGpuInfo, SupportedGpuType};
use super::nvidia_advanced::NvidiaAdvancedDriver;
use crate::drivers::framebuffer::FramebufferDriver;
use crate::drivers::ipc::DriverManager;
use crate::ipc::{DriverType, DriverConfig, DriverCapability};
use crate::syslog;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use alloc::boxed::Box;
use alloc::format;

/// Configuración del sistema de gráficos
#[derive(Debug, Clone)]
pub struct GraphicsConfig {
    pub enable_hardware_acceleration: bool,
    pub enable_cuda: bool,
    pub enable_ray_tracing: bool,
    pub enable_vulkan: bool,
    pub enable_opengl: bool,
    pub max_windows: u32,
    pub max_widgets: u32,
    pub vsync_enabled: bool,
    pub antialiasing_enabled: bool,
}

impl Default for GraphicsConfig {
    fn default() -> Self {
        Self {
            enable_hardware_acceleration: true,
            enable_cuda: true,
            enable_ray_tracing: true,
            enable_vulkan: true,
            enable_opengl: true,
            max_windows: 100,
            max_widgets: 1000,
            vsync_enabled: true,
            antialiasing_enabled: true,
        }
    }
}

/// Gestor de gráficos
pub struct GraphicsManager {
    config: GraphicsConfig,
    driver_manager: DriverManager,
    multi_gpu_manager: MultiGpuManager,
    window_compositor: WindowCompositor,
    widget_manager: WidgetManager,
    nvidia_driver: Option<NvidiaAdvancedDriver>,
    framebuffer: Option<FramebufferDriver>,
    initialized: bool,
    performance_stats: GraphicsPerformanceStats,
}

/// Estadísticas de rendimiento
#[derive(Debug, Clone)]
pub struct GraphicsPerformanceStats {
    pub frames_rendered: u64,
    pub average_fps: f32,
    pub gpu_memory_used: u64,
    pub gpu_memory_total: u64,
    pub cpu_usage: f32,
    pub gpu_usage: f32,
    pub last_frame_time: u64,
}

impl Default for GraphicsPerformanceStats {
    fn default() -> Self {
        Self {
            frames_rendered: 0,
            average_fps: 0.0,
            gpu_memory_used: 0,
            gpu_memory_total: 0,
            cpu_usage: 0.0,
            gpu_usage: 0.0,
            last_frame_time: 0,
        }
    }
}

impl GraphicsManager {
    /// Crear nuevo gestor de gráficos
    pub fn new(config: GraphicsConfig) -> Self {
        Self {
            config,
            driver_manager: DriverManager::new(),
            multi_gpu_manager: MultiGpuManager::new(),
            window_compositor: WindowCompositor::new(),
            widget_manager: WidgetManager::new(),
            nvidia_driver: None,
            framebuffer: None,
            initialized: false,
            performance_stats: GraphicsPerformanceStats::default(),
        }
    }

    /// Inicializar sistema de gráficos
    pub fn initialize(&mut self, framebuffer: FramebufferDriver) -> Result<(), String> {
        self.framebuffer = Some(framebuffer);
        if let Some(ref mut fb) = self.framebuffer { fb.write_text_kernel("[GFX] initialize IN", crate::drivers::framebuffer::Color::LIGHT_GRAY); }

        // Inicializar sistema de múltiples GPUs
        if self.config.enable_hardware_acceleration {
            if let Some(ref mut fb) = self.framebuffer { fb.write_text_kernel("[GFX] init multi-GPU...", crate::drivers::framebuffer::Color::LIGHT_GRAY); }
            if let Err(e) = self.multi_gpu_manager.initialize_all_drivers() {
                // Si falla la inicialización de GPUs, continuar sin aceleración
                // Log de advertencia - continuar sin aceleración
                if let Some(ref mut fb) = self.framebuffer { fb.write_text_kernel("[GFX] multi-GPU fallo (continuo)", crate::drivers::framebuffer::Color::YELLOW); }
            }
        }

        // Inicializar driver NVIDIA si está disponible (para compatibilidad)
        if self.config.enable_hardware_acceleration {
            if let Some(ref mut fb) = self.framebuffer { fb.write_text_kernel("[GFX] init NVIDIA...", crate::drivers::framebuffer::Color::LIGHT_GRAY); }
            if let Err(e) = self.initialize_nvidia_driver() {
                // No es crítico si falla
                // Log de advertencia - continuar sin aceleración
                if let Some(ref mut fb) = self.framebuffer { fb.write_text_kernel("[GFX] NVIDIA fallo (continuo)", crate::drivers::framebuffer::Color::YELLOW); }
            }
        }

        // Crear ventana de escritorio (omitir en entornos sin aceleración, p.ej. QEMU)
        if self.config.enable_hardware_acceleration {
            if let Some(ref mut fb) = self.framebuffer { fb.write_text_kernel("[GFX] create desktop...", crate::drivers::framebuffer::Color::LIGHT_GRAY); }
            self.create_desktop_window();
            if let Some(ref mut fb) = self.framebuffer { fb.write_text_kernel("[GFX] desktop OK", crate::drivers::framebuffer::Color::LIGHT_GRAY); }
        } else {
            if let Some(ref mut fb) = self.framebuffer { fb.write_text_kernel("[GFX] desktop omitido (sin aceleración)", crate::drivers::framebuffer::Color::LIGHT_GRAY); }
        }

        self.initialized = true;
        if let Some(ref mut fb) = self.framebuffer { fb.write_text_kernel("[GFX] initialize OUT", crate::drivers::framebuffer::Color::LIGHT_GRAY); }
        Ok(())
    }

    /// Inicializar driver NVIDIA
    fn initialize_nvidia_driver(&mut self) -> Result<(), String> {
        let nvidia_driver = NvidiaAdvancedDriver::new();
        let driver_box = Box::new(nvidia_driver);
        
        match self.driver_manager.register_driver(driver_box) {
            Ok(driver_id) => {
                
                // Obtener información de la GPU
                if let Some(driver_info) = self.driver_manager.get_driver_info(driver_id) {
                }
                
                Ok(())
            }
            Err(e) => {
                Err(e)
            }
        }
    }

    /// Crear ventana de escritorio
    fn create_desktop_window(&mut self) {
        let desktop_id = self.window_compositor.create_window(
            "Eclipse OS Desktop".to_string(),
            Position { x: 0, y: 0 },
            Size { width: 800, height: 600 }
        );
        
    }

    /// Obtener información de todas las GPUs detectadas
    pub fn get_gpu_info(&self) -> &Vec<UnifiedGpuInfo> {
        self.multi_gpu_manager.get_unified_gpus()
    }

    /// Obtener GPU activa
    pub fn get_active_gpu(&self) -> Option<&UnifiedGpuInfo> {
        self.multi_gpu_manager.get_active_gpu()
    }

    /// Cambiar GPU activa
    pub fn set_active_gpu(&mut self, gpu_index: usize) -> Result<(), String> {
        self.multi_gpu_manager.set_active_gpu(gpu_index)
    }

    /// Obtener estadísticas de GPUs
    pub fn get_gpu_statistics(&self) -> (u64, u32, u32, u32) {
        let stats = self.multi_gpu_manager.get_total_statistics();
        (stats.total_memory, stats.total_compute_units, stats.total_ray_tracing_units, stats.total_ai_accelerators)
    }

    /// Verificar si hay aceleración por hardware disponible
    pub fn has_hardware_acceleration(&self) -> bool {
        !self.multi_gpu_manager.get_unified_gpus().is_empty()
    }

    /// Crear ventana
    pub fn create_window(&mut self, title: String, position: Position, size: Size) -> WindowId {
        self.window_compositor.create_window(title, position, size)
    }

    /// Destruir ventana
    pub fn destroy_window(&mut self, window_id: WindowId) -> Result<(), String> {
        self.window_compositor.destroy_window(window_id)
    }

    /// Crear widget
    pub fn create_widget(&mut self, widget_type: WidgetType, position: Position, size: Size) -> WidgetId {
        self.widget_manager.create_widget(widget_type, position, size)
    }

    /// Destruir widget
    pub fn destroy_widget(&mut self, widget_id: WidgetId) -> Result<(), String> {
        self.widget_manager.destroy_widget(widget_id)
    }

    /// Renderizar frame
    pub fn render_frame(&mut self) -> Result<(), String> {
        if !self.initialized {
            return Err(String::from("Sistema de gráficos no inicializado"));
        }

        let start_time = self.get_current_time();

        // Componer ventanas
        if let Some(ref mut fb) = self.framebuffer {
            self.window_compositor.compose(fb);
        }

        // Dibujar widgets
        self.draw_widgets();

        // Actualizar estadísticas
        self.update_performance_stats(start_time);

        Ok(())
    }

    /// Dibujar widgets
    fn draw_widgets(&mut self) {
        if let Some(ref mut fb) = self.framebuffer {
            // Obtener lista de widgets visibles
            let widget_stats = self.widget_manager.get_statistics();
            
            // Dibujar cada widget
            for widget_id in 1..widget_stats.next_widget_id {
                if let Err(e) = self.widget_manager.draw_widget(fb, widget_id) {
                    // Ignorar errores de widgets no encontrados
                    if !e.contains("no encontrado") {
                    }
                }
            }
        }
    }

    /// Actualizar estadísticas de rendimiento
    fn update_performance_stats(&mut self, start_time: u64) {
        let end_time = self.get_current_time();
        let frame_time = end_time - start_time;
        
        self.performance_stats.frames_rendered += 1;
        self.performance_stats.last_frame_time = frame_time;
        
        // Calcular FPS promedio (simulado)
        if frame_time > 0 {
            self.performance_stats.average_fps = 1000.0 / frame_time as f32;
        }
    }


    /// Obtener estadísticas de rendimiento
    pub fn get_performance_stats(&self) -> &GraphicsPerformanceStats {
        &self.performance_stats
    }

    /// Obtener estadísticas del sistema de ventanas
    pub fn get_window_stats(&self) -> String {
        let stats = self.window_compositor.get_statistics();
        format!("{}", stats)
    }

    /// Obtener estadísticas del sistema de widgets
    pub fn get_widget_stats(&self) -> String {
        let stats = self.widget_manager.get_statistics();
        format!("{}", stats)
    }

    /// Manejar evento de mouse
    pub fn handle_mouse_event(&mut self, x: i32, y: i32, button: u8) {
        // Buscar ventana en la posición
        if let Some(window_id) = self.window_compositor.get_window_at(x, y) {
            // Establecer foco en la ventana
            if let Err(e) = self.window_compositor.set_focus(window_id) {
            }
        }

        // Manejar clic en widgets
        if let Some(widget_id) = self.widget_manager.handle_click(x, y) {
        }

        // Manejar hover en widgets
        self.widget_manager.handle_hover(x, y);
    }

    /// Crear ventana de demostración
    pub fn create_demo_window(&mut self) -> WindowId {
        let window_id = self.create_window(
            "Demo Window".to_string(),
            Position { x: 100, y: 100 },
            Size { width: 400, height: 300 }
        );

        // Crear algunos widgets de demostración
        let button_id = self.create_widget(
            WidgetType::Button,
            Position { x: 20, y: 30 },
            Size { width: 100, height: 30 }
        );

        if let Some(button) = self.widget_manager.get_widget_mut(button_id) {
            button.set_text("Click Me!".to_string());
        }

        let label_id = self.create_widget(
            WidgetType::Label,
            Position { x: 20, y: 70 },
            Size { width: 200, height: 20 }
        );

        if let Some(label) = self.widget_manager.get_widget_mut(label_id) {
            label.set_text("Hello, Eclipse OS!".to_string());
        }

        let checkbox_id = self.create_widget(
            WidgetType::Checkbox,
            Position { x: 20, y: 100 },
            Size { width: 150, height: 20 }
        );

        if let Some(checkbox) = self.widget_manager.get_widget_mut(checkbox_id) {
            checkbox.set_text("Enable Feature".to_string());
        }

        let slider_id = self.create_widget(
            WidgetType::Slider,
            Position { x: 20, y: 130 },
            Size { width: 200, height: 20 }
        );

        if let Some(slider) = self.widget_manager.get_widget_mut(slider_id) {
            slider.set_text("Volume".to_string());
            slider.set_value(50);
        }

        let progress_id = self.create_widget(
            WidgetType::ProgressBar,
            Position { x: 20, y: 160 },
            Size { width: 200, height: 20 }
        );

        if let Some(progress) = self.widget_manager.get_widget_mut(progress_id) {
            progress.set_text("Loading...".to_string());
            progress.set_value(75);
        }

        window_id
    }

    /// Obtener tiempo actual (simulado)
    fn get_current_time(&self) -> u64 {
        // En un sistema real, esto vendría del timer del sistema
        0
    }

    /// Configurar el gestor
    pub fn configure(&mut self, config: GraphicsConfig) {
        self.config = config;
    }

    /// Verificar si está inicializado
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
}
