//! COSMIC Desktop Environment Mejorado - Sistema Unificado
//! 
//! COSMIC es el entorno de escritorio principal de Eclipse OS, integrando
//! las mejores características de Lunar: renderizado IA con UUID, aceleración
//! CUDA, efectos visuales avanzados, y optimización automática de rendimiento.

pub mod integration;
pub mod theme;
pub mod ai_features;
pub mod compositor;
pub mod window_manager;
pub mod demo;
pub mod start_menu;
pub mod taskbar;
pub mod window_operations;
pub mod wayland_integration;
pub mod wayland_demo;

// === MÓDULOS INTEGRADOS DESDE LUNAR ===
pub mod ai_renderer;
pub mod cuda_acceleration;
pub mod visual_effects;
pub mod ai_performance;
pub mod ai_autodiagnostic;
pub mod animations;
pub mod uuid_system;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::format;
use crate::desktop_ai::PerformanceStats;
use crate::drivers::framebuffer::FramebufferDriver;

// === IMPORTACIONES DE CARACTERÍSTICAS LUNAR INTEGRADAS ===
use ai_renderer::{AIRenderer, ObjectUUID, ObjectType, ObjectContent};
use cuda_acceleration::{CosmicCuda, CudaConfig, CudaStats};
use visual_effects::{CosmicVisualEffects, VisualEffectConfig, VisualEffectType, EffectIntensity};
use ai_performance::{AIPerformanceModel, PerformanceMetric, OptimizationAction};
use ai_autodiagnostic::{AIAutoDiagnostic, DiagnosticResult, AutoCorrectAction};
use animations::{AnimationManager, AnimationType, AnimationConfig};
use uuid_system::{SimpleUUID, UUIDGenerator, CounterUUIDGenerator};

/// Eventos de COSMIC Desktop Environment
#[derive(Debug, Clone)]
pub enum CosmicEvent {
    KeyPress { key_code: u32, modifiers: u32 },
    MouseMove { x: i32, y: i32 },
    MouseClick { x: i32, y: i32, button: u32 },
    WindowClose,
    WindowResize { width: u32, height: u32 },
    AppLaunch { command: String },
}

/// Configuración de COSMIC Mejorado para Eclipse OS
#[derive(Debug, Clone)]
pub struct CosmicConfig {
    // === CONFIGURACIÓN BÁSICA DE COSMIC ===
    pub enable_ai_features: bool,
    pub enable_space_theme: bool,
    pub enable_hardware_acceleration: bool,
    pub window_manager_mode: WindowManagerMode,
    pub ai_assistant_enabled: bool,
    pub performance_mode: PerformanceMode,
    
    // === CARACTERÍSTICAS INTEGRADAS DESDE LUNAR ===
    /// Habilitar renderizado IA con UUID
    pub enable_ai_rendering: bool,
    /// Habilitar aceleración CUDA
    pub enable_cuda_acceleration: bool,
    /// Habilitar efectos visuales avanzados
    pub enable_visual_effects: bool,
    /// Nivel de efectos visuales (0-100)
    pub visual_effects_level: u8,
    /// Habilitar efectos de partículas
    pub enable_particle_effects: bool,
    /// Habilitar sistema de animaciones
    pub enable_animations: bool,
    /// Habilitar autodiagnóstico IA
    pub enable_ai_autodiagnostic: bool,
    /// Habilitar optimización automática de rendimiento
    pub enable_ai_performance_optimization: bool,
    /// Resolución objetivo
    pub target_resolution: (u32, u32),
    /// Tema por defecto
    pub default_theme: String,
}

/// Modos de gestión de ventanas
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowManagerMode {
    Tiling,
    Floating,
    Hybrid,
}

/// Modos de rendimiento
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PerformanceMode {
    PowerSave,
    Balanced,
    Performance,
    Maximum,
}

