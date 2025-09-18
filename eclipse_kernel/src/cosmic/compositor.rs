//! Compositor COSMIC personalizado para Eclipse OS
//! 
//! Implementa un compositor optimizado que integra las características
//! únicas de Eclipse OS con el sistema de composición de COSMIC.

use super::{CosmicPerformanceStats, WindowManagerMode};
use crate::wayland::rendering::{WaylandRenderer, RenderBackend};
use crate::drivers::framebuffer::{FramebufferDriver, Color};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::vec;

/// Compositor COSMIC para Eclipse OS
pub struct CosmicCompositor {
    renderer: Option<WaylandRenderer>,
    framebuffer: Option<FramebufferDriver>,
    window_manager_mode: WindowManagerMode,
    active_windows: Vec<CompositorWindow>,
    performance_stats: CosmicPerformanceStats,
    initialized: bool,
}

/// Ventana en el compositor
#[derive(Debug, Clone)]
pub struct CompositorWindow {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub z_order: u32,
    pub visible: bool,
    pub buffer: Vec<u32>,
    pub needs_redraw: bool,
}

/// Configuración del compositor
#[derive(Debug, Clone)]
pub struct CompositorConfig {
    pub render_backend: RenderBackend,
    pub vsync_enabled: bool,
    pub hardware_acceleration: bool,
    pub max_windows: u32,
    pub frame_rate: u32,
}

impl Default for CompositorConfig {
    fn default() -> Self {
        Self {
            render_backend: RenderBackend::Software,
            vsync_enabled: true,
            hardware_acceleration: true,
            max_windows: 100,
            frame_rate: 60,
        }
    }
}

impl CosmicCompositor {
    /// Crear nuevo compositor
    pub fn new() -> Self {
        Self {
            renderer: None,
            framebuffer: None,
            window_manager_mode: WindowManagerMode::Hybrid,
            active_windows: Vec::new(),
            performance_stats: CosmicPerformanceStats::default(),
            initialized: false,
        }
    }

    /// Crear compositor con configuración
    pub fn with_config(config: CompositorConfig) -> Self {
        Self {
            renderer: None,
            framebuffer: None,
            window_manager_mode: WindowManagerMode::Hybrid,
            active_windows: Vec::new(),
            performance_stats: CosmicPerformanceStats::default(),
            initialized: false,
        }
    }

    /// Inicializar compositor
    pub fn initialize(&mut self, config: CompositorConfig) -> Result<(), String> {
        if self.initialized {
            return Ok(());
        }

        // Obtener framebuffer
        self.framebuffer = crate::drivers::framebuffer::get_framebuffer().map(|fb| unsafe { core::ptr::read(fb) });

        // Inicializar renderer: preferir OpenGL y caer a backend solicitado si falla
        // Esto explota además el fallback interno cuando el backend pedido es Software
        let mut renderer = WaylandRenderer::new(RenderBackend::OpenGL);
        match renderer.initialize() {
            Ok(_) => {
                // OpenGL activo
                self.renderer = Some(renderer);
            }
            Err(_) => {
                // Fallback al backend solicitado en config (p.ej., Software)
                let mut fallback = WaylandRenderer::new(config.render_backend);
                fallback
                    .initialize()
                    .map_err(|_| "No se pudo inicializar el renderer (OpenGL ni fallback)".to_string())?;
                self.renderer = Some(fallback);
            }
        }

        // Configurar framebuffer real en el renderer si está disponible
        if let (Some(ref fb), Some(ref mut renderer)) = (&self.framebuffer, &mut self.renderer) {
            renderer.framebuffer.width = fb.info.width;
            renderer.framebuffer.height = fb.info.height;
            renderer.framebuffer.pitch = fb.info.pixels_per_scan_line * 4;
            renderer.framebuffer.format = crate::wayland::surface::BufferFormat::XRGB8888;
            renderer.framebuffer.address = fb.info.base_address as *mut u8;
        }

        // Log en framebuffer del backend efectivo
        if let (Some(ref mut fb), Some(ref renderer)) = (&mut self.framebuffer, &self.renderer) {
            let stats = renderer.get_stats();
            let backend_str = match stats.backend {
                RenderBackend::OpenGL => "OpenGL",
                RenderBackend::Vulkan => "Vulkan",
                RenderBackend::DirectFB => "DirectFB",
                RenderBackend::Software => "Software",
            };
            let msg = alloc::format!("Compositor backend: {}", backend_str);
            fb.write_text_kernel(&msg, Color::LIGHT_GRAY);
        }

        self.initialized = true;
        Ok(())
    }