impl Default for CosmicConfig {
    fn default() -> Self {
        Self {
            // === CONFIGURACIÓN BÁSICA DE COSMIC ===
            enable_ai_features: true,
            enable_space_theme: true,
            enable_hardware_acceleration: true,
            window_manager_mode: WindowManagerMode::Hybrid,
            ai_assistant_enabled: true,
            performance_mode: PerformanceMode::Balanced,
            
            // === CARACTERÍSTICAS INTEGRADAS DESDE LUNAR ===
            enable_ai_rendering: true,
            enable_cuda_acceleration: true,
            enable_visual_effects: true,
            visual_effects_level: 85,
            enable_particle_effects: true,
            enable_animations: true,
            enable_ai_autodiagnostic: true,
            enable_ai_performance_optimization: true,
            target_resolution: (1024, 768),
            default_theme: "cosmic_space".to_string(),
        }
    }
}

/// Estado de COSMIC en Eclipse OS
#[derive(Debug)]
pub struct CosmicState {
    pub initialized: bool,
    pub compositor_running: bool,
    pub window_manager_active: bool,
    pub ai_features_enabled: bool,
    pub theme_applied: bool,
    pub active_windows: Vec<u32>,
    pub performance_stats: CosmicPerformanceStats,
}

/// Estadísticas de rendimiento de COSMIC
#[derive(Debug, Clone)]
pub struct CosmicPerformanceStats {
    pub frame_rate: f32,
    pub memory_usage: u64,
    pub cpu_usage: f32,
    pub gpu_usage: f32,
    pub window_count: u32,
    pub compositor_latency: u64,
}

/// Estadísticas unificadas de rendimiento (integrado desde Lunar)
#[derive(Debug, Clone)]
pub struct UnifiedPerformanceStats {
    pub frame_rate: f32,
    pub memory_usage: u64,
    pub cpu_usage: f32,
    pub gpu_usage: f32,
    pub window_count: u32,
    pub compositor_latency: u64,
}

impl Default for CosmicPerformanceStats {
    fn default() -> Self {
        Self {
            frame_rate: 0.0,
            memory_usage: 0,
            cpu_usage: 0.0,
            gpu_usage: 0.0,
            window_count: 0,
            compositor_latency: 0,
        }
    }
}

impl Default for UnifiedPerformanceStats {
    fn default() -> Self {
        Self {
            frame_rate: 0.0,
            memory_usage: 0,
            cpu_usage: 0.0,
            gpu_usage: 0.0,
            window_count: 0,
            compositor_latency: 0,
        }
    }
}

/// Gestor Principal de COSMIC Mejorado - Sistema Unificado
pub struct CosmicManager {
    // === COMPONENTES BÁSICOS DE COSMIC ===
    config: CosmicConfig,
    state: CosmicState,
    integration: Option<integration::CosmicIntegration>,
    wayland_integration: Option<wayland_integration::CosmicWaylandIntegration>,
    theme: Option<theme::EclipseSpaceTheme>,
    ai_features: Option<ai_features::CosmicAIFeatures>,
    start_menu: start_menu::StartMenu,
    taskbar: taskbar::Taskbar,
    window_operations: window_operations::WindowOperationsManager,
    
    // === CARACTERÍSTICAS INTEGRADAS DESDE LUNAR ===
    /// Sistema de renderizado IA con UUID
    ai_renderer: AIRenderer,
    /// Aceleración CUDA integrada
    cuda_acceleration: CosmicCuda,
    /// Efectos visuales avanzados
    visual_effects: CosmicVisualEffects,
    /// Optimización de rendimiento IA
    ai_performance: AIPerformanceModel,
    /// Autodiagnóstico IA
    ai_autodiagnostic: AIAutoDiagnostic,
    /// Gestor de animaciones
    animation_manager: AnimationManager,
    /// Generador de UUID
    uuid_generator: CounterUUIDGenerator,
    /// Estadísticas unificadas
    unified_stats: UnifiedPerformanceStats,
    /// Contador de frames
    frame_count: u64,
    /// FPS actual
    current_fps: f32,
}

impl CosmicManager {
    /// Crear nuevo gestor de COSMIC
    pub fn new() -> Self {
        Self {
            config: CosmicConfig::default(),
            state: CosmicState {
                initialized: false,
                compositor_running: false,
                window_manager_active: false,
                ai_features_enabled: false,
                theme_applied: false,
                active_windows: Vec::new(),
                performance_stats: CosmicPerformanceStats::default(),
            },
            integration: None,
            wayland_integration: None,
            theme: None,
            ai_features: None,
            start_menu: start_menu::StartMenu::new(),
            taskbar: taskbar::Taskbar::new(),
            window_operations: window_operations::WindowOperationsManager::new(),
            
            // === CARACTERÍSTICAS INTEGRADAS DESDE LUNAR ===
            ai_renderer: AIRenderer::new(),
            cuda_acceleration: CosmicCuda::new(),
            visual_effects: CosmicVisualEffects::new(),
            ai_performance: AIPerformanceModel::new(),
            ai_autodiagnostic: AIAutoDiagnostic::new(),
            animation_manager: AnimationManager::new(),
            uuid_generator: CounterUUIDGenerator::new(),
            unified_stats: UnifiedPerformanceStats::default(),
            frame_count: 0,
            current_fps: 0.0,
        }
    }

    /// Crear gestor con configuración personalizada
    pub fn with_config(config: CosmicConfig) -> Self {
        Self {
            config,
            state: CosmicState {
                initialized: false,
                compositor_running: false,
                window_manager_active: false,
                ai_features_enabled: false,
                theme_applied: false,
                active_windows: Vec::new(),
                performance_stats: CosmicPerformanceStats::default(),
            },
            integration: None,
            wayland_integration: None,
            theme: None,
            ai_features: None,
            start_menu: start_menu::StartMenu::new(),
            taskbar: taskbar::Taskbar::new(),
            window_operations: window_operations::WindowOperationsManager::new(),
            
            // === CARACTERÍSTICAS INTEGRADAS DESDE LUNAR ===
            ai_renderer: AIRenderer::new(),
            cuda_acceleration: CosmicCuda::new(),
            visual_effects: CosmicVisualEffects::new(),
            ai_performance: AIPerformanceModel::new(),
            ai_autodiagnostic: AIAutoDiagnostic::new(),
            animation_manager: AnimationManager::new(),
            uuid_generator: CounterUUIDGenerator::new(),
            unified_stats: UnifiedPerformanceStats::default(),
            frame_count: 0,
            current_fps: 0.0,
        }
    }

    /// Inicializar COSMIC
    pub fn initialize(&mut self) -> Result<(), String> {
        if self.state.initialized {
            return Ok(());
        }

        // Inicializar integración base
        self.integration = Some(integration::CosmicIntegration::new()?);
        
        // Aplicar tema espacial si está habilitado
        if self.config.enable_space_theme {
            let mut theme = theme::EclipseSpaceTheme::new();
            theme.apply()?;
            self.theme = Some(theme);
            self.state.theme_applied = true;
        }

        // Inicializar características de IA si están habilitadas
        if self.config.enable_ai_features {
            self.ai_features = Some(ai_features::CosmicAIFeatures::new()?);
            self.state.ai_features_enabled = true;
        }

        self.state.initialized = true;
        Ok(())
    }

    /// Iniciar compositor COSMIC
    pub fn start_compositor(&mut self) -> Result<(), String> {
        if !self.state.initialized {
            return Err("COSMIC no inicializado".to_string());
        }

        if let Some(ref mut integration) = self.integration {
            integration.start_compositor()?;
            self.state.compositor_running = true;
        }

        Ok(())
    }