    /// Crear nueva ventana
    pub fn create_window(&mut self, id: u32, x: i32, y: i32, width: u32, height: u32) -> Result<(), String> {
        if !self.initialized {
            return Err("Compositor no inicializado".to_string());
        }

        let window = CompositorWindow {
            id,
            x,
            y,
            width,
            height,
            z_order: self.active_windows.len() as u32,
            visible: true,
            buffer: vec![0; (width * height) as usize],
            needs_redraw: true,
        };

        self.active_windows.push(window);
        Ok(())
    }

    /// Destruir ventana
    pub fn destroy_window(&mut self, id: u32) -> Result<(), String> {
        if !self.initialized {
            return Err("Compositor no inicializado".to_string());
        }

        self.active_windows.retain(|w| w.id != id);
        Ok(())
    }

    /// Mover ventana
    pub fn move_window(&mut self, id: u32, x: i32, y: i32) -> Result<(), String> {
        if !self.initialized {
            return Err("Compositor no inicializado".to_string());
        }

        if let Some(window) = self.active_windows.iter_mut().find(|w| w.id == id) {
            window.x = x;
            window.y = y;
            window.needs_redraw = true;
        }

        Ok(())
    }

    /// Redimensionar ventana
    pub fn resize_window(&mut self, id: u32, width: u32, height: u32) -> Result<(), String> {
        if !self.initialized {
            return Err("Compositor no inicializado".to_string());
        }

        if let Some(window) = self.active_windows.iter_mut().find(|w| w.id == id) {
            window.width = width;
            window.height = height;
            window.buffer.resize((width * height) as usize, 0);
            window.needs_redraw = true;
        }

        Ok(())
    }

    /// Cambiar orden Z de ventana
    pub fn set_window_z_order(&mut self, id: u32, z_order: u32) -> Result<(), String> {
        if !self.initialized {
            return Err("Compositor no inicializado".to_string());
        }

        if let Some(window) = self.active_windows.iter_mut().find(|w| w.id == id) {
            window.z_order = z_order;
            window.needs_redraw = true;
        }

        Ok(())
    }

    /// Mostrar/ocultar ventana
    pub fn set_window_visibility(&mut self, id: u32, visible: bool) -> Result<(), String> {
        if !self.initialized {
            return Err("Compositor no inicializado".to_string());
        }

        if let Some(window) = self.active_windows.iter_mut().find(|w| w.id == id) {
            window.visible = visible;
            window.needs_redraw = true;
        }

        Ok(())
    }

    /// Actualizar buffer de ventana
    pub fn update_window_buffer(&mut self, id: u32, buffer: &[u32]) -> Result<(), String> {
        if !self.initialized {
            return Err("Compositor no inicializado".to_string());
        }

        if let Some(window) = self.active_windows.iter_mut().find(|w| w.id == id) {
            if buffer.len() == window.buffer.len() {
                window.buffer.copy_from_slice(buffer);
                window.needs_redraw = true;
            }
        }

        Ok(())
    }

    /// Renderizar frame completo
    pub fn render_frame(&mut self) -> Result<(), String> {
        if !self.initialized {
            return Err("Compositor no inicializado".to_string());
        }

        // Limpiar pantalla (simulado para WaylandRenderer)
        // En una implementación real, esto limpiaría el framebuffer

        // Ordenar ventanas por Z-order
        self.active_windows.sort_by_key(|w| w.z_order);

        // Renderizar ventanas visibles
        let window_count = self.active_windows.len();
        for i in 0..window_count {
            let window = &self.active_windows[i];
            if window.visible && window.needs_redraw {
                // Crear una copia de la ventana para evitar problemas de préstamo
                let window_copy = CompositorWindow {
                    id: window.id,
                    x: window.x,
                    y: window.y,
                    width: window.width,
                    height: window.height,
                    z_order: window.z_order,
                    visible: window.visible,
                    needs_redraw: window.needs_redraw,
                    buffer: window.buffer.clone(),
                };
                self.render_window(&window_copy)?;
            }
        }

        // Presentar frame
        self.present_frame()?;

        // Actualizar estadísticas
        self.update_performance_stats();

        Ok(())
    }