    /// Iniciar gestor de ventanas
    pub fn start_window_manager(&mut self) -> Result<(), String> {
        if !self.state.compositor_running {
            return Err("Compositor no ejecutándose".to_string());
        }

        if let Some(ref mut integration) = self.integration {
            integration.start_window_manager(self.config.window_manager_mode)?;
            self.state.window_manager_active = true;
        }

        Ok(())
    }

    /// Obtener estadísticas de rendimiento
    pub fn get_performance_stats(&mut self) -> &CosmicPerformanceStats {
        if let Some(ref mut integration) = self.integration {
            self.state.performance_stats = integration.get_performance_stats();
        }
        &self.state.performance_stats
    }

    /// Obtener estado de COSMIC
    pub fn get_state(&self) -> &CosmicState {
        &self.state
    }

    /// Procesar eventos de COSMIC
    pub fn process_events(&mut self) -> Result<(), String> {
        if !self.state.initialized {
            return Err("COSMIC no inicializado".to_string());
        }

        if let Some(ref mut integration) = self.integration {
            integration.process_events()?;
        }

        Ok(())
    }

    /// Renderizar frame de COSMIC
    pub fn render_frame(&mut self) -> Result<(), String> {
        if !self.state.initialized {
            return Err("COSMIC no inicializado".to_string());
        }

        if let Some(ref mut integration) = self.integration {
            integration.render_frame()?;
        }

        // Renderizar la barra de tareas directamente en el framebuffer principal
        self.render_taskbar_to_main_framebuffer()?;

        Ok(())
    }

    /// Renderizar la barra de tareas directamente en el framebuffer principal
    fn render_taskbar_to_main_framebuffer(&mut self) -> Result<(), String> {
        // Obtener el framebuffer principal del sistema
        if let Some(fb_ptr) = crate::drivers::framebuffer::get_framebuffer() {
            // Crear una referencia mutable segura al framebuffer
            let fb_unsafe = unsafe { core::ptr::read(fb_ptr) };
            let mut fb_copy = fb_unsafe;
            
            // Renderizar la barra de tareas
            self.render_taskbar(&mut fb_copy)?;
            
            // Actualizar el framebuffer principal con los cambios
            unsafe {
                core::ptr::write(fb_ptr, fb_copy);
            }
        }
        Ok(())
    }


    /// Destruir ventana
    pub fn destroy_window(&mut self, window_id: u32) -> Result<(), String> {
        if !self.state.window_manager_active {
            return Err("Gestor de ventanas no activo".to_string());
        }

        if let Some(ref mut integration) = self.integration {
            integration.destroy_window(window_id)?;
            self.state.active_windows.retain(|&id| id != window_id);
        }

        Ok(())
    }

    /// Obtener información del framebuffer
    pub fn get_framebuffer_info(&self) -> Option<String> {
        self.integration.as_ref()?.get_framebuffer_info()
    }

    /// Aplicar tema personalizado
    pub fn apply_custom_theme(&mut self, theme_name: &str) -> Result<(), String> {
        if !self.state.initialized {
            return Err("COSMIC no inicializado".to_string());
        }

        // En implementación real, cargar tema desde archivo
        match theme_name {
            "space" => {
                if let Some(ref mut theme) = self.theme {
                    theme.apply()?;
                }
                Ok(())
            }
            "dark" => {
                // Aplicar tema oscuro
                Ok(())
            }
            "light" => {
                // Aplicar tema claro
                Ok(())
            }
            _ => Err("Tema no encontrado".to_string())
        }
    }

    /// Obtener sugerencias de IA
    pub fn get_ai_suggestions(&mut self) -> Vec<String> {
        let mut suggestions = Vec::new();
        
        if let Some(ref mut ai_features) = self.ai_features {
            // Crear estadísticas básicas sin mutar self
            let stats = ai_features::PerformanceStats {
                render_time: 0,
                cache_hits: 0,
                cache_misses: 0,
                cache_hit_rate: 0.0,
                windows_count: 0,
                cpu_usage: 0.0,
                memory_usage: 0.0,
                gpu_usage: 0.0,
                compositor_latency: 0.0,
            };
            
            // Obtener sugerencias de optimización
            let perf_suggestions = ai_features.analyze_performance(&stats);
            
            for suggestion in perf_suggestions {
                suggestions.push(suggestion.description);
            }
        }
        
        suggestions
    }

    /// Aplicar optimización sugerida por IA
    pub fn apply_ai_optimization(&mut self, optimization: &str) -> Result<(), String> {
        if let Some(ref mut ai_features) = self.ai_features {
            // En implementación real, aplicar optimización específica
            match optimization {
                "reduce_effects" => {
                    // Reducir efectos visuales
                    Ok(())
                }
                "optimize_memory" => {
                    // Optimizar uso de memoria
                    Ok(())
                }
                "adjust_window_layout" => {
                    // Ajustar layout de ventanas
                    Ok(())
                }
                _ => Err("Optimización no reconocida".to_string())
            }
        } else {
            Err("Características de IA no disponibles".to_string())
        }
    }

    /// Obtener información del sistema COSMIC
    pub fn get_system_info(&self) -> String {
        let mut info = String::new();
        
        info.push_str("=== COSMIC Desktop Environment ===\n");
        info.push_str(&format!("Estado: {}\n", 
            if self.state.initialized { "Inicializado" } else { "No inicializado" }));
        info.push_str(&format!("Compositor: {}\n", 
            if self.state.compositor_running { "Activo" } else { "Inactivo" }));
        info.push_str(&format!("Gestor de ventanas: {}\n", 
            if self.state.window_manager_active { "Activo" } else { "Inactivo" }));
        info.push_str(&format!("Tema aplicado: {}\n", 
            if self.state.theme_applied { "Sí" } else { "No" }));
        info.push_str(&format!("IA habilitada: {}\n", 
            if self.state.ai_features_enabled { "Sí" } else { "No" }));
        info.push_str(&format!("Ventanas activas: {}\n", self.state.active_windows.len()));
        
        if let Some(ref integration) = self.integration {
            if let Some(fb_info) = integration.get_framebuffer_info() {
                info.push_str(&format!("{}\n", fb_info));
            }
        }
        
        info.push_str(&format!("Modo de ventanas: {:?}\n", self.config.window_manager_mode));
        info.push_str(&format!("Modo de rendimiento: {:?}\n", self.config.performance_mode));
        
        info
    }

    /// Detener COSMIC
    pub fn shutdown(&mut self) -> Result<(), String> {
        if !self.state.initialized {
            return Ok(());
        }

        // Detener integración
        if let Some(ref mut integration) = self.integration {
            integration.shutdown()?;
        }

        // Limpiar estado
        self.state.initialized = false;
        self.state.compositor_running = false;
        self.state.window_manager_active = false;
        self.state.active_windows.clear();

        Ok(())
    }

    /// Alternar menú de inicio
    pub fn toggle_start_menu(&mut self) {
        self.start_menu.toggle();
    }

    /// Verificar si el menú de inicio está abierto
    pub fn is_start_menu_open(&self) -> bool {
        self.start_menu.is_open()
    }

    /// Manejar entrada del menú de inicio
    pub fn handle_start_menu_input(&mut self, key_code: u32) -> Option<String> {
        start_menu::handle_start_menu_input(&mut self.start_menu, key_code)
    }

    /// Renderizar menú de inicio
    pub fn render_start_menu(&mut self, fb: &mut FramebufferDriver) -> Result<(), String> {
        start_menu::render_start_menu(fb, &self.start_menu)
    }

    /// Obtener framebuffer (método auxiliar para las aplicaciones)
    pub fn get_framebuffer(&mut self) -> Result<&mut FramebufferDriver, String> {
        // En una implementación real, esto obtendría el framebuffer del sistema
        // Por ahora, devolvemos un error ya que no tenemos acceso directo
        Err("Framebuffer no disponible en este contexto".to_string())
    }