    /// Limpiar pantalla
    fn clear_screen(&mut self) -> Result<(), String> {
        // Simulado - en una implementación real limpiaríamos el framebuffer
        Ok(())
    }

    /// Renderizar ventana individual
    fn render_window(&mut self, window: &CompositorWindow) -> Result<(), String> {
        if let Some(ref mut renderer) = self.renderer {
            // Registrar superficie si no está registrada
            // Crear buffer para la superficie
            let mut buffer = crate::wayland::buffer::SharedMemoryBuffer::new(
                window.width,
                window.height,
                crate::wayland::surface::BufferFormat::XRGB8888
            );
            // Copiar contenido de la ventana al buffer (u32 -> u8)
            let expected_len = (window.width * window.height) as usize;
            if window.buffer.len() == expected_len {
                let dst = buffer.get_data_mut();
                // Interpretar dst como [u32] para copia directa
                if dst.len() >= expected_len * 4 {
                    unsafe {
                        let dst_u32 = core::slice::from_raw_parts_mut(dst.as_mut_ptr() as *mut u32, expected_len);
                        dst_u32.copy_from_slice(&window.buffer);
                    }
                }
            }
            renderer.register_surface(window.id, buffer, (window.x, window.y))?;

            // Actualizar buffer de la superficie
            let mut buffer = crate::wayland::buffer::SharedMemoryBuffer::new(
                window.width,
                window.height,
                crate::wayland::surface::BufferFormat::XRGB8888
            );
            // Copiar nuevamente el contenido actualizado
            let expected_len2 = (window.width * window.height) as usize;
            if window.buffer.len() == expected_len2 {
                let dst2 = buffer.get_data_mut();
                if dst2.len() >= expected_len2 * 4 {
                    unsafe {
                        let dst_u32_2 = core::slice::from_raw_parts_mut(dst2.as_mut_ptr() as *mut u32, expected_len2);
                        dst_u32_2.copy_from_slice(&window.buffer);
                    }
                }
            }
            renderer.update_surface_buffer(window.id, buffer)?;
        }
        Ok(())
    }

    /// Presentar frame
    fn present_frame(&mut self) -> Result<(), String> {
        if let Some(ref mut renderer) = self.renderer {
            // Simular presentación de frame
            // En implementación real, esto llamaría a present_frame()
        }
        Ok(())
    }

    /// Actualizar estadísticas de rendimiento
    fn update_performance_stats(&mut self) {
        self.performance_stats.window_count = self.active_windows.len() as u32;
        self.performance_stats.frame_rate = 60.0; // Simulado
        self.performance_stats.memory_usage = self.calculate_memory_usage();
        self.performance_stats.cpu_usage = 15.0; // Simulado
        self.performance_stats.gpu_usage = 25.0; // Simulado
        self.performance_stats.compositor_latency = 16; // 16ms para 60fps
    }

    /// Calcular uso de memoria
    fn calculate_memory_usage(&self) -> u64 {
        let mut total = 0;
        for window in &self.active_windows {
            total += (window.buffer.len() * 4) as u64; // 4 bytes por píxel
        }
        total
    }

    /// Obtener estadísticas de rendimiento
    pub fn get_performance_stats(&self) -> &CosmicPerformanceStats {
        &self.performance_stats
    }

    /// Obtener ventanas activas
    pub fn get_active_windows(&self) -> &[CompositorWindow] {
        &self.active_windows
    }

    /// Obtener ventana por ID
    pub fn get_window(&self, id: u32) -> Option<&CompositorWindow> {
        self.active_windows.iter().find(|w| w.id == id)
    }

    /// Configurar modo de gestión de ventanas
    pub fn set_window_manager_mode(&mut self, mode: WindowManagerMode) {
        self.window_manager_mode = mode;
    }

    /// Obtener modo de gestión de ventanas
    pub fn get_window_manager_mode(&self) -> WindowManagerMode {
        self.window_manager_mode
    }

    /// Verificar si está inicializado
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Detener compositor
    pub fn shutdown(&mut self) -> Result<(), String> {
        if !self.initialized {
            return Ok(());
        }

        self.active_windows.clear();
        self.renderer = None;
        self.framebuffer = None;
        self.initialized = false;

        Ok(())
    }
}

impl Default for CosmicCompositor {
    fn default() -> Self {
        Self::new()
    }
}