    /// Obtener eventos de entrada (método auxiliar para las aplicaciones)
    pub fn get_input_events(&mut self) -> Result<Vec<CosmicEvent>, String> {
        // En una implementación real, esto obtendría los eventos del sistema de entrada
        // Por ahora, devolvemos una lista vacía
        Ok(Vec::new())
    }

    // === Métodos de la Barra de Tareas ===

    /// Renderizar la barra de tareas
    pub fn render_taskbar(&mut self, fb: &mut FramebufferDriver) -> Result<(), String> {
        taskbar::render_taskbar(fb, &self.taskbar)
    }

    /// Manejar clic en la barra de tareas
    pub fn handle_taskbar_click(&mut self, x: u32, y: u32, screen_height: u32) -> Option<taskbar::TaskbarAction> {
        taskbar::handle_taskbar_click(&mut self.taskbar, x, y, screen_height)
    }

    /// Agregar ventana a la barra de tareas
    pub fn add_window_to_taskbar(&mut self, id: u32, title: String, icon: String) {
        self.taskbar.add_window(id, title, icon);
    }

    /// Remover ventana de la barra de tareas
    pub fn remove_window_from_taskbar(&mut self, id: u32) {
        self.taskbar.remove_window(id);
    }

    /// Marcar ventana como activa en la barra de tareas
    pub fn set_active_window_in_taskbar(&mut self, id: u32) {
        self.taskbar.set_active_window(id);
    }

    /// Minimizar/restaurar ventana desde la barra de tareas
    pub fn toggle_window_minimize_in_taskbar(&mut self, id: u32) {
        self.taskbar.toggle_window_minimize(id);
    }

    /// Actualizar información del sistema en la barra de tareas
    pub fn update_taskbar_system_info(&mut self, time: String, battery: u8, network: String) {
        self.taskbar.update_time(time);
        self.taskbar.update_battery(battery);
        self.taskbar.update_network(network);
    }

    /// Verificar si el botón de inicio está presionado
    pub fn is_start_button_pressed(&self) -> bool {
        self.taskbar.is_start_button_pressed
    }

    /// Obtener altura de la barra de tareas
    pub fn get_taskbar_height(&self) -> u32 {
        self.taskbar.height
    }

    /// Obtener ventanas abiertas en la barra de tareas
    pub fn get_taskbar_windows(&self) -> &Vec<taskbar::TaskbarWindow> {
        &self.taskbar.window_buttons
    }

    // === Métodos de Operaciones de Ventana ===

    /// Crear una nueva ventana
    pub fn create_window(&mut self, title: String, icon: String, x: i32, y: i32, width: u32, height: u32) -> u32 {
        let window_id = self.window_operations.create_window(title.clone(), icon.clone(), x, y, width, height);
        
        // Agregar a la barra de tareas
        self.taskbar.add_window(window_id, title, icon);
        
        window_id
    }

    /// Minimizar ventana
    pub fn minimize_window(&mut self, window_id: u32) -> Result<(), String> {
        self.window_operations.execute_operation(window_id, window_operations::WindowOperation::Minimize)?;
        self.taskbar.toggle_window_minimize(window_id);
        Ok(())
    }

    /// Maximizar ventana
    pub fn maximize_window(&mut self, window_id: u32) -> Result<(), String> {
        self.window_operations.execute_operation(window_id, window_operations::WindowOperation::Maximize)?;
        Ok(())
    }

    /// Restaurar ventana
    pub fn restore_window(&mut self, window_id: u32) -> Result<(), String> {
        self.window_operations.execute_operation(window_id, window_operations::WindowOperation::Restore)?;
        self.taskbar.toggle_window_minimize(window_id);
        Ok(())
    }

    /// Cerrar ventana
    pub fn close_window(&mut self, window_id: u32) -> Result<(), String> {
        self.window_operations.execute_operation(window_id, window_operations::WindowOperation::Close)?;
        self.taskbar.remove_window(window_id);
        Ok(())
    }

    /// Mover ventana
    pub fn move_window(&mut self, window_id: u32, x: i32, y: i32) -> Result<(), String> {
        self.window_operations.execute_operation(window_id, window_operations::WindowOperation::Move { x, y })?;
        Ok(())
    }

    /// Redimensionar ventana
    pub fn resize_window(&mut self, window_id: u32, width: u32, height: u32) -> Result<(), String> {
        self.window_operations.execute_operation(window_id, window_operations::WindowOperation::Resize { width, height })?;
        Ok(())
    }

    /// Enfocar ventana
    pub fn focus_window(&mut self, window_id: u32) {
        self.window_operations.focus_window(window_id);
        self.taskbar.set_active_window(window_id);
    }

    /// Iniciar arrastre de ventana
    pub fn start_window_drag(&mut self, window_id: u32, start_x: i32, start_y: i32) -> Result<(), String> {
        self.window_operations.start_drag(window_id, start_x, start_y)
    }

    /// Actualizar arrastre de ventana
    pub fn update_window_drag(&mut self, current_x: i32, current_y: i32) -> Result<(), String> {
        self.window_operations.update_drag(current_x, current_y)
    }

    /// Finalizar arrastre de ventana
    pub fn end_window_drag(&mut self) {
        self.window_operations.end_drag();
    }

    /// Iniciar redimensionamiento de ventana
    pub fn start_window_resize(&mut self, window_id: u32, corner: window_operations::ResizeCorner, start_x: i32, start_y: i32) -> Result<(), String> {
        self.window_operations.start_resize(window_id, corner, start_x, start_y)
    }

    /// Actualizar redimensionamiento de ventana
    pub fn update_window_resize(&mut self, current_x: i32, current_y: i32) -> Result<(), String> {
        self.window_operations.update_resize(current_x, current_y)
    }

    /// Finalizar redimensionamiento de ventana
    pub fn end_window_resize(&mut self) {
        self.window_operations.end_resize();
    }

    /// Detectar esquina de redimensionamiento
    pub fn detect_resize_corner(&self, window_id: u32, x: i32, y: i32) -> Option<window_operations::ResizeCorner> {
        self.window_operations.detect_resize_corner(window_id, x, y)
    }

    /// Obtener información de ventana
    pub fn get_window_info(&self, window_id: u32) -> Option<&window_operations::WindowInfo> {
        self.window_operations.get_window_info(window_id)
    }

    /// Obtener todas las ventanas
    pub fn get_all_windows(&self) -> Vec<&window_operations::WindowInfo> {
        self.window_operations.get_all_windows()
    }

    /// Obtener ventana enfocada
    pub fn get_focused_window(&self) -> Option<&window_operations::WindowInfo> {
        self.window_operations.get_focused_window()
    }

    /// Obtener ventanas ordenadas por Z-order
    pub fn get_windows_by_z_order(&self) -> Vec<&window_operations::WindowInfo> {
        self.window_operations.get_windows_by_z_order()
    }

    /// Minimizar todas las ventanas
    pub fn minimize_all_windows(&mut self) {
        self.window_operations.minimize_all();
    }

    /// Restaurar todas las ventanas
    pub fn restore_all_windows(&mut self) {
        self.window_operations.restore_all();
    }

    /// Cambiar a ventana siguiente
    pub fn switch_to_next_window(&mut self) {
        self.window_operations.switch_to_next_window();
        if let Some(focused) = self.window_operations.get_focused_window() {
            self.taskbar.set_active_window(focused.id);
        }
    }

    /// Cambiar a ventana anterior
    pub fn switch_to_previous_window(&mut self) {
        self.window_operations.switch_to_previous_window();
        if let Some(focused) = self.window_operations.get_focused_window() {
            self.taskbar.set_active_window(focused.id);
        }
    }

    /// Renderizar controles de ventana
    pub fn render_window_controls(&mut self, fb: &mut FramebufferDriver, window_id: u32) -> Result<(), String> {
        if let Some(window_info) = self.window_operations.get_window_info(window_id) {
            window_operations::render_window_controls(fb, window_info)
        } else {
            Err("Ventana no encontrada".to_string())
        }
    }

    // === Métodos de Integración Wayland ===
    
    /// Inicializar integración de Wayland
    pub fn initialize_wayland(&mut self) -> Result<(), String> {
        if self.wayland_integration.is_some() {
            return Ok(());
        }
        
        let mut wayland_integration = wayland_integration::CosmicWaylandIntegration::new()?;
        wayland_integration.initialize()?;
        
        self.wayland_integration = Some(wayland_integration);
        Ok(())
    }
    
    /// Crear aplicación nativa de Wayland
    pub fn create_wayland_app(&mut self, app_type: wayland_integration::NativeAppType) -> Result<u32, String> {
        if let Some(ref mut wayland_integration) = self.wayland_integration {
            let object_id = wayland_integration.create_native_app(app_type)?;
            Ok(object_id)
        } else {
            Err("Integración de Wayland no inicializada".to_string())
        }
    }
    
    /// Manejar eventos de Wayland
    pub fn handle_wayland_events(&mut self) -> Result<Vec<CosmicEvent>, String> {
        if let Some(ref mut wayland_integration) = self.wayland_integration {
            wayland_integration.handle_wayland_events()
        } else {
            Ok(Vec::new())
        }
    }
    
    /// Renderizar frame integrado con Wayland
    pub fn render_wayland_frame(&mut self, fb: &mut FramebufferDriver) -> Result<(), String> {
        if let Some(ref mut wayland_integration) = self.wayland_integration {
            wayland_integration.render_integrated_frame(fb)
        } else {
            // Fallback al renderizado normal de COSMIC
            self.render_taskbar(fb)
        }
    }
    
    /// Obtener información del servidor Wayland
    pub fn get_wayland_server_info(&self) -> Result<wayland_integration::ServerInfo, String> {
        if let Some(ref wayland_integration) = self.wayland_integration {
            wayland_integration.get_server_info()
        } else {
            Err("Integración de Wayland no inicializada".to_string())
        }
    }
    
    /// Crear workspace virtual
    pub fn create_virtual_workspace(&mut self, name: String) -> Result<u32, String> {
        if let Some(ref mut wayland_integration) = self.wayland_integration {
            wayland_integration.create_virtual_workspace(name)
        } else {
            Err("Integración de Wayland no inicializada".to_string())
        }
    }
    
    /// Cambiar tema dinámico
    pub fn change_wayland_theme(&mut self, theme: String) -> Result<(), String> {
        if let Some(ref mut wayland_integration) = self.wayland_integration {
            wayland_integration.change_theme(theme)
        } else {
            Err("Integración de Wayland no inicializada".to_string())
        }
    }
    
    /// Configurar panel de Wayland
    pub fn configure_wayland_panel(&mut self, height: u32, position: wayland_integration::cosmic_protocols::PanelPosition) -> Result<(), String> {
        if let Some(ref mut wayland_integration) = self.wayland_integration {
            wayland_integration.configure_panel(height, position)
        } else {
            Err("Integración de Wayland no inicializada".to_string())
        }
    }
    
    /// Obtener estadísticas de rendimiento de Wayland
    pub fn get_wayland_performance_stats(&self) -> Result<wayland_integration::PerformanceStats, String> {
        if let Some(ref wayland_integration) = self.wayland_integration {
            wayland_integration.get_performance_stats()
        } else {
            Err("Integración de Wayland no inicializada".to_string())
        }
    }
    
    /// Verificar si Wayland está activo
    pub fn is_wayland_active(&self) -> bool {
        self.wayland_integration.is_some()
    }
}

impl Default for CosmicManager {
    fn default() -> Self {
        Self::new()
    }
}
